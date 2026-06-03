/// TLS configuration helpers.
///
/// Thin wrappers around `rustls` and `rustls-pemfile` that load server
/// certificates / keys and build client root stores.
use std::fs;
use std::io::BufReader;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::error::HttpError;

// ---------------------------------------------------------------------------
// Server TLS config
// ---------------------------------------------------------------------------

/// Load a `rustls::ServerConfig` from PEM certificate and key files.
///
/// `cert_file` may contain a certificate chain (leaf first).
/// `key_file` must contain a single RSA, ECDSA, or PKCS#8 private key.
pub fn server_config(
    cert_file: &str,
    key_file:  &str,
) -> Result<Arc<rustls::ServerConfig>, HttpError> {
    let certs = load_certs(cert_file)?;
    let key   = load_private_key(key_file)?;

    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| HttpError::Tls(e.to_string()))?;

    Ok(Arc::new(cfg))
}

// ---------------------------------------------------------------------------
// Client TLS config
// ---------------------------------------------------------------------------

/// Build a `rustls::ClientConfig` that trusts the Mozilla root CA bundle
/// (via `webpki-roots`).
///
/// This is the equivalent of Go's default `http.Transport` which uses the
/// system root store.  For custom CA certificates call `client_config_with_roots`.
pub fn default_client_config() -> Arc<rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }))
}

/// Build a `rustls::ClientConfig` that trusts a custom PEM CA certificate file
/// in addition to the Mozilla roots.
pub fn client_config_with_ca(ca_file: &str) -> Result<Arc<rustls::ClientConfig>, HttpError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let ca_certs = load_certs(ca_file)?;
    for cert in ca_certs {
        roots.add(cert).map_err(|e| HttpError::Tls(e.to_string()))?;
    }

    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

// ---------------------------------------------------------------------------
// PEM loading helpers
// ---------------------------------------------------------------------------

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, HttpError> {
    let f   = fs::File::open(path).map_err(|e| HttpError::Io(e))?;
    let mut r = BufReader::new(f);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| HttpError::Tls(e.to_string()))
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, HttpError> {
    let f   = fs::File::open(path).map_err(HttpError::Io)?;
    let mut r = BufReader::new(f);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| HttpError::Tls(e.to_string()))?
        .ok_or_else(|| HttpError::Tls(format!("no private key found in {path}")))
}
