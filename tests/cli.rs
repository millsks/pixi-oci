//! `pixi-oci` as pixi runs it, against a lockfile pixi wrote (tests/fixtures/workspace).

use assert_cmd::Command;
use predicates::prelude::*;

const LOCKFILE: &str = "tests/fixtures/workspace/pixi.lock";

fn pixi_oci() -> Command {
    Command::cargo_bin("pixi-oci").unwrap()
}

fn plan_json(args: &[&str]) -> serde_json::Value {
    let output = pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--format", "json"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn layer_names(plan: &serde_json::Value) -> Vec<Vec<String>> {
    plan["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| {
            layer["packages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|package| package["name"].as_str().unwrap().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn help_and_version_name_the_pixi_subcommand() {
    pixi_oci()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: pixi oci"));
    pixi_oci()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("pixi-oci "));
}

#[test]
fn the_text_plan_lists_one_layer_per_package() {
    pixi_oci()
        .args([
            "plan",
            "--lockfile",
            LOCKFILE,
            "--platform",
            "linux-64",
            "--layers",
            "per-package",
        ])
        .assert()
        .success()
        .stdout(predicate::str::starts_with(
            "Environment default on linux-64 (image platform linux/amd64)\n",
        ))
        .stdout(predicate::str::contains(
            "1 PyPI package, 23 layers, download size unknown",
        ))
        .stdout(predicate::str::contains("  #  Layer"))
        .stdout(predicate::str::contains("python 3.13"));
}

#[test]
fn dependencies_sit_below_what_depends_on_them() {
    let plan = plan_json(&["--platform", "linux-64", "--layers", "per-package"]);
    let layers = layer_names(&plan);
    let position = |name: &str| layers.iter().position(|layer| layer.iter().any(|n| n == name)).unwrap();
    assert!(position("libgcc") < position("openssl"));
    assert!(position("openssl") < position("python"));
    assert!(position("libzlib") < position("python"));
    // The PyPI layer is the top one.
    assert_eq!(layers.last().unwrap(), &["six"]);
    assert_eq!(plan["layers"].as_array().unwrap().last().unwrap()["kind"], "pypi");
}

fn kinds(plan: &serde_json::Value) -> Vec<&str> {
    plan["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn layers_are_grouped_by_default() {
    let plan = plan_json(&["--platform", "linux-64"]);
    assert_eq!(plan["layering"], "grouped");
    let kinds = kinds(&plan);
    assert!(kinds.contains(&"conda-group"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"pypi"));
    assert!(kinds.len() < 23, "{} layers", kinds.len());
    // python is far over the size for a layer of its own.
    assert!(layer_names(&plan).contains(&vec!["python".to_string()]));
    let packages: usize = layer_names(&plan).iter().map(Vec::len).sum();
    assert_eq!(packages, 23);
}

#[test]
fn single_puts_everything_in_one_layer() {
    let plan = plan_json(&["--platform", "linux-64", "--layers", "single", "--max-layers", "1"]);
    assert_eq!(plan["layering"], "single");
    assert_eq!(kinds(&plan), ["environment"]);
    assert_eq!(layer_names(&plan)[0].len(), 23);
}

#[test]
fn packages_lists_each_shared_layer() {
    let plain = pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--platform", "linux-64"])
        .output()
        .unwrap();
    let listed = pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--platform", "linux-64", "--packages"])
        .output()
        .unwrap();
    let plain = String::from_utf8(plain.stdout).unwrap();
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(plain.contains("in layers of their own"), "{plain}");
    assert!(!plain.contains("       six "));
    assert!(listed.contains("       six "), "{listed}");
    assert!(listed.lines().count() > plain.lines().count());
}

#[test]
fn the_plan_is_the_same_on_every_run() {
    assert_eq!(
        plan_json(&["--platform", "linux-64"]),
        plan_json(&["--platform", "linux-64"])
    );
}

#[test]
fn json_carries_what_a_build_needs() {
    let plan = plan_json(&[
        "--environment",
        "slim",
        "--platform",
        "linux-aarch64",
        "--layers",
        "per-package",
    ]);
    assert_eq!(plan["environment"], "slim");
    assert_eq!(
        plan["oci_platform"],
        serde_json::json!({"os": "linux", "architecture": "arm64", "variant": "v8"})
    );
    assert_eq!(layer_names(&plan), [["libzlib"], ["zlib"]]);
    let zlib = &plan["layers"][1]["packages"][0];
    assert_eq!(zlib["kind"], "conda");
    assert!(
        zlib["location"]
            .as_str()
            .unwrap()
            .starts_with("https://conda.anaconda.org/conda-forge/linux-aarch64/")
    );
    assert_eq!(zlib["sha256"].as_str().unwrap().len(), 64);
    assert!(zlib["size"].as_u64().unwrap() > 0);
    assert_eq!(plan["layers"][1]["size"], zlib["size"]);
}

#[test]
fn max_layers_merges_the_top_of_the_order() {
    let plan = plan_json(&["--platform", "linux-64", "--layers", "per-package", "--max-layers", "5"]);
    let kinds: Vec<&str> = plan["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["conda", "conda", "conda", "conda-merged", "pypi"]);
    let packages: usize = layer_names(&plan).iter().map(Vec::len).sum();
    assert_eq!(packages, 23);
}

#[test]
fn a_platform_that_is_not_linux_is_refused() {
    pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--platform", "osx-arm64"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("osx-arm64 is not a Linux platform"));
}

#[test]
fn an_unlocked_platform_names_the_locked_ones() {
    pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--platform", "linux-ppc64le"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not locked for linux-ppc64le"))
        .stderr(predicate::str::contains("linux-64, linux-aarch64, osx-arm64"));
}

#[test]
fn an_unknown_environment_names_the_known_ones() {
    pixi_oci()
        .args(["plan", "--lockfile", LOCKFILE, "--environment", "prod"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no environment named `prod`"))
        .stderr(predicate::str::contains("default, app, pypi, slim"));
}

#[test]
fn a_missing_lockfile_says_how_to_make_one() {
    pixi_oci()
        .args(["plan", "--lockfile", "tests/fixtures/missing/pixi.lock"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not read"))
        .stderr(predicate::str::contains("pixi lock"));
}

#[test]
fn a_file_that_is_not_a_lockfile_is_refused() {
    pixi_oci()
        .args(["plan", "--lockfile", "tests/fixtures/workspace/pixi.toml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a lockfile pixi-oci can read"));
}

#[test]
fn one_layer_is_too_few() {
    pixi_oci()
        .args([
            "plan",
            "--lockfile",
            LOCKFILE,
            "--max-layers",
            "1",
            "--platform",
            "linux-64",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--max-layers must be at least 2"));
}

#[test]
fn build_refuses_a_bad_tag_before_reading_anything() {
    pixi_oci()
        .args([
            "build",
            "--lockfile",
            "tests/fixtures/missing/pixi.lock",
            "--tag",
            "Not/Valid:x",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not an image name and tag"));
}

#[test]
fn build_refuses_a_non_linux_platform() {
    pixi_oci()
        .args(["build", "--lockfile", LOCKFILE, "-e", "app", "--platform", "osx-arm64"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("osx-arm64 is not a Linux platform"));
}

#[test]
fn build_checks_its_options() {
    pixi_oci()
        .args(["build", "--env", "NOT-VALID=1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("expected KEY=VALUE"));
    pixi_oci()
        .args(["build", "--entrypoint", "[1, 2]"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a JSON array of strings"));
}

#[test]
fn validate_reports_problems_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    pixi_oci()
        .arg("validate")
        .arg(dir.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("is not a valid OCI image: 2 problems"))
        .stdout(predicate::str::contains("oci-layout: missing"));
    let output = pixi_oci()
        .arg("validate")
        .arg(dir.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["problems"][1]["at"], "index.json");
}

#[test]
fn validate_refuses_what_is_not_an_image() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = |path: &std::path::Path| {
        let output = pixi_oci().arg("validate").arg(path).output().unwrap();
        assert!(!output.status.success());
        // miette wraps lines at the terminal width and frames them, so a message's words are compared without
        // either: where it wraps depends on the path's length.
        String::from_utf8_lossy(&output.stderr)
            .split(|c: char| c.is_whitespace() || "│╰─▶×".contains(c))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(stderr(&dir.path().join("missing.tar")).contains("does not exist"));
    let text = dir.path().join("notes.txt");
    std::fs::write(
        &text,
        "not an archive, just some text that is long enough to fill a header",
    )
    .unwrap();
    let message = stderr(&text);
    assert!(
        message.contains("is neither an OCI image layout directory nor a .tar archive of one"),
        "{message}"
    );
}

#[test]
fn inspect_refuses_what_is_not_an_image() {
    let dir = tempfile::tempdir().unwrap();
    pixi_oci()
        .arg("inspect")
        .arg(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("pixi oci validate"));
    pixi_oci()
        .args(["inspect", "image.tar", "--raw", "manifest", "--format", "json"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn check_validates_first_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    pixi_oci()
        .arg("check")
        .arg(dir.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("2 problems in 0 files"))
        .stdout(predicate::str::contains("note: skipped"));
    let output = pixi_oci()
        .arg("check")
        .arg(dir.path())
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["problems"][0]["kind"], "spec");
    pixi_oci()
        .arg("check")
        .arg(dir.path().join("missing.tar"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}

#[test]
fn build_refuses_a_base_that_is_not_pinned_before_downloading() {
    let dir = tempfile::tempdir().unwrap();
    let build = |base: &str| {
        let output = pixi_oci()
            .args([
                "build",
                "--lockfile",
                LOCKFILE,
                "-e",
                "app",
                "--platform",
                "linux-64",
                "--base",
                base,
            ])
            .arg("--output")
            .arg(dir.path().join("image.tar"))
            .arg("--cache-dir")
            .arg(dir.path().join("cache"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        String::from_utf8_lossy(&output.stderr)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let unpinned = build("debian:bookworm-slim");
    assert!(unpinned.contains("is not pinned to a digest"), "{unpinned}");
    assert!(unpinned.contains("crane digest"), "{unpinned}");
    let bad = build("Not A Reference");
    assert!(bad.contains("is not an image reference"), "{bad}");
    assert!(!dir.path().join("cache").exists());
}

#[test]
fn push_refuses_a_bad_destination_or_an_invalid_image_before_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let stderr = |args: &[&str]| {
        let output = pixi_oci().args(args).output().unwrap();
        assert!(!output.status.success());
        // Without miette's line wrapping and frame, which depend on the path's length.
        String::from_utf8_lossy(&output.stderr)
            .split(|c: char| c.is_whitespace() || "│╰─▶×".contains(c))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let image = dir.path().to_str().unwrap();
    assert!(stderr(&["push", image, "Not A Reference"]).contains("is not an image reference"));
    // Nothing listens on port 9: an attempt to connect would fail differently.
    let invalid = stderr(&["push", image, "127.0.0.1:9/team/app:1"]);
    assert!(invalid.contains("is not a valid OCI image: 2 problems"), "{invalid}");
}
