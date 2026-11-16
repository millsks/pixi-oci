//! Pulling an image from an OCI registry, pinned by digest, into the cache: the manifest for one platform, its
//! config, and its layers, each checked against its digest before it is kept.
//!
//! Registries are spoken to over the distribution API (`/v2/`), anonymously or with the credentials `pixi auth
//! login` keeps (or, for CI, `PIXI_OCI_REGISTRY_USERNAME` and `PIXI_OCI_REGISTRY_PASSWORD`), through bearer tokens
//! or basic auth as the registry asks. Nothing of docker's or podman's is read or run, and nothing is unpacked:
//! blobs are kept as they were served, named by their digest.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::platform::OciPlatform;

const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
/// The most a manifest or config is read into memory.
const DOCUMENT_LIMIT: u64 = 4 << 20;

/// Why an image could not be pulled.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RegistryError {
    #[error("`{value}` is not an image reference")]
    #[diagnostic(help(
        "write it as [registry/]repository[:tag]@sha256:<digest>, for example \
        gcr.io/distroless/cc-debian12@sha256:..."
    ))]
    Reference { value: String },

    #[error("{reference} is not pinned to a digest")]
    #[diagnostic(help(
        "a base image is pinned so the same lockfile builds the same image: add @sha256:<digest>, \
        which `pixi oci digest {reference}` prints (as do `crane digest` and `skopeo inspect`)"
    ))]
    NotPinned { reference: String },

    #[error("cannot reach {url}")]
    Http {
        url: String,
        #[source]
        source: Box<ureq::Error>,
    },

    #[error("{url} answered {status}: {message}")]
    Status { url: String, status: u16, message: String },

    #[error("{registry} refused access to {repository}")]
    #[diagnostic(help(
        "log in with `pixi auth login {registry} --username <name> --password <token>`, or set \
        PIXI_OCI_REGISTRY_USERNAME and PIXI_OCI_REGISTRY_PASSWORD"
    ))]
    Unauthorized { registry: String, repository: String },

    #[error("{what} from {reference} hashes to {actual}, not {expected}")]
    Digest {
        what: String,
        reference: String,
        expected: String,
        actual: String,
    },

    #[error("{reference} has no image for {platform}; it has {}", .available.join(", "))]
    NoPlatform {
        reference: String,
        platform: String,
        available: Vec<String>,
    },

    #[error("{reference}: {message}")]
    Manifest { reference: String, message: String },

    #[error("cannot write the cache at {}", .path.display())]
    Cache {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot read pixi's credentials for {registry}: {message}")]
    #[diagnostic(help("`pixi auth logout {registry}`, then log in again with `pixi auth login`"))]
    Credentials { registry: String, message: String },
}

/// An image reference: `[registry/]repository[:tag][@digest]`, read as docker reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// As written, or `docker.io` when none is.
    pub registry: String,
    /// With `library/` for an official Docker Hub image.
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    pub fn parse(value: &str) -> Result<ImageRef, RegistryError> {
        let refused = || RegistryError::Reference {
            value: value.to_string(),
        };
        let (rest, digest) = match value.split_once('@') {
            Some((rest, digest)) => {
                crate::read::blob_path(digest).ok_or_else(refused)?;
                (rest, Some(digest.to_string()))
            }
            None => (value, None),
        };
        let (registry, path) = match rest.split_once('/') {
            Some((first, path)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), path)
            }
            _ => ("docker.io".to_string(), rest),
        };
        // A tag follows the last `:` after the last `/`, so a registry's port is not taken for one.
        let (path, tag) = match path.rsplit_once(':') {
            Some((path, tag)) if !tag.contains('/') => (path, Some(tag.to_string())),
            _ => (path, None),
        };
        let valid_tag = tag.as_deref().is_none_or(|tag| {
            !tag.is_empty()
                && tag.len() <= 128
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        });
        let valid_path = !path.is_empty()
            && path.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
            });
        if !valid_tag || !valid_path || registry.is_empty() {
            return Err(refused());
        }
        let repository = if registry == "docker.io" && !path.contains('/') {
            format!("library/{path}")
        } else {
            path.to_string()
        };
        Ok(ImageRef {
            registry,
            repository,
            tag,
            digest,
        })
    }

    /// The host the API is served from.
    fn api_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    /// `https`, except for a registry on this machine, which docker also speaks plain HTTP to.
    fn origin(&self) -> String {
        let host = self.api_host();
        let local = host.starts_with("localhost") || host.starts_with("127.0.0.1") || host.starts_with("[::1]");
        let scheme = if local { "http" } else { "https" };
        format!("{scheme}://{host}")
    }

    fn base_url(&self) -> String {
        format!("{}/v2/{}", self.origin(), self.repository)
    }

    /// A `Location` a registry answered with, absolute or relative to its origin.
    fn resolve(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            location.to_string()
        } else if location.starts_with('/') {
            format!("{}{location}", self.origin())
        } else {
            format!("{}/{location}", self.origin())
        }
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

/// A user name and password, or an identity token (user name `<token>`).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({}, <redacted>)", self.username)
    }
}

/// Where credentials come from: docker's and podman's files and helpers, or none.
pub trait CredentialSource: Send + Sync {
    fn credentials(&self, registry: &str) -> Result<Option<Credentials>, RegistryError>;
}

/// No credentials: anonymous pulls only.
pub struct Anonymous;

impl CredentialSource for Anonymous {
    fn credentials(&self, _: &str) -> Result<Option<Credentials>, RegistryError> {
        Ok(None)
    }
}

/// The credentials `pixi auth login` keeps, read as pixi reads them: the file `RATTLER_AUTH_FILE` names, the
/// operating system's keychain, `~/.rattler/credentials.json`, then `.netrc`. A user name and password is used as
/// it is; a token alone is sent as the password, which registries such as GHCR take with any user name.
pub struct PixiCredentials {
    storage: Option<rattler_networking::AuthenticationStorage>,
}

impl PixiCredentials {
    pub fn from_env() -> Self {
        PixiCredentials {
            storage: rattler_networking::AuthenticationStorage::from_env_and_defaults().ok(),
        }
    }
}

impl CredentialSource for PixiCredentials {
    fn credentials(&self, registry: &str) -> Result<Option<Credentials>, RegistryError> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        for host in config_keys(registry).iter().map(|key| key_host(key)) {
            // As pixi looks a host up: the host itself, then `*.host` and the wildcards above it, which is where
            // `pixi auth login ghcr.io` keeps them (`*.ghcr.io`).
            let (_, found) =
                storage
                    .get_by_url(format!("https://{host}/"))
                    .map_err(|err| RegistryError::Credentials {
                        registry: registry.to_string(),
                        message: err.to_string(),
                    })?;
            let credentials = match found {
                Some(rattler_networking::Authentication::BasicHTTP { username, password }) => {
                    Credentials { username, password }
                }
                Some(rattler_networking::Authentication::BearerToken(token)) => Credentials {
                    username: "pixi-oci".to_string(),
                    password: token,
                },
                Some(rattler_networking::Authentication::OAuth { access_token, .. }) => Credentials {
                    username: "pixi-oci".to_string(),
                    password: access_token,
                },
                // A conda token goes in a channel's URL, and S3 keys to a bucket: neither signs in to a registry.
                _ => continue,
            };
            return Ok(Some(credentials));
        }
        Ok(None)
    }
}

/// Credentials from `PIXI_OCI_REGISTRY_USERNAME` and `PIXI_OCI_REGISTRY_PASSWORD`, for CI, given only to the one
/// registry they are for: `PIXI_OCI_REGISTRY` when it is set, else the registry `default` names (a push's
/// destination), else none. A token for one registry is never sent to another, such as a base image's.
pub struct EnvCredentials {
    registry: Option<String>,
    credentials: Option<Credentials>,
}

impl EnvCredentials {
    pub fn from_env(default: Option<&str>) -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        let credentials = match (var("PIXI_OCI_REGISTRY_USERNAME"), var("PIXI_OCI_REGISTRY_PASSWORD")) {
            (Some(username), Some(password)) => Some(Credentials { username, password }),
            _ => None,
        };
        let registry = var("PIXI_OCI_REGISTRY").or_else(|| default.map(str::to_string));
        EnvCredentials { registry, credentials }
    }
}

impl CredentialSource for EnvCredentials {
    fn credentials(&self, registry: &str) -> Result<Option<Credentials>, RegistryError> {
        let ours = self.registry.as_deref().is_some_and(|wanted| {
            config_keys(registry)
                .iter()
                .any(|key| key_host(key) == key_host(wanted))
        });
        Ok(self.credentials.clone().filter(|_| ours))
    }
}

/// The first source that has credentials for a registry.
pub struct Chain(pub Vec<Box<dyn CredentialSource>>);

impl CredentialSource for Chain {
    fn credentials(&self, registry: &str) -> Result<Option<Credentials>, RegistryError> {
        for source in &self.0 {
            if let Some(found) = source.credentials(registry)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }
}

/// The names a registry is known by: Docker Hub has several.
fn config_keys(registry: &str) -> Vec<String> {
    if registry == "docker.io" {
        [
            "https://index.docker.io/v1/",
            "index.docker.io",
            "docker.io",
            "registry-1.docker.io",
        ]
        .iter()
        .map(|key| key.to_string())
        .collect()
    } else {
        vec![registry.to_string()]
    }
}

/// `https://host/v1/` and `host` both name `host`.
fn key_host(key: &str) -> &str {
    let key = key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
        .unwrap_or(key);
    key.split('/').next().unwrap_or(key)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One layer of a pulled image, in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledLayer {
    pub digest: String,
    pub size: u64,
    /// The OCI media type: docker's are given as their OCI equivalents, which name the same bytes.
    pub media_type: String,
    pub path: PathBuf,
}

/// An image pulled for one platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Pulled {
    pub reference: ImageRef,
    /// The digest of the image manifest for the platform, which is the one pinned unless that is an index.
    pub manifest_digest: String,
    /// The manifest's media type, OCI or docker, as the registry serves it.
    pub manifest_media_type: String,
    pub config_digest: String,
    pub config: Value,
    pub layers: Vec<PulledLayer>,
}

/// A registry client with a blob cache.
pub struct Registry {
    agent: ureq::Agent,
    root: PathBuf,
    credentials: Box<dyn CredentialSource>,
    /// Bearer tokens by registry and repository, for the life of the client.
    tokens: Mutex<BTreeMap<String, String>>,
}

/// What `send` got: the body, and its content type.
struct Answer {
    content_type: Option<String>,
    body: Box<dyn Read + Send + Sync>,
}

impl Registry {
    /// A client with its cache under `root` (the package cache's root), docker's and podman's credentials, and
    /// proxies from the usual environment variables.
    pub fn new(root: PathBuf) -> Self {
        Self::for_registry(root, None)
    }

    /// A client whose `PIXI_OCI_REGISTRY_*` credentials, if any, go to `registry` when `PIXI_OCI_REGISTRY` does not
    /// name one: the destination of a push.
    pub fn for_registry(root: PathBuf, registry: Option<&str>) -> Self {
        let credentials = Chain(vec![
            Box::new(EnvCredentials::from_env(registry)),
            Box::new(PixiCredentials::from_env()),
        ]);
        Self::with(root, Box::new(credentials), ureq::Proxy::try_from_env())
    }

    pub fn with(root: PathBuf, credentials: Box<dyn CredentialSource>, proxy: Option<ureq::Proxy>) -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(concat!("pixi-oci/", env!("CARGO_PKG_VERSION")))
            .proxy(proxy)
            .http_status_as_error(false)
            .build()
            .new_agent();
        Registry {
            agent,
            root,
            credentials,
            tokens: Mutex::new(BTreeMap::new()),
        }
    }

    /// Where the cache keeps the blob or document `digest`, once fetched.
    pub(crate) fn blob_file(&self, digest: &str) -> Option<PathBuf> {
        let hex = digest.strip_prefix("sha256:")?;
        crate::read::blob_path(digest)?;
        Some(self.root.join("blobs").join("sha256").join(hex))
    }

    /// The digest of what `reference` names: its own digest when it is pinned, else what the registry serves for its
    /// tag (`latest` when it has none), an index or a manifest, which is then cached by that digest.
    pub fn resolve(&self, reference: &ImageRef) -> Result<String, RegistryError> {
        if let Some(digest) = &reference.digest {
            return Ok(digest.clone());
        }
        let tag = reference.tag.as_deref().unwrap_or("latest");
        let url = format!("{}/manifests/{tag}", reference.base_url());
        let answer = self.send(reference, &url)?;
        let mut bytes = Vec::new();
        answer
            .body
            .take(DOCUMENT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| http_err(&url, ureq::Error::Io(err)))?;
        if bytes.len() as u64 > DOCUMENT_LIMIT {
            return Err(RegistryError::Manifest {
                reference: reference.to_string(),
                message: format!("the manifest is larger than {DOCUMENT_LIMIT} bytes"),
            });
        }
        let digest = format!("sha256:{}", crate::fetch::hex(&Sha256::digest(&bytes)));
        if let Some(file) = self.blob_file(&digest)
            && !file.is_file()
        {
            self.keep(&file, |out| out.write_all(&bytes))?;
        }
        Ok(digest)
    }

    /// The image manifest `reference` (pinned by digest) names for `platform`, and its digest: the manifest itself,
    /// or the platform's entry of an index followed. Only manifests are fetched.
    pub fn platform_manifest(
        &self,
        reference: &ImageRef,
        platform: &OciPlatform,
    ) -> Result<(String, Value), RegistryError> {
        let pinned = reference.digest.clone().ok_or_else(|| RegistryError::NotPinned {
            reference: reference.to_string(),
        })?;
        let bad = |message: String| RegistryError::Manifest {
            reference: reference.to_string(),
            message,
        };
        let (bytes, content_type) = self.document(reference, "manifests", &pinned, "the manifest")?;
        let mut manifest: Value =
            serde_json::from_slice(&bytes).map_err(|err| bad(format!("its manifest is not JSON: {err}")))?;
        let mut manifest_digest = pinned;
        let media_type = manifest["mediaType"]
            .as_str()
            .map(str::to_string)
            .or(content_type)
            .unwrap_or_default();
        if media_type == OCI_INDEX || media_type == DOCKER_LIST || manifest["manifests"].is_array() {
            let entries = manifest["manifests"].as_array().cloned().unwrap_or_default();
            let platform_of = |entry: &Value| crate::read::platform_text(&entry["platform"]);
            let wanted = platform.to_string();
            let matches = |entry: &&Value| {
                let p = &entry["platform"];
                p["os"] == platform.os
                    && p["architecture"] == platform.architecture
                    // arm64 images often leave out the v8 variant that is the only one there is.
                    && (p["variant"].as_str() == platform.variant
                        || (platform.architecture == "arm64" && p["variant"].is_null()))
            };
            let entry = entries.iter().find(matches).ok_or_else(|| RegistryError::NoPlatform {
                reference: reference.to_string(),
                platform: wanted,
                available: entries.iter().filter_map(platform_of).collect(),
            })?;
            let digest = entry["digest"]
                .as_str()
                .ok_or_else(|| bad("an index entry has no digest".to_string()))?
                .to_string();
            let (bytes, _) = self.document(reference, "manifests", &digest, "the platform's manifest")?;
            manifest = serde_json::from_slice(&bytes).map_err(|err| bad(format!("its manifest is not JSON: {err}")))?;
            manifest_digest = digest;
        }
        Ok((manifest_digest, manifest))
    }

    /// Pull `reference`, which must be pinned by digest, for `platform`.
    pub fn pull(&self, reference: &ImageRef, platform: &OciPlatform) -> Result<Pulled, RegistryError> {
        let bad = |message: String| RegistryError::Manifest {
            reference: reference.to_string(),
            message,
        };
        let (manifest_digest, manifest) = self.platform_manifest(reference, platform)?;
        if manifest["schemaVersion"] != 2 || !manifest["layers"].is_array() {
            return Err(bad("its manifest is not an OCI or docker v2 image manifest".to_string()));
        }
        let config_digest = manifest["config"]["digest"]
            .as_str()
            .ok_or_else(|| bad("its manifest names no config".to_string()))?
            .to_string();
        let (config_bytes, _) = self.document(reference, "blobs", &config_digest, "the config")?;
        let config: Value =
            serde_json::from_slice(&config_bytes).map_err(|err| bad(format!("its config is not JSON: {err}")))?;
        if config["os"] != platform.os || config["architecture"] != platform.architecture {
            return Err(bad(format!(
                "its config is for {}/{}, not {platform}",
                config["os"].as_str().unwrap_or("?"),
                config["architecture"].as_str().unwrap_or("?")
            )));
        }
        let mut layers = Vec::new();
        for layer in manifest["layers"].as_array().into_iter().flatten() {
            let digest = layer["digest"]
                .as_str()
                .ok_or_else(|| bad("a layer has no digest".to_string()))?
                .to_string();
            let media_type = match layer["mediaType"].as_str().unwrap_or_default() {
                "application/vnd.docker.image.rootfs.diff.tar.gzip" => "application/vnd.oci.image.layer.v1.tar+gzip",
                "application/vnd.docker.image.rootfs.diff.tar" => "application/vnd.oci.image.layer.v1.tar",
                other => other,
            }
            .to_string();
            if !media_type.starts_with("application/vnd.oci.image.layer.") {
                return Err(bad(format!(
                    "layer {digest} has media type {media_type}, which is not a layer"
                )));
            }
            let path = self.blob(reference, &digest)?;
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            layers.push(PulledLayer {
                digest,
                size,
                media_type,
                path,
            });
        }
        if config["rootfs"]["diff_ids"].as_array().map(Vec::len) != Some(layers.len()) {
            return Err(bad("its config does not list a diff_id for each layer".to_string()));
        }
        let manifest_media_type = manifest["mediaType"].as_str().unwrap_or(OCI_MANIFEST).to_string();
        Ok(Pulled {
            reference: reference.clone(),
            manifest_digest,
            manifest_media_type,
            config_digest,
            config,
            layers,
        })
    }

    /// A manifest or config by digest: from the cache, or fetched, checked and cached.
    pub(crate) fn document(
        &self,
        reference: &ImageRef,
        kind: &str,
        digest: &str,
        what: &str,
    ) -> Result<(Vec<u8>, Option<String>), RegistryError> {
        let file = self.blob_file(digest).ok_or_else(|| RegistryError::Manifest {
            reference: reference.to_string(),
            message: format!("{digest} is not a sha256 digest"),
        })?;
        if let Ok(bytes) = fs::read(&file) {
            return Ok((bytes, None));
        }
        let url = format!("{}/{kind}/{digest}", reference.base_url());
        let answer = self.send(reference, &url)?;
        let mut bytes = Vec::new();
        answer
            .body
            .take(DOCUMENT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| http_err(&url, ureq::Error::Io(err)))?;
        if bytes.len() as u64 > DOCUMENT_LIMIT {
            return Err(RegistryError::Manifest {
                reference: reference.to_string(),
                message: format!("{what} is larger than {DOCUMENT_LIMIT} bytes"),
            });
        }
        let actual = format!("sha256:{}", crate::fetch::hex(&Sha256::digest(&bytes)));
        if actual != digest {
            return Err(RegistryError::Digest {
                what: what.to_string(),
                reference: reference.to_string(),
                expected: digest.to_string(),
                actual,
            });
        }
        self.keep(&file, |out| out.write_all(&bytes))?;
        Ok((bytes, answer.content_type))
    }

    /// A blob in the cache: there already, or streamed in, checked, and kept.
    fn blob(&self, reference: &ImageRef, digest: &str) -> Result<PathBuf, RegistryError> {
        let file = self.blob_file(digest).ok_or_else(|| RegistryError::Manifest {
            reference: reference.to_string(),
            message: format!("{digest} is not a sha256 digest"),
        })?;
        if file.is_file() {
            return Ok(file);
        }
        let url = format!("{}/blobs/{digest}", reference.base_url());
        let mut answer = self.send(reference, &url)?;
        let dir = file.parent().expect("a blob file has a directory").to_path_buf();
        let cache_err = |source| RegistryError::Cache {
            path: dir.clone(),
            source,
        };
        fs::create_dir_all(&dir).map_err(cache_err)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(&dir)
            .map_err(cache_err)?;
        let actual = crate::fetch::copy_hashed(&mut answer.body, temp.as_file_mut())
            .map_err(|err| http_err(&url, ureq::Error::Io(err)))?;
        let actual = format!("sha256:{actual}");
        if actual != digest {
            return Err(RegistryError::Digest {
                what: format!("blob {digest}"),
                reference: reference.to_string(),
                expected: digest.to_string(),
                actual,
            });
        }
        temp.persist(&file).map_err(|err| cache_err(err.error))?;
        Ok(file)
    }

    fn keep(&self, file: &Path, write: impl FnOnce(&mut fs::File) -> io::Result<()>) -> Result<(), RegistryError> {
        let dir = file.parent().expect("a blob file has a directory");
        let cache_err = |source| RegistryError::Cache {
            path: dir.to_path_buf(),
            source,
        };
        fs::create_dir_all(dir).map_err(cache_err)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(dir)
            .map_err(cache_err)?;
        write(temp.as_file_mut()).map_err(cache_err)?;
        temp.persist(file).map_err(|err| cache_err(err.error))?;
        Ok(())
    }

    /// GET `url` for a pull, answering an authentication challenge once.
    fn send(&self, reference: &ImageRef, url: &str) -> Result<Answer, RegistryError> {
        let answer = self.call(reference, Method::Get, url, &Payload::None, false)?;
        match answer.status {
            200..=299 => Ok(Answer {
                content_type: answer.content_type,
                body: answer.body,
            }),
            _ => Err(failed(reference, url, answer)),
        }
    }

    /// A request, answering an authentication challenge once: a bearer token from the registry's token service (for
    /// `pull`, or `pull,push` when `push`), or basic auth, with credentials when there are any. Tokens are kept per
    /// repository and kind for the client's life.
    fn call(
        &self,
        reference: &ImageRef,
        method: Method,
        url: &str,
        payload: &Payload,
        push: bool,
    ) -> Result<Response, RegistryError> {
        let token_key = format!(
            "{}/{}{}",
            reference.registry,
            reference.repository,
            if push { "#push" } else { "" }
        );
        let cached = self
            .tokens
            .lock()
            .ok()
            .and_then(|tokens| tokens.get(&token_key).cloned());
        let answer = self.attempt(method, url, cached.as_deref(), payload)?;
        if answer.status != 401 {
            return Ok(answer);
        }
        let challenge = answer.challenge.clone().unwrap_or_default();
        let authorization = self.authorize(reference, &challenge, push)?;
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.insert(token_key, authorization.clone());
        }
        self.attempt(method, url, Some(&authorization), payload)
    }

    /// The `Authorization` value a challenge asks for.
    fn authorize(&self, reference: &ImageRef, challenge: &str, push: bool) -> Result<String, RegistryError> {
        let credentials = self.credentials.credentials(&reference.registry)?;
        let basic = credentials.as_ref().map(|c| {
            format!(
                "Basic {}",
                base64_encode(format!("{}:{}", c.username, c.password).as_bytes())
            )
        });
        let (scheme, params) = parse_challenge(challenge);
        if !scheme.eq_ignore_ascii_case("bearer") {
            return basic.ok_or_else(|| RegistryError::Unauthorized {
                registry: reference.registry.clone(),
                repository: reference.repository.clone(),
            });
        }
        let realm = params.get("realm").cloned().unwrap_or_default();
        let mut request = self.agent.get(&realm);
        if let Some(service) = params.get("service") {
            request = request.query("service", service);
        }
        // A push needs `pull,push` on the repository, whatever the challenge to a first HEAD or GET asked for.
        let scope = match params.get("scope") {
            Some(scope) if !push || scope.ends_with("push") => scope.clone(),
            _ if push => format!("repository:{}:pull,push", reference.repository),
            _ => format!("repository:{}:pull", reference.repository),
        };
        request = request.query("scope", &scope);
        if let Some(basic) = &basic {
            request = request.header("Authorization", basic);
        }
        let response = request.call().map_err(|err| http_err(&realm, err))?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(RegistryError::Unauthorized {
                registry: reference.registry.clone(),
                repository: reference.repository.clone(),
            });
        }
        let mut body = response.into_body();
        let bytes = body
            .with_config()
            .limit(DOCUMENT_LIMIT)
            .read_to_vec()
            .map_err(|err| http_err(&realm, err))?;
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let token = body["token"]
            .as_str()
            .or_else(|| body["access_token"].as_str())
            .filter(|token| !token.is_empty())
            .ok_or_else(|| RegistryError::Status {
                url: realm.clone(),
                status,
                message: "the token service sent no token".to_string(),
            })?;
        Ok(format!("Bearer {token}"))
    }

    fn attempt(
        &self,
        method: Method,
        url: &str,
        authorization: Option<&str>,
        payload: &Payload,
    ) -> Result<Response, RegistryError> {
        let accept = [OCI_INDEX, OCI_MANIFEST, DOCKER_LIST, DOCKER_MANIFEST].join(", ");
        let sent = match method {
            Method::Get | Method::Head => {
                let mut request = if method == Method::Get {
                    self.agent.get(url)
                } else {
                    self.agent.head(url)
                }
                .header("Accept", &accept);
                if let Some(authorization) = authorization {
                    request = request.header("Authorization", authorization);
                }
                request.call()
            }
            Method::Post | Method::Put => {
                let mut request = if method == Method::Post {
                    self.agent.post(url)
                } else {
                    self.agent.put(url)
                };
                if let Some(authorization) = authorization {
                    request = request.header("Authorization", authorization);
                }
                match payload {
                    Payload::None => request.header("Content-Length", "0").send_empty(),
                    Payload::Bytes { bytes, content_type } => {
                        request.header("Content-Type", *content_type).send(*bytes)
                    }
                    Payload::Blob { image, name, size } => {
                        let reader = image.reader(name).map_err(|err| http_err(url, ureq::Error::Io(err)))?;
                        request
                            .header("Content-Type", "application/octet-stream")
                            .header("Content-Length", size.to_string())
                            .send(ureq::SendBody::from_owned_reader(reader))
                    }
                }
            }
        };
        let response = sent.map_err(|err| http_err(url, err))?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        Ok(Response {
            status: response.status().as_u16(),
            challenge: header("www-authenticate"),
            content_type: header("content-type")
                .map(|value| value.split(';').next().unwrap_or_default().trim().to_string()),
            location: header("location"),
            digest: header("docker-content-digest"),
            body: Box::new(response.into_body().into_reader()),
        })
    }

    /// Whether the repository `destination` names already holds the blob `digest`.
    pub(crate) fn has_blob(&self, destination: &ImageRef, digest: &str) -> Result<bool, RegistryError> {
        let url = format!("{}/blobs/{digest}", destination.base_url());
        let answer = self.call(destination, Method::Head, &url, &Payload::None, true)?;
        match answer.status {
            200..=299 => Ok(true),
            404 => Ok(false),
            _ => Err(failed(destination, &url, answer)),
        }
    }

    /// Start an upload to `destination`, or mount `digest` from the repository `from` on the same registry. A
    /// mount the registry made is `None`; otherwise the upload URL to `PUT` the blob to.
    pub(crate) fn start_upload(
        &self,
        destination: &ImageRef,
        mount: Option<(&str, &str)>,
    ) -> Result<Option<String>, RegistryError> {
        let mut url = format!("{}/blobs/uploads/", destination.base_url());
        if let Some((digest, from)) = mount {
            url.push_str(&format!("?mount={digest}&from={from}"));
        }
        let answer = self.call(destination, Method::Post, &url, &Payload::None, true)?;
        match answer.status {
            201 if mount.is_some() => Ok(None),
            202 => {
                let location = answer.location.clone().ok_or_else(|| RegistryError::Status {
                    url: url.clone(),
                    status: 202,
                    message: "the registry gave no upload location".to_string(),
                })?;
                Ok(Some(destination.resolve(&location)))
            }
            _ => Err(failed(destination, &url, answer)),
        }
    }

    /// Send the blob `name` of `image` to an upload URL, as `digest`.
    pub(crate) fn upload(
        &self,
        destination: &ImageRef,
        location: &str,
        digest: &str,
        image: &crate::read::Image,
        name: &str,
        size: u64,
    ) -> Result<(), RegistryError> {
        let separator = if location.contains('?') { '&' } else { '?' };
        let url = format!("{location}{separator}digest={digest}");
        let payload = Payload::Blob { image, name, size };
        let answer = self.call(destination, Method::Put, &url, &payload, true)?;
        match answer.status {
            200..=299 => Ok(()),
            _ => Err(failed(destination, &url, answer)),
        }
    }

    /// Put a manifest under `reference` (a tag or a digest), returning the digest the registry says it stored.
    pub(crate) fn put_manifest(
        &self,
        destination: &ImageRef,
        reference: &str,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<Option<String>, RegistryError> {
        let url = format!("{}/manifests/{reference}", destination.base_url());
        let payload = Payload::Bytes {
            bytes,
            content_type: media_type,
        };
        let answer = self.call(destination, Method::Put, &url, &payload, true)?;
        match answer.status {
            200..=299 => Ok(answer.digest.clone()),
            _ => Err(failed(destination, &url, answer)),
        }
    }
}

struct Response {
    status: u16,
    challenge: Option<String>,
    content_type: Option<String>,
    location: Option<String>,
    digest: Option<String>,
    body: Box<dyn Read + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Head,
    Post,
    Put,
}

/// A request body that can be sent again, after an authentication challenge.
enum Payload<'a> {
    None,
    Bytes {
        bytes: &'a [u8],
        content_type: &'a str,
    },
    /// A blob of an image on disk, read afresh for each attempt.
    Blob {
        image: &'a crate::read::Image,
        name: &'a str,
        size: u64,
    },
}

/// The error for an answer that is not the one asked for: refused access, or the status and the start of the body.
fn failed(reference: &ImageRef, url: &str, answer: Response) -> RegistryError {
    if answer.status == 401 || answer.status == 403 {
        return RegistryError::Unauthorized {
            registry: reference.registry.clone(),
            repository: reference.repository.clone(),
        };
    }
    let mut message = String::new();
    let _ = answer.body.take(512).read_to_string(&mut message);
    RegistryError::Status {
        url: url.to_string(),
        status: answer.status,
        message: message.trim().to_string(),
    }
}

fn http_err(url: &str, source: ureq::Error) -> RegistryError {
    RegistryError::Http {
        url: url.to_string(),
        source: Box::new(source),
    }
}

/// `Bearer realm="https://auth",service="registry",scope="repository:a:pull"` as its scheme and parameters.
fn parse_challenge(challenge: &str) -> (String, BTreeMap<String, String>) {
    let challenge = challenge.trim();
    let (scheme, rest) = challenge.split_once(' ').unwrap_or((challenge, ""));
    let mut params = BTreeMap::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else {
            break;
        };
        let key = key.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let (value, remaining) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        params.insert(key, value.to_string());
        rest = remaining.trim_start_matches(',').trim();
    }
    (scheme.to_string(), params)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn sha(bytes: &[u8]) -> String {
        format!("sha256:{}", crate::fetch::hex(&Sha256::digest(bytes)))
    }

    #[derive(Clone, Default)]
    struct Auth {
        /// Demand `Bearer t0k`, from a token service that wants this `Authorization` if any.
        token: Option<Option<String>>,
        /// Demand this basic `Authorization`.
        basic: Option<String>,
    }

    /// A registry on a local port: `routes` by path, `auth` demanded on `/v2/`, every request path recorded.
    struct Fake {
        host: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn fake(routes: HashMap<String, (String, Vec<u8>)>, auth: Auth) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let realm = format!("http://{host}/token");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                let authorization = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                    })
                    .map(str::to_string);
                seen.lock().unwrap().push(path.clone());
                let reply = |status: &str, headers: &[String], body: &[u8]| {
                    let mut out = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for header in headers {
                        out.push_str(header);
                        out.push_str("\r\n");
                    }
                    out.push_str("\r\n");
                    let mut bytes = out.into_bytes();
                    bytes.extend_from_slice(body);
                    bytes
                };
                let response = if path.starts_with("/token") {
                    match &auth.token {
                        Some(Some(wanted)) if authorization.as_deref() != Some(wanted.as_str()) => {
                            reply("401 Unauthorized", &[], b"{}")
                        }
                        _ => reply(
                            "200 OK",
                            &["Content-Type: application/json".into()],
                            br#"{"token":"t0k"}"#,
                        ),
                    }
                } else if auth.token.is_some() && authorization.as_deref() != Some("Bearer t0k") {
                    let challenge = format!(
                        "WWW-Authenticate: Bearer realm=\"{realm}\",service=\"fake\",scope=\"repository:team/base:pull\""
                    );
                    reply("401 Unauthorized", &[challenge], b"")
                } else if auth.basic.is_some() && authorization != auth.basic {
                    reply(
                        "401 Unauthorized",
                        &["WWW-Authenticate: Basic realm=\"fake\"".into()],
                        b"",
                    )
                } else {
                    match routes.get(&path) {
                        Some((content_type, body)) => reply("200 OK", &[format!("Content-Type: {content_type}")], body),
                        None => reply("404 Not Found", &[], b"{\"errors\":[{\"code\":\"MANIFEST_UNKNOWN\"}]}"),
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        Fake { host, requests }
    }

    impl Fake {
        fn count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    /// One layer, its config and its manifest, for `architecture`, with OCI or docker media types.
    struct Image {
        layer: Vec<u8>,
        config: Vec<u8>,
        manifest: Vec<u8>,
    }

    fn image(architecture: &str, docker: bool) -> Image {
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o755);
        tar.append_data(&mut header, "bin/sh", &b"#!sh\n"[..]).unwrap();
        let tar = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        let layer = gz.finish().unwrap();
        let config = serde_json::to_vec(&json!({
            "os": "linux", "architecture": architecture,
            "config": {"Env": ["PATH=/usr/bin:/bin"]},
            "rootfs": {"type": "layers", "diff_ids": [sha(&tar)]},
        }))
        .unwrap();
        let (manifest_type, config_type, layer_type) = if docker {
            (
                DOCKER_MANIFEST,
                "application/vnd.docker.container.image.v1+json",
                "application/vnd.docker.image.rootfs.diff.tar.gzip",
            )
        } else {
            (
                OCI_MANIFEST,
                crate::image::CONFIG_MEDIA_TYPE,
                crate::image::LAYER_MEDIA_TYPE,
            )
        };
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": manifest_type,
            "config": {"mediaType": config_type, "digest": sha(&config), "size": config.len()},
            "layers": [{"mediaType": layer_type, "digest": sha(&layer), "size": layer.len()}],
        }))
        .unwrap();
        Image {
            layer,
            config,
            manifest,
        }
    }

    fn routes(images: &[&Image]) -> HashMap<String, (String, Vec<u8>)> {
        let mut routes = HashMap::new();
        for image in images {
            routes.insert(
                format!("/v2/team/base/manifests/{}", sha(&image.manifest)),
                (OCI_MANIFEST.to_string(), image.manifest.clone()),
            );
            routes.insert(
                format!("/v2/team/base/blobs/{}", sha(&image.config)),
                ("application/octet-stream".to_string(), image.config.clone()),
            );
            routes.insert(
                format!("/v2/team/base/blobs/{}", sha(&image.layer)),
                ("application/octet-stream".to_string(), image.layer.clone()),
            );
        }
        routes
    }

    fn amd64() -> OciPlatform {
        crate::platform::oci_platform("linux-64").unwrap()
    }

    fn arm64() -> OciPlatform {
        crate::platform::oci_platform("linux-aarch64").unwrap()
    }

    struct Static(Option<Credentials>);

    impl CredentialSource for Static {
        fn credentials(&self, _: &str) -> Result<Option<Credentials>, RegistryError> {
            Ok(self.0.clone())
        }
    }

    fn client(root: &Path, credentials: Option<Credentials>) -> Registry {
        Registry::with(root.to_path_buf(), Box::new(Static(credentials)), None)
    }

    fn user() -> Credentials {
        Credentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        }
    }

    fn reference(host: &str, digest: &str) -> ImageRef {
        ImageRef::parse(&format!("{host}/team/base:1@{digest}")).unwrap()
    }

    #[test]
    fn references_are_read_as_docker_reads_them() {
        let hex = "a".repeat(64);
        let parsed = ImageRef::parse(&format!("debian:bookworm-slim@sha256:{hex}")).unwrap();
        assert_eq!(
            (
                parsed.registry.as_str(),
                parsed.repository.as_str(),
                parsed.tag.as_deref()
            ),
            ("docker.io", "library/debian", Some("bookworm-slim"))
        );
        assert_eq!(parsed.base_url(), "https://registry-1.docker.io/v2/library/debian");
        let ubi = ImageRef::parse("registry.access.redhat.com/ubi9/ubi-minimal").unwrap();
        assert_eq!(
            (ubi.registry.as_str(), ubi.repository.as_str(), ubi.tag, ubi.digest),
            ("registry.access.redhat.com", "ubi9/ubi-minimal", None, None)
        );
        let local = ImageRef::parse("localhost:5000/team/app:1.0").unwrap();
        assert_eq!(local.base_url(), "http://localhost:5000/v2/team/app");
        assert_eq!(local.to_string(), "localhost:5000/team/app:1.0");
        assert_eq!(ImageRef::parse("someone/tool").unwrap().repository, "someone/tool");
        for bad in [
            "",
            "UPPER/case",
            "a//b",
            "app:bad tag",
            "app@sha256:short",
            "app:",
            ":tag",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_image_is_pulled_checked_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let base = image("amd64", false);
        let server = fake(routes(&[&base]), Auth::default());
        let pinned = reference(&server.host, &sha(&base.manifest));
        let pulled = client(dir.path(), None).pull(&pinned, &amd64()).unwrap();
        assert_eq!(pulled.manifest_digest, sha(&base.manifest));
        assert_eq!(pulled.config["architecture"], "amd64");
        assert_eq!(pulled.layers.len(), 1);
        assert_eq!(pulled.layers[0].digest, sha(&base.layer));
        assert_eq!(pulled.layers[0].media_type, crate::image::LAYER_MEDIA_TYPE);
        assert_eq!(fs::read(&pulled.layers[0].path).unwrap(), base.layer);
        assert_eq!(server.count(), 3);
        // Everything is in the cache now: a new client asks the registry for nothing.
        let again = client(dir.path(), None).pull(&pinned, &amd64()).unwrap();
        assert_eq!(again, pulled);
        assert_eq!(server.count(), 3);
    }

    #[test]
    fn an_index_is_followed_to_the_platform() {
        let dir = tempfile::tempdir().unwrap();
        let (amd, arm) = (image("amd64", true), image("arm64", true));
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2, "mediaType": DOCKER_LIST,
            "manifests": [
                {"mediaType": DOCKER_MANIFEST, "digest": sha(&amd.manifest), "size": amd.manifest.len(), "platform": {"os": "linux", "architecture": "amd64"}},
                {"mediaType": DOCKER_MANIFEST, "digest": sha(&arm.manifest), "size": arm.manifest.len(), "platform": {"os": "linux", "architecture": "arm64"}},
            ],
        }))
        .unwrap();
        let mut routes = routes(&[&amd, &arm]);
        routes.insert(
            format!("/v2/team/base/manifests/{}", sha(&index)),
            (DOCKER_LIST.to_string(), index.clone()),
        );
        let server = fake(routes, Auth::default());
        let pinned = reference(&server.host, &sha(&index));
        let registry = client(dir.path(), None);
        let pulled = registry.pull(&pinned, &arm64()).unwrap();
        assert_eq!(pulled.manifest_digest, sha(&arm.manifest));
        assert_eq!(pulled.config["architecture"], "arm64");
        // docker's layer media type is given as its OCI equivalent.
        assert_eq!(pulled.layers[0].media_type, crate::image::LAYER_MEDIA_TYPE);
        let err = registry
            .pull(&pinned, &crate::platform::oci_platform("linux-ppc64le").unwrap())
            .unwrap_err();
        assert!(matches!(err, RegistryError::NoPlatform { .. }), "{err}");
        assert!(err.to_string().ends_with("it has linux/amd64, linux/arm64"), "{err}");
    }

    #[test]
    fn content_that_does_not_match_its_digest_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let base = image("amd64", false);
        let mut routes = routes(&[&base]);
        // The manifest served under another manifest's digest.
        let other = sha(b"something else");
        routes.insert(
            format!("/v2/team/base/manifests/{other}"),
            (OCI_MANIFEST.to_string(), base.manifest.clone()),
        );
        // A layer whose bytes are not the ones its digest names.
        routes.insert(
            format!("/v2/team/base/blobs/{}", sha(&base.layer)),
            ("application/octet-stream".to_string(), b"tampered".to_vec()),
        );
        let server = fake(routes, Auth::default());
        let registry = client(dir.path(), None);
        let err = registry.pull(&reference(&server.host, &other), &amd64()).unwrap_err();
        assert!(matches!(err, RegistryError::Digest { .. }), "{err}");
        let err = registry
            .pull(&reference(&server.host, &sha(&base.manifest)), &amd64())
            .unwrap_err();
        assert!(
            err.to_string().starts_with(&format!("blob {} from ", sha(&base.layer))),
            "{err}"
        );
        assert!(registry.blob_file(&sha(&base.layer)).is_some_and(|file| !file.exists()));
        assert!(registry.blob_file(&other).is_some_and(|file| !file.exists()));
    }

    #[test]
    fn a_base_must_be_pinned_and_for_the_platform() {
        let dir = tempfile::tempdir().unwrap();
        let unpinned = ImageRef::parse("debian:bookworm-slim").unwrap();
        let err = client(dir.path(), None).pull(&unpinned, &amd64()).unwrap_err();
        assert!(matches!(err, RegistryError::NotPinned { .. }), "{err}");

        let base = image("arm64", false);
        let server = fake(routes(&[&base]), Auth::default());
        let err = client(dir.path(), None)
            .pull(&reference(&server.host, &sha(&base.manifest)), &amd64())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("its config is for linux/arm64, not linux/amd64"),
            "{err}"
        );

        let missing = reference(&server.host, &sha(b"nothing"));
        let err = client(dir.path(), None).pull(&missing, &amd64()).unwrap_err();
        assert!(matches!(err, RegistryError::Status { status: 404, .. }), "{err}");
        assert!(err.to_string().contains("MANIFEST_UNKNOWN"), "{err}");
    }

    #[test]
    fn a_bearer_token_is_fetched_once_with_credentials_when_needed() {
        let base = image("amd64", false);
        // Anonymous token, as Docker Hub and most public registries give.
        let dir = tempfile::tempdir().unwrap();
        let server = fake(
            routes(&[&base]),
            Auth {
                token: Some(None),
                basic: None,
            },
        );
        let pulled = client(dir.path(), None).pull(&reference(&server.host, &sha(&base.manifest)), &amd64());
        assert!(pulled.is_ok(), "{pulled:?}");
        let requests = server.requests.lock().unwrap().clone();
        assert_eq!(
            requests.iter().filter(|path| path.starts_with("/token")).count(),
            1,
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|path| path.contains("scope=repository%3Ateam%2Fbase%3Apull")
                    || path.contains("scope=repository:team/base:pull")),
            "{requests:?}"
        );

        // A private repository: the token service wants the user's credentials.
        let basic = format!("Basic {}", base64_encode(b"user:pass"));
        let dir = tempfile::tempdir().unwrap();
        let server = fake(
            routes(&[&base]),
            Auth {
                token: Some(Some(basic)),
                basic: None,
            },
        );
        let pinned = reference(&server.host, &sha(&base.manifest));
        let err = client(dir.path(), None).pull(&pinned, &amd64()).unwrap_err();
        assert!(matches!(err, RegistryError::Unauthorized { .. }), "{err}");
        assert!(client(dir.path(), Some(user())).pull(&pinned, &amd64()).is_ok());
    }

    #[test]
    fn basic_auth_is_sent_when_asked_for() {
        let base = image("amd64", false);
        let dir = tempfile::tempdir().unwrap();
        let basic = format!("Basic {}", base64_encode(b"user:pass"));
        let server = fake(
            routes(&[&base]),
            Auth {
                token: None,
                basic: Some(basic),
            },
        );
        let pinned = reference(&server.host, &sha(&base.manifest));
        let err = client(dir.path(), None).pull(&pinned, &amd64()).unwrap_err();
        assert!(err.to_string().contains("refused access to team/base"), "{err}");
        let wrong = Credentials {
            username: "user".to_string(),
            password: "nope".to_string(),
        };
        assert!(client(dir.path(), Some(wrong)).pull(&pinned, &amd64()).is_err());
        assert!(client(dir.path(), Some(user())).pull(&pinned, &amd64()).is_ok());
    }

    #[test]
    fn credentials_come_from_pixis_store() {
        use rattler_networking::Authentication;
        use rattler_networking::authentication_storage::backends::memory::MemoryStorage;
        let mut storage = rattler_networking::AuthenticationStorage::empty();
        storage.add_backend(std::sync::Arc::new(MemoryStorage::new()));
        let store = |host: &str, auth: Authentication| storage.store(host, &auth).unwrap();
        // Where `pixi auth login ghcr.io` puts them.
        store(
            "*.ghcr.io",
            Authentication::BasicHTTP {
                username: "kevin".to_string(),
                password: "pat".to_string(),
            },
        );
        store("quay.io", Authentication::BearerToken("robot-token".to_string()));
        store(
            "index.docker.io",
            Authentication::BasicHTTP {
                username: "hub".to_string(),
                password: "secret".to_string(),
            },
        );
        store("conda.example.com", Authentication::CondaToken("t".to_string()));
        let pixi = PixiCredentials { storage: Some(storage) };
        let found = |registry: &str| pixi.credentials(registry).unwrap().map(|c| (c.username, c.password));
        assert_eq!(found("ghcr.io"), Some(("kevin".to_string(), "pat".to_string())));
        assert_eq!(
            found("quay.io"),
            Some(("pixi-oci".to_string(), "robot-token".to_string()))
        );
        // Docker Hub is found under any of the names it is known by.
        assert_eq!(found("docker.io"), Some(("hub".to_string(), "secret".to_string())));
        // A conda token is for a channel's URL, not a registry.
        assert_eq!(found("conda.example.com"), None);
        assert_eq!(found("registry.example.com"), None);
        assert_eq!(PixiCredentials { storage: None }.credentials("ghcr.io").unwrap(), None);
        // The real store, as pixi reads it on this machine, answers without failing.
        assert!(PixiCredentials::from_env().credentials("registry.invalid").is_ok());
        assert!(Anonymous.credentials("x").unwrap().is_none());
    }

    #[test]
    fn environment_credentials_go_only_to_their_registry() {
        let token = Credentials {
            username: "ci".to_string(),
            password: "token".to_string(),
        };
        let ghcr = EnvCredentials {
            registry: Some("ghcr.io".to_string()),
            credentials: Some(token.clone()),
        };
        assert_eq!(ghcr.credentials("ghcr.io").unwrap(), Some(token.clone()));
        // A base image pulled from Docker Hub during the same build never sees the GHCR token.
        assert_eq!(ghcr.credentials("docker.io").unwrap(), None);
        let hub = EnvCredentials {
            registry: Some("https://index.docker.io/v1/".to_string()),
            credentials: Some(token.clone()),
        };
        assert_eq!(hub.credentials("docker.io").unwrap(), Some(token.clone()));
        let nowhere = EnvCredentials {
            registry: None,
            credentials: Some(token.clone()),
        };
        assert_eq!(nowhere.credentials("ghcr.io").unwrap(), None);
        let chain = Chain(vec![Box::new(nowhere), Box::new(Static(Some(user())))]);
        assert_eq!(chain.credentials("ghcr.io").unwrap(), Some(user()));
        assert_eq!(Chain(Vec::new()).credentials("ghcr.io").unwrap(), None);
        // With none set in this process's environment, there is nothing to give.
        assert!(
            EnvCredentials::from_env(Some("ghcr.io")).credentials.is_none()
                || std::env::var_os("PIXI_OCI_REGISTRY_PASSWORD").is_some()
        );
    }

    #[test]
    fn base64_and_challenges_are_read_like_the_specs_say() {
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"pi"), "cGk=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"pix"), "cGl4");
        let (scheme, params) = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/debian:pull""#,
        );
        assert_eq!(scheme, "Bearer");
        assert_eq!(params["realm"], "https://auth.docker.io/token");
        assert_eq!(params["scope"], "repository:library/debian:pull");
        let (scheme, params) = parse_challenge("Basic realm=fake, charset=UTF-8");
        assert_eq!(
            (scheme.as_str(), params["realm"].as_str(), params["charset"].as_str()),
            ("Basic", "fake", "UTF-8")
        );
        assert_eq!(parse_challenge("Bearer").1.len(), 0);
    }

    /// The two base images the docs recommend, pinned to the index digests their tags had on 2026-11-15.
    pub(crate) const BASES: [&str; 2] = [
        "registry.access.redhat.com/ubi9/ubi-minimal@sha256:baafce2d34783a28b4a472779f304e97e00f9d35c99553bc53b068b14344d9a9",
        "debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc",
    ];

    /// Pull both from their real registries, for both platforms the fixture locks.
    #[test]
    fn real_base_images_are_pulled() {
        if std::env::var("PIXI_OCI_NETWORK_TESTS").as_deref() != Ok("1") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::with(
            dir.path().to_path_buf(),
            Box::new(Anonymous),
            ureq::Proxy::try_from_env(),
        );
        for base in BASES {
            for platform in [amd64(), arm64()] {
                let pulled = registry.pull(&ImageRef::parse(base).unwrap(), &platform).unwrap();
                assert_eq!(pulled.config["architecture"], platform.architecture, "{base}");
                assert!(!pulled.layers.is_empty(), "{base}");
                for layer in &pulled.layers {
                    assert!(layer.size > 0 && layer.path.is_file(), "{base}");
                }
            }
        }
    }
}
