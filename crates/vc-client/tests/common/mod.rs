#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vc_client::ServerKey;
use vc_client::app::UiCommand;
use vc_client::audio::{FakeBackend, FakeHandle, FakeMode, NullBackend};
use vc_client::gui::{AppOptions, VoiceApp};
use vc_client::net::NetOptions;
use vc_client::profile::{InputMode, Profile};
use vc_client::session::ConnStatus;
use vc_server::{ServerConfig, ServerHandle, Snapshot};

pub const T: Duration = Duration::from_secs(5);

/// A vc-server running on its own tokio runtime.
pub struct TestServer {
    pub rt: tokio::runtime::Runtime,
    pub handle: Option<ServerHandle>,
    pub cfg: ServerConfig,
    _dir: Option<tempfile::TempDir>,
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

pub fn server_config(cert_dir: &Path) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        cert_dir: cert_dir.to_path_buf(),
        hello_timeout_ms: 2000,
        idle_timeout_ms: 3000,
        keep_alive_ms: 500,
        ..ServerConfig::default()
    }
}

impl TestServer {
    pub fn start(f: impl FnOnce(&mut ServerConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = server_config(dir.path());
        f(&mut cfg);
        let mut s = Self::start_cfg(cfg);
        s._dir = Some(dir);
        s
    }

    pub fn start_cfg(cfg: ServerConfig) -> Self {
        let rt = runtime();
        let handle = rt.block_on(vc_server::run(cfg.clone())).unwrap();
        let mut cfg = cfg;
        cfg.bind = handle.addr;
        Self {
            rt,
            handle: Some(handle),
            cfg,
            _dir: None,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.handle.as_ref().unwrap().addr
    }

    pub fn fingerprint_hex(&self) -> String {
        self.handle.as_ref().unwrap().fingerprint_hex()
    }

    pub fn snapshot(&self) -> Snapshot {
        let h = self.handle.as_ref().unwrap();
        self.rt.block_on(h.snapshot()).unwrap()
    }

    pub fn nick_channel(&self, nick: &str) -> Option<String> {
        let s = self.snapshot();
        let u = s.users.iter().find(|u| u.nick == nick)?;
        s.channels
            .iter()
            .find(|c| c.id == u.channel)
            .map(|c| c.name.clone())
    }

    pub fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
    }

    /// Stop and start again on the same port with `cert_dir`.
    pub fn restart_with_cert_dir(&mut self, cert_dir: PathBuf) {
        self.stop();
        let mut cfg = self.cfg.clone();
        cfg.cert_dir = cert_dir;
        let deadline = Instant::now() + T;
        loop {
            match self.rt.block_on(vc_server::run(cfg.clone())) {
                Ok(h) => {
                    self.handle = Some(h);
                    self.cfg = cfg;
                    return;
                }
                Err(e) if Instant::now() < deadline => {
                    eprintln!("rebind retry: {e}");
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("restart failed: {e}"),
            }
        }
    }

    pub fn restart(&mut self) {
        let d = self.cfg.cert_dir.clone();
        self.restart_with_cert_dir(d);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn fast_net() -> NetOptions {
    NetOptions {
        backoff_base: Duration::from_millis(100),
        keep_alive: Duration::from_millis(300),
        idle_timeout: Duration::from_secs(3),
    }
}

/// A full client app without a window or audio thread, on fake devices.
pub struct Client {
    pub app: VoiceApp,
    pub fake: FakeHandle,
    pub dir: tempfile::TempDir,
}

pub fn client_with(mode: FakeMode) -> Client {
    let dir = tempfile::tempdir().unwrap();
    client_in(dir, mode)
}

pub fn client_in(dir: tempfile::TempDir, mode: FakeMode) -> Client {
    let fake = FakeBackend::new(mode);
    let h = fake.handle();
    let app = VoiceApp::with_options(
        None,
        Box::new(fake),
        Profile::new(dir.path()),
        vec![],
        AppOptions {
            net: fast_net(),
            audio_thread: false,
            fake_screen: false,
        },
    );
    Client { app, fake: h, dir }
}

pub fn null_client() -> (VoiceApp, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let app = VoiceApp::with_options(
        None,
        Box::new(NullBackend),
        Profile::new(dir.path()),
        vec![],
        AppOptions {
            net: fast_net(),
            audio_thread: false,
            fake_screen: false,
        },
    );
    (app, dir)
}

/// Add a saved server through the model's dialog commands. Returns its key.
pub fn add_server(
    app: &mut VoiceApp,
    label: &str,
    addr: SocketAddr,
    nick: &str,
    pw: Option<&str>,
) -> ServerKey {
    app.dispatch(vec![UiCommand::OpenAddServer]);
    {
        let d = app.model.ui.server_dialog.as_mut().unwrap();
        d.label = label.into();
        d.addr = addr.to_string();
        d.nick = nick.into();
        d.password = pw.unwrap_or("").into();
        // Callers connect explicitly.
        d.connect_now = false;
    }
    app.dispatch(vec![UiCommand::SaveServerDialog]);
    assert!(app.model.ui.server_dialog.is_none(), "dialog saved");
    app.model
        .saved
        .iter()
        .find(|s| s.label == label)
        .unwrap()
        .key
}

impl Client {
    pub fn add(&mut self, label: &str, addr: SocketAddr, nick: &str) -> ServerKey {
        add_server(&mut self.app, label, addr, nick, None)
    }

    pub fn connect(&mut self, key: ServerKey) {
        self.app.dispatch(vec![UiCommand::Connect(key)]);
        self.wait(
            |a| a.model.status(key) == ConnStatus::Connected,
            "connected",
        );
    }

    pub fn wait(&mut self, pred: impl Fn(&VoiceApp) -> bool, what: &str) {
        wait_app(&mut self.app, pred, T, what);
    }

    pub fn mode(&mut self, m: InputMode) {
        self.app.dispatch(vec![UiCommand::SetInputMode(m)]);
    }

    /// One 20 ms audio step: capture → tick → send, playback pulled.
    pub fn audio_step(&mut self) {
        self.fake.drive(960);
        self.app.tick_audio();
        self.app.poll();
    }
}

pub fn wait_app(app: &mut VoiceApp, pred: impl Fn(&VoiceApp) -> bool, d: Duration, what: &str) {
    let deadline = Instant::now() + d;
    loop {
        app.poll();
        if pred(app) {
            return;
        }
        if Instant::now() > deadline {
            let st: Vec<_> = app
                .model
                .saved
                .iter()
                .map(|s| (s.label.clone(), app.model.status(s.key)))
                .collect();
            panic!(
                "timed out waiting for {what}; statuses {st:?}; toasts {:?}",
                app.model.ui.toasts
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Run audio steps on all clients in lock-step for `frames` × 20 ms.
pub fn run_audio(clients: &mut [&mut Client], frames: usize) {
    for _ in 0..frames {
        let start = Instant::now();
        for c in clients.iter_mut() {
            c.audio_step();
        }
        let el = start.elapsed();
        if el < Duration::from_millis(20) {
            std::thread::sleep(Duration::from_millis(20) - el);
        }
    }
}

pub fn rms_db(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -100.0;
    }
    let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    if ms <= 1e-12 {
        -100.0
    } else {
        10.0 * ms.log10()
    }
}

/// Zero-crossing frequency estimate over `x`.
pub fn zero_crossing_hz(x: &[f32], rate: f32) -> f32 {
    let crossings = x
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f32 / 2.0 / (x.len() as f32 / rate)
}

/// Average per-frame zero-crossing frequency over the loud 20 ms frames.
pub fn loud_frames_hz(x: &[f32]) -> (usize, f32) {
    let loud: Vec<&[f32]> = x.chunks_exact(960).filter(|f| rms_db(f) > -30.0).collect();
    if loud.is_empty() {
        return (0, 0.0);
    }
    let hz = loud
        .iter()
        .map(|f| zero_crossing_hz(f, 48_000.0))
        .sum::<f32>()
        / loud.len() as f32;
    (loud.len(), hz)
}

/// Goertzel power of `freq` in `x`.
pub fn tone_power(x: &[f32], freq: f32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * freq / 48_000.0;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for v in x {
        let s0 = v + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - c * s1 * s2) / (x.len() as f32).powi(2)
}

/// UDP forwarder; `stop()` silently drops everything (simulates a dead path).
pub struct BlackholeRelay {
    pub addr: SocketAddr,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BlackholeRelay {
    pub fn start(server: SocketAddr) -> Self {
        use std::net::UdpSocket;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let back = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        back.connect(server).unwrap();
        let addr = front.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let peer = Arc::new(Mutex::new(None));
        {
            let (f, b, p, s) = (front.clone(), back.clone(), peer.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                while let Ok((n, from)) = f.recv_from(&mut buf) {
                    *p.lock().unwrap() = Some(from);
                    if !s.load(Ordering::Relaxed) {
                        let _ = b.send(&buf[..n]);
                    }
                }
            });
        }
        {
            let (f, b, p, s) = (front, back, peer, stop.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                while let Ok(n) = b.recv(&mut buf) {
                    let to = *p.lock().unwrap();
                    if let Some(to) = to
                        && !s.load(Ordering::Relaxed)
                    {
                        let _ = f.send_to(&buf[..n], to);
                    }
                }
            });
        }
        Self { addr, stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
