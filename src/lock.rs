//! What an image is built from: the packages `pixi.lock` locks for one environment and platform.

use std::path::{Path, PathBuf};

use rattler_conda_types::RepoDataRecord;
use rattler_lock::{CondaPackageData, LockFile, LockedPackage, UrlOrPath};
use serde::Serialize;

/// Why a lockfile could not be read into packages.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum LockError {
    #[error("could not read {path}")]
    #[diagnostic(help("run `pixi lock` in the workspace, or pass the lockfile with --lockfile"))]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not a lockfile pixi-oci can read")]
    #[diagnostic(help("a lockfile written by a newer pixi may need a newer pixi-oci"))]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<rattler_lock::ParseCondaLockError>,
    },

    #[error("the lockfile has no environment named `{name}`")]
    #[diagnostic(help("environments in this lockfile: {}", .available.join(", ")))]
    EnvironmentNotFound { name: String, available: Vec<String> },

    #[error("environment `{environment}` is not locked for {platform}")]
    #[diagnostic(help(
        "add it to the workspace's platforms (`pixi workspace platform add {platform}`); locked: {}",
        .available.join(", ")
    ))]
    PlatformNotFound {
        platform: String,
        environment: String,
        available: Vec<String>,
    },
}

/// Where a package comes from, which decides how it gets into an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A built conda package, downloaded and extracted as it is.
    Conda,
    /// A conda package pixi builds from source; there is no archive to extract yet.
    CondaSource,
    /// A PyPI distribution, installed into the environment's site-packages.
    Pypi,
}

/// One locked package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub kind: Kind,
    /// Conda build string, such as `h5888daf_1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Names the package depends on, from its conda record. Version constraints are dropped: the lockfile already
    /// chose the version, and only the order between packages is wanted here.
    #[serde(skip)]
    pub depends: Vec<String>,
    /// Where the archive is fetched from, a URL or a path.
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Archive size in bytes, where the lockfile records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The repodata record of a built conda package, which its `conda-meta` record in the image is written from.
    #[serde(skip)]
    pub record: Option<Box<RepoDataRecord>>,
}

/// The packages of one environment on one platform, in the lockfile's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locked {
    pub environment: String,
    pub platform: String,
    pub packages: Vec<Package>,
}

/// Read and parse the lockfile at `path`.
pub fn load(path: &Path) -> Result<LockFile, LockError> {
    let contents = std::fs::read_to_string(path).map_err(|source| LockError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    LockFile::from_str_with_base_directory(&contents, path.parent()).map_err(|source| LockError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// The packages `lock` locks for `environment` on `platform`.
pub fn select(lock: &LockFile, environment: &str, platform: &str) -> Result<Locked, LockError> {
    let env = lock
        .environment(environment)
        .ok_or_else(|| LockError::EnvironmentNotFound {
            name: environment.to_string(),
            available: environment_names(lock),
        })?;
    let locked_platform = env.platforms().find(|p| p.name().as_str() == platform).ok_or_else(|| {
        let mut available: Vec<String> = env.platforms().map(|p| p.name().to_string()).collect();
        available.sort();
        LockError::PlatformNotFound {
            platform: platform.to_string(),
            environment: environment.to_string(),
            available,
        }
    })?;
    let packages = env
        .packages(locked_platform)
        .map(|packages| packages.map(package).collect())
        .unwrap_or_default();
    Ok(Locked {
        environment: environment.to_string(),
        platform: platform.to_string(),
        packages,
    })
}

/// Every environment in the lockfile: `default` first, the rest alphabetical.
pub fn environment_names(lock: &LockFile) -> Vec<String> {
    let mut names: Vec<String> = lock.environments().map(|(name, _)| name.to_string()).collect();
    names.sort_by_key(|name| (name != "default", name.clone()));
    names
}

fn package(locked: &LockedPackage) -> Package {
    match locked {
        LockedPackage::Conda(conda) => conda_package(conda),
        LockedPackage::Pypi(pypi) => Package {
            name: pypi.name().to_string(),
            version: pypi.version().map(ToString::to_string).unwrap_or_default(),
            kind: Kind::Pypi,
            build: None,
            depends: Vec::new(),
            location: location_string(pypi.location().inner()),
            sha256: pypi
                .as_wheel()
                .and_then(|wheel| wheel.hash.as_ref())
                .and_then(|hash| hash.sha256())
                .map(hex),
            size: None,
            record: None,
        },
    }
}

fn conda_package(conda: &CondaPackageData) -> Package {
    let record = conda.record();
    let (kind, repodata) = match conda {
        // A binary package whose location is not a URL (a relative path) has no repodata record; building refuses it.
        CondaPackageData::Binary(binary) => (
            Kind::Conda,
            RepoDataRecord::try_from(binary.as_ref()).ok().map(Box::new),
        ),
        CondaPackageData::Source(_) => (Kind::CondaSource, None),
    };
    Package {
        name: conda.name().as_normalized().to_string(),
        version: record
            .map(|record| record.version.as_str().into_owned())
            .unwrap_or_default(),
        kind,
        build: record
            .map(|record| record.build.as_str().to_string())
            .filter(|build| !build.is_empty()),
        depends: record
            .map(|record| record.depends.iter().map(|spec| spec_name(spec)).collect())
            .unwrap_or_default(),
        location: location_string(conda.location()),
        sha256: record.and_then(|record| record.sha256.as_ref()).map(hex),
        size: record.and_then(|record| record.size),
        record: repodata,
    }
}

/// The package name a match spec such as `libgcc >=13` or `python_abi 3.13.* *_cp313` names.
fn spec_name(spec: &str) -> String {
    spec.split([' ', '=', '<', '>', '!', '~', '['])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn location_string(location: &UrlOrPath) -> String {
    match location {
        UrlOrPath::Url(url) => url.to_string(),
        UrlOrPath::Path(path) => path.to_string(),
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_names_drop_every_kind_of_constraint() {
        assert_eq!(spec_name("libgcc >=13"), "libgcc");
        assert_eq!(spec_name("python_abi 3.13.* *_cp313"), "python_abi");
        assert_eq!(spec_name("openssl>=3.5.0,<4.0a0"), "openssl");
        assert_eq!(spec_name("tzdata"), "tzdata");
        assert_eq!(spec_name("__glibc >=2.17,<3.0.a0"), "__glibc");
        assert_eq!(spec_name("numpy[version='>=1.21']"), "numpy");
        assert_eq!(spec_name("pip!=24.0"), "pip");
        assert_eq!(spec_name("ca-certificates~=2025"), "ca-certificates");
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex([0x00, 0x0f, 0xab]), "000fab");
    }
}
