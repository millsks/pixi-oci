//! `digest`: the digest a registry serves for a tag, to pin a base with, fetching only manifests.

use crate::platform::OciPlatform;
use crate::registry::{ImageRef, Registry, RegistryError};

/// The digest `reference` names: the tag's own (an index or a manifest), or with `platform`, that platform's
/// manifest. With `pinned`, the whole reference as it was given, with the digest added, ready for `--base`.
pub fn digest(
    given: &str,
    reference: &ImageRef,
    platform: Option<&OciPlatform>,
    pinned: bool,
    registry: &Registry,
) -> Result<String, RegistryError> {
    let mut digest = registry.resolve(reference)?;
    if let Some(platform) = platform {
        let resolved = ImageRef {
            digest: Some(digest.clone()),
            ..reference.clone()
        };
        let (platform_digest, manifest) = registry.platform_manifest(&resolved, platform)?;
        if platform_digest == digest {
            // A single manifest says nothing of its platform: its config does.
            let config_digest = manifest["config"]["digest"].as_str().unwrap_or_default();
            let (bytes, _) = registry.document(reference, "blobs", config_digest, "the config")?;
            let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
            if config["os"] != platform.os || config["architecture"] != platform.architecture {
                return Err(RegistryError::NoPlatform {
                    reference: reference.to_string(),
                    platform: platform.to_string(),
                    available: vec![format!(
                        "{}/{}",
                        config["os"].as_str().unwrap_or("?"),
                        config["architecture"].as_str().unwrap_or("?")
                    )],
                });
            }
        }
        digest = platform_digest;
    }
    Ok(if pinned {
        let name = given.split_once('@').map_or(given, |(name, _)| name);
        format!("{name}@{digest}")
    } else {
        digest
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    fn a_tag_gives_the_digest_it_was_pushed_with_without_fetching_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let plan = testing::plan_of(vec![testing::simple(dir.path(), "tool", &[])], 100);
        let built = dir.path().join("built.tar");
        let written = testing::build_image(dir.path(), &plan, &built);
        let server = fake(Behaviour::default());
        let given = format!("{}/team/app:1.0", server.host);
        let reference = ImageRef::parse(&given).unwrap();
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
        let expected = &written.manifest.digest;
        assert_eq!(&digest(&given, &reference, None, false, &registry).unwrap(), expected);
        let amd64 = crate::platform::oci_platform("linux-64").unwrap();
        assert_eq!(
            &digest(&given, &reference, Some(&amd64), false, &registry).unwrap(),
            expected
        );
        assert_eq!(
            digest(&given, &reference, None, true, &registry).unwrap(),
            format!("{given}@{expected}")
        );
        // A reference already pinned is answered from itself, and --pinned replaces nothing it should keep.
        let pinned_given = format!("{given}@{expected}");
        let pinned = ImageRef::parse(&pinned_given).unwrap();
        assert_eq!(
            digest(&pinned_given, &pinned, None, true, &registry).unwrap(),
            pinned_given
        );
        let requests = server.store.lock().unwrap().requests.clone();
        // Only the config, to know a single manifest's platform; never a layer.
        let config_only = |r: &&String| !r.contains("/blobs/") || r.contains(&written.config.digest);
        assert!(requests.iter().all(|r| config_only(&r)), "{requests:#?}");

        // The digest is the one pull resolves.
        let pulled = crate::pull::pull(&reference, &amd64, &dir.path().join("p.tar"), false, &registry).unwrap();
        assert_eq!(&pulled.digest, expected);

        let arm = crate::platform::oci_platform("linux-aarch64").unwrap();
        let err = digest(&given, &reference, Some(&arm), false, &registry).unwrap_err();
        assert!(matches!(err, RegistryError::NoPlatform { .. }), "{err}");
        assert!(err.to_string().contains("linux/arm64"), "{err}");
    }
}
