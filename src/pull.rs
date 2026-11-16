//! `pull`: an image in a registry written to an OCI image layout directory or a `.tar` archive.
//!
//! pixi-oci has no image store, so an image is pulled into a file, as `build` writes one, with no daemon. The tag is
//! resolved to the digest the registry serves, an index is followed to the platform's manifest, and every document
//! and blob is checked against its digest on the way through the blob cache. The manifest and config are written as
//! they were served, so the pulled image has the registry's digest.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::build::BuildError;
use crate::image::{self, Reference};
use crate::layer::Blob;
use crate::platform::OciPlatform;
use crate::registry::{ImageRef, Registry, RegistryError};
use crate::report::{plural, size_text};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PullError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Registry(#[from] RegistryError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Output(#[from] BuildError),

    #[error("could not write {}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// What a pull wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pulled {
    /// The reference pulled, pinned to what the tag resolved to.
    pub reference: String,
    /// The image manifest for the platform.
    pub digest: String,
    pub platform: String,
    pub layers: usize,
    /// Compressed size of the layers.
    pub size: u64,
    pub output: String,
}

impl Pulled {
    pub fn text(&self) -> String {
        format!(
            "Pulled {} to {}\n  manifest {}\n  {}, {} for {}\n",
            self.reference,
            self.output,
            self.digest,
            plural(self.layers, "layer"),
            size_text(Some(self.size)),
            self.platform
        )
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a pull always serializes")
    }
}

/// Pull `reference` for `platform` into `output`: a layout directory, or a `.tar` archive.
pub fn pull(
    reference: &ImageRef,
    platform: &OciPlatform,
    output: &Path,
    force: bool,
    registry: &Registry,
) -> Result<Pulled, PullError> {
    crate::build::prepare_output(output, force)?;
    let digest = registry.resolve(reference)?;
    let pinned = ImageRef {
        digest: Some(digest.clone()),
        ..reference.clone()
    };
    let image = registry.pull(&pinned, platform)?;

    let is_archive = output.extension().is_some_and(|extension| extension == "tar");
    let write_err = |path: &Path| {
        let path = path.to_path_buf();
        move |source| PullError::Write { path, source }
    };
    let scratch = tempfile::tempdir().map_err(write_err(&std::env::temp_dir()))?;
    let layout = if is_archive {
        scratch.path().join("layout")
    } else {
        output.to_path_buf()
    };
    let blobs = layout.join("blobs/sha256");
    fs::create_dir_all(&blobs).map_err(write_err(&blobs))?;
    let cached = |digest: &str| registry.blob_file(digest).expect("a pulled image's digests are sha256");
    let mut copies: Vec<(String, PathBuf)> = vec![
        (image.manifest_digest.clone(), cached(&image.manifest_digest)),
        (image.config_digest.clone(), cached(&image.config_digest)),
    ];
    copies.extend(
        image
            .layers
            .iter()
            .map(|layer| (layer.digest.clone(), layer.path.clone())),
    );
    for (digest, source) in &copies {
        let target = blobs.join(digest.trim_start_matches("sha256:"));
        if !target.exists() {
            fs::copy(source, &target).map_err(write_err(&target))?;
        }
    }

    let manifest_size = fs::metadata(cached(&image.manifest_digest))
        .map_err(write_err(&cached(&image.manifest_digest)))?
        .len();
    // A tag names the image for docker load and podman load; a reference pinned only by digest has none.
    let name = Reference {
        name: format!("{}/{}", reference.registry, reference.repository),
        tag: reference.tag.clone().unwrap_or_default(),
    };
    let named = reference.tag.as_ref().map(|_| &name);
    image::write_index(
        &layout,
        platform,
        &image.manifest_media_type,
        &Blob {
            digest: image.manifest_digest.clone(),
            diff_id: String::new(),
            size: manifest_size,
        },
        named,
        &image.config_digest,
        &image
            .layers
            .iter()
            .map(|layer| layer.digest.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(write_err(&layout))?;
    if is_archive {
        image::write_archive(&layout, output, 0).map_err(write_err(output))?;
    }
    Ok(Pulled {
        reference: pinned.to_string(),
        digest: image.manifest_digest,
        platform: platform.to_string(),
        layers: image.layers.len(),
        size: image.layers.iter().map(|layer| layer.size).sum(),
        output: output.display().to_string(),
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

    fn amd64() -> OciPlatform {
        crate::platform::oci_platform("linux-64").unwrap()
    }

    #[test]
    fn a_pushed_image_pulls_back_with_its_digest() {
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
        let destination = ImageRef::parse(&format!("{}/team/app:1.0", server.host)).unwrap();
        crate::push::push(
            &built,
            None,
            &destination,
            &client(&dir.path().join("push-cache")),
            &mut std::io::sink(),
        )
        .unwrap();

        let cache = dir.path().join("pull-cache");
        for output in ["pulled.tar", "pulled-layout"] {
            let path = dir.path().join(output);
            let pulled = pull(&destination, &amd64(), &path, false, &client(&cache)).unwrap();
            assert_eq!(pulled.digest, written.manifest.digest, "{output}");
            assert_eq!(pulled.layers, 2);
            assert!(
                pulled.reference.contains("/team/app:1.0@sha256:"),
                "{}",
                pulled.reference
            );
            let report = crate::validate::validate(&path).unwrap();
            assert!(report.valid, "{}", report.text());
            let inspection = crate::inspect::inspect(&path, None).unwrap();
            assert_eq!(inspection.manifest.digest, written.manifest.digest);
            assert_eq!(
                inspection.reference.as_deref(),
                Some(format!("{}/team/app:1.0", server.host).as_str())
            );
            assert!(pulled.text().starts_with("Pulled "), "{}", pulled.text());
            let json: serde_json::Value = serde_json::from_str(&pulled.json()).unwrap();
            assert_eq!(json["layers"], 2);
        }
        // The second pull above found every blob in the cache: only manifests were asked for.
        let requests = server.store.lock().unwrap().requests.clone();
        let blob_gets = requests
            .iter()
            .filter(|r| r.starts_with("GET ") && r.contains("/blobs/"))
            .count();
        assert_eq!(blob_gets, 3, "{requests:#?}");

        // By digest alone, the image has no tag to load under.
        let pinned = ImageRef::parse(&format!("{}/team/app@{}", server.host, written.manifest.digest)).unwrap();
        let path = dir.path().join("by-digest");
        pull(&pinned, &amd64(), &path, false, &client(&cache)).unwrap();
        let docker: serde_json::Value = serde_json::from_slice(&fs::read(path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(docker[0]["RepoTags"], serde_json::json!([]));
        assert!(crate::validate::validate(&path).unwrap().valid);
    }

    #[test]
    fn a_pull_refuses_an_existing_output_and_an_unknown_tag() {
        let dir = tempfile::tempdir().unwrap();
        let server = fake(Behaviour::default());
        let reference = ImageRef::parse(&format!("{}/team/app:missing", server.host)).unwrap();
        let existing = dir.path().join("exists.tar");
        fs::write(&existing, b"x").unwrap();
        let err = pull(&reference, &amd64(), &existing, false, &client(dir.path())).unwrap_err();
        assert!(matches!(err, PullError::Output(_)), "{err}");
        assert!(server.store.lock().unwrap().requests.is_empty());
        let err = pull(
            &reference,
            &amd64(),
            &dir.path().join("new.tar"),
            false,
            &client(dir.path()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }
}
