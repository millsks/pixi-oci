//! The workspace's pixi manifest beside the lockfile, read for what `pixi run` would do in an environment: its
//! activation variables and its tasks.
//!
//! The manifest is `pixi.toml`, else `pyproject.toml` with a `[tool.pixi]` table, as pixi picks it. An environment's
//! features are the default feature (unless `no-default-feature`) and then the ones it lists, in order; within a
//! feature, `[target.<selector>]` tables for the platform (`unix`, `linux`, then the platform itself) come after the
//! feature's own, and later ones win.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

/// A pixi manifest, as the table pixi reads: the whole of `pixi.toml`, or `[tool.pixi]` of `pyproject.toml`.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub path: PathBuf,
    table: Table,
}

/// A task, as the image runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    pub command: Vec<String>,
    /// `cwd`, as written: relative to the workspace's root.
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ManifestError {
    #[error("could not read {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{} is not valid TOML: {message}", .path.display())]
    Parse { path: PathBuf, message: String },

    #[error("--task needs the workspace's manifest, and there is no pixi.toml or pyproject.toml with [tool.pixi] in {}", .dir.display())]
    #[diagnostic(help("point --lockfile at the pixi.lock beside the manifest"))]
    NoManifest { dir: PathBuf },

    #[error("{} has no environment `{name}`", .path.display())]
    NoEnvironment { path: PathBuf, name: String },

    #[error("environment `{environment}` has no task `{name}`")]
    #[diagnostic(help("its tasks: {}", if .available.is_empty() { "none".to_string() } else { .available.join(", ") }))]
    NoTask {
        environment: String,
        name: String,
        available: Vec<String>,
    },

    #[error("task `{name}` runs nothing of its own")]
    #[diagnostic(help("an image starts one command: give a task with a `cmd`"))]
    NoCommand { name: String },

    #[error("task `{name}` depends on {}", .depends.join(", "))]
    #[diagnostic(help(
        "nothing runs before an image's entrypoint: run those tasks' work when building the environment, or give \
         a task that stands alone"
    ))]
    DependsOn { name: String, depends: Vec<String> },

    #[error("task `{name}`'s argument `{arg}` has no default")]
    #[diagnostic(help("give the argument a default in the task, or use --entrypoint and --cmd"))]
    ArgWithoutDefault { name: String, arg: String },

    #[error("task `{name}` in {} is not a task: {message}", .path.display())]
    Malformed {
        path: PathBuf,
        name: String,
        message: String,
    },
}

/// The manifest in `dir`, if there is one.
pub fn find(dir: &Path) -> Result<Option<Manifest>, ManifestError> {
    let read = |path: PathBuf| -> Result<Table, ManifestError> {
        let text = std::fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        text.parse::<Table>().map_err(|err| ManifestError::Parse {
            path: path.clone(),
            message: err.to_string(),
        })
    };
    let pixi = dir.join("pixi.toml");
    if pixi.is_file() {
        let table = read(pixi.clone())?;
        return Ok(Some(Manifest { path: pixi, table }));
    }
    let pyproject = dir.join("pyproject.toml");
    if pyproject.is_file() {
        let mut table = read(pyproject.clone())?;
        if let Some(Value::Table(tool)) = table.remove("tool")
            && let Some(Value::Table(pixi)) = tool.get("pixi").cloned()
        {
            return Ok(Some(Manifest {
                path: pyproject,
                table: pixi,
            }));
        }
    }
    Ok(None)
}

impl Manifest {
    /// The tables of `environment`'s features in order, each followed by its targets for `platform`.
    fn sections(&self, environment: &str, platform: &str) -> Result<Vec<&Table>, ManifestError> {
        let mut features: Vec<Option<&str>> = Vec::new();
        let spec = self.table.get("environments").and_then(|envs| envs.get(environment));
        if spec.is_none() && environment == "default" {
            features.push(None);
        } else {
            let Some(spec) = spec else {
                return Err(ManifestError::NoEnvironment {
                    path: self.path.clone(),
                    name: environment.to_string(),
                });
            };
            let (listed, no_default): (&[Value], bool) = match spec {
                Value::Array(names) => (names.as_slice(), false),
                Value::Table(table) => (
                    table
                        .get("features")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    table
                        .get("no-default-feature")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
                _ => (&[][..], false),
            };
            if !no_default {
                features.push(None);
            }
            features.extend(listed.iter().filter_map(Value::as_str).map(Some));
        }
        let selectors = ["unix", "linux", platform];
        let mut sections = Vec::new();
        for feature in features {
            let table = match feature {
                None => Some(&self.table),
                Some(name) => self
                    .table
                    .get("feature")
                    .and_then(|features| features.get(name))
                    .and_then(Value::as_table),
            };
            let Some(table) = table else { continue };
            sections.push(table);
            if let Some(targets) = table.get("target").and_then(Value::as_table) {
                sections.extend(
                    selectors
                        .iter()
                        .filter_map(|selector| targets.get(*selector).and_then(Value::as_table)),
                );
            }
        }
        Ok(sections)
    }

    /// The environment's static `[activation.env]` variables, sorted by name, later features winning;
    /// `$CONDA_PREFIX` and `${CONDA_PREFIX}` in a value become `prefix`. Also whether it has activation scripts,
    /// which are not run.
    pub fn activation(
        &self,
        environment: &str,
        platform: &str,
        prefix: &str,
    ) -> Result<(Vec<(String, String)>, bool), ManifestError> {
        let mut env = BTreeMap::new();
        let mut scripts = false;
        for section in self.sections(environment, platform)? {
            let Some(activation) = section.get("activation").and_then(Value::as_table) else {
                continue;
            };
            if let Some(vars) = activation.get("env").and_then(Value::as_table) {
                for (key, value) in vars {
                    if let Some(value) = value.as_str() {
                        env.insert(key.clone(), expand_prefix(value, prefix));
                    }
                }
            }
            scripts |= activation
                .get("scripts")
                .and_then(Value::as_array)
                .is_some_and(|list| !list.is_empty());
        }
        Ok((env.into_iter().collect(), scripts))
    }

    /// The task `name` of `environment`, as the image would start it.
    pub fn task(&self, environment: &str, platform: &str, name: &str) -> Result<Task, ManifestError> {
        let sections = self.sections(environment, platform)?;
        let found = sections
            .iter()
            .rev()
            .find_map(|section| section.get("tasks").and_then(|tasks| tasks.get(name)));
        let Some(task) = found else {
            let mut available: Vec<String> = sections
                .iter()
                .filter_map(|section| section.get("tasks").and_then(Value::as_table))
                .flat_map(|tasks| tasks.keys().cloned())
                .collect();
            available.sort();
            available.dedup();
            return Err(ManifestError::NoTask {
                environment: environment.to_string(),
                name: name.to_string(),
                available,
            });
        };
        let malformed = |message: &str| ManifestError::Malformed {
            path: self.path.clone(),
            name: name.to_string(),
            message: message.to_string(),
        };
        let (cmd, table) = match task {
            Value::String(_) | Value::Array(_) => (Some(task), None),
            Value::Table(table) => (table.get("cmd"), Some(table)),
            _ => return Err(malformed("expected a command, a list of words, or a table")),
        };
        if let Some(table) = table {
            let depends = table.get("depends-on").or_else(|| table.get("depends_on"));
            let depends: Vec<String> = match depends {
                Some(Value::String(one)) => vec![one.clone()],
                Some(Value::Array(list)) => list
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(name) => Some(name.clone()),
                        Value::Table(spec) => spec.get("task").and_then(Value::as_str).map(str::to_string),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if !depends.is_empty() {
                return Err(ManifestError::DependsOn {
                    name: name.to_string(),
                    depends,
                });
            }
        }
        let defaults = table.map(|table| args(table, name)).transpose()?.unwrap_or_default();
        let command = match cmd {
            None => {
                return Err(ManifestError::NoCommand { name: name.to_string() });
            }
            Some(Value::Array(words)) => words
                .iter()
                .map(|word| {
                    word.as_str()
                        .map(|word| render(word, &defaults))
                        .ok_or_else(|| malformed("every word of `cmd` must be a string"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(Value::String(line)) => command_line(&render(line, &defaults)),
            Some(_) => return Err(malformed("`cmd` must be a string or a list of strings")),
        };
        if command.is_empty() {
            return Err(ManifestError::NoCommand { name: name.to_string() });
        }
        let cwd = table
            .and_then(|table| table.get("cwd"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let env = table
            .and_then(|table| table.get("env"))
            .and_then(Value::as_table)
            .map(|vars| {
                vars.iter()
                    .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Task {
            name: name.to_string(),
            command,
            cwd,
            env,
        })
    }
}

/// A task's arguments and their defaults; one without a default cannot be filled in.
fn args(table: &Table, name: &str) -> Result<BTreeMap<String, String>, ManifestError> {
    let mut defaults = BTreeMap::new();
    for arg in table.get("args").and_then(Value::as_array).into_iter().flatten() {
        let (arg, default) = match arg {
            Value::String(arg) => (arg.clone(), None),
            Value::Table(spec) => (
                spec.get("arg").and_then(Value::as_str).unwrap_or_default().to_string(),
                spec.get("default").and_then(Value::as_str).map(str::to_string),
            ),
            _ => continue,
        };
        let Some(default) = default else {
            return Err(ManifestError::ArgWithoutDefault {
                name: name.to_string(),
                arg,
            });
        };
        defaults.insert(arg, default);
    }
    Ok(defaults)
}

/// `{{ name }}` replaced by the argument's default, as pixi fills a task's arguments.
fn render(text: &str, defaults: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (arg, default) in defaults {
        for form in [format!("{{{{ {arg} }}}}"), format!("{{{{{arg}}}}}")] {
            out = out.replace(&form, default);
        }
    }
    out
}

/// A task's command line as the image runs it: plain words split on whitespace, anything with shell syntax through
/// `/bin/sh -c`.
pub fn command_line(line: &str) -> Vec<String> {
    let line = line.trim();
    let shell = line.chars().any(|c| "&|;<>()$`*?[]{}~\"'\\#\n".contains(c))
        || line.split_whitespace().next().is_some_and(|first| first.contains('='));
    if shell {
        vec!["/bin/sh".to_string(), "-c".to_string(), line.to_string()]
    } else {
        line.split_whitespace().map(str::to_string).collect()
    }
}

fn expand_prefix(value: &str, prefix: &str) -> String {
    value
        .replace("${CONDA_PREFIX}", prefix)
        .replace("$CONDA_PREFIX", prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, text: &str) -> (tempfile::TempDir, Manifest) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), text).unwrap();
        let manifest = find(dir.path()).unwrap().unwrap();
        (dir, manifest)
    }

    const PIXI: &str = r#"
[workspace]
name = "app"
channels = ["conda-forge"]
platforms = ["linux-64", "linux-aarch64"]

[activation.env]
LOG_LEVEL = "info"
DATA = "$CONDA_PREFIX/share/app"

[target.linux.activation.env]
LOG_LEVEL = "warning"

[tasks]
serve = { cmd = "uvicorn main:app --host 0.0.0.0", cwd = "app", env = { WORKERS = "2" } }
shell = "echo $HOME && ls"
words = ["python", "-m", "http.server"]
greet = { cmd = "echo hello {{ who }}", args = [{ arg = "who", default = "world" }] }
needs = { cmd = "echo", args = ["who"] }
both = { cmd = "echo both", depends-on = ["serve"] }
alias = { depends-on = ["serve", "shell"] }

[target.linux-64.tasks]
serve = "uvicorn main:app --host 0.0.0.0 --port 9000"

[feature.prod.activation]
env = { LOG_LEVEL = "error" }
scripts = ["setup.sh"]

[feature.prod.tasks]
migrate = "alembic upgrade head"

[environments]
prod = ["prod"]
bare = { features = ["prod"], no-default-feature = true }
"#;

    #[test]
    fn activation_variables_follow_features_and_targets() {
        let (_dir, manifest) = manifest("pixi.toml", PIXI);
        let (env, scripts) = manifest.activation("default", "linux-64", "/opt/env").unwrap();
        assert_eq!(
            env,
            [
                ("DATA".to_string(), "/opt/env/share/app".to_string()),
                ("LOG_LEVEL".to_string(), "warning".to_string())
            ]
        );
        assert!(!scripts);
        let (env, scripts) = manifest.activation("prod", "linux-64", "/opt/env").unwrap();
        assert_eq!(env[1], ("LOG_LEVEL".to_string(), "error".to_string()));
        assert!(scripts);
        let (env, _) = manifest.activation("bare", "linux-64", "/opt/env").unwrap();
        assert_eq!(env, [("LOG_LEVEL".to_string(), "error".to_string())]);
        assert!(matches!(
            manifest.activation("missing", "linux-64", "/opt/env"),
            Err(ManifestError::NoEnvironment { .. })
        ));
    }

    #[test]
    fn a_task_becomes_a_command() {
        let (_dir, manifest) = manifest("pixi.toml", PIXI);
        fn words(task: &Task) -> Vec<&str> {
            task.command.iter().map(String::as_str).collect()
        }
        // The platform's own definition wins over the feature's.
        let serve = manifest.task("default", "linux-64", "serve").unwrap();
        assert_eq!(
            words(&serve),
            ["uvicorn", "main:app", "--host", "0.0.0.0", "--port", "9000"]
        );
        let serve = manifest.task("default", "linux-aarch64", "serve").unwrap();
        assert_eq!(words(&serve), ["uvicorn", "main:app", "--host", "0.0.0.0"]);
        assert_eq!(serve.cwd.as_deref(), Some("app"));
        assert_eq!(serve.env, [("WORKERS".to_string(), "2".to_string())]);
        assert_eq!(
            words(&manifest.task("default", "linux-64", "shell").unwrap()),
            ["/bin/sh", "-c", "echo $HOME && ls"]
        );
        assert_eq!(
            words(&manifest.task("default", "linux-64", "words").unwrap()),
            ["python", "-m", "http.server"]
        );
        assert_eq!(
            words(&manifest.task("default", "linux-64", "greet").unwrap()),
            ["echo", "hello", "world"]
        );
        assert_eq!(
            words(&manifest.task("prod", "linux-64", "migrate").unwrap()),
            ["alembic", "upgrade", "head"]
        );
    }

    #[test]
    fn a_task_the_image_cannot_start_is_refused() {
        let (_dir, manifest) = manifest("pixi.toml", PIXI);
        let err = |name: &str, environment: &str| manifest.task(environment, "linux-64", name).unwrap_err();
        assert!(matches!(err("both", "default"), ManifestError::DependsOn { .. }));
        assert!(matches!(err("alias", "default"), ManifestError::DependsOn { .. }));
        assert!(matches!(
            err("needs", "default"),
            ManifestError::ArgWithoutDefault { .. }
        ));
        match err("migrate", "default") {
            ManifestError::NoTask { available, .. } => assert!(available.contains(&"serve".to_string())),
            other => panic!("{other}"),
        }
        // The prod-only feature's task is not the default environment's; bare has no default feature's tasks.
        assert!(matches!(err("serve", "bare"), ManifestError::NoTask { .. }));
    }

    #[test]
    fn a_pyproject_workspace_is_read_from_tool_pixi() {
        let (_dir, manifest) = manifest(
            "pyproject.toml",
            r#"
[project]
name = "app"

[tool.pixi.workspace]
channels = ["conda-forge"]
platforms = ["linux-64"]

[tool.pixi.tasks]
serve = "python -m app"
"#,
        );
        assert!(manifest.path.ends_with("pyproject.toml"));
        let task = manifest.task("default", "linux-64", "serve").unwrap();
        assert_eq!(task.command, ["python", "-m", "app"]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        assert!(find(dir.path()).unwrap().is_none());
        std::fs::write(dir.path().join("pixi.toml"), "not = [toml").unwrap();
        assert!(matches!(find(dir.path()), Err(ManifestError::Parse { .. })));
    }

    #[test]
    fn shell_syntax_goes_through_sh() {
        for plain in ["python -m app", "gunicorn --bind 0.0.0.0:8000 app:create_app"] {
            assert_ne!(command_line(plain)[0], "/bin/sh", "{plain}");
        }
        for shell in [
            "a && b",
            "a | b",
            "a; b",
            "a > out",
            "echo $X",
            "ls *.py",
            "echo 'quoted'",
            "FOO=1 run",
            "gunicorn 'app:create_app()'",
        ] {
            assert_eq!(command_line(shell)[..2], ["/bin/sh", "-c"], "{shell}");
        }
    }
}
