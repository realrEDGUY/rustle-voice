//! TOFU pinning and handshake checks against real QUIC servers.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use vc_client::app::UiCommand;
use vc_client::audio::FakeMode;
use vc_client::session::ConnStatus;

#[test]
fn tofu_first_connect_pins_fingerprint() {
    let srv = TestServer::start(|_| {});
    let mut c = client_with(FakeMode::Silent);
    let k = c.add("S", srv.addr(), "alice");
    assert_eq!(c.app.model.saved_server(k).unwrap().fingerprint, None);
    c.connect(k);
    c.wait(
        |a| a.model.saved_server(k).unwrap().fingerprint.is_some(),
        "pinned",
    );
    assert_eq!(
        c.app.model.saved_server(k).unwrap().fingerprint.as_deref(),
        Some(srv.fingerprint_hex().as_str())
    );
    let on_disk = std::fs::read_to_string(c.dir.path().join("servers.toml")).unwrap();
    assert!(on_disk.contains(&srv.fingerprint_hex()), "{on_disk}");
}

#[test]
fn restart_with_new_cert_triggers_fingerprint_changed() {
    let mut srv = TestServer::start(|_| {});
    let old_fp = srv.fingerprint_hex();
    let mut c = client_with(FakeMode::Silent);
    let k = c.add("S", srv.addr(), "alice");
    c.connect(k);
    let new_dir = tempfile::tempdir().unwrap();
    srv.restart_with_cert_dir(new_dir.path().to_path_buf());
    let new_fp = srv.fingerprint_hex();
    assert_ne!(old_fp, new_fp);
    c.wait(|a| a.model.ui.tofu_prompt.is_some(), "TOFU prompt");
    let (pk, seen) = c.app.model.ui.tofu_prompt.unwrap();
    assert_eq!(pk, k);
    assert_eq!(vc_client::tofu::to_hex(&seen), new_fp);
    assert_eq!(
        c.app.model.status(k),
        ConnStatus::Failed("Server identity changed".into())
    );
    // Stays failed (no silent retry) and the old pin is kept until the user decides.
    std::thread::sleep(Duration::from_millis(500));
    c.app.poll();
    assert!(matches!(c.app.model.status(k), ConnStatus::Failed(_)));
    assert_eq!(
        c.app.model.saved_server(k).unwrap().fingerprint.as_deref(),
        Some(old_fp.as_str())
    );
    c.app.dispatch(vec![UiCommand::TrustNewIdentity]);
    c.wait(
        |a| a.model.status(k) == ConnStatus::Connected,
        "connected after trust",
    );
    assert_eq!(
        c.app.model.saved_server(k).unwrap().fingerprint.as_deref(),
        Some(new_fp.as_str())
    );
}

#[derive(Debug)]
struct WrongKey(Arc<CertifiedKey>);

impl ResolvesServerCert for WrongKey {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

fn raw_server(
    rt: &tokio::runtime::Runtime,
    tls: rustls::ServerConfig,
    datagrams: bool,
) -> (quinn::Endpoint, std::net::SocketAddr) {
    let qsc = QuicServerConfig::try_from(Arc::new(tls)).unwrap();
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qsc));
    let mut t = quinn::TransportConfig::default();
    if !datagrams {
        t.datagram_receive_buffer_size(None);
    }
    sc.transport_config(Arc::new(t));
    let _g = rt.enter();
    let ep = quinn::Endpoint::server(sc, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = ep.local_addr().unwrap();
    let acc = ep.clone();
    rt.spawn(async move {
        while let Some(inc) = acc.accept().await {
            tokio::spawn(async move {
                if let Ok(conn) = inc.await {
                    // Hold the connection open until the peer goes away.
                    let _ = conn.closed().await;
                }
            });
        }
    });
    (ep, addr)
}

#[test]
fn pinned_cert_with_different_key_fails_handshake() {
    let cert_dir = tempfile::tempdir().unwrap();
    let (cert, _key) = vc_server::tls::load_or_create_cert(cert_dir.path()).unwrap();
    let fp_hex = vc_server::tls::fingerprint_hex(&vc_server::tls::fingerprint(cert.as_ref()));
    // Positive control: the real server with that cert + its own key connects.
    let real = TestServer::start_cfg(server_config(cert_dir.path()));
    let mut ok = client_with(FakeMode::Silent);
    let kok = ok.add("S", real.addr(), "alice");
    ok.app
        .model
        .saved
        .iter_mut()
        .find(|s| s.key == kok)
        .unwrap()
        .fingerprint = Some(fp_hex.clone());
    ok.connect(kok);
    assert_eq!(real.fingerprint_hex(), fp_hex);

    // Impostor: presents the pinned cert's DER but signs with a different key.
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let wrong_key =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(other.signing_key.serialize_der()));
    let signer = rustls::crypto::ring::sign::any_supported_type(&wrong_key).unwrap();
    let ck = Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(cert.to_vec())],
        signer,
    ));
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(WrongKey(ck)));
    tls.alpn_protocols = vec![vc_proto::ALPN.to_vec()];
    let rt = runtime();
    let (_ep, addr) = raw_server(&rt, tls, true);

    let mut c = client_with(FakeMode::Silent);
    let k = c.add("Impostor", addr, "alice");
    c.app
        .model
        .saved
        .iter_mut()
        .find(|s| s.key == k)
        .unwrap()
        .fingerprint = Some(fp_hex.clone());
    c.app.dispatch(vec![UiCommand::Connect(k)]);
    c.wait(
        |a| matches!(a.model.status(k), ConnStatus::Reconnecting { .. }),
        "handshake failure leads to a retry",
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        c.app.poll();
        assert_ne!(
            c.app.model.status(k),
            ConnStatus::Connected,
            "must never connect"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        c.app.model.ui.tofu_prompt.is_none(),
        "same fingerprint, so not an identity change: the signature check failed"
    );
    assert!(c.app.model.session(k).unwrap().my_id.is_none());
}

#[test]
fn datagram_too_small_reports_error() {
    let cert_dir = tempfile::tempdir().unwrap();
    let (cert, key) = vc_server::tls::load_or_create_cert(cert_dir.path()).unwrap();
    let tls = vc_server::tls::rustls_server_config(cert, key).unwrap();
    let rt = runtime();
    let (_ep, addr) = raw_server(&rt, tls, false);
    let mut c = client_with(FakeMode::Silent);
    let k = c.add("NoDgram", addr, "alice");
    c.app.dispatch(vec![UiCommand::Connect(k)]);
    c.wait(
        |a| matches!(a.model.status(k), ConnStatus::Failed(_)),
        "failed",
    );
    assert_eq!(
        c.app.model.status(k),
        ConnStatus::Failed("server does not support voice datagrams".into())
    );
}
