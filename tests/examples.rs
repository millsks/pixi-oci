//! The examples under `examples/projects/pixi`. Every one is complete and plans for both platforms it is locked
//! for, offline, on every run. With `PIXI_OCI_NETWORK_TESTS=1`, each lockfile is checked against its manifest with
//! `pixi lock --check`, and on Linux a few examples are built on a base image, validated and checked.

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
    for (name, entrypoint) in [
        ("01-hello-python", "python"),
        ("05-native-binary", "rg"),
        ("13-pypi-mix", "ruff"),
    ] {
        let lockfile = Path::new("examples/projects/pixi").join(name).join("pixi.lock");
        let image = out.path().join(format!("{name}.tar"));
        let built = pixi_oci(&[
            "build",
            "--lockfile",
            lockfile.to_str().unwrap(),
            "-p",
            platform,
            "--base",
            DEBIAN,
            "--entrypoint",
            entrypoint,
            "-o",
            image.to_str().unwrap(),
        ]);
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
        assert!(checked.status.success(), "{name}: {report:#}");
        assert_eq!(report["problems"], serde_json::json!([]), "{name}");
    }
}
