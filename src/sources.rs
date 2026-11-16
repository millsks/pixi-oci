//! Where a package is downloaded from: pixi's `mirrors`, and the `s3://`, `gcs://` and `oci://` locations that
//! rattler's own middleware signs and resolves, as pixi does.
//!
//! pixi's config is read from the files pixi reads (`/etc/pixi`, the user's config directory, `$PIXI_HOME` or
//! `~/.pixi`, with rattler's shared files under them) and the workspace's `.pixi/config.toml`, the last taking
//! precedence. Only `mirrors` and `s3-options` are used.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use rattler_networking::s3_middleware::S3Config;
use sha2::{Digest, Sha256};

/// The mirrors and S3 settings from pixi's config.
#[derive(Clone, Default)]
pub struct Sources {
    /// Each channel URL a mirror stands in for, longest first as rattler matches them, with its mirrors in order.
    mirrors: Vec<(String, Vec<String>)>,
    s3: HashMap<String, S3Config>,
}

/// Why pixi's config could not be read.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("could not read pixi's config: {message}")]
#[diagnostic(help("pixi-oci reads `mirrors` and `s3-options` from it as pixi does; `pixi config list` shows it"))]
pub struct ConfigError {
    message: String,
}

impl Sources {
    /// pixi's config as pixi reads it for a workspace in `workspace`.
    pub fn from_pixi_config(workspace: Option<&Path>) -> Result<Self, ConfigError> {
        Self::from_locations(rattler_config::locations::config_search_paths("pixi"), workspace)
    }

    /// The config in those of `search` that exist, then in the workspace's `.pixi/config.toml`, later ones winning.
    fn from_locations(
        search: Vec<rattler_config::locations::ConfigLocation>,
        workspace: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        let mut locations: Vec<rattler_config::locations::ConfigLocation> =
            search.into_iter().filter(|location| location.path.is_file()).collect();
        if let Some(file) = workspace.map(|dir| dir.join(".pixi").join("config.toml"))
            && file.is_file()
        {
            locations.push(rattler_config::locations::ConfigLocation {
                path: file,
                layer: rattler_config::locations::ConfigLayer::Tool,
            });
        }
        let config =
            rattler_config::config::ConfigBase::<rattler_config::config::NoExtension>::load_from_locations(locations)
                .map_err(|err| ConfigError {
                message: err.to_string(),
            })?;
        Ok(Self::from_config(&config))
    }

    /// The sources a config gives.
    pub fn from_config(config: &rattler_config::config::CommonConfig) -> Self {
        let slash = |url: &url::Url| {
            let text = url.to_string();
            if text.ends_with('/') { text } else { format!("{text}/") }
        };
        let mut mirrors: Vec<(String, Vec<String>)> = config
            .mirrors
            .iter()
            .map(|(channel, mirrors)| (slash(channel), mirrors.iter().map(slash).collect()))
            .collect();
        mirrors.sort_by_key(|(channel, _)| std::cmp::Reverse(channel.len()));
        Sources {
            mirrors,
            s3: rattler_networking::s3_middleware::compute_s3_config_from_config(config),
        }
    }

    /// The pixi config in `toml`, as a file of it would give it; for tests.
    #[cfg(test)]
    pub fn from_toml(toml: &str) -> Self {
        let (config, _) =
            rattler_config::config::ConfigBase::<rattler_config::config::NoExtension>::from_toml_str(toml).unwrap();
        Self::from_config(&config)
    }

    /// Where to download `location` from, in the order to try: its mirrors, if a mirror stands in for its channel,
    /// else the location itself.
    pub fn candidates(&self, location: &str) -> Vec<String> {
        for (channel, mirrors) in &self.mirrors {
            if let Some(rest) = location.strip_prefix(channel.as_str()) {
                return mirrors.iter().map(|mirror| format!("{mirror}{rest}")).collect();
            }
        }
        vec![location.to_string()]
    }
}

/// Whether `url` is downloaded through rattler's middleware rather than over plain http.
pub fn is_remote_scheme(url: &str) -> bool {
    url.split_once("://")
        .is_some_and(|(scheme, _)| matches!(scheme, "s3" | "gcs" | "oci"))
}

/// Why a download through rattler's middleware failed.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}")]
    Request(String),
    #[error("the server answered {0}")]
    Status(u16),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A client for `s3://`, `gcs://` and `oci://` downloads, made the first time one is needed: rattler's S3, GCS and
/// OCI middleware over reqwest, run on a runtime of its own.
pub struct Remote {
    sources: Sources,
    storage: rattler_networking::AuthenticationStorage,
    made: OnceLock<Result<(tokio::runtime::Runtime, reqwest_middleware::ClientWithMiddleware), String>>,
}

impl Remote {
    pub fn new(sources: Sources, storage: rattler_networking::AuthenticationStorage) -> Self {
        Remote {
            sources,
            storage,
            made: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&(tokio::runtime::Runtime, reqwest_middleware::ClientWithMiddleware), RemoteError> {
        self.made
            .get_or_init(|| {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| err.to_string())?;
                let client = reqwest::Client::builder()
                    .user_agent(concat!("pixi-oci/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|err| err.to_string())?;
                let with = reqwest_middleware::ClientBuilder::new(client.clone())
                    .with(
                        rattler_networking::OciMiddleware::new(client)
                            .with_authentication_storage(self.storage.clone()),
                    )
                    .with(rattler_networking::s3_middleware::S3Middleware::new(
                        self.sources.s3.clone(),
                        self.storage.clone(),
                    ))
                    .with(rattler_networking::GCSMiddleware::default())
                    .with(rattler_networking::AuthenticationMiddleware::from_auth_storage(
                        self.storage.clone(),
                    ))
                    .build();
                Ok((runtime, with))
            })
            .as_ref()
            .map_err(|message| RemoteError::Request(message.clone()))
    }

    /// Stream `url` into `out`, returning the sha256 of what was written.
    pub fn download(&self, url: &str, out: &mut impl Write) -> Result<String, RemoteError> {
        let (runtime, client) = self.client()?;
        let parsed = url::Url::parse(url).map_err(|err| RemoteError::Request(err.to_string()))?;
        runtime.block_on(async {
            let mut response = client.get(parsed).send().await.map_err(|err| match err {
                // reqwest names the URL it sent, which for S3 is presigned: a key id and a signature in its query.
                reqwest_middleware::Error::Reqwest(err) => RemoteError::Request(error_chain(&err.without_url())),
                other => RemoteError::Request(error_chain(&other)),
            })?;
            let status = response.status().as_u16();
            if !response.status().is_success() {
                return Err(RemoteError::Status(status));
            }
            let mut hasher = Sha256::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| RemoteError::Request(error_chain(&err.without_url())))?
            {
                hasher.update(&chunk);
                out.write_all(&chunk)?;
            }
            Ok(crate::fetch::hex(&hasher.finalize()))
        })
    }
}

/// An error and its causes, on one line: reqwest's own message rarely says what went wrong.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_channel_a_mirror_stands_in_for_wins() {
        let sources = Sources::from_toml(
            r#"
[mirrors]
"https://conda.anaconda.org" = ["https://mirror.example/all"]
"https://conda.anaconda.org/conda-forge/" = ["https://mirror.example/cf/", "oci://ghcr.io/channel-mirrors/conda-forge"]
"#,
        );
        assert_eq!(
            sources.candidates("https://conda.anaconda.org/conda-forge/noarch/a-1-0.conda"),
            [
                "https://mirror.example/cf/noarch/a-1-0.conda",
                "oci://ghcr.io/channel-mirrors/conda-forge/noarch/a-1-0.conda"
            ]
        );
        assert_eq!(
            sources.candidates("https://conda.anaconda.org/bioconda/linux-64/b-1-0.conda"),
            ["https://mirror.example/all/bioconda/linux-64/b-1-0.conda"]
        );
        assert_eq!(
            sources.candidates("https://prefix.dev/x/noarch/c.conda"),
            ["https://prefix.dev/x/noarch/c.conda"]
        );
        assert_eq!(Sources::default().candidates("s3://b/c.conda"), ["s3://b/c.conda"]);
    }

    #[test]
    fn schemes_for_the_middleware_are_known() {
        for remote in ["s3://b/c.conda", "gcs://b/c.conda", "oci://ghcr.io/c/a.conda"] {
            assert!(is_remote_scheme(remote), "{remote}");
        }
        for plain in ["https://h/c.conda", "http://h/c.conda", "file:///c.conda", "c.conda"] {
            assert!(!is_remote_scheme(plain), "{plain}");
        }
    }

    #[test]
    fn pixis_config_files_and_the_workspaces_are_read() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".pixi")).unwrap();
        std::fs::write(
            workspace.path().join(".pixi/config.toml"),
            "[mirrors]\n\"https://conda.anaconda.org/conda-forge\" = [\"https://mirror.example/cf\"]\n",
        )
        .unwrap();
        // A user file and the workspace's: the workspace's wins for the channel both name.
        let user = home.path().join("config.toml");
        std::fs::write(
            &user,
            "[mirrors]\n\"https://conda.anaconda.org/conda-forge\" = [\"https://user.example/cf\"]\n\"https://repo.example/x\" = [\"https://user.example/x\"]\n",
        )
        .unwrap();
        let search = || {
            vec![
                rattler_config::locations::ConfigLocation {
                    path: home.path().join("missing.toml"),
                    layer: rattler_config::locations::ConfigLayer::Shared,
                },
                rattler_config::locations::ConfigLocation {
                    path: user.clone(),
                    layer: rattler_config::locations::ConfigLayer::Tool,
                },
            ]
        };
        let sources = Sources::from_locations(search(), Some(workspace.path())).unwrap();
        assert_eq!(
            sources.candidates("https://repo.example/x/noarch/b.conda"),
            ["https://user.example/x/noarch/b.conda"]
        );
        assert_eq!(
            sources.candidates("https://conda.anaconda.org/conda-forge/noarch/a.conda"),
            ["https://mirror.example/cf/noarch/a.conda"]
        );
        std::fs::write(workspace.path().join(".pixi/config.toml"), "mirrors = 3\n").unwrap();
        let err = Sources::from_locations(search(), Some(workspace.path())).err().unwrap();
        assert!(err.to_string().starts_with("could not read pixi's config"), "{err}");
        assert!(Sources::from_locations(Vec::new(), None).is_ok());
    }
}
