//! Self-signed certificate handling and the QUIC server crypto config.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::config::ServerConfig;

/// Load `cert.der` / `key.der` from `dir`, generating and writing them on first run.
pub fn load_or_create_cert(
    dir: &Path,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    if cert_path.exists() && key_path.exists() {
        let cert =
            std::fs::read(&cert_path).with_context(|| format!("read {}", cert_path.display()))?;
        let key =
            std::fs::read(&key_path).with_context(|| format!("read {}", key_path.display()))?;
        return Ok((
            CertificateDer::from(cert),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        ));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("generate self-signed certificate")?;
    let cert = ck.cert.der().to_vec();
    let key = ck.signing_key.serialize_der();
    std::fs::write(&cert_path, &cert).with_context(|| format!("write {}", cert_path.display()))?;
    std::fs::write(&key_path, &key).with_context(|| format!("write {}", key_path.display()))?;
    Ok((
        CertificateDer::from(cert),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    ))
}

/// SHA-256 of the certificate DER.
pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Lowercase hex of a fingerprint.
pub fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Transport settings shared by the server config.
pub fn transport_config(cfg: &ServerConfig) -> Result<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        Duration::from_millis(cfg.idle_timeout_ms)
            .try_into()
            .context("idle timeout out of range")?,
    ));
    t.keep_alive_interval(Some(Duration::from_millis(cfg.keep_alive_ms)));
    // Blob (uni) streams are refused until a peer is welcomed; the net layer then
    // raises this per connection with `set_max_concurrent_uni_streams`.
    t.max_concurrent_uni_streams(0u32.into());
    t.max_concurrent_bidi_streams(1u32.into());
    Ok(t)
}

/// Build the rustls (TLS 1.3 only, ring, ALPN `vc/1`) config for the server.
pub fn rustls_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![cert], key)?;
    cfg.alpn_protocols = vec![vc_proto::ALPN.to_vec()];
    Ok(cfg)
}

/// Build the quinn server config from a cert/key and the server config.
pub fn server_crypto(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    cfg: &ServerConfig,
) -> Result<quinn::ServerConfig> {
    let tls = rustls_server_config(cert, key)?;
    let qsc = QuicServerConfig::try_from(Arc::new(tls)).context("quic server config")?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qsc));
    sc.transport_config(Arc::new(transport_config(cfg)?));
    Ok(sc)
}
