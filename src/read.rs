//! An image on disk, read back: an OCI image layout directory, or a `.tar` archive of one read in place by offset,
//! never unpacked.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The most a JSON document of an image (index, manifest, config) is read into memory; anything larger is refused
/// rather than read. Real ones are a few KiB.
pub const JSON_LIMIT: u64 = 4 << 20;

/// Why an image could not be opened at all.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum ReadError {
    #[error("{} does not exist", .path.display())]
    Missing { path: PathBuf },

    #[error("{} is neither an OCI image layout directory nor a .tar archive of one", .path.display())]
    #[diagnostic(help("pass the directory or the .tar file `pixi oci build --output` wrote"))]
    NotAnImage {
        path: PathBuf,
        #[source]
        source: Option<io::Error>,
    },
}

/// An image layout, as a directory or as an archive.
#[derive(Debug)]
pub enum Image {
    Layout(PathBuf),
    /// Each regular file of the archive by its path, as (offset of its data, size).
    Archive {
        path: PathBuf,
        entries: BTreeMap<String, (u64, u64)>,
    },
}

impl Image {
    /// Open `path`: a directory is a layout; a file is read as a tar archive.
    pub fn open(path: &Path) -> Result<Image, ReadError> {
        let metadata = fs::metadata(path).map_err(|_| ReadError::Missing {
            path: path.to_path_buf(),
        })?;
        if metadata.is_dir() {
            return Ok(Image::Layout(path.to_path_buf()));
        }
        let not_an_image = |source: Option<io::Error>| ReadError::NotAnImage {
            path: path.to_path_buf(),
            source,
        };
        let file = fs::File::open(path).map_err(|err| not_an_image(Some(err)))?;
        let mut archive = tar::Archive::new(io::BufReader::new(file));
        let mut entries = BTreeMap::new();
        for entry in archive.entries().map_err(|err| not_an_image(Some(err)))? {
            let entry = entry.map_err(|err| not_an_image(Some(err)))?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let name = entry.path().map_err(|err| not_an_image(Some(err)))?;
            let name = name.to_string_lossy().trim_start_matches("./").to_string();
            entries.insert(name, (entry.raw_file_position(), entry.size()));
        }
        if entries.is_empty() {
            return Err(not_an_image(None));
        }
        Ok(Image::Archive {
            path: path.to_path_buf(),
            entries,
        })
    }

    /// Whether the image holds the file `name`, a path relative to the layout's root.
    pub fn has(&self, name: &str) -> bool {
        if !plain(name) {
            return false;
        }
        match self {
            Image::Layout(root) => root.join(name).is_file(),
            Image::Archive { entries, .. } => entries.contains_key(name),
        }
    }

    /// The size of the file `name`, if the image holds it.
    pub fn size(&self, name: &str) -> Option<u64> {
        if !plain(name) {
            return None;
        }
        match self {
            Image::Layout(root) => fs::metadata(root.join(name))
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len()),
            Image::Archive { entries, .. } => entries.get(name).map(|(_, size)| *size),
        }
    }

    /// A reader over the file `name`.
    pub fn reader(&self, name: &str) -> io::Result<Box<dyn Read>> {
        if !plain(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name} is not a path inside the image"),
            ));
        }
        match self {
            Image::Layout(root) => Ok(Box::new(io::BufReader::new(fs::File::open(root.join(name))?))),
            Image::Archive { path, entries } => {
                let (offset, size) = entries
                    .get(name)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} is not in the archive")))?;
                let mut file = fs::File::open(path)?;
                file.seek(SeekFrom::Start(*offset))?;
                Ok(Box::new(io::BufReader::new(file.take(*size))))
            }
        }
    }

    /// The whole file `name`, refused past [`JSON_LIMIT`].
    pub fn read_small(&self, name: &str) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.reader(name)?.take(JSON_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > JSON_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{name} is larger than {JSON_LIMIT} bytes"),
            ));
        }
        Ok(bytes)
    }
}

/// The path inside the layout of the blob with `digest`, if it is a well-formed sha256 digest.
pub fn blob_path(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| format!("blobs/sha256/{hex}"))
}

/// A relative path with no `..`, `.` or empty parts, so it cannot leave the layout.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_name_blob_paths_only_when_well_formed() {
        let hex = "a".repeat(64);
        assert_eq!(blob_path(&format!("sha256:{hex}")), Some(format!("blobs/sha256/{hex}")));
        assert_eq!(blob_path(&format!("sha512:{hex}")), None);
        assert_eq!(blob_path(&format!("sha256:{}", "A".repeat(64))), None);
        assert_eq!(blob_path("sha256:abc"), None);
        assert_eq!(blob_path(&format!("sha256:../{}", "a".repeat(61))), None);
    }

    #[test]
    fn paths_cannot_leave_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("index.json"), b"{}").unwrap();
        let image = Image::open(dir.path()).unwrap();
        assert!(image.has("index.json"));
        assert_eq!(image.size("index.json"), Some(2));
        for name in ["../index.json", "./index.json", "", "a//b", "a\\b", "/etc/passwd"] {
            assert!(!image.has(name), "{name}");
            assert!(image.size(name).is_none(), "{name}");
            assert!(image.reader(name).is_err(), "{name}");
        }
    }

    #[test]
    fn an_archive_is_read_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.tar");
        let mut builder = tar::Builder::new(fs::File::create(&path).unwrap());
        for (name, data) in [
            ("oci-layout", &b"{\"imageLayoutVersion\":\"1.0.0\"}"[..]),
            ("./index.json", b"{}"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, name, data).unwrap();
        }
        builder.finish().unwrap();
        drop(builder);
        let image = Image::open(&path).unwrap();
        assert!(matches!(image, Image::Archive { .. }));
        assert_eq!(image.read_small("index.json").unwrap(), b"{}");
        assert_eq!(image.size("oci-layout"), Some(30));
        assert!(image.reader("missing.json").is_err());
    }

    #[test]
    fn other_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = Image::open(&dir.path().join("nope")).unwrap_err();
        assert!(matches!(err, ReadError::Missing { .. }));
        let text = dir.path().join("notes.txt");
        fs::write(&text, b"hello, this is not a tar archive at all").unwrap();
        assert!(matches!(Image::open(&text).unwrap_err(), ReadError::NotAnImage { .. }));
        let empty = dir.path().join("empty.tar");
        tar::Builder::new(fs::File::create(&empty).unwrap()).finish().unwrap();
        assert!(matches!(Image::open(&empty).unwrap_err(), ReadError::NotAnImage { .. }));
    }

    #[test]
    fn large_documents_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.json"), vec![b' '; JSON_LIMIT as usize + 1]).unwrap();
        let image = Image::open(dir.path()).unwrap();
        assert!(image.read_small("big.json").is_err());
    }
}
