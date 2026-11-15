//! Installing one conda package into a layer directory, as if into the image's prefix.
//!
//! Conda packages carry a placeholder prefix in some files (shebangs, `pkg-config` files, compiled-in paths) that
//! installation replaces with the environment's real path. The layer is staged in a temporary directory, but the
//! environment will live at the image's prefix, so rattler is told to write that path instead.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rattler::install::{InstallOptions, PythonInfo, link_package_sync};
use rattler_conda_types::prefix::Prefix;
use rattler_conda_types::{PrefixRecord, RepoDataRecord, Subdir, Version};

use crate::lock::Package;

/// Why a package could not be installed into its layer.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum InstallError {
    #[error("could not install {name} into its layer")]
    Link {
        name: String,
        #[source]
        source: Box<rattler::install::InstallError>,
    },

    #[error("could not write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{name} is a noarch Python package, but the environment has no python to install it for")]
    NoPython { name: String },

    #[error("{name}'s location in the lockfile is not a URL, so it has no repodata record to install from")]
    #[diagnostic(help("pixi-oci installs packages from channels; lock this one from a channel"))]
    NoRecord { name: String },

    #[error("{platform} is not a platform rattler knows")]
    Platform { platform: String },
}

/// What every package of one environment is installed with.
pub struct Target {
    /// The environment's path inside the image, such as `/opt/env`.
    pub prefix: String,
    pub platform: Subdir,
    /// For noarch Python packages; `None` when the environment has no python.
    pub python: Option<PythonInfo>,
}

impl Target {
    /// The target for an environment at `prefix` on `platform`, whose python (if any) is `python_version`.
    pub fn new(prefix: &str, platform: &str, python_version: Option<&str>) -> Result<Self, InstallError> {
        let platform: Subdir = platform.parse().map_err(|_| InstallError::Platform {
            platform: platform.to_string(),
        })?;
        let python = python_version
            .and_then(|version| version.parse::<Version>().ok())
            .and_then(|version| PythonInfo::from_version(&version, None, platform).ok());
        Ok(Self {
            prefix: prefix.trim_end_matches('/').to_string(),
            platform,
            python,
        })
    }
}

/// Install the extracted `package_dir` of `package` into `layer_root`, under the target's prefix, with its
/// `conda-meta` record beside it.
pub fn install(package: &Package, package_dir: &Path, layer_root: &Path, target: &Target) -> Result<(), InstallError> {
    let record: RepoDataRecord = package
        .record
        .as_deref()
        .cloned()
        .ok_or_else(|| InstallError::NoRecord {
            name: package.name.clone(),
        })?;
    if record.package_record.noarch.is_python() && target.python.is_none() {
        return Err(InstallError::NoPython {
            name: package.name.clone(),
        });
    }
    let staged = layer_root.join(target.prefix.trim_start_matches('/'));
    let prefix = Prefix::create(&staged).map_err(|source| InstallError::Write {
        path: staged.clone(),
        source,
    })?;
    let options = InstallOptions {
        target_prefix: Some(PathBuf::from(&target.prefix)),
        platform: Some(target.platform),
        python_info: target.python.clone(),
        // Copies only: a hard link into the cache would let a later prefix replacement change the cached file.
        allow_hard_links: Some(false),
        allow_ref_links: Some(false),
        allow_symbolic_links: Some(true),
        ..InstallOptions::default()
    };
    // A registry per package: in an image the layer above simply wins, so there is nothing to rename.
    let registry = Arc::new(Mutex::new(Default::default()));
    let (paths, _) =
        link_package_sync(package_dir, &prefix, registry, options).map_err(|source| InstallError::Link {
            name: package.name.clone(),
            source: Box::new(source),
        })?;
    let prefix_record = PrefixRecord::from_repodata_record(record, paths);
    let meta = staged.join("conda-meta").join(prefix_record.file_name());
    prefix_record
        .write_to_path(&meta, true)
        .map_err(|source| InstallError::Write { path: meta, source })
}

/// Paths a staged layer holds that do not belong in the image: the backup marker rattler writes into every new
/// prefix, and the empty history file except in the bottom layer.
pub fn skip_in_layer(prefix: &str, first: bool) -> impl Fn(&str) -> bool {
    let prefix = prefix.trim_matches('/').to_string();
    move |path: &str| {
        let Some(rest) = path.strip_prefix(&prefix).and_then(|rest| rest.strip_prefix('/')) else {
            return false;
        };
        rest == "CACHEDIR.TAG" || (!first && rest == "conda-meta/history")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_knows_python_when_there_is_one() {
        let target = Target::new("/opt/env/", "linux-64", Some("3.13.18")).unwrap();
        assert_eq!(target.prefix, "/opt/env");
        assert_eq!(target.python.unwrap().short_version, (3, 13));
        let target = Target::new("/opt/env", "linux-aarch64", None).unwrap();
        assert!(target.python.is_none());
        assert!(matches!(
            Target::new("/opt/env", "not-a-platform", None),
            Err(InstallError::Platform { .. })
        ));
    }

    #[test]
    fn layers_drop_rattlers_markers() {
        let first = skip_in_layer("/opt/env", true);
        assert!(first("opt/env/CACHEDIR.TAG"));
        assert!(!first("opt/env/conda-meta/history"));
        assert!(!first("opt/env/bin/python"));
        assert!(!first("CACHEDIR.TAG"));
        let later = skip_in_layer("/opt/env", false);
        assert!(later("opt/env/conda-meta/history"));
        assert!(!later("opt/env/conda-meta/zlib-1.3.2-h0_0.json"));
    }

    #[test]
    fn a_package_without_a_record_is_refused() {
        let package = Package {
            name: "local".to_string(),
            version: "1".to_string(),
            kind: crate::lock::Kind::Conda,
            build: None,
            depends: Vec::new(),
            location: "pkgs/local-1-0.conda".to_string(),
            sha256: None,
            size: None,
            record: None,
        };
        let target = Target::new("/opt/env", "linux-64", None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = install(&package, dir.path(), dir.path(), &target).unwrap_err();
        assert!(matches!(err, InstallError::NoRecord { .. }), "{err}");
    }
}
