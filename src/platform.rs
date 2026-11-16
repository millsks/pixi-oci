//! Conda platforms and the OCI platforms an image built from them runs on.
//!
//! A container image carries Linux userland, so only conda's `linux-*` subdirectories have an OCI platform. The
//! mapping follows the architecture names in the OCI image index spec, which are Go's `GOARCH` values.

use std::fmt;

use serde::Serialize;

/// The platform an OCI image declares in its config and in an image index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciPlatform {
    /// Always `linux` for images built from conda packages.
    pub os: &'static str,
    /// `amd64`, `arm64`, `ppc64le`, ...
    pub architecture: &'static str,
    /// Only where the spec gives the architecture variants, such as `v8` for `arm64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<&'static str>,
}

impl fmt::Display for OciPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(variant) = self.variant {
            write!(f, "/{variant}")?;
        }
        Ok(())
    }
}

/// The OCI platform for a conda platform name, or `None` when images cannot be built for it.
pub fn oci_platform(conda_platform: &str) -> Option<OciPlatform> {
    let (architecture, variant) = match conda_platform {
        "linux-64" => ("amd64", None),
        "linux-aarch64" => ("arm64", Some("v8")),
        "linux-ppc64le" => ("ppc64le", None),
        "linux-s390x" => ("s390x", None),
        "linux-armv7l" => ("arm", Some("v7")),
        "linux-armv6l" => ("arm", Some("v6")),
        "linux-32" => ("386", None),
        "linux-riscv64" => ("riscv64", None),
        _ => return None,
    };
    Some(OciPlatform {
        os: "linux",
        architecture,
        variant,
    })
}

/// The conda platforms images are built for.
const LINUX: [&str; 8] = [
    "linux-64",
    "linux-aarch64",
    "linux-ppc64le",
    "linux-s390x",
    "linux-armv7l",
    "linux-armv6l",
    "linux-32",
    "linux-riscv64",
];

/// The OCI platform an image config or a descriptor names (`os`, `architecture`, `variant`), when it is one of the
/// Linux platforms above. An arm64 image that leaves out its variant is v8, the only one there is.
pub fn from_oci(os: &str, architecture: &str, variant: Option<&str>) -> Option<OciPlatform> {
    LINUX.iter().filter_map(|name| oci_platform(name)).find(|platform| {
        platform.os == os
            && platform.architecture == architecture
            && (platform.variant == variant || (architecture == "arm64" && variant.is_none()))
    })
}

/// A platform as a person gives one: a conda platform (`linux-aarch64`) or an OCI one (`linux/arm64/v8`).
pub fn parse(text: &str) -> Option<OciPlatform> {
    oci_platform(text).or_else(|| {
        let mut parts = text.split('/');
        let (os, architecture) = (parts.next()?, parts.next()?);
        let variant = parts.next();
        if parts.next().is_some() {
            return None;
        }
        from_oci(os, architecture, variant)
    })
}

/// The Linux platform a build defaults to: the host's architecture, so an image built on an Apple Silicon Mac runs
/// natively there, and `linux-64` anywhere else.
pub fn default_conda_platform() -> &'static str {
    default_for_arch(std::env::consts::ARCH)
}

fn default_for_arch(arch: &str) -> &'static str {
    match arch {
        "aarch64" => "linux-aarch64",
        "powerpc64" => "linux-ppc64le",
        "s390x" => "linux-s390x",
        _ => "linux-64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_platforms_map_to_oci_architectures() {
        let cases = [
            ("linux-64", "linux/amd64"),
            ("linux-aarch64", "linux/arm64/v8"),
            ("linux-ppc64le", "linux/ppc64le"),
            ("linux-s390x", "linux/s390x"),
            ("linux-armv7l", "linux/arm/v7"),
            ("linux-armv6l", "linux/arm/v6"),
            ("linux-32", "linux/386"),
            ("linux-riscv64", "linux/riscv64"),
        ];
        for (conda, oci) in cases {
            assert_eq!(oci_platform(conda).unwrap().to_string(), oci, "{conda}");
        }
    }

    #[test]
    fn non_linux_platforms_have_no_image_platform() {
        for conda in ["osx-arm64", "osx-64", "win-64", "noarch", "emscripten-wasm32"] {
            assert_eq!(oci_platform(conda), None, "{conda}");
        }
    }

    #[test]
    fn the_default_follows_the_host_architecture() {
        assert_eq!(default_for_arch("x86_64"), "linux-64");
        assert_eq!(default_for_arch("aarch64"), "linux-aarch64");
        assert_eq!(default_for_arch("powerpc64"), "linux-ppc64le");
        assert_eq!(default_for_arch("s390x"), "linux-s390x");
        assert_eq!(default_for_arch("riscv64"), "linux-64");
        assert!(oci_platform(default_conda_platform()).is_some());
    }

    #[test]
    fn platforms_are_read_in_either_spelling() {
        assert_eq!(parse("linux-aarch64").unwrap().to_string(), "linux/arm64/v8");
        assert_eq!(parse("linux/arm64/v8").unwrap().to_string(), "linux/arm64/v8");
        assert_eq!(parse("linux/arm64").unwrap().to_string(), "linux/arm64/v8");
        assert_eq!(parse("linux/amd64").unwrap().to_string(), "linux/amd64");
        assert_eq!(parse("linux/arm/v7").unwrap().to_string(), "linux/arm/v7");
        for unknown in ["linux/arm", "windows/amd64", "linux/amd64/v2/x", "osx-arm64", "linux"] {
            assert!(parse(unknown).is_none(), "{unknown}");
        }
        assert_eq!(from_oci("linux", "s390x", None).unwrap().to_string(), "linux/s390x");
    }
}
