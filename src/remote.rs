//! An image in a registry, read for `inspect`, `validate` and `check` as if it were a layout on disk.
//!
//! The image is written to a temporary layout through the registry client's blob cache: for `inspect` only its
//! manifest and config, so no layer is downloaded; for `validate` and `check` its layers too, which a later `pull`
//! then finds in the cache.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::platform::OciPlatform;
use crate::pull::PullError;
use crate::registry::{ImageRef, Registry, RegistryError};

/// The registry reference `image` names, or `None` when it is a file or directory.
///
/// An existing path is always a path. `docker://REF` is always a reference, as skopeo writes one. Otherwise it is a
/// reference when it has a tag, a digest or a repository path (`debian:bookworm-slim`, `ghcr.io/team/app`) and does
/// not look like a path (`.tar`, `./x`, `/x`, `C:\x`).
pub fn reference(image: &Path) -> Result<Option<ImageRef>, RegistryError> {
    if image.exists() {
        return Ok(None);
    }
    let Some(text) = image.to_str() else {
        return Ok(None);
    };
    if let Some(explicit) = text.strip_prefix("docker://") {
        return ImageRef::parse(explicit).map(Some);
    }
    let path_like = text.ends_with(".tar")
        || text.starts_with('.')
        || text.starts_with('/')
        || text.starts_with('~')
        || text.contains('\\')
        || image.is_absolute();
    if path_like || !text.contains([':', '@', '/']) {
        return Ok(None);
    }
    Ok(ImageRef::parse(text).ok())
}

/// An image fetched from a registry into a temporary layout, which goes when this does.
pub struct RemoteImage {
    _dir: tempfile::TempDir,
    pub layout: PathBuf,
    /// The reference, pinned to the digest its tag resolved to.
    pub name: String,
    /// The document the registry serves for the reference: an index, or the image's manifest.
    pub served: Vec<u8>,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RemoteError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Registry(#[from] RegistryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Pull(#[from] PullError),

    #[error("{reference} is for {platform}, which is not a Linux platform pixi-oci knows")]
    UnknownPlatform { reference: String, platform: String },

    #[error("`{value}` is not a platform")]
    #[diagnostic(help("give a conda platform (linux-64, linux-aarch64) or an OCI one (linux/amd64, linux/arm64/v8)"))]
    BadPlatform { value: String },

    #[error("could not make a temporary directory")]
    Temp(#[source] std::io::Error),
}

/// Fetch `reference` for `platform` (or the one it has, or the host's Linux from an index) into a temporary layout,
/// with its layers when `layers` is set.
pub fn fetch(
    reference: &ImageRef,
    platform: Option<&str>,
    layers: bool,
    registry: &Registry,
) -> Result<RemoteImage, RemoteError> {
    let asked = platform
        .map(|value| {
            crate::platform::parse(value).ok_or_else(|| RemoteError::BadPlatform {
                value: value.to_string(),
            })
        })
        .transpose()?;
    let digest = registry.resolve(reference)?;
    let pinned = ImageRef {
        digest: Some(digest.clone()),
        ..reference.clone()
    };
    let (served, _) = registry.document(&pinned, "manifests", &digest, "the manifest")?;
    let top: Value = serde_json::from_slice(&served).unwrap_or_default();
    let is_index = top["manifests"].is_array();
    let host =
        || crate::platform::oci_platform(crate::platform::default_conda_platform()).expect("the default is Linux");
    // From an index, the platform asked for or the host's; a single manifest is for the one its config names.
    let wanted = asked.clone().unwrap_or_else(host);
    let (manifest_digest, manifest) = registry.platform_manifest(&pinned, &wanted)?;
    let config_digest = manifest["config"]["digest"].as_str().unwrap_or_default().to_string();
    let (config_bytes, _) = registry.document(&pinned, "blobs", &config_digest, "the config")?;
    let config: Value = serde_json::from_slice(&config_bytes).unwrap_or_default();
    let os = config["os"].as_str().unwrap_or("?");
    let architecture = config["architecture"].as_str().unwrap_or("?");
    let variant = config["variant"].as_str();
    let named = format!(
        "{os}/{architecture}{}",
        variant.map(|v| format!("/{v}")).unwrap_or_default()
    );
    let platform: OciPlatform =
        crate::platform::from_oci(os, architecture, variant).ok_or_else(|| RemoteError::UnknownPlatform {
            reference: pinned.to_string(),
            platform: named.clone(),
        })?;
    if !is_index
        && let Some(asked) = &asked
        && *asked != platform
    {
        return Err(RegistryError::NoPlatform {
            reference: pinned.to_string(),
            platform: asked.to_string(),
            available: vec![platform.to_string()],
        }
        .into());
    }

    let dir = tempfile::tempdir().map_err(RemoteError::Temp)?;
    let layout = dir.path().join("layout");
    if layers {
        crate::pull::pull(&pinned, &platform, &layout, false, registry)?;
    } else {
        let media_type = manifest["mediaType"]
            .as_str()
            .unwrap_or(crate::image::MANIFEST_MEDIA_TYPE)
            .to_string();
        crate::pull::write_layout(
            registry,
            &layout,
            &platform,
            &pinned,
            &manifest_digest,
            &media_type,
            &config_digest,
            &[],
        )?;
    }
    Ok(RemoteImage {
        _dir: dir,
        layout,
        name: pinned.to_string(),
        served,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CredentialSource, Credentials};
    use crate::testing;
    use crate::testing::registry::{Behaviour, fake};

    struct Anonymous;

    impl CredentialSource for Anonymous {
        fn credentials(&self, _: &str) -> Result<Option<Credentials>, RegistryError> {
            Ok(None)
        }
    }

    fn client(root: &Path) -> Registry {
        Registry::with(root.to_path_buf(), Box::new(Anonymous), None)
    }

    #[test]
    fn references_and_paths_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("ghcr.io:app");
        std::fs::write(&existing, b"").unwrap();
        assert!(reference(&existing).unwrap().is_none());
        for path in [
            "app.tar",
            "./layout",
            "/abs/layout",
            "~/x:1",
            "C:\\images\\app",
            "layout",
        ] {
            assert!(reference(Path::new(path)).unwrap().is_none(), "{path}");
        }
        for (given, expected) in [
            ("debian:bookworm-slim", "docker.io/library/debian:bookworm-slim"),
            ("ghcr.io/team/app", "ghcr.io/team/app"),
            ("docker://debian", "docker.io/library/debian"),
        ] {
            assert_eq!(
                reference(Path::new(given)).unwrap().unwrap().to_string(),
                expected,
                "{given}"
            );
        }
        assert!(reference(Path::new("docker://Not A Reference")).is_err());
    }

    #[test]
    fn a_registry_image_reads_as_a_layout_and_inspect_downloads_no_layer() {
        let dir = tempfile::tempdir().unwrap();
        let plan = testing::plan_of(
            vec![
                testing::simple(dir.path(), "tool", &["base"]),
                testing::simple(dir.path(), "base", &[]),
            ],
            100,
        );
        let built = dir.path().join("built.tar");
        let written = testing::build_image(dir.path(), &plan, &built);
        let server = fake(Behaviour::default());
        let reference = ImageRef::parse(&format!("{}/team/app:1.0", server.host)).unwrap();
        crate::push::push(
            &built,
            None,
            &reference,
            &client(&dir.path().join("push")),
            &mut std::io::sink(),
        )
        .unwrap();
        server.store.lock().unwrap().requests.clear();
        let registry = client(&dir.path().join("cache"));

        // Settings only: manifest and config, no layer.
        let settings = fetch(&reference, None, false, &registry).unwrap();
        assert!(
            settings
                .name
                .ends_with(&format!("/team/app:1.0@{}", written.manifest.digest)),
            "{}",
            settings.name
        );
        let mut inspection = crate::inspect::inspect(&settings.layout, None).unwrap();
        inspection.image = settings.name.clone();
        assert_eq!(inspection.manifest.digest, written.manifest.digest);
        assert_eq!(inspection.layers.len(), 2);
        assert_eq!(inspection.platform.as_deref(), Some("linux/amd64"));
        let requests = server.store.lock().unwrap().requests.clone();
        let blobs: Vec<_> = requests.iter().filter(|r| r.contains("/blobs/")).collect();
        assert_eq!(blobs.len(), 1, "only the config: {requests:#?}");
        assert!(blobs[0].contains(&written.config.digest));
        let served: Value = serde_json::from_slice(&settings.served).unwrap();
        assert_eq!(served["config"]["digest"], written.config.digest.as_str());

        // With layers, the image validates and checks as the file it was pushed from does.
        let whole = fetch(&reference, Some("linux-64"), true, &registry).unwrap();
        let report = crate::validate::validate(&whole.layout).unwrap();
        assert!(report.valid, "{}", report.text());
        let remote = crate::check::check(&whole.layout, None).unwrap();
        let local = crate::check::check(&built, None).unwrap();
        assert_eq!(remote.ok, local.ok);
        assert_eq!(remote.problems.len(), local.problems.len());
        assert_eq!(remote.checked.files, local.checked.files);
        drop(whole);

        // A single manifest is for the platform its config names: another is refused, either spelling.
        for other in ["linux-aarch64", "linux/arm64/v8"] {
            let err = fetch(&reference, Some(other), false, &registry).err().unwrap();
            assert!(err.to_string().contains("has no image for linux/arm64/v8"), "{err}");
        }
        let err = fetch(&reference, Some("beos/amd64"), false, &registry).err().unwrap();
        assert!(matches!(err, RemoteError::BadPlatform { .. }), "{err}");
    }
}
