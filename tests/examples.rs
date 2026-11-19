//! The examples under `examples/projects/pixi`. Every one is complete and plans for both platforms it is locked
//! for, offline, on every run. With `PIXI_OCI_NETWORK_TESTS=1`, each lockfile is checked against its manifest with
//! `pixi lock --check`, and on Linux a few examples are built on a base image, validated and checked.
//!
//! The examples that carry an application name its files with `--add` in their README; those files are collected
//! as a build would collect them, and what their ignore files leave out is checked here.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

const PLATFORMS: [&str; 2] = ["linux-64", "linux-aarch64"];
/// Pinned as in the Base images docs and `tests/network.rs`.
const DEBIAN: &str = "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc";

fn examples() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir("examples/projects/pixi")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn pixi_oci(args: &[&str]) -> std::process::Output {
    Command::cargo_bin("pixi-oci").unwrap().args(args).output().unwrap()
}

/// The `--add SRC:DEST` values in an example's README, as written there (paths relative to the repository).
fn readme_adds(dir: &Path) -> Vec<String> {
    let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
    let words: Vec<&str> = readme.split_whitespace().collect();
    words
        .windows(2)
        .filter(|pair| pair[0] == "--add")
        .map(|pair| pair[1].to_string())
        .collect()
}

/// What the README's `--add`s put in the image, collected as `build` collects them.
fn application(dir: &Path) -> pixi_oci::add::App {
    let adds: Vec<pixi_oci::add::Add> = readme_adds(dir)
        .iter()
        .map(|add| pixi_oci::add::parse_add(add).unwrap())
        .collect();
    pixi_oci::add::collect(&adds, Some(dir)).unwrap()
}

fn network() -> bool {
    std::env::var("PIXI_OCI_NETWORK_TESTS").as_deref() == Ok("1")
}

#[test]
fn every_example_is_complete_and_plans_for_both_platforms() {
    let examples = examples();
    assert!(examples.len() >= 14, "{examples:?}");
    for (i, dir) in examples.iter().enumerate() {
        let name = dir.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(&format!("{:02}-", i + 1)), "{name} is out of sequence");
        for file in ["pixi.toml", "pixi.lock", "README.md"] {
            assert!(dir.join(file).is_file(), "{name} has no {file}");
        }
        let lockfile = dir.join("pixi.lock");
        for platform in PLATFORMS {
            let output = pixi_oci(&[
                "plan",
                "--lockfile",
                lockfile.to_str().unwrap(),
                "-p",
                platform,
                "--format",
                "json",
            ]);
            assert!(
                output.status.success(),
                "{name} {platform}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let layers = plan["layers"].as_array().unwrap();
            assert!(
                !layers.is_empty() && layers.len() <= 100,
                "{name} {platform}: {} layers",
                layers.len()
            );
        }
    }
}

/// The examples with an application of their own, and paths their image must hold or must not.
const APPLICATIONS: [(&str, &[&str], &[&str]); 8] = [
    ("01-hello-python", &["app/hello.py"], &[]),
    (
        "03-flask",
        &[
            "srv/app/__init__.py",
            "srv/app/templates/index.html",
            "srv/app/static/style.css",
        ],
        &[],
    ),
    ("04-fastapi", &["app/main.py"], &[]),
    ("05-native-binary", &["usr/local/bin/search"], &[]),
    ("06-r-analysis", &["app/analysis.R", "data/sales.csv"], &[]),
    (
        "08-data-analysis",
        &[
            "app/report/__main__.py",
            "app/report/summarize.py",
            "etc/report/report.toml",
            "data/sales.csv",
        ],
        &[
            "app/report/notebooks",
            "app/report/notebooks/explore.ipynb",
            "data/raw",
            "data/raw/README.md",
        ],
    ),
    (
        "09-streamlit-dashboard",
        &["app/dashboard.py", "app/.streamlit/config.toml"],
        &[],
    ),
    (
        "11-machine-learning",
        &[
            "app/train.py",
            "app/predict.py",
            "app/data/flowers.csv",
            "app/data/README.md",
        ],
        &[],
    ),
];

#[test]
fn every_add_in_a_readme_names_files_that_exist() {
    for dir in examples() {
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let adds = readme_adds(&dir);
        let has_app = APPLICATIONS.iter().any(|(example, _, _)| *example == name);
        assert_eq!(!adds.is_empty(), has_app, "{name}: {adds:?}");
        for add in adds {
            let parsed = pixi_oci::add::parse_add(&add).unwrap_or_else(|err| panic!("{name}: {add}: {err}"));
            assert!(
                parsed.source.exists(),
                "{name}: {} does not exist",
                parsed.source.display()
            );
        }
    }
}

#[test]
fn applications_hold_their_files_and_leave_out_what_is_ignored() {
    for (name, present, absent) in APPLICATIONS {
        let app = application(&Path::new("examples/projects/pixi").join(name));
        for path in present {
            assert!(
                app.items.contains_key(*path),
                "{name}: no {path} in {:?}",
                app.items.keys()
            );
        }
        for path in absent {
            assert!(!app.items.contains_key(*path), "{name}: {path} should be left out");
        }
        assert!(
            !app.items
                .keys()
                .any(|path| path.contains("__pycache__") || path.ends_with(".pyc")),
            "{name}"
        );
    }
    let search = &application(Path::new("examples/projects/pixi/05-native-binary")).items["usr/local/bin/search"];
    assert!(matches!(search, pixi_oci::add::Item::File { executable: true, .. }));
}

#[test]
fn every_lockfile_matches_its_manifest() {
    if !network() || Command::new("pixi").arg("--version").output().is_err() {
        return;
    }
    for dir in examples() {
        let status = Command::new("pixi")
            .args(["lock", "--check", "--manifest-path"])
            .arg(dir.join("pixi.toml"))
            .status()
            .unwrap();
        assert!(
            status.success(),
            "{} is out of date: `pixi run examples-lock {}`",
            dir.display(),
            dir.file_name().unwrap().to_string_lossy()
        );
    }
}

/// Small examples of each kind, built on Debian for this machine's platform: `check` reports nothing.
#[test]
fn small_examples_build_and_check() {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => return,
    };
    if !network() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    // A simple and a larger application among them, built with their README's --adds. Clean ones report nothing;
    // 11's environment has scipy files its README names, so there only the application must be clean.
    for (name, entrypoint, clean) in [
        ("01-hello-python", "python", true),
        ("05-native-binary", "rg", true),
        ("11-machine-learning", "python", false),
        ("13-pypi-mix", "ruff", true),
    ] {
        let dir = Path::new("examples/projects/pixi").join(name);
        let lockfile = dir.join("pixi.lock");
        let image = out.path().join(format!("{name}.tar"));
        let mut args = vec![
            "build".to_string(),
            "--lockfile".to_string(),
            lockfile.to_string_lossy().into_owned(),
            "-p".to_string(),
            platform.to_string(),
            "--base".to_string(),
            DEBIAN.to_string(),
            "--entrypoint".to_string(),
            entrypoint.to_string(),
            "-o".to_string(),
            image.to_string_lossy().into_owned(),
        ];
        for add in readme_adds(&dir) {
            args.extend(["--add".to_string(), add]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let built = pixi_oci(&args);
        assert!(
            built.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        assert!(
            pixi_oci(&["validate", image.to_str().unwrap()]).status.success(),
            "{name}"
        );
        let checked = pixi_oci(&["check", image.to_str().unwrap(), "--format", "json"]);
        let report: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
        if clean {
            assert!(checked.status.success(), "{name}: {report:#}");
            assert_eq!(report["problems"], serde_json::json!([]), "{name}");
        }
        for problem in report["problems"].as_array().unwrap() {
            for path in problem["paths"].as_array().into_iter().flatten() {
                assert!(
                    path.as_str().unwrap().starts_with("/opt/env/"),
                    "{name}: a problem outside the environment: {problem:#}"
                );
            }
        }
    }
}
