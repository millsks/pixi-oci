//! `inspect`: what an image holds, read from its index, manifest and config without opening a layer.
//!
//! The packages of each layer come from the history entry pixi-oci writes for it (`pixi-oci: name version build,
//! ...`); for an image another tool built, a layer shows its history line instead.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::read::{Image, ReadError, blob_path, platform_text};
use crate::report::{plural, size_text};

/// How deep an index may nest indexes before the rest is ignored.
const NESTING: usize = 4;

/// Why an image could not be inspected.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum InspectError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Read(#[from] ReadError),

    #[error("{image} cannot be inspected: {message}")]
    #[diagnostic(help("`pixi oci validate {image}` lists everything wrong with it"))]
    Broken { image: String, message: String },

    #[error("{image} holds an image for {}, not {asked}", .available.join(", "))]
    #[diagnostic(help("pick one of those with --platform"))]
    NoPlatform {
        image: String,
        asked: String,
        available: Vec<String>,
    },

    #[error("{image} holds images for {}", .available.join(", "))]
    #[diagnostic(help("pick one with --platform, for example --platform {}", .available[0]))]
    SeveralPlatforms { image: String, available: Vec<String> },
}

/// Which stored document `--raw` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Raw {
    Index,
    Manifest,
    Config,
}

/// A conda package a layer holds, as its history line names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerPackage {
    pub name: String,
    pub version: String,
    pub build: Option<String>,
}

/// One layer of the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LayerInfo {
    pub digest: String,
    /// Compressed size, as the manifest records it.
    pub size: u64,
    pub media_type: String,
    pub diff_id: Option<String>,
    pub created_by: Option<String>,
    pub packages: Vec<LayerPackage>,
}

/// A stored document: its digest and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Document {
    pub digest: String,
    pub size: u64,
}

/// What an image holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Inspection {
    pub image: String,
    pub reference: Option<String>,
    pub platform: Option<String>,
    pub manifest: Document,
    pub config: Document,
    pub created: Option<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub workdir: Option<String>,
    pub env: Vec<String>,
    pub labels: serde_json::Map<String, Value>,
    pub layers: Vec<LayerInfo>,
}

/// A fully qualified name as docker shows it: `docker.io/library/app:1` is `app:1`, `docker.io/team/app:1` is
/// `team/app:1`, and any other registry stays.
fn familiar(name: &str) -> String {
    name.strip_prefix("docker.io/library/")
        .or_else(|| name.strip_prefix("docker.io/"))
        .unwrap_or(name)
        .to_string()
}

/// One image manifest an index leads to, with the annotations of the top-level entry that led there.
struct Candidate {
    descriptor: Value,
    platform: Option<String>,
    top: Value,
}

/// Inspect the image at `path`, for `platform` when it holds several.
pub fn inspect(path: &Path, platform: Option<&str>) -> Result<Inspection, InspectError> {
    let (image, candidate) = open(path, platform)?;
    let name = path.display().to_string();
    let broken = |message: String| InspectError::Broken {
        image: name.clone(),
        message,
    };
    let manifest_digest = candidate.descriptor["digest"].as_str().unwrap_or_default().to_string();
    let (manifest, manifest_size) = json_blob(&image, &manifest_digest).map_err(&broken)?;
    let config_digest = manifest["config"]["digest"].as_str().unwrap_or_default().to_string();
    let (config, config_size) = json_blob(&image, &config_digest).map_err(&broken)?;

    let diff_ids: Vec<&str> = config["rootfs"]["diff_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let history: Vec<&Value> = config["history"]
        .as_array()
        .map(|history| history.iter().filter(|entry| entry["empty_layer"] != true).collect())
        .unwrap_or_default();
    let layers = manifest["layers"]
        .as_array()
        .ok_or_else(|| broken("its manifest has no layers list".to_string()))?
        .iter()
        .enumerate()
        .map(|(i, layer)| {
            let created_by = history
                .get(i)
                .and_then(|entry| entry["created_by"].as_str())
                .map(str::to_string);
            LayerInfo {
                digest: layer["digest"].as_str().unwrap_or_default().to_string(),
                size: layer["size"].as_u64().unwrap_or(0),
                media_type: layer["mediaType"].as_str().unwrap_or_default().to_string(),
                diff_id: diff_ids.get(i).map(|id| id.to_string()),
                packages: created_by.as_deref().map(packages).unwrap_or_default(),
                created_by,
            }
        })
        .collect();

    let settings = &config["config"];
    let strings = |value: &Value| -> Option<Vec<String>> {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
    };
    let annotation = |key: &str| candidate.top["annotations"][key].as_str().map(str::to_string);
    Ok(Inspection {
        image: name.clone(),
        reference: annotation("io.containerd.image.name")
            .map(|name| familiar(&name))
            .or_else(|| annotation("org.opencontainers.image.ref.name")),
        platform: candidate.platform.or_else(|| platform_text(&config)),
        manifest: Document {
            digest: manifest_digest,
            size: manifest_size,
        },
        config: Document {
            digest: config_digest,
            size: config_size,
        },
        created: config["created"].as_str().map(str::to_string),
        entrypoint: strings(&settings["Entrypoint"]),
        cmd: strings(&settings["Cmd"]),
        workdir: settings["WorkingDir"]
            .as_str()
            .filter(|dir| !dir.is_empty())
            .map(str::to_string),
        env: strings(&settings["Env"]).unwrap_or_default(),
        labels: settings["Labels"].as_object().cloned().unwrap_or_default(),
        layers,
    })
}

/// The exact stored bytes of the index, or of the manifest or config of the image for `platform`.
pub fn raw(path: &Path, platform: Option<&str>, which: Raw) -> Result<String, InspectError> {
    let (image, candidate) = open(path, platform)?;
    let broken = |message: String| InspectError::Broken {
        image: path.display().to_string(),
        message,
    };
    let bytes = match which {
        Raw::Index => image
            .read_small("index.json")
            .map_err(|err| broken(format!("index.json: {err}")))?,
        Raw::Manifest | Raw::Config => {
            let manifest_digest = candidate.descriptor["digest"].as_str().unwrap_or_default();
            let manifest = read_blob(&image, manifest_digest).map_err(&broken)?;
            if which == Raw::Manifest {
                manifest
            } else {
                let value: Value = serde_json::from_slice(&manifest)
                    .map_err(|err| broken(format!("its manifest is not JSON: {err}")))?;
                read_blob(&image, value["config"]["digest"].as_str().unwrap_or_default()).map_err(&broken)?
            }
        }
    };
    String::from_utf8(bytes).map_err(|_| broken("the document is not UTF-8".to_string()))
}

/// Open the image and pick the manifest for `platform`, or the only one.
fn open(path: &Path, platform: Option<&str>) -> Result<(Image, Candidate), InspectError> {
    let image = Image::open(path)?;
    let name = path.display().to_string();
    let index: Value = image
        .read_small("index.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(|| InspectError::Broken {
            image: name.clone(),
            message: "it has no readable index.json".to_string(),
        })?;
    let mut candidates = Vec::new();
    collect(&image, &index, None, 0, &mut candidates);
    let available: Vec<String> = candidates
        .iter()
        .map(|candidate| {
            candidate
                .platform
                .clone()
                .unwrap_or_else(|| "an unnamed platform".to_string())
        })
        .collect();
    if candidates.is_empty() {
        return Err(InspectError::Broken {
            image: name,
            message: "its index lists no image manifest".to_string(),
        });
    }
    let chosen = match platform {
        None if candidates.len() == 1 => 0,
        None => {
            return Err(InspectError::SeveralPlatforms { image: name, available });
        }
        Some(asked) => {
            // A conda platform (`linux-aarch64`) is taken as the OCI platform its images are built for.
            let wanted = crate::platform::oci_platform(asked).map_or_else(|| asked.to_string(), |p| p.to_string());
            candidates
                .iter()
                .position(|candidate| candidate.platform.as_deref() == Some(wanted.as_str()))
                .ok_or_else(|| InspectError::NoPlatform {
                    image: name,
                    asked: asked.to_string(),
                    available,
                })?
        }
    };
    Ok((image, candidates.swap_remove(chosen)))
}

/// Every image manifest `index` leads to, through nested indexes.
fn collect(image: &Image, index: &Value, top: Option<&Value>, depth: usize, out: &mut Vec<Candidate>) {
    for entry in index["manifests"].as_array().into_iter().flatten() {
        let top = top.unwrap_or(entry);
        let media_type = entry["mediaType"].as_str().unwrap_or_default();
        if media_type.contains("index") || media_type.contains("manifest.list") {
            if depth < NESTING
                && let Some(nested) = entry["digest"]
                    .as_str()
                    .and_then(|digest| read_blob(image, digest).ok())
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                collect(image, &nested, Some(top), depth + 1, out);
            }
        } else {
            out.push(Candidate {
                descriptor: entry.clone(),
                platform: platform_text(&entry["platform"]),
                top: top.clone(),
            });
        }
    }
}

fn read_blob(image: &Image, digest: &str) -> Result<Vec<u8>, String> {
    let name = blob_path(digest).ok_or_else(|| format!("`{digest}` is not a sha256 digest"))?;
    image.read_small(&name).map_err(|err| format!("{name}: {err}"))
}

fn json_blob(image: &Image, digest: &str) -> Result<(Value, u64), String> {
    let bytes = read_blob(image, digest)?;
    let value = serde_json::from_slice(&bytes).map_err(|err| format!("{digest} is not JSON: {err}"))?;
    Ok((value, bytes.len() as u64))
}

/// The packages of a pixi-oci history line; none for any other.
fn packages(created_by: &str) -> Vec<LayerPackage> {
    let Some(list) = created_by.strip_prefix("pixi-oci: ") else {
        return Vec::new();
    };
    list.split(", ")
        .filter_map(|package| {
            let mut words = package.split_whitespace();
            Some(LayerPackage {
                name: words.next()?.to_string(),
                version: words.next()?.to_string(),
                build: words.next().map(str::to_string),
            })
        })
        .collect()
}

impl LayerInfo {
    /// The layer in a few words: its package, a count of packages, or its history line.
    fn describe(&self) -> String {
        match self.packages.as_slice() {
            [one] => format!("{} {}", one.name, one.version),
            [] => match &self.created_by {
                Some(line) if line.chars().count() > 48 => {
                    format!("{}...", line.chars().take(45).collect::<String>().trim_end())
                }
                Some(line) => line.clone(),
                None => "(no history)".to_string(),
            },
            many => plural(many.len(), "package"),
        }
    }
}

impl Inspection {
    /// For a person: the image, its settings, then a row per layer; with `packages`, each layer of several packages
    /// lists them.
    pub fn text(&self, packages: bool) -> String {
        let mut out = String::new();
        match &self.reference {
            Some(reference) => out.push_str(&format!("Image {reference} ({})\n", self.image)),
            None => out.push_str(&format!("Image {}\n", self.image)),
        }
        let mut field = |key: &str, values: Vec<String>| {
            for (i, value) in values.iter().enumerate() {
                let key = if i == 0 { key } else { "" };
                out.push_str(&format!("  {key:<11} {value}\n"));
            }
        };
        let json = |value: &Vec<String>| serde_json::to_string(value).expect("strings serialize");
        field("platform", self.platform.iter().cloned().collect());
        field("manifest", vec![self.manifest.digest.clone()]);
        field("config", vec![self.config.digest.clone()]);
        field("created", self.created.iter().cloned().collect());
        field("entrypoint", self.entrypoint.iter().map(json).collect());
        field("cmd", self.cmd.iter().map(json).collect());
        field("workdir", self.workdir.iter().cloned().collect());
        field("env", self.env.clone());
        field(
            "labels",
            self.labels
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{key}={}",
                        value.as_str().map_or_else(|| value.to_string(), str::to_string)
                    )
                })
                .collect(),
        );

        let total: u64 = self.layers.iter().map(|layer| layer.size).sum();
        out.push_str(&format!(
            "\n{}, {}\n\n",
            plural(self.layers.len(), "layer"),
            size_text(Some(total))
        ));
        let mut rows: Vec<(String, String, String, String)> = Vec::new();
        for (i, layer) in self.layers.iter().enumerate() {
            let digest: String = layer.digest.chars().take("sha256:".len() + 12).collect();
            rows.push((
                (i + 1).to_string(),
                layer.describe(),
                size_text(Some(layer.size)),
                digest,
            ));
            if packages && layer.packages.len() > 1 {
                for package in &layer.packages {
                    rows.push((
                        String::new(),
                        format!("  {} {}", package.name, package.version),
                        String::new(),
                        String::new(),
                    ));
                }
            }
        }
        let number = rows.iter().map(|row| row.0.len()).max().unwrap_or(1);
        let what = rows.iter().map(|row| row.1.len()).max().unwrap_or(0).max("Layer".len());
        let size = rows.iter().map(|row| row.2.len()).max().unwrap_or(0).max("Size".len());
        out.push_str(&format!(
            "  {:>number$}  {:<what$}  {:<size$}  Digest\n",
            "#", "Layer", "Size"
        ));
        for (n, w, s, d) in rows {
            out.push_str(format!("  {n:>number$}  {w:<what$}  {s:<size$}  {d}").trim_end());
            out.push('\n');
        }
        out
    }

    /// For a script.
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("an inspection always serializes")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::plan::Layering;
    use crate::testing;

    fn names(plan: &crate::plan::Plan) -> Vec<Vec<String>> {
        plan.layers
            .iter()
            .map(|layer| layer.packages.iter().map(|p| p.name.clone()).collect())
            .collect()
    }

    fn sha(bytes: &[u8]) -> String {
        format!("sha256:{}", crate::fetch::hex(&Sha256::digest(bytes)))
    }

    #[test]
    fn inspect_reports_the_layers_plan_predicted() {
        for (layering, output) in [(Layering::Grouped, "image.tar"), (Layering::PerPackage, "layout")] {
            let dir = tempfile::tempdir().unwrap();
            let packages = vec![
                testing::simple(dir.path(), "tool", &["base"]),
                testing::simple(dir.path(), "base", &[]),
                testing::simple(dir.path(), "extra", &["base"]),
            ];
            let plan = testing::plan_with(packages, layering, 100);
            let path = dir.path().join(output);
            let written = testing::build_image(dir.path(), &plan, &path);
            let inspection = inspect(&path, None).unwrap();
            let seen: Vec<Vec<String>> = inspection
                .layers
                .iter()
                .map(|layer| layer.packages.iter().map(|p| p.name.clone()).collect())
                .collect();
            assert_eq!(seen, names(&plan), "{layering:?}");
            assert_eq!(inspection.manifest.digest, written.manifest.digest);
            assert_eq!(inspection.config.digest, written.config.digest);
            assert_eq!(inspection.reference.as_deref(), Some("test:1"));
            assert_eq!(inspection.platform.as_deref(), Some("linux/amd64"));
            assert_eq!(inspection.created.as_deref(), Some("1970-01-01T00:00:00Z"));
            assert_eq!(
                inspection.entrypoint,
                Some(vec!["tool".to_string(), "--serve".to_string()])
            );
            assert_eq!(inspection.cmd, None);
            assert_eq!(inspection.workdir.as_deref(), Some("/work"));
            assert!(
                inspection.env.contains(&"GREETING=hi".to_string()),
                "{:?}",
                inspection.env
            );
            assert_eq!(inspection.labels["dev.pixi.environment"], "test");
            assert!(inspection.layers.iter().all(|layer| layer.diff_id.is_some()));
            assert_eq!(inspection.layers[0].packages[0].build.as_deref(), Some("h0_0"));
        }
    }

    #[test]
    fn the_text_lists_settings_and_layers() {
        let dir = tempfile::tempdir().unwrap();
        let packages = vec![
            testing::simple(dir.path(), "tool", &["base"]),
            testing::simple(dir.path(), "base", &[]),
        ];
        let plan = testing::plan_with(packages, Layering::Grouped, 100);
        let path = dir.path().join("image.tar");
        testing::build_image(dir.path(), &plan, &path);
        let inspection = inspect(&path, None).unwrap();
        let text = inspection.text(false);
        assert!(
            text.starts_with(&format!("Image test:1 ({})\n", path.display())),
            "{text}"
        );
        for line in [
            "  platform    linux/amd64\n",
            "  entrypoint  [\"tool\",\"--serve\"]\n",
            "  workdir     /work\n",
            "              GREETING=hi\n",
            "  labels      dev.pixi.environment=test\n",
            "\n1 layer, ",
            "  #  Layer       Size",
            "  1  2 packages  ",
        ] {
            assert!(text.contains(line), "no {line:?} in\n{text}");
        }
        assert!(!text.contains("  cmd "), "{text}");
        let listed = inspection.text(true);
        assert!(listed.contains("\n       base 1.0\n       tool 1.0\n"), "{listed}");
        let json: Value = serde_json::from_str(&inspection.json()).unwrap();
        assert_eq!(json["layers"][0]["packages"][1]["name"], "tool");
        assert_eq!(json["manifest"]["digest"], inspection.manifest.digest.as_str());
    }

    #[test]
    fn raw_documents_are_the_stored_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let plan = testing::plan_of(vec![testing::simple(dir.path(), "base", &[])], 100);
        let layout = dir.path().join("layout");
        let written = testing::build_image(dir.path(), &plan, &layout);
        let manifest = raw(&layout, None, Raw::Manifest).unwrap();
        assert_eq!(sha(manifest.as_bytes()), written.manifest.digest);
        let config = raw(&layout, None, Raw::Config).unwrap();
        assert_eq!(sha(config.as_bytes()), written.config.digest);
        let index = raw(&layout, None, Raw::Index).unwrap();
        assert_eq!(index.as_bytes(), fs::read(layout.join("index.json")).unwrap());
    }

    /// A layout with one manifest listed for two platforms, under a nested index.
    fn two_platforms(dir: &std::path::Path) -> PathBuf {
        let plan = testing::plan_of(vec![testing::simple(dir, "base", &[])], 100);
        let layout = dir.join("layout");
        testing::build_image(dir, &plan, &layout);
        let index: Value = serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
        let amd = index["manifests"][0].clone();
        let mut arm = amd.clone();
        arm["platform"] = json!({"os": "linux", "architecture": "arm64", "variant": "v8"});
        let nested = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": crate::image::INDEX_MEDIA_TYPE,
            "manifests": [amd, arm],
        }))
        .unwrap();
        let digest = sha(&nested);
        fs::write(layout.join(blob_path(&digest).unwrap()), &nested).unwrap();
        let top = json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": crate::image::INDEX_MEDIA_TYPE,
                "digest": digest,
                "size": nested.len(),
                "annotations": {"org.opencontainers.image.ref.name": "multi"},
            }],
        });
        fs::write(layout.join("index.json"), serde_json::to_vec(&top).unwrap()).unwrap();
        layout
    }

    #[test]
    fn a_platform_is_picked_from_several() {
        let dir = tempfile::tempdir().unwrap();
        let layout = two_platforms(dir.path());
        let err = inspect(&layout, None).unwrap_err();
        assert!(matches!(err, InspectError::SeveralPlatforms { .. }), "{err}");
        assert!(err.to_string().contains("linux/amd64, linux/arm64/v8"), "{err}");
        let arm = inspect(&layout, Some("linux-aarch64")).unwrap();
        assert_eq!(arm.platform.as_deref(), Some("linux/arm64/v8"));
        assert_eq!(arm.reference.as_deref(), Some("multi"));
        assert_eq!(
            inspect(&layout, Some("linux/amd64")).unwrap().platform.as_deref(),
            Some("linux/amd64")
        );
        let err = inspect(&layout, Some("linux-s390x")).unwrap_err();
        assert!(matches!(err, InspectError::NoPlatform { .. }), "{err}");
        assert!(raw(&layout, None, Raw::Manifest).is_err());
        assert!(raw(&layout, Some("linux/arm64/v8"), Raw::Config).is_ok());
    }

    #[test]
    fn layers_of_other_tools_show_their_history() {
        let dir = tempfile::tempdir().unwrap();
        let plan = testing::plan_of(vec![testing::simple(dir.path(), "base", &[])], 100);
        let layout = dir.path().join("layout");
        testing::build_image(dir.path(), &plan, &layout);
        let line = format!("/bin/sh -c apt-get update && apt-get install -y {}", "curl ".repeat(10));
        let index: Value = serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
        let manifest_name = blob_path(index["manifests"][0]["digest"].as_str().unwrap()).unwrap();
        let mut manifest: Value = serde_json::from_slice(&fs::read(layout.join(&manifest_name)).unwrap()).unwrap();
        let config_name = blob_path(manifest["config"]["digest"].as_str().unwrap()).unwrap();
        let mut config: Value = serde_json::from_slice(&fs::read(layout.join(&config_name)).unwrap()).unwrap();
        config["history"] = json!([{"created_by": "#(nop) ENV X=1", "empty_layer": true}, {"created_by": line}]);
        let config = serde_json::to_vec(&config).unwrap();
        fs::write(layout.join(blob_path(&sha(&config)).unwrap()), &config).unwrap();
        manifest["config"]["digest"] = json!(sha(&config));
        let manifest = serde_json::to_vec(&manifest).unwrap();
        fs::write(layout.join(blob_path(&sha(&manifest)).unwrap()), &manifest).unwrap();
        let mut index = index;
        index["manifests"][0]["digest"] = json!(sha(&manifest));
        fs::write(layout.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();

        let inspection = inspect(&layout, None).unwrap();
        assert!(inspection.layers[0].packages.is_empty());
        assert_eq!(inspection.layers[0].created_by.as_deref(), Some(line.as_str()));
        let text = inspection.text(true);
        assert!(
            text.contains("/bin/sh -c apt-get update && apt-get install..."),
            "{text}"
        );
        let mut no_history = inspection.layers[0].clone();
        no_history.created_by = None;
        assert_eq!(no_history.describe(), "(no history)");
    }

    #[test]
    fn a_broken_image_points_to_validate() {
        let dir = tempfile::tempdir().unwrap();
        let err = inspect(dir.path(), None).unwrap_err();
        assert!(err.to_string().contains("has no readable index.json"), "{err}");
        fs::write(
            dir.path().join("index.json"),
            br#"{"schemaVersion": 2, "manifests": []}"#,
        )
        .unwrap();
        let err = inspect(dir.path(), None).unwrap_err();
        assert!(err.to_string().contains("lists no image manifest"), "{err}");
        fs::write(
            dir.path().join("index.json"),
            br#"{"manifests": [{"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": "md5:x"}]}"#,
        )
        .unwrap();
        let err = inspect(dir.path(), None).unwrap_err();
        assert!(err.to_string().contains("`md5:x` is not a sha256 digest"), "{err}");
        assert!(raw(dir.path(), None, Raw::Manifest).is_err());
        assert!(matches!(
            inspect(&dir.path().join("missing"), None).unwrap_err(),
            InspectError::Read(_)
        ));
    }

    #[test]
    fn names_are_shown_as_docker_shows_them() {
        assert_eq!(familiar("docker.io/library/app:1"), "app:1");
        assert_eq!(familiar("docker.io/team/app:1"), "team/app:1");
        assert_eq!(familiar("quay.io/team/app:1"), "quay.io/team/app:1");
    }

    #[test]
    fn history_lines_name_packages() {
        assert_eq!(
            packages("pixi-oci: python 3.13.1 h0_0, six 1.17.0"),
            [
                LayerPackage {
                    name: "python".into(),
                    version: "3.13.1".into(),
                    build: Some("h0_0".into()),
                },
                LayerPackage {
                    name: "six".into(),
                    version: "1.17.0".into(),
                    build: None,
                },
            ]
        );
        assert!(packages("COPY . /app").is_empty());
        assert!(packages("pixi-oci: lonely").is_empty());
    }
}
