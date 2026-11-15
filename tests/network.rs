//! A real image, built from conda-forge and run. Needs the network, so it runs only when PIXI_OCI_NETWORK_TESTS=1
//! (CI sets it on Linux); on other hosts it passes without doing anything.
//!
//! The image is built with its prefix at a scratch path on this machine, its layers are unpacked there in order,
//! and the python they hold is run on the host's glibc: if prefix replacement, noarch installation or layer order
//! were wrong, python would not start or `six` would not import.

use std::fs;
use std::path::Path;
use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;

fn enabled() -> Option<&'static str> {
    if std::env::var("PIXI_OCI_NETWORK_TESTS").as_deref() != Ok("1") {
        return None;
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

fn unpack_image(archive: &Path, root: &Path) {
    let layout = root.join("layout");
    tar::Archive::new(fs::File::open(archive).unwrap())
        .unpack(&layout)
        .unwrap();
    let index: serde_json::Value = serde_json::from_slice(&fs::read(layout.join("index.json")).unwrap()).unwrap();
    let digest = index["manifests"][0]["digest"]
        .as_str()
        .unwrap()
        .trim_start_matches("sha256:");
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(layout.join("blobs/sha256").join(digest)).unwrap()).unwrap();
    for layer in manifest["layers"].as_array().unwrap() {
        let digest = layer["digest"].as_str().unwrap().trim_start_matches("sha256:");
        let blob = fs::File::open(layout.join("blobs/sha256").join(digest)).unwrap();
        // The layers hold paths relative to `/`, and the prefix is an absolute path under `root`. Directories that
        // exist already (`/tmp` and the scratch directory above the prefix) belong to someone else: a container
        // runtime would merge them, and this test leaves them alone.
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(blob));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = Path::new("/").join(entry.path().unwrap());
            if entry.header().entry_type().is_dir() && path.is_dir() {
                continue;
            }
            assert!(entry.unpack_in("/").unwrap(), "{} escapes the root", path.display());
        }
    }
}

#[test]
fn a_built_image_runs_its_python() {
    let Some(platform) = enabled() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("env");
    let archive = dir.path().join("app.tar");
    let status = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args(["build", "--lockfile", "tests/fixtures/workspace/pixi.lock", "-e", "app"])
        .args(["--platform", platform, "--tag", "app:test", "--prefix"])
        .arg(&prefix)
        .arg("--output")
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());

    let validated = Command::cargo_bin("pixi-oci")
        .unwrap()
        .arg("validate")
        .arg(&archive)
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&validated.stdout);
    assert!(validated.status.success(), "{report}");
    assert!(report.contains("is a valid OCI image"), "{report}");

    // inspect reads back the layers plan predicted, package for package.
    let json = |args: &[&str]| -> serde_json::Value {
        let output = Command::cargo_bin("pixi-oci").unwrap().args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let names = |layers: &serde_json::Value| -> Vec<Vec<String>> {
        layers
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
    };
    let lockfile = "tests/fixtures/workspace/pixi.lock";
    let plan = json(&[
        "plan",
        "--lockfile",
        lockfile,
        "-e",
        "app",
        "--platform",
        platform,
        "--format",
        "json",
    ]);
    let inspection = json(&["inspect", archive.to_str().unwrap(), "--format", "json"]);
    assert_eq!(names(&inspection["layers"]), names(&plan["layers"]));
    assert_eq!(inspection["reference"], "app:test");

    // check reads the real binaries: no glibc yet (until a base image), tk's X11 library that the environment does
    // not lock, and the shell scripts' interpreters; nothing else.
    let checked = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args(["check", archive.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(!checked.status.success());
    let report: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap();
    let kinds: Vec<&str> = report["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"glibc"), "{report:#}");
    assert!(
        kinds.iter().all(|kind| ["glibc", "library", "shebang"].contains(kind)),
        "{report:#}"
    );
    for problem in report["problems"].as_array().unwrap() {
        if problem["kind"] == "library" {
            assert!(
                problem["message"].as_str().unwrap().starts_with("libX11.so.6 "),
                "{report:#}"
            );
        }
    }
    assert!(report["checked"]["elf"].as_u64().unwrap() > 50);

    unpack_image(&archive, dir.path());
    let output = Command::new(prefix.join("bin/python"))
        .args([
            "-c",
            "import sys, six, ssl, sqlite3, zlib; print(sys.prefix, six.__version__)",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("{} 1.17.0", prefix.display())
    );
    assert!(prefix.join("conda-meta/history").is_file());
    assert!(prefix.join("conda-meta").read_dir().unwrap().count() > 20);
}
