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

/// The two base images the docs recommend, pinned as in `src/registry.rs`.
const BASES: [(&str, &str); 2] = [
    (
        "ubi9",
        "registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9",
    ),
    (
        "bookworm",
        "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc",
    ),
];

/// On each base: the image validates, `check` finds nothing but what the environment really lacks, and where docker
/// is installed (GitHub's Linux runners), the image loads and its python runs on the base's glibc.
#[test]
fn an_image_on_each_base_runs_its_python() {
    let Some(platform) = enabled() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let docker = Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|output| output.status.success());
    for (name, base) in BASES {
        let archive = dir.path().join(format!("{name}.tar"));
        let tag = format!("pixi-oci-test-{name}:latest");
        let status = Command::cargo_bin("pixi-oci")
            .unwrap()
            .args(["build", "--lockfile", "tests/fixtures/workspace/pixi.lock", "-e", "app"])
            .args([
                "--platform",
                platform,
                "--tag",
                &tag,
                "--base",
                base,
                "--entrypoint",
                "python",
                "--output",
            ])
            .arg(&archive)
            .status()
            .unwrap();
        assert!(status.success(), "{name}");
        let pixi_oci = |args: &[&str]| {
            Command::cargo_bin("pixi-oci")
                .unwrap()
                .args(args)
                .arg(&archive)
                .output()
                .unwrap()
        };
        assert!(pixi_oci(&["validate"]).status.success(), "{name}");
        let report: serde_json::Value =
            serde_json::from_slice(&pixi_oci(&["check", "--format", "json"]).stdout).unwrap();
        for problem in report["problems"].as_array().unwrap() {
            let message = problem["message"].as_str().unwrap();
            // tk links to X11, which the environment does not lock; ubi-minimal has no perl for conda's perl scripts.
            let expected = message.starts_with("libX11.so.6 ") || message.contains("#!/usr/bin/env perl");
            assert!(expected, "{name}: {report:#}");
        }
        if docker {
            let loaded = Command::new("docker")
                .args(["load", "-i"])
                .arg(&archive)
                .status()
                .unwrap();
            assert!(loaded.success(), "{name}");
            let output = Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    &tag,
                    "-c",
                    "import ssl, sqlite3, six; print(ssl.OPENSSL_VERSION)",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).starts_with("OpenSSL 3."),
                "{name}"
            );
            let _ = Command::new("docker").args(["rmi", &tag]).status();
        }
    }
}

/// Every file under `dir`, relative to it, with its sha256, leaving out what an installer writes about itself and
/// what python writes when it runs. The environment's own path, `prefix`, is read as `<prefix>` in every file, since
/// two installs into two places write their own place into files such as conda's `conda-site.pth`.
fn tree(dir: &Path, prefix: &Path) -> std::collections::BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, prefix: &[u8], out: &mut std::collections::BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let name = relative.rsplit('/').next().unwrap_or_default().to_string();
            if name == "__pycache__"
                || ["INSTALLER", "REQUESTED", "RECORD", "direct_url.json", "uv_cache.json"].contains(&name.as_str())
            {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, prefix, out);
            } else {
                let bytes = replace(&fs::read(&path).unwrap(), prefix, b"<prefix>");
                out.insert(
                    relative,
                    pixi_oci::fetch::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes)),
                );
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, dir, prefix.to_string_lossy().as_bytes(), &mut out);
    out
}

/// `bytes` with every `from` replaced by `to`.
fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if !from.is_empty() && bytes[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// The paths a RECORD lists, leaving out the ones the installer writes about itself.
fn record_paths(record: &Path) -> std::collections::BTreeSet<String> {
    fs::read_to_string(record)
        .unwrap()
        .lines()
        .filter_map(|line| line.split(',').next())
        .filter(|path| !path.is_empty() && !path.contains("__pycache__"))
        .filter(|path| {
            !["INSTALLER", "REQUESTED", "RECORD", "direct_url.json", "uv_cache.json"]
                .iter()
                .any(|name| path.ends_with(name))
        })
        .map(str::to_string)
        .collect()
}

/// The fixture's PyPI wheels, installed by pixi-oci into an image and by pixi itself (through uv) into an environment:
/// site-packages must hold the same files with the same bytes, each RECORD must list the same paths, and the
/// launchers and the binary from `.data/scripts` must run.
#[test]
fn wheels_install_as_pixi_installs_them() {
    let Some(platform) = enabled() else {
        return;
    };
    if Command::new("pixi").arg("--version").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    for file in ["pixi.toml", "pixi.lock"] {
        fs::copy(Path::new("tests/fixtures/workspace").join(file), workspace.join(file)).unwrap();
    }
    let installed = Command::new("pixi")
        .args(["install", "--frozen", "-e", "pypi", "--manifest-path"])
        .arg(workspace.join("pixi.toml"))
        .status()
        .unwrap();
    assert!(installed.success());
    let reference = workspace.join(".pixi/envs/pypi");

    let prefix = dir.path().join("env");
    let archive = dir.path().join("pypi.tar");
    let built = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args([
            "build",
            "--lockfile",
            "tests/fixtures/workspace/pixi.lock",
            "-e",
            "pypi",
        ])
        .args(["--platform", platform, "--tag", "pypi:test", "--prefix"])
        .arg(&prefix)
        .arg("--output")
        .arg(&archive)
        .status()
        .unwrap();
    assert!(built.success());
    unpack_image(&archive, dir.path());

    let site = "lib/python3.13/site-packages";
    let ours = tree(&prefix.join(site), &prefix);
    let theirs = tree(&reference.join(site), &reference);
    let only_ours: Vec<&String> = ours.keys().filter(|path| !theirs.contains_key(*path)).collect();
    let only_theirs: Vec<&String> = theirs.keys().filter(|path| !ours.contains_key(*path)).collect();
    assert!(
        only_ours.is_empty() && only_theirs.is_empty(),
        "only pixi-oci: {only_ours:?}\nonly pixi: {only_theirs:?}"
    );
    let differ: Vec<&String> = ours.keys().filter(|path| ours[*path] != theirs[*path]).collect();
    assert!(differ.is_empty(), "different bytes: {differ:?}");

    for dist_info in fs::read_dir(prefix.join(site)).unwrap() {
        let dist_info = dist_info.unwrap().path();
        if !dist_info.to_string_lossy().ends_with(".dist-info") {
            continue;
        }
        let name = dist_info.file_name().unwrap().to_owned();
        let record = dist_info.join("RECORD");
        if !reference.join(site).join(&name).join("RECORD").is_file() {
            continue;
        }
        assert_eq!(
            record_paths(&record),
            record_paths(&reference.join(site).join(&name).join("RECORD")),
            "{}",
            name.to_string_lossy()
        );
        assert_eq!(fs::read_to_string(dist_info.join("INSTALLER")).unwrap(), "pixi-oci\n");
    }

    for (program, expected) in [("black", "black, 25.11.0"), ("ruff", "ruff 0.14.5")] {
        let output = Command::new(prefix.join("bin").join(program))
            .arg("--version")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).starts_with(expected),
            "{program}"
        );
    }
}

/// A PyPI-only command line tool as the image's entrypoint, on a base, run by docker where it is installed.
#[test]
fn a_wheel_entrypoint_runs_in_a_container() {
    let Some(platform) = enabled() else {
        return;
    };
    let docker = Command::new("docker")
        .arg("info")
        .output()
        .is_ok_and(|output| output.status.success());
    if !docker {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("black.tar");
    let tag = "pixi-oci-test-black:latest";
    let built = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args([
            "build",
            "--lockfile",
            "tests/fixtures/workspace/pixi.lock",
            "-e",
            "pypi",
        ])
        .args([
            "--platform",
            platform,
            "--tag",
            tag,
            "--base",
            BASES[1].1,
            "--entrypoint",
            "black",
            "--output",
        ])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(built.success());
    assert!(
        Command::new("docker")
            .args(["load", "-i"])
            .arg(&archive)
            .status()
            .unwrap()
            .success()
    );
    let output = Command::new("docker")
        .args(["run", "--rm", tag, "--version"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("black, 25.11.0"));
    let _ = Command::new("docker").args(["rmi", tag]).status();
}

/// The fixture's small `slim` environment, built with no base for this machine's Linux platform.
fn slim_image(dir: &Path, platform: &str) -> std::path::PathBuf {
    let archive = dir.join("slim.tar");
    let built = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args([
            "build",
            "--lockfile",
            "tests/fixtures/workspace/pixi.lock",
            "-e",
            "slim",
            "--platform",
            platform,
            "--output",
        ])
        .arg(&archive)
        .status()
        .unwrap();
    assert!(built.success());
    archive
}

fn push_json(archive: &Path, destination: &str) -> serde_json::Value {
    let output = Command::cargo_bin("pixi-oci")
        .unwrap()
        .arg("push")
        .arg(archive)
        .args([destination, "--format", "json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A plain `registry:2`, started in docker where docker is installed: the image goes up once, a second push sends
/// nothing, and docker pulls it back.
#[test]
fn an_image_pushes_to_a_registry() {
    let Some(platform) = enabled() else {
        return;
    };
    let docker = |args: &[&str]| Command::new("docker").args(args).output();
    if !docker(&["info"]).is_ok_and(|output| output.status.success()) {
        return;
    }
    let started = docker(&["run", "-d", "--rm", "-p", "127.0.0.1::5000", "registry:2"]).unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let container = String::from_utf8_lossy(&started.stdout).trim().to_string();
    let port = String::from_utf8_lossy(&docker(&["port", &container, "5000"]).unwrap().stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    // The registry takes a moment to listen.
    let ready = (0..50).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(200));
        std::net::TcpStream::connect(&port).is_ok()
    });
    assert!(ready, "registry:2 on {port} did not start");

    let dir = tempfile::tempdir().unwrap();
    let archive = slim_image(dir.path(), platform);
    let destination = format!("{port}/pixi-oci/slim:test");
    let first = push_json(&archive, &destination);
    assert!(first["uploaded"].as_u64().unwrap() >= 2, "{first:#}");
    let second = push_json(&archive, &destination);
    assert_eq!(second["uploaded"], 0, "{second:#}");
    assert_eq!(second["digest"], first["digest"]);
    let pulled = docker(&["pull", &destination]).unwrap();
    assert!(pulled.status.success(), "{}", String::from_utf8_lossy(&pulled.stderr));
    // pixi-oci pulls it back too, with the digest it was pushed with.
    let back = dir.path().join("back.tar");
    let output = Command::cargo_bin("pixi-oci")
        .unwrap()
        .args(["pull", &destination, "--platform", platform, "--format", "json", "-o"])
        .arg(&back)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["digest"], first["digest"]);
    let _ = docker(&["rmi", &destination]);
    let _ = docker(&["rm", "-f", &container]);
}

/// GitHub's registry, with the workflow's token: only where CI sets PIXI_OCI_GHCR_TEST to the repository to push
/// to, which it does for this repository's own runs (a fork's token cannot push).
#[test]
fn an_image_pushes_to_ghcr() {
    let Some(platform) = enabled() else {
        return;
    };
    let Some(repository) = std::env::var("PIXI_OCI_GHCR_TEST")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let archive = slim_image(dir.path(), platform);
    let destination = format!("{repository}:ci-{platform}");
    let first = push_json(&archive, &destination);
    let second = push_json(&archive, &destination);
    assert_eq!(second["uploaded"], 0, "{second:#}");
    assert_eq!(second["digest"], first["digest"]);
}

/// The two documented bases, pulled by tag for both platforms, with no docker: each validates.
#[test]
fn public_images_pull_for_both_platforms() {
    if enabled().is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (name, base) in BASES {
        let tag = base.split('@').next().unwrap();
        for platform in ["linux-64", "linux-aarch64"] {
            let output = dir.path().join(format!("{name}-{platform}.tar"));
            let pulled = Command::cargo_bin("pixi-oci")
                .unwrap()
                .args(["pull", tag, "--platform", platform, "-o"])
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                pulled.status.success(),
                "{tag} {platform}: {}",
                String::from_utf8_lossy(&pulled.stderr)
            );
            let validated = Command::cargo_bin("pixi-oci")
                .unwrap()
                .arg("validate")
                .arg(&output)
                .output()
                .unwrap();
            assert!(validated.status.success(), "{tag} {platform}");
        }
    }
}
