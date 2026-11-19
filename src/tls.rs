//! Which certificates HTTPS connections trust, as pixi decides: the operating system's store by default, so a
//! company's own CA (a TLS-inspecting proxy, a private registry) is trusted wherever the system trusts it.
//!
//! `SSL_CERT_FILE` names a PEM bundle to trust instead. Otherwise pixi's `tls-root-certs` decides: `system` (the
//! default; `native` and `all` are its old names) or `webpki`, the Mozilla roots built into the binary.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rattler_config::config::tls::TlsRootCerts;
use ureq::tls::{RootCerts, TlsConfig};

/// The TLS settings every HTTPS client of pixi-oci's uses; read once.
pub fn config() -> TlsConfig {
    static CONFIG: OnceLock<TlsConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let file = std::env::var_os("SSL_CERT_FILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            let roots = roots(file.as_deref(), pixi_setting()).unwrap_or_else(|message| {
                eprintln!("warning: {message}; using the system's certificates");
                RootCerts::PlatformVerifier
            });
            TlsConfig::builder().root_certs(roots).build()
        })
        .clone()
}

/// The roots for a bundle file, if one is named, else for pixi's setting.
fn roots(file: Option<&Path>, setting: Option<TlsRootCerts>) -> Result<RootCerts, String> {
    if let Some(file) = file {
        return bundle(file);
    }
    Ok(match setting.unwrap_or_default() {
        TlsRootCerts::System => RootCerts::PlatformVerifier,
        TlsRootCerts::Webpki => RootCerts::WebPki,
    })
}

/// The certificates in a PEM bundle.
fn bundle(file: &Path) -> Result<RootCerts, String> {
    let shown = file.display();
    let pem = std::fs::read(file).map_err(|err| format!("could not read SSL_CERT_FILE {shown}: {err}"))?;
    let mut certs = Vec::new();
    for item in ureq::tls::parse_pem(&pem) {
        if let ureq::tls::PemItem::Certificate(cert) =
            item.map_err(|err| format!("SSL_CERT_FILE {shown} is not a PEM bundle: {err}"))?
        {
            certs.push(cert);
        }
    }
    if certs.is_empty() {
        return Err(format!("SSL_CERT_FILE {shown} holds no certificates"));
    }
    Ok(RootCerts::new_with_certs(&certs))
}

/// pixi's `tls-root-certs`, from its config files and the current workspace's `.pixi/config.toml`.
fn pixi_setting() -> Option<TlsRootCerts> {
    let mut locations: Vec<_> = rattler_config::locations::config_search_paths("pixi")
        .into_iter()
        .filter(|location| location.path.is_file())
        .collect();
    let local = Path::new(".pixi").join("config.toml");
    if local.is_file() {
        locations.push(rattler_config::locations::ConfigLocation {
            path: local,
            layer: rattler_config::locations::ConfigLayer::Tool,
        });
    }
    rattler_config::config::ConfigBase::<rattler_config::config::NoExtension>::load_from_locations(locations)
        .ok()
        .and_then(|config| config.tls_root_certs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A self-signed certificate, only for parsing.
    const CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBfTCCASOgAwIBAgIUJ6kX6bU0b0Yq5o3Xb1s3W8d4Q0EwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJcGl4aS1vY2kwHhcNMjYwMTAxMDAwMDAwWhcNMzYwMTAxMDAw
MDAwWjAUMRIwEAYDVQQDDAlwaXhpLW9jaTBZMBMGByqGSM49AgEGCCqGSM49AwEH
A0IABHk2E7q2yPZp8bHjZ2w0yq0G5zN7mJcQm1+3r8KkqzG8m4XlQhJk2rS7y1lS
cJm2n7Gq7mJ3m2b8F7v3Kq3hQ6ajUzBRMB0GA1UdDgQWBBQh2V3k1o7z8n2m3b6Q
9pZ8x2k1sDAfBgNVHSMEGDAWgBQh2V3k1o7z8n2m3b6Q9pZ8x2k1sDAPBgNVHRMB
Af8EBTADAQH/MAoGCCqGSM49BAMCA0gAMEUCIQDf
-----END CERTIFICATE-----
";

    #[test]
    fn the_system_store_is_the_default_as_in_pixi() {
        assert!(matches!(roots(None, None), Ok(RootCerts::PlatformVerifier)));
        assert!(matches!(
            roots(None, Some(TlsRootCerts::System)),
            Ok(RootCerts::PlatformVerifier)
        ));
        assert!(matches!(roots(None, Some(TlsRootCerts::Webpki)), Ok(RootCerts::WebPki)));
    }

    #[test]
    fn ssl_cert_file_wins_over_pixis_setting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ca.pem");
        std::fs::write(&file, CERT).unwrap();
        match roots(Some(&file), Some(TlsRootCerts::Webpki)) {
            Ok(RootCerts::Specific(certs)) => assert_eq!(certs.len(), 1),
            other => panic!("expected the bundle's certificate, got {other:?}"),
        }
    }

    #[test]
    fn a_bundle_that_is_missing_or_empty_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let missing = roots(Some(&dir.path().join("none.pem")), None).unwrap_err();
        assert!(missing.contains("could not read SSL_CERT_FILE"), "{missing}");
        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let none = roots(Some(&empty), None).unwrap_err();
        assert!(none.contains("holds no certificates"), "{none}");
    }

    #[test]
    fn pixis_config_names_the_setting() {
        let (config, _) = rattler_config::config::ConfigBase::<rattler_config::config::NoExtension>::from_toml_str(
            "tls-root-certs = \"webpki\"\n",
        )
        .unwrap();
        assert_eq!(config.tls_root_certs, Some(TlsRootCerts::Webpki));
    }
}
