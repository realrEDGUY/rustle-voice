//! Golden files shared with the Swift app's unit tests: `snapshot.json` and
//! `live.json` come from a hand-filled model, and `commands.json` holds one of
//! every command. Rust writes and checks the first two (`VC_BLESS=1` rewrites
//! them) and parses the third; Swift decodes the first two and encodes the same
//! commands, so a field renamed on one side fails a test on the other.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use egui::{Color32, ColorImage};
use vc_client::app::{AppModel, UiCommand};
use vc_client::net::NetEvent;
use vc_client::profile::{InputMode, Profile, SavedServer};
use vc_client::session::ConnStatus;
use vc_mobile::cmd::{self, MobileCmd};
use vc_mobile::view;
use vc_proto::{ChannelId, ChannelInfo, ServerMsg, SessionId, UserInfo, VoiceTuning};

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn check(name: &str, value: &impl serde::Serialize) {
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    let path = golden(name);
    if std::env::var_os("VC_BLESS").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with VC_BLESS=1)", path.display()));
    assert_eq!(
        text, want,
        "{name} changed: rerun with VC_BLESS=1 and update Swift"
    );
}

fn user(id: u32, nick: &str, channel: u32, muted: bool, sharing: bool) -> UserInfo {
    UserInfo {
        id: SessionId(id),
        nick: nick.into(),
        channel: ChannelId(channel),
        muted,
        deafened: false,
        sharing,
    }
}

/// Two servers: "Relay" connected with three users, chat and a watched share;
/// "Locked" failed with a TOFU prompt. A fixed `now` drives the speaking state.
fn model(dir: &std::path::Path, now: Instant) -> AppModel {
    let profile = Profile::new(dir);
    profile
        .save_servers(&[
            SavedServer {
                key: 1,
                label: "Relay".into(),
                addr: "127.0.0.1:7878".into(),
                nick: "ios".into(),
                password: None,
                fingerprint: None,
            },
            SavedServer {
                key: 2,
                label: "Locked".into(),
                addr: "127.0.0.1:7879".into(),
                nick: "ios".into(),
                password: Some("secret".into()),
                fingerprint: None,
            },
        ])
        .unwrap();
    let mut m = AppModel::load(profile);
    m.handle(UiCommand::Connect(1));
    m.handle(UiCommand::Connect(2));
    let welcome = ServerMsg::Welcome {
        session_id: SessionId(1),
        server_name: "Relay server".into(),
        motd: "Be nice".into(),
        default_channel: ChannelId(1),
        channels: vec![
            ChannelInfo {
                id: ChannelId(1),
                name: "Lobby".into(),
                permanent: true,
            },
            ChannelInfo {
                id: ChannelId(5),
                name: "Gaming".into(),
                permanent: false,
            },
        ],
        users: vec![
            user(1, "ios", 1, false, false),
            user(2, "bob", 1, false, true),
            user(3, "carol", 5, true, false),
        ],
        max_image_bytes: 4 * 1024 * 1024,
        max_screen_fps: 0,
        max_screen_height: 0,
        voice_tuning: VoiceTuning::default(),
    };
    m.apply_net(
        NetEvent::Msg {
            key: 1,
            msg: welcome,
        },
        now,
    );
    m.apply_net(
        NetEvent::Msg {
            key: 1,
            msg: ServerMsg::Chat {
                from: SessionId(2),
                channel: ChannelId(1),
                text: "hi".into(),
                ts_ms: 1_700_000_000_000,
            },
        },
        now,
    );
    let pixel = Arc::new(ColorImage::new([1, 1], vec![Color32::RED]));
    m.on_image(
        1,
        SessionId(2),
        ChannelId(1),
        1_700_000_001_000,
        640,
        480,
        Some(pixel),
    );
    m.on_image(
        1,
        SessionId(2),
        ChannelId(1),
        1_700_000_002_000,
        32,
        32,
        None,
    );
    m.handle(UiCommand::WatchScreen {
        key: 1,
        user: SessionId(2),
        on: true,
    });
    m.handle(UiCommand::MuteStream {
        key: 1,
        user: SessionId(2),
        muted: true,
    });
    m.handle(UiCommand::SetUserVolume {
        key: 1,
        nick: "bob".into(),
        volume: 1.5,
    });
    m.handle(UiCommand::MuteUser {
        key: 1,
        nick: "carol".into(),
        muted: true,
    });
    m.handle(UiCommand::SetInputMode(InputMode::Ptt));
    m.handle(UiCommand::Select(1));
    let s = m.sessions.get_mut(&1).unwrap();
    s.ping_ms = Some(42);
    s.note_voice(SessionId(2), now);
    m.apply_net(
        NetEvent::Status {
            key: 2,
            status: ConnStatus::Failed("Wrong server password".into()),
        },
        now,
    );
    m.apply_net(
        NetEvent::FingerprintChanged {
            key: 2,
            seen: [0xab; 32],
        },
        now,
    );
    m.audio_warning = Some("No audio device: running without audio (test)".into());
    m.input_level_db = -23.4;
    m.transmitting = true;
    m
}

#[test]
fn snapshot_and_live_match_golden() {
    let dir = tempfile::tempdir().unwrap();
    let now = Instant::now();
    let m = model(dir.path(), now);
    check("snapshot.json", &view::snapshot(&m));
    check("live.json", &view::live(&m, now));
}

#[test]
fn snapshot_caps_chat_and_sends_it_for_the_selected_server_only() {
    let dir = tempfile::tempdir().unwrap();
    let now = Instant::now();
    let mut m = model(dir.path(), now);
    for i in 0..250 {
        m.apply_net(
            NetEvent::Msg {
                key: 1,
                msg: ServerMsg::Chat {
                    from: SessionId(2),
                    channel: ChannelId(1),
                    text: format!("line {i}"),
                    ts_ms: i,
                },
            },
            now,
        );
    }
    let snap = view::snapshot(&m);
    let chat = &snap.servers[0].chat;
    assert_eq!(chat.len(), view::CHAT_LINES);
    assert_eq!(chat.last().unwrap().text, "line 249");
    m.handle(UiCommand::Select(2));
    let snap = view::snapshot(&m);
    assert!(snap.servers[0].chat.is_empty(), "not selected: no chat");
    assert_eq!(snap.streams.len(), 1, "streams cover every server");
}

/// Every command variant, by its `"t"` tag. The match makes a new variant fail
/// to compile until it is listed here (and so in `commands.json`).
fn tag(c: &MobileCmd) -> &'static str {
    match c {
        MobileCmd::Select { .. } => "Select",
        MobileCmd::Connect { .. } => "Connect",
        MobileCmd::Disconnect { .. } => "Disconnect",
        MobileCmd::TalkHere { .. } => "TalkHere",
        MobileCmd::RemoveServer { .. } => "RemoveServer",
        MobileCmd::SaveServer { .. } => "SaveServer",
        MobileCmd::Join { .. } => "Join",
        MobileCmd::CreateChannel { .. } => "CreateChannel",
        MobileCmd::RenameChannel { .. } => "RenameChannel",
        MobileCmd::DeleteChannel { .. } => "DeleteChannel",
        MobileCmd::SendChat { .. } => "SendChat",
        MobileCmd::ToggleMute => "ToggleMute",
        MobileCmd::ToggleDeafen => "ToggleDeafen",
        MobileCmd::SetInputMode { .. } => "SetInputMode",
        MobileCmd::SetVadThreshold { .. } => "SetVadThreshold",
        MobileCmd::SetMasterVolume { .. } => "SetMasterVolume",
        MobileCmd::SetNickname { .. } => "SetNickname",
        MobileCmd::SetUserVolume { .. } => "SetUserVolume",
        MobileCmd::MuteUser { .. } => "MuteUser",
        MobileCmd::WatchScreen { .. } => "WatchScreen",
        MobileCmd::MuteStream { .. } => "MuteStream",
        MobileCmd::UploadFile { .. } => "UploadFile",
        MobileCmd::TrustNewIdentity => "TrustNewIdentity",
        MobileCmd::CancelTofu => "CancelTofu",
        MobileCmd::DismissToast { .. } => "DismissToast",
    }
}

const ALL_TAGS: [&str; 25] = [
    "Select",
    "Connect",
    "Disconnect",
    "TalkHere",
    "RemoveServer",
    "SaveServer",
    "Join",
    "CreateChannel",
    "RenameChannel",
    "DeleteChannel",
    "SendChat",
    "ToggleMute",
    "ToggleDeafen",
    "SetInputMode",
    "SetVadThreshold",
    "SetMasterVolume",
    "SetNickname",
    "SetUserVolume",
    "MuteUser",
    "WatchScreen",
    "MuteStream",
    "UploadFile",
    "TrustNewIdentity",
    "CancelTofu",
    "DismissToast",
];

#[test]
fn every_golden_command_parses() {
    let text = std::fs::read_to_string(golden("commands.json")).unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    let mut tags = Vec::new();
    for e in &entries {
        let c = cmd::parse(&e.to_string()).unwrap_or_else(|err| panic!("{e}: {err}"));
        tags.push(tag(&c));
    }
    assert_eq!(
        tags, ALL_TAGS,
        "commands.json lists every command once, in order"
    );
    let unique: BTreeSet<_> = tags.iter().collect();
    assert_eq!(unique.len(), ALL_TAGS.len());
    assert_eq!(
        cmd::parse(r#"{"t":"SetInputMode","mode":"Ptt"}"#).unwrap(),
        MobileCmd::SetInputMode {
            mode: InputMode::Ptt
        }
    );
}

#[test]
fn unknown_or_share_commands_are_rejected() {
    for bad in [
        r#"{"t":"ShareScreenOn","key":1}"#,
        r#"{"t":"ToggleScreenShare"}"#,
        r#"{"t":"SetStreamSound","on":true}"#,
        r#"{"t":"Join","key":1}"#,
        r#"not json"#,
        r#"{}"#,
    ] {
        assert!(cmd::parse(bad).is_err(), "{bad} must not parse");
    }
}

#[test]
fn every_exported_function_is_in_the_header() {
    let lib = include_str!("../src/lib.rs");
    let header = include_str!("../include/vc_mobile.h");
    let mut names = Vec::new();
    for line in lib.lines() {
        if let Some(rest) = line.split("extern \"C\" fn ").nth(1) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            names.push(name);
        }
    }
    assert!(names.len() >= 18, "found {names:?}");
    for n in &names {
        assert!(
            header.contains(&format!("{n}(")),
            "{n} is exported but missing from vc_mobile.h"
        );
    }
    // And nothing declared in the header is missing from the library.
    for line in header.lines() {
        if let Some(pos) = line.find("vc_")
            && line.contains('(')
            && !line.trim_start().starts_with('*')
            && !line.trim_start().starts_with("/*")
        {
            let name: String = line[pos..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(names.contains(&name), "{name} declared but not exported");
        }
    }
}
