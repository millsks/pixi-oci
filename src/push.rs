//! `push`: an image layout or archive sent to a registry, with no Docker.
//!
//! The image is validated first. Then each blob the manifest names (its config, then its layers) is asked for with a
//! `HEAD` and sent only if the repository does not have it: mounted from the base image's repository when the base
//! lives on the same registry, uploaded otherwise. The manifest goes last, under the tag, and the digest the registry
//! answers with must be the image's.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::inspect::{InspectError, Raw, inspect, raw};
use crate::read::{Image, blob_path};
use crate::registry::{ImageRef, Registry, RegistryError};
use crate::report::{plural, size_text};

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PushError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Inspect(#[from] InspectError),

    #[error("{image} is not a valid OCI image: {}", plural(*.problems, "problem"))]
    #[diagnostic(help("`pixi oci validate {image}` lists them"))]
    Invalid { image: String, problems: usize },

    #[error(transparent)]
    #[diagnostic(transparent)]
    Registry(#[from] RegistryError),

    #[error("{destination} is pinned to {pinned}, but the image's manifest is {digest}")]
    WrongDigest {
        destination: String,
        pinned: String,
        digest: String,
    },

    #[error("{registry} stored the manifest as {stored}, not {digest}")]
    #[diagnostic(help("the registry rewrote the manifest; pull it by the digest it reports, or report this"))]
    Stored {
        registry: String,
        stored: String,
        digest: String,
    },
}

/// What a push did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pushed {
    /// Where the manifest went: the destination with its tag, or its digest.
    pub reference: String,
    pub digest: String,
    /// Blobs sent, and their bytes.
    pub uploaded: usize,
    pub uploaded_size: u64,
    /// Blobs the registry mounted from the base image's repository.
    pub mounted: usize,
    /// Blobs the repository had already.
    pub present: usize,
}

impl Pushed {
    pub fn text(&self) -> String {
        format!(
            "Pushed {}\n  digest {}\n  {}: {} uploaded ({}), {} mounted, {} already there\n",
            self.reference,
            self.digest,
            plural(self.uploaded + self.mounted + self.present, "blob"),
            self.uploaded,
            size_text(Some(self.uploaded_size)),
            self.mounted,
            self.present
        )
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a push always serializes")
    }
}

/// Push the image at `path` (its manifest for `platform`, when it holds several) to `destination`.
pub fn push(
    path: &Path,
    platform: Option<&str>,
    destination: &ImageRef,
    registry: &Registry,
    progress: &mut dyn Write,
) -> Result<Pushed, PushError> {
    let report = crate::validate::validate(path).map_err(InspectError::from)?;
    if !report.valid {
        return Err(PushError::Invalid {
            image: path.display().to_string(),
            problems: report.problems.len(),
        });
    }
    let inspection = inspect(path, platform)?;
    let manifest_text = raw(path, platform, Raw::Manifest)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).map_err(|err| InspectError::Broken {
        image: path.display().to_string(),
        message: format!("its manifest is not JSON: {err}"),
    })?;
    let digest = inspection.manifest.digest.clone();
    if let Some(pinned) = &destination.digest
        && *pinned != digest
    {
        return Err(PushError::WrongDigest {
            destination: destination.to_string(),
            pinned: pinned.clone(),
            digest,
        });
    }
    // The base image, when it lives on the destination's registry, can lend its layers without an upload.
    let mount_from = manifest["annotations"]["org.opencontainers.image.base.name"]
        .as_str()
        .and_then(|name| ImageRef::parse(name).ok())
        .filter(|base| base.registry == destination.registry && base.repository != destination.repository)
        .map(|base| base.repository);

    let image = Image::open(path).map_err(InspectError::from)?;
    let mut blobs: Vec<(String, u64)> = vec![(inspection.config.digest.clone(), inspection.config.size)];
    blobs.extend(inspection.layers.iter().map(|layer| (layer.digest.clone(), layer.size)));
    let mut pushed = Pushed {
        reference: String::new(),
        digest: digest.clone(),
        uploaded: 0,
        uploaded_size: 0,
        mounted: 0,
        present: 0,
    };
    let total = blobs.len();
    for (i, (blob, size)) in blobs.iter().enumerate() {
        let short: String = blob.chars().take("sha256:".len() + 12).collect();
        if registry.has_blob(destination, blob)? {
            pushed.present += 1;
            let _ = writeln!(progress, "[{}/{total}] {short} already there", i + 1);
            continue;
        }
        let mount = mount_from.as_deref().map(|from| (blob.as_str(), from));
        match registry.start_upload(destination, mount)? {
            None => {
                pushed.mounted += 1;
                let _ = writeln!(
                    progress,
                    "[{}/{total}] {short} mounted from {}",
                    i + 1,
                    mount_from.as_deref().unwrap_or_default()
                );
            }
            Some(location) => {
                let _ = writeln!(
                    progress,
                    "[{}/{total}] {short} uploading ({})",
                    i + 1,
                    size_text(Some(*size))
                );
                let name = blob_path(blob).expect("a validated image's digests are sha256");
                registry.upload(destination, &location, blob, &image, &name, *size)?;
                pushed.uploaded += 1;
                pushed.uploaded_size += size;
            }
        }
    }

    let media_type = manifest["mediaType"]
        .as_str()
        .unwrap_or(crate::image::MANIFEST_MEDIA_TYPE)
        .to_string();
    let target = match (&destination.digest, &destination.tag) {
        (Some(pinned), _) => pinned.clone(),
        (None, Some(tag)) => tag.clone(),
        (None, None) => "latest".to_string(),
    };
    let stored = registry.put_manifest(destination, &target, manifest_text.as_bytes(), &media_type)?;
    if let Some(stored) = stored
        && stored != digest
    {
        return Err(PushError::Stored {
            registry: destination.registry.clone(),
            stored,
            digest,
        });
    }
    let name = format!("{}/{}", destination.registry, destination.repository);
    pushed.reference = if destination.digest.is_some() {
        format!("{name}@{digest}")
    } else {
        format!("{name}:{target}")
    };
    Ok(pushed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::registry::{CredentialSource, Credentials};
    use crate::testing;

    fn sha(bytes: &[u8]) -> String {
        format!("sha256:{}", crate::fetch::hex(&Sha256::digest(bytes)))
    }

    #[derive(Default)]
    struct Store {
        blobs: HashMap<(String, String), Vec<u8>>,
        manifests: HashMap<(String, String), Vec<u8>>,
        /// `METHOD path` of every request.
        requests: Vec<String>,
        /// The scope of every token asked for.
        scopes: Vec<String>,
        uploads: usize,
    }

    #[derive(Clone, Copy, Default)]
    struct Behaviour {
        /// Demand a bearer token: any for reads, a push token for writes.
        tokens: bool,
        /// Answer manifest PUTs with another digest.
        lie: bool,
    }

    struct Fake {
        host: String,
        store: Arc<Mutex<Store>>,
    }

    /// A registry on a local port that keeps what is pushed to it.
    fn fake(behaviour: Behaviour) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let store = Arc::new(Mutex::new(Store::default()));
        let shared = store.clone();
        let realm = format!("http://{host}/token");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).is_err() {
                    continue;
                }
                let mut parts = first.split_whitespace();
                let (method, target) = (
                    parts.next().unwrap_or_default().to_string(),
                    parts.next().unwrap_or_default().to_string(),
                );
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.trim_end().split_once(':') {
                        headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                let mut body = Vec::new();
                if let Some(length) = headers.get("content-length").and_then(|l| l.parse::<usize>().ok()) {
                    body.resize(length, 0);
                    reader.read_exact(&mut body).unwrap();
                } else if headers.get("transfer-encoding").is_some_and(|t| t.contains("chunked")) {
                    loop {
                        let mut size = String::new();
                        reader.read_line(&mut size).unwrap();
                        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
                        let mut chunk = vec![0; size + 2];
                        reader.read_exact(&mut chunk).unwrap();
                        if size == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..size]);
                    }
                }
                let reply = handle(&shared, behaviour, &realm, &method, &target, &headers, &body);
                let mut stream = stream;
                let _ = stream.write_all(&reply);
            }
        });
        Fake { host, store }
    }

    fn reply(status: &str, headers: &[String], body: &[u8]) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for header in headers {
            head.push_str(header);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn handle(
        store: &Mutex<Store>,
        behaviour: Behaviour,
        realm: &str,
        method: &str,
        target: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> Vec<u8> {
        let mut store = store.lock().unwrap();
        store.requests.push(format!("{method} {target}"));
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let params: HashMap<&str, String> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k, v.replace("%3A", ":").replace("%2F", "/").replace("%2C", ",")))
            .collect();
        if path == "/token" {
            let scope = params.get("scope").cloned().unwrap_or_default();
            store.scopes.push(scope.clone());
            let token = if scope.ends_with("push") { "push" } else { "pull" };
            return reply(
                "200 OK",
                &["Content-Type: application/json".into()],
                format!("{{\"token\":\"{token}\"}}").as_bytes(),
            );
        }
        let Some(rest) = path.strip_prefix("/v2/") else {
            return reply("404 Not Found", &[], b"");
        };
        let (repo, kind, tail) = if let Some((repo, tail)) = rest.split_once("/blobs/uploads/") {
            (repo.to_string(), "uploads", tail.to_string())
        } else if let Some((repo, tail)) = rest.split_once("/blobs/") {
            (repo.to_string(), "blobs", tail.to_string())
        } else if let Some((repo, tail)) = rest.split_once("/manifests/") {
            (repo.to_string(), "manifests", tail.to_string())
        } else {
            return reply("404 Not Found", &[], b"");
        };
        if behaviour.tokens {
            let token = headers.get("authorization").cloned().unwrap_or_default();
            let writes = method == "POST" || method == "PUT";
            let allowed = if writes {
                token == "Bearer push"
            } else {
                token.starts_with("Bearer ")
            };
            if !allowed {
                let challenge = format!(
                    "WWW-Authenticate: Bearer realm=\"{realm}\",service=\"fake\",scope=\"repository:{repo}:pull\""
                );
                return reply("401 Unauthorized", &[challenge], b"");
            }
        }
        match (method, kind) {
            ("HEAD", "blobs") | ("GET", "blobs") => match store.blobs.get(&(repo.clone(), tail.clone())) {
                Some(blob) => reply("200 OK", &[], if method == "GET" { blob } else { b"" }),
                None => reply("404 Not Found", &[], b""),
            },
            ("POST", "uploads") => {
                if let (Some(digest), Some(from)) = (params.get("mount"), params.get("from"))
                    && let Some(blob) = store.blobs.get(&(from.clone(), digest.clone())).cloned()
                {
                    store.blobs.insert((repo.clone(), digest.clone()), blob);
                    return reply("201 Created", &[], b"");
                }
                store.uploads += 1;
                let location = format!("Location: /v2/{repo}/blobs/uploads/{}?_state=s", store.uploads);
                reply("202 Accepted", &[location], b"")
            }
            ("PUT", "uploads") => {
                let digest = params.get("digest").cloned().unwrap_or_default();
                if sha(body) != digest || !params.contains_key("_state") {
                    return reply("400 Bad Request", &[], b"{\"errors\":[{\"code\":\"DIGEST_INVALID\"}]}");
                }
                store.blobs.insert((repo, digest), body.to_vec());
                reply("201 Created", &[], b"")
            }
            ("PUT", "manifests") => {
                let digest = if behaviour.lie { sha(b"other") } else { sha(body) };
                store.manifests.insert((repo, tail), body.to_vec());
                reply("201 Created", &[format!("Docker-Content-Digest: {digest}")], b"")
            }
            _ => reply("405 Method Not Allowed", &[], b""),
        }
    }

    struct Static(Option<Credentials>);

    impl CredentialSource for Static {
        fn credentials(&self, _: &str) -> Result<Option<Credentials>, RegistryError> {
            Ok(self.0.clone())
        }
    }

    fn client(dir: &Path) -> Registry {
        Registry::with(dir.join("cache"), Box::new(Static(None)), None)
    }

    /// An image of `packages` (`name`, with its file's content), a layer each.
    fn image(dir: &Path, name: &str, packages: &[(&str, &str)]) -> std::path::PathBuf {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let packages = packages
            .iter()
            .map(|(package, content)| {
                testing::package(
                    &root,
                    testing::Spec::new(package).file("share/data.txt", *content, false, None),
                )
            })
            .collect();
        let plan = testing::plan_with(packages, crate::plan::Layering::PerPackage, 100);
        let output = root.join("image.tar");
        testing::build_image(&root, &plan, &output);
        output
    }

    fn destination(fake: &Fake, path: &str) -> ImageRef {
        ImageRef::parse(&format!("{}/{path}", fake.host)).unwrap()
    }

    #[test]
    fn a_second_push_finds_everything_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a"), ("beta", "b")]);
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app:1.0");
        let mut progress = Vec::new();
        let first = push(&path, None, &to, &client(dir.path()), &mut progress).unwrap();
        assert_eq!((first.uploaded, first.mounted, first.present), (3, 0, 0));
        assert_eq!(first.reference, format!("{}/team/app:1.0", fake.host));
        let progress = String::from_utf8(progress).unwrap();
        assert!(
            progress.starts_with("[1/3] sha256:") && progress.contains(" uploading ("),
            "{progress}"
        );
        let stored = fake.store.lock().unwrap().manifests[&("team/app".to_string(), "1.0".to_string())].clone();
        assert_eq!(sha(&stored), first.digest);
        assert_eq!(
            String::from_utf8(stored).unwrap(),
            raw(&path, None, Raw::Manifest).unwrap()
        );

        let second = push(&path, None, &to, &client(dir.path()), &mut std::io::sink()).unwrap();
        assert_eq!((second.uploaded, second.mounted, second.present), (0, 0, 3));
        assert_eq!(second.digest, first.digest);
        assert!(
            second
                .text()
                .contains("3 blobs: 0 uploaded (0 B), 0 mounted, 3 already there"),
            "{}",
            second.text()
        );
        let json: serde_json::Value = serde_json::from_str(&second.json()).unwrap();
        assert_eq!(json["present"], 3);
    }

    #[test]
    fn changing_one_package_uploads_its_layer_and_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app");
        // One alpha package for both versions: a package's record names its archive's location, so alpha must be
        // the same archive for its layer to be the same.
        let alpha = testing::package(
            dir.path(),
            testing::Spec::new("alpha").file("share/a.txt", "a", false, None),
        );
        let beta = |version: &str, content: &str| {
            let root = dir.path().join(version);
            std::fs::create_dir_all(&root).unwrap();
            testing::package(
                &root,
                testing::Spec::new("beta").file("share/b.txt", content, false, None),
            )
        };
        let mut digests = Vec::new();
        for (version, content) in [("v1", "b"), ("v2", "b, changed")] {
            let plan = testing::plan_with(
                vec![alpha.clone(), beta(version, content)],
                crate::plan::Layering::PerPackage,
                100,
            );
            let path = dir.path().join(format!("{version}.tar"));
            testing::build_image(dir.path(), &plan, &path);
            let pushed = push(&path, None, &to, &client(dir.path()), &mut std::io::sink()).unwrap();
            digests.push((pushed.uploaded, pushed.present));
        }
        // The changed package's layer, and the config whose diff_ids changed with it; alpha's layer is reused.
        assert_eq!(digests, [(3, 0), (2, 1)]);
    }

    #[test]
    fn a_base_on_the_same_registry_lends_its_layers() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let mut base = testing::base_image(dir.path(), &[testing::BaseEntry::File("bin/sh", b"sh".to_vec(), 0o755)]);
        base.reference = ImageRef::parse(&format!("{}/os/base:9@sha256:{}", fake.host, "b".repeat(64))).unwrap();
        let layer = std::fs::read(&base.layers[0].path).unwrap();
        fake.store
            .lock()
            .unwrap()
            .blobs
            .insert(("os/base".to_string(), base.layers[0].digest.clone()), layer);
        let plan = testing::plan_of(vec![testing::simple(dir.path(), "tool", &[])], 100);
        let path = dir.path().join("on-base.tar");
        let options = crate::build::BuildOptions {
            prefix: "/opt/env".to_string(),
            output: path.clone(),
            force: true,
            reference: crate::image::Reference::parse("app:1").unwrap(),
            env: Vec::new(),
            entrypoint: None,
            cmd: None,
            workdir: None,
            source_date_epoch: 0,
            max_memory: 16 << 20,
            base: Some(base),
        };
        crate::build::build(
            &plan,
            &options,
            &crate::fetch::Fetcher::new(dir.path().join("cache")),
            &mut std::io::sink(),
        )
        .unwrap();
        let mut progress = Vec::new();
        let pushed = push(
            &path,
            None,
            &destination(&fake, "team/app:1"),
            &client(dir.path()),
            &mut progress,
        )
        .unwrap();
        assert_eq!((pushed.uploaded, pushed.mounted, pushed.present), (2, 1, 0));
        assert!(String::from_utf8(progress).unwrap().contains("mounted from os/base"));
    }

    #[test]
    fn writes_ask_for_a_push_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let fake = fake(Behaviour {
            tokens: true,
            lie: false,
        });
        let pushed = push(
            &path,
            None,
            &destination(&fake, "team/app:1"),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(pushed.uploaded, 2);
        let scopes = fake.store.lock().unwrap().scopes.clone();
        assert!(
            scopes.iter().all(|scope| scope == "repository:team/app:pull,push"),
            "{scopes:?}"
        );
        assert_eq!(scopes.len(), 1, "one token for the whole push: {scopes:?}");
    }

    #[test]
    fn a_push_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake(Behaviour::default());
        let to = destination(&fake, "team/app:1");
        let err = push(dir.path(), None, &to, &client(dir.path()), &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, PushError::Invalid { problems: 2, .. }), "{err}");
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let pinned = destination(&fake, &format!("team/app@sha256:{}", "0".repeat(64)));
        let err = push(&path, None, &pinned, &client(dir.path()), &mut std::io::sink()).unwrap_err();
        assert!(matches!(err, PushError::WrongDigest { .. }), "{err}");
        assert!(fake.store.lock().unwrap().requests.is_empty());
    }

    #[test]
    fn a_push_by_digest_and_a_registry_that_disagrees() {
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "v1", &[("alpha", "a")]);
        let digest = inspect(&path, None).unwrap().manifest.digest;
        let fake = fake(Behaviour::default());
        let pushed = push(
            &path,
            None,
            &destination(&fake, &format!("team/app@{digest}")),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap();
        assert_eq!(pushed.reference, format!("{}/team/app@{digest}", fake.host));
        assert!(
            fake.store
                .lock()
                .unwrap()
                .manifests
                .contains_key(&("team/app".to_string(), digest.clone()))
        );

        let liar = self::fake(Behaviour {
            tokens: false,
            lie: true,
        });
        let err = push(
            &path,
            None,
            &destination(&liar, "team/app:1"),
            &client(dir.path()),
            &mut std::io::sink(),
        )
        .unwrap_err();
        assert!(matches!(err, PushError::Stored { .. }), "{err}");
    }
}
