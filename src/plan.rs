//! How a locked environment is split into image layers.
//!
//! Each conda package gets a layer of its own, so a change to one package re-uploads one layer and an image shares
//! layers with every other image built from the same packages. Layers are ordered dependencies first: the packages
//! everything rests on (`libgcc`, `openssl`, `python`) sit at the bottom, where they change least often.
//!
//! Overlay filesystems limit how many layers an image can have, so past `max_layers` the packages at the top of the
//! order, the ones least depended on, share the last layer. All PyPI packages share one layer above the conda ones:
//! they are installed into the same site-packages and are usually what changes most.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::lock::{Kind, Locked, Package};
use crate::platform::{OciPlatform, oci_platform};
use crate::report::plural;

/// The most layers a plan uses unless told otherwise. Docker's overlay2 driver refuses images with more than 128,
/// and the base image and the image's own configuration need some.
pub const DEFAULT_MAX_LAYERS: usize = 100;

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
    /// The conda packages left over once the layer limit is reached.
    CondaMerged,
    /// Every PyPI package.
    Pypi,
}

/// The layers an image of one environment on one platform is built from, bottom first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub environment: String,
    pub platform: String,
    pub oci_platform: OciPlatform,
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

/// Plan the layers for `locked`, using at most `max_layers` of them.
pub fn plan(locked: Locked, max_layers: usize) -> Result<Plan, PlanError> {
    if max_layers < 2 {
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
    let conda_budget = if pypi.is_empty() { max_layers } else { max_layers - 1 };

    let mut ordered = dependency_order(conda).into_iter();
    let mut layers: Vec<Layer> = ordered
        .by_ref()
        .take(conda_budget.saturating_sub(1))
        .map(|package| layer(LayerKind::Conda, vec![package]))
        .collect();
    let rest: Vec<Package> = ordered.collect();
    match rest.len() {
        0 => {}
        1 => layers.push(layer(LayerKind::Conda, rest)),
        _ => layers.push(layer(LayerKind::CondaMerged, rest)),
    }

    if !pypi.is_empty() {
        let mut pypi = pypi;
        pypi.sort_by(|a, b| a.name.cmp(&b.name));
        layers.push(layer(LayerKind::Pypi, pypi));
    }

    Ok(Plan {
        environment: locked.environment,
        platform: locked.platform,
        oci_platform,
        layers,
    })
}

fn layer(kind: LayerKind, packages: Vec<Package>) -> Layer {
    let size = packages.iter().map(|package| package.size).sum();
    Layer { kind, packages, size }
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
        let plan = plan(locked("linux-64", packages), 100).unwrap();
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
        let plan = plan(locked("linux-64", packages), 100).unwrap();
        assert_eq!(names(&plan), [["libgcc"], ["pip"], ["python"]]);
        assert_eq!(plan.size(), None);
    }

    #[test]
    fn pypi_packages_share_the_top_layer() {
        let packages = vec![pypi("requests"), conda("python", &[], Some(10)), pypi("certifi")];
        let plan = plan(locked("linux-aarch64", packages), 100).unwrap();
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
        let plan = plan(locked("linux-64", packages), 3).unwrap();
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
        let plan = plan(locked("linux-64", packages), 2).unwrap();
        assert_eq!(names(&plan), [["a"], ["b"]]);
        assert!(plan.layers.iter().all(|layer| layer.kind == LayerKind::Conda));
    }

    #[test]
    fn an_empty_environment_has_no_layers() {
        let plan = plan(locked("linux-64", Vec::new()), 100).unwrap();
        assert!(plan.layers.is_empty());
        assert_eq!(plan.size(), Some(0));
    }

    #[test]
    fn only_linux_platforms_can_be_planned() {
        let err = plan(locked("osx-arm64", vec![conda("a", &[], None)]), 100).unwrap_err();
        assert!(matches!(err, PlanError::NotLinux { .. }), "{err}");
        assert!(err.to_string().contains("osx-arm64"));
    }

    #[test]
    fn source_packages_are_refused_by_name() {
        let mut one = conda("mytool", &[], None);
        one.kind = Kind::CondaSource;
        let mut two = conda("another", &[], None);
        two.kind = Kind::CondaSource;
        let err = plan(locked("linux-64", vec![one.clone(), conda("a", &[], None)]), 100).unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment `default` builds 1 package from source: mytool"
        );
        let err = plan(locked("linux-64", vec![one, two]), 100).unwrap_err();
        assert_eq!(
            err.to_string(),
            "environment `default` builds 2 packages from source: another, mytool"
        );
    }

    #[test]
    fn fewer_than_two_layers_is_refused() {
        let err = plan(locked("linux-64", Vec::new()), 1).unwrap_err();
        assert!(matches!(err, PlanError::TooFewLayers));
    }
}
