//! What every package of one environment is installed for: the prefix in the image, the platform, and python.

use rattler::install::PythonInfo;
use rattler_conda_types::{Subdir, Version};

/// Why an environment's install target could not be set up.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum TargetError {
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
    pub fn new(prefix: &str, platform: &str, python_version: Option<&str>) -> Result<Self, TargetError> {
        let platform: Subdir = platform.parse().map_err(|_| TargetError::Platform {
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
            Err(TargetError::Platform { .. })
        ));
    }
}
