//! Image layers: a directory tree as a gzip-compressed tar, byte for byte the same every time.
//!
//! An OCI layer is known by two digests: the sha256 of the compressed blob (what the manifest names and the
//! registry stores) and the sha256 of the uncompressed tar (the `diff_id` the image config lists). Both are computed
//! while the layer is written. Entries are in byte order of their path, owned by root, and stamped with one fixed
//! time, so the same tree gives the same digests on any machine.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::{EntryType, Header};

use crate::fetch::hex;

/// A written layer blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// `sha256:<hex>` of the compressed bytes.
    pub digest: String,
    /// `sha256:<hex>` of the uncompressed tar.
    pub diff_id: String,
    /// Compressed size in bytes.
    pub size: u64,
}

/// A writer that hashes and counts what passes through it.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    count: u64,
}

impl<W: Write> Hashing<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            count: 0,
        }
    }

    fn finish(self) -> (W, String, u64) {
        (self.inner, hex(&self.hasher.finalize()), self.count)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.count += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// One entry of a layer: where it is in the image, and where its content is on disk.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path inside the image, relative to `/`, with forward slashes and no trailing slash.
    pub path: String,
    /// The file, directory or symlink on disk.
    pub source: PathBuf,
}

/// Every entry under `root`, named by their path relative to it, in byte order. `skip` is consulted with each
/// relative path; a skipped directory is skipped with everything under it.
pub fn entries(root: &Path, skip: &dyn Fn(&str) -> bool) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    walk(root, "", skip, &mut out)?;
    out.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, skip: &dyn Fn(&str) -> bool, out: &mut Vec<Entry>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{:?} is not UTF-8", entry.path())))?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if skip(&path) {
            continue;
        }
        let file_type = entry.file_type()?;
        out.push(Entry {
            path: path.clone(),
            source: entry.path(),
        });
        if file_type.is_dir() {
            walk(&entry.path(), &path, skip, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
/// Write `entries` as a gzip-compressed tar blob into `blobs_dir`, named by its digest, with every entry's
/// modification time set to `mtime` (seconds since the Unix epoch).
pub fn write(entries: &[Entry], blobs_dir: &Path, mtime: u64) -> io::Result<Blob> {
    let mut writer = Writer::new(blobs_dir, mtime)?;
    for entry in entries {
        let metadata = fs::symlink_metadata(&entry.source)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            writer.symlink(&entry.path, &fs::read_link(&entry.source)?)?;
        } else if file_type.is_dir() {
            writer.dir(&entry.path)?;
        } else {
            writer.file(
                &entry.path,
                executable(&metadata),
                metadata.len(),
                File::open(&entry.source)?,
            )?;
        }
    }
    writer.finish()
}

type Stack = tar::Builder<Hashing<flate2::write::GzEncoder<Hashing<io::BufWriter<File>>>>>;

/// A layer written one entry at a time, straight into its compressed blob.
///
/// Every entry is owned by root, stamped with one time, and given one of three modes. A directory is written once,
/// the first time it is named or needed as a parent, so entries can arrive in any order a caller fixes.
pub struct Writer {
    tar: Stack,
    temp: tempfile::NamedTempFile,
    blobs_dir: PathBuf,
    mtime: u64,
    dirs: std::collections::HashSet<String>,
}

impl Writer {
    /// A layer that will be written into `blobs_dir`, every entry stamped with `mtime`.
    pub fn new(blobs_dir: &Path, mtime: u64) -> io::Result<Self> {
        fs::create_dir_all(blobs_dir)?;
        let temp = tempfile::Builder::new().prefix(".layer-").tempfile_in(blobs_dir)?;
        let file = Hashing::new(io::BufWriter::new(temp.reopen()?));
        // No file name and a zero time in the gzip header, so the compressed bytes depend on the content only.
        let gzip = flate2::GzBuilder::new().mtime(0).write(file, Compression::default());
        let mut tar = tar::Builder::new(Hashing::new(gzip));
        tar.follow_symlinks(false);
        Ok(Self {
            tar,
            temp,
            blobs_dir: blobs_dir.to_path_buf(),
            mtime,
            dirs: std::collections::HashSet::new(),
        })
    }

    fn header(&self, kind: EntryType, mode: u32, size: u64) -> Header {
        let mut header = Header::new_gnu();
        header.set_mtime(self.mtime);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mode(mode);
        header.set_entry_type(kind);
        header.set_size(size);
        header
    }

    /// Write the directories above `path` that are not written yet, outermost first.
    pub fn parents(&mut self, path: &str) -> io::Result<()> {
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash;
            let parent = path[..end].to_string();
            self.dir(&parent)?;
            end += 1;
        }
        Ok(())
    }

    /// A directory, once.
    pub fn dir(&mut self, path: &str) -> io::Result<()> {
        let path = path.trim_end_matches('/');
        if self.dirs.contains(path) {
            return Ok(());
        }
        self.parents(path)?;
        let mut header = self.header(EntryType::Directory, 0o755, 0);
        self.tar.append_data(&mut header, format!("{path}/"), io::empty())?;
        self.dirs.insert(path.to_string());
        Ok(())
    }

    /// A regular file of `size` bytes read from `content`.
    pub fn file(&mut self, path: &str, executable: bool, size: u64, content: impl io::Read) -> io::Result<()> {
        self.parents(path)?;
        let mode = if executable { 0o755 } else { 0o644 };
        let mut header = self.header(EntryType::Regular, mode, size);
        self.tar.append_data(&mut header, path, content)
    }

    /// A symbolic link to `target`, which is written as given.
    pub fn symlink(&mut self, path: &str, target: &Path) -> io::Result<()> {
        self.parents(path)?;
        let mut header = self.header(EntryType::Symlink, 0o755, 0);
        self.tar.append_link(&mut header, path, target)
    }

    /// Finish the tar and the gzip stream and move the blob to its digest's name.
    pub fn finish(self) -> io::Result<Blob> {
        let tarred = self.tar.into_inner()?;
        let (gzip, diff_id, _) = tarred.finish();
        let file = gzip.finish()?;
        let (mut buffered, digest, size) = file.finish();
        buffered.flush()?;
        drop(buffered);
        self.temp
            .persist(self.blobs_dir.join(&digest))
            .map_err(|err| err.error)?;
        Ok(Blob {
            digest: format!("sha256:{digest}"),
            diff_id: format!("sha256:{diff_id}"),
            size,
        })
    }
}

/// Whether a file on disk is executable. Only this is kept of its mode: the rest depends on the umask it was
/// created under, which must not reach the image or its digest.
#[cfg(all(test, unix))]
fn executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(all(test, not(unix)))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("opt/env/bin")).unwrap();
        fs::write(dir.path().join("opt/env/bin/tool"), b"#!/bin/sh\necho hi\n").unwrap();
        fs::write(dir.path().join("opt/env/README"), b"readme").unwrap();
        fs::write(dir.path().join("CACHEDIR.TAG"), b"skip me").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join("opt/env/bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("tool", dir.path().join("opt/env/bin/alias")).unwrap();
        }
        dir
    }

    fn read_back(blobs: &Path, blob: &Blob) -> Vec<(String, EntryType, u64, u32, Option<String>)> {
        let file = File::open(blobs.join(blob.digest.trim_start_matches("sha256:"))).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        archive
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let header = entry.header();
                (
                    entry.path().unwrap().to_string_lossy().into_owned(),
                    header.entry_type(),
                    header.mtime().unwrap(),
                    header.mode().unwrap(),
                    entry
                        .link_name()
                        .unwrap()
                        .map(|link| link.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn entries_are_sorted_and_skips_apply() {
        let dir = tree();
        let found = entries(dir.path(), &|path| path == "CACHEDIR.TAG").unwrap();
        let paths: Vec<&str> = found.iter().map(|e| e.path.as_str()).collect();
        let mut expected = vec!["opt", "opt/env", "opt/env/README", "opt/env/bin", "opt/env/bin/tool"];
        if cfg!(unix) {
            expected.insert(4, "opt/env/bin/alias");
        }
        assert_eq!(paths, expected);
    }

    #[test]
    fn a_skipped_directory_takes_its_contents_with_it() {
        let dir = tree();
        let found = entries(dir.path(), &|path| path == "opt/env/bin").unwrap();
        assert!(found.iter().all(|e| !e.path.starts_with("opt/env/bin")));
    }

    #[test]
    fn the_same_tree_gives_the_same_blob() {
        let dir = tree();
        let found = entries(dir.path(), &|_| false).unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let first = write(&found, blobs.path(), 0).unwrap();
        // Touch every file: only content, names and modes may decide the digest.
        for entry in &found {
            if entry.source.is_file() {
                let content = fs::read(&entry.source).unwrap();
                fs::write(&entry.source, content).unwrap();
            }
        }
        let second = write(&found, blobs.path(), 0).unwrap();
        assert_eq!(first, second);
        assert!(first.digest.starts_with("sha256:") && first.digest.len() == 71);
        assert_ne!(first.digest, first.diff_id);
        let on_disk = fs::metadata(blobs.path().join(first.digest.trim_start_matches("sha256:"))).unwrap();
        assert_eq!(on_disk.len(), first.size);
        // No temporary files are left beside the blob.
        assert_eq!(fs::read_dir(blobs.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_diff_id_is_the_hash_of_the_uncompressed_tar() {
        let dir = tree();
        let found = entries(dir.path(), &|_| false).unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let blob = write(&found, blobs.path(), 0).unwrap();
        let file = File::open(blobs.path().join(blob.digest.trim_start_matches("sha256:"))).unwrap();
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(file).read_to_end(&mut tar).unwrap();
        assert_eq!(blob.diff_id, format!("sha256:{}", hex(&Sha256::digest(&tar))));
    }

    #[test]
    fn entries_are_owned_by_root_at_the_given_time() {
        let dir = tree();
        let found = entries(dir.path(), &|path| path == "CACHEDIR.TAG").unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let blob = write(&found, blobs.path(), 1_700_000_000).unwrap();
        let read = read_back(blobs.path(), &blob);
        assert!(read.iter().all(|(_, _, mtime, _, _)| *mtime == 1_700_000_000));
        let dirs: Vec<&str> = read
            .iter()
            .filter(|(_, kind, ..)| *kind == EntryType::Directory)
            .map(|(path, ..)| path.as_str())
            .collect();
        assert_eq!(dirs, ["opt/", "opt/env/", "opt/env/bin/"]);
        // Directories and executables 0755, everything else 0644, whatever the modes on disk.
        assert!(
            read.iter()
                .filter(|(_, kind, ..)| *kind == EntryType::Directory)
                .all(|entry| entry.3 == 0o755)
        );
        let readme = read.iter().find(|(path, ..)| path == "opt/env/README").unwrap();
        assert_eq!(readme.3, 0o644);
        if cfg!(unix) {
            let tool = read.iter().find(|(path, ..)| path == "opt/env/bin/tool").unwrap();
            assert_eq!(tool.3, 0o755);
            let alias = read.iter().find(|(path, ..)| path == "opt/env/bin/alias").unwrap();
            assert_eq!(alias.1, EntryType::Symlink);
            assert_eq!(alias.4.as_deref(), Some("tool"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_umask_does_not_reach_the_layer() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree();
        let blobs = tempfile::tempdir().unwrap();
        let open = write(&entries(dir.path(), &|_| false).unwrap(), blobs.path(), 0).unwrap();
        // As a user with umask 077 would have created them: directories and files closed to everyone else.
        for (path, mode) in [
            ("opt", 0o700),
            ("opt/env", 0o700),
            ("opt/env/bin", 0o700),
            ("opt/env/README", 0o600),
            ("opt/env/bin/tool", 0o700),
        ] {
            fs::set_permissions(dir.path().join(path), fs::Permissions::from_mode(mode)).unwrap();
        }
        let closed = write(&entries(dir.path(), &|_| false).unwrap(), blobs.path(), 0).unwrap();
        assert_eq!(open, closed);
    }

    #[test]
    fn long_paths_survive() {
        let dir = tempfile::tempdir().unwrap();
        let deep = "d".repeat(60);
        let path = dir.path().join(&deep).join(&deep).join("f".repeat(80));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"x").unwrap();
        let found = entries(dir.path(), &|_| false).unwrap();
        let blobs = tempfile::tempdir().unwrap();
        let blob = write(&found, blobs.path(), 0).unwrap();
        let read = read_back(blobs.path(), &blob);
        let longest = read.iter().map(|(path, ..)| path.len()).max().unwrap();
        assert_eq!(longest, 60 + 1 + 60 + 1 + 80);
    }
}
