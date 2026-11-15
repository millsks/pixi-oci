//! A layer written straight from package archives, without unpacking them on the host.
//!
//! Each archive's entries are read in the archive's own order and written into the layer as they come. Their Linux
//! modes, symlinks and exact file names go from the archive's tar headers into the layer's, so the host file
//! system never has to hold them: no Unix modes or symlinks on Windows, no case folding on Windows or macOS. The
//! only files the host keeps are archives, blobs and spill files, all named by hashes.
//!
//! Installation follows rattler's own, as `rattler::install::link_package_sync` does it: the same paths for
//! `noarch: python` packages, the same placeholder replacement (rattler's public functions), the same entry-point
//! scripts, and a `conda-meta` record built the same way. `parity` tests compare the two file for file.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use rattler::install::link::copy_and_replace_placeholders;
use rattler::install::python_entry_point_template;
use rattler_conda_types::package::{IndexJson, LinkJson, NoArchLinks, PackageFile, PathType, PathsEntry, PathsJson};
use rattler_conda_types::prefix_record::{self, PathsEntry as InstalledPath};
use rattler_conda_types::{PrefixRecord, RepoDataRecord};
use rattler_digest::Sha256;
use rattler_digest::digest::Digest;

use crate::layer::{Blob, Writer};
use crate::lock::Package;
use crate::target::Target;

/// Why a layer could not be written from its packages' archives.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum StreamError {
    #[error("could not read {name}'s archive at {}", .archive.display())]
    Archive {
        name: String,
        archive: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{name}'s archive has no readable {file}")]
    #[diagnostic(help("the package may be damaged or built by a very old conda-build; re-lock or rebuild it"))]
    Metadata {
        name: String,
        file: String,
        #[source]
        source: Box<rattler_package_streaming::ExtractError>,
    },

    #[error("{name} is a noarch Python package, but the environment has no python to install it for")]
    NoPython { name: String },

    #[error("{name}'s location in the lockfile is not a URL, so it has no repodata record to install from")]
    #[diagnostic(help("pixi-oci installs packages from channels; lock this one from a channel"))]
    NoRecord { name: String },

    #[error("{name}'s paths.json lists {path}, but its archive has no such file")]
    Missing { name: String, path: String },

    #[error("{name} would install {path}, which is outside the environment's prefix")]
    #[diagnostic(help("a conda package's paths stay inside its environment; this archive is damaged or crafted"))]
    OutsidePrefix { name: String, path: String },

    #[error("{name}'s archive holds {path} as a hard link, which pixi-oci cannot place yet")]
    HardLink { name: String, path: String },

    #[error("could not write the layer")]
    Write(#[source] io::Error),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Wheel(#[from] crate::wheel::WheelError),
}

/// One package of a layer and the archive it is read from.
pub struct Input<'a> {
    pub package: &'a Package,
    pub archive: PathBuf,
}

/// What a layer is written with besides its packages.
pub struct Options<'a> {
    pub target: &'a Target,
    /// The bottom layer also carries the environment's empty `conda-meta/history`.
    pub first: bool,
    /// Seconds since the Unix epoch, on every entry.
    pub mtime: u64,
    /// Files rewritten in memory up to this size; larger ones wait in a temporary file under `spill_dir`.
    pub spill_limit: usize,
    pub spill_dir: &'a Path,
}

/// A package's metadata, read from its archive before its content is streamed.
struct Meta {
    index: IndexJson,
    paths: PathsJson,
    link: Option<LinkJson>,
}

/// Write one layer holding `inputs`, in order, into `blobs_dir`: the conda packages, then the PyPI wheels on top.
pub fn write_layer(inputs: &[Input<'_>], options: &Options<'_>, blobs_dir: &Path) -> Result<Blob, StreamError> {
    let (wheel_inputs, inputs): (Vec<&Input<'_>>, Vec<&Input<'_>>) = inputs
        .iter()
        .partition(|input| input.package.kind == crate::lock::Kind::Pypi);
    let metas = inputs
        .iter()
        .map(|input| read_meta(input, options.target))
        .collect::<Result<Vec<_>, _>>()?;
    let wheels = wheel_inputs
        .iter()
        .map(|input| crate::wheel::read(&input.package.name, &input.archive, options.target))
        .collect::<Result<Vec<_>, _>>()?;

    // Where a later package of the layer installs the same path, it wins, as it would if they were installed one
    // after the other into one prefix. Every package still records the path in its own conda-meta record.
    // Every path a package installs stays inside the prefix: an archive whose paths.json says otherwise is refused
    // before any of the layer is written.
    for (input, meta) in inputs.iter().zip(&metas) {
        let commands = entry_point_commands(meta);
        let paths = meta
            .paths
            .paths
            .iter()
            .map(|entry| install_path(meta, entry, options.target))
            .chain(commands.iter().map(|command| entry_point_path(options.target, command)));
        for path in paths {
            if !inside_prefix(&path) {
                return Err(StreamError::OutsidePrefix {
                    name: input.package.name.clone(),
                    path: path.to_string_lossy().into_owned(),
                });
            }
        }
    }

    let mut owner: HashMap<String, usize> = HashMap::new();
    for (i, meta) in metas.iter().enumerate() {
        for entry in &meta.paths.paths {
            owner.insert(
                image_path(options.target, &install_path(meta, entry, options.target)),
                i,
            );
        }
        for command in entry_point_commands(meta) {
            owner.insert(
                image_path(options.target, &entry_point_path(options.target, &command)),
                i,
            );
        }
    }
    // A wheel installed after the conda packages replaces a file one of them installed at the same path.
    for (w, wheel) in wheels.iter().enumerate() {
        for path in wheel.paths() {
            owner.insert(image_path(options.target, &path), metas.len() + w);
        }
    }

    let mut writer = Writer::new(blobs_dir, options.mtime).map_err(StreamError::Write)?;
    if options.first {
        let history = image_path(options.target, Path::new("conda-meta/history"));
        writer
            .file(&history, false, 0, io::empty())
            .map_err(StreamError::Write)?;
    }
    for (i, (input, meta)) in inputs.iter().zip(&metas).enumerate() {
        let writes = |path: &str| owner.get(path) == Some(&i);
        install_package(input, meta, options, &mut writer, &writes)?;
    }
    for (w, wheel) in wheels.iter().enumerate() {
        let writes = |path: &str| owner.get(path) == Some(&(metas.len() + w));
        let image = |path: &Path| image_path(options.target, path);
        crate::wheel::write(wheel, options.target, &mut writer, &image, &writes)?;
    }
    writer.finish().map_err(StreamError::Write)
}

fn read_meta(input: &Input<'_>, target: &Target) -> Result<Meta, StreamError> {
    let metadata_err = |file: &str| {
        let name = input.package.name.clone();
        let file = file.to_string();
        move |source| StreamError::Metadata {
            name,
            file,
            source: Box::new(source),
        }
    };
    let index: IndexJson = read_package_file(&input.archive).map_err(metadata_err("info/index.json"))?;
    let paths: PathsJson = read_package_file(&input.archive).map_err(metadata_err("info/paths.json"))?;
    let link = if index.noarch.is_python() {
        if target.python.is_none() {
            return Err(StreamError::NoPython {
                name: input.package.name.clone(),
            });
        }
        // A noarch package without link.json has no entry points; rattler treats it the same way.
        read_package_file::<LinkJson>(&input.archive).ok()
    } else {
        None
    };
    Ok(Meta { index, paths, link })
}

fn read_package_file<P: PackageFile>(archive: &Path) -> Result<P, rattler_package_streaming::ExtractError> {
    rattler_package_streaming::seek::read_package_file::<P>(archive)
}

/// Where a package path is installed, relative to the prefix, with forward slashes on every host: a Linux path,
/// and the same in the `conda-meta` record whether the image was built on Linux or Windows.
fn install_path(meta: &Meta, entry: &PathsEntry, target: &Target) -> PathBuf {
    let path = match (&target.python, meta.index.noarch.is_python()) {
        (Some(python), true) => python.get_python_noarch_target_path(&entry.relative_path).into_owned(),
        _ => entry.relative_path.clone(),
    };
    forward_slashes(&path)
}

/// Whether a path relative to the prefix stays inside it: only plain names, no `..`, no root, no drive.
pub(crate) fn inside_prefix(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .to_string_lossy()
            .split('/')
            .all(|part| !part.is_empty() && part != ".." && part != "." && !part.contains(':') && !part.contains('\\'))
}

fn forward_slashes(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().replace('\\', "/"))
}

/// A path relative to the prefix, as a path in the image relative to `/`, with forward slashes.
fn image_path(target: &Target, relative: &Path) -> String {
    let relative = relative.to_string_lossy().replace('\\', "/");
    format!("{}/{relative}", target.prefix.trim_matches('/'))
}

fn entry_point_commands(meta: &Meta) -> Vec<String> {
    match meta.link.as_ref().map(|link| &link.noarch) {
        Some(NoArchLinks::Python(entry_points)) => entry_points
            .entry_points
            .iter()
            .map(|entry_point| entry_point.command.clone())
            .collect(),
        _ => Vec::new(),
    }
}

fn entry_point_path(target: &Target, command: &str) -> PathBuf {
    let python = target
        .python
        .as_ref()
        .expect("entry points only for noarch python, which needs python");
    forward_slashes(&python.bin_dir.join(command))
}

/// The package's files, in archive order, then its empty directories and entry points, then its record.
fn install_package(
    input: &Input<'_>,
    meta: &Meta,
    options: &Options<'_>,
    writer: &mut Writer,
    writes: &dyn Fn(&str) -> bool,
) -> Result<(), StreamError> {
    let name = &input.package.name;
    let target = options.target;
    let by_path: HashMap<String, &PathsEntry> = meta
        .paths
        .paths
        .iter()
        .map(|entry| (archive_key(&entry.relative_path), entry))
        .collect();
    let mut installed: Vec<InstalledPath> = Vec::with_capacity(by_path.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(by_path.len());

    let archive_err = |source| StreamError::Archive {
        name: name.clone(),
        archive: input.archive.clone(),
        source,
    };
    let file = File::open(&input.archive).map_err(archive_err)?;
    let is_conda = input.archive.extension().is_some_and(|extension| extension == "conda");
    if is_conda {
        let archive =
            rattler_package_streaming::seek::stream_conda_content(BufReader::new(file)).map_err(|source| {
                StreamError::Metadata {
                    name: name.clone(),
                    file: "pkg-*.tar.zst".to_string(),
                    source: Box::new(source),
                }
            })?;
        stream_entries(
            archive,
            input,
            meta,
            &by_path,
            options,
            writer,
            writes,
            &mut installed,
            &mut seen,
        )?;
    } else {
        let archive = rattler_package_streaming::read::stream_tar_bz2(BufReader::new(file));
        stream_entries(
            archive,
            input,
            meta,
            &by_path,
            options,
            writer,
            writes,
            &mut installed,
            &mut seen,
        )?;
    }

    for entry in &meta.paths.paths {
        if seen.contains(&archive_key(&entry.relative_path)) {
            continue;
        }
        let relative = install_path(meta, entry, target);
        let path = image_path(target, &relative);
        match entry.path_type {
            PathType::Directory => {
                if writes(&path) {
                    writer.dir(&path).map_err(StreamError::Write)?;
                }
                installed.push(record(entry, relative, None, None));
            }
            // rattler skips a symlink its extraction could not create; an archive always has it.
            _ => {
                return Err(StreamError::Missing {
                    name: name.clone(),
                    path: entry.relative_path.to_string_lossy().into_owned(),
                });
            }
        }
    }

    if let Some(NoArchLinks::Python(entry_points)) = meta.link.as_ref().map(|link| &link.noarch) {
        let python = target.python.as_ref().expect("checked when the metadata was read");
        for entry_point in &entry_points.entry_points {
            let relative = entry_point_path(target, &entry_point.command);
            let script = python_entry_point_template(&target.prefix, false, entry_point, python);
            let path = image_path(target, &relative);
            if writes(&path) {
                writer
                    .file(&path, true, script.len() as u64, script.as_bytes())
                    .map_err(StreamError::Write)?;
            }
            installed.push(InstalledPath {
                relative_path: relative,
                original_path: None,
                path_type: prefix_record::PathType::UnixPythonEntryPoint,
                no_link: false,
                sha256: Some(rattler_digest::compute_bytes_digest::<Sha256>(script.as_bytes())),
                sha256_in_prefix: None,
                size_in_bytes: Some(script.len() as u64),
                file_mode: None,
                prefix_placeholder: None,
            });
        }
    }

    installed.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let repodata: RepoDataRecord = input
        .package
        .record
        .as_deref()
        .cloned()
        .ok_or_else(|| StreamError::NoRecord { name: name.clone() })?;
    let prefix_record = PrefixRecord::from_repodata_record(repodata, installed);
    let mut json = Vec::new();
    prefix_record.write_to(&mut json, true).map_err(StreamError::Write)?;
    let path = image_path(target, &Path::new("conda-meta").join(prefix_record.file_name()));
    writer
        .file(&path, false, json.len() as u64, json.as_slice())
        .map_err(StreamError::Write)
}

/// A paths.json path as the archive's tar names it.
fn archive_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[allow(clippy::too_many_arguments)]
fn stream_entries<R: Read>(
    mut archive: tar::Archive<R>,
    input: &Input<'_>,
    meta: &Meta,
    by_path: &HashMap<String, &PathsEntry>,
    options: &Options<'_>,
    writer: &mut Writer,
    writes: &dyn Fn(&str) -> bool,
    installed: &mut Vec<InstalledPath>,
    seen: &mut HashSet<String>,
) -> Result<(), StreamError> {
    let name = &input.package.name;
    let target = options.target;
    let archive_err = |source| StreamError::Archive {
        name: name.clone(),
        archive: input.archive.clone(),
        source,
    };
    for entry in archive.entries().map_err(archive_err)? {
        let mut entry = entry.map_err(archive_err)?;
        let key = {
            let path = entry.path().map_err(archive_err)?;
            let key = archive_key(&path);
            key.strip_prefix("./").unwrap_or(&key).trim_end_matches('/').to_string()
        };
        // Only what paths.json lists is installed: `info/` in a .tar.bz2, and anything else, is not.
        let Some(paths_entry) = by_path.get(&key) else {
            continue;
        };
        seen.insert(key.clone());
        let relative = install_path(meta, paths_entry, target);
        let path = image_path(target, &relative);
        let header = entry.header().clone();
        match header.entry_type() {
            tar::EntryType::Symlink => {
                let link = entry
                    .link_name()
                    .map_err(archive_err)?
                    .map(|link| link.into_owned())
                    .unwrap_or_default();
                if writes(&path) {
                    writer.symlink(&path, &link).map_err(StreamError::Write)?;
                }
                // As rattler records it: paths.json's hash, and paths.json's size or else the link's own, which
                // a Unix file system reports as the length of its target.
                let size = paths_entry
                    .size_in_bytes
                    .unwrap_or(link.as_os_str().as_encoded_bytes().len() as u64);
                installed.push(record(paths_entry, relative, None, Some(size)));
            }
            tar::EntryType::Directory => {
                if writes(&path) {
                    writer.dir(&path).map_err(StreamError::Write)?;
                }
                installed.push(record(paths_entry, relative, None, None));
            }
            tar::EntryType::Link => {
                return Err(StreamError::HardLink {
                    name: name.clone(),
                    path: key,
                });
            }
            _ => {
                let executable = header.mode().map_err(archive_err)? & 0o111 != 0;
                let size = header.size().map_err(archive_err)?;
                if let Some(placeholder) = &paths_entry.prefix_placeholder {
                    let rewritten = rewrite(&mut entry, size, placeholder, options).map_err(archive_err)?;
                    let changed = Some(rewritten.sha256) != paths_entry.sha256;
                    if writes(&path) {
                        writer
                            .file(
                                &path,
                                executable,
                                rewritten.len,
                                rewritten.reader().map_err(StreamError::Write)?,
                            )
                            .map_err(StreamError::Write)?;
                    }
                    installed.push(InstalledPath {
                        file_mode: Some(placeholder.file_mode),
                        prefix_placeholder: Some(placeholder.placeholder.clone()),
                        ..record(
                            paths_entry,
                            relative,
                            changed.then_some(rewritten.sha256),
                            Some(rewritten.len),
                        )
                    });
                } else {
                    let sha256 = if writes(&path) {
                        let mut hashing = HashingReader::new(&mut entry);
                        writer
                            .file(&path, executable, size, &mut hashing)
                            .map_err(StreamError::Write)?;
                        hashing.digest()
                    } else {
                        let mut hashing = HashingReader::new(&mut entry);
                        io::copy(&mut hashing, &mut io::sink()).map_err(archive_err)?;
                        hashing.digest()
                    };
                    // rattler records sha256_in_prefix only where it differs from paths.json's hash, which for
                    // a copied file is only when paths.json has none.
                    let in_prefix = paths_entry.sha256.is_none().then_some(sha256);
                    installed.push(record(paths_entry, relative, in_prefix, Some(size)));
                }
            }
        }
    }
    Ok(())
}

/// The installed record of a path, as rattler writes it: paths.json's hash, and the installed one only where the
/// installed bytes differ.
fn record(
    entry: &PathsEntry,
    relative: PathBuf,
    sha256_in_prefix: Option<rattler_digest::Sha256Hash>,
    size_in_bytes: Option<u64>,
) -> InstalledPath {
    InstalledPath {
        relative_path: relative,
        original_path: None,
        path_type: entry.path_type.into(),
        no_link: entry.no_link,
        sha256: entry.sha256,
        sha256_in_prefix,
        size_in_bytes: size_in_bytes.or(entry.size_in_bytes),
        file_mode: None,
        prefix_placeholder: None,
    }
}

/// A rewritten file: in memory, or in a temporary file when it outgrew the spill limit.
struct Rewritten {
    content: Spill,
    len: u64,
    sha256: rattler_digest::Sha256Hash,
}

impl Rewritten {
    fn reader(&self) -> io::Result<Box<dyn Read + '_>> {
        self.content.reader()
    }
}

/// Replace the placeholder in `entry`, `size` bytes long.
///
/// A file up to the spill limit is rewritten whole in memory. A larger one waits in a temporary file and is
/// rewritten in pieces of at most [`PIECE`] bytes, each cut where no replacement can cross the cut, so memory stays
/// near the piece size however large the file is, and the bytes are the same as a whole-file rewrite:
///
/// - binary files: right after a NUL byte. The placeholder sits in a C string, which a NUL ends, and its
///   replacement (padded to the same length) never runs past that NUL.
/// - text files: right after a newline, as the placeholder has none, but not just before a line starting `#!`,
///   since rattler treats a piece that starts with `#!` as a script's first line.
fn rewrite(
    entry: &mut impl Read,
    size: u64,
    placeholder: &rattler_conda_types::package::PrefixPlaceholder,
    options: &Options<'_>,
) -> io::Result<Rewritten> {
    let replace = |bytes: &[u8], out: &mut SpillWriter<'_>| {
        copy_and_replace_placeholders(
            bytes,
            out,
            &placeholder.placeholder,
            &options.target.prefix,
            &options.target.platform,
            placeholder.file_mode,
        )
    };
    if size <= options.spill_limit as u64 {
        let mut bytes = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut bytes)?;
        let mut out = SpillWriter::new(options.spill_limit, options.spill_dir);
        replace(&bytes, &mut out)?;
        let out = out.finish()?;
        return Ok(Rewritten {
            content: out.content,
            len: out.len,
            sha256: out.sha256,
        });
    }

    let mut source = tempfile::Builder::new()
        .prefix(".spill-")
        .tempfile_in(options.spill_dir)?;
    io::copy(entry, source.as_file_mut())?;
    source.as_file_mut().flush()?;
    let mut reader = BufReader::new(source.reopen()?);
    // Straight to a file: the result is about as large as the source, which was past the limit.
    let mut out = SpillWriter::new(0, options.spill_dir);
    let binary = placeholder.file_mode == rattler_conda_types::package::FileMode::Binary;
    let mut piece: Vec<u8> = Vec::with_capacity(PIECE);
    loop {
        let before = piece.len();
        let want = (PIECE.max(before + 1)) - before;
        (&mut reader).take(want as u64).read_to_end(&mut piece)?;
        let at_end = piece.len() == before;
        if at_end {
            if !piece.is_empty() {
                replace(&piece, &mut out)?;
            }
            break;
        }
        let cut = if binary {
            piece.iter().rposition(|&b| b == 0).map(|i| i + 1)
        } else {
            text_cut(&piece)
        };
        match cut {
            Some(cut) if cut < piece.len() || binary => {
                replace(&piece[..cut], &mut out)?;
                piece.drain(..cut);
            }
            // No safe cut yet (a very long line, or no NUL): read more before rewriting.
            _ => {}
        }
    }
    let out = out.finish()?;
    Ok(Rewritten {
        content: out.content,
        len: out.len,
        sha256: out.sha256,
    })
}

/// The largest piece a large file is rewritten in.
const PIECE: usize = 4 * 1024 * 1024;

/// Where a text piece can be cut: after its last newline that is not followed by `#!` (the bytes after the cut
/// start the next piece, and rattler would read a piece starting `#!` as a shebang line).
fn text_cut(piece: &[u8]) -> Option<usize> {
    let mut end = piece.len();
    while let Some(newline) = piece[..end].iter().rposition(|&b| b == b'\n') {
        let next = &piece[newline + 1..];
        // Only a cut whose next bytes are known (two of them, or the end of what was read) can be judged.
        if next.len() >= 2 && !next.starts_with(b"#!") {
            return Some(newline + 1);
        }
        end = newline;
    }
    None
}

/// Bytes kept in memory until they pass a limit, then in a temporary file.
enum Spill {
    Memory(Vec<u8>),
    File(tempfile::NamedTempFile),
}

impl Spill {
    fn reader(&self) -> io::Result<Box<dyn Read + '_>> {
        match self {
            Self::Memory(bytes) => Ok(Box::new(bytes.as_slice())),
            Self::File(file) => Ok(Box::new(BufReader::new(file.reopen()?))),
        }
    }
}

struct Spilled {
    content: Spill,
    len: u64,
    sha256: rattler_digest::Sha256Hash,
}

/// A writer that keeps up to `limit` bytes in memory, then moves everything to a temporary file in `dir`.
struct SpillWriter<'a> {
    content: Spill,
    limit: usize,
    dir: &'a Path,
    len: u64,
    hasher: Sha256,
}

impl<'a> SpillWriter<'a> {
    fn new(limit: usize, dir: &'a Path) -> Self {
        Self {
            content: Spill::Memory(Vec::new()),
            limit,
            dir,
            len: 0,
            hasher: Sha256::new(),
        }
    }

    fn finish(mut self) -> io::Result<Spilled> {
        if let Spill::File(file) = &mut self.content {
            file.as_file_mut().flush()?;
        }
        Ok(Spilled {
            content: self.content,
            len: self.len,
            sha256: self.hasher.finalize(),
        })
    }
}

impl Write for SpillWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Spill::Memory(bytes) = &mut self.content
            && bytes.len() + buf.len() > self.limit
        {
            let mut file = tempfile::Builder::new().prefix(".spill-").tempfile_in(self.dir)?;
            file.as_file_mut().write_all(bytes)?;
            self.content = Spill::File(file);
        }
        match &mut self.content {
            Spill::Memory(bytes) => bytes.extend_from_slice(buf),
            Spill::File(file) => file.as_file_mut().write_all(buf)?,
        }
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that hashes what is read through it.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn digest(self) -> rattler_digest::Sha256Hash {
        self.hasher.finalize()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spills_happen_past_the_limit_and_keep_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut small = SpillWriter::new(8, dir.path());
        small.write_all(b"12345678").unwrap();
        let small = small.finish().unwrap();
        assert!(matches!(small.content, Spill::Memory(_)));

        let mut big = SpillWriter::new(8, dir.path());
        big.write_all(b"1234").unwrap();
        big.write_all(b"56789").unwrap();
        let big = big.finish().unwrap();
        assert!(matches!(big.content, Spill::File(_)));
        let mut read = Vec::new();
        big.content.reader().unwrap().read_to_end(&mut read).unwrap();
        assert_eq!(read, b"123456789");
        assert_eq!(big.len, 9);
        assert_eq!(big.sha256, rattler_digest::compute_bytes_digest::<Sha256>(b"123456789"));
    }

    #[test]
    fn only_plain_relative_paths_stay_inside_the_prefix() {
        for inside in ["bin/python", "lib/python3.13/site-packages/six.py", "a"] {
            assert!(inside_prefix(Path::new(inside)), "{inside}");
        }
        for outside in [
            "",
            "../etc/passwd",
            "bin/../../etc",
            "/etc/passwd",
            "./bin",
            "bin//x",
            "C:/x",
            "a\\..\\b",
        ] {
            assert!(!inside_prefix(Path::new(outside)), "{outside}");
        }
    }

    #[test]
    fn image_paths_use_forward_slashes_under_the_prefix() {
        let target = Target::new("/opt/env", "linux-64", None).unwrap();
        assert_eq!(image_path(&target, Path::new("bin/python")), "opt/env/bin/python");
        assert_eq!(image_path(&target, Path::new("lib\\x.so")), "opt/env/lib/x.so");
    }
}

#[cfg(test)]
mod cut_tests {
    use super::*;

    #[test]
    fn text_is_cut_after_a_newline_but_never_before_a_shebang() {
        assert_eq!(text_cut(b"one\ntwo\nthree"), Some(8));
        assert_eq!(text_cut(b"one\n#!two\nthree"), Some(10));
        assert_eq!(text_cut(b"one\n#!two"), None);
        assert_eq!(text_cut(b"no newline"), None);
        // The bytes after the last newline are not known yet: no cut there.
        assert_eq!(text_cut(b"one\ntwo\n"), Some(4));
    }
}
