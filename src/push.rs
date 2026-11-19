//! `push`: an image layout or archive sent to a registry, with no Docker.
//!
//! The image is validated first. Then each blob the manifest names (its config, then its layers) is asked for with a
//! `HEAD` and sent only if the repository does not have it: mounted from the base image's repository when the base
//! lives on the same registry, uploaded otherwise. The manifest goes last, under the tag, and the digest the registry
//! answers with must be the image's.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::inspect::{InspectError, Raw, inspect, raw};
use crate::read::{Image, blob_path};
use crate::registry::{ImageRef, Registry, RegistryError};
use crate::report::{plural, size_text};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PushError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Inspect(#[from] InspectError),

    #[error("{image} is not a valid OCI image: {}", plural(*.problems, "problem"))]
    #[diagnostic(help("`pixi oci validate {image}` lists them"))]
    Invalid { image: String, problems: usize },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Registry(#[from] RegistryError),

    #[error("{destination} is pinned to {pinned}, but the image's manifest is {digest}")]
    WrongDigest {
        destination: String,
        pinned: String,
        digest: String,
    },

    #[error("{registry} stored the manifest as {stored}, not {digest}")]
    #[diagnostic(help("the registry rewrote the manifest; pull it by the digest it reports, or report this"))]
    Stored {
        registry: String,
        stored: String,
        digest: String,
    },
}

/// What a push did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pushed {
    /// Where the manifest went: the destination with its tag, or its digest.
    pub reference: String,
    pub digest: String,
    /// Blobs sent, and their bytes.
    pub uploaded: usize,
    pub uploaded_size: u64,
    /// Blobs the registry mounted from the base image's repository.
    pub mounted: usize,
    /// Blobs the repository had already.
    pub present: usize,
}

impl Pushed {
    pub fn text(&self) -> String {
        format!(
            "Pushed {}\n  digest {}\n  {}: {} uploaded ({}), {} mounted, {} already there\n",
            self.reference,
            self.digest,
            plural(self.uploaded + self.mounted + self.present, "blob"),
            self.uploaded,
            size_text(Some(self.uploaded_size)),
            self.mounted,
            self.present
        )
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a push always serializes")
    }
}

/// Push the image at `path` (its manifest for `platform`, when it holds several) to `destination`.
pub fn push(
    path: &Path,
    platform: Option<&str>,
    destination: &ImageRef,
    registry: &Registry,
    progress: &mut dyn Write,
) -> Result<Pushed, PushError> {
    let report = crate::validate::validate(path).map_err(InspectError::from)?;
    if !report.valid {
        return Err(PushError::Invalid {
            image: path.display().to_string(),
            problems: report.problems.len(),
        });
    }
    let inspection = inspect(path, platform)?;
    let manifest_text = raw(path, platform, Raw::Manifest)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).map_err(|err| InspectError::Broken {
        image: path.display().to_string(),
        message: format!("its manifest is not JSON: {err}"),
    })?;
    let digest = inspection.manifest.digest.clone();
    if let Some(pinned) = &destination.digest
        && *pinned != digest
    {
        return Err(PushError::WrongDigest {
            destination: destination.to_string(),
            pinned: pinned.clone(),
            digest,
        });
    }
    // The base image, when it lives on the destination's registry, can lend its layers without an upload.
    let mount_from = manifest["annotations"]["org.opencontainers.image.base.name"]
        .as_str()
        .and_then(|name| ImageRef::parse(name).ok())
        .filter(|base| base.registry == destination.registry && base.repository != destination.repository)
        .map(|base| base.repository);

    let image = Image::open(path).map_err(InspectError::from)?;
    let mut blobs: Vec<(String, u64)> = vec![(inspection.config.digest.clone(), inspection.config.size)];
    blobs.extend(inspection.layers.iter().map(|layer| (layer.digest.clone(), layer.size)));
    let mut pushed = Pushed {
        reference: String::new(),
        digest: digest.clone(),
        uploaded: 0,
        uploaded_size: 0,
        mounted: 0,
        present: 0,
    };
    let total = blobs.len();
    for (i, (blob, size)) in blobs.iter().enumerate() {
        let short: String = blob.chars().take("sha256:".len() + 12).collect();
        if registry.has_blob(destination, blob)? {
            pushed.present += 1;
            let _ = writeln!(progress, "[{}/{total}] {short} already there", i + 1);
            continue;
        }
        let mount = mount_from.as_deref().map(|from| (blob.as_str(), from));
        match registry.start_upload(destination, mount)? {
            None => {
                pushed.mounted += 1;
                let _ = writeln!(
                    progress,
                    "[{}/{total}] {short} mounted from {}",
                    i + 1,
                    mount_from.as_deref().unwrap_or_default()
                );
            }
            Some(start) => {
                let _ = writeln!(
                    progress,
                    "[{}/{total}] {short} uploading ({})",
                    i + 1,
                    size_text(Some(*size))
                );
                let name = blob_path(blob).expect("a validated image's digests are sha256");
                registry.upload(destination, &start, blob, &image, &name, *size)?;
                pushed.uploaded += 1;
                pushed.uploaded_size += size;
            }
        }
    }

    let media_type = manifest["mediaType"]
        .as_str()
        .unwrap_or(crate::image::MANIFEST_MEDIA_TYPE)
        .to_string();
    let target = match (&destination.digest, &destination.tag) {
        (Some(pinned), _) => pinned.clone(),
        (None, Some(tag)) => tag.clone(),
        (None, None) => "latest".to_string(),
    };
    let stored = registry.put_manifest(destination, &target, manifest_text.as_bytes(), &media_type)?;
    if let Some(stored) = stored
        && stored != digest
    {
        return Err(PushError::Stored {
            registry: destination.registry.clone(),
            stored,
            digest,
        });
    }
    let name = format!("{}/{}", destination.registry, destination.repository);
    pushed.reference = if destination.digest.is_some() {
        format!("{name}@{digest}")
    } else {
        format!("{name}:{target}")
    };
    Ok(pushed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CredentialSource, Credentials};
    use crate::testing;
    use crate::testing::registry::{Behaviour, Fake, fake, sha};

    struct Static(Option<Credentials>);

    impl CredentialSource for Static {
        fn credentials(&self, _: &str) -> Result<Option<Credentials>, RegistryError> {
            Ok(self.0.clone())
        }
    }

    fn client(dir: &Path) -> Registry {
        Registry::with(dir.join("cache"), Box::new(Static(None)), None)
    }

    /// An image of `packages` (`name`, with its file's content), a layer each.
    fn image(dir: &Path, name: &str, packages: &[(&str, &str)]) -> std::path::PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let packages = packages
            .iter()
            .map(|(package, content)| {
                testing::package(
                    &root,
                    testing::Spec::new(package).file("share/data.txt", *content, false, None),
                )
            })
            .collect();
        let plan = testing::plan_with(packages, crate::plan::Layering::PerPackage, 100);
        let output = root.join("image.tar");
        testing::build_image(&root, &plan, &output);
        output
    }

    fn destination(fake: &Fake, path: &str) -> ImageRef {
        ImageRef::parse(&format!("{}/{path}", fake.host)).unwrap()
    }

    #[test]
    fn large_blobs_go_up_in_chunks_a_size_limit_lets_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a"), ("beta", "b")]);
        let image = crate::read::Image::open(&path).unwrap();
        let limit = 200;
        // Behind a token, so every chunk is also sent again after the registry's challenge.
        let fake = fake(Behaviour {
            tokens: true,
            max_body: Some(limit),
            ..Behaviour::default()
        });
        let to = destination(&fake, "team/app:1.0");
        // In one request each, a layer is over the proxy's limit.
        let err = push(&path, None, &to, &client(dir.path()), &mut std::io::sink()).unwrap_err();
        assert!(err.to_string().contains("413"), "{err}");

        fake.store.lock().unwrap().requests.clear();
        let chunked = client(dir.path()).with_chunk_size(Some(100));
        let pushed = push(&path, None, &to, &chunked, &mut std::io::sink()).unwrap();
        assert_eq!(pushed.uploaded, 3);
        let store = fake.store.lock().unwrap();
        let patches = store.requests.iter().filter(|r| r.starts_with("PATCH ")).count();
        let mut expected = 0;
        for ((repo, digest), blob) in &store.blobs {
            assert_eq!(repo, "team/app");
            assert_eq!(&sha(blob), digest);
            let name = blob_path(digest).unwrap();
            assert_eq!(image.size(&name), Some(blob.len() as u64));
            if blob.len() > 100 {
                expected += blob.len().div_ceil(100);
            }
        }
        assert!(expected > 0);
        assert_eq!(patches, expected, "{:#?}", store.requests);
        assert!(store.partial.is_empty());
    }

    #[test]
    fn a_registry_minimum_chunk_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let fake = fake(Behaviour {
            chunk_min: Some(150),
            ..Behaviour::default()
        });
        let to = destination(&fake, "team/app:1.0");
        let chunked = client(dir.path()).with_chunk_size(Some(10));
        push(&path, None, &to, &chunked, &mut std::io::sink()).unwrap();
        let store = fake.store.lock().unwrap();
        let patches = store.requests.iter().filter(|r| r.starts_with("PATCH ")).count();
        let expected: usize = store
            .blobs
            .values()
            .filter(|blob| blob.len() > 10)
            .map(|blob| blob.len().div_ceil(150))
            .sum();
        assert_eq!(patches, expected);

        // Without a chunk size, nothing is chunked: a POST and a PUT for each blob, as before.
        drop(store);
        let other = destination(&fake, "team/other:1.0");
        fake.store.lock().unwrap().requests.clear();
        push(&path, None, &other, &client(dir.path()), &mut std::io::sink()).unwrap();
        let requests = fake.store.lock().unwrap().requests.clone();
        assert!(!requests.iter().any(|r| r.starts_with("PATCH ")), "{requests:#?}");
    }

    #[test]
    fn a_second_push_finds_everything_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a"), ("beta", "b")]);
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app:1.0");
        let mut progress = Vec::new();
        let first = push(&path, None, &to, &client(dir.path()), &mut progress).unwrap();
        assert_eq!((first.uploaded, first.mounted, first.present), (3, 0, 0));
        assert_eq!(first.reference, format!("{}/team/app:1.0", fake.host));
        let progress = String::from_utf8(progress).unwrap();
        assert!(
            progress.starts_with("[1/3] sha256:") && progress.contains(" uploading ("),
            "{progress}"
        );
        let stored = fake.store.lock().unwrap().manifests[&("team/app".to_string(), "1.0".to_string())].clone();
        assert_eq!(sha(&stored), first.digest);
        assert_eq!(
            String::from_utf8(stored).unwrap(),
            raw(&path, None, Raw::Manifest).unwrap()
        );

        let second = push(&path, None, &to, &client(dir.path()), &mut std::io::sink()).unwrap();
        assert_eq!((second.uploaded, second.mounted, second.present), (0, 0, 3));
        assert_eq!(second.digest, first.digest);
        assert!(
            second
                .text()
                .contains("3 blobs: 0 uploaded (0 B), 0 mounted, 3 already there"),
            "{}",
            second.text()
        );
        let json: serde_json::Value = serde_json::from_str(&second.json()).unwrap();
        assert_eq!(json["present"], 3);
    }

    #[test]
    fn changing_one_package_uploads_its_layer_and_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app");
        // One alpha package for both versions: a package's record names its archive's location, so alpha must be
        // the same archive for its layer to be the same.
        let alpha = testing::package(
            dir.path(),
            testing::Spec::new("alpha").file("share/a.txt", "a", false, None),
        );
        let beta = |version: &str, content: &str| {
            let root = dir.path().join(version);
            std::fs::create_dir_all(&root).unwrap();
            testing::package(
                &root,
                testing::Spec::new("beta").file("share/b.txt", content, false, None),
            )
        };
        let mut digests = Vec::new();
        for (version, content) in [("v1", "b"), ("v2", "b, changed")] {
            let plan = testing::plan_with(
                vec![alpha.clone(), beta(version, content)],
                crate::plan::Layering::PerPackage,
                100,
            );
            let path = dir.path().join(format!("{version}.tar"));
            testing::build_image(dir.path(), &plan, &path);
            let pushed = push(&path, None, &to, &client(dir.path()), &mut std::io::sink()).unwrap();
            digests.push((pushed.uploaded, pushed.present));
        }
        // The changed package's layer, and the config whose diff_ids changed with it; alpha's layer is reused.
        assert_eq!(digests, [(3, 0), (2, 1)]);
    }

    #[test]
    fn a_base_on_the_same_registry_lends_its_layers() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let mut base = testing::base_image(dir.path(), &[testing::BaseEntry::File("bin/sh", b"sh".to_vec(), 0o755)]);
        base.reference = ImageRef::parse(&format!("{}/os/base:9@sha256:{}", fake.host, "b".repeat(64))).unwrap();
        let layer = std::fs::read(&base.layers[0].path).unwrap();
        fake.store
            .lock()
            .unwrap()
            .blobs
            .insert(("os/base".to_string(), base.layers[0].digest.clone()), layer);
        let plan = testing::plan_of(vec![testing::simple(dir.path(), "tool", &[])], 100);
        let path = dir.path().join("on-base.tar");
        let options = crate::build::BuildOptions {
            prefix: "/opt/env".to_string(),
            output: path.clone(),
            force: true,
            reference: crate::image::Reference::parse("app:1").unwrap(),
            env: Vec::new(),
            entrypoint: None,
            cmd: None,
            workdir: None,
            source_date_epoch: 0,
            max_memory: 16 << 20,
            base: Some(base),
            labels: Vec::new(),
            annotations: Vec::new(),
            user: None,
            expose: Vec::new(),
            stop_signal: None,
            app: None,
        };
        crate::build::build(
            &plan,
            &options,
            &crate::fetch::Fetcher::new(dir.path().join("cache")),
            &mut std::io::sink(),
        )
        .unwrap();
        let mut progress = Vec::new();
        let pushed = push(
            &path,
            None,
            &destination(&fake, "team/app:1"),
            &client(dir.path()),
            &mut progress,
        )
        .unwrap();
        assert_eq!((pushed.uploaded, pushed.mounted, pushed.present), (2, 1, 0));
        assert!(String::from_utf8(progress).unwrap().contains("mounted from os/base"));
    }

    #[test]
    fn writes_ask_for_a_push_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let fake = fake(Behaviour {
            tokens: true,
            ..Behaviour::default()
        });
        let pushed = push(
            &path,
            None,
            &destination(&fake, "team/app:1"),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(pushed.uploaded, 2);
        let scopes = fake.store.lock().unwrap().scopes.clone();
        assert!(
            scopes.iter().all(|scope| scope == "repository:team/app:pull,push"),
            "{scopes:?}"
        );
        assert_eq!(scopes.len(), 1, "one token for the whole push: {scopes:?}");
    }

    #[test]
    fn a_push_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app:1");
        let err = push(dir.path(), None, &to, &client(dir.path()), &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, PushError::Invalid { problems: 2, .. }), "{err}");
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let pinned = destination(&fake, &format!("team/app@sha256:{}", "0".repeat(64)));
        let err = push(&path, None, &pinned, &client(dir.path()), &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, PushError::WrongDigest { .. }), "{err}");
        assert!(fake.store.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn a_push_by_digest_and_a_registry_that_disagrees() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let digest = inspect(&path, None).unwrap().manifest.digest;
        let fake = fake(Behaviour::default());
        let pushed = push(
            &path,
            None,
            &destination(&fake, &format!("team/app@{digest}")),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(pushed.reference, format!("{}/team/app@{digest}", fake.host));
        assert!(
            fake.store
                .lock()
                .unwrap()
                .manifests
                .contains_key(&("team/app".to_string(), digest.clone()))
        );

        let liar = self::fake(Behaviour {
            lie: true,
            ..Behaviour::default()
        });
        let err = push(
            &path,
            None,
            &destination(&liar, "team/app:1"),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert!(matches!(err, PushError::Stored { .. }), "{err}");
    }
}
