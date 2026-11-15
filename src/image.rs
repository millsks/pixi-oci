//! The OCI image around the layers: config, manifest, and the image layout or archive they are written as.
//!
//! An [OCI image layout](https://github.com/opencontainers/image-spec/blob/main/image-layout.md) is a directory
//! with `oci-layout`, `index.json` and `blobs/sha256/`. An archive is that directory as a tar, plus the
//! `manifest.json` that `docker load` reads, so the one file loads with `docker load`, `podman load`, and
//! `skopeo copy oci-archive:`.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::layer::{self, Blob, Entry};
use crate::platform::OciPlatform;

pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

/// A layer as the image records it.
#[derive(Debug, Clone)]
pub struct ImageLayer {
    pub blob: Blob,
    /// The layer's media type: [`LAYER_MEDIA_TYPE`] for the environment's, whatever a base image used for its own.
    pub media_type: String,
    /// What put this layer there, for the config's history: `python 3.13.18 h2b5c4f5_0`. A base image's layers
    /// keep the base's own history instead.
    pub created_by: String,
}

impl ImageLayer {
    /// A layer pixi-oci wrote.
    pub fn new(blob: Blob, created_by: String) -> Self {
        ImageLayer {
            blob,
            media_type: LAYER_MEDIA_TYPE.to_string(),
            created_by,
        }
    }
}

/// The image an image is built on: its config, and its name and manifest digest for the manifest's annotations.
#[derive(Debug, Clone)]
pub struct Base {
    pub config: Value,
    pub name: String,
    pub digest: String,
    /// How many of the image's layers, from the bottom, are the base's.
    pub layers: usize,
}

/// The image's runtime settings.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub env: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub workdir: Option<String>,
    pub labels: Vec<(String, String)>,
    /// The base image, whose config the image's config starts from.
    pub base: Option<Base>,
}

/// A name and tag, as `registry/name:tag` or `name:tag`; `latest` when no tag is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub tag: String,
}

impl Reference {
    pub fn parse(value: &str) -> Option<Self> {
        // The tag is after the last colon, unless that colon is part of a registry's port (`host:5000/name`).
        let (name, tag) = match value.rsplit_once(':') {
            Some((name, tag)) if !tag.contains('/') => (name, tag),
            _ => (value, "latest"),
        };
        let valid_tag = !tag.is_empty()
            && tag.len() <= 128
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            && !tag.starts_with(['.', '-']);
        let valid_name = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/:".contains(c));
        (valid_tag && valid_name).then(|| Self {
            name: name.to_string(),
            tag: tag.to_string(),
        })
    }
}

impl Reference {
    /// The name as docker normalizes it: `app:1` is `docker.io/library/app:1`, `team/app:1` is
    /// `docker.io/team/app:1`, and a name whose first part has a dot, a colon or is `localhost` keeps its registry.
    pub fn qualified(&self) -> String {
        let has_registry = self
            .name
            .split_once('/')
            .is_some_and(|(first, _)| first.contains('.') || first.contains(':') || first == "localhost");
        let name = if has_registry {
            self.name.clone()
        } else if self.name.contains('/') {
            format!("docker.io/{}", self.name)
        } else {
            format!("docker.io/library/{}", self.name)
        };
        format!("{name}:{}", self.tag)
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.name, self.tag)
    }
}

/// The image config JSON.
pub fn config(platform: &OciPlatform, layers: &[ImageLayer], settings: &Settings, created: &str) -> Value {
    // On a base, the config starts from the base's, as FROM does: its user, ports, stop signal and the rest stay.
    // The settings below are decided by the build, which has already merged the base's into them.
    let mut config = match &settings.base {
        Some(base) => base.config["config"].as_object().cloned().unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    for key in ["Env", "Entrypoint", "Cmd", "WorkingDir"] {
        config.remove(key);
    }
    config.insert("Env".into(), json!(settings.env));
    if let Some(entrypoint) = &settings.entrypoint {
        config.insert("Entrypoint".into(), json!(entrypoint));
    }
    if let Some(cmd) = &settings.cmd {
        config.insert("Cmd".into(), json!(cmd));
    }
    if let Some(workdir) = &settings.workdir {
        config.insert("WorkingDir".into(), json!(workdir));
    }
    let mut labels = config
        .remove("Labels")
        .and_then(|labels| labels.as_object().cloned())
        .unwrap_or_default();
    for (key, value) in &settings.labels {
        labels.insert(key.clone(), json!(value));
    }
    if !labels.is_empty() {
        config.insert("Labels".into(), Value::Object(labels));
    }
    let mut image = match &settings.base {
        Some(base) => base.config.as_object().cloned().unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    // A docker build's record of the container it ran in describes the base, not this image.
    for key in ["container", "container_config", "docker_version", "author", "comment"] {
        image.remove(key);
    }
    let mut image = Value::Object(image);
    image["created"] = json!(created);
    image["architecture"] = json!(platform.architecture);
    image["os"] = json!(platform.os);
    match platform.variant {
        Some(variant) => image["variant"] = json!(variant),
        None => {
            if let Some(object) = image.as_object_mut() {
                object.remove("variant");
            }
        }
    }
    image["config"] = Value::Object(config);
    image["rootfs"] = json!({
        "type": "layers",
        "diff_ids": layers.iter().map(|layer| layer.blob.diff_id.clone()).collect::<Vec<_>>(),
    });
    let base_layers = settings.base.as_ref().map_or(0, |base| base.layers);
    let mut history: Vec<Value> = match &settings.base {
        Some(base) => base.config["history"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| (0..base.layers).map(|_| json!({"created_by": "base image"})).collect()),
        None => Vec::new(),
    };
    history.extend(
        layers
            .iter()
            .skip(base_layers)
            .map(|layer| json!({"created": created, "created_by": layer.created_by})),
    );
    image["history"] = json!(history);
    image
}

/// The image manifest JSON for a config blob and layers.
pub fn manifest(config: &Blob, layers: &[ImageLayer], base: Option<&Base>) -> Value {
    let mut manifest = json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_MEDIA_TYPE,
        "config": {"mediaType": CONFIG_MEDIA_TYPE, "digest": config.digest, "size": config.size},
        "layers": layers.iter().map(|layer| json!({
            "mediaType": layer.media_type,
            "digest": layer.blob.digest,
            "size": layer.blob.size,
        })).collect::<Vec<_>>(),
    });
    // The image spec's own annotations for the image an image is built on.
    if let Some(base) = base {
        manifest["annotations"] = json!({
            "org.opencontainers.image.base.digest": base.digest,
            "org.opencontainers.image.base.name": base.name,
        });
    }
    manifest
}

/// Write `value` as a blob into `blobs_dir`, returning its descriptor fields. The digest covers the exact bytes.
pub fn write_json_blob(value: &Value, blobs_dir: &Path) -> io::Result<Blob> {
    let bytes = serde_json::to_vec(value).expect("JSON values always serialize");
    let digest = crate::fetch::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    fs::create_dir_all(blobs_dir)?;
    fs::write(blobs_dir.join(&digest), &bytes)?;
    Ok(Blob {
        digest: format!("sha256:{digest}"),
        diff_id: format!("sha256:{digest}"),
        size: bytes.len() as u64,
    })
}

/// The finished image: where its manifest is and what it is called.
#[derive(Debug, Clone)]
pub struct Written {
    pub manifest: Blob,
    pub config: Blob,
    pub reference: Reference,
    /// Compressed size of all layers, which is what a registry stores and a pull downloads.
    pub layers_size: u64,
}

/// Write config, manifest, `index.json` and `oci-layout` into `layout`, whose `blobs/sha256` already holds the
/// layers.
pub fn finish_layout(
    layout: &Path,
    platform: &OciPlatform,
    layers: &[ImageLayer],
    settings: &Settings,
    reference: &Reference,
    created: &str,
) -> io::Result<Written> {
    let blobs = layout.join("blobs/sha256");
    let config_blob = write_json_blob(&config(platform, layers, settings, created), &blobs)?;
    let manifest_blob = write_json_blob(&manifest(&config_blob, layers, settings.base.as_ref()), &blobs)?;
    let mut platform_json = json!({"architecture": platform.architecture, "os": platform.os});
    if let Some(variant) = platform.variant {
        platform_json["variant"] = json!(variant);
    }
    let index = json!({
        "schemaVersion": 2,
        "mediaType": INDEX_MEDIA_TYPE,
        "manifests": [{
            "mediaType": MANIFEST_MEDIA_TYPE,
            "digest": manifest_blob.digest,
            "size": manifest_blob.size,
            "platform": platform_json,
            "annotations": {
                // containerd, nerdctl and docker's containerd image store read the full name, and take it as it is
                // written, so it is written fully qualified; skopeo and podman read the tag.
                "io.containerd.image.name": reference.qualified(),
                "org.opencontainers.image.ref.name": reference.tag,
            },
        }],
    });
    fs::write(layout.join("index.json"), serde_json::to_vec_pretty(&index)?)?;
    fs::write(layout.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    // What `docker load` reads: the config and the layers, by their paths in the archive.
    let docker = json!([{
        "Config": format!("blobs/sha256/{}", config_blob.digest.trim_start_matches("sha256:")),
        "RepoTags": [reference.to_string()],
        "Layers": layers.iter()
            .map(|layer| format!("blobs/sha256/{}", layer.blob.digest.trim_start_matches("sha256:")))
            .collect::<Vec<_>>(),
    }]);
    fs::write(layout.join("manifest.json"), serde_json::to_vec_pretty(&docker)?)?;
    Ok(Written {
        manifest: manifest_blob,
        config: config_blob,
        reference: reference.clone(),
        layers_size: layers.iter().map(|layer| layer.blob.size).sum(),
    })
}

/// Tar the layout directory `layout` into the archive `output`, uncompressed: its layers are compressed already.
/// The archive is written beside `output` and renamed into place when it is complete, so a failure never leaves a
/// partial archive where the image should be.
pub fn write_archive(layout: &Path, output: &Path, mtime: u64) -> io::Result<()> {
    let entries: Vec<Entry> = layer::entries(layout, &|_| false)?;
    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".pixi-oci-")
        .suffix(".tar")
        .tempfile_in(dir)?;
    let file = io::BufWriter::new(temp.reopen()?);
    let mut tar = tar::Builder::new(file);
    tar.follow_symlinks(false);
    for entry in &entries {
        let metadata = fs::metadata(&entry.source)?;
        let mut header = tar::Header::new_gnu();
        header.set_mtime(mtime);
        header.set_uid(0);
        header.set_gid(0);
        if metadata.is_dir() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_size(0);
            tar.append_data(&mut header, format!("{}/", entry.path), io::empty())?;
        } else {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(metadata.len());
            tar.append_data(&mut header, &entry.path, fs::File::open(&entry.source)?)?;
        }
    }
    let mut file = tar.into_inner()?;
    io::Write::flush(&mut file)?;
    drop(file);
    temp.persist(output).map_err(|err| err.error)?;
    Ok(())
}

/// RFC 3339 in UTC for seconds since the Unix epoch, as the image spec's `created` wants it.
pub fn rfc3339(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rem = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil algorithm: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::oci_platform;

    fn blob(n: u8) -> Blob {
        Blob {
            digest: format!("sha256:{}", format!("{n:02x}").repeat(32)),
            diff_id: format!("sha256:{}", format!("{:02x}", n + 1).repeat(32)),
            size: 100 + n as u64,
        }
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn references_parse_names_tags_and_ports() {
        let parse = |s: &str| Reference::parse(s).map(|r| (r.name, r.tag));
        assert_eq!(parse("app"), Some(("app".into(), "latest".into())));
        assert_eq!(parse("app:1.0"), Some(("app".into(), "1.0".into())));
        assert_eq!(
            parse("quay.io/kevin/app:v2"),
            Some(("quay.io/kevin/app".into(), "v2".into()))
        );
        assert_eq!(
            parse("localhost:5000/app"),
            Some(("localhost:5000/app".into(), "latest".into()))
        );
        assert_eq!(
            parse("localhost:5000/app:dev"),
            Some(("localhost:5000/app".into(), "dev".into()))
        );
        assert_eq!(parse("App"), None);
        assert_eq!(parse("app:"), None);
        assert_eq!(parse("app:-x"), None);
        assert_eq!(parse(""), None);
        assert_eq!(Reference::parse("a:b").unwrap().to_string(), "a:b");
    }

    #[test]
    fn the_containerd_name_is_fully_qualified() {
        let qualified = |value: &str| Reference::parse(value).unwrap().qualified();
        assert_eq!(qualified("app"), "docker.io/library/app:latest");
        assert_eq!(qualified("team/app:1"), "docker.io/team/app:1");
        assert_eq!(qualified("quay.io/team/app:1"), "quay.io/team/app:1");
        assert_eq!(qualified("localhost/app:2"), "localhost/app:2");
        assert_eq!(qualified("host:5000/app"), "host:5000/app:latest");
    }

    #[test]
    fn the_config_lists_every_layer_and_setting() {
        let platform = oci_platform("linux-aarch64").unwrap();
        let layers = vec![
            ImageLayer::new(blob(1), "libzlib 1.3.2 h0_0".into()),
            ImageLayer::new(blob(3), "zlib 1.3.2 h0_0".into()),
        ];
        let settings = Settings {
            env: vec!["PATH=/opt/env/bin".into()],
            entrypoint: Some(vec!["python".into()]),
            cmd: Some(vec!["-V".into()]),
            workdir: Some("/work".into()),
            labels: vec![("a".into(), "b".into())],
            base: None,
        };
        let config = config(&platform, &layers, &settings, "1970-01-01T00:00:00Z");
        assert_eq!(config["architecture"], "arm64");
        assert_eq!(config["variant"], "v8");
        assert_eq!(config["os"], "linux");
        assert_eq!(config["config"]["Env"], json!(["PATH=/opt/env/bin"]));
        assert_eq!(config["config"]["Entrypoint"], json!(["python"]));
        assert_eq!(config["config"]["Cmd"], json!(["-V"]));
        assert_eq!(config["config"]["WorkingDir"], "/work");
        assert_eq!(config["config"]["Labels"]["a"], "b");
        assert_eq!(config["rootfs"]["diff_ids"], json!([blob(1).diff_id, blob(3).diff_id]));
        assert_eq!(config["history"][1]["created_by"], "zlib 1.3.2 h0_0");

        let bare = super::config(&oci_platform("linux-64").unwrap(), &[], &Settings::default(), "x");
        assert!(bare.get("variant").is_none());
        assert_eq!(bare["config"], json!({"Env": []}));
    }

    #[test]
    fn the_layout_is_complete() {
        let layout = tempfile::tempdir().unwrap();
        let platform = oci_platform("linux-64").unwrap();
        let reference = Reference::parse("quay.io/kevin/app:1").unwrap();
        let written = finish_layout(
            layout.path(),
            &platform,
            &[],
            &Settings::default(),
            &reference,
            "1970-01-01T00:00:00Z",
        )
        .unwrap();
        let index: Value = serde_json::from_slice(&fs::read(layout.path().join("index.json")).unwrap()).unwrap();
        assert_eq!(index["manifests"][0]["digest"], written.manifest.digest);
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "1"
        );
        assert_eq!(
            fs::read_to_string(layout.path().join("oci-layout")).unwrap(),
            r#"{"imageLayoutVersion":"1.0.0"}"#
        );
        let manifest_path = layout
            .path()
            .join("blobs/sha256")
            .join(written.manifest.digest.trim_start_matches("sha256:"));
        let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["config"]["digest"], written.config.digest);
        let docker: Value = serde_json::from_slice(&fs::read(layout.path().join("manifest.json")).unwrap()).unwrap();
        assert_eq!(docker[0]["RepoTags"], json!(["quay.io/kevin/app:1"]));

        let archive = layout.path().with_extension("tar");
        write_archive(layout.path(), &archive, 0).unwrap();
        let mut names: Vec<String> = tar::Archive::new(fs::File::open(&archive).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.contains(&"index.json".to_string()));
        assert!(names.contains(&"oci-layout".to_string()));
        assert!(names.contains(&"manifest.json".to_string()));
        assert!(names.iter().any(|n| n.starts_with("blobs/sha256/")));
        fs::remove_file(archive).unwrap();
    }
}
