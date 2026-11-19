//! How a locked environment is split into image layers.
//!
//! Layers are content-addressed, so a change to one package re-uploads only the layers that hold it, and an image
//! shares layers with every other image built from the same packages. Conda packages are ordered dependencies first:
//! the packages everything rests on (`libgcc`, `openssl`, `python`) sit at the bottom, where they change least often.
//!
//! [`Layering::Single`], the default, puts the whole environment in one layer above the base image's, so an image
//! is a base, an environment and an application.
//!
//! [`Layering::Grouped`] gives each package of [`OWN_LAYER`] or more a layer of its own and groups the small ones,
//! for a large environment that changes often, where re-uploading only the changed packages pays for the extra
//! layers. A group ends after a package whose name hashes to a cut point, never at a position, so adding or removing
//! a package changes only the group it lands in. [`Layering::PerPackage`] gives every package a layer.
//!
//! Overlay filesystems limit how many layers an image can have, so past `max_layers` the layers at the top of the
//! order, the ones least depended on, are merged into the last. With more than one layer, all PyPI packages share
//! one above the conda ones: they are installed into the same site-packages and are usually what changes most.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::lock::{Kind, Locked, Package};
use crate::platform::{OciPlatform, oci_platform};
use crate::report::plural;

/// The most layers a plan uses unless told otherwise. Docker's overlay2 driver refuses images with more than 128,
/// and the base image and the image's own configuration need some.
pub const DEFAULT_MAX_LAYERS: usize = 100;

/// A conda package whose archive is at least this large gets a layer of its own when grouping.
pub const OWN_LAYER: u64 = 4 << 20;
/// A group is not ended before its archives add up to this much.
const GROUP_MIN: u64 = 1 << 20;
/// A group is ended once its archives add up to this much, whatever the names.
const GROUP_MAX: u64 = 8 << 20;
/// What a package counts for in a group when the lockfile does not record its size.
const UNKNOWN_SIZE: u64 = 256 << 10;
/// Past [`GROUP_MIN`], about one package name in this many ends a group.
const CUT_ONE_IN: u64 = 4;

/// How packages are split into layers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Layering {
    /// Packages of 4 MiB or more alone, smaller ones in groups.
    Grouped,
    /// One layer per package.
    PerPackage,
    /// The whole environment in one layer.
    #[default]
    Single,
}

/// Why a locked environment cannot be planned into layers.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PlanError {
    #[error("{platform} is not a Linux platform, so no container image can be built for it")]
    #[diagnostic(help("pick a linux-* platform with --platform, for example linux-64 or linux-aarch64"))]
    NotLinux { platform: String },

    #[error("environment `{environment}` builds {} from source: {}", plural(.names.len(), "package"), .names.join(", "))]
    #[diagnostic(help(
        "pixi-oci layers built conda packages; build these first (`pixi build`) and lock the results from a channel"
    ))]
    SourcePackages { environment: String, names: Vec<String> },

    #[error("--max-layers must be at least 2: one for the conda packages and one for the PyPI packages")]
    TooFewLayers,
}

/// What goes into one layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Layer {
    pub kind: LayerKind,
    pub packages: Vec<Package>,
    /// Sum of the packages' archive sizes, where the lockfile records them; `None` when any is unknown.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayerKind {
    /// One conda package.
    Conda,
    /// Small conda packages grouped together.
    CondaGroup,
    /// The conda packages left over once the layer limit is reached.
    CondaMerged,
    /// Every PyPI package.
    Pypi,
    /// Every package of the environment, conda and PyPI.
    Environment,
}

/// The layers an image of one environment on one platform is built from, bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub environment: String,
    pub platform: String,
    pub oci_platform: OciPlatform,
    pub layering: Layering,
    pub layers: Vec<Layer>,
}

impl Plan {
    /// How many packages of `kind` the plan holds.
    pub fn count(&self, kind: Kind) -> usize {
        self.layers
            .iter()
            .flat_map(|layer| &layer.packages)
            .filter(|package| package.kind == kind)
            .count()
    }

    /// Sum of every layer's size, or `None` when any is unknown.
    pub fn size(&self) -> Option<u64> {
        self.layers.iter().map(|layer| layer.size).sum()
    }
}

/// Plan the layers for `locked` the way `layering` says, using at most `max_layers` of them.
pub fn plan(locked: Locked, layering: Layering, max_layers: usize) -> Result<Plan, PlanError> {
    if max_layers < 2 && layering != Layering::Single {
        return Err(PlanError::TooFewLayers);
    }
    let oci_platform = oci_platform(&locked.platform).ok_or_else(|| PlanError::NotLinux {
        platform: locked.platform.clone(),
    })?;
    let mut sources: Vec<String> = locked
        .packages
        .iter()
        .filter(|package| package.kind == Kind::CondaSource)
        .map(|package| package.name.clone())
        .collect();
    if !sources.is_empty() {
        sources.sort();
        return Err(PlanError::SourcePackages {
            environment: locked.environment,
            names: sources,
        });
    }

    let (conda, pypi): (Vec<Package>, Vec<Package>) = locked
        .packages
        .into_iter()
        .partition(|package| package.kind == Kind::Conda);
    let ordered = dependency_order(conda);
    let mut pypi = pypi;
    pypi.sort_by(|a, b| a.name.cmp(&b.name));

    let layers = match layering {
        Layering::Single => {
            let everything: Vec<Package> = ordered.into_iter().chain(pypi).collect();
            if everything.is_empty() {
                Vec::new()
            } else {
                vec![layer(LayerKind::Environment, everything)]
            }
        }
        Layering::Grouped | Layering::PerPackage => {
            let conda_layers = if layering == Layering::Grouped {
                grouped(ordered)
            } else {
                ordered
                    .into_iter()
                    .map(|package| layer(LayerKind::Conda, vec![package]))
                    .collect()
            };
            let conda_budget = if pypi.is_empty() { max_layers } else { max_layers - 1 };
            let mut layers = capped(conda_layers, conda_budget);
            if !pypi.is_empty() {
                layers.push(layer(LayerKind::Pypi, pypi));
            }
            layers
        }
    };

    Ok(Plan {
        environment: locked.environment,
        platform: locked.platform,
        oci_platform,
        layering,
        layers,
    })
}

fn layer(kind: LayerKind, packages: Vec<Package>) -> Layer {
    let size = packages.iter().map(|package| package.size).sum();
    Layer { kind, packages, size }
}

/// A group's layer: a lone package keeps the kind of a package's own layer.
fn group(packages: Vec<Package>) -> Layer {
    let kind = if packages.len() == 1 {
        LayerKind::Conda
    } else {
        LayerKind::CondaGroup
    };
    layer(kind, packages)
}

/// `ordered` with each package of [`OWN_LAYER`] or more alone and the rest in groups, in order. A group closes after
/// [`GROUP_MIN`] at a package whose name is a cut point, or at [`GROUP_MAX`]; one still under [`GROUP_MIN`] when a
/// large package comes along stays open past it rather than become a layer of its own.
fn grouped(ordered: Vec<Package>) -> Vec<Layer> {
    let mut layers = Vec::new();
    let mut open: Vec<Package> = Vec::new();
    let mut weight = 0;
    for package in ordered {
        match package.size {
            Some(size) if size >= OWN_LAYER => {
                if weight >= GROUP_MIN {
                    layers.push(group(std::mem::take(&mut open)));
                    weight = 0;
                }
                layers.push(layer(LayerKind::Conda, vec![package]));
            }
            size => {
                weight += size.unwrap_or(UNKNOWN_SIZE);
                let close = weight >= GROUP_MAX || (weight >= GROUP_MIN && cut_point(&package.name));
                open.push(package);
                if close {
                    layers.push(group(std::mem::take(&mut open)));
                    weight = 0;
                }
            }
        }
    }
    if !open.is_empty() {
        layers.push(group(open));
    }
    layers
}

/// Whether a group may end after the package `name`: a fixed property of the name, so it does not move when other
/// packages come and go.
fn cut_point(name: &str) -> bool {
    let digest = rattler_digest::compute_bytes_digest::<rattler_digest::Sha256>(name.as_bytes());
    let first = u64::from_be_bytes(digest[..8].try_into().expect("a sha256 has 8 bytes to spare"));
    first.is_multiple_of(CUT_ONE_IN)
}

/// `layers` within `budget`: past it, the layers at the top are merged into one.
fn capped(mut layers: Vec<Layer>, budget: usize) -> Vec<Layer> {
    if layers.len() <= budget {
        return layers;
    }
    let rest: Vec<Package> = layers
        .split_off(budget.saturating_sub(1))
        .into_iter()
        .flat_map(|layer| layer.packages)
        .collect();
    layers.push(layer(LayerKind::CondaMerged, rest));
    layers
}

/// `packages` with every package after the packages it depends on, ties broken by name so the order is the same
/// for the same lockfile. Conda dependencies can form cycles (`python` and `pip`); a cycle is broken at its
/// alphabetically first package.
fn dependency_order(packages: Vec<Package>) -> Vec<Package> {
    let index: BTreeMap<String, usize> = packages
        .iter()
        .enumerate()
        .map(|(i, package)| (package.name.clone(), i))
        .collect();
    // Edges inside the environment only: virtual packages (`__glibc`) and anything not locked here are not layers.
    let depends: Vec<BTreeSet<usize>> = packages
        .iter()
        .enumerate()
        .map(|(i, package)| {
            package
                .depends
                .iter()
                .filter_map(|name| index.get(name).copied())
                .filter(|&j| j != i)
                .collect()
        })
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); packages.len()];
    for (i, deps) in depends.iter().enumerate() {
        for &j in deps {
            dependents[j].push(i);
        }
    }
    let mut waiting: Vec<usize> = depends.iter().map(BTreeSet::len).collect();
    let mut placed = vec![false; packages.len()];
    // Ready packages keyed by name, so the next one out is always the alphabetically first.
    let mut ready: BTreeMap<&str, usize> = (0..packages.len())
        .filter(|&i| waiting[i] == 0)
        .map(|i| (packages[i].name.as_str(), i))
        .collect();
    let mut order = Vec::with_capacity(packages.len());
    while order.len() < packages.len() {
        let next = match ready.pop_first() {
            Some((_, i)) => i,
            // Only cycles are left: take the alphabetically first unplaced package as if its dependencies were met.
            None => (0..packages.len())
                .filter(|&i| !placed[i])
                .min_by_key(|&i| packages[i].name.as_str())
                .expect("an unplaced package remains while the order is short"),
        };
        placed[next] = true;
        order.push(next);
        for &dependent in &dependents[next] {
            if placed[dependent] {
                continue;
            }
            waiting[dependent] = waiting[dependent].saturating_sub(1);
            if waiting[dependent] == 0 {
                ready.insert(packages[dependent].name.as_str(), dependent);
            }
        }
    }
    let mut slots: Vec<Option<Package>> = packages.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conda(name: &str, depends: &[&str], size: Option<u64>) -> Package {
        Package {
            name: name.to_string(),
            version: "1.0".to_string(),
            kind: Kind::Conda,
            build: Some("h0_0".to_string()),
            depends: depends.iter().map(ToString::to_string).collect(),
            location: format!("https://conda.anaconda.org/conda-forge/linux-64/{name}-1.0-h0_0.conda"),
            sha256: None,
            size,
            record: None,
        }
    }

    fn pypi(name: &str) -> Package {
        Package {
            name: name.to_string(),
            version: "2.0".to_string(),
            kind: Kind::Pypi,
            build: None,
            depends: Vec::new(),
            location: format!("https://files.pythonhosted.org/{name}-2.0-py3-none-any.whl"),
            sha256: None,
            size: None,
            record: None,
        }
    }

    fn locked(platform: &str, packages: Vec<Package>) -> Locked {
        Locked {
            environment: "default".to_string(),
            platform: platform.to_string(),
            packages,
        }
    }

    fn names(plan: &Plan) -> Vec<Vec<&str>> {
        plan.layers
            .iter()
            .map(|layer| layer.packages.iter().map(|p| p.name.as_str()).collect())
            .collect()
    }

    #[test]
    fn dependencies_come_first_and_ties_go_alphabetically() {
        let packages = vec![
            conda("python", &["libgcc >=13", "openssl >=3", "__glibc >=2.17"], Some(30)),
            conda("openssl", &["libgcc"], Some(3)),
            conda("zlib", &["libgcc"], Some(1)),
            conda("libgcc", &[], Some(1)),
            conda("ca-certificates", &[], Some(1)),
        ];
        let plan = plan(locked("linux-64", packages), Layering::PerPackage, 100).unwrap();
        assert_eq!(
            names(&plan),
            [["ca-certificates"], ["libgcc"], ["openssl"], ["python"], ["zlib"]]
        );
        assert!(plan.layers.iter().all(|layer| layer.kind == LayerKind::Conda));
        assert_eq!(plan.size(), Some(36));
        assert_eq!(plan.oci_platform.to_string(), "linux/amd64");
    }

    #[test]
    fn a_cycle_is_broken_at_its_first_name() {
        let packages = vec![
            conda("python", &["pip", "libgcc"], None),
            conda("pip", &["python"], None),
            conda("libgcc", &[], None),
        ];
        let plan = plan(locked("linux-64", packages), Layering::PerPackage, 100).unwrap();
        assert_eq!(names(&plan), [["libgcc"], ["pip"], ["python"]]);
        assert_eq!(plan.size(), None);
    }

    #[test]
    fn pypi_packages_share_the_top_layer() {
        let packages = vec![pypi("requests"), conda("python", &[], Some(10)), pypi("certifi")];
        let plan = plan(locked("linux-aarch64", packages), Layering::PerPackage, 100).unwrap();
        assert_eq!(names(&plan), vec![vec!["python"], vec!["certifi", "requests"]]);
        assert_eq!(plan.layers[1].kind, LayerKind::Pypi);
        assert_eq!(plan.count(Kind::Conda), 1);
        assert_eq!(plan.count(Kind::Pypi), 2);
        // A PyPI layer's size is unknown, since the lockfile does not record wheel sizes.
        assert_eq!(plan.size(), None);
    }

    #[test]
    fn past_the_limit_the_top_of_the_order_shares_a_layer() {
        let packages = vec![
            conda("d", &["c"], Some(1)),
            conda("c", &["b"], Some(1)),
            conda("b", &["a"], Some(1)),
            conda("a", &[], Some(1)),
            pypi("x"),
        ];
        let plan = plan(locked("linux-64", packages), Layering::PerPackage, 3).unwrap();
        assert_eq!(names(&plan), vec![vec!["a"], vec!["b", "c", "d"], vec!["x"]]);
        assert_eq!(
            plan.layers.iter().map(|l| l.kind).collect::<Vec<_>>(),
            [LayerKind::Conda, LayerKind::CondaMerged, LayerKind::Pypi]
        );
        assert_eq!(plan.layers[1].size, Some(3));
    }

    #[test]
    fn a_single_leftover_package_keeps_a_layer_of_its_own() {
        let packages = vec![conda("a", &[], None), conda("b", &[], None)];
        let plan = plan(locked("linux-64", packages), Layering::PerPackage, 2).unwrap();
        assert_eq!(names(&plan), [["a"], ["b"]]);
        assert!(plan.layers.iter().all(|layer| layer.kind == LayerKind::Conda));
    }

    #[test]
    fn an_empty_environment_has_no_layers() {
        let plan = plan(locked("linux-64", Vec::new()), Layering::PerPackage, 100).unwrap();
        assert!(plan.layers.is_empty());
        assert_eq!(plan.size(), Some(0));
    }

    #[test]
    fn only_linux_platforms_can_be_planned() {
        let err = plan(
            locked("osx-arm64", vec![conda("a", &[], None)]),
            Layering::PerPackage,
            100,
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::NotLinux { .. }), "{err}");
        assert!(err.to_string().contains("osx-arm64"));
    }

    #[test]
    fn source_packages_are_refused_by_name() {
        let mut one = conda("mytool", &[], None);
        one.kind = Kind::CondaSource;
        let mut two = conda("another", &[], None);
        two.kind = Kind::CondaSource;
        let err = plan(
            locked("linux-64", vec![one.clone(), conda("a", &[], None)]),
            Layering::PerPackage,
            100,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment `default` builds 1 package from source: mytool"
        );
        let err = plan(locked("linux-64", vec![one, two]), Layering::PerPackage, 100).unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment `default` builds 2 packages from source: another, mytool"
        );
    }

    #[test]
    fn fewer_than_two_layers_is_refused() {
        let err = plan(locked("linux-64", Vec::new()), Layering::PerPackage, 1).unwrap_err();
        assert!(matches!(err, PlanError::TooFewLayers));
        let err = plan(locked("linux-64", Vec::new()), Layering::Grouped, 1).unwrap_err();
        assert!(matches!(err, PlanError::TooFewLayers));
    }

    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;

    /// `count` small packages that depend on nothing, named so they sort in the order given.
    fn small(prefix: &str, count: usize, size: Option<u64>) -> Vec<Package> {
        (0..count)
            .map(|i| conda(&format!("{prefix}{i:03}"), &[], size))
            .collect()
    }

    fn kinds(plan: &Plan) -> Vec<LayerKind> {
        plan.layers.iter().map(|layer| layer.kind).collect()
    }

    #[test]
    fn grouping_gives_large_packages_their_own_layers() {
        let mut packages = small("lib", 40, Some(300 * KIB));
        packages.push(conda("python", &["lib000"], Some(30 * MIB)));
        packages.push(conda("numpy", &["python"], Some(OWN_LAYER)));
        let plan = plan(locked("linux-64", packages), Layering::Grouped, 100).unwrap();
        assert_eq!(plan.layering, Layering::Grouped);
        let alone: Vec<&str> = plan
            .layers
            .iter()
            .filter(|layer| layer.packages.len() == 1)
            .map(|layer| layer.packages[0].name.as_str())
            .collect();
        assert_eq!(alone, ["python", "numpy"]);
        assert!(plan.layers.len() < 15, "{} layers", plan.layers.len());
        let python = plan.layers.iter().position(|l| l.packages[0].name == "python").unwrap();
        let numpy = plan.layers.iter().position(|l| l.packages[0].name == "numpy").unwrap();
        assert!(python < numpy);
        // Every group but the one left open at the end reached its smallest size.
        let groups: Vec<&Layer> = plan
            .layers
            .iter()
            .filter(|layer| layer.kind == LayerKind::CondaGroup)
            .collect();
        for layer in &groups[..groups.len() - 1] {
            let size = layer.size.unwrap();
            assert!((GROUP_MIN..GROUP_MAX + OWN_LAYER).contains(&size), "{size}");
        }
        assert_eq!(plan.count(Kind::Conda), 42);
    }

    #[test]
    fn adding_a_package_changes_only_the_group_it_lands_in() {
        let before = small("pkg", 120, Some(200 * KIB));
        let mut after = before.clone();
        after.push(conda("pkg050a", &[], Some(200 * KIB)));
        let before = plan(locked("linux-64", before), Layering::Grouped, 100).unwrap();
        let after = plan(locked("linux-64", after), Layering::Grouped, 100).unwrap();
        let changed = before
            .layers
            .iter()
            .filter(|layer| !after.layers.contains(layer))
            .count();
        assert!(changed <= 2, "{changed} of {} layers changed", before.layers.len());
        assert!(before.layers.len() > 10);
    }

    #[test]
    fn a_small_group_stays_open_past_a_large_package() {
        let packages = vec![
            conda("a", &[], Some(10 * KIB)),
            conda("big", &["a"], Some(5 * MIB)),
            conda("c", &["big"], Some(10 * KIB)),
        ];
        let plan = plan(locked("linux-64", packages), Layering::Grouped, 100).unwrap();
        assert_eq!(names(&plan), vec![vec!["big"], vec!["a", "c"]]);
        assert_eq!(kinds(&plan), [LayerKind::Conda, LayerKind::CondaGroup]);
    }

    #[test]
    fn a_group_ends_at_its_largest_size_whatever_the_names() {
        // 3 MiB each: past GROUP_MAX after three, cut point or not.
        let packages = small("x", 9, Some(3 * MIB));
        let plan = plan(locked("linux-64", packages), Layering::Grouped, 100).unwrap();
        assert!(plan.layers.iter().all(|layer| layer.packages.len() <= 3));
        assert!(plan.layers.iter().all(|layer| layer.size.unwrap() <= 9 * MIB));
    }

    #[test]
    fn packages_of_unknown_size_are_still_grouped() {
        let plan = plan(locked("linux-64", small("u", 60, None)), Layering::Grouped, 100).unwrap();
        assert!(plan.layers.len() < 20, "{} layers", plan.layers.len());
        assert_eq!(plan.size(), None);
        assert_eq!(plan.count(Kind::Conda), 60);
    }

    #[test]
    fn grouped_layers_are_capped_too() {
        let mut packages = small("big", 10, Some(5 * MIB));
        packages.push(pypi("requests"));
        let plan = plan(locked("linux-64", packages), Layering::Grouped, 4).unwrap();
        assert_eq!(
            kinds(&plan),
            [
                LayerKind::Conda,
                LayerKind::Conda,
                LayerKind::CondaMerged,
                LayerKind::Pypi
            ]
        );
        assert_eq!(plan.layers[2].packages.len(), 8);
    }

    #[test]
    fn single_puts_the_whole_environment_in_one_layer() {
        let packages = vec![
            pypi("requests"),
            conda("python", &["libgcc"], Some(30 * MIB)),
            conda("libgcc", &[], Some(MIB)),
        ];
        // The layer limit does not apply to one layer.
        let plan = plan(locked("linux-64", packages), Layering::Single, 1).unwrap();
        assert_eq!(names(&plan), [["libgcc", "python", "requests"]]);
        assert_eq!(kinds(&plan), [LayerKind::Environment]);
        assert_eq!(plan.count(Kind::Pypi), 1);
        let empty = super::plan(locked("linux-64", Vec::new()), Layering::Single, 100).unwrap();
        assert!(empty.layers.is_empty());
    }

    #[test]
    fn cut_points_never_change() {
        // Changing which names are cut points changes every grouped image's layers: see CLAUDE.md.
        let names = [
            "python", "numpy", "openssl", "libgcc", "zlib", "pandas", "tk", "six", "bzip2", "ncurses", "readline",
            "libffi",
        ];
        let cuts: Vec<&str> = names.into_iter().filter(|name| cut_point(name)).collect();
        assert_eq!(cuts, ["libgcc", "zlib", "pandas", "tk", "readline", "libffi"]);
    }

    #[test]
    fn the_layering_is_part_of_the_plan() {
        let plan = plan(locked("linux-64", Vec::new()), Layering::PerPackage, 100).unwrap();
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["layering"], "per-package");
        assert_eq!(serde_json::to_value(LayerKind::CondaGroup).unwrap(), "conda-group");
    }
}
