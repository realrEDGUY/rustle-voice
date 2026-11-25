//! A scripted peer for testing other clients by hand (such as the iOS app): a
//! real `VoiceApp` with a 440 Hz fake microphone and, with `--share`, the
//! synthetic test screen. It is driven headless through egui_kittest, like the
//! GUI tests, and prints what it sees, one line each, to stdout:
//!
//! ```text
//! [peer] connected
//! [peer] joined <nick> | left <nick> | moved <nick> <channel>
//! [peer] state <nick> muted=<b> deafened=<b>
//! [peer] hearing <nick> | silent <nick>
//! [peer] chat <nick>: <text>
//! [peer] image from <nick>
//! [peer] watchers <n>
//! ```
//!
//! ```text
//! cargo run -p vc-client --example fake_peer -- --addr 127.0.0.1:7878 --nick bob \
//!     --share --stream-sound --say hi --image pic.png --image-repeat 3
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use egui_kittest::Harness;
use vc_client::ServerKey;
use vc_client::app::UiCommand;
use vc_client::audio::{FakeBackend, FakeMode};
use vc_client::gui::{AppOptions, VoiceApp};
use vc_client::media::UploadSource;
use vc_client::profile::{InputMode, Profile, SavedServer};
use vc_client::session::ConnStatus;
use vc_proto::SessionId;

#[derive(Debug, Parser)]
#[command(name = "fake_peer", about = "Scripted voicechat peer for manual tests")]
struct Args {
    /// Server address (host:port).
    #[arg(long, default_value = "127.0.0.1:7878")]
    addr: String,
    #[arg(long, default_value = "bob")]
    nick: String,
    #[arg(long)]
    password: Option<String>,
    /// Join this channel once connected.
    #[arg(long)]
    channel: Option<String>,
    /// Share the synthetic test screen (320x180, changes every frame).
    #[arg(long)]
    share: bool,
    /// Send stream sound (a 1500 Hz tone) to watchers.
    #[arg(long)]
    stream_sound: bool,
    /// Send this chat line every 10 s.
    #[arg(long)]
    say: Option<String>,
    /// Send this PNG or JPEG once connected.
    #[arg(long)]
    image: Option<PathBuf>,
    /// How many times to send `--image` (300 ms apart).
    #[arg(long, default_value_t = 1)]
    image_repeat: u32,
    /// Stay silent (no fake microphone tone).
    #[arg(long)]
    silent: bool,
}

const KEY: ServerKey = 1;
const LABEL: &str = "Peer";

fn say(line: impl AsRef<str>) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "[peer] {}", line.as_ref());
    let _ = out.flush();
}

#[derive(Clone, PartialEq)]
struct Seen {
    nick: String,
    channel: String,
    muted: bool,
    deafened: bool,
}

/// What was printed last, to print only changes.
#[derive(Default)]
struct Printed {
    users: BTreeMap<SessionId, Seen>,
    speaking: BTreeSet<SessionId>,
    chat_len: usize,
    watchers: Option<u32>,
}

fn main() {
    let args = Args::parse();
    let dir = tempfile::tempdir().expect("temp profile");
    let profile = Profile::new(dir.path());
    profile
        .save_servers(&[SavedServer {
            key: KEY,
            label: LABEL.into(),
            addr: args.addr.clone(),
            nick: args.nick.clone(),
            password: args.password.clone(),
            fingerprint: None,
        }])
        .expect("save servers");
    let mode = if args.silent {
        FakeMode::Silent
    } else {
        FakeMode::Sine(440.0)
    };
    let app = VoiceApp::with_options(
        None,
        Box::new(FakeBackend::new(mode).realtime()),
        profile,
        vec![LABEL.into()],
        AppOptions {
            fake_screen: args.share,
            ..AppOptions::default()
        },
    );
    let mut h: Harness<'static, VoiceApp> = Harness::builder()
        .with_size([1280.0, 860.0])
        .build_ui_state(|ui, app: &mut VoiceApp| app.frame(ui), app);
    // Talk all the time, so the other side always sees "speaking".
    h.state_mut()
        .dispatch(vec![UiCommand::SetInputMode(InputMode::Open)]);

    let mut printed = Printed::default();
    let mut was_connected = false;
    let mut next_say = Instant::now();
    let mut images_left = 0u32;
    let mut next_image = Instant::now();
    let mut last_status = String::new();
    loop {
        h.step();
        let app = h.state_mut();
        let status = app.model.status(KEY);
        let label = status.label();
        if label != last_status {
            if status != ConnStatus::Connected {
                say(format!("status {label}"));
            }
            last_status = label;
        }
        let connected = status == ConnStatus::Connected;
        if connected && !was_connected {
            say("connected");
            on_connect(app, &args);
            // Chat history survives a reconnect: only lines after it are new.
            printed = Printed {
                chat_len: app.model.session(KEY).map_or(0, |s| s.chat.len()),
                ..Printed::default()
            };
            images_left = if args.image.is_some() {
                args.image_repeat
            } else {
                0
            };
            next_image = Instant::now() + Duration::from_millis(500);
            next_say = Instant::now() + Duration::from_secs(1);
        }
        was_connected = connected;
        if connected {
            report(app, &mut printed);
            let now = Instant::now();
            if let Some(text) = &args.say
                && now >= next_say
            {
                next_say = now + Duration::from_secs(10);
                app.model.ui.chat_input = text.clone();
                app.dispatch(vec![UiCommand::Select(KEY), UiCommand::SendChat]);
            }
            if images_left > 0
                && now >= next_image
                && let Some(path) = &args.image
            {
                images_left -= 1;
                next_image = now + Duration::from_millis(300);
                app.dispatch(vec![
                    UiCommand::Select(KEY),
                    UiCommand::UploadImage(UploadSource::File(path.clone())),
                ]);
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Apply the scripted actions after each (re)connect.
fn on_connect(app: &mut VoiceApp, args: &Args) {
    app.dispatch(vec![UiCommand::Select(KEY)]);
    if let Some(name) = &args.channel {
        let id = app
            .model
            .session(KEY)
            .and_then(|s| s.channels.values().find(|c| c.name == *name).map(|c| c.id));
        match id {
            Some(id) => app.dispatch(vec![UiCommand::Join(id)]),
            None => say(format!("no channel named {name:?}")),
        }
    }
    if args.share && app.model.screen_share.is_none() {
        app.dispatch(vec![UiCommand::ShareScreenOn(KEY)]);
    }
    if args.stream_sound {
        app.dispatch(vec![UiCommand::SetStreamSound { on: true }]);
    }
}

/// Print every change since the last call.
fn report(app: &VoiceApp, p: &mut Printed) {
    let Some(s) = app.model.session(KEY) else {
        return;
    };
    let now = Instant::now();
    let channel_name = |id| {
        s.channels
            .get(&id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("#{}", id.0))
    };
    let mut current = BTreeMap::new();
    for u in s.users.values() {
        if Some(u.id) == s.my_id {
            continue;
        }
        current.insert(
            u.id,
            Seen {
                nick: u.nick.clone(),
                channel: channel_name(u.channel),
                muted: u.muted,
                deafened: u.deafened,
            },
        );
    }
    for (id, now_seen) in &current {
        match p.users.get(id) {
            None => {
                say(format!("joined {}", now_seen.nick));
                say(format!(
                    "state {} muted={} deafened={}",
                    now_seen.nick, now_seen.muted, now_seen.deafened
                ));
            }
            Some(before) => {
                if before.channel != now_seen.channel {
                    say(format!("moved {} {}", now_seen.nick, now_seen.channel));
                }
                if (before.muted, before.deafened) != (now_seen.muted, now_seen.deafened) {
                    say(format!(
                        "state {} muted={} deafened={}",
                        now_seen.nick, now_seen.muted, now_seen.deafened
                    ));
                }
            }
        }
    }
    for (id, before) in &p.users {
        if !current.contains_key(id) {
            say(format!("left {}", before.nick));
        }
    }
    let speaking: BTreeSet<SessionId> = current
        .keys()
        .copied()
        .filter(|id| s.is_speaking(*id, now))
        .collect();
    for id in speaking.difference(&p.speaking) {
        if let Some(u) = current.get(id) {
            say(format!("hearing {}", u.nick));
        }
    }
    for id in p.speaking.difference(&speaking) {
        let nick = current
            .get(id)
            .or_else(|| p.users.get(id))
            .map(|u| u.nick.clone())
            .unwrap_or_default();
        say(format!("silent {nick}"));
    }
    let me = s.my_id.and_then(|id| s.nick_of(id)).unwrap_or_default();
    if p.chat_len > s.chat.len() {
        p.chat_len = 0;
    }
    for line in &s.chat[p.chat_len..] {
        if line.from_nick == me {
            continue;
        }
        if line.image.is_some() {
            say(format!("image from {}", line.from_nick));
        } else {
            say(format!("chat {}: {}", line.from_nick, line.text));
        }
    }
    p.chat_len = s.chat.len();
    if s.sharing && p.watchers != Some(s.watchers) {
        say(format!("watchers {}", s.watchers));
        p.watchers = Some(s.watchers);
    }
    p.users = current;
    p.speaking = speaking;
}
