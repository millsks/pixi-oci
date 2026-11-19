//! Images defined in a file instead of flags: `[tool.pixi-oci.images.NAME]` in the workspace's manifest, or
//! `[images.NAME]` in `pixi-oci.toml` beside the lockfile.
//!
//! `--config PATH` names the only file read. Otherwise pixi-oci reads the manifest's table (`pixi.toml`, else
//! `pyproject.toml`) and `pixi-oci.toml`; when both define images it refuses, rather than quietly prefer one. A flag
//! given on the command line wins over its key; lists (`env`, `labels`, `annotations`, `expose`, `add`) take the
//! file's entries first and the flags' after. Relative paths in a file are relative to its directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::{BuildArgs, CommandLine, LayerArgs, Selection};
use crate::plan::Layering;

/// The images a file defines, by name.
#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub images: BTreeMap<String, ImageSpec>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    images: BTreeMap<String, ImageSpec>,
}

#[derive(Debug, Deserialize)]
struct ManifestDoc {
    tool: Option<ToolTable>,
}

#[derive(Debug, Deserialize)]
struct ToolTable {
    #[serde(rename = "pixi-oci")]
    pixi_oci: Option<File>,
}

/// One image, every key a build option.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ImageSpec {
    lockfile: Option<PathBuf>,
    environment: Option<String>,
    platform: Option<String>,
    tag: Option<String>,
    output: Option<PathBuf>,
    force: Option<bool>,
    allow_unpinned: Option<bool>,
    prefix: Option<String>,
    env: Option<BTreeMap<String, String>>,
    entrypoint: Option<Words>,
    cmd: Option<Words>,
    workdir: Option<String>,
    task: Option<String>,
    add: Option<Vec<AddSpec>>,
    labels: Option<BTreeMap<String, String>>,
    annotations: Option<BTreeMap<String, String>>,
    user: Option<Scalar>,
    expose: Option<Vec<Scalar>>,
    stop_signal: Option<Scalar>,
    layers: Option<Layering>,
    max_layers: Option<usize>,
    max_memory: Option<Scalar>,
    cache_dir: Option<PathBuf>,
    base: Option<String>,
}

/// A command as a list of words or one line, as the flags take it.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Words {
    List(Vec<String>),
    Line(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum AddSpec {
    Pair(String),
    Table { src: PathBuf, dest: String },
}

/// A string or a number: a port, a signal, a user id, a size.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Scalar {
    Number(u64),
    Text(String),
}

impl Scalar {
    fn text(&self) -> String {
        match self {
            Scalar::Number(number) => number.to_string(),
            Scalar::Text(text) => text.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ConfigError {
    #[error("could not read {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{} is not a pixi-oci config: {message}", .path.display())]
    #[diagnostic(help("keys are the build options' names: see `pixi oci build --help` and the docs' Config files"))]
    Invalid { path: PathBuf, message: String },

    #[error("images are defined in both {} and {}", .manifest.display(), .file.display())]
    #[diagnostic(help("keep them in one, or name the one to read with --config"))]
    Both { manifest: PathBuf, file: PathBuf },

    #[error("no image `{name}`{}", .path.as_ref().map(|path| format!(" in {}", path.display())).unwrap_or_default())]
    #[diagnostic(help("{}", if .available.is_empty() { "no config file defines images here".to_string() } else { format!("its images: {}", .available.join(", ")) }))]
    NoImage {
        name: String,
        path: Option<PathBuf>,
        available: Vec<String>,
    },

    #[error("image `{image}` in {}: `{key}`: {message}", .path.display())]
    Value {
        path: PathBuf,
        image: String,
        key: String,
        message: String,
    },

    #[error("{} images would be built, so --{flag} would name them all", .count)]
    #[diagnostic(help("name one image to build, such as `pixi oci build {example}`"))]
    OneFlagForMany {
        count: usize,
        flag: String,
        example: String,
    },
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn invalid(path: &Path) -> impl Fn(toml::de::Error) -> ConfigError + '_ {
    move |err| ConfigError::Invalid {
        path: path.to_path_buf(),
        message: err.to_string().trim().replace('\n', " "),
    }
}

/// The config to build from: `--config`'s file, else what `dir` holds. `None` when no file defines images.
pub fn locate(config: Option<&Path>, dir: &Path) -> Result<Option<Config>, ConfigError> {
    if let Some(path) = config {
        let file: File = toml::from_str(&read(path)?).map_err(invalid(path))?;
        if file.images.is_empty() {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "it defines no images: give each one as [images.NAME]".to_string(),
            });
        }
        return Ok(Some(Config {
            path: path.to_path_buf(),
            images: file.images,
        }));
    }
    let manifest = [dir.join("pixi.toml"), dir.join("pyproject.toml")]
        .into_iter()
        .find(|path| path.is_file());
    let from_manifest = match &manifest {
        Some(path) => {
            let doc: ManifestDoc = toml::from_str(&read(path)?).map_err(invalid(path))?;
            doc.tool
                .and_then(|tool| tool.pixi_oci)
                .filter(|file| !file.images.is_empty())
                .map(|file| Config {
                    path: path.clone(),
                    images: file.images,
                })
        }
        None => None,
    };
    let fallback = dir.join("pixi-oci.toml");
    let from_file = if fallback.is_file() {
        let file: File = toml::from_str(&read(&fallback)?).map_err(invalid(&fallback))?;
        (!file.images.is_empty()).then(|| Config {
            path: fallback.clone(),
            images: file.images,
        })
    } else {
        None
    };
    match (from_manifest, from_file) {
        (Some(manifest), Some(file)) => Err(ConfigError::Both {
            manifest: manifest.path,
            file: file.path,
        }),
        (manifest, file) => Ok(manifest.or(file)),
    }
}

impl Config {
    /// The images to build: `name`, or every one.
    pub fn select(&self, name: Option<&str>) -> Result<Vec<(String, &ImageSpec)>, ConfigError> {
        match name {
            Some(name) => self
                .images
                .get(name)
                .map(|spec| vec![(name.to_string(), spec)])
                .ok_or_else(|| ConfigError::NoImage {
                    name: name.to_string(),
                    path: Some(self.path.clone()),
                    available: self.images.keys().cloned().collect(),
                }),
            None => Ok(self.images.iter().map(|(name, spec)| (name.clone(), spec)).collect()),
        }
    }

    fn dir(&self) -> PathBuf {
        match self.path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }
}

/// Whether the command line gave the option with this id: what was given wins over the file.
pub type Given<'a> = &'a dyn Fn(&str) -> bool;

/// `args` with `spec`'s keys for the options the command line did not give.
pub fn apply_build(
    config: &Config,
    name: &str,
    spec: &ImageSpec,
    mut args: BuildArgs,
    given: Given<'_>,
) -> Result<BuildArgs, ConfigError> {
    let dir = config.dir();
    let fail = |key: &str, message: String| ConfigError::Value {
        path: config.path.clone(),
        image: name.to_string(),
        key: key.to_string(),
        message,
    };
    let path = |relative: &Path| dir.join(relative);
    apply_selection(&mut args.selection, spec, &dir, given);
    apply_layers(&mut args.layers, spec, given);
    if !given("output") {
        args.output = path(spec.output.as_deref().unwrap_or(Path::new(&format!("{name}.tar"))));
    }
    if !given("force")
        && let Some(force) = spec.force
    {
        args.force = force;
    }
    if !given("tag") {
        args.tag = Some(spec.tag.clone().unwrap_or_else(|| name.to_string()));
    }
    if !given("prefix")
        && let Some(prefix) = &spec.prefix
    {
        args.prefix = prefix.clone();
    }
    let words = |key: &str, words: &Words| -> Result<CommandLine, ConfigError> {
        match words {
            Words::List(list) if list.is_empty() => Err(fail(key, "the command is empty".to_string())),
            Words::List(list) => Ok(CommandLine(list.clone())),
            Words::Line(line) => crate::build::parse_command(line)
                .map(CommandLine)
                .map_err(|message| fail(key, message)),
        }
    };
    if !given("entrypoint")
        && let Some(entrypoint) = &spec.entrypoint
    {
        args.entrypoint = Some(words("entrypoint", entrypoint)?);
    }
    if !given("cmd")
        && let Some(cmd) = &spec.cmd
    {
        args.cmd = Some(words("cmd", cmd)?);
    }
    if !given("workdir") && spec.workdir.is_some() {
        args.workdir = spec.workdir.clone();
    }
    if !given("task") && spec.task.is_some() {
        args.task = spec.task.clone();
    }
    if !given("user")
        && let Some(user) = &spec.user
    {
        args.user = Some(crate::build::parse_user(&user.text()).map_err(|message| fail("user", message))?);
    }
    if !given("stop_signal")
        && let Some(signal) = &spec.stop_signal
    {
        args.stop_signal =
            Some(crate::build::parse_signal(&signal.text()).map_err(|message| fail("stop-signal", message))?);
    }
    if !given("max_memory")
        && let Some(size) = &spec.max_memory
    {
        args.max_memory = crate::build::parse_size(&size.text()).map_err(|message| fail("max-memory", message))?;
    }
    if !given("cache_dir")
        && let Some(cache) = &spec.cache_dir
    {
        args.cache_dir = Some(path(cache));
    }
    if !given("base") && spec.base.is_some() {
        args.base = spec.base.clone();
    }
    if !given("allow_unpinned")
        && let Some(allow) = spec.allow_unpinned
    {
        args.allow_unpinned = allow;
    }

    // Lists: the file's first, then the flags', which win where a key repeats.
    let mut env = Vec::new();
    for (key, value) in spec.env.iter().flatten() {
        env.push(crate::build::parse_env(&format!("{key}={value}")).map_err(|message| fail("env", message))?);
    }
    env.append(&mut args.env);
    args.env = env;
    let pairs = |map: &Option<BTreeMap<String, String>>, key: &str| -> Result<Vec<(String, String)>, ConfigError> {
        map.iter()
            .flatten()
            .map(|(k, v)| crate::build::parse_key_value(&format!("{k}={v}")).map_err(|message| fail(key, message)))
            .collect()
    };
    let mut labels = pairs(&spec.labels, "labels")?;
    labels.append(&mut args.label);
    args.label = labels;
    let mut annotations = pairs(&spec.annotations, "annotations")?;
    annotations.append(&mut args.annotation);
    args.annotation = annotations;
    let mut expose = Vec::new();
    for port in spec.expose.iter().flatten() {
        expose.push(crate::build::parse_port(&port.text()).map_err(|message| fail("expose", message))?);
    }
    expose.append(&mut args.expose);
    args.expose = expose;
    let mut add = Vec::new();
    for item in spec.add.iter().flatten() {
        let mut parsed = match item {
            AddSpec::Pair(pair) => crate::add::parse_add(pair).map_err(|message| fail("add", message))?,
            AddSpec::Table { src, dest } => {
                crate::add::parse_add(&format!("{}:{dest}", src.display())).map_err(|message| fail("add", message))?
            }
        };
        parsed.source = path(&parsed.source);
        add.push(parsed);
    }
    add.append(&mut args.add);
    args.add = add;
    Ok(args)
}

/// `selection` and `layers` with `spec`'s keys for what the command line did not give, for `plan`.
pub fn apply_plan(
    config: &Config,
    spec: &ImageSpec,
    selection: &mut Selection,
    layers: &mut LayerArgs,
    given: Given<'_>,
) {
    let dir = config.dir();
    apply_selection(selection, spec, &dir, given);
    apply_layers(layers, spec, given);
}

fn apply_selection(selection: &mut Selection, spec: &ImageSpec, dir: &Path, given: Given<'_>) {
    if !given("lockfile") {
        selection.lockfile = dir.join(spec.lockfile.as_deref().unwrap_or(Path::new("pixi.lock")));
    }
    if !given("environment")
        && let Some(environment) = &spec.environment
    {
        selection.environment = environment.clone();
    }
    if !given("platform") && spec.platform.is_some() {
        selection.platform = spec.platform.clone();
    }
}

fn apply_layers(layers: &mut LayerArgs, spec: &ImageSpec, given: Given<'_>) {
    if !given("layers")
        && let Some(layering) = spec.layers
    {
        layers.layers = layering;
    }
    if !given("max_layers")
        && let Some(max) = spec.max_layers
    {
        layers.max_layers = max;
    }
}

/// Refuse a flag that names one image when several are built.
pub fn one_image_flags(count: usize, example: &str, given: Given<'_>) -> Result<(), ConfigError> {
    if count > 1 {
        for (id, flag) in [("output", "output"), ("tag", "tag")] {
            if given(id) {
                return Err(ConfigError::OneFlagForMany {
                    count,
                    flag: flag.to_string(),
                    example: example.to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The build arguments a command line gives, and which options it gave.
    fn parse(args: &[&str]) -> (BuildArgs, clap::ArgMatches) {
        let matches = crate::cli::Cli::command()
            .try_get_matches_from(["pixi-oci", "build"].iter().chain(args))
            .unwrap();
        let cli = crate::cli::Cli::from_arg_matches(&matches).unwrap();
        let crate::cli::Command::Build(build) = cli.command else {
            unreachable!()
        };
        let sub = matches.subcommand_matches("build").unwrap().clone();
        (*build, sub)
    }

    fn given(matches: &clap::ArgMatches) -> impl Fn(&str) -> bool + '_ {
        |id| crate::cli::given(matches, id)
    }

    const MANIFEST: &str = r#"
[workspace]
name = "api"

[tool.pixi-oci.images.api]
environment = "prod"
platform = "linux-aarch64"
base = "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc"
tag = "ghcr.io/team/api:1.4.0"
entrypoint = ["uvicorn", "main:app"]
cmd = "--host 0.0.0.0"
env = { LOG_LEVEL = "info" }
labels = { "org.opencontainers.image.source" = "https://github.com/team/api" }
expose = [8000, "9000/udp"]
user = 1000
stop-signal = "TERM"
layers = "grouped"
max-layers = 50
max-memory = "1G"
add = ["app:/app", { src = "config/api.toml", dest = "/etc/api/" }]

[tool.pixi-oci.images.worker]
task = "work"
"#;

    #[test]
    fn the_manifests_table_is_the_default_and_pixi_oci_toml_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        assert!(locate(None, dir.path()).unwrap().is_none());
        write(dir.path(), "pixi.toml", "[workspace]\nname = \"x\"\n");
        assert!(locate(None, dir.path()).unwrap().is_none());
        let fallback = write(dir.path(), "pixi-oci.toml", "[images.job]\ntask = \"run\"\n");
        let config = locate(None, dir.path()).unwrap().unwrap();
        assert_eq!(config.path, fallback);
        assert_eq!(config.images.keys().collect::<Vec<_>>(), ["job"]);
        // Both defining images is refused, naming both.
        let manifest = write(dir.path(), "pixi.toml", MANIFEST);
        match locate(None, dir.path()).unwrap_err() {
            ConfigError::Both { manifest: m, file } => assert_eq!((m, file), (manifest.clone(), fallback.clone())),
            other => panic!("{other}"),
        }
        // --config is the only file read.
        let only = write(dir.path(), "other.toml", "[images.solo]\n");
        let config = locate(Some(&only), dir.path()).unwrap().unwrap();
        assert_eq!(config.images.keys().collect::<Vec<_>>(), ["solo"]);
        std::fs::remove_file(&fallback).unwrap();
        let config = locate(None, dir.path()).unwrap().unwrap();
        assert_eq!(config.path, manifest);
        assert_eq!(config.images.keys().collect::<Vec<_>>(), ["api", "worker"]);
    }

    #[test]
    fn pyproject_holds_it_under_tool() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"api\"\n\n[tool.pixi.workspace]\nplatforms = [\"linux-64\"]\n\n[tool.pixi-oci.images.api]\ntask = \"serve\"\n",
        );
        let config = locate(None, dir.path()).unwrap().unwrap();
        assert!(config.path.ends_with("pyproject.toml"));
        assert_eq!(config.images["api"].task.as_deref(), Some("serve"));
    }

    #[test]
    fn unknown_keys_and_wrong_values_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pixi-oci.toml", "[images.api]\nentrypont = [\"x\"]\n");
        let err = locate(None, dir.path()).unwrap_err().to_string();
        assert!(err.contains("pixi-oci.toml") && err.contains("entrypont"), "{err}");
        write(dir.path(), "pixi-oci.toml", "[images.api]\nmax-layers = \"many\"\n");
        assert!(matches!(locate(None, dir.path()), Err(ConfigError::Invalid { .. })));
        write(dir.path(), "pixi-oci.toml", "[image.api]\n");
        assert!(matches!(locate(None, dir.path()), Err(ConfigError::Invalid { .. })));

        write(dir.path(), "pixi-oci.toml", "[images.api]\nexpose = [\"http\"]\n");
        let config = locate(None, dir.path()).unwrap().unwrap();
        let (args, matches) = parse(&[]);
        let err = apply_build(&config, "api", &config.images["api"], args, &given(&matches)).unwrap_err();
        assert!(err.to_string().starts_with("image `api` in"), "{err}");
        assert!(err.to_string().contains("`expose`"), "{err}");
        assert!(matches!(
            config.select(Some("web")),
            Err(ConfigError::NoImage { available, .. }) if available == ["api"]
        ));
    }

    #[test]
    fn allow_unpinned_is_a_key_too() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pixi-oci.toml",
            "[images.api]\nbase = \"debian:bookworm-slim\"\nallow-unpinned = true\n",
        );
        let config = locate(None, dir.path()).unwrap().unwrap();
        let (args, matches) = parse(&[]);
        let args = apply_build(&config, "api", &config.images["api"], args, &given(&matches)).unwrap();
        assert!(args.allow_unpinned);
        assert_eq!(args.base.as_deref(), Some("debian:bookworm-slim"));
    }

    #[test]
    fn keys_become_options_and_flags_win() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pixi.toml", MANIFEST);
        let config = locate(None, dir.path()).unwrap().unwrap();
        let spec = &config.images["api"];
        let (args, matches) = parse(&[]);
        let args = apply_build(&config, "api", spec, args, &given(&matches)).unwrap();
        assert_eq!(args.selection.lockfile, dir.path().join("pixi.lock"));
        assert_eq!(args.selection.environment, "prod");
        assert_eq!(args.selection.platform.as_deref(), Some("linux-aarch64"));
        assert_eq!(args.output, dir.path().join("api.tar"));
        assert_eq!(args.tag.as_deref(), Some("ghcr.io/team/api:1.4.0"));
        assert_eq!(
            args.entrypoint,
            Some(CommandLine(vec!["uvicorn".into(), "main:app".into()]))
        );
        assert_eq!(args.cmd, Some(CommandLine(vec!["--host".into(), "0.0.0.0".into()])));
        assert_eq!(args.env, ["LOG_LEVEL=info"]);
        assert_eq!(args.expose, ["8000/tcp", "9000/udp"]);
        assert_eq!(args.user.as_deref(), Some("1000"));
        assert_eq!(args.stop_signal.as_deref(), Some("SIGTERM"));
        assert_eq!(args.layers.layers, Layering::Grouped);
        assert_eq!(args.layers.max_layers, 50);
        assert_eq!(args.max_memory, 1 << 30);
        assert_eq!(args.add.len(), 2);
        assert_eq!(args.add[0].source, dir.path().join("app"));
        assert_eq!(args.add[1].dest, "/etc/api/");

        // Flags win over keys; lists put the flags' after the file's.
        let (args, matches) = parse(&[
            "--layers",
            "single",
            "-e",
            "default",
            "--env",
            "LOG_LEVEL=debug",
            "--expose",
            "7000",
            "-o",
            "elsewhere.tar",
        ]);
        let args = apply_build(&config, "api", spec, args, &given(&matches)).unwrap();
        assert_eq!(args.layers.layers, Layering::Single);
        assert_eq!(args.layers.max_layers, 50);
        assert_eq!(args.selection.environment, "default");
        assert_eq!(args.env, ["LOG_LEVEL=info", "LOG_LEVEL=debug"]);
        assert_eq!(args.expose, ["8000/tcp", "9000/udp", "7000/tcp"]);
        assert_eq!(args.output, PathBuf::from("elsewhere.tar"));

        // An image with few keys takes its name as its tag and output.
        let (args, matches) = parse(&[]);
        let worker = apply_build(&config, "worker", &config.images["worker"], args, &given(&matches)).unwrap();
        assert_eq!(worker.tag.as_deref(), Some("worker"));
        assert_eq!(worker.output, dir.path().join("worker.tar"));
        assert_eq!(worker.task.as_deref(), Some("work"));
        assert_eq!(worker.selection.environment, "default");

        let (_, matches) = parse(&["--tag", "x"]);
        assert!(one_image_flags(2, "api", &given(&matches)).is_err());
        assert!(one_image_flags(1, "api", &given(&matches)).is_ok());
    }
}
