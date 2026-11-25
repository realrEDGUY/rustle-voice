//! QUIC net layer. One state task owns `ServerState`; one task per connection
//! feeds it events; a bounded writer queue per connection drains control messages.
//! Images and screen frames travel on their own unidirectional streams (one per
//! blob), so they never block the control stream.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, ConnectionError, ReadToEndError, VarInt};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use vc_proto::blob::{decode_header, encode_header, header_len};
use vc_proto::{
    ClientBlob, ClientMsg, ErrorCode, FrameDecoder, MAX_BLOB_STREAMS, MAX_SCREEN_FRAME_BYTES,
    STREAM_BIT, ServerBlob, ServerMsg, SessionId,
};

use crate::config::ServerConfig;
use crate::state::{Action, Event, ServerState, Snapshot};
use crate::tls;

/// The session id after `sid`: never 0 and never carrying [`STREAM_BIT`], which
/// tags stream-sound speakers.
pub fn next_session_id(sid: u32) -> u32 {
    (sid.wrapping_add(1) & !STREAM_BIT).max(1)
}

/// Why a connection ended, as seen by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    Timeout,
    ClosedByPeer,
    ClosedByServer(String),
    Other(String),
}

enum Out {
    Msg(ServerMsg),
    Close(ErrorCode, String),
}

/// Blob streams to one peer that may be in flight at once. Images share one pool;
/// screen frames get one pool per sharer, so one stream never starves another and
/// frames from one sharer stay in order. When a pool is full, further blobs are
/// dropped for that peer (never kicked).
const IMAGE_PERMITS: usize = 8;
const FRAME_PERMITS_PER_SHARER: usize = 1;
/// Upper bound on one blob stream, in either direction.
const BLOB_TIMEOUT: Duration = Duration::from_secs(60);

struct Peer {
    conn: Connection,
    out: mpsc::Sender<Out>,
    images: Arc<Semaphore>,
    /// Frame permits keyed by the sharer the frame comes from, created lazily.
    frames: Mutex<HashMap<SessionId, Arc<Semaphore>>>,
}

enum Ctl {
    Register { sid: SessionId, peer: Peer },
    Ev(Event),
    Snapshot(oneshot::Sender<Snapshot>),
}

/// Handle to a running server.
pub struct ServerHandle {
    pub addr: SocketAddr,
    pub fingerprint: [u8; 32],
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
    ctl: mpsc::Sender<Ctl>,
    disconnects: Arc<Mutex<Vec<(SessionId, DisconnectReason)>>>,
}

impl ServerHandle {
    /// Fingerprint as lowercase hex.
    pub fn fingerprint_hex(&self) -> String {
        tls::fingerprint_hex(&self.fingerprint)
    }

    /// Current channels and users.
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.ctl.send(Ctl::Snapshot(tx)).await.ok()?;
        rx.await.ok()
    }

    /// Every connection end seen so far, with its reason.
    pub fn disconnects(&self) -> Vec<(SessionId, DisconnectReason)> {
        self.disconnects.lock().expect("poisoned").clone()
    }

    /// Close every connection and the endpoint, and wait until it is idle.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Bind and run the server. Returns once the endpoint is listening.
pub async fn run(cfg: ServerConfig) -> Result<ServerHandle> {
    let (cert, key) = tls::load_or_create_cert(&cfg.cert_dir)?;
    let fingerprint = tls::fingerprint(cert.as_ref());
    let server_cfg = tls::server_crypto(cert, key, &cfg)?;
    let endpoint = quinn::Endpoint::server(server_cfg, cfg.bind)
        .with_context(|| format!("bind {}", cfg.bind))?;
    let addr = endpoint.local_addr()?;

    let (ctl_tx, ctl_rx) = mpsc::channel::<Ctl>(4096);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let disconnects = Arc::new(Mutex::new(Vec::new()));

    let accept_ep = endpoint.clone();
    let accept_ctl = ctl_tx.clone();
    let accept_disc = disconnects.clone();
    let queue = cfg.control_queue;
    let max_image_bytes = cfg.max_image_bytes;
    let accept_task = tokio::spawn(async move {
        let mut next_sid: u32 = 1;
        while let Some(incoming) = accept_ep.accept().await {
            let sid = SessionId(next_sid);
            next_sid = next_session_id(next_sid);
            let ctl = accept_ctl.clone();
            let disc = accept_disc.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => connection_task(sid, conn, ctl, queue, max_image_bytes, disc).await,
                    Err(e) => tracing::debug!("handshake failed: {e}"),
                }
            });
        }
    });

    let state = ServerState::new(cfg);
    let join = tokio::spawn(state_task(
        state,
        ctl_rx,
        shutdown_rx,
        endpoint,
        accept_task,
    ));

    Ok(ServerHandle {
        addr,
        fingerprint,
        shutdown: Some(shutdown_tx),
        join: Some(join),
        ctl: ctl_tx,
        disconnects,
    })
}

async fn state_task(
    mut state: ServerState,
    mut ctl_rx: mpsc::Receiver<Ctl>,
    mut shutdown_rx: oneshot::Receiver<()>,
    endpoint: quinn::Endpoint,
    accept_task: JoinHandle<()>,
) {
    let mut peers: HashMap<SessionId, Peer> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let actions = tokio::select! {
            _ = &mut shutdown_rx => break,
            _ = tick.tick() => state.handle(Event::Tick { now: Instant::now() }),
            ctl = ctl_rx.recv() => match ctl {
                None => break,
                Some(Ctl::Register { sid, peer }) => {
                    peers.insert(sid, peer);
                    state.handle(Event::Connected { sid, now: Instant::now() })
                }
                Some(Ctl::Snapshot(tx)) => {
                    let _ = tx.send(state.snapshot());
                    continue;
                }
                Some(Ctl::Ev(ev)) => {
                    let gone = if let Event::Disconnected { sid } = &ev { Some(*sid) } else { None };
                    let a = state.handle(ev);
                    if let Some(sid) = gone {
                        peers.remove(&sid);
                    }
                    a
                }
            },
        };
        execute(&mut state, &mut peers, actions);
    }
    tracing::info!("shutting down");
    accept_task.abort();
    for p in peers.values() {
        p.conn.close(VarInt::from_u32(0), b"server shutdown");
    }
    endpoint.close(VarInt::from_u32(0), b"server shutdown");
    let _ = tokio::time::timeout(Duration::from_secs(3), endpoint.wait_idle()).await;
}

fn execute(state: &mut ServerState, peers: &mut HashMap<SessionId, Peer>, actions: Vec<Action>) {
    let mut queue = std::collections::VecDeque::from(actions);
    while let Some(a) = queue.pop_front() {
        match a {
            Action::Send { to, msg } => {
                let Some(p) = peers.get(&to) else { continue };
                if p.out.try_send(Out::Msg(msg)).is_err() {
                    tracing::warn!("{to}: control queue full, disconnecting slow consumer");
                    let p = peers.remove(&to).expect("present");
                    p.conn.close(
                        VarInt::from_u32(ErrorCode::SlowConsumer.as_u32()),
                        b"slow consumer",
                    );
                    // Treat as gone right away so later broadcasts skip it.
                    queue.extend(state.handle(Event::Disconnected { sid: to }));
                }
            }
            Action::ForwardVoice { to, bytes } => {
                for sid in to {
                    if let Some(p) = peers.get(&sid) {
                        let _ = p.conn.send_datagram(bytes.clone());
                    }
                }
            }
            Action::EnableBlobs { sid } => {
                if let Some(p) = peers.get(&sid) {
                    p.conn
                        .set_max_concurrent_uni_streams(VarInt::from_u32(MAX_BLOB_STREAMS));
                }
            }
            Action::SendBlob { to, header, body } => send_blob(peers, to, header, body),
            Action::Close { sid, code, reason } => {
                let Some(p) = peers.get(&sid) else { continue };
                if p.out.try_send(Out::Close(code, reason.clone())).is_err() {
                    p.conn
                        .close(VarInt::from_u32(code.as_u32()), reason.as_bytes());
                } else {
                    // Fallback in case the writer never runs (no stream opened).
                    let conn = p.conn.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        conn.close(VarInt::from_u32(code.as_u32()), reason.as_bytes());
                    });
                }
            }
        }
    }
}

/// Open one blob stream per recipient. Runs detached; failures only drop the blob.
fn send_blob(
    peers: &HashMap<SessionId, Peer>,
    to: Vec<SessionId>,
    header: ServerBlob,
    body: Bytes,
) {
    let head = match encode_header(&header) {
        Ok(h) => Bytes::from(h),
        Err(e) => {
            tracing::warn!("blob header encode failed: {e}");
            return;
        }
    };
    for sid in to {
        let Some(p) = peers.get(&sid) else { continue };
        let sem = match header {
            ServerBlob::Image { .. } => p.images.clone(),
            ServerBlob::ScreenFrame { from } => {
                let mut frames = p.frames.lock().unwrap_or_else(|e| e.into_inner());
                frames
                    .entry(from)
                    .or_insert_with(|| Arc::new(Semaphore::new(FRAME_PERMITS_PER_SHARER)))
                    .clone()
            }
        };
        let Ok(permit) = sem.try_acquire_owned() else {
            tracing::debug!("{sid}: blob permits exhausted, dropping blob");
            continue;
        };
        let conn = p.conn.clone();
        let head = head.clone();
        let body = body.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut s = match tokio::time::timeout(BLOB_TIMEOUT, conn.open_uni()).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!("{sid}: blob stream open failed: {e}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("{sid}: blob stream open timed out");
                    return;
                }
            };
            let _ = s.set_priority(-1);
            // Hold the permit until the peer has acknowledged everything, so a peer
            // that stops reading runs out of permits instead of piling up streams.
            let res = tokio::time::timeout(BLOB_TIMEOUT, async {
                s.write_chunk(head).await?;
                s.write_chunk(body).await?;
                s.finish()?;
                let _ = s.stopped().await;
                Ok::<(), anyhow::Error>(())
            })
            .await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::debug!("{sid}: blob send failed: {e}"),
                Err(_) => {
                    // Reset, or quinn would keep the unsent body buffered until the
                    // connection closes.
                    let _ = s.reset(VarInt::from_u32(0));
                    tracing::debug!("{sid}: blob send timed out");
                }
            }
        });
    }
}

/// Read one client blob stream and hand it to the state task. A bad header or an
/// over-limit body stops the stream (an over-limit image also reports `BlobTooLarge`).
async fn read_blob(
    sid: SessionId,
    mut recv: quinn::RecvStream,
    max_image_bytes: u32,
    ctl: &mpsc::Sender<Ctl>,
) -> Result<()> {
    let mut prefix = [0u8; 4];
    recv.read_exact(&mut prefix).await?;
    let len = match header_len(prefix) {
        Ok(l) => l,
        Err(e) => {
            let _ = recv.stop(VarInt::from_u32(0));
            return Err(e.into());
        }
    };
    let mut json = vec![0u8; len];
    recv.read_exact(&mut json).await?;
    let header: ClientBlob = match decode_header(&json) {
        Ok(h) => h,
        Err(e) => {
            let _ = recv.stop(VarInt::from_u32(0));
            return Err(e.into());
        }
    };
    let limit = match header {
        ClientBlob::Image => max_image_bytes,
        ClientBlob::ScreenFrame => MAX_SCREEN_FRAME_BYTES,
    } as usize;
    match recv.read_to_end(limit).await {
        Ok(body) => {
            let _ = ctl
                .send(Ctl::Ev(Event::Blob {
                    sid,
                    header,
                    body: Bytes::from(body),
                    now: Instant::now(),
                }))
                .await;
            Ok(())
        }
        Err(ReadToEndError::TooLong) => {
            let _ = recv.stop(VarInt::from_u32(0));
            if header == ClientBlob::Image {
                let _ = ctl.send(Ctl::Ev(Event::BlobTooLarge { sid })).await;
            }
            Err(anyhow::anyhow!("blob over {limit} bytes"))
        }
        Err(e) => {
            let _ = recv.stop(VarInt::from_u32(0));
            Err(e.into())
        }
    }
}

async fn connection_task(
    sid: SessionId,
    conn: Connection,
    ctl: mpsc::Sender<Ctl>,
    queue: usize,
    max_image_bytes: u32,
    disconnects: Arc<Mutex<Vec<(SessionId, DisconnectReason)>>>,
) {
    tracing::info!("{sid}: connected from {}", conn.remote_address());
    let (out_tx, out_rx) = mpsc::channel::<Out>(queue);
    if ctl
        .send(Ctl::Register {
            sid,
            peer: Peer {
                conn: conn.clone(),
                out: out_tx,
                images: Arc::new(Semaphore::new(IMAGE_PERMITS)),
                frames: Mutex::new(HashMap::new()),
            },
        })
        .await
        .is_err()
    {
        return;
    }

    let control = async {
        let (send, mut recv) = conn.accept_bi().await?;
        let writer = tokio::spawn(writer_task(conn.clone(), send, out_rx));
        let mut dec = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        let res: Result<()> = async {
            while let Some(n) = recv.read(&mut buf).await? {
                dec.push(&buf[..n]);
                while let Some(msg) = dec.next::<ClientMsg>()? {
                    ctl.send(Ctl::Ev(Event::Msg { sid, msg }))
                        .await
                        .map_err(|_| anyhow::anyhow!("state task gone"))?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = &res {
            tracing::debug!("{sid}: control stream ended: {e}");
            if e.downcast_ref::<vc_proto::FrameError>().is_some() {
                // A malformed frame means the peer speaks another protocol version.
                conn.close(
                    VarInt::from_u32(ErrorCode::BadVersion.as_u32()),
                    b"bad frame (protocol mismatch)",
                );
            }
        }
        writer.abort();
        Ok::<(), anyhow::Error>(())
    };

    let datagrams = async {
        loop {
            let d = conn.read_datagram().await?;
            if ctl
                .send(Ctl::Ev(Event::Voice { sid, bytes: d }))
                .await
                .is_err()
            {
                return Ok::<(), ConnectionError>(());
            }
        }
    };

    // Uni streams are refused (limit 0) until `EnableBlobs` raises the limit after
    // Welcome, so nothing is buffered for a peer that has not said Hello.
    let blobs = async {
        let mut readers = JoinSet::new();
        loop {
            while readers.try_join_next().is_some() {}
            let Ok(recv) = conn.accept_uni().await else {
                return;
            };
            let ctl = ctl.clone();
            readers.spawn(async move {
                match tokio::time::timeout(
                    BLOB_TIMEOUT,
                    read_blob(sid, recv, max_image_bytes, &ctl),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::debug!("{sid}: blob stream dropped: {e}"),
                    Err(_) => tracing::debug!("{sid}: blob stream timed out"),
                }
            });
        }
    };

    tokio::select! {
        _ = control => {}
        _ = datagrams => {}
        _ = blobs => {}
    }
    // Wait for the connection to be fully closed so the reason is known.
    let reason = conn.closed().await;
    let why = match &reason {
        ConnectionError::TimedOut => DisconnectReason::Timeout,
        ConnectionError::ApplicationClosed(_) | ConnectionError::ConnectionClosed(_) => {
            DisconnectReason::ClosedByPeer
        }
        ConnectionError::LocallyClosed => DisconnectReason::ClosedByServer(
            conn.close_reason()
                .map(|r| r.to_string())
                .unwrap_or_default(),
        ),
        other => DisconnectReason::Other(other.to_string()),
    };
    tracing::info!("{sid}: disconnected ({why:?}: {reason})");
    disconnects.lock().expect("poisoned").push((sid, why));
    let _ = ctl.send(Ctl::Ev(Event::Disconnected { sid })).await;
}

async fn writer_task(conn: Connection, mut send: quinn::SendStream, mut rx: mpsc::Receiver<Out>) {
    while let Some(item) = rx.recv().await {
        match item {
            Out::Msg(msg) => {
                let Ok(bytes) = vc_proto::encode(&msg) else {
                    continue;
                };
                if send.write_all(&bytes).await.is_err() {
                    return;
                }
            }
            Out::Close(code, reason) => {
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_millis(500), send.stopped()).await;
                conn.close(VarInt::from_u32(code.as_u32()), reason.as_bytes());
                return;
            }
        }
    }
}
