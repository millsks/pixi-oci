//! `validate`: an image checked against the OCI image spec, without running anything.
//!
//! Every blob an image names is read once and hashed; every layer is also uncompressed and hashed for its diff_id.
//! Each problem found is collected rather than stopping at the first, so one run says everything that is wrong.

use std::io::{self, Read};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::image::{CONFIG_MEDIA_TYPE, INDEX_MEDIA_TYPE, MANIFEST_MEDIA_TYPE};
use crate::read::{Image, ReadError, blob_path, platform_text};
use crate::report::{plural, size_text};

const DOCKER_MANIFEST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_LIST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_CONFIG_MEDIA_TYPE: &str = "application/vnd.docker.container.image.v1+json";

/// How a layer's bytes are compressed, by its media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    None,
    Gzip,
    Zstd,
}

fn layer_compression(media_type: &str) -> Option<Compression> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Some(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Some(Compression::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => Some(Compression::Zstd),
        _ => None,
    }
}

/// One thing wrong with an image: where, and what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub at: String,
    pub message: String,
}

/// One image manifest that was checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Checked {
    pub digest: String,
    pub platform: Option<String>,
    pub layers: usize,
    /// Compressed size of the layers.
    pub size: u64,
}

/// What `validate` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub image: String,
    pub valid: bool,
    pub manifests: Vec<Checked>,
    pub problems: Vec<Problem>,
}

impl Report {
    /// The report for a person: a verdict, then each manifest or each problem.
    pub fn text(&self) -> String {
        let mut out = String::new();
        if self.valid {
            out.push_str(&format!("{} is a valid OCI image\n", self.image));
        } else {
            out.push_str(&format!(
                "{} is not a valid OCI image: {}\n",
                self.image,
                plural(self.problems.len(), "problem")
            ));
        }
        for manifest in &self.manifests {
            out.push_str(&format!(
                "  manifest {} for {}: {}, {}\n",
                short(&manifest.digest),
                manifest.platform.as_deref().unwrap_or("an unnamed platform"),
                plural(manifest.layers, "layer"),
                size_text(Some(manifest.size)),
            ));
        }
        for problem in &self.problems {
            out.push_str(&format!("  {}: {}\n", problem.at, problem.message));
        }
        out
    }

    /// The report for a script.
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serializes")
    }
}

/// Check the image at `path`. An image that cannot be opened at all is an error; everything else is a problem in
/// the report.
pub fn validate(path: &Path) -> Result<Report, ReadError> {
    let image = Image::open(path)?;
    let mut checker = Checker {
        image: &image,
        problems: Vec::new(),
        manifests: Vec::new(),
    };
    checker.layout();
    let problems = checker.problems;
    Ok(Report {
        image: path.display().to_string(),
        valid: problems.is_empty(),
        manifests: checker.manifests,
        problems,
    })
}

/// `sha256:` and the first 12 hex digits, for messages.
fn short(digest: &str) -> String {
    digest.chars().take("sha256:".len() + 12).collect()
}

struct Checker<'a> {
    image: &'a Image,
    problems: Vec<Problem>,
    manifests: Vec<Checked>,
}

/// A descriptor's fields, read leniently so a missing field is a problem rather than a crash.
struct Descriptor<'v> {
    media_type: Option<&'v str>,
    digest: Option<&'v str>,
    size: Option<u64>,
    value: &'v Value,
}

fn descriptor(value: &Value) -> Descriptor<'_> {
    Descriptor {
        media_type: value["mediaType"].as_str(),
        digest: value["digest"].as_str(),
        size: value["size"].as_u64(),
        value,
    }
}

/// Hashes and counts what passes through.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

fn hashing<R: Read>(inner: R) -> Hashing<R> {
    Hashing {
        inner,
        hasher: Sha256::new(),
        count: 0,
    }
}

fn digest_of(hasher: Sha256) -> String {
    format!("sha256:{}", crate::fetch::hex(&hasher.finalize()))
}

impl Checker<'_> {
    fn problem(&mut self, at: impl Into<String>, message: impl Into<String>) {
        self.problems.push(Problem {
            at: at.into(),
            message: message.into(),
        });
    }

    fn layout(&mut self) {
        match self.image.read_small("oci-layout") {
            Err(_) => self.problem("oci-layout", "missing: every image layout has one"),
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) if value["imageLayoutVersion"] == "1.0.0" => {}
                Ok(value) => self.problem(
                    "oci-layout",
                    format!("imageLayoutVersion is {}, not \"1.0.0\"", value["imageLayoutVersion"]),
                ),
                Err(err) => self.problem("oci-layout", format!("not JSON: {err}")),
            },
        }
        let Some(index) = self.json_file("index.json") else {
            return;
        };
        self.index(&index, "index.json");
        if self.image.has("manifest.json") {
            self.docker_manifest(&index);
        }
    }

    /// A JSON document stored under its own name, such as `index.json`.
    fn json_file(&mut self, name: &str) -> Option<Value> {
        match self.image.read_small(name) {
            Err(_) => {
                self.problem(name, "missing");
                None
            }
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => Some(value),
                Err(err) => {
                    self.problem(name, format!("not JSON: {err}"));
                    None
                }
            },
        }
    }

    /// An image index, at the top level or nested.
    fn index(&mut self, index: &Value, at: &str) {
        if index["schemaVersion"] != 2 {
            self.problem(at, format!("schemaVersion is {}, not 2", index["schemaVersion"]));
        }
        if let Some(media_type) = index["mediaType"].as_str()
            && media_type != INDEX_MEDIA_TYPE
        {
            self.problem(at, format!("mediaType is {media_type}, not {INDEX_MEDIA_TYPE}"));
        }
        let Some(manifests) = index["manifests"].as_array() else {
            self.problem(at, "has no manifests list");
            return;
        };
        if manifests.is_empty() {
            self.problem(at, "lists no manifests");
        }
        for (i, entry) in manifests.iter().enumerate() {
            let entry = descriptor(entry);
            let entry_at = format!("{at} manifest {}", i + 1);
            if let Some(name) = entry.value["annotations"]["org.opencontainers.image.ref.name"].as_str()
                && !valid_ref_name(name)
            {
                self.problem(
                    &entry_at,
                    format!("org.opencontainers.image.ref.name `{name}` is not a valid reference name"),
                );
            }
            match entry.media_type {
                Some(MANIFEST_MEDIA_TYPE | DOCKER_MANIFEST_MEDIA_TYPE) => {
                    if let Some(manifest) = self.json_blob(&entry, &entry_at) {
                        let digest = entry.digest.unwrap_or_default().to_string();
                        self.manifest(&manifest, &entry, &format!("manifest {}", short(&digest)), digest);
                    }
                }
                Some(INDEX_MEDIA_TYPE | DOCKER_LIST_MEDIA_TYPE) => {
                    if let Some(nested) = self.json_blob(&entry, &entry_at) {
                        let nested_at = format!("index {}", short(entry.digest.unwrap_or_default()));
                        self.index(&nested, &nested_at);
                    }
                }
                Some(other) => self.problem(&entry_at, format!("mediaType {other} is not a manifest or an index")),
                None => self.problem(&entry_at, "has no mediaType"),
            }
        }
    }

    /// Check a descriptor's blob exists and matches its digest and size; return its bytes when it is small enough
    /// to be a JSON document.
    fn json_blob(&mut self, entry: &Descriptor, at: &str) -> Option<Value> {
        let name = self.blob_name(entry, at)?;
        let bytes = match self.image.read_small(&name) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.problem(at, format!("cannot read {name}: {err}"));
                return None;
            }
        };
        let digest = digest_of(Sha256::new_with_prefix(&bytes));
        self.compare(entry, at, &digest, bytes.len() as u64);
        match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            Err(err) => {
                self.problem(at, format!("{name} is not JSON: {err}"));
                None
            }
        }
    }

    /// The blob path of a descriptor, if its digest is well formed and the image holds it.
    fn blob_name(&mut self, entry: &Descriptor, at: &str) -> Option<String> {
        let Some(digest) = entry.digest else {
            self.problem(at, "has no digest");
            return None;
        };
        let Some(name) = blob_path(digest) else {
            self.problem(
                at,
                format!("digest {digest} is not a sha256 digest (`sha256:` and 64 lowercase hex digits)"),
            );
            return None;
        };
        if !self.image.has(&name) {
            self.problem(at, format!("blob {name} is missing"));
            return None;
        }
        Some(name)
    }

    fn compare(&mut self, entry: &Descriptor, at: &str, digest: &str, size: u64) {
        if Some(digest) != entry.digest {
            self.problem(
                at,
                format!(
                    "content hashes to {digest}, not the {} its descriptor names",
                    entry.digest.unwrap_or("digest")
                ),
            );
        }
        match entry.size {
            None => self.problem(at, "descriptor has no size"),
            Some(expected) if expected != size => {
                self.problem(at, format!("blob is {size} bytes, but its descriptor says {expected}"));
            }
            Some(_) => {}
        }
    }

    fn manifest(&mut self, manifest: &Value, entry: &Descriptor, at: &str, digest: String) {
        if manifest["schemaVersion"] != 2 {
            self.problem(at, format!("schemaVersion is {}, not 2", manifest["schemaVersion"]));
        }
        if let (Some(inner), Some(outer)) = (manifest["mediaType"].as_str(), entry.media_type)
            && inner != outer
        {
            self.problem(at, format!("mediaType is {inner}, but the index lists it as {outer}"));
        }
        let config = descriptor(&manifest["config"]);
        let config_at = format!("{at} config");
        match config.media_type {
            Some(CONFIG_MEDIA_TYPE | DOCKER_CONFIG_MEDIA_TYPE) => {}
            Some(other) => self.problem(&config_at, format!("mediaType {other} is not an image config")),
            None => self.problem(&config_at, "has no mediaType"),
        }
        let config_value = self.json_blob(&config, &config_at);

        let layers: Vec<Descriptor> = manifest["layers"]
            .as_array()
            .map(|layers| layers.iter().map(descriptor).collect())
            .unwrap_or_default();
        if manifest["layers"].as_array().is_none() {
            self.problem(at, "has no layers list");
        }
        let mut diff_ids = Vec::with_capacity(layers.len());
        let mut size = 0;
        for (i, layer) in layers.iter().enumerate() {
            let layer_at = format!("{at} layer {}", i + 1);
            size += layer.size.unwrap_or(0);
            diff_ids.push(self.layer(layer, &layer_at));
        }

        if let Some(config_value) = config_value {
            self.config(&config_value, &config_at, &diff_ids, entry);
        }
        self.manifests.push(Checked {
            digest,
            platform: platform_text(&entry.value["platform"]),
            layers: layers.len(),
            size,
        });
    }

    /// Check a layer blob; return the sha256 of its uncompressed tar, when that could be worked out.
    fn layer(&mut self, layer: &Descriptor, at: &str) -> Option<String> {
        let compression = match layer.media_type {
            None => {
                self.problem(at, "has no mediaType");
                None
            }
            Some(media_type) => {
                let compression = layer_compression(media_type);
                if compression.is_none() {
                    self.problem(at, format!("mediaType {media_type} is not a layer type"));
                }
                compression
            }
        };
        let name = self.blob_name(layer, at)?;
        let reader = match self.image.reader(&name) {
            Ok(reader) => reader,
            Err(err) => {
                self.problem(at, format!("cannot read {name}: {err}"));
                return None;
            }
        };
        let mut compressed = hashing(reader);
        let uncompressed = match compression {
            Some(Compression::Gzip) => {
                let mut tar = hashing(flate2::read::MultiGzDecoder::new(&mut compressed));
                match io::copy(&mut tar, &mut io::sink()) {
                    Ok(_) => Some(digest_of(tar.hasher)),
                    Err(err) => {
                        self.problem(at, format!("is not valid gzip: {err}"));
                        None
                    }
                }
            }
            Some(Compression::None) => None,
            Some(Compression::Zstd) | None => None,
        };
        // Whatever the decoder left unread still counts toward the blob's digest and size.
        if let Err(err) = io::copy(&mut compressed, &mut io::sink()) {
            self.problem(at, format!("cannot read {name}: {err}"));
            return None;
        }
        let digest = digest_of(compressed.hasher);
        self.compare(layer, at, &digest, compressed.count);
        match compression {
            Some(Compression::None) => Some(digest),
            _ => uncompressed,
        }
    }

    fn config(&mut self, config: &Value, at: &str, diff_ids: &[Option<String>], entry: &Descriptor) {
        if config["rootfs"]["type"] != "layers" {
            self.problem(
                at,
                format!("rootfs.type is {}, not \"layers\"", config["rootfs"]["type"]),
            );
        }
        let listed: Vec<&str> = config["rootfs"]["diff_ids"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if listed.len() != diff_ids.len() {
            self.problem(
                at,
                format!(
                    "lists {} for {}",
                    plural(listed.len(), "diff_id"),
                    plural(diff_ids.len(), "layer")
                ),
            );
        }
        for (i, (listed, actual)) in listed.iter().zip(diff_ids).enumerate() {
            if let Some(actual) = actual
                && listed != actual
            {
                self.problem(
                    at,
                    format!(
                        "diff_id {} is {listed}, but layer {} uncompresses to {actual}",
                        i + 1,
                        i + 1
                    ),
                );
            }
        }
        if let Some(history) = config["history"].as_array() {
            let filled = history.iter().filter(|entry| entry["empty_layer"] != true).count();
            if filled != diff_ids.len() {
                self.problem(
                    at,
                    format!(
                        "history has {} that are not empty_layer, for {}",
                        plural(filled, "entry"),
                        plural(diff_ids.len(), "layer")
                    ),
                );
            }
        }
        let platform = &entry.value["platform"];
        if platform.is_object() {
            for field in ["os", "architecture", "variant"] {
                if platform[field] != config[field] && !(platform[field].is_null() && config[field].is_null()) {
                    self.problem(
                        at,
                        format!("{field} is {}, but the index says {}", config[field], platform[field]),
                    );
                }
            }
        }
    }

    /// The `manifest.json` `docker load` reads: it must name the same config and layers, in order, as the index.
    fn docker_manifest(&mut self, index: &Value) {
        let Some(docker) = self.json_file("manifest.json") else {
            return;
        };
        let Some(images) = docker.as_array() else {
            self.problem("manifest.json", "is not a list");
            return;
        };
        let expected: Vec<(String, Vec<String>)> = index["manifests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["digest"].as_str().and_then(blob_path))
            .filter_map(|name| self.image.read_small(&name).ok())
            .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|manifest| {
                let path = |value: &Value| value["digest"].as_str().and_then(blob_path).unwrap_or_default();
                let layers = manifest["layers"].as_array().into_iter().flatten().map(path).collect();
                (path(&manifest["config"]), layers)
            })
            .collect();
        for (i, image) in images.iter().enumerate() {
            let at = format!("manifest.json image {}", i + 1);
            let config = image["Config"].as_str().unwrap_or_default().to_string();
            let layers: Vec<String> = image["Layers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|layer| layer.as_str().map(str::to_string))
                .collect();
            for name in std::iter::once(&config).chain(&layers) {
                if !self.image.has(name) {
                    self.problem(&at, format!("names {name}, which the image does not hold"));
                }
            }
            if !expected.iter().any(|(c, l)| *c == config && *l == layers) {
                self.problem(
                    &at,
                    "names a config and layers that no manifest in index.json lists, in that order",
                );
            }
        }
    }
}

/// The image spec's grammar for `org.opencontainers.image.ref.name`: components of letters and digits joined by
/// separators (`-._:@+` or `--`), the components joined by `/`.
fn valid_ref_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|component| {
            let bytes = component.as_bytes();
            let alnum = |b: u8| b.is_ascii_alphanumeric();
            let separator = |b: u8| matches!(b, b'-' | b'.' | b'_' | b':' | b'@' | b'+');
            if bytes.is_empty() || !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
                return false;
            }
            let mut i = 0;
            while i < bytes.len() {
                if alnum(bytes[i]) {
                    i += 1;
                } else if bytes[i] == b'-'
                    && bytes.get(i + 1) == Some(&b'-')
                    && bytes.get(i + 2).copied().is_some_and(alnum)
                {
                    i += 2;
                } else if separator(bytes[i]) && bytes.get(i + 1).copied().is_some_and(alnum) {
                    i += 1;
                } else {
                    return false;
                }
            }
            true
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// An image of two small packages, built to `name` under `dir`: a `.tar` name gives an archive.
    fn built(dir: &Path, name: &str) -> PathBuf {
        let plan = crate::testing::plan_of(
            vec![
                crate::testing::simple(dir, "tool", &["base"]),
                crate::testing::simple(dir, "base", &[]),
            ],
            100,
        );
        let output = dir.join(name);
        crate::testing::build_image(dir, &plan, &output);
        output
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn blob(layout: &Path, digest: &str) -> PathBuf {
        layout.join(blob_path(digest).unwrap())
    }

    fn write_blob(layout: &Path, bytes: &[u8]) -> (String, u64) {
        let digest = digest_of(Sha256::new_with_prefix(bytes));
        fs::create_dir_all(layout.join("blobs/sha256")).unwrap();
        fs::write(blob(layout, &digest), bytes).unwrap();
        (digest, bytes.len() as u64)
    }

    /// Edit the config, then the manifest, then the index of the layout's first image, writing each changed
    /// document as a new blob and pointing its parent at it, so only the edits themselves are wrong.
    fn rewrite(
        layout: &Path,
        config_edit: impl FnOnce(&mut Value),
        manifest_edit: impl FnOnce(&mut Value),
        index_edit: impl FnOnce(&mut Value),
    ) {
        let mut index = read_json(&layout.join("index.json"));
        let mut manifest = read_json(&blob(layout, index["manifests"][0]["digest"].as_str().unwrap()));
        let mut config = read_json(&blob(layout, manifest["config"]["digest"].as_str().unwrap()));
        config_edit(&mut config);
        let (digest, size) = write_blob(layout, &serde_json::to_vec(&config).unwrap());
        manifest["config"]["digest"] = json!(digest);
        manifest["config"]["size"] = json!(size);
        manifest_edit(&mut manifest);
        let (digest, size) = write_blob(layout, &serde_json::to_vec(&manifest).unwrap());
        index["manifests"][0]["digest"] = json!(digest);
        index["manifests"][0]["size"] = json!(size);
        index_edit(&mut index);
        fs::write(layout.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        let path = |value: &Value| blob_path(value["digest"].as_str().unwrap()).unwrap();
        let docker = json!([{
            "Config": path(&manifest["config"]),
            "RepoTags": ["test:1"],
            "Layers": manifest["layers"].as_array().unwrap().iter().map(path).collect::<Vec<_>>(),
        }]);
        fs::write(layout.join("manifest.json"), serde_json::to_vec(&docker).unwrap()).unwrap();
    }

    fn layer_path(layout: &Path, n: usize) -> PathBuf {
        let index = read_json(&layout.join("index.json"));
        let manifest = read_json(&blob(layout, index["manifests"][0]["digest"].as_str().unwrap()));
        blob(layout, manifest["layers"][n]["digest"].as_str().unwrap())
    }

    /// The problems of the image at `path`, as `at: message` lines.
    fn problems(path: &Path) -> Vec<String> {
        let report = validate(path).unwrap();
        assert_eq!(report.valid, report.problems.is_empty());
        report
            .problems
            .iter()
            .map(|p| format!("{}: {}", p.at, p.message))
            .collect()
    }

    fn assert_found(problems: &[String], needles: &[&str]) {
        for needle in needles {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "no `{needle}` in {problems:#?}"
            );
        }
    }

    #[test]
    fn a_built_image_is_valid_as_a_layout_and_as_an_archive() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["layout", "image.tar"] {
            let own = dir.path().join(format!("{name}-dir"));
            fs::create_dir_all(&own).unwrap();
            let report = validate(&built(&own, name)).unwrap();
            assert!(report.valid, "{}", report.text());
            assert_eq!(report.manifests.len(), 1);
            assert_eq!(report.manifests[0].layers, 2);
            assert_eq!(report.manifests[0].platform.as_deref(), Some("linux/amd64"));
            assert!(
                report.text().contains("is a valid OCI image\n  manifest sha256:"),
                "{}",
                report.text()
            );
            let json: Value = serde_json::from_str(&report.json()).unwrap();
            assert_eq!(json["valid"], true);
            assert_eq!(json["problems"], json!([]));
        }
    }

    #[test]
    fn a_changed_layer_is_caught() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        let path = layer_path(&layout, 0);
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        let found = problems(&layout);
        assert_found(
            &found,
            &["layer 1: content hashes to sha256:", "layer 1: is not valid gzip"],
        );
        let report = validate(&layout).unwrap();
        assert!(
            report.text().contains("is not a valid OCI image: "),
            "{}",
            report.text()
        );
    }

    #[test]
    fn a_missing_blob_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        fs::remove_file(layer_path(&layout, 1)).unwrap();
        let found = problems(&layout);
        assert_found(
            &found,
            &[
                "layer 2: blob blobs/sha256/",
                "is missing",
                "manifest.json image 1: names blobs/sha256/",
            ],
        );
        // Nothing else is wrong, and the diff_id count still matches the layers.
        assert!(!found.iter().any(|p| p.contains("diff_id")), "{found:#?}");
    }

    #[test]
    fn descriptors_must_match_their_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        rewrite(
            &layout,
            |_| {},
            |manifest| {
                let size = manifest["layers"][0]["size"].as_u64().unwrap();
                manifest["layers"][0]["size"] = json!(size + 1);
                manifest["layers"][1]["mediaType"] = json!("text/plain");
                manifest["config"]["mediaType"] = json!("application/json");
                manifest["schemaVersion"] = json!(1);
                manifest["mediaType"] = json!(INDEX_MEDIA_TYPE);
            },
            |_| {},
        );
        assert_found(
            &problems(&layout),
            &[
                "layer 1: blob is",
                "but its descriptor says",
                "layer 2: mediaType text/plain is not a layer type",
                "config: mediaType application/json is not an image config",
                "schemaVersion is 1, not 2",
                "but the index lists it as",
            ],
        );
    }

    #[test]
    fn diff_ids_and_history_must_match_the_layers() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        rewrite(
            &layout,
            |config| {
                config["rootfs"]["diff_ids"][0] = json!(format!("sha256:{}", "0".repeat(64)));
                config["history"].as_array_mut().unwrap().pop();
                config["architecture"] = json!("arm64");
            },
            |_| {},
            |_| {},
        );
        assert_found(
            &problems(&layout),
            &[
                "diff_id 1 is sha256:0000",
                "but layer 1 uncompresses to sha256:",
                "history has 1 entry that are not empty_layer, for 2 layers",
                "architecture is \"arm64\", but the index says \"amd64\"",
            ],
        );

        rewrite(
            &layout,
            |config| {
                config["rootfs"] = json!({"type": "files", "diff_ids": []});
            },
            |_| {},
            |_| {},
        );
        assert_found(
            &problems(&layout),
            &[
                "rootfs.type is \"files\", not \"layers\"",
                "lists 0 diff_ids for 2 layers",
            ],
        );
    }

    #[test]
    fn the_index_and_its_entries_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        rewrite(
            &layout,
            |_| {},
            |_| {},
            |index| {
                index["schemaVersion"] = json!(3);
                index["mediaType"] = json!(MANIFEST_MEDIA_TYPE);
                index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"] = json!("bad tag!");
                let mut copy = index["manifests"][0].clone();
                copy["digest"] = json!("md5:abc");
                index["manifests"].as_array_mut().unwrap().push(copy);
                let mut odd = index["manifests"][0].clone();
                odd["mediaType"] = json!("text/plain");
                index["manifests"].as_array_mut().unwrap().push(odd);
                let mut bare = index["manifests"][0].clone();
                bare.as_object_mut().unwrap().remove("mediaType");
                index["manifests"].as_array_mut().unwrap().push(bare);
            },
        );
        assert_found(
            &problems(&layout),
            &[
                "index.json: schemaVersion is 3, not 2",
                "index.json: mediaType is application/vnd.oci.image.manifest.v1+json",
                "`bad tag!` is not a valid reference name",
                "index.json manifest 2: digest md5:abc is not a sha256 digest",
                "index.json manifest 3: mediaType text/plain is not a manifest or an index",
                "index.json manifest 4: has no mediaType",
            ],
        );

        fs::write(layout.join("index.json"), br#"{"schemaVersion": 2, "manifests": []}"#).unwrap();
        assert_found(&problems(&layout), &["index.json: lists no manifests"]);
        fs::write(layout.join("index.json"), br#"{"schemaVersion": 2}"#).unwrap();
        assert_found(&problems(&layout), &["index.json: has no manifests list"]);
        fs::write(layout.join("index.json"), b"not json").unwrap();
        assert_found(&problems(&layout), &["index.json: not JSON"]);
    }

    #[test]
    fn docker_load_must_see_the_same_image() {
        let dir = tempfile::tempdir().unwrap();
        let layout = built(dir.path(), "layout");
        let mut docker = read_json(&layout.join("manifest.json"));
        docker[0]["Layers"].as_array_mut().unwrap().reverse();
        fs::write(layout.join("manifest.json"), serde_json::to_vec(&docker).unwrap()).unwrap();
        assert_eq!(
            problems(&layout),
            ["manifest.json image 1: names a config and layers that no manifest in index.json lists, in that order"]
        );
        fs::write(layout.join("manifest.json"), b"{}").unwrap();
        assert_eq!(problems(&layout), ["manifest.json: is not a list"]);
    }

    #[test]
    fn a_directory_that_is_not_a_layout_says_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            problems(dir.path()),
            ["oci-layout: missing: every image layout has one", "index.json: missing"]
        );
        fs::write(dir.path().join("oci-layout"), br#"{"imageLayoutVersion":"2.0.0"}"#).unwrap();
        assert_found(&problems(dir.path()), &["imageLayoutVersion is \"2.0.0\""]);
        fs::write(dir.path().join("oci-layout"), b"?").unwrap();
        assert_found(&problems(dir.path()), &["oci-layout: not JSON"]);
        assert!(validate(&dir.path().join("missing")).is_err());
    }

    /// A layout written by hand: an uncompressed layer, under an index nested in the top one.
    #[test]
    fn uncompressed_layers_and_nested_indexes_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path();
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(2);
        header.set_mode(0o644);
        tar.append_data(&mut header, "hi.txt", &b"hi"[..]).unwrap();
        let (layer, layer_size) = write_blob(layout, &tar.into_inner().unwrap());
        let config = json!({
            "architecture": "arm64", "os": "linux", "variant": "v8",
            "rootfs": {"type": "layers", "diff_ids": [layer]},
        });
        let (config, config_size) = write_blob(layout, &serde_json::to_vec(&config).unwrap());
        let manifest = json!({
            "schemaVersion": 2, "mediaType": MANIFEST_MEDIA_TYPE,
            "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": config, "size": config_size},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": layer, "size": layer_size}],
        });
        let (manifest, manifest_size) = write_blob(layout, &serde_json::to_vec(&manifest).unwrap());
        let nested = json!({
            "schemaVersion": 2, "mediaType": INDEX_MEDIA_TYPE,
            "manifests": [{
                "mediaType": MANIFEST_MEDIA_TYPE, "digest": manifest, "size": manifest_size,
                "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"},
            }],
        });
        let (nested, nested_size) = write_blob(layout, &serde_json::to_vec(&nested).unwrap());
        let top = json!({
            "schemaVersion": 2,
            "manifests": [{"mediaType": INDEX_MEDIA_TYPE, "digest": nested, "size": nested_size}],
        });
        fs::write(layout.join("index.json"), serde_json::to_vec(&top).unwrap()).unwrap();
        fs::write(layout.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        let report = validate(layout).unwrap();
        assert!(report.valid, "{}", report.text());
        assert_eq!(report.manifests[0].platform.as_deref(), Some("linux/arm64/v8"));
        assert_eq!(report.manifests[0].size, layer_size);
    }

    #[test]
    fn reference_names_follow_the_spec() {
        for good in [
            "latest",
            "1.0",
            "v1.0.0-rc.1",
            "app:1",
            "quay.io/team/app:1.0",
            "a--b",
            "a+b@c",
        ] {
            assert!(valid_ref_name(good), "{good}");
        }
        for bad in ["", "-a", "a-", "a b", "a/", "/a", "a//b", "a---b", "a..b", "ä"] {
            assert!(!valid_ref_name(bad), "{bad}");
        }
    }
}
