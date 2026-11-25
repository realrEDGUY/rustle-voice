//! Minimal test client built only on vc-proto + quinn (no vc-client dependency).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{Connection, Endpoint, SendStream};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::sync::mpsc;
use vc_proto::blob::{decode_header, encode_header, header_len};
use vc_proto::{
    ADPCM_FRAME_BYTES, ClientBlob, ClientMsg, ClientVoice, FrameDecoder, PROTOCOL_VERSION,
    ServerBlob, ServerMsg, ServerVoice, SessionId,
};

/// Accepts any certificate but still checks the handshake signature.
#[derive(Debug)]
struct AnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
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
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Clone)]
pub struct Opts {
    pub alpn: Vec<u8>,
    /// Spawn a reader that drains the control stream. `false` = never read.
    pub read: bool,
    /// Tiny receive windows, so a non-reading client backs up the server fast.
    pub small_window: bool,
    /// Per-stream receive window only (the connection window stays large), so one
    /// unread blob stream backs up without stalling the others.
    pub stream_window: Option<u32>,
    /// Local UDP socket to bind (default 127.0.0.1:0).
    pub bind: SocketAddr,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            alpn: vc_proto::ALPN.to_vec(),
            read: true,
            small_window: false,
            stream_window: None,
            bind: "127.0.0.1:0".parse().unwrap(),
        }
    }
}

pub struct RawClient {
    pub endpoint: Endpoint,
    pub conn: Connection,
    send: SendStream,
    rx: mpsc::UnboundedReceiver<ServerMsg>,
    pub id: Option<SessionId>,
}

pub fn client_config(opts: &Opts) -> quinn::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![opts.alpn.clone()];
    let qc = QuicClientConfig::try_from(Arc::new(tls)).unwrap();
    let mut cc = quinn::ClientConfig::new(Arc::new(qc));
    let mut t = quinn::TransportConfig::default();
    if opts.small_window {
        t.stream_receive_window(2048u32.into());
        t.receive_window(4096u32.into());
    }
    if let Some(w) = opts.stream_window {
        t.stream_receive_window(w.into());
        t.receive_window(quinn::VarInt::MAX);
    }
    t.keep_alive_interval(Some(Duration::from_millis(200)));
    cc.transport_config(Arc::new(t));
    cc
}

/// Open a QUIC connection (no Hello sent yet).
pub async fn open(addr: SocketAddr, opts: Opts) -> Result<RawClient> {
    let mut endpoint = Endpoint::client(opts.bind)?;
    endpoint.set_default_client_config(client_config(&opts));
    let conn = endpoint
        .connect(addr, "localhost")?
        .await
        .context("quic connect")?;
    let (send, mut recv) = conn.open_bi().await?;
    let (tx, rx) = mpsc::unbounded_channel();
    if opts.read {
        tokio::spawn(async move {
            let mut dec = FrameDecoder::new();
            let mut buf = vec![0u8; 16 * 1024];
            while let Ok(Some(n)) = recv.read(&mut buf).await {
                dec.push(&buf[..n]);
                while let Ok(Some(m)) = dec.next::<ServerMsg>() {
                    if tx.send(m).is_err() {
                        return;
                    }
                }
            }
        });
    } else {
        // Keep the recv half alive without reading it.
        tokio::spawn(async move {
            let _keep = recv;
            let _tx = tx;
            std::future::pending::<()>().await;
        });
    }
    Ok(RawClient {
        endpoint,
        conn,
        send,
        rx,
        id: None,
    })
}

/// Connect and send Hello. Does not wait for Welcome.
pub async fn connect_opts(
    addr: SocketAddr,
    nick: &str,
    password: Option<&str>,
    opts: Opts,
) -> Result<RawClient> {
    let mut c = open(addr, opts).await?;
    c.send(&ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nick: nick.into(),
        password: password.map(Into::into),
        h264: false,
    })
    .await?;
    Ok(c)
}

pub async fn connect(addr: SocketAddr, nick: &str, password: Option<&str>) -> Result<RawClient> {
    connect_opts(addr, nick, password, Opts::default()).await
}

/// Connect, send Hello and wait for Welcome.
pub async fn join(addr: SocketAddr, nick: &str) -> RawClient {
    let mut c = connect(addr, nick, None).await.expect("connect");
    match c.recv_timeout(Duration::from_secs(5)).await {
        Some(ServerMsg::Welcome { session_id, .. }) => c.id = Some(session_id),
        other => panic!("expected Welcome for {nick}, got {other:?}"),
    }
    c
}

impl RawClient {
    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let bytes = vc_proto::encode(msg)?;
        self.send.write_all(&bytes).await?;
        Ok(())
    }

    /// Write raw bytes on the control stream (for malformed-frame tests).
    pub async fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.send.write_all(bytes).await?;
        Ok(())
    }

    pub async fn recv_timeout(&mut self, d: Duration) -> Option<ServerMsg> {
        tokio::time::timeout(d, self.rx.recv()).await.ok().flatten()
    }

    /// Receive until `pred` matches, discarding others.
    pub async fn recv_until(
        &mut self,
        d: Duration,
        mut pred: impl FnMut(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        let deadline = tokio::time::Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let m = tokio::time::timeout(left, self.rx.recv()).await.ok()??;
            if pred(&m) {
                return Some(m);
            }
        }
    }

    pub fn send_voice(&self, seq: u32) {
        let payload = vec![0x11u8; ADPCM_FRAME_BYTES];
        let bytes = ClientVoice {
            seq,
            payload: &payload,
        }
        .encode();
        self.conn
            .send_datagram(Bytes::from(bytes))
            .expect("send datagram");
    }

    /// Send one blob on a new uni stream and finish it.
    pub async fn send_blob(&self, header: &ClientBlob, body: &[u8]) -> Result<()> {
        let mut raw = encode_header(header)?;
        raw.extend_from_slice(body);
        self.send_blob_raw(&raw).await
    }

    /// Send arbitrary bytes as a blob stream (for malformed-header tests).
    pub async fn send_blob_raw(&self, bytes: &[u8]) -> Result<()> {
        let mut s = self.conn.open_uni().await?;
        s.write_all(bytes).await?;
        s.finish()?;
        Ok(())
    }

    /// Accept the next server blob stream and read it whole.
    pub async fn recv_blob_timeout(&self, d: Duration) -> Option<(ServerBlob, Vec<u8>)> {
        tokio::time::timeout(d, async {
            let mut r = self.conn.accept_uni().await.ok()?;
            let mut prefix = [0u8; 4];
            r.read_exact(&mut prefix).await.ok()?;
            let mut json = vec![0u8; header_len(prefix).ok()?];
            r.read_exact(&mut json).await.ok()?;
            let header: ServerBlob = decode_header(&json).ok()?;
            let body = r.read_to_end(64 << 20).await.ok()?;
            Some((header, body))
        })
        .await
        .ok()
        .flatten()
    }

    pub async fn recv_voice_timeout(&self, d: Duration) -> Option<(SessionId, u32)> {
        let b = tokio::time::timeout(d, self.conn.read_datagram())
            .await
            .ok()?
            .ok()?;
        let v = ServerVoice::parse(&b).ok()?;
        Some((v.speaker, v.seq))
    }
}
