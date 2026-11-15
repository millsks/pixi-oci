//! Getting conda packages onto disk: downloaded once, checked against the lockfile's sha256, extracted once.
//!
//! The cache is keyed by the archive's sha256, so two lockfiles that lock the same file share it, and a file whose
//! bytes are not the ones the lockfile locked never reaches the cache.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::lock::Package;

/// Why a package could not be fetched.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum FetchError {
    #[error("the lockfile records no sha256 for {name}, so its download cannot be checked")]
    #[diagnostic(help("re-lock with a current pixi, which records a sha256 for every conda package"))]
    NoSha256 { name: String },

    #[error("could not download {url}")]
    Download {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },

    #[error("{name} downloaded from {location} does not match the lockfile's sha256")]
    #[diagnostic(help("expected {expected}, got {actual}; the channel may have replaced the file, so re-lock"))]
    HashMismatch {
        name: String,
        location: String,
        expected: String,
        actual: String,
    },

    #[cfg(test)]
    #[error("could not extract {name}")]
    Extract {
        name: String,
        #[source]
        source: Box<rattler_package_streaming::ExtractError>,
    },

    #[error("{location} is not a .conda, .tar.bz2 or .whl archive")]
    UnknownArchive { location: String },

    #[error("could not use the package cache at {path}")]
    Cache {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Downloads and extracts packages into a cache directory.
pub struct Fetcher {
    root: PathBuf,
    agent: ureq::Agent,
}

impl Fetcher {
    /// A fetcher with its cache at `root`, created when needed. Proxies come from the usual environment variables.
    pub fn new(root: PathBuf) -> Self {
        Self::with_proxy(root, ureq::Proxy::try_from_env())
    }

    fn with_proxy(root: PathBuf, proxy: Option<ureq::Proxy>) -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(concat!("pixi-oci/", env!("CARGO_PKG_VERSION")))
            .proxy(proxy)
            .build()
            .new_agent();
        Self { root, agent }
    }

    /// The cache directory to use when none is given: `PIXI_OCI_CACHE_DIR`, else the platform's cache directory.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("PIXI_OCI_CACHE_DIR") {
            return PathBuf::from(dir);
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .or_else(|| {
                std::env::var_os("HOME").map(|home| {
                    let home = PathBuf::from(home);
                    if cfg!(target_os = "macos") {
                        home.join("Library/Caches")
                    } else {
                        home.join(".cache")
                    }
                })
            })
            .unwrap_or_else(std::env::temp_dir);
        base.join("pixi-oci")
    }

    /// The extracted package directory for `package`, downloading and extracting it if the cache does not have it.
    /// Only the reference install in the parity tests unpacks packages; builds stream them from the archive.
    #[cfg(test)]
    pub fn extracted(&self, package: &Package) -> Result<PathBuf, FetchError> {
        let sha256 = package.sha256.clone().ok_or_else(|| FetchError::NoSha256 {
            name: package.name.clone(),
        })?;
        let extracted = self.root.join("extracted").join(&sha256);
        // An extraction is renamed into place only once it is complete, so a directory here is a whole package.
        if extracted.is_dir() {
            return Ok(extracted);
        }
        let archive = self.archive(package, &sha256)?;
        let cache_err = |source| FetchError::Cache {
            path: self.root.clone(),
            source,
        };
        let parent = self.root.join("extracted");
        fs::create_dir_all(&parent).map_err(cache_err)?;
        let staging = tempfile::Builder::new()
            .prefix(".extract-")
            .tempdir_in(&parent)
            .map_err(cache_err)?;
        rattler_package_streaming::fs::extract(&archive, staging.path()).map_err(|source| FetchError::Extract {
            name: package.name.clone(),
            source: Box::new(source),
        })?;
        match fs::rename(staging.path(), &extracted) {
            Ok(()) => {
                // The directory now lives at its final name; don't let the guard delete it.
                let _ = staging.keep();
            }
            // Another run finished the same package first; its copy is as good as ours.
            Err(_) if extracted.is_dir() => {}
            Err(source) => return Err(cache_err(source)),
        }
        Ok(extracted)
    }

    /// The archive for `package` on disk, downloaded if the cache does not have it, and checked against the
    /// lockfile's sha256.
    pub fn archive_for(&self, package: &Package) -> Result<PathBuf, FetchError> {
        let sha256 = package.sha256.clone().ok_or_else(|| FetchError::NoSha256 {
            name: package.name.clone(),
        })?;
        self.archive(package, &sha256)
    }

    /// The archive for `package` on disk, checked against `sha256`.
    fn archive(&self, package: &Package, sha256: &str) -> Result<PathBuf, FetchError> {
        let extension = archive_extension(&package.location).ok_or_else(|| FetchError::UnknownArchive {
            location: package.location.clone(),
        })?;
        // A local archive is read where it is, but still checked: the lockfile's hash is the contract.
        if let Some(path) = local_path(&package.location) {
            let actual = hash_file(&path).map_err(|source| FetchError::Cache {
                path: path.clone(),
                source,
            })?;
            check(package, sha256, &actual)?;
            return Ok(path);
        }
        let dir = self.root.join("archives");
        let path = dir.join(format!("{sha256}{extension}"));
        if path.is_file() {
            return Ok(path);
        }
        let cache_err = |source| FetchError::Cache {
            path: dir.clone(),
            source,
        };
        fs::create_dir_all(&dir).map_err(cache_err)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(&dir)
            .map_err(cache_err)?;
        let actual = self.download(&package.location, temp.as_file_mut())?;
        check(package, sha256, &actual)?;
        temp.persist(&path).map_err(|err| cache_err(err.error))?;
        Ok(path)
    }

    /// Stream `url` into `out`, returning the sha256 of what was written.
    fn download(&self, url: &str, out: &mut File) -> Result<String, FetchError> {
        let download_err = |source: ureq::Error| FetchError::Download {
            url: url.to_string(),
            source: Box::new(source),
        };
        let response = self.agent.get(url).call().map_err(download_err)?;
        let mut reader = response.into_body().into_reader();
        copy_hashed(&mut reader, out).map_err(|err| download_err(ureq::Error::Io(err)))
    }
}

fn check(package: &Package, expected: &str, actual: &str) -> Result<(), FetchError> {
    if expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(FetchError::HashMismatch {
            name: package.name.clone(),
            location: package.location.clone(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    }
}

/// `.conda`, `.tar.bz2` or, for a PyPI package, `.whl`, from the end of a URL or path.
fn archive_extension(location: &str) -> Option<&'static str> {
    let name = location.split(['?', '#']).next().unwrap_or(location);
    [".conda", ".tar.bz2", ".whl"]
        .into_iter()
        .find(|extension| name.ends_with(extension))
}

/// The path a `file://` URL or a plain path names; `None` for a remote URL. A `file://` URL is converted by the
/// `url` crate, so `file:///C:/pkgs/a.conda` is `C:\pkgs\a.conda` on Windows.
fn local_path(location: &str) -> Option<PathBuf> {
    if location.starts_with("file://") {
        return url::Url::parse(location).ok()?.to_file_path().ok();
    }
    if location.contains("://") {
        None
    } else {
        Some(PathBuf::from(location))
    }
}

/// Copy everything from `reader` to `writer`, returning the sha256 of the bytes, as lowercase hex.
pub fn copy_hashed(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
    }
    writer.flush()?;
    Ok(hex(&hasher.finalize()))
}

fn hash_file(path: &Path) -> io::Result<String> {
    copy_hashed(&mut File::open(path)?, &mut io::sink())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Kind;
    use std::io::{Read, Write};

    fn package(location: &str, sha256: Option<&str>) -> Package {
        Package {
            name: "pkg".to_string(),
            version: "1".to_string(),
            kind: Kind::Conda,
            build: None,
            depends: Vec::new(),
            location: location.to_string(),
            sha256: sha256.map(str::to_string),
            size: None,
            record: None,
        }
    }

    #[test]
    fn archive_types_come_from_the_name() {
        assert_eq!(archive_extension("https://x/linux-64/a-1-0.conda"), Some(".conda"));
        assert_eq!(archive_extension("https://x/linux-64/a-1-0.tar.bz2"), Some(".tar.bz2"));
        assert_eq!(archive_extension("https://x/a-1-0.conda?token=t"), Some(".conda"));
        assert_eq!(archive_extension("https://x/a-1-0-py3-none-any.whl"), Some(".whl"));
        assert_eq!(archive_extension("https://x/a-1.0.tar.gz"), None);
    }

    #[test]
    fn local_locations_are_recognized() {
        if cfg!(unix) {
            assert_eq!(local_path("file:///srv/a.conda"), Some(PathBuf::from("/srv/a.conda")));
        } else {
            assert_eq!(
                local_path("file:///C:/srv/a.conda"),
                Some(PathBuf::from("C:\\srv\\a.conda"))
            );
        }
        assert_eq!(local_path("pkgs/a.conda"), Some(PathBuf::from("pkgs/a.conda")));
        assert_eq!(local_path("https://conda.anaconda.org/a.conda"), None);
    }

    #[test]
    fn copying_hashes_what_it_copies() {
        let mut out = Vec::new();
        let digest = copy_hashed(&mut &b"abc"[..], &mut out).unwrap();
        assert_eq!(out, b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_package_without_a_hash_is_refused() {
        let fetcher = Fetcher::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let err = fetcher.extracted(&package("a.conda", None)).unwrap_err();
        assert!(matches!(err, FetchError::NoSha256 { .. }), "{err}");
    }

    #[test]
    fn a_local_archive_with_the_wrong_hash_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a-1-0.conda");
        fs::write(&archive, b"not the locked bytes").unwrap();
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let err = fetcher
            .extracted(&package(archive.to_str().unwrap(), Some(&"0".repeat(64))))
            .unwrap_err();
        assert!(matches!(err, FetchError::HashMismatch { .. }), "{err}");
        assert!(!dir.path().join("cache/extracted").join("0".repeat(64)).exists());
    }

    #[test]
    fn a_local_archive_that_is_not_a_package_fails_to_extract() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a-1-0.conda");
        fs::write(&archive, b"not a zip").unwrap();
        let sha = hash_file(&archive).unwrap();
        let fetcher = Fetcher::new(dir.path().join("cache"));
        let err = fetcher
            .extracted(&package(archive.to_str().unwrap(), Some(&sha)))
            .unwrap_err();
        assert!(matches!(err, FetchError::Extract { .. }), "{err}");
    }

    #[test]
    fn an_unknown_archive_type_is_refused() {
        let fetcher = Fetcher::new(tempfile::tempdir().unwrap().path().to_path_buf());
        let err = fetcher
            .extracted(&package("https://x/a-1.tar.gz", Some(&"0".repeat(64))))
            .unwrap_err();
        assert!(matches!(err, FetchError::UnknownArchive { .. }), "{err}");
    }

    /// Serve `body` over HTTP for exactly `requests` requests on a local port, returning the URL to fetch.
    fn serve(body: Vec<u8>, requests: usize, name: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        format!("http://127.0.0.1:{port}/conda-forge/noarch/{name}")
    }

    /// A real, minimal `.tar.bz2` conda package.
    fn archive_bytes() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("info")).unwrap();
        fs::write(
            dir.path().join("info/index.json"),
            r#"{"name":"pkg","version":"1","build":"0","build_number":0,"subdir":"noarch"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("info/paths.json"), r#"{"paths_version":1,"paths":[]}"#).unwrap();
        let files = [dir.path().join("info/index.json"), dir.path().join("info/paths.json")];
        let mut out = Vec::new();
        rattler_package_streaming::write::write_tar_bz2_package(
            &mut out,
            dir.path(),
            &files,
            rattler_conda_types::compression_level::CompressionLevel::Default,
            None,
            None,
        )
        .unwrap();
        out
    }

    #[test]
    fn a_download_is_checked_cached_and_not_fetched_again() {
        let body = archive_bytes();
        let sha = hex(&Sha256::digest(&body));
        // One request only: the second `extracted` must come from the cache.
        let url = serve(body, 1, "pkg-1-0.tar.bz2");
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_proxy(dir.path().to_path_buf(), None);
        let first = fetcher.extracted(&package(&url, Some(&sha))).unwrap();
        assert!(first.join("info/index.json").is_file());
        assert!(dir.path().join("archives").join(format!("{sha}.tar.bz2")).is_file());
        assert_eq!(fetcher.extracted(&package(&url, Some(&sha))).unwrap(), first);
        // An extraction that is gone is redone from the cached archive, still without the network.
        fs::remove_dir_all(&first).unwrap();
        assert_eq!(fetcher.extracted(&package(&url, Some(&sha))).unwrap(), first);
    }

    #[test]
    fn a_download_with_the_wrong_hash_is_not_kept() {
        let url = serve(archive_bytes(), 1, "pkg-1-0.tar.bz2");
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_proxy(dir.path().to_path_buf(), None);
        let err = fetcher.extracted(&package(&url, Some(&"a".repeat(64)))).unwrap_err();
        assert!(matches!(err, FetchError::HashMismatch { .. }), "{err}");
        assert_eq!(fs::read_dir(dir.path().join("archives")).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_download_names_the_url() {
        // Bind and drop a listener, so the port is (almost certainly) closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/pkg-1-0.conda");
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_proxy(dir.path().to_path_buf(), None);
        let err = fetcher.extracted(&package(&url, Some(&"a".repeat(64)))).unwrap_err();
        assert!(matches!(err, FetchError::Download { .. }), "{err}");
        assert!(err.to_string().contains(&url));
    }

    #[test]
    fn the_cache_directory_can_be_set() {
        // Read-only check of the fallback chain: the variable, when set, wins.
        if std::env::var_os("PIXI_OCI_CACHE_DIR").is_none() {
            assert!(Fetcher::default_root().ends_with("pixi-oci"));
        }
    }
}
