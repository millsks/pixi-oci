//! The streamed build checked against rattler's own installer, file for file.
//!
//! Every test here builds the same plan twice: once the way `build` does, streamed from the archives, and once
//! through the reference install in `install.rs`, rattler's linker into a directory on disk. The two images'
//! file systems must be the same: every path, kind, mode, file content, symlink target and `conda-meta` record.
//! The reference needs a file system that holds Unix modes and symlinks, so these run on Unix only.

use std::path::{Path, PathBuf};

use crate::build::{BuildOptions, DEFAULT_MAX_MEMORY, build};
use crate::fetch::Fetcher;
use crate::image::Reference;
use crate::lock::Package;
use crate::plan::{Layering, Plan};
use crate::target::Target;
use crate::testing::{self, PLACEHOLDER, Spec};
use crate::{install, layer};

/// The plan's layers through the reference install, as blobs in `dir`.
fn reference(plan: &Plan, fetcher: &Fetcher, dir: &Path) -> Vec<PathBuf> {
    let python = plan
        .layers
        .iter()
        .flat_map(|layer| &layer.packages)
        .find(|package| package.name == "python")
        .map(|package| package.version.as_str());
    let target = Target::new("/opt/env", &plan.platform, python).unwrap();
    let blobs = dir.join("reference-blobs");
    plan.layers
        .iter()
        .enumerate()
        .map(|(i, planned)| {
            let staging = tempfile::tempdir_in(dir).unwrap();
            for package in &planned.packages {
                let extracted = fetcher.extracted(package).unwrap();
                install::install(package, &extracted, staging.path(), &target).unwrap();
            }
            let entries = layer::entries(staging.path(), &install::skip_in_layer(&target.prefix, i == 0)).unwrap();
            let blob = layer::write(&entries, &blobs, 0).unwrap();
            blobs.join(blob.digest.trim_start_matches("sha256:"))
        })
        .collect()
}

/// The plan's layers as `build` writes them, with `max_memory`, and the image's manifest digest.
fn streamed(plan: &Plan, fetcher: &Fetcher, dir: &Path, name: &str, max_memory: u64) -> (Vec<PathBuf>, String) {
    let layout = dir.join(name);
    let options = BuildOptions {
        prefix: "/opt/env".to_string(),
        output: layout.clone(),
        force: true,
        reference: Reference::parse("parity:1").unwrap(),
        env: Vec::new(),
        entrypoint: None,
        cmd: None,
        workdir: None,
        source_date_epoch: 0,
        max_memory,
    };
    let written = build(plan, &options, fetcher, &mut std::io::sink()).unwrap();
    (testing::layout_layers(&layout), written.manifest.digest)
}

fn assert_parity(plan: &Plan, fetcher: &Fetcher, dir: &Path) {
    let reference = testing::flatten(&reference(plan, fetcher, dir));
    let (layers, _) = streamed(plan, fetcher, dir, "streamed", DEFAULT_MAX_MEMORY);
    testing::assert_same(&reference, &testing::flatten(&layers));
}

/// Packages that cover what installation does: text and binary placeholders, symlinks, names that differ only in
/// case, both archive formats, and a noarch Python package with an entry point. (An empty `directory` entry is not
/// among them: rattler's linker refuses one, so it is tested in `build` alone.)
fn packages(dir: &Path) -> Vec<Package> {
    let mut python = Spec::new("python")
        .file("bin/python3.13", binary_with_placeholder(4096), true, Some("binary"))
        .file("lib/python3.13/os.py", "import sys\n", false, None);
    python.version = "3.13.1".to_string();
    python
        .symlinks
        .push(("bin/python".to_string(), "python3.13".to_string()));
    python.conda_format = true;

    let mut lib = Spec::new("libthing")
        .file(
            "lib/pkgconfig/thing.pc",
            format!("prefix={PLACEHOLDER}\nlibdir=${{prefix}}/lib\n"),
            false,
            Some("text"),
        )
        .file(
            "lib/libthing.so.1",
            vec![0x7f, b'E', b'L', b'F', 0, 1, 2, 3],
            true,
            None,
        );
    // Names that differ only in case: kept apart by the streamed build everywhere, but only a case-sensitive file
    // system can hold them for the reference install (and for making the package at all).
    if cfg!(target_os = "linux") {
        lib = lib
            .file("share/doc/Thing/README", "Case matters here.\n", false, None)
            .file("share/doc/thing/README", "And here.\n", false, None);
    }
    lib.symlinks
        .push(("lib/libthing.so".to_string(), "libthing.so.1".to_string()));

    let mut tool = Spec::new("thing-tool")
        .file(
            "site-packages/thing_tool/__init__.py",
            "def main():\n    print('thing')\n",
            false,
            None,
        )
        .file(
            "python-scripts/thing-helper",
            format!("#!{PLACEHOLDER}/bin/python\nprint('helper')\n"),
            true,
            Some("text"),
        );
    tool.depends = vec!["python".to_string()];
    tool.noarch_python = Some(vec!["thing = thing_tool:main".to_string()]);
    tool.conda_format = true;

    vec![
        testing::package(dir, python),
        testing::package(dir, lib),
        testing::package(dir, tool),
        testing::simple(dir, "plain", &["libthing"]),
    ]
}

/// Bytes like a compiled program's: the placeholder as a C string among binary data.
fn binary_with_placeholder(len: usize) -> Vec<u8> {
    let mut bytes = vec![0x7f, b'E', b'L', b'F'];
    bytes.extend((0..len).map(|i| (i % 251) as u8));
    bytes.extend_from_slice(PLACEHOLDER.as_bytes());
    bytes.extend_from_slice(b"/lib/python3.13\0");
    bytes.extend((0..len).map(|i| (i % 13) as u8));
    bytes
}

#[test]
fn streamed_layers_match_rattlers_install() {
    let dir = tempfile::tempdir().unwrap();
    let plan = testing::plan_of(packages(dir.path()), 100);
    assert_parity(&plan, &Fetcher::new(dir.path().join("cache")), dir.path());
}

#[test]
fn a_merged_layer_matches_too() {
    let dir = tempfile::tempdir().unwrap();
    let plan = testing::plan_of(packages(dir.path()), 2);
    assert_eq!(plan.layers.len(), 2);
    assert_parity(&plan, &Fetcher::new(dir.path().join("cache")), dir.path());
}

#[test]
fn grouped_and_single_layers_match_too() {
    for layering in [Layering::Grouped, Layering::Single] {
        let dir = tempfile::tempdir().unwrap();
        let plan = testing::plan_with(packages(dir.path()), layering, 100);
        assert!(plan.layers.iter().any(|layer| layer.packages.len() > 1));
        assert_parity(&plan, &Fetcher::new(dir.path().join("cache")), dir.path());
    }
}

#[test]
fn spilling_changes_nothing_in_the_image() {
    let dir = tempfile::tempdir().unwrap();
    // 6 MiB naming the placeholder over and over: past the 2 MiB limit a 16 MiB budget gives, and longer than one
    // 4 MiB piece, so it is rewritten from a temporary file in pieces.
    let line = format!("{PLACEHOLDER}/share/data/{}\n", "x".repeat(40));
    let big = line.repeat(6 * 1024 * 1024 / line.len());
    // Lines that start `#!` in the middle of the file must not be taken for a script's first line at a cut.
    let big = format!(
        "#!{PLACEHOLDER}/bin/python\n{}",
        big.replacen(&line, &format!("#!{PLACEHOLDER}/x\n"), 3)
    );
    let binary = binary_with_placeholder(6 * 1024 * 1024);
    let spec = Spec::new("bigdata")
        .file("share/data/index.txt", big.clone(), false, Some("text"))
        .file("lib/libbig.so", binary, true, Some("binary"));
    let plan = testing::plan_of(vec![testing::package(dir.path(), spec)], 100);
    let fetcher = Fetcher::new(dir.path().join("cache"));
    assert_eq!(crate::build::budget(16 << 20, 8).0, 2 << 20);

    let (roomy, roomy_digest) = streamed(&plan, &fetcher, dir.path(), "roomy", DEFAULT_MAX_MEMORY);
    let (tight, tight_digest) = streamed(&plan, &fetcher, dir.path(), "tight", 16 << 20);
    assert_eq!(roomy_digest, tight_digest);
    let flattened = testing::flatten(&tight);
    assert_eq!(flattened, testing::flatten(&roomy));
    let content = &flattened["opt/env/share/data/index.txt"].2;
    assert_eq!(
        content.len(),
        big.len() - big.matches(PLACEHOLDER).count() * (PLACEHOLDER.len() - "/opt/env".len())
    );
    assert!(!String::from_utf8_lossy(content).contains(PLACEHOLDER));
    testing::assert_same(&testing::flatten(&reference(&plan, &fetcher, dir.path())), &flattened);
}

/// Real packages from conda-forge, for every environment of the fixture lockfile that `build` can make.
#[test]
fn conda_forge_packages_match_rattlers_install() {
    if std::env::var("PIXI_OCI_NETWORK_TESTS").as_deref() != Ok("1") {
        return;
    }
    let lockfile = crate::lock::load(Path::new("tests/fixtures/workspace/pixi.lock")).unwrap();
    let cache = std::env::var_os("PIXI_OCI_CACHE_DIR").map_or_else(Fetcher::default_root, PathBuf::from);
    let fetcher = Fetcher::new(cache);
    for (environment, platform, layering, max_layers) in [
        ("app", "linux-64", Layering::PerPackage, 100),
        ("app", "linux-aarch64", Layering::Grouped, 100),
        ("app", "linux-64", Layering::PerPackage, 4),
        ("app", "linux-64", Layering::Single, 100),
        ("slim", "linux-64", Layering::Grouped, 100),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let locked = crate::lock::select(&lockfile, environment, platform).unwrap();
        let plan = crate::plan::plan(locked, layering, max_layers).unwrap();
        assert_parity(&plan, &fetcher, dir.path());
    }
}
