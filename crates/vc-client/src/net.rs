//! Client network layer: a tokio runtime on a background thread, one task per server.
//!
//! UI → net: `tokio::sync::mpsc::UnboundedSender<NetCmd>`.
//! net → UI: `std::sync::mpsc::Receiver<NetEvent>` (+ `request_repaint`).
//! Screen-share frames bypass that queue: each sharer has one overwrite slot that
//! the UI drains with `NetHandle::take_screen_frames`, so a hidden window that
//! stops draining events cannot pile up decoded frames. Chat images keep their
//! place in the event queue as a pixel-free `NetEvent::Image`; the decoded pixels
//! wait in a bounded `ImageInbox` (`NetHandle::take_image`).
//!
//! Images and frames travel on their own QUIC uni streams (one per blob), never on
//! the control stream.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use egui::ColorImage;
use quinn::{Connection, ConnectionError, VarInt};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use vc_proto::blob::{decode_header, encode_header, header_len};
use vc_proto::{
    ADPCM_FRAME_BYTES, ChannelId, ClientBlob, ClientMsg, ClientVoice, ErrorCode, FrameDecoder,
    MAX_IMAGE_BYTES, MAX_SCREEN_FRAME_BYTES, PROTOCOL_VERSION, ServerBlob, ServerMsg, ServerVoice,
    SessionId,
};

use crate::ServerKey;
use crate::media::{
    DISPLAY_MAX_SIDE, RECV_DECODE_MAX_ALLOC, RECV_DECODE_MAX_SIDE, SCREEN_DISPLAY_MAX_SIDE,
    UploadError, UploadSource, decode_image, prepare_upload, to_color_image,
};
use crate::session::{ConnStatus, MAX_IMAGES, error_text};
use crate::tofu::{self, Fingerprint};

#[derive(Debug, Clone, PartialEq)]
pub enum NetCmd {
    Connect {
        key: ServerKey,
        addr: String,
        nick: String,
        password: Option<String>,
        fingerprint: Option<Fingerprint>,
    },
    Disconnect {
        key: ServerKey,
    },
    Send {
        key: ServerKey,
        msg: ClientMsg,
    },
    SendVoice {
        key: ServerKey,
        seq: u32,
        payload: Vec<u8>,
    },
    /// Encode (off the UI thread) and post an image to the current channel.
    SendImage {
        key: ServerKey,
        source: UploadSource,
        /// The server's `max_image_bytes`.
        cap: u32,
    },
    /// One encoded screen frame. Dropped if the previous one is still in flight.
    SendScreenFrame {
        key: ServerKey,
        jpeg: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CloseReason {
    Timeout,
    ServerClosed {
        code: Option<ErrorCode>,
        reason: String,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum NetEvent {
    Status {
        key: ServerKey,
        status: ConnStatus,
    },
    Msg {
        key: ServerKey,
        msg: ServerMsg,
    },
    Voice {
        key: ServerKey,
        speaker: SessionId,
        seq: u32,
        payload: Vec<u8>,
    },
    /// First connect to a server: its fingerprint, to be saved.
    Pinned {
        key: ServerKey,
        fp: Fingerprint,
    },
    FingerprintChanged {
        key: ServerKey,
        seen: Fingerprint,
    },
    Closed {
        key: ServerKey,
        reason: CloseReason,
    },
    /// A chat image arrived. It carries no pixels: the decoded image waits in the
    /// bounded inbox under `seq` (`NetHandle::take_image`). `w`×`h` is its size as
    /// sent, before display downscaling.
    Image {
        key: ServerKey,
        from: SessionId,
        channel: ChannelId,
        ts_ms: u64,
        seq: u64,
        w: u32,
        h: u32,
    },
    /// An image upload failed before it reached the server.
    UploadFailed {
        key: ServerKey,
        error: UploadError,
    },
    /// A ping was answered. The round trip is measured on arrival, on the network
    /// runtime, so the value does not depend on when the UI next drains events.
    Pong {
        key: ServerKey,
        rtt_ms: u64,
    },
}

/// Latest decoded screen frame per (server, sharer).
type FrameSlots = Arc<Mutex<HashMap<(ServerKey, SessionId), Arc<ColorImage>>>>;

/// Decoded chat images waiting for the UI, oldest first, at most `MAX_IMAGES`.
/// A hidden window that stops draining keeps at most that many; an evicted
/// image's line later reads "[image expired]".
#[derive(Default)]
pub struct ImageInbox {
    queue: Mutex<VecDeque<(u64, ServerKey, Arc<ColorImage>)>>,
    next_seq: AtomicU64,
}

impl ImageInbox {
    /// Store an image and return its `seq`, evicting the oldest when full.
    pub fn push(&self, key: ServerKey, image: Arc<ColorImage>) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut q = self.queue.lock().expect("poisoned");
        while q.len() >= MAX_IMAGES {
            q.pop_front();
        }
        q.push_back((seq, key, image));
        seq
    }

    /// Remove and return the image stored under `seq`, if it was not evicted.
    pub fn take(&self, seq: u64) -> Option<Arc<ColorImage>> {
        let mut q = self.queue.lock().expect("poisoned");
        let i = q.iter().position(|(s, _, _)| *s == seq)?;
        q.remove(i).map(|(_, _, img)| img)
    }

    /// Drop every image of a server whose connection ended.
    pub fn clear(&self, key: ServerKey) {
        self.queue
            .lock()
            .expect("poisoned")
            .retain(|(_, k, _)| *k != key);
    }

    pub fn len(&self) -> usize {
        self.queue.lock().expect("poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Receives voice directly on the network thread (bypasses the UI thread).
pub type VoiceSink = Arc<dyn Fn(ServerKey, SessionId, u32, &[u8]) + Send + Sync>;

/// Local address for the client endpoint: loopback servers get a loopback bind,
/// so no firewall prompt appears for local use.
pub fn client_bind_addr(server: SocketAddr) -> SocketAddr {
    match server.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        IpAddr::V6(ip) if ip.is_loopback() => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0),
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    }
}

/// Reconnect delay for attempt `n` (1-based): base·2^(n-1) up to 16·base, then 20·base.
pub fn backoff(base: Duration, attempt: u32) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    if attempt <= 5 {
        base * (1u32 << (attempt - 1))
    } else {
        base * 20
    }
}

#[derive(Clone)]
struct Emitter {
    tx: std::sync::mpsc::Sender<NetEvent>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
    sink: Arc<Mutex<Option<VoiceSink>>>,
    frames: FrameSlots,
    images: Arc<ImageInbox>,
}

impl Emitter {
    fn emit(&self, ev: NetEvent) {
        let _ = self.tx.send(ev);
        self.repaint();
    }

    fn repaint(&self) {
        if let Some(ctx) = self.ctx.lock().expect("poisoned").as_ref() {
            ctx.request_repaint();
        }
    }

    /// Overwrite the sharer's frame slot (never queues).
    fn put_frame(&self, key: ServerKey, from: SessionId, image: Arc<ColorImage>) {
        self.frames
            .lock()
            .expect("poisoned")
            .insert((key, from), image);
        self.repaint();
    }

    fn clear_frames(&self, key: ServerKey) {
        self.frames
            .lock()
            .expect("poisoned")
            .retain(|(k, _), _| *k != key);
        self.images.clear(key);
    }

    fn sink(&self) -> Option<VoiceSink> {
        self.sink.lock().expect("poisoned").clone()
    }
}

pub struct NetHandle {
    cmd_tx: mpsc::UnboundedSender<NetCmd>,
    evt_rx: std::sync::mpsc::Receiver<NetEvent>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
    sink: Arc<Mutex<Option<VoiceSink>>>,
    frames: FrameSlots,
    images: Arc<ImageInbox>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy)]
pub struct NetOptions {
    pub backoff_base: Duration,
    pub keep_alive: Duration,
    pub idle_timeout: Duration,
}

impl Default for NetOptions {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_millis(500),
            keep_alive: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(5),
        }
    }
}

impl NetHandle {
    pub fn new() -> Self {
        Self::with_options(NetOptions::default())
    }

    pub fn with_backoff(base: Duration) -> Self {
        Self::with_options(NetOptions {
            backoff_base: base,
            ..NetOptions::default()
        })
    }

    pub fn with_options(opts: NetOptions) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let ctx = Arc::new(Mutex::new(None));
        let sink = Arc::new(Mutex::new(None));
        let frames: FrameSlots = Arc::new(Mutex::new(HashMap::new()));
        let images = Arc::new(ImageInbox::default());
        let em = Emitter {
            tx: evt_tx,
            ctx: ctx.clone(),
            sink: sink.clone(),
            frames: frames.clone(),
            images: images.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("vc-net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(manager(cmd_rx, shutdown_rx, em, opts));
            })
            .expect("spawn net thread");
        Self {
            cmd_tx,
            evt_rx,
            ctx,
            sink,
            frames,
            images,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    pub fn set_ctx(&self, ctx: egui::Context) {
        *self.ctx.lock().expect("poisoned") = Some(ctx);
    }

    pub fn set_voice_sink(&self, sink: VoiceSink) {
        *self.sink.lock().expect("poisoned") = Some(sink);
    }

    pub fn send(&self, cmd: NetCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<NetCmd> {
        self.cmd_tx.clone()
    }

    pub fn try_recv(&self) -> Option<NetEvent> {
        self.evt_rx.try_recv().ok()
    }

    pub fn drain(&self) -> Vec<NetEvent> {
        self.evt_rx.try_iter().collect()
    }

    /// Take the newest screen frame of every sharer that sent one since the last call.
    pub fn take_screen_frames(&self) -> Vec<(ServerKey, SessionId, Arc<ColorImage>)> {
        self.frames
            .lock()
            .expect("poisoned")
            .drain()
            .map(|((k, s), img)| (k, s, img))
            .collect()
    }

    /// Take the decoded pixels of a `NetEvent::Image`, unless already evicted.
    pub fn take_image(&self, seq: u64) -> Option<Arc<ColorImage>> {
        self.images.take(seq)
    }
}

impl Default for NetHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NetHandle {
    fn drop(&mut self) {
        // Other threads (the audio clock) may still hold command senders, so stop the
        // manager explicitly; it closes every connection before returning.
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum ConnCmd {
    Send(ClientMsg),
    Voice(u32, Vec<u8>),
    Image(UploadSource, u32),
    Frame(Vec<u8>),
    Disconnect,
}

struct ConnTask {
    tx: mpsc::UnboundedSender<ConnCmd>,
    join: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct ConnParams {
    key: ServerKey,
    addr: String,
    nick: String,
    password: Option<String>,
    fingerprint: Option<Fingerprint>,
}

async fn manager(
    mut rx: mpsc::UnboundedReceiver<NetCmd>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    em: Emitter,
    opts: NetOptions,
) {
    let mut conns: HashMap<ServerKey, ConnTask> = HashMap::new();
    loop {
        let cmd = tokio::select! {
            _ = &mut shutdown => break,
            c = rx.recv() => match c {
                Some(c) => c,
                None => break,
            },
        };
        conns.retain(|_, c| !c.join.is_finished());
        match cmd {
            NetCmd::Connect {
                key,
                addr,
                nick,
                password,
                fingerprint,
            } => {
                if let Some(old) = conns.remove(&key) {
                    stop(old).await;
                }
                let params = ConnParams {
                    key,
                    addr,
                    nick,
                    password,
                    fingerprint,
                };
                conns.insert(key, spawn_conn(params, em.clone(), opts));
            }
            NetCmd::Disconnect { key } => {
                if let Some(old) = conns.remove(&key) {
                    stop(old).await;
                }
                em.emit(NetEvent::Status {
                    key,
                    status: ConnStatus::Disconnected,
                });
            }
            NetCmd::Send { key, msg } => {
                if let Some(c) = conns.get(&key) {
                    let _ = c.tx.send(ConnCmd::Send(msg));
                }
            }
            NetCmd::SendVoice { key, seq, payload } => {
                if let Some(c) = conns.get(&key) {
                    let _ = c.tx.send(ConnCmd::Voice(seq, payload));
                }
            }
            NetCmd::SendImage { key, source, cap } => {
                if let Some(c) = conns.get(&key) {
                    let _ = c.tx.send(ConnCmd::Image(source, cap));
                }
            }
            NetCmd::SendScreenFrame { key, jpeg } => {
                if let Some(c) = conns.get(&key) {
                    let _ = c.tx.send(ConnCmd::Frame(jpeg));
                }
            }
        }
    }
    for (_, c) in conns.drain() {
        stop(c).await;
    }
}

async fn stop(c: ConnTask) {
    let _ = c.tx.send(ConnCmd::Disconnect);
    if tokio::time::timeout(Duration::from_millis(500), c.join)
        .await
        .is_err()
    {
        // Task aborted by drop of the timeout future's handle is fine.
    }
}

fn spawn_conn(params: ConnParams, em: Emitter, opts: NetOptions) -> ConnTask {
    let (tx, rx) = mpsc::unbounded_channel();
    let join = tokio::spawn(conn_loop(params, rx, em, opts));
    ConnTask { tx, join }
}

enum Outcome {
    /// User asked to disconnect.
    User,
    /// Do not retry; status text given.
    Fatal(String),
    /// Retry after backoff.
    Lost(CloseReason),
}

fn fatal_code(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::BadPassword | ErrorCode::BadVersion | ErrorCode::NickTaken | ErrorCode::BadNick
    )
}

async fn conn_loop(
    mut p: ConnParams,
    mut rx: mpsc::UnboundedReceiver<ConnCmd>,
    em: Emitter,
    opts: NetOptions,
) {
    let key = p.key;
    let mut attempt: u32 = 0;
    loop {
        em.emit(NetEvent::Status {
            key,
            status: if attempt == 0 {
                ConnStatus::Connecting
            } else {
                ConnStatus::Reconnecting { attempt }
            },
        });
        let mut welcomed = false;
        let outcome = connect_once(&mut p, &mut rx, &em, opts, &mut welcomed).await;
        if welcomed {
            attempt = 0;
        }
        match outcome {
            Outcome::User => {
                em.emit(NetEvent::Status {
                    key,
                    status: ConnStatus::Disconnected,
                });
                return;
            }
            Outcome::Fatal(text) => {
                em.emit(NetEvent::Closed {
                    key,
                    reason: CloseReason::Error(text.clone()),
                });
                em.emit(NetEvent::Status {
                    key,
                    status: ConnStatus::Failed(text),
                });
                return;
            }
            Outcome::Lost(reason) => {
                tracing::info!("server {key}: connection lost: {reason:?}");
                em.emit(NetEvent::Closed { key, reason });
                attempt += 1;
                em.emit(NetEvent::Status {
                    key,
                    status: ConnStatus::Reconnecting { attempt },
                });
                let wait = tokio::time::sleep(backoff(opts.backoff_base, attempt));
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        _ = &mut wait => break,
                        c = rx.recv() => match c {
                            None | Some(ConnCmd::Disconnect) => {
                                em.emit(NetEvent::Status { key, status: ConnStatus::Disconnected });
                                return;
                            }
                            Some(_) => {} // drop sends while offline
                        }
                    }
                }
            }
        }
    }
}

fn map_conn_error(e: &ConnectionError) -> CloseReason {
    match e {
        ConnectionError::TimedOut => CloseReason::Timeout,
        ConnectionError::ApplicationClosed(c) => CloseReason::ServerClosed {
            code: u32::try_from(c.error_code.into_inner())
                .ok()
                .and_then(ErrorCode::from_u32),
            reason: String::from_utf8_lossy(&c.reason).into_owned(),
        },
        other => CloseReason::Error(other.to_string()),
    }
}

fn outcome_for_close(reason: CloseReason, last_error: Option<ErrorCode>) -> Outcome {
    let code = match &reason {
        CloseReason::ServerClosed { code, .. } => code.or(last_error),
        _ => last_error,
    };
    match code {
        Some(ErrorCode::BadVersion) => Outcome::Fatal(crate::update::bad_version_text(error_text(
            ErrorCode::BadVersion,
        ))),
        Some(c) if fatal_code(c) => Outcome::Fatal(error_text(c).to_string()),
        _ => Outcome::Lost(reason),
    }
}

async fn connect_once(
    p: &mut ConnParams,
    rx: &mut mpsc::UnboundedReceiver<ConnCmd>,
    em: &Emitter,
    opts: NetOptions,
    welcomed: &mut bool,
) -> Outcome {
    let key = p.key;
    let addr = match tokio::net::lookup_host(p.addr.trim()).await {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return Outcome::Lost(CloseReason::Error("address did not resolve".into())),
        },
        Err(e) => return Outcome::Lost(CloseReason::Error(format!("resolve {}: {e}", p.addr))),
    };
    let endpoint = match quinn::Endpoint::client(client_bind_addr(addr)) {
        Ok(e) => e,
        Err(e) => return Outcome::Lost(CloseReason::Error(format!("bind: {e}"))),
    };
    let (mut cc, seen) = tofu::client_config(p.fingerprint);
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(opts.keep_alive));
    if let Ok(idle) = opts.idle_timeout.try_into() {
        t.max_idle_timeout(Some(idle));
    }
    cc.transport_config(Arc::new(t));

    let connecting = match endpoint.connect_with(cc, addr, "localhost") {
        Ok(c) => c,
        Err(e) => return Outcome::Lost(CloseReason::Error(format!("connect: {e}"))),
    };
    // Allow Disconnect while the handshake is in flight.
    let conn = tokio::select! {
        r = connecting => r,
        () = wait_disconnect(rx) => return Outcome::User,
    };
    let seen_fp = *seen.lock().expect("poisoned");
    let conn = match conn {
        Ok(c) => c,
        Err(e) => {
            if let (Some(exp), Some(s)) = (p.fingerprint, seen_fp)
                && exp != s
            {
                em.emit(NetEvent::FingerprintChanged { key, seen: s });
                return Outcome::Fatal("Server identity changed".into());
            }
            return Outcome::Lost(map_conn_error(&e));
        }
    };
    if p.fingerprint.is_none()
        && let Some(fp) = seen_fp
    {
        p.fingerprint = Some(fp);
        em.emit(NetEvent::Pinned { key, fp });
    }
    match conn.max_datagram_size() {
        Some(n) if n >= ServerVoice::LEN => {}
        _ => {
            conn.close(VarInt::from_u32(0), b"no datagrams");
            return Outcome::Fatal("server does not support voice datagrams".into());
        }
    }
    let out = run_connection(p, &conn, rx, em, welcomed).await;
    em.clear_frames(key);
    endpoint.close(VarInt::from_u32(0), b"bye");
    out
}

async fn wait_disconnect(rx: &mut mpsc::UnboundedReceiver<ConnCmd>) {
    loop {
        match rx.recv().await {
            None | Some(ConnCmd::Disconnect) => return,
            Some(_) => {}
        }
    }
}

async fn run_connection(
    p: &ConnParams,
    conn: &Connection,
    rx: &mut mpsc::UnboundedReceiver<ConnCmd>,
    em: &Emitter,
    welcomed: &mut bool,
) -> Outcome {
    let key = p.key;
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return Outcome::Lost(map_conn_error(&e)),
    };
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        nick: p.nick.clone(),
        password: p.password.clone(),
        h264: crate::video::decoder_available(),
    };
    match vc_proto::encode(&hello) {
        Ok(b) => {
            if send.write_all(&b).await.is_err() {
                return Outcome::Lost(CloseReason::Error("write failed".into()));
            }
        }
        Err(e) => return Outcome::Fatal(e.to_string()),
    }
    // Control writes run in their own task so a stalled stream never blocks the
    // select loop below (and with it voice sends). `None` finishes the stream.
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Option<Vec<u8>>>();
    let writer = tokio::spawn(async move {
        while let Some(item) = ctl_rx.recv().await {
            match item {
                Some(b) => {
                    if send.write_all(&b).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = send.finish();
                    return;
                }
            }
        }
    });

    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<Result<ServerMsg, String>>();
    let reader = tokio::spawn(async move {
        let mut dec = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    dec.push(&buf[..n]);
                    loop {
                        match dec.next::<ServerMsg>() {
                            Ok(Some(m)) => {
                                if msg_tx.send(Ok(m)).is_err() {
                                    return;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let _ = msg_tx.send(Err(e.to_string()));
                                return;
                            }
                        }
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    let _ = msg_tx.send(Err(e.to_string()));
                    return;
                }
            }
        }
    });

    let dconn = conn.clone();
    let dem = em.clone();
    let datagrams = tokio::spawn(async move {
        let mut last_ui: HashMap<SessionId, Instant> = HashMap::new();
        while let Ok(d) = dconn.read_datagram().await {
            let Ok(v) = ServerVoice::parse(&d) else {
                continue;
            };
            match dem.sink() {
                Some(sink) => {
                    sink(key, v.speaker, v.seq, v.payload);
                    if v.stream_of().is_some() {
                        // Stream sound never drives the speaking indicator.
                        continue;
                    }
                    // Throttled notification for the speaking indicator.
                    let now = Instant::now();
                    let due = last_ui
                        .get(&v.speaker)
                        .is_none_or(|t| now.duration_since(*t) >= Duration::from_millis(100));
                    if due {
                        last_ui.insert(v.speaker, now);
                        dem.emit(NetEvent::Voice {
                            key,
                            speaker: v.speaker,
                            seq: v.seq,
                            payload: Vec::new(),
                        });
                    }
                }
                None => {
                    let _ = dem.tx.send(NetEvent::Voice {
                        key,
                        speaker: v.speaker,
                        seq: v.seq,
                        payload: v.payload.to_vec(),
                    });
                }
            }
        }
    });

    let bconn = conn.clone();
    let bem = em.clone();
    let blobs = tokio::spawn(async move {
        let decode = Arc::new(DecodeSlots::default());
        let mut readers = JoinSet::new();
        while let Ok(recv) = bconn.accept_uni().await {
            while readers.try_join_next().is_some() {}
            let em = bem.clone();
            let decode = decode.clone();
            readers.spawn(async move {
                let r =
                    tokio::time::timeout(BLOB_TIMEOUT, recv_blob(key, recv, &em, &decode)).await;
                match r {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::debug!("server {key}: blob dropped: {e}"),
                    Err(_) => tracing::debug!("server {key}: blob timed out"),
                }
            });
        }
    });

    let mut sends: JoinSet<()> = JoinSet::new();
    let frame_in_flight = Arc::new(AtomicBool::new(false));
    let mut last_error: Option<ErrorCode> = None;
    let outcome = loop {
        while sends.try_join_next().is_some() {}
        tokio::select! {
            m = msg_rx.recv() => match m {
                Some(Ok(msg)) => {
                    let is_welcome = matches!(msg, ServerMsg::Welcome { .. });
                    if let ServerMsg::Error { code, .. } = &msg
                        && !*welcomed
                    {
                        last_error = Some(*code);
                    }
                    match msg {
                        // The nonce is the send time in ms (see `UiCommand::Ping`).
                        ServerMsg::Pong { nonce } => em.emit(NetEvent::Pong {
                            key,
                            rtt_ms: crate::app::unix_ms().saturating_sub(nonce),
                        }),
                        msg => em.emit(NetEvent::Msg { key, msg }),
                    }
                    if is_welcome {
                        *welcomed = true;
                        em.emit(NetEvent::Status { key, status: ConnStatus::Connected });
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!("server {key}: control stream error: {e}");
                    let r = conn.closed().await;
                    break outcome_for_close(map_conn_error(&r), last_error);
                }
                None => {
                    // Stream finished: wait for the close reason.
                    let r = tokio::time::timeout(Duration::from_secs(10), conn.closed()).await;
                    break match r {
                        Ok(r) => outcome_for_close(map_conn_error(&r), last_error),
                        Err(_) => {
                            conn.close(VarInt::from_u32(0), b"stream ended");
                            outcome_for_close(CloseReason::Error("control stream ended".into()), last_error)
                        }
                    };
                }
            },
            c = rx.recv() => match c {
                None | Some(ConnCmd::Disconnect) => {
                    let _ = ctl_tx.send(None);
                    conn.close(VarInt::from_u32(0), b"user disconnect");
                    break Outcome::User;
                }
                Some(ConnCmd::Send(msg)) => {
                    if let Ok(b) = vc_proto::encode(&msg) {
                        let _ = ctl_tx.send(Some(b));
                    }
                }
                Some(ConnCmd::Image(source, cap)) => {
                    let conn = conn.clone();
                    let em = em.clone();
                    sends.spawn(async move {
                        let prepared =
                            tokio::task::spawn_blocking(move || prepare_upload(source, cap)).await;
                        let body = match prepared {
                            Ok(Ok(b)) => b,
                            Ok(Err(error)) => {
                                em.emit(NetEvent::UploadFailed { key, error });
                                return;
                            }
                            Err(e) => {
                                tracing::warn!("server {key}: image encode panicked: {e}");
                                return;
                            }
                        };
                        if let Err(e) = send_blob(&conn, &ClientBlob::Image, body).await {
                            tracing::debug!("server {key}: image send failed: {e}");
                        }
                    });
                }
                Some(ConnCmd::Frame(jpeg)) => {
                    // One frame in flight per connection; newer frames are dropped
                    // while it is busy (the next grab supersedes them anyway).
                    if !frame_in_flight.swap(true, Ordering::AcqRel) {
                        let conn = conn.clone();
                        let busy = frame_in_flight.clone();
                        sends.spawn(async move {
                            if let Err(e) = send_blob(&conn, &ClientBlob::ScreenFrame, jpeg).await {
                                tracing::debug!("server {key}: frame send failed: {e}");
                            }
                            busy.store(false, Ordering::Release);
                        });
                    }
                }
                Some(ConnCmd::Voice(seq, payload)) => {
                    if payload.len() == ADPCM_FRAME_BYTES {
                        let bytes = ClientVoice { seq, payload: &payload }.encode();
                        let _ = conn.send_datagram(Bytes::from(bytes));
                    }
                }
            },
            r = conn.closed() => {
                break outcome_for_close(map_conn_error(&r), last_error);
            }
        }
    };
    reader.abort();
    datagrams.abort();
    blobs.abort();
    writer.abort();
    sends.abort_all();
    outcome
}

/// Image decodes running at once per connection.
const DECODE_PERMITS: usize = 2;

/// Decode slots of one connection: images share `DECODE_PERMITS`; each sharer's
/// frames get one slot of their own, so one busy stream never starves another.
struct DecodeSlots {
    images: Arc<Semaphore>,
    frames: Mutex<HashMap<SessionId, Arc<Semaphore>>>,
}

impl Default for DecodeSlots {
    fn default() -> Self {
        Self {
            images: Arc::new(Semaphore::new(DECODE_PERMITS)),
            frames: Mutex::new(HashMap::new()),
        }
    }
}

impl DecodeSlots {
    fn frame_slot(&self, from: SessionId) -> Arc<Semaphore> {
        self.frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(from)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
    }
}
/// Upper bound on one blob stream, in either direction.
const BLOB_TIMEOUT: Duration = Duration::from_secs(60);

/// Send one blob on a fresh uni stream at low priority (control traffic wins).
async fn send_blob(conn: &Connection, header: &ClientBlob, body: Vec<u8>) -> anyhow::Result<()> {
    let head = encode_header(header)?;
    let mut s = tokio::time::timeout(BLOB_TIMEOUT, conn.open_uni()).await??;
    let _ = s.set_priority(-1);
    // Done only once the server acknowledged it all, so "one frame in flight" holds.
    let res = tokio::time::timeout(BLOB_TIMEOUT, async {
        s.write_all(&head).await?;
        s.write_chunk(Bytes::from(body)).await?;
        s.finish()?;
        let _ = s.stopped().await;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match res {
        Ok(r) => r,
        Err(e) => {
            // Reset so quinn drops the unsent rest instead of buffering it.
            let _ = s.reset(VarInt::from_u32(0));
            Err(e.into())
        }
    }
}

/// Read one server blob, decode it off the async threads, and hand it to the UI.
async fn recv_blob(
    key: ServerKey,
    mut recv: quinn::RecvStream,
    em: &Emitter,
    decode: &DecodeSlots,
) -> anyhow::Result<()> {
    let mut prefix = [0u8; 4];
    recv.read_exact(&mut prefix).await?;
    let len = header_len(prefix)?;
    let mut json = vec![0u8; len];
    recv.read_exact(&mut json).await?;
    let header: ServerBlob = decode_header(&json)?;
    let limit = match header {
        ServerBlob::Image { .. } => MAX_IMAGE_BYTES,
        ServerBlob::ScreenFrame { .. } => MAX_SCREEN_FRAME_BYTES,
    } as usize;
    let body = match recv.read_to_end(limit).await {
        Ok(b) => b,
        Err(e) => {
            let _ = recv.stop(VarInt::from_u32(0));
            return Err(e.into());
        }
    };
    // Images wait for a decode slot; a frame is dropped while its sharer's
    // previous frame is still decoding.
    let (_permit, display_max) = match header {
        ServerBlob::Image { .. } => (
            decode.images.clone().acquire_owned().await?,
            DISPLAY_MAX_SIDE,
        ),
        ServerBlob::ScreenFrame { from } => match decode.frame_slot(from).try_acquire_owned() {
            Ok(p) => (p, SCREEN_DISPLAY_MAX_SIDE),
            Err(_) => return Ok(()),
        },
    };
    let screen = matches!(header, ServerBlob::ScreenFrame { .. });
    // Stable per-sharer decoder slot.
    let slot = match &header {
        ServerBlob::ScreenFrame { from } => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (key, from.0).hash(&mut h);
            h.finish()
        }
        ServerBlob::Image { .. } => 0,
    };
    let image = tokio::task::spawn_blocking(move || {
        let img = if screen && vc_proto::sniff_screen(&body) == Some(vc_proto::ScreenKind::H264) {
            // A native decoder, not a validator: the server only checked the start code.
            let (w, h, rgba) = crate::video::decode_h264(slot, &body)
                .map_err(|e| anyhow::anyhow!("H.264 frame: {e}"))?;
            image::RgbaImage::from_raw(w, h, rgba)
                .ok_or_else(|| anyhow::anyhow!("H.264 frame has the wrong size"))?
        } else {
            decode_image(&body, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC)?
        };
        anyhow::Ok(Arc::new(to_color_image(&img, display_max)))
    })
    .await??;
    match header {
        ServerBlob::Image {
            from,
            channel,
            ts_ms,
        } => {
            let w = image.source_size.x.round() as u32;
            let h = image.source_size.y.round() as u32;
            let seq = em.images.push(key, image);
            em.emit(NetEvent::Image {
                key,
                from,
                channel,
                ts_ms,
                seq,
                w,
                h,
            });
        }
        ServerBlob::ScreenFrame { from } => em.put_frame(key, from, image),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_inbox_bounded_drops_oldest() {
        let inbox = ImageInbox::default();
        let img = Arc::new(ColorImage::filled([1, 1], egui::Color32::RED));
        let seqs: Vec<u64> = (0..30)
            .map(|i| inbox.push(1 + i % 2, img.clone()))
            .collect();
        assert_eq!(inbox.len(), MAX_IMAGES);
        for s in &seqs[..30 - MAX_IMAGES] {
            assert!(inbox.take(*s).is_none(), "oldest seq {s} evicted");
        }
        assert!(inbox.take(seqs[29]).is_some());
        assert!(inbox.take(seqs[29]).is_none(), "taken once");
        assert_eq!(inbox.len(), MAX_IMAGES - 1);
        // A closed connection's images go.
        inbox.clear(1);
        assert!(!inbox.is_empty());
        inbox.clear(2);
        assert!(inbox.is_empty());
    }

    #[test]
    fn client_bind_addr_loopback_vs_remote() {
        let lo: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        assert_eq!(client_bind_addr(lo).to_string(), "127.0.0.1:0");
        let lo6: SocketAddr = "[::1]:7878".parse().unwrap();
        assert_eq!(client_bind_addr(lo6).to_string(), "[::1]:0");
        let remote: SocketAddr = "192.168.1.10:7878".parse().unwrap();
        assert_eq!(client_bind_addr(remote).to_string(), "0.0.0.0:0");
        let remote6: SocketAddr = "[2001:db8::1]:7878".parse().unwrap();
        assert_eq!(client_bind_addr(remote6).to_string(), "[::]:0");
    }

    fn app_close(code: u32) -> ConnectionError {
        ConnectionError::ApplicationClosed(quinn::ApplicationClose {
            error_code: VarInt::from_u32(code),
            reason: Bytes::from_static(b"bad frame (protocol mismatch)"),
        })
    }

    #[test]
    fn bad_version_close_is_fatal() {
        let e = app_close(ErrorCode::BadVersion.as_u32());
        match outcome_for_close(map_conn_error(&e), None) {
            Outcome::Fatal(t) => assert_eq!(t, "Protocol version mismatch"),
            _ => panic!("BadVersion close must be fatal"),
        }
        // Positive control: a non-fatal code (slow consumer) is retried.
        let e = app_close(ErrorCode::SlowConsumer.as_u32());
        assert!(matches!(
            outcome_for_close(map_conn_error(&e), None),
            Outcome::Lost(_)
        ));
        // And an unknown code is retried, not fatal.
        assert!(matches!(
            outcome_for_close(map_conn_error(&app_close(100)), None),
            Outcome::Lost(_)
        ));
    }

    #[test]
    fn backoff_schedule() {
        let b = Duration::from_millis(500);
        let got: Vec<u64> = (1..=8).map(|a| backoff(b, a).as_millis() as u64).collect();
        assert_eq!(got, [500, 1000, 2000, 4000, 8000, 10000, 10000, 10000]);
        assert_eq!(backoff(b, 0), Duration::ZERO);
    }
}
