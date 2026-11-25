//! `MobileApp` against real in-process servers: the JSON commands and views the
//! iOS app uses, end to end.

use std::path::Path;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use serde_json::Value;
use vc_client::audio::{FakeBackend, FakeMode};
use vc_client::gui::{AppOptions, VoiceApp};
use vc_client::net::NetOptions;
use vc_client::profile::{Profile, SavedServer};
use vc_client::session::ConnStatus;
use vc_mobile::MobileApp;
use vc_server::{ServerConfig, ServerHandle};

const T: Duration = Duration::from_secs(8);

struct Server {
    rt: tokio::runtime::Runtime,
    handle: Option<ServerHandle>,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let cfg = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            cert_dir: dir.path().to_path_buf(),
            hello_timeout_ms: 2000,
            idle_timeout_ms: 3000,
            keep_alive_ms: 500,
            ..ServerConfig::default()
        };
        let handle = rt.block_on(vc_server::run(cfg)).unwrap();
        Self {
            rt,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn addr(&self) -> String {
        self.handle.as_ref().unwrap().addr.to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt.block_on(h.shutdown());
        }
    }
}

fn fast_opts() -> AppOptions {
    AppOptions {
        net: NetOptions {
            backoff_base: Duration::from_millis(100),
            keep_alive: Duration::from_millis(300),
            idle_timeout: Duration::from_secs(3),
        },
        ..AppOptions::default()
    }
}

/// A `MobileApp` plus the last snapshot it returned.
struct Phone {
    app: MobileApp,
    snap: Value,
    _dir: tempfile::TempDir,
}

impl Phone {
    fn new(audio: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::in_dir(dir, audio)
    }

    fn in_dir(dir: tempfile::TempDir, audio: &str) -> Self {
        let app = MobileApp::with_options(dir.path(), audio, fast_opts()).unwrap();
        let mut p = Self {
            app,
            snap: Value::Null,
            _dir: dir,
        };
        p.step();
        p
    }

    fn step(&mut self) {
        self.app.step();
        if let Some(s) = self.app.snapshot() {
            self.snap = serde_json::from_str(&s).unwrap();
        }
    }

    fn cmd(&mut self, json: Value) -> Result<(), String> {
        let r = self.app.command(&json.to_string());
        self.step();
        r
    }

    fn wait(&mut self, what: &str, pred: impl Fn(&Value) -> bool) {
        let deadline = Instant::now() + T;
        loop {
            self.step();
            if pred(&self.snap) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; snapshot: {}",
                self.snap
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Add a server through the form command and wait until it is connected.
    fn add(&mut self, addr: &str, nick: &str) -> u64 {
        self.cmd(serde_json::json!({
            "t": "SaveServer", "editing": null, "label": "", "addr": addr,
            "nick": nick, "password": "", "connect_now": true
        }))
        .unwrap();
        let key = self.snap["selected"].as_u64().unwrap();
        self.wait("connected", |s| {
            server(s, key)["status"]["kind"] == "connected"
        });
        key
    }

    fn server(&self, key: u64) -> Value {
        server(&self.snap, key).clone()
    }
}

fn server(snap: &Value, key: u64) -> &Value {
    snap["servers"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["key"] == key))
        .unwrap_or(&Value::Null)
}

fn channel<'a>(s: &'a Value, name: &str) -> Option<&'a Value> {
    s["channels"].as_array()?.iter().find(|c| c["name"] == name)
}

fn has_user(s: &Value, ch: &str, nick: &str) -> bool {
    channel(s, ch).is_some_and(|c| {
        c["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["nick"] == nick)
    })
}

#[test]
fn connects_and_shows_lobby_with_me() {
    let srv = Server::start();
    let mut p = Phone::new("fake-sine");
    let key = p.add(&srv.addr(), "ios");
    let s = p.server(key);
    let lobby = channel(&s, "Lobby").expect("Lobby");
    assert_eq!(lobby["permanent"], true);
    let me = &lobby["users"][0];
    assert_eq!(
        (me["nick"].as_str(), me["me"].as_bool()),
        (Some("ios"), Some(true))
    );
    assert_eq!(s["my_channel"], s["default_channel"]);
    assert_eq!(p.snap["voice"]["talk_target"], key);
    assert_eq!(p.snap["voice"]["channel_name"], "Lobby");
    // The ping comes back within a couple of seconds.
    let deadline = Instant::now() + T;
    let mut live = Value::Null;
    loop {
        p.step();
        if let Some(l) = p.app.live() {
            live = serde_json::from_str(&l).unwrap();
        }
        if live["servers"][0]["ping_ms"].is_u64() {
            break;
        }
        assert!(Instant::now() < deadline, "no ping: {live}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Unchanged views are not sent again.
    p.app.snapshot();
    assert!(p.app.snapshot().is_none());
}

#[test]
fn channel_create_rename_delete_and_join() {
    let srv = Server::start();
    let mut p = Phone::new("fake-sine");
    let key = p.add(&srv.addr(), "ios");
    p.cmd(serde_json::json!({"t": "CreateChannel", "key": key, "name": "Temp"}))
        .unwrap();
    p.wait("in Temp", |s| has_user(server(s, key), "Temp", "ios"));
    let temp = channel(&p.server(key), "Temp").unwrap().clone();
    assert_eq!(temp["permanent"], false, "temp badge");
    assert_eq!(p.server(key)["my_channel"], temp["id"]);
    p.cmd(serde_json::json!({
        "t": "RenameChannel", "key": key, "channel": temp["id"], "name": "Temp2"
    }))
    .unwrap();
    p.wait("renamed", |s| channel(server(s, key), "Temp2").is_some());
    p.cmd(serde_json::json!({"t": "DeleteChannel", "key": key, "channel": temp["id"]}))
        .unwrap();
    p.wait("deleted, back in Lobby", |s| {
        channel(server(s, key), "Temp2").is_none() && has_user(server(s, key), "Lobby", "ios")
    });
    let afk = channel(&p.server(key), "AFK").unwrap()["id"].clone();
    p.cmd(serde_json::json!({"t": "Join", "key": key, "channel": afk}))
        .unwrap();
    p.wait("in AFK", |s| has_user(server(s, key), "AFK", "ios"));
}

#[test]
fn chat_round_trip_and_unsent_draft() {
    let srv = Server::start();
    let mut a = Phone::new("fake-sine");
    let mut b = Phone::new("fake-sine");
    let ka = a.add(&srv.addr(), "alice");
    let kb = b.add(&srv.addr(), "bob");
    a.cmd(serde_json::json!({"t": "SendChat", "key": ka, "text": "hello there"}))
        .unwrap();
    b.wait("alice's line", |s| {
        server(s, kb)["chat"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["from"] == "alice" && l["text"] == "hello there")
    });
    let err = a
        .cmd(serde_json::json!({"t": "SendChat", "key": ka, "text": "   "}))
        .unwrap_err();
    assert_eq!(err, "Not sent");
    let long = "x".repeat(vc_proto::MAX_CHAT + 1);
    assert!(
        a.cmd(serde_json::json!({"t": "SendChat", "key": ka, "text": long}))
            .is_err()
    );
    assert!(
        a.app.app.model.ui.chat_input.is_empty(),
        "Swift owns the draft"
    );
}

#[test]
fn bad_address_is_an_error_and_clears_the_dialog() {
    let mut p = Phone::new("fake-sine");
    let err = p
        .cmd(serde_json::json!({
            "t": "SaveServer", "editing": null, "label": "x", "addr": "not an address",
            "nick": "ios", "password": "", "connect_now": true
        }))
        .unwrap_err();
    assert_eq!(err, "Invalid address");
    assert!(p.app.app.model.ui.server_dialog.is_none());
    assert!(p.snap["servers"].as_array().unwrap().is_empty());
    let err = p
        .cmd(serde_json::json!({
            "t": "SaveServer", "editing": null, "label": "x", "addr": "127.0.0.1:1",
            "nick": "", "password": "", "connect_now": false
        }))
        .unwrap_err();
    assert!(err.contains("Nickname"), "{err}");
}

#[test]
fn unknown_json_is_an_error_not_a_panic() {
    let mut p = Phone::new("fake-sine");
    assert!(p.app.command("{\"t\":\"Nope\"}").is_err());
    assert!(p.app.command("garbage").is_err());
    assert!(
        p.app
            .command("{\"t\":\"ShareScreenOn\",\"key\":1}")
            .is_err()
    );
    p.step();
    p.cmd(serde_json::json!({"t": "ToggleMute"})).unwrap();
    assert_eq!(p.snap["voice"]["muted"], true);
}

#[test]
fn changed_fingerprint_prompts_and_trust_connects() {
    let srv = Server::start();
    let dir = tempfile::tempdir().unwrap();
    Profile::new(dir.path())
        .save_servers(&[SavedServer {
            key: 1,
            label: "Pinned".into(),
            addr: srv.addr(),
            nick: "ios".into(),
            password: None,
            fingerprint: Some("00".repeat(32)),
        }])
        .unwrap();
    let mut p = Phone::in_dir(dir, "fake-sine");
    p.cmd(serde_json::json!({"t": "Connect", "key": 1}))
        .unwrap();
    p.wait("tofu prompt", |s| s["tofu"]["key"] == 1);
    assert_eq!(
        p.snap["tofu"]["fingerprint"],
        srv.handle.as_ref().unwrap().fingerprint_hex()
    );
    assert_ne!(p.server(1)["status"]["kind"], "connected");
    p.cmd(serde_json::json!({"t": "TrustNewIdentity"})).unwrap();
    p.wait("connected after trust", |s| {
        s["tofu"].is_null() && server(s, 1)["status"]["kind"] == "connected"
    });
}

#[test]
fn watching_a_fake_share_gives_frames() {
    let srv = Server::start();
    // Alice is a desktop app sharing the synthetic screen, driven like the GUI tests.
    let adir = tempfile::tempdir().unwrap();
    let profile = Profile::new(adir.path());
    profile
        .save_servers(&[SavedServer {
            key: 1,
            label: "S".into(),
            addr: srv.addr(),
            nick: "alice".into(),
            password: None,
            fingerprint: None,
        }])
        .unwrap();
    let alice = VoiceApp::with_options(
        None,
        Box::new(FakeBackend::new(FakeMode::Silent)),
        profile,
        vec!["S".into()],
        AppOptions {
            audio_thread: false,
            fake_screen: true,
            ..fast_opts()
        },
    );
    let mut h: Harness<'static, VoiceApp> = Harness::builder()
        .with_size([1280.0, 860.0])
        .build_ui_state(|ui, app: &mut VoiceApp| app.frame(ui), alice);
    let deadline = Instant::now() + T;
    while h.state().model.status(1) != ConnStatus::Connected {
        h.step();
        assert!(Instant::now() < deadline, "alice connects");
        std::thread::sleep(Duration::from_millis(10));
    }
    h.state_mut()
        .dispatch(vec![vc_client::app::UiCommand::ShareScreenOn(1)]);

    let mut bob = Phone::new("fake-sine");
    let kb = bob.add(&srv.addr(), "bob");
    let alice_id = {
        let deadline = Instant::now() + T;
        loop {
            h.step();
            bob.step();
            let s = bob.server(kb);
            let sharer = channel(&s, "Lobby").and_then(|c| {
                c["users"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|u| u["nick"] == "alice" && u["sharing"] == true)
                    .cloned()
            });
            if let Some(u) = sharer {
                break u["id"].as_u64().unwrap() as u32;
            }
            assert!(Instant::now() < deadline, "bob sees alice sharing");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    bob.cmd(serde_json::json!({"t": "WatchScreen", "key": kb, "user": alice_id, "on": true}))
        .unwrap();
    assert_eq!(bob.snap["streams"][0]["nick"], "alice");
    assert_eq!(bob.snap["streams"][0]["user"], alice_id);
    let mut first = None;
    let deadline = Instant::now() + T;
    loop {
        h.step();
        bob.step();
        if let Some((img, generation)) = bob.app.screen(kb, alice_id) {
            assert_eq!(img.size, [320, 180], "fake screen size");
            match first {
                None => first = Some(generation),
                Some(g) if generation > g => break,
                Some(_) => {}
            }
        }
        assert!(Instant::now() < deadline, "frames keep coming ({first:?})");
        std::thread::sleep(Duration::from_millis(10));
    }
    bob.cmd(serde_json::json!({"t": "MuteStream", "key": kb, "user": alice_id, "muted": true}))
        .unwrap();
    assert_eq!(bob.snap["streams"][0]["sound_muted"], true);
    bob.cmd(serde_json::json!({"t": "WatchScreen", "key": kb, "user": alice_id, "on": false}))
        .unwrap();
    assert!(bob.snap["streams"].as_array().unwrap().is_empty());
    assert!(bob.app.screen(kb, alice_id).is_none());
}

#[test]
fn external_audio_flows_without_deadlock() {
    let srv = Server::start();
    let mut p = Phone::new("external");
    let audio = p.app.audio().expect("external mode has a handle");
    assert!(audio.capture_running() && audio.playback_running());
    assert!(Phone::new("fake-sine").app.audio().is_none());
    let key = p.add(&srv.addr(), "ios");
    p.cmd(serde_json::json!({"t": "SetInputMode", "mode": "Open"}))
        .unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        let audio = audio.clone();
        std::thread::spawn(move || {
            let tone: Vec<f32> = (0..441).map(|i| (i as f32 * 0.06).sin() * 0.5).collect();
            let mut out = vec![0.0; 441];
            let mut n = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                audio.push_capture(&tone, 44_100);
                audio.pull_playback(&mut out, 44_100);
                n += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            n
        })
    };
    let deadline = Instant::now() + T;
    loop {
        p.step();
        p.app.set_ptt(true);
        p.app.set_ptt(false);
        if p.app.app.model.transmitting {
            break;
        }
        assert!(Instant::now() < deadline, "the pushed tone is transmitted");
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(worker.join().unwrap() > 0);
    assert_eq!(p.server(key)["status"]["kind"], "connected");
}

#[test]
fn unknown_audio_mode_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MobileApp::new(Path::new(dir.path()), "cpal").is_err());
}
