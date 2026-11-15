//! `pixi oci build`: a plan's layers fetched, installed and written as an OCI image.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::fetch::{FetchError, Fetcher};
use crate::image::{self, ImageLayer, Reference, Settings, Written};
use crate::lock::Kind;
use crate::plan::{LayerKind, Plan};
use crate::report::{plural, size_text};
use crate::stream::{self, StreamError};
use crate::target::{Target, TargetError};

/// Why an image could not be built.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum BuildError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Fetch(#[from] FetchError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Target(#[from] TargetError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Stream(#[from] StreamError),

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
    /// Roughly how much memory the build's own buffers may use, in bytes. It sets how many layers are written at
    /// once and how large a rewritten file may grow in memory before it waits in a temporary file. It never changes
    /// the image.
    pub max_memory: u64,
}

/// The default for `--max-memory`: 512 MiB.
pub const DEFAULT_MAX_MEMORY: u64 = 512 * 1024 * 1024;

/// How a memory budget is spent: the largest file rewritten whole in memory, and the layers written at once.
///
/// A layer being written holds at most one file being rewritten in memory, twice (read in, written out); a larger
/// file is rewritten in 4 MiB pieces from a temporary file. On top of that come about 16 MiB of decompression
/// window (conda-forge's zstd frames use up to 8 MiB) and archive, tar and gzip buffers. The in-memory limit is an
/// eighth of the budget, between 1 and 16 MiB, and as many layers run at once as fit, at least one and no more than
/// the machine has cores.
pub fn budget(max_memory: u64, cores: usize) -> (usize, usize) {
    const MIB: u64 = 1024 * 1024;
    let spill = (max_memory / 8).clamp(MIB, 16 * MIB);
    let per_layer = 2 * spill + 16 * MIB;
    let jobs = (max_memory / per_layer).clamp(1, cores.max(1) as u64);
    (spill as usize, jobs as usize)
}

impl BuildOptions {
    /// Whether `output` names an archive rather than a layout directory.
    pub fn is_archive(&self) -> bool {
        self.output.extension().is_some_and(|extension| extension == "tar")
    }
}

/// Build `plan` as an image. Progress goes to `progress`, one line per layer as it starts.
pub fn build(
    plan: &Plan,
    options: &BuildOptions,
    fetcher: &Fetcher,
    progress: &mut (dyn Write + Send),
) -> Result<Written, BuildError> {
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

    // Every archive first, one at a time: downloads are what the network is for, and a failed one stops the build
    // before any layer is written.
    let archives = plan
        .layers
        .iter()
        .map(|layer| {
            layer
                .packages
                .iter()
                .map(|package| fetcher.archive_for(package))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;

    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let (spill_limit, jobs) = budget(options.max_memory, cores);
    let total = plan.layers.len();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<ImageLayer, BuildError>>>> = Mutex::new((0..total).map(|_| None).collect());
    let progress = Mutex::new(progress);
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(total.max(1)) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(planned) = plan.layers.get(i) else {
                        break;
                    };
                    if let Ok(mut out) = progress.lock() {
                        let _ = writeln!(
                            out,
                            "[{}/{total}] {} ({})",
                            i + 1,
                            describe(planned),
                            size_text(planned.size)
                        );
                    }
                    let inputs: Vec<stream::Input<'_>> = planned
                        .packages
                        .iter()
                        .zip(&archives[i])
                        .map(|(package, archive)| stream::Input {
                            package,
                            archive: archive.clone(),
                        })
                        .collect();
                    let stream_options = stream::Options {
                        target: &target,
                        first: i == 0,
                        mtime: options.source_date_epoch,
                        spill_limit,
                        spill_dir: scratch.path(),
                    };
                    let layer = stream::write_layer(&inputs, &stream_options, &blobs)
                        .map(|blob| ImageLayer {
                            blob,
                            created_by: created_by(planned),
                        })
                        .map_err(BuildError::from);
                    let failed = layer.is_err();
                    if let Ok(mut results) = results.lock() {
                        results[i] = Some(layer);
                    }
                    if failed {
                        // Let the other workers stop after their current layer.
                        next.store(total, Ordering::SeqCst);
                        break;
                    }
                }
            });
        }
    });
    let mut layers = Vec::with_capacity(total);
    for result in results.into_inner().unwrap_or_default() {
        match result {
            Some(layer) => layers.push(layer?),
            // A layer that never started because an earlier one failed: that failure was returned above.
            None => break,
        }
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

/// A size such as `512M`, `1G`, `1.5GiB` or `4096`: binary units, so `M` is MiB. At least 16 MiB, since less would
/// only slow a build down without saving anything that matters.
pub fn parse_size(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    let split = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("`{value}` is not a size; use a number with K, M or G, such as 512M"))?;
    let multiplier: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        _ => return Err(format!("`{value}` has an unknown unit; use K, M or G")),
    };
    let bytes = (number * multiplier as f64) as u64;
    if bytes < 16 << 20 {
        return Err(format!("`{value}` is too small; give at least 16M"));
    }
    Ok(bytes)
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
    use crate::lock::Package;
    use std::io::Read;

    fn package(dir: &Path, name: &str, depends: &[&str]) -> crate::lock::Package {
        crate::testing::simple(dir, name, depends)
    }

    fn plan_of(packages: Vec<crate::lock::Package>, max_layers: usize) -> Plan {
        crate::testing::plan_of(packages, max_layers)
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
            // One layer at a time, so progress lines arrive in order.
            max_memory: 16 << 20,
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
        // Packages made in tests carry Unix modes and symlinks only when made on Unix; real ones always do.
        if cfg!(unix) {
            assert_eq!(*mode, 0o755);
            assert_eq!(bottom["opt/env/bin/base-link"].2.as_deref(), Some("base"));
        }
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
    fn an_empty_directory_entry_lands_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = crate::testing::Spec::new("dirs").file("share/dirs/README", "x", false, None);
        spec.empty_dirs.push("var/cache/dirs".to_string());
        let plan = plan_of(vec![crate::testing::package(dir.path(), spec)], 100);
        let layout = dir.path().join("layout");
        let fetcher = Fetcher::new(dir.path().join("cache"));
        build(&plan, &options(layout.clone()), &fetcher, &mut std::io::sink()).unwrap();
        let files = crate::testing::flatten(&crate::testing::layout_layers(&layout));
        assert_eq!(files["opt/env/var/cache/dirs"].0, "dir");
        assert_eq!(files["opt/env/var/cache/dirs"].1, 0o755);
        let record = String::from_utf8(files["opt/env/conda-meta/dirs-1.0-h0_0.json"].2.clone()).unwrap();
        assert!(record.contains("\"var/cache/dirs\""), "{record}");
    }

    #[test]
    fn a_package_cannot_place_files_outside_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = crate::testing::Spec::new("sneaky").file("bin/ok", "ok", true, None);
        spec.listed_only.push("../../etc/passwd".to_string());
        let plan = plan_of(vec![crate::testing::package(dir.path(), spec)], 100);
        let layout = dir.path().join("layout");
        let err = build(
            &plan,
            &options(layout),
            &Fetcher::new(dir.path().join("cache")),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "sneaky would install ../../etc/passwd, which is outside the environment's prefix"
        );
    }

    #[test]
    fn a_listed_file_missing_from_its_archive_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = crate::testing::Spec::new("partial").file("bin/ok", "ok", true, None);
        spec.listed_only.push("lib/gone.so".to_string());
        let plan = plan_of(vec![crate::testing::package(dir.path(), spec)], 100);
        let err = build(
            &plan,
            &options(dir.path().join("layout")),
            &Fetcher::new(dir.path().join("cache")),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "partial's paths.json lists lib/gone.so, but its archive has no such file"
        );
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
    fn the_budget_sets_the_spill_limit_and_the_parallel_layers() {
        const MIB: u64 = 1 << 20;
        assert_eq!(budget(512 * MIB, 8), (16 << 20, 8));
        assert_eq!(budget(512 * MIB, 2), (16 << 20, 2));
        assert_eq!(budget(128 * MIB, 8), (16 << 20, 2));
        assert_eq!(budget(64 * MIB, 8), (8 << 20, 2));
        assert_eq!(budget(16 * MIB, 8), (2 << 20, 1));
        assert_eq!(budget(16 * MIB, 0), (2 << 20, 1));
    }

    #[test]
    fn sizes_parse_in_binary_units() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1G").unwrap(), 1 << 30);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 << 29);
        assert_eq!(parse_size(" 64 MB ").unwrap(), 64 << 20);
        assert_eq!(parse_size("33554432").unwrap(), 32 << 20);
        assert!(parse_size("8M").unwrap_err().contains("too small"));
        assert!(parse_size("lots").unwrap_err().contains("not a size"));
        assert!(parse_size("5T").unwrap_err().contains("unknown unit"));
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
