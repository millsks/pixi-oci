//! `pixi oci build`: a plan's layers fetched, installed and written as an OCI image.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::fetch::{FetchError, Fetcher};
use crate::image::{self, ImageLayer, Reference, Settings, Written};
use crate::install::{self, InstallError, Target};
use crate::layer;
use crate::lock::Kind;
use crate::plan::{LayerKind, Plan};
use crate::report::{plural, size_text};

/// Why an image could not be built.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum BuildError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fetch(#[from] FetchError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Install(#[from] InstallError),

    #[error("environment `{environment}` has PyPI packages, which pixi-oci does not install yet: {}", .names.join(", "))]
    #[diagnostic(help("build an environment without PyPI dependencies for now; PyPI support is on the roadmap"))]
    Pypi { environment: String, names: Vec<String> },

    #[error("{} already exists", .path.display())]
    #[diagnostic(help("pass --force to replace it, or pick another --output"))]
    OutputExists { path: PathBuf },

    #[error("the image prefix must be an absolute path, not `{prefix}`")]
    RelativePrefix { prefix: String },

    #[error("could not write {}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "pixi-oci builds images on Linux and macOS; Windows cannot store the Unix modes and symlinks a layer holds"
    )]
    UnsupportedHost,
}

/// Everything a build needs besides the plan.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Where the environment lives in the image.
    pub prefix: String,
    /// A directory for an image layout, or a `.tar` path for an archive.
    pub output: PathBuf,
    pub force: bool,
    pub reference: Reference,
    /// Extra `KEY=VALUE` environment variables, after the ones pixi-oci sets.
    pub env: Vec<String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub workdir: Option<String>,
    /// Seconds since the Unix epoch, stamped on every file and on the image.
    pub source_date_epoch: u64,
}

impl BuildOptions {
    /// Whether `output` names an archive rather than a layout directory.
    pub fn is_archive(&self) -> bool {
        self.output.extension().is_some_and(|extension| extension == "tar")
    }
}

/// Build `plan` as an image. Progress goes to `progress`, one line per layer.
pub fn build(
    plan: &Plan,
    options: &BuildOptions,
    fetcher: &Fetcher,
    progress: &mut dyn Write,
) -> Result<Written, BuildError> {
    if cfg!(windows) {
        return Err(BuildError::UnsupportedHost);
    }
    if !options.prefix.starts_with('/') {
        return Err(BuildError::RelativePrefix {
            prefix: options.prefix.clone(),
        });
    }
    let mut pypi: Vec<String> = plan
        .layers
        .iter()
        .flat_map(|layer| &layer.packages)
        .filter(|package| package.kind == Kind::Pypi)
        .map(|package| package.name.clone())
        .collect();
    if !pypi.is_empty() {
        pypi.sort();
        return Err(BuildError::Pypi {
            environment: plan.environment.clone(),
            names: pypi,
        });
    }
    prepare_output(&options.output, options.force)?;

    let python_version = plan
        .layers
        .iter()
        .flat_map(|layer| &layer.packages)
        .find(|package| package.name == "python")
        .map(|package| package.version.as_str());
    let target = Target::new(&options.prefix, &plan.platform, python_version)?;

    // An archive is a layout tarred up, so both are first written as a layout.
    let scratch = tempfile::tempdir().map_err(write_err(std::env::temp_dir()))?;
    let layout = if options.is_archive() {
        scratch.path().join("layout")
    } else {
        options.output.clone()
    };
    let blobs = layout.join("blobs/sha256");
    fs::create_dir_all(&blobs).map_err(write_err(&blobs))?;

    let total = plan.layers.len();
    let mut layers = Vec::with_capacity(total);
    for (i, planned) in plan.layers.iter().enumerate() {
        let _ = writeln!(
            progress,
            "[{}/{total}] {} ({})",
            i + 1,
            describe(planned),
            size_text(planned.size)
        );
        let staging = tempfile::tempdir_in(scratch.path()).map_err(write_err(scratch.path()))?;
        for package in &planned.packages {
            let extracted = fetcher.extracted(package)?;
            install::install(package, &extracted, staging.path(), &target)?;
        }
        let skip = install::skip_in_layer(&target.prefix, i == 0);
        let entries = layer::entries(staging.path(), &skip).map_err(write_err(staging.path()))?;
        let blob = layer::write(&entries, &blobs, options.source_date_epoch).map_err(write_err(&blobs))?;
        layers.push(ImageLayer {
            blob,
            created_by: created_by(planned),
        });
    }

    let created = image::rfc3339(options.source_date_epoch);
    let written = image::finish_layout(
        &layout,
        &plan.oci_platform,
        &layers,
        &settings(plan, options),
        &options.reference,
        &created,
    )
    .map_err(write_err(&layout))?;
    if options.is_archive() {
        image::write_archive(&layout, &options.output, options.source_date_epoch)
            .map_err(write_err(&options.output))?;
    }
    Ok(written)
}

/// The image's settings: the environment on `PATH` and as `CONDA_PREFIX`, then what was asked for.
fn settings(plan: &Plan, options: &BuildOptions) -> Settings {
    let prefix = options.prefix.trim_end_matches('/');
    let mut env = vec![
        format!("PATH={prefix}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        format!("CONDA_PREFIX={prefix}"),
    ];
    env.extend(options.env.iter().cloned());
    Settings {
        env,
        entrypoint: options.entrypoint.clone(),
        cmd: options.cmd.clone(),
        workdir: options.workdir.clone(),
        labels: vec![
            ("dev.pixi.environment".to_string(), plan.environment.clone()),
            ("dev.pixi.platform".to_string(), plan.platform.clone()),
        ],
    }
}

fn prepare_output(output: &Path, force: bool) -> Result<(), BuildError> {
    if !output.exists() {
        if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(write_err(parent))?;
        }
        return Ok(());
    }
    if !force {
        return Err(BuildError::OutputExists {
            path: output.to_path_buf(),
        });
    }
    let removed = if output.is_dir() {
        fs::remove_dir_all(output)
    } else {
        fs::remove_file(output)
    };
    removed.map_err(write_err(output))
}

fn describe(layer: &crate::plan::Layer) -> String {
    match layer.kind {
        LayerKind::Conda => {
            let package = &layer.packages[0];
            format!("{} {}", package.name, package.version)
        }
        LayerKind::CondaMerged => format!("{} more conda packages", layer.packages.len()),
        LayerKind::Pypi => plural(layer.packages.len(), "PyPI package"),
    }
}

/// A layer's line in the image history: each package as `name version build`.
fn created_by(layer: &crate::plan::Layer) -> String {
    let packages: Vec<String> = layer
        .packages
        .iter()
        .map(|package| match &package.build {
            Some(build) => format!("{} {} {build}", package.name, package.version),
            None => format!("{} {}", package.name, package.version),
        })
        .collect();
    format!("pixi-oci: {}", packages.join(", "))
}

fn write_err(path: impl AsRef<Path>) -> impl FnOnce(std::io::Error) -> BuildError {
    let path = path.as_ref().to_path_buf();
    move |source| BuildError::Write { path, source }
}

/// The image command line as given: a JSON array of strings (`["python", "-m", "http.server"]`), or words split on
/// whitespace.
pub fn parse_command(value: &str) -> Result<Vec<String>, String> {
    let trimmed = value.trim();
    let words: Vec<String> = if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|err| format!("not a JSON array of strings: {err}"))?
    } else {
        trimmed.split_whitespace().map(str::to_string).collect()
    };
    if words.is_empty() {
        Err("the command is empty".to_string())
    } else {
        Ok(words)
    }
}

/// A `KEY=VALUE` environment variable, checked.
pub fn parse_env(value: &str) -> Result<String, String> {
    match value.split_once('=') {
        Some((key, _))
            if !key.is_empty()
                && !key.starts_with(|c: char| c.is_ascii_digit())
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            Ok(value.to_string())
        }
        _ => Err("expected KEY=VALUE, with a KEY of letters, digits and underscores".to_string()),
    }
}

/// `SOURCE_DATE_EPOCH` from the environment, the convention reproducible builds share; 0 when unset or unreadable.
pub fn source_date_epoch() -> u64 {
    std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{Locked, Package};
    use std::io::Read;

    use rattler_conda_types::compression_level::CompressionLevel;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{PackageName, PackageRecord, RepoDataRecord, Version};
    use rattler_package_streaming::write::write_tar_bz2_package;
    use sha2::{Digest, Sha256};

    const PLACEHOLDER: &str = "/opt/conda-build-placeholder_placehold_placehold_placehold_placehold";

    /// A `.tar.bz2` conda package with a script that names the placeholder prefix and, on Unix, a symlink to it.
    fn package(dir: &Path, name: &str, depends: &[&str]) -> Package {
        let root = dir.join(format!("src-{name}"));
        fs::create_dir_all(root.join("info")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        let script = format!("#!{PLACEHOLDER}/bin/sh\necho {name} in {PLACEHOLDER}\n");
        fs::write(root.join("bin").join(name), &script).unwrap();
        let mut paths = vec![serde_json::json!({
            "_path": format!("bin/{name}"),
            "path_type": "hardlink",
            "sha256": crate::fetch::hex(&Sha256::digest(script.as_bytes())),
            "size_in_bytes": script.len(),
            "prefix_placeholder": PLACEHOLDER,
            "file_mode": "text",
        })];
        let mut files = vec![PathBuf::from(format!("bin/{name}"))];
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("bin").join(name), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink(name, root.join("bin").join(format!("{name}-link"))).unwrap();
            paths.push(serde_json::json!({"_path": format!("bin/{name}-link"), "path_type": "softlink"}));
            files.push(PathBuf::from(format!("bin/{name}-link")));
        }
        let index = serde_json::json!({
            "name": name, "version": "1.0", "build": "h0_0", "build_number": 0,
            "subdir": "linux-64", "depends": depends,
        });
        fs::write(root.join("info/index.json"), index.to_string()).unwrap();
        fs::write(
            root.join("info/paths.json"),
            serde_json::json!({"paths_version": 1, "paths": paths}).to_string(),
        )
        .unwrap();
        files.push(PathBuf::from("info/index.json"));
        files.push(PathBuf::from("info/paths.json"));
        let files: Vec<PathBuf> = files.iter().map(|file| root.join(file)).collect();
        let file_name = format!("{name}-1.0-h0_0.tar.bz2");
        let archive = dir.join(&file_name);
        write_tar_bz2_package(
            fs::File::create(&archive).unwrap(),
            &root,
            &files,
            CompressionLevel::Default,
            None,
            None,
        )
        .unwrap();
        let sha256 = crate::fetch::hex(&Sha256::digest(fs::read(&archive).unwrap()));
        let url = url::Url::from_file_path(&archive).unwrap();
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            "1.0".parse::<Version>().unwrap(),
            "h0_0".parse().unwrap(),
        );
        record.subdir = "linux-64".to_string();
        record.depends = depends.iter().map(ToString::to_string).collect();
        Package {
            name: name.to_string(),
            version: "1.0".to_string(),
            kind: Kind::Conda,
            build: Some("h0_0".to_string()),
            depends: depends.iter().map(ToString::to_string).collect(),
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

    fn plan_of(packages: Vec<Package>, max_layers: usize) -> Plan {
        let locked = Locked {
            environment: "test".to_string(),
            platform: "linux-64".to_string(),
            packages,
        };
        crate::plan::plan(locked, max_layers).unwrap()
    }

    fn options(output: PathBuf) -> BuildOptions {
        BuildOptions {
            prefix: "/opt/env".to_string(),
            output,
            force: false,
            reference: Reference::parse("test:1").unwrap(),
            env: vec!["GREETING=hi".to_string()],
            entrypoint: Some(vec!["base".to_string()]),
            cmd: None,
            workdir: None,
            source_date_epoch: 0,
        }
    }

    fn blob(layout: &Path, digest: &str) -> PathBuf {
        layout.join("blobs/sha256").join(digest.trim_start_matches("sha256:"))
    }

    /// Every file of a layer blob: path to (contents, mode, symlink target).
    fn layer_files(path: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, u32, Option<String>)> {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(path).unwrap()));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mode = entry.header().mode().unwrap();
                let link = entry.link_name().unwrap().map(|l| l.to_string_lossy().into_owned());
                let mut content = Vec::new();
                entry.read_to_end(&mut content).unwrap();
                (name, (content, mode, link))
            })
            .collect()
    }

    fn manifest(layout: &Path, written: &Written) -> serde_json::Value {
        serde_json::from_slice(&fs::read(blob(layout, &written.manifest.digest)).unwrap()).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_layout_holds_one_layer_per_package_with_the_prefix_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(
            vec![package(dir.path(), "tool", &["base"]), package(dir.path(), "base", &[])],
            100,
        );
        let layout = dir.path().join("layout");
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let mut progress = Vec::new();
        let written = build(&plan, &options(layout.clone()), &fetcher, &mut progress).unwrap();
        let expected = format!(
            "[1/2] base 1.0 ({})\n[2/2] tool 1.0 ({})\n",
            size_text(plan.layers[0].size),
            size_text(plan.layers[1].size)
        );
        assert_eq!(String::from_utf8(progress).unwrap(), expected);
        assert_eq!(written.reference.to_string(), "test:1");

        let manifest = manifest(&layout, &written);
        let layers = manifest["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().all(|l| l["mediaType"] == crate::image::LAYER_MEDIA_TYPE));

        let bottom = layer_files(&blob(&layout, layers[0]["digest"].as_str().unwrap()));
        let (script, mode, _) = &bottom["opt/env/bin/base"];
        assert_eq!(
            String::from_utf8_lossy(script),
            "#!/opt/env/bin/sh\necho base in /opt/env\n"
        );
        assert_eq!(*mode, 0o755);
        assert_eq!(bottom["opt/env/bin/base-link"].2.as_deref(), Some("base"));
        assert!(bottom.contains_key("opt/env/conda-meta/base-1.0-h0_0.json"));
        assert!(bottom.contains_key("opt/env/conda-meta/history"));
        assert!(!bottom.keys().any(|path| path.ends_with("CACHEDIR.TAG")));
        assert!(!bottom.keys().any(|path| path.contains("tool")));

        let top = layer_files(&blob(&layout, layers[1]["digest"].as_str().unwrap()));
        assert!(top.contains_key("opt/env/bin/tool"));
        assert!(top.contains_key("opt/env/conda-meta/tool-1.0-h0_0.json"));
        assert!(!top.contains_key("opt/env/conda-meta/history"));

        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(blob(&layout, &written.config.digest)).unwrap()).unwrap();
        assert_eq!(
            config["config"]["Env"],
            serde_json::json!([
                "PATH=/opt/env/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                "CONDA_PREFIX=/opt/env",
                "GREETING=hi"
            ])
        );
        assert_eq!(config["config"]["Entrypoint"], serde_json::json!(["base"]));
        assert_eq!(config["config"]["Labels"]["dev.pixi.environment"], "test");
        assert_eq!(config["history"][1]["created_by"], "pixi-oci: tool 1.0 h0_0");
        assert_eq!(config["rootfs"]["diff_ids"].as_array().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn the_same_plan_gives_the_same_image_and_archives_hold_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(
            vec![package(dir.path(), "a", &[]), package(dir.path(), "b", &["a"])],
            100,
        );
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let first = build(&plan, &options(dir.path().join("one")), &fetcher, &mut std::io::sink()).unwrap();
        let second = build(&plan, &options(dir.path().join("two")), &fetcher, &mut std::io::sink()).unwrap();
        assert_eq!(first.manifest, second.manifest);

        let archive = dir.path().join("image.tar");
        let third = build(&plan, &options(archive.clone()), &fetcher, &mut std::io::sink()).unwrap();
        assert_eq!(first.manifest, third.manifest);
        let names: Vec<String> = tar::Archive::new(fs::File::open(&archive).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&format!(
            "blobs/sha256/{}",
            first.manifest.digest.trim_start_matches("sha256:")
        )));
        let again = dir.path().join("again.tar");
        build(&plan, &options(again.clone()), &fetcher, &mut std::io::sink()).unwrap();
        assert_eq!(fs::read(&archive).unwrap(), fs::read(&again).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_merged_layer_holds_several_packages() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(
            vec![
                package(dir.path(), "a", &[]),
                package(dir.path(), "b", &[]),
                package(dir.path(), "c", &[]),
            ],
            2,
        );
        let layout = dir.path().join("layout");
        let written = build(
            &plan,
            &options(layout.clone()),
            &Fetcher::new(dir.path().join("cache")),
            &mut std::io::sink(),
        )
        .unwrap();
        let manifest = manifest(&layout, &written);
        let top = layer_files(&blob(&layout, manifest["layers"][1]["digest"].as_str().unwrap()));
        assert!(top.contains_key("opt/env/bin/b") && top.contains_key("opt/env/bin/c"));
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(blob(&layout, &written.config.digest)).unwrap()).unwrap();
        assert_eq!(config["history"][1]["created_by"], "pixi-oci: b 1.0 h0_0, c 1.0 h0_0");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_output_is_kept_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_of(vec![package(dir.path(), "a", &[])], 100);
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let output = dir.path().join("image.tar");
        fs::write(&output, b"previous").unwrap();
        let err = build(&plan, &options(output.clone()), &fetcher, &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, BuildError::OutputExists { .. }), "{err}");
        assert_eq!(fs::read(&output).unwrap(), b"previous");
        let mut forced = options(output.clone());
        forced.force = true;
        build(&plan, &forced, &fetcher, &mut std::io::sink()).unwrap();
        assert_ne!(fs::read(&output).unwrap(), b"previous");

        let layout = dir.path().join("layout");
        fs::create_dir_all(layout.join("old")).unwrap();
        let mut forced = options(layout.clone());
        forced.force = true;
        build(&plan, &forced, &fetcher, &mut std::io::sink()).unwrap();
        assert!(!layout.join("old").exists());
    }

    #[test]
    fn pypi_packages_and_relative_prefixes_are_refused_before_any_work() {
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let mut pypi = Package {
            name: "six".to_string(),
            version: "1.17.0".to_string(),
            kind: Kind::Pypi,
            build: None,
            depends: Vec::new(),
            location: "https://files.pythonhosted.org/six.whl".to_string(),
            sha256: None,
            size: None,
            record: None,
        };
        let plan = plan_of(vec![pypi.clone()], 100);
        let output = dir.path().join("image.tar");
        let err = build(&plan, &options(output.clone()), &fetcher, &mut std::io::sink()).unwrap_err();
        if cfg!(windows) {
            assert!(matches!(err, BuildError::UnsupportedHost));
            return;
        }
        assert_eq!(
            err.to_string(),
            "environment `test` has PyPI packages, which pixi-oci does not install yet: six"
        );
        pypi.kind = Kind::Conda;
        let mut relative = options(output.clone());
        relative.prefix = "opt/env".to_string();
        let err = build(&plan_of(vec![pypi], 100), &relative, &fetcher, &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, BuildError::RelativePrefix { .. }), "{err}");
        assert!(!output.exists());
    }

    #[test]
    fn the_epoch_comes_from_the_environment_convention() {
        // Only read here: setting the variable would race the other tests.
        let expected = std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        assert_eq!(source_date_epoch(), expected);
    }

    #[test]
    fn commands_are_json_or_words() {
        assert_eq!(
            parse_command(r#"["python", "-m", "http.server"]"#).unwrap(),
            ["python", "-m", "http.server"]
        );
        assert_eq!(parse_command("  python  -V ").unwrap(), ["python", "-V"]);
        assert_eq!(parse_command(r#"["say hi"]"#).unwrap(), ["say hi"]);
        assert!(parse_command("").is_err());
        assert!(parse_command("[]").is_err());
        assert!(parse_command("[1]").is_err());
    }

    #[test]
    fn environment_variables_need_a_key() {
        assert_eq!(parse_env("A=1").unwrap(), "A=1");
        assert_eq!(
            parse_env("SSL_CERT_FILE=/opt/env/ssl/cacert.pem").unwrap(),
            "SSL_CERT_FILE=/opt/env/ssl/cacert.pem"
        );
        assert_eq!(parse_env("EMPTY=").unwrap(), "EMPTY=");
        assert!(parse_env("NOVALUE").is_err());
        assert!(parse_env("=x").is_err());
        assert!(parse_env("1A=x").is_err());
        assert!(parse_env("A-B=x").is_err());
    }
}
