//! A PyPI wheel installed into a layer, as pip and uv install one into a conda environment's python.
//!
//! The wheel's root and its `.data/purelib` and `.data/platlib` go into `site-packages`; `.data/scripts` into the
//! environment's `bin`, with a `#!python` line pointed at the environment's python; `.data/headers` into
//! `include/pythonX.Y/<name>`; and `.data/data` into the prefix. Each `console_scripts` and `gui_scripts` entry point
//! gets a launcher in `bin`. Every file is checked against the wheel's `RECORD` as it is written, and the installed
//! `RECORD` lists where each file went, with `INSTALLER` beside it. Nothing is unpacked on the host: entries are read
//! from the wheel and written into the layer.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::layer::Writer;
use crate::target::Target;

/// What `INSTALLER` says installed a wheel.
const INSTALLER: &str = "pixi-oci\n";

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum WheelError {
    #[error("{name}'s wheel cannot be read")]
    Open {
        name: String,
        #[source]
        source: zip::result::ZipError,
    },

    #[error("{name}'s wheel holds {path}, which would install outside the environment's prefix")]
    #[diagnostic(help("a wheel's files stay inside its environment; this wheel is damaged or crafted"))]
    Unsafe { name: String, path: String },

    #[error("{name}'s wheel is not a wheel pixi-oci can install: {message}")]
    Metadata { name: String, message: String },

    #[error("{name}'s wheel holds {path}, which its RECORD {message}")]
    #[diagnostic(help("the wheel matched the lockfile's hash, so it was built this way: report it to the package"))]
    Record {
        name: String,
        path: String,
        message: String,
    },

    #[error("{name} is a PyPI package, but the environment has no python to install it for")]
    NoPython { name: String },

    #[error("could not write {name}'s files into the layer")]
    Write {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// How a wheel file is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// As it is in the wheel.
    Plain,
    /// A `.data/scripts` file: executable, and a `#!python` first line pointed at the environment's python.
    Script,
}

#[derive(Debug, Clone)]
struct WheelFile {
    index: usize,
    /// The path in the wheel, as `RECORD` names it.
    name: String,
    /// Relative to the prefix.
    install: PathBuf,
    kind: Kind,
    executable: bool,
}

/// A wheel read and mapped onto the prefix, ready to be written.
#[derive(Debug)]
pub struct Wheel {
    name: String,
    archive: PathBuf,
    files: Vec<WheelFile>,
    /// Entry point launchers: path relative to the prefix, and the script.
    launchers: Vec<(PathBuf, String)>,
    /// The `.dist-info` directory, relative to the prefix.
    dist_info: PathBuf,
    /// `RECORD`'s hashes and sizes, by the path in the wheel.
    record: BTreeMap<String, (Option<String>, Option<u64>)>,
}

impl Wheel {
    /// Every path the wheel installs, relative to the prefix.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.files.iter().map(|file| file.install.clone()).collect();
        paths.extend(self.launchers.iter().map(|(path, _)| path.clone()));
        paths.push(self.dist_info.join("INSTALLER"));
        paths.push(self.dist_info.join("RECORD"));
        paths
    }
}

fn metadata(name: &str, message: impl Into<String>) -> WheelError {
    WheelError::Metadata {
        name: name.to_string(),
        message: message.into(),
    }
}

fn open(name: &str, archive: &Path) -> Result<zip::ZipArchive<File>, WheelError> {
    let file = File::open(archive).map_err(|source| WheelError::Open {
        name: name.to_string(),
        source: zip::result::ZipError::Io(source),
    })?;
    zip::ZipArchive::new(file).map_err(|source| WheelError::Open {
        name: name.to_string(),
        source,
    })
}

/// Read the wheel of package `name` at `archive` and map its files onto `target`'s prefix.
pub fn read(name: &str, archive: &Path, target: &Target) -> Result<Wheel, WheelError> {
    let python = target
        .python
        .as_ref()
        .ok_or_else(|| WheelError::NoPython { name: name.to_string() })?;
    let mut zip = open(name, archive)?;
    let mut entries: Vec<(usize, String, bool)> = Vec::new();
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(|source| WheelError::Open {
            name: name.to_string(),
            source,
        })?;
        let raw = entry.name().map(|n| n.into_owned()).unwrap_or_default();
        if entry.is_dir() {
            continue;
        }
        if entry.enclosed_name().is_none() || raw.contains('\\') || raw.starts_with('/') {
            return Err(WheelError::Unsafe {
                name: name.to_string(),
                path: raw,
            });
        }
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        entries.push((index, raw, executable));
    }

    // The one top-level `.dist-info` directory, and the `.data` directory named like it.
    let dist_infos: Vec<&str> = {
        let mut dirs: Vec<&str> = entries
            .iter()
            .filter_map(|(_, path, _)| path.split_once('/').map(|(top, _)| top))
            .filter(|top| top.ends_with(".dist-info"))
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    };
    let [dist_info] = dist_infos.as_slice() else {
        return Err(metadata(
            name,
            format!("it has {} .dist-info directories, not one", dist_infos.len()),
        ));
    };
    let dist_info = dist_info.to_string();
    let stem = dist_info.trim_end_matches(".dist-info");
    let data = format!("{stem}.data");
    let distribution = stem.split('-').next().unwrap_or(stem).to_string();

    let read_text = |zip: &mut zip::ZipArchive<File>, path: &str| -> Result<Option<String>, WheelError> {
        match zip.by_name(path) {
            Ok(mut entry) => {
                let mut text = String::new();
                entry
                    .read_to_string(&mut text)
                    .map_err(|err| metadata(name, format!("{path} is not text: {err}")))?;
                Ok(Some(text))
            }
            Err(zip::result::ZipError::FileNotFound) => Ok(None),
            Err(source) => Err(WheelError::Open {
                name: name.to_string(),
                source,
            }),
        }
    };
    let wheel_file = read_text(&mut zip, &format!("{dist_info}/WHEEL"))?
        .ok_or_else(|| metadata(name, format!("it has no {dist_info}/WHEEL")))?;
    let version = wheel_file
        .lines()
        .find_map(|line| line.strip_prefix("Wheel-Version:"))
        .map(str::trim)
        .unwrap_or_default();
    if !version.starts_with("1.") {
        return Err(metadata(name, format!("Wheel-Version {version:?} is not 1.x")));
    }
    let record_text = read_text(&mut zip, &format!("{dist_info}/RECORD"))?
        .ok_or_else(|| metadata(name, format!("it has no {dist_info}/RECORD")))?;
    let record = parse_record(&record_text);

    let site = &python.site_packages_path;
    let include = PathBuf::from(format!(
        "include/python{}.{}/{distribution}",
        python.short_version.0, python.short_version.1
    ));
    let record_name = format!("{dist_info}/RECORD");
    let mut files = Vec::new();
    for (index, path, executable) in entries {
        if path == record_name {
            continue;
        }
        let (install, kind) = match path.strip_prefix(&format!("{data}/")) {
            Some(rest) => {
                let (category, inner) = rest
                    .split_once('/')
                    .ok_or_else(|| metadata(name, format!("{path} is not in a .data category")))?;
                match category {
                    "purelib" | "platlib" => (site.join(inner), Kind::Plain),
                    "scripts" => (python.bin_dir.join(inner), Kind::Script),
                    "headers" => (include.join(inner), Kind::Plain),
                    "data" => (PathBuf::from(inner), Kind::Plain),
                    other => {
                        return Err(metadata(
                            name,
                            format!("{path} is in an unknown .data category {other}"),
                        ));
                    }
                }
            }
            None => (site.join(&path), Kind::Plain),
        };
        if !crate::stream::inside_prefix(&forward(&install)) {
            return Err(WheelError::Unsafe {
                name: name.to_string(),
                path,
            });
        }
        files.push(WheelFile {
            index,
            name: path,
            install: forward(&install),
            kind,
            executable: executable || kind == Kind::Script,
        });
    }

    let mut launchers = Vec::new();
    if let Some(entry_points) = read_text(&mut zip, &format!("{dist_info}/entry_points.txt"))? {
        for (command, module, attribute) in console_entry_points(&entry_points) {
            let path = forward(&python.bin_dir.join(&command));
            if !crate::stream::inside_prefix(&path) || command.contains('/') {
                return Err(WheelError::Unsafe {
                    name: name.to_string(),
                    path: command,
                });
            }
            launchers.push((path, launcher(&python.shebang(&target.prefix), &module, &attribute)));
        }
    }

    Ok(Wheel {
        name: name.to_string(),
        archive: archive.to_path_buf(),
        files,
        launchers,
        dist_info: forward(&site.join(&dist_info)),
        record,
    })
}

/// Write `wheel` into the layer, each path under the image path `image_path` gives it, where `writes` says this wheel
/// owns it.
pub fn write(
    wheel: &Wheel,
    target: &Target,
    writer: &mut Writer,
    image_path: &dyn Fn(&Path) -> String,
    writes: &dyn Fn(&str) -> bool,
) -> Result<(), WheelError> {
    let python = target.python.as_ref().ok_or_else(|| WheelError::NoPython {
        name: wheel.name.clone(),
    })?;
    let write_err = |source| WheelError::Write {
        name: wheel.name.clone(),
        source,
    };
    let site = forward(&python.site_packages_path);
    let mut zip = open(&wheel.name, &wheel.archive)?;
    let mut rows: Vec<(String, String, u64)> = Vec::new();
    for file in &wheel.files {
        let mut entry = zip.by_index(file.index).map_err(|source| WheelError::Open {
            name: wheel.name.clone(),
            source,
        })?;
        let path = image_path(&file.install);
        let (digest, size) = match file.kind {
            Kind::Script => {
                let mut content = Vec::new();
                entry.read_to_end(&mut content).map_err(write_err)?;
                let original = (Sha256::digest(&content), content.len() as u64);
                check_record(wheel, &file.name, &original.0, original.1)?;
                let content = with_python_shebang(content, &python.shebang(&target.prefix));
                if writes(&path) {
                    writer
                        .file(&path, true, content.len() as u64, content.as_slice())
                        .map_err(write_err)?;
                }
                (Sha256::digest(&content), content.len() as u64)
            }
            Kind::Plain => {
                let size = entry.size();
                let mut hashing = Hashing {
                    inner: &mut entry,
                    hasher: Sha256::new(),
                    count: 0,
                };
                if writes(&path) {
                    writer
                        .file(&path, file.executable, size, &mut hashing)
                        .map_err(write_err)?;
                } else {
                    io::copy(&mut hashing, &mut io::sink()).map_err(write_err)?;
                }
                let (digest, count) = (hashing.hasher.finalize(), hashing.count);
                check_record(wheel, &file.name, &digest, count)?;
                (digest, count)
            }
        };
        rows.push((relative_to(&site, &file.install), record_hash(&digest), size));
    }
    for (install, script) in &wheel.launchers {
        let path = image_path(install);
        if writes(&path) {
            writer
                .file(&path, true, script.len() as u64, script.as_bytes())
                .map_err(write_err)?;
        }
        rows.push((
            relative_to(&site, install),
            record_hash(&Sha256::digest(script.as_bytes())),
            script.len() as u64,
        ));
    }
    let installer = wheel.dist_info.join("INSTALLER");
    let path = image_path(&installer);
    if writes(&path) {
        writer
            .file(&path, false, INSTALLER.len() as u64, INSTALLER.as_bytes())
            .map_err(write_err)?;
    }
    rows.push((
        relative_to(&site, &installer),
        record_hash(&Sha256::digest(INSTALLER.as_bytes())),
        INSTALLER.len() as u64,
    ));
    let record = wheel.dist_info.join("RECORD");
    let mut text = String::new();
    for (path, hash, size) in &rows {
        text.push_str(&format!("{},{hash},{size}\n", csv_field(path)));
    }
    text.push_str(&format!("{},,\n", csv_field(&relative_to(&site, &record))));
    let path = image_path(&record);
    if writes(&path) {
        writer
            .file(&path, false, text.len() as u64, text.as_bytes())
            .map_err(write_err)?;
    }
    Ok(())
}

/// Check a file against the wheel's `RECORD`, which must list it with a sha256 and, if it gives one, its size.
fn check_record(wheel: &Wheel, path: &str, digest: &[u8], size: u64) -> Result<(), WheelError> {
    let failed = |message: String| WheelError::Record {
        name: wheel.name.clone(),
        path: path.to_string(),
        message,
    };
    let Some((hash, recorded_size)) = wheel.record.get(path) else {
        return Err(failed("does not list".to_string()));
    };
    match hash.as_deref() {
        Some(hash) => {
            let Some(expected) = hash.strip_prefix("sha256=") else {
                return Err(failed(format!("lists with a {hash} hash, not sha256")));
            };
            if expected.trim_end_matches('=') != record_hash(digest).trim_start_matches("sha256=") {
                return Err(failed(format!("lists with another hash ({hash})")));
            }
        }
        None => return Err(failed("lists without a hash".to_string())),
    }
    if let Some(recorded) = recorded_size
        && *recorded != size
    {
        return Err(failed(format!("lists as {recorded} bytes, but it is {size}")));
    }
    Ok(())
}

/// `RECORD`'s rows: path to (hash, size). Paths are CSV fields, so a path with a comma is quoted.
fn parse_record(text: &str) -> BTreeMap<String, (Option<String>, Option<u64>)> {
    let mut rows = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields = csv_fields(line);
        let Some(path) = fields.first() else { continue };
        let hash = fields.get(1).filter(|hash| !hash.is_empty()).cloned();
        let size = fields.get(2).and_then(|size| size.parse().ok());
        rows.insert(path.clone(), (hash, size));
    }
    rows
}

fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            (c, _) => field.push(c),
        }
    }
    fields.push(field);
    fields
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// `sha256=` and the digest in unpadded URL-safe base64, as `RECORD` writes it.
fn record_hash(digest: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::from("sha256=");
    for chunk in digest.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// `path` relative to the directory `base`, both relative to the prefix: `../../../bin/black` from site-packages.
fn relative_to(base: &Path, path: &Path) -> String {
    let base: Vec<Component> = base.components().collect();
    let path: Vec<Component> = path.components().collect();
    let common = base.iter().zip(&path).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); base.len() - common];
    parts.extend(
        path[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

fn forward(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

/// A `#!python` or `#!pythonw` first line, as wheels write it for their scripts, replaced by `shebang`.
fn with_python_shebang(content: Vec<u8>, shebang: &str) -> Vec<u8> {
    let line_end = content.iter().position(|&b| b == b'\n').unwrap_or(content.len());
    let first = &content[..line_end];
    let first = first.strip_suffix(b"\r").unwrap_or(first);
    if first == b"#!python" || first == b"#!pythonw" {
        let mut out = shebang.as_bytes().to_vec();
        out.extend_from_slice(&content[line_end..]);
        out
    } else {
        content
    }
}

/// The `console_scripts` and `gui_scripts` of an `entry_points.txt`: command, module, attribute.
fn console_entry_points(text: &str) -> Vec<(String, String, String)> {
    let mut points = Vec::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        if section != "console_scripts" && section != "gui_scripts" {
            continue;
        }
        let Some((command, target)) = line.split_once('=') else {
            continue;
        };
        // `module:attr [extra]`: extras say what the script needs, not how to run it.
        let target = target.split('[').next().unwrap_or_default().trim();
        let (module, attribute) = target.split_once(':').unwrap_or((target, ""));
        points.push((
            command.trim().to_string(),
            module.trim().to_string(),
            attribute.trim().to_string(),
        ));
    }
    points
}

/// An entry point's launcher, as pip and uv write it.
fn launcher(shebang: &str, module: &str, attribute: &str) -> String {
    let (import, call) = if attribute.is_empty() {
        (format!("import {module}"), module.to_string())
    } else {
        let first = attribute.split('.').next().unwrap_or(attribute);
        (format!("from {module} import {first}"), attribute.to_string())
    };
    format!(
        "{shebang}\n# -*- coding: utf-8 -*-\nimport re\nimport sys\n{import}\nif __name__ == \"__main__\":\n    \
         sys.argv[0] = re.sub(r\"(-script\\.pyw|\\.exe)?$\", \"\", sys.argv[0])\n    sys.exit({call}())\n"
    )
}

struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;

    use super::*;
    use crate::lock::{Kind, Package};
    use crate::testing;

    /// A wheel of `files` (path, content, mode), with a RECORD listing them unless `record` gives one.
    fn wheel_bytes(files: &[(&str, &[u8], u32)], record: Option<&str>) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let mut rows = String::new();
        let dist_info = files
            .iter()
            .find_map(|(path, _, _)| {
                path.split_once(".dist-info/")
                    .map(|(stem, _)| format!("{stem}.dist-info"))
            })
            .unwrap_or_default();
        for (path, content, mode) in files {
            // Made on Unix, as Linux wheels are: on Windows the crate would write DOS, which has no mode bits.
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .system(zip::System::Unix)
                .unix_permissions(*mode);
            zip.start_file(*path, options).unwrap();
            zip.write_all(content).unwrap();
            rows.push_str(&format!(
                "{},{},{}\n",
                csv_field(path),
                record_hash(&Sha256::digest(content)),
                content.len()
            ));
        }
        rows.push_str(&format!("{dist_info}/RECORD,,\n"));
        let options = zip::write::SimpleFileOptions::default()
            .system(zip::System::Unix)
            .unix_permissions(0o644);
        zip.start_file(format!("{dist_info}/RECORD"), options).unwrap();
        zip.write_all(record.unwrap_or(&rows).as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    const WHEEL: &[u8] =
        b"Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: false\nTag: cp313-cp313-linux_x86_64\n";

    /// Every shape a wheel's files come in.
    fn demo() -> Vec<u8> {
        wheel_bytes(
            &[
                ("demo/__init__.py", b"VERSION = 1\n", 0o644),
                ("demo/_speed.so", b"\x7fELF fake", 0o755),
                ("demo-1.0.data/platlib/demo_native.py", b"native = True\n", 0o644),
                ("demo-1.0.data/purelib/demo_pure.py", b"pure = True\n", 0o644),
                ("demo-1.0.data/scripts/demo-tool", b"#!python\nimport demo\n", 0o644),
                ("demo-1.0.data/scripts/demo-win", b"#!pythonw\r\nimport demo\r\n", 0o644),
                ("demo-1.0.data/scripts/demo-bin", b"\x7fELF binary", 0o755),
                ("demo-1.0.data/headers/demo.h", b"int demo;\n", 0o644),
                ("demo-1.0.data/data/share/demo/notes.txt", b"notes\n", 0o644),
                ("demo-1.0.dist-info/METADATA", b"Name: demo\nVersion: 1.0\n", 0o644),
                ("demo-1.0.dist-info/WHEEL", WHEEL, 0o644),
                (
                    "demo-1.0.dist-info/entry_points.txt",
                    b"[console_scripts]\ndemo = demo.cli:main\ndemo-class = demo.cli:App.run [extra]\n\n[gui_scripts]\ndemo-gui = demo\n\n[demo.plugins]\nignored = demo:x\n",
                    0o644,
                ),
            ],
            None,
        )
    }

    fn python(dir: &Path) -> Package {
        let mut spec = testing::Spec::new("python").file("bin/python3.13", "python", true, None);
        spec.version = "3.13.1".to_string();
        testing::package(dir, spec)
    }

    fn wheel_package(dir: &Path, name: &str, bytes: &[u8]) -> Package {
        let path = dir.join(format!("{name}-1.0-py3-none-any.whl"));
        fs::write(&path, bytes).unwrap();
        Package {
            name: name.to_string(),
            version: "1.0".to_string(),
            kind: Kind::Pypi,
            build: None,
            depends: Vec::new(),
            location: path.to_string_lossy().into_owned(),
            sha256: Some(crate::fetch::hex(&Sha256::digest(bytes))),
            size: None,
            record: None,
        }
    }

    use std::fs;

    /// Build `packages` and return the image's files.
    /// An image's files: path to (kind, mode, content).
    type Files = BTreeMap<String, (&'static str, u32, Vec<u8>)>;

    fn built(
        dir: &Path,
        packages: Vec<Package>,
        layering: crate::plan::Layering,
    ) -> Result<Files, crate::build::BuildError> {
        let plan = testing::plan_with(packages, layering, 100);
        let layout = dir.join(format!("layout-{layering:?}"));
        let options = crate::build::BuildOptions {
            prefix: "/opt/env".to_string(),
            output: layout.clone(),
            force: true,
            reference: crate::image::Reference::parse("wheels:1").unwrap(),
            env: Vec::new(),
            entrypoint: None,
            cmd: None,
            workdir: None,
            source_date_epoch: 0,
            max_memory: 16 << 20,
            base: None,
            labels: Vec::new(),
            annotations: Vec::new(),
            user: None,
            expose: Vec::new(),
            stop_signal: None,
        };
        crate::build::build(
            &plan,
            &options,
            &crate::fetch::Fetcher::new(dir.join("cache")),
            &mut io::sink(),
        )?;
        let report = crate::validate::validate(&layout).unwrap();
        assert!(report.valid, "{}", report.text());
        Ok(testing::flatten(&testing::layout_layers(&layout)))
    }

    const SITE: &str = "opt/env/lib/python3.13/site-packages";

    #[test]
    fn a_wheel_is_installed_as_pip_installs_it() {
        let dir = tempfile::tempdir().unwrap();
        let packages = vec![python(dir.path()), wheel_package(dir.path(), "demo", &demo())];
        for layering in [crate::plan::Layering::Grouped, crate::plan::Layering::Single] {
            let files = built(dir.path(), packages.clone(), layering).unwrap();
            let file = |path: &str| {
                files
                    .get(path)
                    .unwrap_or_else(|| panic!("no {path} in {:#?}", files.keys()))
            };
            assert_eq!(file(&format!("{SITE}/demo/__init__.py")).2, b"VERSION = 1\n");
            assert_eq!(file(&format!("{SITE}/demo/_speed.so")).1, 0o755);
            assert_eq!(file(&format!("{SITE}/demo_native.py")).2, b"native = True\n");
            assert_eq!(file(&format!("{SITE}/demo_pure.py")).2, b"pure = True\n");
            let tool = file("opt/env/bin/demo-tool");
            assert_eq!(
                (tool.1, tool.2.as_slice()),
                (0o755, &b"#!/opt/env/bin/python3.13\nimport demo\n"[..])
            );
            assert_eq!(
                file("opt/env/bin/demo-win").2,
                b"#!/opt/env/bin/python3.13\nimport demo\r\n"
            );
            assert_eq!(file("opt/env/bin/demo-bin").2, b"\x7fELF binary");
            assert_eq!(file("opt/env/include/python3.13/demo/demo.h").2, b"int demo;\n");
            assert_eq!(file("opt/env/share/demo/notes.txt").2, b"notes\n");
            let launcher = String::from_utf8(file("opt/env/bin/demo").2.clone()).unwrap();
            assert!(launcher.starts_with("#!/opt/env/bin/python3.13\n"), "{launcher}");
            assert!(
                launcher.contains("from demo.cli import main\n") && launcher.contains("sys.exit(main())"),
                "{launcher}"
            );
            let class = String::from_utf8(file("opt/env/bin/demo-class").2.clone()).unwrap();
            assert!(
                class.contains("from demo.cli import App\n") && class.contains("sys.exit(App.run())"),
                "{class}"
            );
            let gui = String::from_utf8(file("opt/env/bin/demo-gui").2.clone()).unwrap();
            assert!(
                gui.contains("import demo\n") && gui.contains("sys.exit(demo())"),
                "{gui}"
            );
            assert!(!files.contains_key("opt/env/bin/ignored"));
            assert_eq!(
                file(&format!("{SITE}/demo-1.0.dist-info/INSTALLER")).2,
                INSTALLER.as_bytes()
            );

            let record = String::from_utf8(file(&format!("{SITE}/demo-1.0.dist-info/RECORD")).2.clone()).unwrap();
            let rows = parse_record(&record);
            for (path, content) in [
                ("demo/__init__.py", &b"VERSION = 1\n"[..]),
                ("demo_native.py", b"native = True\n"),
                ("../../../bin/demo-tool", b"#!/opt/env/bin/python3.13\nimport demo\n"),
                ("../../../include/python3.13/demo/demo.h", b"int demo;\n"),
                ("../../../share/demo/notes.txt", b"notes\n"),
            ] {
                let (hash, size) = &rows[path];
                assert_eq!(
                    hash.as_deref(),
                    Some(record_hash(&Sha256::digest(content)).as_str()),
                    "{path}"
                );
                assert_eq!(*size, Some(content.len() as u64), "{path}");
            }
            assert!(rows.contains_key("../../../bin/demo") && rows.contains_key("demo-1.0.dist-info/INSTALLER"));
            assert_eq!(rows["demo-1.0.dist-info/RECORD"], (None, None));
        }
    }

    #[test]
    fn a_wheel_replaces_a_conda_file_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut conda = testing::Spec::new("conda-demo").file(
            "lib/python3.13/site-packages/demo/__init__.py",
            "from conda\n",
            false,
            None,
        );
        conda.depends = vec!["python".to_string()];
        let packages = vec![
            python(dir.path()),
            testing::package(dir.path(), conda),
            wheel_package(dir.path(), "demo", &demo()),
        ];
        let files = built(dir.path(), packages, crate::plan::Layering::Single).unwrap();
        assert_eq!(files[&format!("{SITE}/demo/__init__.py")].2, b"VERSION = 1\n");
    }

    fn refused(files: &[(&str, &[u8], u32)], record: Option<&str>, python_too: bool) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut packages = vec![wheel_package(dir.path(), "bad", &wheel_bytes(files, record))];
        if python_too {
            packages.insert(0, python(dir.path()));
        }
        built(dir.path(), packages, crate::plan::Layering::Grouped)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_wheel_that_breaks_the_rules_is_refused_by_name() {
        let ok_wheel = ("bad-1.0.dist-info/WHEEL", WHEEL, 0o644);
        let err = refused(
            &[("bad/x.py", b"x", 0o644), ok_wheel],
            Some("bad/x.py,sha256=AAAA,1\nbad-1.0.dist-info/WHEEL,,\n"),
            true,
        );
        assert!(
            err.contains("bad's wheel holds bad/x.py, which its RECORD lists with another hash (sha256=AAAA)"),
            "{err}"
        );
        let err = refused(
            &[("bad/x.py", b"x", 0o644), ok_wheel],
            Some("bad-1.0.dist-info/WHEEL,,\n"),
            true,
        );
        assert!(
            err.contains("bad's wheel holds bad/x.py, which its RECORD does not list"),
            "{err}"
        );
        let err = refused(&[("bad/x.py", b"x", 0o644), ok_wheel], Some("bad/x.py,,\n"), true);
        assert!(
            err.contains("holds bad/x.py, which its RECORD lists without a hash"),
            "{err}"
        );
        let err = refused(
            &[("bad/x.py", b"x", 0o644), ok_wheel],
            Some("bad/x.py,md5=abc,1\n"),
            true,
        );
        assert!(
            err.contains("holds bad/x.py, which its RECORD lists with a md5=abc hash, not sha256"),
            "{err}"
        );
        let err = refused(
            &[("bad-1.0.data/../../../etc/passwd", b"x", 0o644), ok_wheel],
            None,
            true,
        );
        assert!(
            err.contains("which would install outside the environment's prefix") || err.contains("cannot be read"),
            "{err}"
        );
        let err = refused(
            &[("bad-1.0.data/data/../../../../../etc/x", b"x", 0o644), ok_wheel],
            None,
            true,
        );
        assert!(
            err.contains("outside the environment's prefix") || err.contains("cannot be read"),
            "{err}"
        );
        let err = refused(&[("bad-1.0.data/weird/x", b"x", 0o644), ok_wheel], None, true);
        assert!(err.contains("is in an unknown .data category weird"), "{err}");
        let err = refused(&[("bad-1.0.data/x", b"x", 0o644), ok_wheel], None, true);
        assert!(err.contains("is not in a .data category"), "{err}");
        let err = refused(
            &[("bad/x.py", b"x", 0o644), ("bad-1.0.dist-info/METADATA", b"m", 0o644)],
            None,
            true,
        );
        assert!(err.contains("it has no bad-1.0.dist-info/WHEEL"), "{err}");
        let err = refused(
            &[
                ("bad/x.py", b"x", 0o644),
                ("bad-1.0.dist-info/WHEEL", b"Wheel-Version: 2.0\n", 0o644),
            ],
            None,
            true,
        );
        assert!(err.contains("Wheel-Version \"2.0\" is not 1.x"), "{err}");
        let err = refused(&[ok_wheel, ("other-1.0.dist-info/WHEEL", WHEEL, 0o644)], None, true);
        assert!(err.contains("it has 2 .dist-info directories, not one"), "{err}");
        let err = refused(
            &[
                ("bad/x.py", b"x", 0o644),
                ok_wheel,
                (
                    "bad-1.0.dist-info/entry_points.txt",
                    b"[console_scripts]\n../evil = bad:x\n",
                    0o644,
                ),
            ],
            None,
            true,
        );
        assert!(err.contains("outside the environment's prefix"), "{err}");
        let err = refused(&[("bad/x.py", b"x", 0o644), ok_wheel], None, false);
        assert!(
            err.contains("bad is a PyPI package, but the environment has no python"),
            "{err}"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut package = wheel_package(dir.path(), "junk", b"not a zip at all");
        package.sha256 = Some(crate::fetch::hex(&Sha256::digest(b"not a zip at all")));
        let err = built(
            dir.path(),
            vec![python(dir.path()), package],
            crate::plan::Layering::Grouped,
        )
        .unwrap_err();
        assert!(err.to_string().contains("junk's wheel cannot be read"), "{err}");
    }

    #[test]
    fn the_small_formats_are_read_and_written_like_the_specs_say() {
        assert_eq!(
            record_hash(&Sha256::digest(b"")),
            "sha256=47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU"
        );
        let rows = parse_record("a.py,sha256=x,3\n\"b,c.py\",sha256=y,4\n\"d\"\"e\",,\n\nf,,\n");
        assert_eq!(rows["a.py"], (Some("sha256=x".to_string()), Some(3)));
        assert_eq!(rows["b,c.py"], (Some("sha256=y".to_string()), Some(4)));
        assert_eq!(rows["d\"e"], (None, None));
        assert_eq!(csv_field("b,c.py"), "\"b,c.py\"");
        assert_eq!(csv_field("d\"e"), "\"d\"\"e\"");
        assert_eq!(
            relative_to(Path::new("lib/python3.13/site-packages"), Path::new("bin/x")),
            "../../../bin/x"
        );
        assert_eq!(
            relative_to(
                Path::new("lib/python3.13/site-packages"),
                Path::new("lib/python3.13/site-packages/a/b")
            ),
            "a/b"
        );
        assert_eq!(
            with_python_shebang(b"#!/usr/bin/env python\n".to_vec(), "#!/x"),
            b"#!/usr/bin/env python\n"
        );
        assert_eq!(with_python_shebang(b"#!python".to_vec(), "#!/x"), b"#!/x");
        assert_eq!(
            console_entry_points("# c\n[console_scripts]\n; c\nnoeq\n"),
            Vec::<(String, String, String)>::new()
        );
    }
}
