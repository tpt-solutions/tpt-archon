//! Optional TLS transport for the PostgreSQL wire server (feature `tls`).
//!
//! The wire protocol runs unchanged over a TLS stream; this module only adds
//! certificate/key loading and the server-side handshake. Enable with
//! `--features tls` and hand a [`TlsConfig`] (loaded from PEM files) to the
//! server. The protocol loop ([`run_protocol`](crate) in `lib.rs`) is
//! generic over `Read + Write`, so the same code serves both plaintext and TLS
//! connections.

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::{Certificate, PrivateKey, ServerConfig, ServerConnection, StreamOwned};

/// TLS material: a PEM-encoded certificate chain and private key.
pub struct TlsConfig {
    cert_chain: Vec<u8>,
    key: Vec<u8>,
}

impl TlsConfig {
    /// Loads the certificate chain and private key from PEM files.
    pub fn from_pem_files(cert_path: &Path, key_path: &Path) -> io::Result<Self> {
        Ok(Self {
            cert_chain: std::fs::read(cert_path)?,
            key: std::fs::read(key_path)?,
        })
    }

    /// Builds a rustls `ServerConfig` (no client-certificate authentication).
    pub fn into_server_config(self) -> io::Result<ServerConfig> {
        let mut cert_reader = std::io::Cursor::new(&self.cert_chain);
        let certs = rustls_pemfile::certs(&mut cert_reader)?;
        if certs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no certificates in chain",
            ));
        }
        let certs: Vec<Certificate> = certs.into_iter().map(Certificate).collect();
        let key = load_private_key(&self.key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certs, PrivateKey(key))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(config)
    }
}

/// Reads the first PEM-encoded private key of any supported format
/// (PKCS#8, RSA, or SEC1/EC) from `pem`. Returns the DER bytes.
fn load_private_key(pem: &[u8]) -> Result<Vec<u8>, String> {
    type Parser = fn(&mut dyn std::io::BufRead) -> Result<Vec<Vec<u8>>, std::io::Error>;
    let parsers: [Parser; 3] = [
        rustls_pemfile::pkcs8_private_keys,
        rustls_pemfile::rsa_private_keys,
        rustls_pemfile::ec_private_keys,
    ];
    for parser in parsers {
        let mut reader = std::io::Cursor::new(pem);
        if let Ok(mut keys) = parser(&mut reader) {
            if let Some(key) = keys.pop() {
                return Ok(key);
            }
        }
    }
    Err("no private key found".to_string())
}

/// Performs the server-side TLS handshake over `stream` using `config`,
/// returning a TLS stream that implements `Read + Write`.
pub fn accept(
    stream: std::net::TcpStream,
    config: Arc<ServerConfig>,
) -> io::Result<StreamOwned<ServerConnection, std::net::TcpStream>> {
    let conn =
        ServerConnection::new(config).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(StreamOwned::new(conn, stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cert_chain_is_rejected() {
        let cfg = TlsConfig {
            cert_chain: Vec::new(),
            key: Vec::new(),
        };
        assert!(cfg.into_server_config().is_err());
    }

    #[test]
    fn missing_pem_files_error() {
        let res = TlsConfig::from_pem_files(
            std::path::Path::new("/nonexistent-cert-archon.pem"),
            std::path::Path::new("/nonexistent-key-archon.pem"),
        );
        assert!(res.is_err());
    }
}
