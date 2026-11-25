//! Trust-on-first-use certificate pinning.

use std::sync::{Arc, Mutex};

use quinn::crypto::rustls::QuicClientConfig;
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

pub type Fingerprint = [u8; 32];
pub type SeenSlot = Arc<Mutex<Option<Fingerprint>>>;

pub const FINGERPRINT_CHANGED: &str = "fingerprint changed";

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    let d = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

pub fn to_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode lowercase or uppercase hex of any even length. No whitespace or prefix allowed.
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

pub fn from_hex(s: &str) -> Option<Fingerprint> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    hex_decode(s)?.try_into().ok()
}

#[derive(Debug)]
pub struct TofuVerifier {
    pub expected: Option<Fingerprint>,
    pub seen: SeenSlot,
    pub provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        *self.seen.lock().expect("poisoned") = Some(fp);
        match self.expected {
            Some(exp) if exp != fp => Err(rustls::Error::General(FINGERPRINT_CHANGED.into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not supported".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Rustls client config with the TOFU verifier (TLS 1.3, ring, ALPN `vc/1`).
pub fn rustls_client_config(expected: Option<Fingerprint>) -> (rustls::ClientConfig, SeenSlot) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen: SeenSlot = Arc::new(Mutex::new(None));
    let verifier = TofuVerifier {
        expected,
        seen: seen.clone(),
        provider: provider.clone(),
    };
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3 supported by ring provider")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![vc_proto::ALPN.to_vec()];
    (cfg, seen)
}

/// Quinn client config plus the slot that receives the fingerprint actually seen.
pub fn client_config(expected: Option<Fingerprint>) -> (quinn::ClientConfig, SeenSlot) {
    let (cfg, seen) = rustls_client_config(expected);
    let qc = QuicClientConfig::try_from(Arc::new(cfg)).expect("TLS 1.3 config with initial suite");
    (quinn::ClientConfig::new(Arc::new(qc)), seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tofu_pins_then_detects_change() {
        let a = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let fa = fingerprint(a.cert.der());
        let fb = fingerprint(b.cert.der());
        assert_ne!(fa, fb);
        assert_eq!(from_hex(&to_hex(&fa)), Some(fa));
        assert_eq!(from_hex("zz"), None);
        let name = ServerName::try_from("localhost").unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());

        // First use: no expectation → accepted, fingerprint recorded.
        let v = TofuVerifier {
            expected: None,
            seen: Arc::new(Mutex::new(None)),
            provider: provider.clone(),
        };
        assert!(
            v.verify_server_cert(a.cert.der(), &[], &name, &[], UnixTime::now())
                .is_ok()
        );
        assert_eq!(*v.seen.lock().unwrap(), Some(fa));

        // Pinned to A: A accepted, B rejected but B's fingerprint is recorded.
        let v = TofuVerifier {
            expected: Some(fa),
            seen: Arc::new(Mutex::new(None)),
            provider,
        };
        assert!(
            v.verify_server_cert(a.cert.der(), &[], &name, &[], UnixTime::now())
                .is_ok()
        );
        let e = v
            .verify_server_cert(b.cert.der(), &[], &name, &[], UnixTime::now())
            .unwrap_err();
        assert!(e.to_string().contains(FINGERPRINT_CHANGED), "{e}");
        assert_eq!(*v.seen.lock().unwrap(), Some(fb));
        assert!(!v.supported_verify_schemes().is_empty());
    }
}
