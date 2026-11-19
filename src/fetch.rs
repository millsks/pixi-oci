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

    #[error("{url} refused the download ({status})")]
    #[diagnostic(help(
        "{}",
        if *.had_credentials {
            format!("the credentials pixi has for {host} were refused; log in again with `pixi auth login {host} --token <token>` (or --username and --password)")
        } else {
            format!("the channel needs credentials: log in with `pixi auth login {host} --token <token>` (or --username and --password), as pixi itself does to lock it")
        }
    ))]
    Unauthorized {
        url: String,
        host: String,
        status: u16,
        had_credentials: bool,
    },

    #[error("could not download {url}: {message}")]
    #[diagnostic(help(
        "s3:// downloads are signed with pixi's S3 credentials (`pixi auth login s3://<bucket> --s3-access-key-id ...`) \
        or the AWS environment, gcs:// with Google's application default credentials, and oci:// with pixi's \
        credentials for the registry"
    ))]
    Remote { url: String, message: String },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] crate::sources::ConfigError),

    #[error("could not read pixi's credentials: {message}")]
    Credentials { message: String },

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
    /// pixi's credentials, looked up for each download's host; none in tests unless given.
    credentials: Option<rattler_networking::AuthenticationStorage>,
    /// pixi's mirrors and S3 settings; none in tests unless given.
    sources: crate::sources::Sources,
    /// The client for s3://, gcs:// and oci:// downloads, made when one is first needed.
    remote: std::sync::OnceLock<crate::sources::Remote>,
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
            .tls_config(crate::tls::config())
            // A redirect to another host, such as a channel's storage, is not given the channel's credentials.
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            .build()
            .new_agent();
        Self {
            root,
            agent,
            credentials: None,
            sources: crate::sources::Sources::default(),
            remote: std::sync::OnceLock::new(),
        }
    }

    /// The fetcher with pixi's mirrors and S3 settings, from pixi's config files and the workspace's
    /// `.pixi/config.toml` in `workspace`.
    pub fn with_pixi_config(self, workspace: Option<&Path>) -> Result<Self, FetchError> {
        Ok(self.with_sources(crate::sources::Sources::from_pixi_config(workspace)?))
    }

    pub fn with_sources(mut self, sources: crate::sources::Sources) -> Self {
        self.sources = sources;
        self.remote = std::sync::OnceLock::new();
        self
    }

    fn remote(&self) -> &crate::sources::Remote {
        self.remote.get_or_init(|| {
            crate::sources::Remote::new(
                self.sources.clone(),
                self.credentials
                    .clone()
                    .unwrap_or_else(rattler_networking::AuthenticationStorage::empty),
            )
        })
    }

    /// The fetcher with pixi's credentials, as `pixi auth login` keeps them: the keyring, `RATTLER_AUTH_FILE`,
    /// `~/.rattler/credentials.json` and `~/.netrc`, looked up as pixi looks them up for each download's host.
    pub fn with_pixi_credentials(self) -> Result<Self, FetchError> {
        let storage = rattler_networking::AuthenticationStorage::from_env_and_defaults().map_err(|err| {
            FetchError::Credentials {
                message: err.to_string(),
            }
        })?;
        Ok(self.with_credentials(storage))
    }

    pub fn with_credentials(mut self, storage: rattler_networking::AuthenticationStorage) -> Self {
        self.credentials = Some(storage);
        self.remote = std::sync::OnceLock::new();
        self
    }

    /// The URL to request and the `Authorization` header to send for `url`, as rattler's authentication middleware
    /// gives them: a bearer or basic header, or a conda token in the path (`/t/<token>/...`).
    fn authenticate(&self, url: &str) -> Result<Authenticated, FetchError> {
        let none = Authenticated {
            url: url.to_string(),
            header: None,
            secret: None,
        };
        let Some(storage) = &self.credentials else {
            return Ok(none);
        };
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Ok(none);
        }
        let (_, found) = storage.get_by_url(url).map_err(|err| FetchError::Credentials {
            message: err.to_string(),
        })?;
        use rattler_networking::Authentication;
        Ok(match found {
            Some(Authentication::BearerToken(token))
            | Some(Authentication::OAuth {
                access_token: token, ..
            }) => Authenticated {
                header: Some(format!("Bearer {token}")),
                secret: Some(token),
                ..none
            },
            Some(Authentication::BasicHTTP { username, password }) => {
                let encoded = crate::registry::base64_encode(format!("{username}:{password}").as_bytes());
                Authenticated {
                    header: Some(format!("Basic {encoded}")),
                    secret: Some(password),
                    ..none
                }
            }
            Some(Authentication::CondaToken(token)) => {
                let mut parsed = url::Url::parse(url).map_err(|err| FetchError::Credentials {
                    message: err.to_string(),
                })?;
                let path = format!("/t/{token}{}", parsed.path());
                parsed.set_path(&path);
                Authenticated {
                    url: parsed.to_string(),
                    header: None,
                    secret: Some(token),
                }
            }
            Some(Authentication::S3Credentials { .. }) | None => none,
        })
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

    /// Stream `location` into `out` from the first of its mirrors that serves it (or from itself, when no mirror
    /// stands in for its channel), returning the sha256 of what was written.
    fn download(&self, location: &str, out: &mut File) -> Result<String, FetchError> {
        let mut failed = None;
        for url in self.sources.candidates(location) {
            let restart = out.set_len(0).and_then(|()| io::Seek::rewind(out));
            restart.map_err(|source| FetchError::Cache {
                path: self.root.clone(),
                source,
            })?;
            match self.download_from(&url, out) {
                Ok(sha256) => return Ok(sha256),
                Err(err) => failed = Some(err),
            }
        }
        Err(failed.expect("a location has at least one place to download it from"))
    }

    fn download_from(&self, url: &str, out: &mut File) -> Result<String, FetchError> {
        if crate::sources::is_remote_scheme(url) {
            return self.remote().download(url, out).map_err(|err| match err {
                crate::sources::RemoteError::Status(status @ (401 | 403)) => FetchError::Unauthorized {
                    url: url.to_string(),
                    host: url::Url::parse(url)
                        .ok()
                        .map(|parsed| format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or_default()))
                        .unwrap_or_default(),
                    status,
                    had_credentials: true,
                },
                other => FetchError::Remote {
                    url: url.to_string(),
                    message: other.to_string(),
                },
            });
        }
        let auth = self.authenticate(url)?;
        // Errors name the lockfile's URL, never the one with a token in it, and say nothing a credential is in.
        let download_err = |source: ureq::Error| FetchError::Download {
            url: url.to_string(),
            source: Box::new(scrub(source, auth.secret.as_deref())),
        };
        let mut request = self.agent.get(&auth.url);
        if let Some(header) = &auth.header {
            request = request.header("Authorization", header);
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status @ (401 | 403))) => {
                return Err(FetchError::Unauthorized {
                    url: url.to_string(),
                    host: url::Url::parse(url)
                        .ok()
                        .and_then(|parsed| parsed.host_str().map(str::to_string))
                        .unwrap_or_default(),
                    status,
                    had_credentials: auth.secret.is_some(),
                });
            }
            Err(err) => return Err(download_err(err)),
        };
        let mut reader = response.into_body().into_reader();
        copy_hashed(&mut reader, out).map_err(|err| download_err(ureq::Error::Io(err)))
    }
}

/// What a download is sent with: the URL (with a conda token in it, if that is the credential) and a header.
struct Authenticated {
    url: String,
    header: Option<String>,
    /// The secret itself, to keep out of errors.
    secret: Option<String>,
}

/// `error`, with `secret` taken out of anything it says.
fn scrub(error: ureq::Error, secret: Option<&str>) -> ureq::Error {
    match secret {
        Some(secret) if !secret.is_empty() && error.to_string().contains(secret) => {
            ureq::Error::Io(io::Error::other(error.to_string().replace(secret, "<credential>")))
        }
        _ => error,
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

    /// What a guarded channel wants before it serves a file.
    #[derive(Clone)]
    enum Wants {
        Header(String),
        TokenInPath(String),
    }

    /// A channel that serves `body` for every path, but only to a request with the credential `wants` names, and
    /// answers 401 otherwise. Every request's line and headers are kept.
    fn guarded(body: Vec<u8>, wants: Wants) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_string();
                let line = request.lines().next().unwrap_or_default().to_string();
                let allowed = match &wants {
                    Wants::Header(value) => request
                        .lines()
                        .any(|header| header.eq_ignore_ascii_case(&format!("authorization: {value}"))),
                    Wants::TokenInPath(token) => line.contains(&format!(" /t/{token}/")),
                };
                kept.lock().unwrap().push(request);
                let (status, payload): (&str, &[u8]) = if allowed {
                    ("200 OK", &body)
                } else {
                    ("401 Unauthorized", b"")
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(payload).unwrap();
            }
        });
        (format!("http://127.0.0.1:{port}/private/noarch/pkg-1-0.tar.bz2"), seen)
    }

    /// pixi's credentials as `pixi auth login` writes them to a credentials file, for the host 127.0.0.1.
    fn stored(dir: &Path, authentication: serde_json::Value) -> rattler_networking::AuthenticationStorage {
        stored_for(dir, "127.0.0.1", authentication)
    }

    fn stored_for(
        dir: &Path,
        key: &str,
        authentication: serde_json::Value,
    ) -> rattler_networking::AuthenticationStorage {
        let file = dir.join("credentials.json");
        fs::write(&file, serde_json::json!({ key: authentication }).to_string()).unwrap();
        let mut storage = rattler_networking::AuthenticationStorage::empty();
        storage.add_backend(std::sync::Arc::new(
            rattler_networking::authentication_storage::backends::file::FileStorage::from_path(file).unwrap(),
        ));
        storage
    }

    #[test]
    fn each_kind_of_pixi_credential_is_sent_as_rattler_sends_it() {
        let body = archive_bytes();
        let sha = hex(&Sha256::digest(&body));
        let basic = format!("Basic {}", crate::registry::base64_encode(b"me:s3cret-pass"));
        let cases = [
            (
                serde_json::json!({"BearerToken": "bearer-tok"}),
                Wants::Header("Bearer bearer-tok".into()),
                "bearer-tok",
            ),
            (
                serde_json::json!({"BasicHTTP": {"username": "me", "password": "s3cret-pass"}}),
                Wants::Header(basic),
                "s3cret-pass",
            ),
            (
                serde_json::json!({"CondaToken": "conda-tok"}),
                Wants::TokenInPath("conda-tok".into()),
                "conda-tok",
            ),
        ];
        for (authentication, wants, secret) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (url, seen) = guarded(body.clone(), wants);
            let package = package(&url, Some(&sha));

            // Without credentials, the channel's refusal says to log in, naming the host.
            let without = Fetcher::with_proxy(dir.path().join("a"), None);
            let err = without.archive_for(&package).unwrap_err();
            assert!(
                matches!(
                    err,
                    FetchError::Unauthorized {
                        status: 401,
                        had_credentials: false,
                        ..
                    }
                ),
                "{err}"
            );
            let help = miette::Diagnostic::help(&err).unwrap().to_string();
            assert!(help.contains("pixi auth login 127.0.0.1"), "{help}");

            let fetcher =
                Fetcher::with_proxy(dir.path().join("b"), None).with_credentials(stored(dir.path(), authentication));
            let archive = fetcher.archive_for(&package).unwrap();
            assert_eq!(fs::read(archive).unwrap(), body, "{secret}");
            assert_eq!(seen.lock().unwrap().len(), 2, "{secret}");
        }
    }

    #[test]
    fn refused_credentials_say_what_to_do() {
        let body = archive_bytes();
        let sha = hex(&Sha256::digest(&body));
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = guarded(body, Wants::Header("Bearer right".into()));
        let fetcher = Fetcher::with_proxy(dir.path().join("c"), None)
            .with_credentials(stored(dir.path(), serde_json::json!({"BearerToken": "wrong-token"})));
        let err = fetcher.archive_for(&package(&url, Some(&sha))).unwrap_err();
        assert!(
            matches!(
                err,
                FetchError::Unauthorized {
                    had_credentials: true,
                    ..
                }
            ),
            "{err}"
        );
        let message = format!("{err} {}", miette::Diagnostic::help(&err).unwrap());
        assert!(
            message.contains("were refused") && !message.contains("wrong-token"),
            "{message}"
        );
    }

    #[test]
    fn a_credential_is_scrubbed_from_what_an_error_says() {
        let error = ureq::Error::BadUri("http://h/t/tok-123/a.conda".to_string());
        let scrubbed = scrub(error, Some("tok-123")).to_string();
        assert!(
            !scrubbed.contains("tok-123") && scrubbed.contains("<credential>"),
            "{scrubbed}"
        );
        let untouched = scrub(ureq::Error::BadUri("http://h/a.conda".to_string()), Some("tok-123"));
        assert!(matches!(untouched, ureq::Error::BadUri(_)));
        assert!(matches!(
            scrub(ureq::Error::BadUri("x".into()), None),
            ureq::Error::BadUri(_)
        ));
    }

    /// A build from a channel that needs a token: the token reaches the channel and nothing else, not a layer, the
    /// config, the manifest, the index or what the build prints.
    #[test]
    fn a_private_channels_token_never_reaches_the_image() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "pixi-oci-secret-token-7d1f";
        for (authentication, wants) in [
            (
                serde_json::json!({"CondaToken": secret}),
                Wants::TokenInPath(secret.into()),
            ),
            (
                serde_json::json!({"BearerToken": secret}),
                Wants::Header(format!("Bearer {secret}")),
            ),
        ] {
            let work = tempfile::tempdir_in(dir.path()).unwrap();
            let mut tool = crate::testing::simple(work.path(), "tool", &[]);
            let bytes = fs::read(local_path(&tool.location).unwrap()).unwrap();
            let (url, seen) = guarded(bytes, wants);
            tool.location = url.replace("pkg-1-0.tar.bz2", "tool-1-0.tar.bz2");
            let plan = crate::testing::plan_of(vec![tool.clone()], 10);
            let layout = work.path().join("layout");
            let options = crate::build::BuildOptions {
                prefix: "/opt/env".to_string(),
                output: layout.clone(),
                force: true,
                reference: crate::image::Reference::parse("private:1").unwrap(),
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
                app: None,
            };
            let fetcher = Fetcher::with_proxy(work.path().join("cache"), None)
                .with_credentials(stored(work.path(), authentication));
            let mut printed = Vec::new();
            crate::build::build(&plan, &options, &fetcher, &mut printed).unwrap();
            assert!(seen.lock().unwrap().iter().any(|request| request.contains(secret)));
            assert!(!String::from_utf8_lossy(&printed).contains(secret));

            let mut checked = 0;
            for entry in walk(&layout) {
                let bytes = fs::read(&entry).unwrap();
                let mut plain = Vec::new();
                if flate2::read::GzDecoder::new(&bytes[..])
                    .read_to_end(&mut plain)
                    .is_err()
                {
                    plain = bytes;
                }
                assert!(
                    !String::from_utf8_lossy(&plain).contains(secret),
                    "{} has the token",
                    entry.display()
                );
                checked += 1;
            }
            assert!(checked >= 5, "{checked}");
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    /// A server that answers `fail` to its first `failures` requests and serves `body` after, keeping each request.
    fn flaky(
        body: Vec<u8>,
        failures: usize,
        fail: &'static str,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = seen.clone();
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).unwrap_or(0);
                kept.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request[..read]).to_string());
                let (status, payload): (&str, &[u8]) = if n < failures { (fail, b"") } else { ("200 OK", &body) };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(payload).unwrap();
            }
        });
        (port, seen)
    }

    #[test]
    fn a_mirror_stands_in_for_its_channel_and_the_next_one_for_a_mirror_that_fails() {
        let body = archive_bytes();
        let sha = hex(&Sha256::digest(&body));
        let (down, down_seen) = flaky(Vec::new(), usize::MAX, "503 Service Unavailable");
        // The second mirror wants a token, which pixi has for its host: credentials go with the mirror's URL.
        let (url, up_seen) = guarded(body.clone(), Wants::Header("Bearer mirror-token".into()));
        let up = url::Url::parse(&url).unwrap().port().unwrap();
        let sources = crate::sources::Sources::from_toml(&format!(
            "[mirrors]\n\"https://conda.anaconda.org/conda-forge\" = [\"http://127.0.0.1:{down}/broken\", \"http://127.0.0.1:{up}/private\"]\n"
        ));
        let dir = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_proxy(dir.path().join("cache"), None)
            .with_credentials(stored(dir.path(), serde_json::json!({"BearerToken": "mirror-token"})))
            .with_sources(sources);
        let locked = package(
            "https://conda.anaconda.org/conda-forge/noarch/pkg-1-0.tar.bz2",
            Some(&sha),
        );
        let archive = fetcher.archive_for(&locked).unwrap();
        assert_eq!(fs::read(archive).unwrap(), body);
        assert!(down_seen.lock().unwrap()[0].starts_with("GET /broken/noarch/pkg-1-0.tar.bz2 "));
        assert!(up_seen.lock().unwrap()[0].starts_with("GET /private/noarch/pkg-1-0.tar.bz2 "));

        // When every mirror fails, the error names the last one tried, not the channel.
        let other = package(
            "https://conda.anaconda.org/conda-forge/noarch/other-1-0.tar.bz2",
            Some(&"0".repeat(64)),
        );
        let only_down = crate::sources::Sources::from_toml(&format!(
            "[mirrors]\n\"https://conda.anaconda.org/conda-forge\" = [\"http://127.0.0.1:{down}/broken\"]\n"
        ));
        let err = Fetcher::with_proxy(dir.path().join("cache2"), None)
            .with_sources(only_down)
            .archive_for(&other)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("127.0.0.1:{down}/broken/noarch/other-1-0.tar.bz2")),
            "{err}"
        );
    }

    #[test]
    fn an_s3_package_is_downloaded_presigned_with_pixis_s3_credentials() {
        let body = archive_bytes();
        let sha = hex(&Sha256::digest(&body));
        let (port, seen) = flaky(body.clone(), 0, "");
        let sources = crate::sources::Sources::from_toml(&format!(
            "[s3-options.channels]\nendpoint-url = \"http://127.0.0.1:{port}\"\nregion = \"eu-west-1\"\naddressing-style = \"path\"\n"
        ));
        let dir = tempfile::tempdir().unwrap();
        let secret = "s3-secret-key-not-in-any-error";
        let storage = stored_for(
            dir.path(),
            "s3://channels",
            serde_json::json!({"S3Credentials": {"access_key_id": "AKIATESTKEY", "secret_access_key": secret, "session_token": null}}),
        );
        let fetcher = Fetcher::with_proxy(dir.path().join("cache"), None)
            .with_credentials(storage)
            .with_sources(sources);
        let locked = package("s3://channels/private/noarch/pkg-1-0.tar.bz2", Some(&sha));
        let archive = fetcher.archive_for(&locked).unwrap();
        assert_eq!(fs::read(archive).unwrap(), body);
        let request = seen.lock().unwrap()[0].clone();
        let line = request.lines().next().unwrap();
        assert!(
            line.starts_with("GET /channels/private/noarch/pkg-1-0.tar.bz2?"),
            "{line}"
        );
        assert!(
            line.contains("X-Amz-Signature=") && line.contains("AKIATESTKEY"),
            "{line}"
        );
        assert!(!request.contains(secret));

        // A refusal says to log in, and what it says has no signature or key in it.
        let (port, _) = flaky(Vec::new(), usize::MAX, "403 Forbidden");
        let sources = crate::sources::Sources::from_toml(&format!(
            "[s3-options.channels]\nendpoint-url = \"http://127.0.0.1:{port}\"\nregion = \"eu-west-1\"\naddressing-style = \"path\"\n"
        ));
        let storage = stored_for(
            dir.path(),
            "s3://channels",
            serde_json::json!({"S3Credentials": {"access_key_id": "AKIATESTKEY", "secret_access_key": secret, "session_token": null}}),
        );
        let err = Fetcher::with_proxy(dir.path().join("cache2"), None)
            .with_credentials(storage)
            .with_sources(sources)
            .archive_for(&package("s3://channels/private/noarch/other-1-0.tar.bz2", Some(&sha)))
            .unwrap_err();
        assert!(matches!(err, FetchError::Unauthorized { status: 403, .. }), "{err}");
        let message = format!("{err} {}", miette::Diagnostic::help(&err).unwrap());
        assert!(message.contains("pixi auth login s3://channels"), "{message}");
        assert!(!message.contains("X-Amz") && !message.contains(secret), "{message}");

        // Nothing listening: the error names the s3:// location and says why, still without the signature.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let sources = crate::sources::Sources::from_toml(&format!(
            "[s3-options.channels]\nendpoint-url = \"http://127.0.0.1:{closed}\"\nregion = \"eu-west-1\"\naddressing-style = \"path\"\n"
        ));
        let storage = stored_for(
            dir.path(),
            "s3://channels",
            serde_json::json!({"S3Credentials": {"access_key_id": "AKIATESTKEY", "secret_access_key": secret, "session_token": null}}),
        );
        let err = Fetcher::with_proxy(dir.path().join("cache3"), None)
            .with_credentials(storage)
            .with_sources(sources)
            .archive_for(&package("s3://channels/private/noarch/third-1-0.tar.bz2", Some(&sha)))
            .unwrap_err();
        assert!(matches!(err, FetchError::Remote { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("s3://channels/private/noarch/third-1-0.tar.bz2"),
            "{err}"
        );
        assert!(
            !err.to_string().contains("X-Amz") && !err.to_string().contains("AKIATESTKEY"),
            "{err}"
        );
    }
}
