//! Conda packages made in tests, and images read back, for the build and parity tests.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use rattler_conda_types::compression_level::CompressionLevel;
use rattler_conda_types::package::DistArchiveIdentifier;
use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, Version};
use sha2::{Digest, Sha256};

use crate::lock::{Kind, Locked, Package};
use crate::plan::{Layering, Plan};

/// The prefix conda-build leaves in files for installation to replace.
pub const PLACEHOLDER: &str = "/opt/conda-build-placeholder_placehold_placehold_placehold_placehold";

/// One file of a test package.
pub struct File {
    pub path: String,
    pub content: Vec<u8>,
    pub executable: bool,
    /// `Some("text")` or `Some("binary")` when the content holds the placeholder.
    pub placeholder: Option<&'static str>,
}

/// What a test package holds.
pub struct Spec {
    pub name: String,
    pub version: String,
    pub depends: Vec<String>,
    pub files: Vec<File>,
    /// (path, target), created as symlinks; Unix only.
    pub symlinks: Vec<(String, String)>,
    pub empty_dirs: Vec<String>,
    /// Makes the package `noarch: python`, with these `command = module:function` entry points.
    pub noarch_python: Option<Vec<String>>,
    /// A `.conda` archive, else a `.tar.bz2`.
    pub conda_format: bool,
    /// Paths listed in paths.json with no file in the archive, as a damaged or crafted package might.
    pub listed_only: Vec<String>,
}

impl Spec {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            version: "1.0".to_string(),
            depends: Vec::new(),
            files: Vec::new(),
            symlinks: Vec::new(),
            empty_dirs: Vec::new(),
            noarch_python: None,
            conda_format: false,
            listed_only: Vec::new(),
        }
    }

    pub fn file(
        mut self,
        path: &str,
        content: impl Into<Vec<u8>>,
        executable: bool,
        placeholder: Option<&'static str>,
    ) -> Self {
        self.files.push(File {
            path: path.to_string(),
            content: content.into(),
            executable,
            placeholder,
        });
        self
    }
}

/// A package with a script naming the placeholder and, on Unix, a symlink to it: the everyday test package.
pub fn simple(dir: &Path, name: &str, depends: &[&str]) -> Package {
    let script = format!("#!{PLACEHOLDER}/bin/sh\necho {name} in {PLACEHOLDER}\n");
    let mut spec = Spec::new(name).file(&format!("bin/{name}"), script, true, Some("text"));
    spec.depends = depends.iter().map(ToString::to_string).collect();
    spec.symlinks.push((format!("bin/{name}-link"), name.to_string()));
    package(dir, spec)
}

/// Write `spec` as an archive in `dir` and return the locked package for it, with a `file://` location.
pub fn package(dir: &Path, spec: Spec) -> Package {
    let root = dir.join(format!("src-{}", spec.name));
    fs::create_dir_all(root.join("info")).unwrap();
    let mut paths = Vec::new();
    let mut files = Vec::new();
    for file in &spec.files {
        let path = root.join(&file.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &file.content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if file.executable { 0o755 } else { 0o644 };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let mut entry = serde_json::json!({
            "_path": file.path,
            "path_type": "hardlink",
            "sha256": crate::fetch::hex(&Sha256::digest(&file.content)),
            "size_in_bytes": file.content.len(),
        });
        if let Some(mode) = file.placeholder {
            entry["prefix_placeholder"] = PLACEHOLDER.into();
            entry["file_mode"] = mode.into();
        }
        paths.push(entry);
        files.push(path);
    }
    #[cfg(unix)]
    for (path, target) in &spec.symlinks {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, &full).unwrap();
        paths.push(serde_json::json!({"_path": path, "path_type": "softlink"}));
        files.push(full);
    }
    for path in &spec.listed_only {
        paths.push(serde_json::json!({"_path": path, "path_type": "hardlink", "size_in_bytes": 1}));
    }
    for dir in &spec.empty_dirs {
        fs::create_dir_all(root.join(dir)).unwrap();
        paths.push(serde_json::json!({"_path": dir, "path_type": "directory"}));
        files.push(root.join(dir));
    }
    let mut index = serde_json::json!({
        "name": spec.name, "version": spec.version, "build": "h0_0", "build_number": 0,
        "subdir": if spec.noarch_python.is_some() { "noarch" } else { "linux-64" },
        "depends": spec.depends,
    });
    if let Some(entry_points) = &spec.noarch_python {
        index["noarch"] = "python".into();
        let link = serde_json::json!({
            "noarch": {"type": "python", "entry_points": entry_points},
            "package_metadata_version": 1,
        });
        fs::write(root.join("info/link.json"), link.to_string()).unwrap();
        files.push(root.join("info/link.json"));
    }
    fs::write(root.join("info/index.json"), index.to_string()).unwrap();
    fs::write(
        root.join("info/paths.json"),
        serde_json::json!({"paths_version": 1, "paths": paths}).to_string(),
    )
    .unwrap();
    files.push(root.join("info/index.json"));
    files.push(root.join("info/paths.json"));

    let stem = format!("{}-{}-h0_0", spec.name, spec.version);
    let file_name = if spec.conda_format {
        format!("{stem}.conda")
    } else {
        format!("{stem}.tar.bz2")
    };
    let archive = dir.join(&file_name);
    let out = fs::File::create(&archive).unwrap();
    if spec.conda_format {
        rattler_package_streaming::write::write_conda_package(
            out,
            &root,
            &files,
            CompressionLevel::Default,
            None,
            &stem,
            None,
            None,
        )
        .unwrap();
    } else {
        rattler_package_streaming::write::write_tar_bz2_package(
            out,
            &root,
            &files,
            CompressionLevel::Default,
            None,
            None,
        )
        .unwrap();
    }
    let sha256 = crate::fetch::hex(&Sha256::digest(fs::read(&archive).unwrap()));
    let url = url::Url::from_file_path(&archive).unwrap();
    let mut record = PackageRecord::new(
        PackageName::new_unchecked(spec.name.as_str()),
        spec.version.parse::<Version>().unwrap(),
        "h0_0".parse().unwrap(),
    );
    record.subdir = if spec.noarch_python.is_some() {
        "noarch"
    } else {
        "linux-64"
    }
    .to_string();
    if spec.noarch_python.is_some() {
        record.noarch = rattler_conda_types::NoArchType::python();
    }
    record.depends = spec.depends.clone();
    Package {
        name: spec.name.clone(),
        version: spec.version.clone(),
        kind: Kind::Conda,
        build: Some("h0_0".to_string()),
        depends: spec.depends,
        location: url.to_string(),
        sha256: Some(sha256),
        size: Some(fs::metadata(&archive).unwrap().len()),
        record: Some(Box::new(RepoDataRecord {
            package_record: record,
            identifier: file_name.parse::<DistArchiveIdentifier>().unwrap(),
            url,
            channel: None,
        })),
    }
}

/// The plan for `packages` as one linux-64 environment named `test`, a layer per package.
pub fn plan_of(packages: Vec<Package>, max_layers: usize) -> Plan {
    plan_with(packages, Layering::PerPackage, max_layers)
}

/// The plan for `packages` as one linux-64 environment named `test`, laid out as `layering` says.
pub fn plan_with(packages: Vec<Package>, layering: Layering, max_layers: usize) -> Plan {
    let locked = Locked {
        environment: "test".to_string(),
        platform: "linux-64".to_string(),
        packages,
    };
    crate::plan::plan(locked, layering, max_layers).unwrap()
}

/// The layer blobs of the image in `layout`, bottom first.
pub fn layout_layers(layout: &Path) -> Vec<PathBuf> {
    let blob = |digest: &str| layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"));
    let index: serde_json::Value = serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(blob(index["manifests"][0]["digest"].as_str().unwrap())).unwrap()).unwrap();
    manifest["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| blob(layer["digest"].as_str().unwrap()))
        .collect()
}

/// What an image's file system holds once its layers are applied in order: path to (kind, mode, content).
/// Content is a file's bytes, a symlink's target, or for a `conda-meta` record its JSON with every list sorted, so
/// two records compare by what they say rather than the order rattler happened to write them in.
pub fn flatten(layers: &[PathBuf]) -> BTreeMap<String, (&'static str, u32, Vec<u8>)> {
    let mut files = BTreeMap::new();
    for layer in layers {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(layer).unwrap()));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string();
            let mode = entry.header().mode().unwrap();
            let value = match entry.header().entry_type() {
                tar::EntryType::Directory => ("dir", mode, Vec::new()),
                tar::EntryType::Symlink => {
                    let link = entry.link_name().unwrap().unwrap().to_string_lossy().into_owned();
                    ("symlink", mode, link.into_bytes())
                }
                _ => {
                    let mut content = Vec::new();
                    entry.read_to_end(&mut content).unwrap();
                    if path.contains("/conda-meta/") && path.ends_with(".json") {
                        content = normalized_record(&content);
                    }
                    ("file", mode, content)
                }
            };
            files.insert(path, value);
        }
    }
    files
}

fn normalized_record(json: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(json).unwrap();
    if let Some(paths) = value["paths_data"]["paths"].as_array_mut() {
        paths.sort_by(|a, b| a["_path"].as_str().cmp(&b["_path"].as_str()));
    }
    if let Some(files) = value["files"].as_array_mut() {
        files.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
    serde_json::to_vec_pretty(&value).unwrap()
}

/// Panic with the first differences when two flattened images differ.
pub fn assert_same(
    reference: &BTreeMap<String, (&'static str, u32, Vec<u8>)>,
    streamed: &BTreeMap<String, (&'static str, u32, Vec<u8>)>,
) {
    let mut problems = Vec::new();
    for (path, value) in reference {
        match streamed.get(path) {
            None => problems.push(format!("only in the reference: {path}")),
            Some(other) if other != value => problems.push(format!(
                "differs: {path}: reference {} {:o}, streamed {} {:o}{}",
                value.0,
                value.1,
                other.0,
                other.1,
                if other.2 != value.2 {
                    first_difference(&value.2, &other.2)
                } else {
                    String::new()
                }
            )),
            Some(_) => {}
        }
    }
    for path in streamed.keys().filter(|path| !reference.contains_key(*path)) {
        problems.push(format!("only in the streamed image: {path}"));
    }
    assert!(
        problems.is_empty(),
        "{} of {} entries differ:\n{}",
        problems.len(),
        reference.len().max(streamed.len()),
        problems.iter().take(10).cloned().collect::<Vec<_>>().join("\n")
    );
}

/// The first line where two contents differ, with a few lines of context, for a readable failure.
fn first_difference(reference: &[u8], streamed: &[u8]) -> String {
    let reference = String::from_utf8_lossy(reference);
    let streamed = String::from_utf8_lossy(streamed);
    let (a, b): (Vec<&str>, Vec<&str>) = (reference.lines().collect(), streamed.lines().collect());
    let at = a
        .iter()
        .zip(&b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let from = at.saturating_sub(4);
    format!(
        "; first difference at line {}:\n  reference: {}\n  streamed:  {}",
        at + 1,
        a[from..(at + 1).min(a.len())].join("\n             "),
        b[from..(at + 1).min(b.len())].join("\n             ")
    )
}

/// Build `plan` to `output` (a `.tar` name gives an archive) as the image `test:1`, with an env var, an entrypoint
/// and pixi's labels, from packages made under `dir`.
pub fn build_image(dir: &Path, plan: &Plan, output: &Path) -> crate::image::Written {
    let options = crate::build::BuildOptions {
        prefix: "/opt/env".to_string(),
        output: output.to_path_buf(),
        force: true,
        reference: crate::image::Reference::parse("test:1").unwrap(),
        env: vec!["GREETING=hi".to_string()],
        entrypoint: Some(vec!["tool".to_string(), "--serve".to_string()]),
        cmd: None,
        workdir: Some("/work".to_string()),
        source_date_epoch: 0,
        max_memory: 16 << 20,
        base: None,
        labels: Vec::new(),
        annotations: Vec::new(),
        user: None,
        expose: Vec::new(),
        stop_signal: None,
    };
    let fetcher = crate::fetch::Fetcher::new(dir.join("cache"));
    crate::build::build(plan, &options, &fetcher, &mut std::io::sink()).unwrap()
}

/// One file of a test base image: its path, contents and mode, or a symlink target.
pub enum BaseEntry<'a> {
    File(&'a str, Vec<u8>, u32),
    Link(&'a str, &'a str),
}

/// A base image as `Registry::pull` returns one: one gzip layer of `entries`, cached under `dir`, and a docker-style
/// config with a user, a command, labels, an empty-layer history entry and a docker build's leftovers.
pub fn base_image(dir: &Path, entries: &[BaseEntry]) -> crate::registry::Pulled {
    use std::io::Write as _;
    let mut tar = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_gnu();
        match entry {
            BaseEntry::File(path, content, mode) => {
                header.set_size(content.len() as u64);
                header.set_mode(*mode);
                tar.append_data(&mut header, path, content.as_slice()).unwrap();
            }
            BaseEntry::Link(path, target) => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                tar.append_link(&mut header, path, target).unwrap();
            }
        }
    }
    let tar = tar.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).unwrap();
    let layer = gz.finish().unwrap();
    let digest = format!("sha256:{}", crate::fetch::hex(&Sha256::digest(&layer)));
    let path = dir.join("base-layer.tar.gz");
    fs::write(&path, &layer).unwrap();
    crate::registry::Pulled {
        reference: crate::registry::ImageRef::parse(&format!("example.com/os/base:9@sha256:{}", "b".repeat(64)))
            .unwrap(),
        manifest_digest: format!("sha256:{}", "c".repeat(64)),
        config: serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "container_config": {"Cmd": ["/bin/sh", "-c", "#(nop) build step"]},
            "docker_version": "20.10",
            "config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin:/bin", "LANG=C.UTF-8", "CONDA_PREFIX=/wrong"],
                "Cmd": ["/bin/bash"],
                "User": "1001",
                "WorkingDir": "/home/base",
                "Labels": {"vendor": "Example", "dev.pixi.environment": "overridden"},
            },
            "rootfs": {"type": "layers", "diff_ids": [format!("sha256:{}", crate::fetch::hex(&Sha256::digest(&tar)))]},
            "history": [
                {"created_by": "ADD rootfs.tar /"},
                {"created_by": "ENV LANG=C.UTF-8", "empty_layer": true},
            ],
        }),
        layers: vec![crate::registry::PulledLayer {
            digest,
            size: layer.len() as u64,
            media_type: crate::image::LAYER_MEDIA_TYPE.to_string(),
            path,
        }],
    }
}
