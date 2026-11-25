//! Headless GUI tests with egui_kittest, against real in-process servers.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use egui::accesskit::Role;
use egui::{Key, Modifiers, PointerButton, Pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use vc_client::ServerKey;
use vc_client::audio::{FakeBackend, FakeHandle, FakeMode, NullBackend};
use vc_client::gui::{AppOptions, NO_AUDIO_BANNER, VoiceApp};
use vc_client::profile::{InputMode, Profile, SavedServer};
use vc_client::session::ConnStatus;

type H = Harness<'static, VoiceApp>;

// ------------------------------------------------------------------ helpers

fn harness(app: VoiceApp) -> H {
    Harness::builder()
        .with_size([1280.0, 860.0])
        .build_ui_state(|ui, app: &mut VoiceApp| app.frame(ui), app)
}

struct Gui {
    h: H,
    fake: FakeHandle,
    dir: tempfile::TempDir,
}

fn gui_with(mode: FakeMode, servers: &[(&str, std::net::SocketAddr, &str, Option<&str>)]) -> Gui {
    gui_opts(mode, servers, false)
}

fn gui_opts(
    mode: FakeMode,
    servers: &[(&str, std::net::SocketAddr, &str, Option<&str>)],
    fake_screen: bool,
) -> Gui {
    let dir = tempfile::tempdir().unwrap();
    let profile = Profile::new(dir.path());
    let saved: Vec<SavedServer> = servers
        .iter()
        .enumerate()
        .map(|(i, (label, addr, nick, pw))| SavedServer {
            key: i as u64 + 1,
            label: (*label).into(),
            addr: addr.to_string(),
            nick: (*nick).into(),
            password: pw.map(Into::into),
            fingerprint: None,
        })
        .collect();
    profile.save_servers(&saved).unwrap();
    let fake = FakeBackend::new(mode);
    let fh = fake.handle();
    let app = VoiceApp::with_options(
        None,
        Box::new(fake),
        profile,
        vec![],
        AppOptions {
            net: fast_net(),
            audio_thread: false,
            fake_screen,
        },
    );
    Gui {
        h: harness(app),
        fake: fh,
        dir,
    }
}

fn has(h: &H, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn has_contains(h: &H, part: &str) -> bool {
    h.query_all_by_label_contains(part).next().is_some()
}

fn labels(h: &H) -> Vec<String> {
    h.root()
        .children_recursive()
        .filter_map(|n| {
            let a = n.accesskit_node();
            a.label().or_else(|| a.value())
        })
        .collect()
}

/// Step frames (with 10 ms real sleeps) until `pred` holds; panics with the label tree.
fn step_until(h: &mut H, what: &str, d: Duration, pred: impl Fn(&H) -> bool) {
    let deadline = Instant::now() + d;
    loop {
        h.step();
        if pred(h) {
            return;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}; labels: {:?}", labels(h));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_label(h: &mut H, label: &str) {
    let l = label.to_string();
    step_until(h, &format!("label {label:?}"), T, move |h| has(h, &l));
}

fn wait_contains(h: &mut H, part: &str) {
    let l = part.to_string();
    step_until(h, &format!("label containing {part:?}"), T, move |h| {
        has_contains(h, &l)
    });
}

fn click(h: &mut H, label: &str) {
    h.get_by_label(label).click();
    h.step();
    h.step();
}

fn click_button(h: &mut H, label: &str) {
    h.get_by_role_and_label(Role::Button, label).click();
    h.step();
    h.step();
}

/// All five events go into ONE frame: kittest advances time by `step_dt` per queued
/// event, which would push the two clicks past egui's 0.3 s double-click window.
fn double_click_at<S>(h: &mut Harness<'_, S>, c: Pos2) {
    h.event(egui::Event::PointerMoved(c));
    h.step();
    let ev = &mut h.input_mut().events;
    for pressed in [true, false, true, false] {
        ev.push(egui::Event::PointerButton {
            pos: c,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    h.step();
    h.step();
}

fn type_into(h: &mut H, label: &str, text: &str) {
    let role = if label == "Password" {
        Role::PasswordInput
    } else {
        Role::TextInput
    };
    h.get_by_role_and_label(role, label).click();
    h.step();
    h.key_combination_modifiers(Modifiers::COMMAND, &[Key::A]);
    h.step();
    h.get_by_role_and_label(role, label).type_text(text);
    h.step();
    h.step();
}

/// Right-click `target`, then click the menu item `item` (the last node with
/// that label: popups are drawn last, above any same-named control).
fn menu(h: &mut H, target: &str, item: &str) {
    h.get_by_label(target).click_secondary();
    h.step();
    h.step();
    h.query_all_by_label(item)
        .last()
        .unwrap_or_else(|| panic!("no menu item {item:?}; labels: {:?}", labels(h)))
        .click();
    h.step();
    h.step();
}

/// Connect through the server's rail menu (does not change the selection).
fn connect(h: &mut H, label: &str) {
    menu(h, label, "Connect");
    wait_label(h, &format!("{label}: Connected"));
}

/// Set the input mode from the mic button's right-click menu.
fn set_mode(h: &mut H, mode: InputMode) {
    menu(h, "Mute", mode.label());
}

/// Open the server dialog's "More" section (label, password) if it is closed.
fn more(h: &mut H) {
    if h.query_all_by_role_and_label(Role::TextInput, "Label")
        .next()
        .is_none()
    {
        click(h, "More");
    }
}

/// Untick the Add dialog's "Connect now".
fn no_connect_now(h: &mut H) {
    h.get_by_role_and_label(Role::CheckBox, "Connect now")
        .click();
    h.step();
    h.step();
}

fn key_of(h: &H, label: &str) -> ServerKey {
    h.state()
        .model
        .saved
        .iter()
        .find(|s| s.label == label)
        .unwrap()
        .key
}

fn wait_server<F: Fn(&TestServer) -> bool>(srv: &TestServer, what: &str, f: F) {
    let deadline = Instant::now() + T;
    while !f(srv) {
        assert!(Instant::now() < deadline, "server condition: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn audio_steps(h: &mut H, fake: &FakeHandle, others: &mut [&mut Client], n: usize) {
    for _ in 0..n {
        let start = Instant::now();
        fake.drive(960);
        h.state_mut().tick_audio();
        h.step();
        for c in others.iter_mut() {
            c.audio_step();
        }
        let el = start.elapsed();
        if el < Duration::from_millis(20) {
            std::thread::sleep(Duration::from_millis(20) - el);
        }
    }
}

// ------------------------------------------------------------------ harness

#[test]
fn double_click_helper_triggers_double_clicked() {
    let mut h = Harness::new_ui_state(
        |ui, st: &mut (u32, u32)| {
            let r = ui.add(egui::Label::new("Target").sense(egui::Sense::click()));
            if r.double_clicked() {
                st.0 += 1;
            }
            if r.clicked() {
                st.1 += 1;
            }
        },
        (0u32, 0u32),
    );
    // A single click is not a double click.
    h.get_by_label("Target").click();
    h.run();
    assert_eq!(h.state().0, 0);
    assert_eq!(h.state().1, 1);
    // Wait past the double-click window so the single click above does not pair up.
    for _ in 0..4 {
        h.step();
    }
    let c = h.get_by_label("Target").rect().center();
    double_click_at(&mut h, c);
    h.run();
    assert_eq!(h.state().0, 1, "double click detected once");
}

// ------------------------------------------------------------------ saved servers

#[test]
fn sidebar_lists_saved_servers() {
    let a: std::net::SocketAddr = "127.0.0.1:7001".parse().unwrap();
    let b: std::net::SocketAddr = "127.0.0.1:7002".parse().unwrap();
    let mut g = gui_with(
        FakeMode::Silent,
        &[("Home", a, "me", None), ("Work", b, "me", None)],
    );
    g.h.step();
    for l in [
        "Home",
        "Work",
        "Home: Disconnected",
        "Work: Disconnected",
        // The centre card of the selected server.
        "Connect Home",
        "Home is not connected",
        "Add server",
    ] {
        assert!(has(&g.h, l), "missing {l:?}: {:?}", labels(&g.h));
    }
    assert!(!has(&g.h, "Home [talking]"));
    assert!(
        !has(&g.h, "Connect Work"),
        "only the selected server's card"
    );
    g.h.check_accessibility();
}

#[test]
fn add_server_dialog_saves_entry() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    more(&mut g.h);
    type_into(&mut g.h, "Label", "Fresh");
    type_into(&mut g.h, "Address", "127.0.0.1:9999");
    type_into(&mut g.h, "Nickname", "neo");
    type_into(&mut g.h, "Password", "pw1");
    assert!(has(&g.h, "Add and connect"));
    no_connect_now(&mut g.h);
    g.h.check_accessibility();
    click_button(&mut g.h, "Add");
    wait_label(&mut g.h, "Fresh: Disconnected");
    assert!(!has(&g.h, "Add a server"), "dialog closed");
    let saved = Profile::new(g.dir.path()).load_servers().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].label, "Fresh");
    assert_eq!(saved[0].addr, "127.0.0.1:9999");
    assert_eq!(saved[0].nick, "neo");
    assert_eq!(saved[0].password.as_deref(), Some("pw1"));
    let text = std::fs::read_to_string(g.dir.path().join("servers.toml")).unwrap();
    assert!(text.contains("Fresh"));
    assert_eq!(g.h.state().model.selected, Some(saved[0].key), "selected");
}

#[test]
fn add_server_without_label_derives_it_and_connects() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[]);
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    type_into(&mut g.h, "Address", &srv.addr().to_string());
    type_into(&mut g.h, "Nickname", "neo");
    click_button(&mut g.h, "Add and connect");
    wait_label(&mut g.h, "127.0.0.1: Connected");
    wait_server(&srv, "neo online", |s| s.nick_channel("neo").is_some());
    let k = key_of(&g.h, "127.0.0.1");
    assert_eq!(g.h.state().model.selected, Some(k));
}

#[test]
fn add_server_rejects_bad_address() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    no_connect_now(&mut g.h);
    more(&mut g.h);
    type_into(&mut g.h, "Label", "Bad");
    type_into(&mut g.h, "Address", "not an address");
    // The inline hint comes from a string check, before any lookup.
    assert!(has(&g.h, "Add a port after a colon, like :7878."));
    click_button(&mut g.h, "Add");
    wait_label(&mut g.h, "Invalid address");
    assert!(has(&g.h, "Add a server"), "dialog stays open");
    assert!(!has(&g.h, "Bad: Disconnected"));
    assert!(
        Profile::new(g.dir.path())
            .load_servers()
            .unwrap()
            .is_empty()
    );
    // Fixing the address saves.
    type_into(&mut g.h, "Address", "localhost:7878");
    assert!(!has(&g.h, "Add a port after a colon, like :7878."));
    click_button(&mut g.h, "Add");
    wait_label(&mut g.h, "Bad: Disconnected");
    // Cancel discards.
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    type_into(&mut g.h, "Address", "127.0.0.1:1234");
    click_button(&mut g.h, "Cancel");
    assert!(!has(&g.h, "Add a server"));
    assert_eq!(Profile::new(g.dir.path()).load_servers().unwrap().len(), 1);
}

#[test]
fn edit_and_remove_server() {
    let a: std::net::SocketAddr = "127.0.0.1:7001".parse().unwrap();
    let mut g = gui_with(
        FakeMode::Silent,
        &[("Home", a, "me", None), ("Work", a, "me", None)],
    );
    menu(&mut g.h, "Home", "Edit server…");
    wait_label(&mut g.h, "Edit server");
    more(&mut g.h);
    type_into(&mut g.h, "Label", "Cottage");
    type_into(&mut g.h, "Address", "127.0.0.1:7010");
    click_button(&mut g.h, "Save");
    wait_label(&mut g.h, "Cottage: Disconnected");
    assert!(!has(&g.h, "Home"));
    let saved = Profile::new(g.dir.path()).load_servers().unwrap();
    assert_eq!(saved[0].label, "Cottage");
    assert_eq!(saved[0].addr, "127.0.0.1:7010");
    menu(&mut g.h, "Cottage", "Remove server");
    step_until(&mut g.h, "Cottage removed", T, |h| !has(h, "Cottage"));
    assert!(has(&g.h, "Work"));
    let saved = Profile::new(g.dir.path()).load_servers().unwrap();
    assert_eq!(
        saved.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(),
        ["Work"]
    );
}

// ------------------------------------------------------------------ connect & status

#[test]
fn connect_and_disconnect_buttons_real_server() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    // The selected server's centre card connects.
    click_button(&mut g.h, "Connect S");
    wait_label(&mut g.h, "S: Connected");
    assert!(has(&g.h, "S [talking]"));
    wait_server(&srv, "alice online", |s| s.nick_channel("alice").is_some());
    menu(&mut g.h, "S", "Disconnect");
    wait_label(&mut g.h, "S: Disconnected");
    assert!(has(&g.h, "Connect S"));
    wait_server(&srv, "alice gone", |s| s.snapshot().users.is_empty());
    g.h.check_accessibility();
}

#[test]
fn status_label_reflects_reconnecting_and_error() {
    let mut srv = TestServer::start(|_| {});
    let pw = TestServer::start(|c| c.password = Some("pw".into()));
    let mut g = gui_with(
        FakeMode::Silent,
        &[
            ("S", srv.addr(), "alice", None),
            ("P", pw.addr(), "alice", Some("bad")),
        ],
    );
    connect(&mut g.h, "S");
    srv.stop();
    wait_contains(&mut g.h, "S: Reconnecting (");
    menu(&mut g.h, "P", "Connect");
    wait_label(&mut g.h, "P: Error: Wrong server password");
    // Server comes back: status returns to Connected on its own.
    srv.restart();
    wait_label(&mut g.h, "S: Connected");
}

// ------------------------------------------------------------------ channel tree

fn bob(srv: &TestServer) -> (Client, ServerKey) {
    let mut b = client_with(FakeMode::Silent);
    let k = b.add("S", srv.addr(), "bob");
    b.connect(k);
    (b, k)
}

#[test]
fn channel_tree_renders_users_and_motd() {
    let srv = TestServer::start(|c| {
        c.motd = "Hello from the motd".into();
        c.channels = vec!["Lobby".into(), "AFK".into(), "Games".into()];
    });
    let (_b, _) = bob(&srv);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "bob");
    for l in [
        "Hello from the motd",
        // My channel is named as is; the others offer to join.
        "Lobby",
        "Join AFK",
        "Join Games",
        "alice",
        "bob",
        "New channel",
    ] {
        assert!(has(&g.h, l), "missing {l:?}: {:?}", labels(&g.h));
    }
    g.h.check_accessibility();
}

#[test]
fn click_channel_row_joins() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "Join AFK");
    assert_eq!(srv.nick_channel("alice").as_deref(), Some("Lobby"));
    click(&mut g.h, "Join AFK");
    wait_server(&srv, "alice in AFK", |s| {
        s.nick_channel("alice").as_deref() == Some("AFK")
    });
    let k = key_of(&g.h, "S");
    step_until(&mut g.h, "client sees the move", T, |h| {
        h.state().model.session(k).unwrap().my_channel() == Some(vc_proto::ChannelId(2))
    });
    // The channel menu joins too.
    menu(&mut g.h, "Join Lobby", "Join channel");
    wait_server(&srv, "alice in Lobby", |s| {
        s.nick_channel("alice").as_deref() == Some("Lobby")
    });
}

#[test]
fn join_button_joins() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    click_button(&mut g.h, "Join AFK");
    wait_server(&srv, "alice in AFK", |s| {
        s.nick_channel("alice").as_deref() == Some("AFK")
    });
    click_button(&mut g.h, "Join Lobby");
    wait_server(&srv, "alice in Lobby", |s| {
        s.nick_channel("alice").as_deref() == Some("Lobby")
    });
}

#[test]
fn create_rename_delete_channel_inline() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Raid");
    g.h.key_press(Key::Enter);
    g.h.step();
    wait_label(&mut g.h, "Raid");
    wait_server(&srv, "alice auto-joined Raid", |s| {
        s.nick_channel("alice").as_deref() == Some("Raid")
    });
    assert!(has(&g.h, "temp"));
    assert!(!has(&g.h, "Channel name"), "edit closed");
    menu(&mut g.h, "Raid", "Rename…");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Raid Night");
    g.h.key_press(Key::Enter);
    g.h.step();
    wait_label(&mut g.h, "Raid Night");
    assert!(!has(&g.h, "Raid"));
    menu(&mut g.h, "Raid Night", "Delete channel");
    step_until(&mut g.h, "channel gone", T, |h| !has(h, "Raid Night"));
    wait_server(&srv, "alice back in Lobby", |s| {
        s.nick_channel("alice").as_deref() == Some("Lobby")
    });
    assert_eq!(srv.snapshot().channels.len(), 2);
}

#[test]
fn inline_channel_edit_esc_and_blank_blur_cancel() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    // Esc cancels a rename, even with text typed (Lobby, the default channel,
    // cannot be renamed).
    menu(&mut g.h, "Join AFK", "Rename…");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Zzz");
    g.h.key_press(Key::Escape);
    g.h.step();
    g.h.step();
    assert!(!has(&g.h, "Channel name"), "{:?}", labels(&g.h));
    assert!(g.h.state().model.ui.channel_dialog.is_none());
    // A blank new channel loses focus: cancelled, no error toast.
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    g.h.key_press(Key::Tab);
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.ui.channel_dialog.is_none());
    assert!(
        g.h.state().model.ui.toasts.is_empty(),
        "{:?}",
        g.h.state().model.ui.toasts
    );
    std::thread::sleep(Duration::from_millis(200));
    let names: Vec<String> = srv
        .snapshot()
        .channels
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(!names.iter().any(|n| n == "Zzz"));
}

#[test]
fn new_channel_while_renaming_keeps_the_rename() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Raid");
    g.h.key_press(Key::Enter);
    g.h.step();
    wait_label(&mut g.h, "Raid");
    menu(&mut g.h, "Raid", "Rename…");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Raid Night");
    // Clicking "+" blurs the rename field in the same frame.
    click_button(&mut g.h, "New channel");
    wait_server(&srv, "Raid renamed", |s| {
        s.snapshot().channels.iter().any(|c| c.name == "Raid Night")
    });
    g.h.step();
    let toast = format!(
        "Error: {}",
        vc_client::session::error_text(vc_proto::ErrorCode::BadChannelName)
    );
    assert!(!has(&g.h, &toast), "{:?}", labels(&g.h));
    assert!(
        g.h.state().model.ui.toasts.is_empty(),
        "{:?}",
        g.h.state().model.ui.toasts
    );
    let d = g.h.state().model.ui.channel_dialog.clone().unwrap();
    assert_eq!(
        d.mode,
        vc_client::app::ChannelDialogMode::New,
        "new edit open"
    );
}

#[test]
fn unchanged_rename_then_click_elsewhere_sends_nothing() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    menu(&mut g.h, "Join AFK", "Rename…");
    wait_label(&mut g.h, "Channel name");
    // Click elsewhere without typing: the field blurs with the old name.
    g.h.get_by_role_and_label(Role::TextInput, "Message")
        .click();
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.ui.channel_dialog.is_none(), "edit closed");
    std::thread::sleep(Duration::from_millis(300));
    g.h.step();
    g.h.step();
    assert!(
        g.h.state().model.ui.toasts.is_empty(),
        "{:?}",
        g.h.state().model.ui.toasts
    );
    let names: Vec<String> = srv
        .snapshot()
        .channels
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert_eq!(names, ["Lobby", "AFK"]);
}

#[test]
fn hidden_inline_edit_does_not_block_shortcuts() {
    let s1 = TestServer::start(|_| {});
    let s2 = TestServer::start(|_| {});
    let mut g = gui_with(
        FakeMode::Silent,
        &[
            ("One", s1.addr(), "alice", None),
            ("Two", s2.addr(), "alice", None),
        ],
    );
    connect(&mut g.h, "One");
    connect(&mut g.h, "Two");
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    click(&mut g.h, "Two");
    assert!(g.h.state().model.ui.channel_dialog.is_none(), "pruned");
    g.h.key_combination_modifiers(Modifiers::COMMAND, &[Key::M]);
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.voice.muted, "Ctrl+M works again");
}

#[test]
fn blank_channel_name_shows_toast() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "   ");
    g.h.key_press(Key::Enter);
    g.h.step();
    let toast = format!(
        "Error: {}",
        vc_client::session::error_text(vc_proto::ErrorCode::BadChannelName)
    );
    wait_label(&mut g.h, &toast);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(srv.snapshot().channels.len(), 2, "no channel created");
    // Positive control: a real name works.
    click_button(&mut g.h, "New channel");
    wait_label(&mut g.h, "Channel name");
    type_into(&mut g.h, "Channel name", "Room");
    g.h.key_press(Key::Enter);
    g.h.step();
    wait_server(&srv, "Room created", |s| {
        s.snapshot().channels.iter().any(|c| c.name == "Room")
    });
}

// ------------------------------------------------------------------ multiserver

#[test]
fn two_servers_connected_both_listed() {
    let s1 = TestServer::start(|c| c.name = "Server One".into());
    let s2 = TestServer::start(|c| c.name = "Server Two".into());
    let mut g = gui_with(
        FakeMode::Silent,
        &[
            ("One", s1.addr(), "alice", None),
            ("Two", s2.addr(), "alice", None),
        ],
    );
    connect(&mut g.h, "One");
    connect(&mut g.h, "Two");
    assert!(has(&g.h, "One: Connected"));
    assert!(has(&g.h, "Two: Connected"));
    wait_server(&s1, "on s1", |s| s.nick_channel("alice").is_some());
    wait_server(&s2, "on s2", |s| s.nick_channel("alice").is_some());
}

#[test]
fn sidebar_select_switches_channel_view() {
    let s1 = TestServer::start(|c| {
        c.motd = "motd of one".into();
        c.channels = vec!["OneLobby".into()];
    });
    let s2 = TestServer::start(|c| {
        c.motd = "motd of two".into();
        c.channels = vec!["TwoLobby".into()];
    });
    let mut g = gui_with(
        FakeMode::Silent,
        &[
            ("One", s1.addr(), "alice", None),
            ("Two", s2.addr(), "alice", None),
        ],
    );
    connect(&mut g.h, "One");
    connect(&mut g.h, "Two");
    wait_label(&mut g.h, "motd of one");
    assert!(!has(&g.h, "motd of two"));
    click(&mut g.h, "Two");
    wait_label(&mut g.h, "motd of two");
    assert!(has(&g.h, "TwoLobby"));
    assert!(!has(&g.h, "OneLobby"));
    click(&mut g.h, "One");
    wait_label(&mut g.h, "OneLobby");
}

#[test]
fn talk_here_sets_marker_and_target() {
    let s1 = TestServer::start(|_| {});
    let s2 = TestServer::start(|_| {});
    let mut g = gui_with(
        FakeMode::Silent,
        &[
            ("One", s1.addr(), "alice", None),
            ("Two", s2.addr(), "alice", None),
        ],
    );
    connect(&mut g.h, "One");
    connect(&mut g.h, "Two");
    assert!(has(&g.h, "One [talking]"));
    assert!(!has(&g.h, "Two [talking]"));
    menu(&mut g.h, "Two", "Talk here");
    wait_label(&mut g.h, "Two [talking]");
    assert!(!has(&g.h, "One [talking]"));
    assert_eq!(g.h.state().model.talk_target, Some(key_of(&g.h, "Two")));
    assert_eq!(
        g.h.state().ctl.lock().unwrap().talk_target,
        Some(key_of(&g.h, "Two"))
    );
}

// ------------------------------------------------------------------ chat

#[test]
fn chat_send_and_display_two_clients() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    type_into(&mut a.h, "Message", "hello there");
    click_button(&mut a.h, "Send");
    wait_contains(&mut b.h, "alice: hello there");
    wait_contains(&mut a.h, "alice: hello there");
    // Lines carry a UTC timestamp.
    let line = labels(&b.h)
        .into_iter()
        .find(|l| l.contains("alice: hello there"))
        .unwrap();
    assert!(line.starts_with('[') && line.contains(" UTC] "), "{line}");
    type_into(&mut b.h, "Message", "hi alice");
    click_button(&mut b.h, "Send");
    wait_contains(&mut a.h, "bob: hi alice");
    assert_eq!(a.h.state().model.ui.chat_input, "");
}

#[test]
fn chat_input_limited_to_500_chars() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    type_into(&mut a.h, "Message", &"z".repeat(501));
    assert_eq!(
        a.h.state().model.ui.chat_input.chars().count(),
        500,
        "field capped"
    );
    click_button(&mut a.h, "Send");
    wait_contains(&mut b.h, "alice: zzz");
    let line = labels(&b.h)
        .into_iter()
        .find(|l| l.contains("alice: z"))
        .unwrap();
    let body = line.split("alice: ").nth(1).unwrap();
    assert_eq!(body.chars().count(), 500, "{line}");
    assert!(body.chars().all(|c| c == 'z'));
    assert_eq!(a.h.state().model.ui.chat_input, "", "sent draft cleared");
}

#[test]
fn chat_rejected_intent_keeps_draft_and_toasts() {
    let srv = TestServer::start(|_| {});
    let (mut other, ok) = bob(&srv);
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    a.h.state_mut().model.ui.chat_input = "q".repeat(501);
    a.h.step();
    click_button(&mut a.h, "Send");
    wait_label(&mut a.h, "Error: Message too long");
    assert_eq!(
        a.h.state().model.ui.chat_input.chars().count(),
        501,
        "draft kept"
    );
    std::thread::sleep(Duration::from_millis(300));
    other.app.poll();
    assert!(
        other.app.model.session(ok).unwrap().chat.is_empty(),
        "nothing was sent"
    );
    // Positive control: a valid message from the same field is delivered.
    a.h.state_mut().model.ui.chat_input = "short".into();
    a.h.step();
    click_button(&mut a.h, "Send");
    other.wait(
        |x| !x.model.session(ok).unwrap().chat.is_empty(),
        "valid chat delivered",
    );
}

#[test]
fn chat_enter_key_sends() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    type_into(&mut a.h, "Message", "via enter");
    a.h.key_press(Key::Enter);
    a.h.step();
    wait_contains(&mut a.h, "alice: via enter");
    assert_eq!(a.h.state().model.ui.chat_input, "");
}

#[test]
fn chat_view_switches_with_channel() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    type_into(&mut a.h, "Message", "lobby line");
    click_button(&mut a.h, "Send");
    wait_contains(&mut a.h, "alice: lobby line");
    assert!(has(&a.h, "Chat: Lobby"));
    click_button(&mut a.h, "Join AFK");
    wait_label(&mut a.h, "Chat: AFK");
    assert!(
        !has_contains(&a.h, "lobby line"),
        "AFK view hides Lobby chat"
    );
    type_into(&mut a.h, "Message", "afk line");
    click_button(&mut a.h, "Send");
    wait_contains(&mut a.h, "alice: afk line");
    click_button(&mut a.h, "Join Lobby");
    wait_label(&mut a.h, "Chat: Lobby");
    assert!(has_contains(&a.h, "alice: lobby line"));
    assert!(!has_contains(&a.h, "afk line"));
}

// ------------------------------------------------------------------ mute / deafen

#[test]
fn mute_toggle_seen_by_other_client() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    wait_label(&mut b.h, "alice");
    click_button(&mut a.h, "Mute");
    wait_label(&mut a.h, "Unmute");
    wait_label(&mut b.h, "alice [muted]");
    click_button(&mut a.h, "Unmute");
    wait_label(&mut b.h, "alice");
    assert!(has(&a.h, "Mute"));
}

#[test]
fn deafen_toggle_labels_and_implies_mute() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    click_button(&mut a.h, "Deafen");
    wait_label(&mut a.h, "Undeafen");
    assert!(has(&a.h, "Unmute"), "deafen implies mute");
    wait_label(&mut a.h, "alice [muted] [deaf]");
    wait_server(&srv, "server sees deaf", |s| {
        s.snapshot().users.iter().any(|u| u.deafened && u.muted)
    });
    click_button(&mut a.h, "Undeafen");
    wait_label(&mut a.h, "Deafen");
    assert!(
        has(&a.h, "Mute"),
        "undeafen restores the earlier (unmuted) state"
    );
    wait_label(&mut a.h, "alice");
}

// ------------------------------------------------------------------ input

fn select_combo(h: &mut H, combo: &str, option: &str) {
    h.get_by_role_and_label(Role::ComboBox, combo).click();
    h.step();
    h.step();
    h.get_by_role_and_label(Role::Button, option).click();
    h.step();
    h.step();
}

#[test]
fn input_mode_menu_switches() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    assert_eq!(g.h.state().model.settings.input_mode, InputMode::Vad);
    set_mode(&mut g.h, InputMode::Ptt);
    assert_eq!(g.h.state().model.settings.input_mode, InputMode::Ptt);
    assert_eq!(
        Profile::new(g.dir.path())
            .load_settings()
            .unwrap()
            .input_mode,
        InputMode::Ptt
    );
    set_mode(&mut g.h, InputMode::Open);
    assert_eq!(g.h.state().model.settings.input_mode, InputMode::Open);
    set_mode(&mut g.h, InputMode::Vad);
    assert_eq!(g.h.state().model.settings.input_mode, InputMode::Vad);
    // A plain click on the mic still mutes (the menu is right-click only).
    click_button(&mut g.h, "Mute");
    assert!(g.h.state().model.voice.muted);
}

#[test]
fn ptt_space_held_sends_voice_released_stops() {
    let srv = TestServer::start(|_| {});
    let (mut listener, _) = bob(&srv);
    let mut g = gui_with(FakeMode::Sine(440.0), &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    set_mode(&mut g.h, InputMode::Ptt);
    let fake = g.fake.clone();
    // Released: nothing is heard.
    listener.fake.clear_played();
    audio_steps(&mut g.h, &fake, &mut [&mut listener], 30);
    assert!(has(&g.h, "PTT: released"));
    assert!(rms_db(&listener.fake.played()) < -90.0, "silent before PTT");
    // Held: voice arrives.
    g.h.key_down(Key::Space);
    g.h.step();
    wait_label(&mut g.h, "PTT: held");
    listener.fake.clear_played();
    audio_steps(&mut g.h, &fake, &mut [&mut listener], 40);
    assert!(rms_db(&listener.fake.played()) > -30.0, "voice while held");
    // Released again: stops.
    g.h.key_up(Key::Space);
    g.h.step();
    wait_label(&mut g.h, "PTT: released");
    audio_steps(&mut g.h, &fake, &mut [&mut listener], 15); // drain jitter buffer
    listener.fake.clear_played();
    audio_steps(&mut g.h, &fake, &mut [&mut listener], 30);
    assert!(
        rms_db(&listener.fake.played()) < -90.0,
        "silent after release"
    );
}

#[test]
fn ptt_ignored_while_typing_in_chat() {
    // The composer is shown for a connected server.
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    set_mode(&mut g.h, InputMode::Ptt);
    g.h.get_by_role_and_label(Role::TextInput, "Message")
        .click();
    g.h.step();
    g.h.key_down(Key::Space);
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "PTT: released"));
    assert!(!g.h.state().model.ptt_down);
    g.h.key_up(Key::Space);
    g.h.step();
    // Positive control: unfocus the text field, then Space works.
    g.h.key_press(Key::Escape);
    g.h.step();
    g.h.step();
    g.h.key_down(Key::Space);
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "PTT: held"), "{:?}", labels(&g.h));
    assert!(g.h.state().model.ptt_down);
}

// ------------------------------------------------------------------ speaking & volume

#[test]
fn speaking_indicator_shown() {
    let srv = TestServer::start(|_| {});
    let mut talker = client_with(FakeMode::Sine(440.0));
    let tk = talker.add("S", srv.addr(), "carol");
    talker.connect(tk);
    talker.mode(InputMode::Open);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "carol");
    let fake = g.fake.clone();
    audio_steps(&mut g.h, &fake, &mut [&mut talker], 10);
    wait_label(&mut g.h, "carol (speaking)");
    // Stops ~300 ms after the voice stops.
    talker.mode(InputMode::Ptt);
    audio_steps(&mut g.h, &fake, &mut [&mut talker], 5);
    step_until(&mut g.h, "speaking expires", T, |h| has(h, "carol"));
}

#[test]
fn per_user_volume_slider_updates_and_persists() {
    let srv = TestServer::start(|_| {});
    let (_b, _) = bob(&srv);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "bob");
    g.h.get_by_label("bob").click_secondary();
    g.h.step();
    g.h.step();
    wait_label(&mut g.h, "Volume bob");
    g.h.get_by_role_and_label(Role::Slider, "Volume bob")
        .set_value_accesskit(150.0);
    g.h.step();
    g.h.step();
    let k = key_of(&g.h, "S");
    assert!((g.h.state().model.settings.user_volume(k, "bob") - 1.5).abs() < 1e-3);
    let on_disk = Profile::new(g.dir.path()).load_settings().unwrap();
    assert!((on_disk.user_volume(k, "bob") - 1.5).abs() < 1e-3);
    // The gain reaches the audio pipeline control block.
    let bob_id =
        g.h.state()
            .model
            .session(k)
            .unwrap()
            .users
            .values()
            .find(|u| u.nick == "bob")
            .unwrap()
            .id;
    g.h.step();
    let gain =
        g.h.state()
            .ctl
            .lock()
            .unwrap()
            .gains
            .get(&(k, bob_id))
            .copied();
    assert_eq!(gain, Some(1.5));
    // The value is shown as plain text, with no editable value box. egui's DragValue
    // (the slider's number box) reports `Role::SpinButton` to kittest.
    g.h.get_by_role_and_label(Role::Slider, "Volume bob")
        .set_value_accesskit(45.0);
    g.h.step();
    g.h.step();
    wait_label(&mut g.h, "bob: 45 %");
    assert!((g.h.state().model.settings.user_volume(k, "bob") - 0.45).abs() < 1e-3);
    assert!(
        g.h.query_all_by_role(Role::SpinButton).next().is_none(),
        "no editable value box in the volume menu"
    );
}

// ------------------------------------------------------------------ settings

fn open_settings(h: &mut H) {
    click_button(h, "Settings");
    wait_label(h, "Close settings");
}

fn open_settings_tab(h: &mut H, tab: &str) {
    open_settings(h);
    h.get_by_role_and_label(Role::Tab, tab).click();
    h.step();
    h.step();
}

fn close_settings(h: &mut H) {
    click_button(h, "Close settings");
    step_until(h, "settings closed", T, |h| !has(h, "Close settings"));
}

#[test]
fn settings_device_combos_list_backend_devices() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    open_settings(&mut g.h);
    g.h.get_by_role_and_label(Role::ComboBox, "Microphone")
        .click();
    g.h.step();
    g.h.step();
    assert!(
        has(&g.h, "Fake input 1") && has(&g.h, "Fake input 2"),
        "{:?}",
        labels(&g.h)
    );
    g.h.key_press(Key::Escape);
    g.h.step();
    g.h.step();
    g.h.get_by_role_and_label(Role::ComboBox, "Speakers")
        .click();
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "Fake output 1") && has(&g.h, "Fake output 2"));
    g.h.check_accessibility();
}

#[test]
fn settings_output_device_selection_persists() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    open_settings(&mut g.h);
    select_combo(&mut g.h, "Speakers", "Fake output 2");
    select_combo(&mut g.h, "Microphone", "Fake input 2");
    // Saved as made: no Save button.
    close_settings(&mut g.h);
    let s = Profile::new(g.dir.path()).load_settings().unwrap();
    assert_eq!(s.output_device.as_deref(), Some("fake-output-2"));
    assert_eq!(s.input_device.as_deref(), Some("fake-input-2"));
    // Streams were restarted on the chosen devices.
    let (i, o) = g.fake.selected_devices();
    assert_eq!(i.as_deref(), Some("fake-input-2"));
    assert_eq!(o.as_deref(), Some("fake-output-2"));
    assert!(g.fake.capture_running() && g.fake.playback_running());
}

#[test]
fn settings_vad_slider_persists() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    open_settings(&mut g.h);
    assert!(has(&g.h, "Opens at -45 dB"));
    g.h.get_by_role_and_label(Role::Slider, "Sensitivity")
        .set_value_accesskit(-30.0);
    g.h.step();
    g.h.step();
    // Applied and saved at once.
    assert_eq!(g.h.state().model.settings.vad_threshold_db, -30.0);
    assert!(has(&g.h, "Opens at -30 dB"));
    assert_eq!(
        Profile::new(g.dir.path())
            .load_settings()
            .unwrap()
            .vad_threshold_db,
        -30.0
    );
    assert_eq!(g.h.state().ctl.lock().unwrap().vad_threshold_db, -30.0);
}

#[test]
fn settings_output_volume_persists() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    open_settings(&mut g.h);
    g.h.get_by_role_and_label(Role::Slider, "Output volume")
        .set_value_accesskit(150.0);
    g.h.step();
    g.h.step();
    assert!((g.h.state().model.settings.master_volume - 1.5).abs() < 1e-4);
    assert!(
        (Profile::new(g.dir.path())
            .load_settings()
            .unwrap()
            .master_volume
            - 1.5)
            .abs()
            < 1e-4
    );
    assert!((g.h.state().ctl.lock().unwrap().master - 1.5).abs() < 1e-4);
    // Esc closes the window; the value stays.
    g.h.key_press(Key::Escape);
    g.h.step();
    g.h.step();
    assert!(!has(&g.h, "Close settings"));
    assert!((g.h.state().model.settings.master_volume - 1.5).abs() < 1e-4);
}

#[test]
fn settings_ptt_key_change_applies() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    set_mode(&mut g.h, InputMode::Ptt);
    open_settings(&mut g.h);
    click_button(&mut g.h, vc_client::gui::PTT_KEY_BUTTON);
    step_until(&mut g.h, "listening", T, |h| {
        h.state().model.ui.ptt_listening
    });
    // Listening: the pressed key is not push to talk.
    g.h.key_down(Key::Space);
    g.h.step();
    assert!(!g.h.state().model.ptt_down);
    g.h.key_up(Key::Space);
    g.h.step();
    g.h.step();
    assert_eq!(
        g.h.state().model.settings.ptt_key,
        "Space",
        "first key taken"
    );
    // Space was taken; listen again for F5.
    click_button(&mut g.h, vc_client::gui::PTT_KEY_BUTTON);
    step_until(&mut g.h, "listening", T, |h| {
        h.state().model.ui.ptt_listening
    });
    g.h.key_press(Key::F5);
    g.h.step();
    g.h.step();
    assert!(!g.h.state().model.ui.ptt_listening);
    assert_eq!(g.h.state().model.settings.ptt_key, "F5");
    assert_eq!(
        Profile::new(g.dir.path()).load_settings().unwrap().ptt_key,
        "F5"
    );
    close_settings(&mut g.h);
    g.h.key_down(Key::Space);
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "PTT: released"), "Space no longer the PTT key");
    g.h.key_up(Key::Space);
    g.h.key_down(Key::F5);
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "PTT: held"));
    g.h.key_up(Key::F5);
    g.h.step();
    g.h.step();
    assert!(has(&g.h, "PTT: released"));
}

#[test]
fn ptt_listening_stops_when_leaving_the_voice_tab() {
    let mut g = gui_with(FakeMode::Silent, &[]);
    set_mode(&mut g.h, InputMode::Ptt);
    open_settings(&mut g.h);
    click_button(&mut g.h, vc_client::gui::PTT_KEY_BUTTON);
    step_until(&mut g.h, "listening", T, |h| {
        h.state().model.ui.ptt_listening
    });
    g.h.get_by_role_and_label(Role::Tab, "Profile").click();
    g.h.step();
    g.h.step();
    assert!(!g.h.state().model.ui.ptt_listening);
    type_into(&mut g.h, "Default nickname", "Bob");
    assert_eq!(g.h.state().model.ui.nick_edit, "Bob");
    assert_eq!(g.h.state().model.settings.ptt_key, "Space", "not rebound");
    // Esc first leaves the field, then closes the window.
    for _ in 0..2 {
        if !has(&g.h, "Close settings") {
            break;
        }
        g.h.key_press(Key::Escape);
        g.h.step();
        g.h.step();
    }
    assert!(!has(&g.h, "Close settings"), "Esc closes settings");
    assert_eq!(g.h.state().model.settings.ptt_key, "Space");
}

#[test]
fn settings_nickname_applies_on_reconnect() {
    let mut srv = TestServer::start(|_| {});
    // Fresh profile, empty Settings nickname: add the server keeping the dialog's
    // default nick, which then follows a Settings change.
    let mut g = gui_with(FakeMode::Silent, &[]);
    assert_eq!(g.h.state().model.settings.nickname, "");
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    more(&mut g.h);
    type_into(&mut g.h, "Label", "S");
    type_into(&mut g.h, "Address", &srv.addr().to_string());
    no_connect_now(&mut g.h);
    click_button(&mut g.h, "Add");
    wait_label(&mut g.h, "S: Disconnected");
    assert_eq!(
        g.h.state().model.saved[0].nick,
        vc_client::app::DEFAULT_NICK
    );
    connect(&mut g.h, "S");
    wait_server(&srv, "default nick online", |s| {
        s.nick_channel("user").is_some()
    });
    open_settings_tab(&mut g.h, "Profile");
    assert!(has(&g.h, "applies on next Connect"));
    type_into(&mut g.h, "Default nickname", "alicia");
    // Committed when the window closes, even with the field still focused.
    close_settings(&mut g.h);
    assert_eq!(g.h.state().model.settings.nickname, "alicia");
    // Still the old nick while connected.
    std::thread::sleep(Duration::from_millis(200));
    assert!(srv.nick_channel("user").is_some());
    assert!(srv.nick_channel("alicia").is_none());
    // An automatic reconnect (server restart, same port and cert) keeps the old nick.
    // The restarted server starts empty, so the old nick showing up there proves
    // the reconnect; the Reconnecting status itself can last less than a frame.
    srv.restart();
    wait_server(&srv, "old nick back online", |s| {
        s.nick_channel("user").is_some()
    });
    wait_label(&mut g.h, "S: Connected");
    assert!(srv.nick_channel("alicia").is_none());
    // A manual Disconnect + Connect uses the new nick.
    menu(&mut g.h, "S", "Disconnect");
    wait_label(&mut g.h, "S: Disconnected");
    connect(&mut g.h, "S");
    wait_server(&srv, "new nick online", |s| {
        s.nick_channel("alicia").is_some()
    });
    assert!(srv.nick_channel("user").is_none());
    assert_eq!(
        Profile::new(g.dir.path()).load_servers().unwrap()[0].nick,
        "alicia"
    );
}

// ------------------------------------------------------------------ errors & identity

#[test]
fn error_toast_shown_and_dismissed() {
    let srv = TestServer::start(|_| {});
    let mut taken = client_with(FakeMode::Silent);
    let k = taken.add("S", srv.addr(), "alice");
    taken.connect(k);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    click_button(&mut g.h, "Connect S");
    wait_label(&mut g.h, "Error: Nickname already in use");
    assert!(has(&g.h, "S: Error: Nickname already in use"));
    assert!(has(&g.h, "Dismiss 1"));
    click_button(&mut g.h, "Dismiss 1");
    step_until(&mut g.h, "toast dismissed", T, |h| {
        !has(h, "Error: Nickname already in use")
    });
    assert!(!has(&g.h, "Dismiss 1"));
}

#[test]
fn wrong_password_shows_error() {
    let srv = TestServer::start(|c| c.password = Some("secret".into()));
    let mut g = gui_with(
        FakeMode::Silent,
        &[("S", srv.addr(), "alice", Some("nope"))],
    );
    click_button(&mut g.h, "Connect S");
    wait_label(&mut g.h, "S: Error: Wrong server password");
    wait_label(&mut g.h, "Error: Wrong server password"); // the toast
    // Fix the password through the Edit dialog and connect.
    menu(&mut g.h, "S", "Edit server…");
    wait_label(&mut g.h, "Edit server");
    more(&mut g.h);
    type_into(&mut g.h, "Password", "secret");
    click_button(&mut g.h, "Save");
    connect(&mut g.h, "S");
}

#[test]
fn tofu_dialog_trust_and_cancel() {
    let mut srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    let new_dir = tempfile::tempdir().unwrap();
    srv.restart_with_cert_dir(new_dir.path().to_path_buf());
    wait_label(&mut g.h, "S has a new identity");
    assert!(
        has_contains(&g.h, &srv.fingerprint_hex()),
        "new fingerprint shown"
    );
    assert!(has(&g.h, "Copy fingerprint"));
    click_button(&mut g.h, "Stay safe, cancel");
    step_until(&mut g.h, "dialog closed", T, |h| {
        !has(h, "Trust new identity")
    });
    assert!(has(&g.h, "S: Error: Server identity changed"));
    // Connecting again with the old pin raises the dialog again.
    click_button(&mut g.h, "Connect S");
    wait_label(&mut g.h, "Trust new identity");
    click_button(&mut g.h, "Trust new identity");
    wait_label(&mut g.h, "S: Connected");
    assert_eq!(
        Profile::new(g.dir.path()).load_servers().unwrap()[0]
            .fingerprint
            .as_deref(),
        Some(srv.fingerprint_hex().as_str())
    );
}

#[test]
fn no_audio_device_banner() {
    let (app, _d) = null_client();
    let mut h = harness(app);
    h.step();
    assert!(has(&h, NO_AUDIO_BANNER));
    // Positive control: with working (fake) devices there is no banner.
    let mut g = gui_with(FakeMode::Silent, &[]);
    g.h.step();
    assert!(!has(&g.h, NO_AUDIO_BANNER));
    let _ = NullBackend;
}

// ------------------------------------------------------------------ real wiring

#[test]
fn gui_against_real_server() {
    let srv = TestServer::start(|_| {});
    let (mut watcher, wk) = bob(&srv);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "Lobby");
    wait_label(&mut g.h, "bob");
    wait_label(&mut g.h, "alice");
    click_button(&mut g.h, "Join AFK");
    wait_server(&srv, "alice moved", |s| {
        s.nick_channel("alice").as_deref() == Some("AFK")
    });
    watcher.wait(
        |a| {
            let s = a.model.session(wk).unwrap();
            s.users
                .values()
                .any(|u| u.nick == "alice" && u.channel == vc_proto::ChannelId(2))
        },
        "second client sees alice in AFK",
    );
    assert_eq!(
        g.h.state().model.status(key_of(&g.h, "S")),
        ConnStatus::Connected
    );
}

#[test]
fn gui_eframe_app_polls_net_events() {
    let srv = TestServer::start(|_| {});
    let dir = tempfile::tempdir().unwrap();
    let profile = Profile::new(dir.path());
    profile
        .save_servers(&[SavedServer {
            key: 1,
            label: "S".into(),
            addr: srv.addr().to_string(),
            nick: "eframe".into(),
            password: None,
            fingerprint: None,
        }])
        .unwrap();
    let mut h: H = Harness::builder()
        .with_size([1280.0, 860.0])
        .build_eframe(move |cc| {
            VoiceApp::new(
                cc.egui_ctx.clone(),
                Box::new(FakeBackend::new(FakeMode::Silent)),
                profile,
                vec!["S".into()],
            )
        });
    step_until(&mut h, "eframe app connected", T, |h| {
        has(h, "S: Connected")
    });
    wait_server(&srv, "eframe user online", |s| {
        s.nick_channel("eframe").is_some()
    });
    h.check_accessibility();
}

// ------------------------------------------------------------------ images and screen share

/// A dropped file whose bytes must never be read on the UI thread.
#[derive(Debug)]
struct TestDrop(std::path::PathBuf);

impl egui::DroppedFile for TestDrop {
    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        panic!("dropped files must not be read on the UI thread");
    }
}

fn drop_file(h: &mut H, path: std::path::PathBuf) {
    h.input_mut().dropped_files.push(Arc::new(TestDrop(path)));
    h.step();
}

fn paste_image(h: &mut H, w: usize, hgt: usize) {
    h.get_by_role_and_label(Role::TextInput, "Message").click();
    h.step();
    h.event(egui::Event::PasteImage(Arc::new(egui::ColorImage::filled(
        [w, hgt],
        egui::Color32::from_rgb(20, 120, 220),
    ))));
    h.step();
    h.step();
}

fn write_png(dir: &std::path::Path, name: &str, w: u32, h: u32) -> std::path::PathBuf {
    let p = dir.join(name);
    image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 77, 255]))
        .save(&p)
        .unwrap();
    p
}

#[test]
fn chat_paste_image_reaches_peer() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    assert!(has(&a.h, "Paste (Ctrl+V) or drop an image to send"));
    paste_image(&mut a.h, 64, 48);
    wait_contains(&mut b.h, "alice: [image 64x48]");
    wait_contains(&mut a.h, "alice: [image 64x48]");
    assert_eq!(a.h.state().model.ui.chat_input, "", "paste does not type");
    let key = key_of(&b.h, "S");
    let s = b.h.state().model.session(key).unwrap();
    let img = s.images.values().next().unwrap();
    assert_eq!(img.size, [64, 48]);
    assert_eq!(img.pixels[0], egui::Color32::from_rgb(20, 120, 220));
    assert!(has(&b.h, "View image from alice"));
    assert!(!b.h.state().textures.is_empty(), "thumbnail uploaded");
}

#[test]
fn chat_drop_image_file() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    let p = write_png(a.dir.path(), "pic.png", 30, 20);
    drop_file(&mut a.h, p);
    wait_contains(&mut b.h, "alice: [image 30x20]");
}

#[test]
fn chat_drop_non_image_toasts() {
    let srv = TestServer::start(|_| {});
    let (mut other, ok) = bob(&srv);
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    let p = a.dir.path().join("notes.txt");
    std::fs::write(&p, "just text").unwrap();
    drop_file(&mut a.h, p);
    wait_label(&mut a.h, "Error: Not a PNG or JPEG image");
    std::thread::sleep(Duration::from_millis(300));
    other.app.poll();
    assert!(
        other.app.model.session(ok).unwrap().chat.is_empty(),
        "nothing was sent"
    );
}

#[test]
fn chat_image_click_opens_viewer() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    paste_image(&mut a.h, 40, 30);
    wait_label(&mut b.h, "View image from alice");
    assert!(!has(&b.h, "Image from alice"));
    click(&mut b.h, "View image from alice");
    wait_label(&mut b.h, "Image from alice");
    click_button(&mut b.h, "Close image");
    step_until(&mut b.h, "viewer closed", T, |h| {
        !has(h, "Image from alice")
    });
    assert!(b.h.state().model.ui.viewing.is_empty());
}

#[test]
fn paste_image_ignored_with_dialog_open() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    click_button(&mut a.h, "Add server");
    wait_label(&mut a.h, "Add a server");
    // Ctrl+V while the dialog is open (whatever has focus) must not post the image.
    a.h.event(egui::Event::PasteImage(Arc::new(egui::ColorImage::filled(
        [32, 32],
        egui::Color32::RED,
    ))));
    a.h.step();
    a.h.step();
    assert!(!a.h.state().model.ui.files_hovering);
    click_button(&mut a.h, "Cancel");
    assert!(!has(&a.h, "Add a server"));
    type_into(&mut a.h, "Message", "text after dialog");
    click_button(&mut a.h, "Send");
    wait_contains(&mut b.h, "alice: text after dialog");
    std::thread::sleep(Duration::from_millis(300));
    b.h.step();
    a.h.step();
    assert!(!has_contains(&b.h, "[image"), "{:?}", labels(&b.h));
    assert!(!has_contains(&a.h, "[image"), "{:?}", labels(&a.h));
}

// `paste_image_ignored_when_other_field_focused` is not written: the main window has
// no text field outside a dialog besides the chat input, so dialogs cover it. The
// focus check still guards the per-user volume sliders.

fn gui_sharing(servers: &[(&str, std::net::SocketAddr, &str, Option<&str>)]) -> Gui {
    gui_opts(FakeMode::Silent, servers, true)
}

/// Start sharing on `server`: the Share screen button where this platform can capture,
/// else the voice card's command directly (Linux has no capture backend, so the button
/// is disabled there, but the fake screen still exercises the watching side).
fn start_share(h: &mut H, server: &str) {
    if vc_client::capture::SUPPORTED {
        click_button(h, "Share screen");
    } else {
        let key = key_of(h, server);
        h.state_mut()
            .dispatch(vec![vc_client::app::UiCommand::ShareScreenOn(key)]);
        h.step();
    }
}

/// Stop the running share: the Stop sharing button where it is enabled, else the
/// voice card's command (which stops a running share).
fn stop_share(h: &mut H, server: &str) {
    if vc_client::capture::SUPPORTED {
        click_button(h, "Stop sharing");
    } else {
        let key = key_of(h, server);
        h.state_mut()
            .dispatch(vec![vc_client::app::UiCommand::ShareScreenOn(key)]);
        h.step();
    }
}

#[cfg(not(windows))]
#[test]
fn share_button_disabled_without_capture() {
    const { assert!(!vc_client::capture::SUPPORTED) };
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    connect(&mut a.h, "S");
    wait_label(&mut a.h, "Share screen");
    let b = a.h.get_by_role_and_label(Role::Button, "Share screen");
    assert!(
        b.accesskit_node().is_disabled(),
        "Share screen must be disabled without capture"
    );
    b.click();
    a.h.step();
    a.h.step();
    assert_eq!(a.h.state().capturing(), None);
    assert!(!has(&a.h, "You are sharing your screen"));
    // The hotkey does nothing either.
    a.h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    a.h.step();
    a.h.step();
    assert_eq!(a.h.state().capturing(), None);
}

/// Opt in to `nick`'s share from their user menu and wait for its tile on the stage.
fn watch(h: &mut H, nick: &str) {
    let row = format!("{nick} [sharing]");
    wait_label(h, &row);
    menu(h, &row, "Watch stream");
    wait_label(h, &format!("Screen: {nick}"));
}

#[test]
fn screenshare_fake_visible_to_peer() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    wait_label(&mut a.h, "You are sharing your screen");
    assert!(has(&a.h, "Stop sharing"));
    wait_contains(&mut b.h, "alice [sharing]");
    watch(&mut b.h, "alice");
    // The sharer's UI sees the watcher and switches its capture on.
    wait_label(&mut a.h, "1 watching");
    let c = a.h.state().capture_cfg();
    assert!(c.active);
    assert!(!c.video, "fake-screen tests never use the hardware encoder");
    let key = key_of(&b.h, "S");
    step_until(&mut b.h, "a frame from alice", T, |h| {
        !h.state().model.session(key).unwrap().screens.is_empty()
    });
    assert!(!has(&b.h, "Waiting for screen…"));
    let view =
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .screens
            .values()
            .next()
            .unwrap()
            .clone();
    assert_eq!(view.image.size, [320, 180], "fake source size");
    // Frames keep coming (the fake screen changes every grab).
    let first = view.generation;
    step_until(&mut b.h, "a newer frame", T, |h| {
        h.state()
            .model
            .session(key)
            .unwrap()
            .screens
            .values()
            .next()
            .map(|v| v.generation)
            > Some(first)
    });
    stop_share(&mut a.h, "S");
    step_until(&mut b.h, "share gone", T, |h| {
        !has(h, "Screen: alice") && !has_contains(h, "[sharing]")
    });
    assert!(b.h.state().model.session(key).unwrap().screens.is_empty());
    wait_label(&mut a.h, "Share screen");
    assert_eq!(a.h.state().capturing(), None);
}

#[test]
fn share_indicator_visible_on_other_server() {
    let s1 = TestServer::start(|_| {});
    let s2 = TestServer::start(|_| {});
    let mut a = gui_sharing(&[
        ("One", s1.addr(), "alice", None),
        ("Two", s2.addr(), "alice", None),
    ]);
    let mut b = gui_with(FakeMode::Silent, &[("One", s1.addr(), "bob", None)]);
    connect(&mut a.h, "One");
    connect(&mut a.h, "Two");
    connect(&mut b.h, "One");
    assert_eq!(a.h.state().model.selected, Some(key_of(&a.h, "One")));
    start_share(&mut a.h, "One");
    wait_label(&mut a.h, "You are sharing your screen");
    watch(&mut b.h, "alice");
    click(&mut a.h, "Two");
    step_until(&mut a.h, "server Two selected", T, |h| {
        h.state().model.selected == Some(key_of(h, "Two"))
    });
    // The me bar's LIVE badge and the voice card (on One) stay visible.
    wait_label(&mut a.h, "You are sharing your screen");
    assert!(has(&a.h, "Stop sharing"), "{:?}", labels(&a.h));
    assert_eq!(a.h.state().capturing(), Some(key_of(&a.h, "One")));
    stop_share(&mut a.h, "One");
    step_until(&mut b.h, "share gone on One", T, |h| {
        !has(h, "Screen: alice") && !has_contains(h, "[sharing]")
    });
    step_until(&mut a.h, "button back to Share screen", T, |h| {
        has(h, "Share screen") && h.state().capturing().is_none()
    });
    assert!(!has_contains(&a.h, "You are sharing your screen"));
    assert_eq!(a.h.state().model.screen_share, None, "not moved to Two");
}

#[test]
fn screenshare_stops_on_move() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    watch(&mut b.h, "alice");
    assert_eq!(a.h.state().capturing(), Some(key_of(&a.h, "S")));
    click_button(&mut a.h, "Join AFK");
    step_until(&mut b.h, "share cleared after move", T, |h| {
        !has(h, "Screen: alice") && !has_contains(h, "[sharing]")
    });
    step_until(&mut a.h, "button back to Share screen", T, |h| {
        has(h, "Share screen") && h.state().capturing().is_none()
    });
    assert!(!has(&a.h, "You are sharing your screen"));
}

#[test]
fn screenshare_not_shown_until_watched() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    wait_label(&mut a.h, "0 watching");
    wait_label(&mut b.h, "alice [sharing]");
    let key = key_of(&b.h, "S");
    // Opt-in: nothing is shown or received until bob clicks Watch.
    std::thread::sleep(Duration::from_millis(400));
    b.h.step();
    b.h.step();
    assert!(!has(&b.h, "Screen: alice"));
    assert!(!has(&b.h, "Streams"));
    assert!(b.h.state().model.session(key).unwrap().screens.is_empty());
    assert!(
        !a.h.state().capture_cfg().active,
        "no watchers: capture idles"
    );
    watch(&mut b.h, "alice");
    wait_label(&mut a.h, "1 watching");
    assert!(a.h.state().capture_cfg().active);
    step_until(&mut b.h, "a frame from alice", T, |h| {
        !h.state().model.session(key).unwrap().screens.is_empty()
    });
    assert_eq!(b.h.query_all_by_label("Stop watching alice").count(), 1);
    click_button_last(&mut b.h, "Stop watching alice");
    step_until(&mut b.h, "tile gone", T, |h| {
        !has(h, "Screen: alice") && !has(h, "Streams")
    });
    wait_label(&mut a.h, "0 watching");
    assert!(b.h.state().model.session(key).unwrap().screens.is_empty());
}

/// Click the last button with this label (the same label can be in the main
/// window and, drawn later and on top, the Streams window).
fn click_button_last(h: &mut H, label: &str) {
    h.query_all_by_role_and_label(Role::Button, label)
        .last()
        .unwrap_or_else(|| panic!("no button {label:?}; labels: {:?}", labels(h)))
        .click();
    h.step();
    h.step();
}

#[test]
fn streams_grid_two_sharers() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut c = gui_sharing(&[("S", srv.addr(), "carol", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut c.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    start_share(&mut c.h, "S");
    wait_label(&mut b.h, "alice [sharing]");
    wait_label(&mut b.h, "carol [sharing]");
    let key = key_of(&b.h, "S");
    let ids: Vec<_> =
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .sharers_here()
            .iter()
            .map(|u| u.id)
            .collect();
    // Watch both through the model.
    for user in ids {
        b.h.state_mut()
            .dispatch(vec![vc_client::app::UiCommand::WatchScreen {
                key,
                user,
                on: true,
            }]);
    }
    wait_label(&mut b.h, "Screen: alice");
    wait_label(&mut b.h, "Screen: carol");
    assert_eq!(vc_client::gui::grid_dims(2), (2, 1));
    wait_label(&mut a.h, "1 watching");
    wait_label(&mut c.h, "1 watching");
    step_until(&mut b.h, "frames from both", T, |h| {
        h.state().model.session(key).unwrap().screens.len() == 2
    });
    // Both on the main window's stage; pop them out and back.
    assert!(!has(&b.h, "Streams"));
    click_button_last(&mut b.h, "Pop out stream");
    wait_label(&mut b.h, "Streams");
    assert!(b.h.state().model.ui.streams_popped);
    click_button_last(&mut b.h, "Back to main window");
    step_until(&mut b.h, "back on the stage", T, |h| !has(h, "Streams"));
    assert!(has(&b.h, "Screen: alice") && has(&b.h, "Screen: carol"));
    assert_eq!(
        b.h.state().model.session(key).unwrap().watching.len(),
        2,
        "still watching"
    );
}

#[test]
fn two_images_open_at_once() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    paste_image(&mut a.h, 40, 30);
    wait_contains(&mut b.h, "alice: [image 40x30]");
    paste_image(&mut a.h, 50, 20);
    wait_contains(&mut b.h, "alice: [image 50x20]");
    let key = key_of(&b.h, "S");
    let ids: Vec<u64> =
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .images
            .keys()
            .copied()
            .collect();
    assert_eq!(ids.len(), 2);
    // Open both; their windows may overlap, so don't click through them.
    b.h.state_mut().dispatch(
        ids.iter()
            .map(|id| vc_client::app::UiCommand::ViewImage(*id))
            .collect(),
    );
    b.h.step();
    b.h.step();
    step_until(&mut b.h, "two image windows", T, |h| {
        h.query_all_by_label("Image from alice").count() >= 2
    });
    assert_eq!(b.h.state().model.ui.viewing.len(), 2);
    assert_eq!(b.h.query_all_by_label("Close image").count(), 2);
}

// ------------------------------------------------------------------ stream caps, test max, sound

#[test]
fn test_max_sets_combos_saves_and_can_be_overridden() {
    let mut g = gui_opts(FakeMode::Silent, &[], true);
    open_settings_tab(&mut g.h, "Screen share");
    click_button(&mut g.h, vc_client::gui::TEST_MAX);
    step_until(
        &mut g.h,
        "test max finished",
        Duration::from_secs(15),
        |h| !h.state().probing() && h.state().model.settings.screen_tested.is_some(),
    );
    let tested = g.h.state().model.settings.screen_tested.clone().unwrap();
    // The 180p fake screen only measures the lowest height.
    assert_eq!(tested.height, 480);
    wait_contains(&mut g.h, "Tested (local encode limit): 480p");
    let d = g.h.state().model.settings.clone();
    assert_eq!(
        (d.screen_fps, d.screen_height),
        (tested.fps, 480),
        "choices show it"
    );
    let s = Profile::new(g.dir.path()).load_settings().unwrap();
    assert_eq!((s.screen_fps, s.screen_height), (tested.fps, 480));
    assert_eq!(s.screen_tested, Some(tested));
    // Override: pick other values (saved as picked).
    click(&mut g.h, "2 fps");
    click(&mut g.h, "1080p");
    let s = Profile::new(g.dir.path()).load_settings().unwrap();
    assert_eq!((s.screen_fps, s.screen_height), (2, 1080));
}

#[test]
fn server_caps_mark_choices_and_show_limit() {
    let srv = TestServer::start(|c| {
        c.max_screen_fps = 30;
        c.max_screen_height = 720;
    });
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    open_settings_tab(&mut g.h, "Screen share");
    wait_label(&mut g.h, "Server limit: 720p, 30 fps");
    for l in [
        "60 fps (over server limit)",
        "30 fps",
        "1080p (over server limit)",
        "720p",
    ] {
        assert!(has(&g.h, l), "missing {l:?}: {:?}", labels(&g.h));
    }
    // Over-limit choices stay selectable (the capture clamps them).
    click(&mut g.h, "1080p (over server limit)");
    assert_eq!(g.h.state().model.settings.screen_height, 1080);
}

#[test]
fn stream_sound_toggle_persists_and_viewer_can_mute() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    wait_label(&mut a.h, "You are sharing your screen");
    open_settings_tab(&mut a.h, "Screen share");
    a.h.get_by_role_and_label(Role::CheckBox, vc_client::gui::SHARE_SOUND)
        .click();
    a.h.step();
    a.h.step();
    assert!(a.h.state().model.settings.screen_audio);
    assert!(
        Profile::new(a.dir.path())
            .load_settings()
            .unwrap()
            .screen_audio
    );
    close_settings(&mut a.h);
    assert!(a.h.state().model.settings.screen_audio, "kept after close");
    watch(&mut b.h, "alice");
    wait_label(&mut a.h, "1 watching");
    step_until(&mut a.h, "loopback running", T, |h| {
        h.state().loopback_running()
    });
    assert!(a.fake.loopback_running());
    // The viewer's per-stream mute.
    let key = key_of(&b.h, "S");
    wait_label(&mut b.h, "Mute alice's stream");
    click_button(&mut b.h, "Mute alice's stream");
    wait_label(&mut b.h, "Unmute alice's stream");
    let alice = b.h.state().model.session(key).unwrap().sharers_here()[0].id;
    assert!(b.h.state().model.stream_is_muted(key, alice));
    click_button(&mut b.h, "Unmute alice's stream");
    wait_label(&mut b.h, "Mute alice's stream");
    // Turning it off stops the loopback and is saved too.
    open_settings_tab(&mut a.h, "Screen share");
    a.h.get_by_role_and_label(Role::CheckBox, vc_client::gui::SHARE_SOUND)
        .click();
    a.h.step();
    a.h.step();
    step_until(&mut a.h, "loopback stopped", T, |h| {
        !h.state().loopback_running()
    });
    assert!(!a.fake.loopback_running());
    assert!(
        !Profile::new(a.dir.path())
            .load_settings()
            .unwrap()
            .screen_audio
    );
}

#[test]
fn mute_for_me_keeps_volume() {
    let srv = TestServer::start(|_| {});
    let (_b, _) = bob(&srv);
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    wait_label(&mut g.h, "bob");
    let k = key_of(&g.h, "S");
    menu(&mut g.h, "bob", "Mute for me");
    wait_label(&mut g.h, "Muted for you");
    assert!(g.h.state().model.user_is_muted(k, "bob"));
    let bob_id =
        g.h.state()
            .model
            .session(k)
            .unwrap()
            .users
            .values()
            .find(|u| u.nick == "bob")
            .unwrap()
            .id;
    assert_eq!(
        g.h.state()
            .ctl
            .lock()
            .unwrap()
            .gains
            .get(&(k, bob_id))
            .copied(),
        Some(0.0)
    );
    assert_eq!(g.h.state().model.settings.user_volume(k, "bob"), 1.0);
    menu(&mut g.h, "bob", "Mute for me");
    step_until(&mut g.h, "unmuted", T, |h| !has(h, "Muted for you"));
}

#[test]
fn shortcuts_mute_and_deafen_while_typing() {
    let srv = TestServer::start(|_| {});
    let mut g = gui_with(FakeMode::Silent, &[("S", srv.addr(), "alice", None)]);
    connect(&mut g.h, "S");
    g.h.get_by_role_and_label(Role::TextInput, "Message")
        .click();
    g.h.step();
    g.h.key_combination_modifiers(Modifiers::COMMAND, &[Key::M]);
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.voice.muted, "Ctrl+M works while typing");
    assert_eq!(g.h.state().model.ui.chat_input, "", "nothing typed");
    g.h.key_combination_modifiers(Modifiers::COMMAND, &[Key::D]);
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.voice.deafened);
    // No shortcut with a dialog open.
    click_button(&mut g.h, "Add server");
    wait_label(&mut g.h, "Add a server");
    g.h.key_combination_modifiers(Modifiers::COMMAND, &[Key::D]);
    g.h.step();
    g.h.step();
    assert!(g.h.state().model.voice.deafened, "unchanged");
}

/// alice shares in bob's channel; returns (alice, bob, server key in bob's gui).
fn alice_shares_to_bob(srv: &TestServer) -> (Gui, Gui, ServerKey) {
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    start_share(&mut a.h, "S");
    wait_label(&mut b.h, "alice [sharing]");
    let key = key_of(&b.h, "S");
    (a, b, key)
}

fn alice_id(h: &H, key: ServerKey) -> vc_proto::SessionId {
    h.state()
        .model
        .session(key)
        .unwrap()
        .users
        .values()
        .find(|u| u.nick == "alice")
        .unwrap()
        .id
}

fn stream_gain(h: &mut H, key: ServerKey, user: vc_proto::SessionId) -> Option<f32> {
    h.step();
    h.state()
        .ctl
        .lock()
        .unwrap()
        .gains
        .get(&(key, vc_client::app::stream_speaker(user)))
        .copied()
}

#[test]
fn live_badge_opens_stream_and_never_stops_it() {
    let srv = TestServer::start(|_| {});
    let (_a, mut b, key) = alice_shares_to_bob(&srv);
    let alice = alice_id(&b.h, key);
    // (a) The LIVE badge on alice's row watches her stream.
    wait_label(&mut b.h, "Watch alice's stream");
    click_button(&mut b.h, "Watch alice's stream");
    wait_label(&mut b.h, "Screen: alice");
    assert!(!has(&b.h, "Watch alice's stream"));
    assert!(
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .watching
            .contains(&alice)
    );
    // (b) Watching on the stage: a plain badge, nothing to click.
    for _ in 0..3 {
        b.h.step();
    }
    assert!(!has(&b.h, "Watch alice's stream"));
    assert!(!has(&b.h, "Show alice's stream"));
    assert_eq!(b.h.query_all_by_label("Screen: alice").count(), 1);
    assert!(
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .watching
            .contains(&alice)
    );
    // (c) Popped out: the badge brings the Streams window forward, still watching.
    click_button_last(&mut b.h, "Pop out stream");
    wait_label(&mut b.h, "Streams");
    wait_label(&mut b.h, "Show alice's stream");
    click_button(&mut b.h, "Show alice's stream");
    for _ in 0..3 {
        b.h.step();
    }
    assert_eq!(b.h.query_all_by_label("Screen: alice").count(), 1);
    assert!(
        b.h.state()
            .model
            .session(key)
            .unwrap()
            .watching
            .contains(&alice)
    );
    assert!(b.h.state().model.ui.streams_popped);
}

#[test]
fn live_badge_in_another_channel_is_not_clickable() {
    let srv = TestServer::start(|_| {});
    let mut a = gui_sharing(&[("S", srv.addr(), "alice", None)]);
    let mut b = gui_with(FakeMode::Silent, &[("S", srv.addr(), "bob", None)]);
    connect(&mut a.h, "S");
    connect(&mut b.h, "S");
    wait_label(&mut a.h, "Join AFK");
    click(&mut a.h, "Join AFK");
    step_until(&mut a.h, "alice in AFK", T, |h| !has(h, "Join AFK"));
    start_share(&mut a.h, "S");
    wait_label(&mut b.h, "alice [sharing]");
    for _ in 0..3 {
        b.h.step();
    }
    assert!(!has(&b.h, "Watch alice's stream"));
    assert!(!has(&b.h, "Show alice's stream"));
    assert!(has(&b.h, "live"));
}

#[test]
fn stream_volume_from_user_menu_and_tile() {
    let srv = TestServer::start(|_| {});
    let (_a, mut b, key) = alice_shares_to_bob(&srv);
    let alice = alice_id(&b.h, key);
    watch(&mut b.h, "alice");
    // (e) The user menu's stream volume slider.
    b.h.get_by_label("alice [sharing]").click_secondary();
    b.h.step();
    b.h.step();
    wait_label(&mut b.h, "Stream volume alice");
    b.h.get_by_role_and_label(Role::Slider, "Stream volume alice")
        .set_value_accesskit(150.0);
    b.h.step();
    b.h.step();
    wait_label(&mut b.h, "alice's stream: 150 %");
    assert_eq!(stream_gain(&mut b.h, key, alice), Some(1.5));
    assert_eq!(
        b.h.state().ctl.lock().unwrap().gains.get(&(key, alice)),
        None,
        "alice's voice is untouched"
    );
    assert!((b.h.state().model.settings.stream_volume(key, "alice") - 1.5).abs() < 1e-3);
    let on_disk = Profile::new(b.dir.path()).load_settings().unwrap();
    assert!((on_disk.stream_volume(key, "alice") - 1.5).abs() < 1e-3);
    assert!((on_disk.user_volume(key, "alice") - 1.0).abs() < 1e-3);
    b.h.key_press(Key::Escape);
    b.h.step();
    b.h.step();
    // (f) Right-click the tile's speaker button.
    step_until(&mut b.h, "menu closed", T, |h| {
        !has(h, "Stream volume alice")
    });
    b.h.get_by_role_and_label(Role::Button, "Mute alice's stream")
        .click_secondary();
    b.h.step();
    b.h.step();
    wait_label(&mut b.h, "Stream volume alice");
    b.h.get_by_role_and_label(Role::Slider, "Stream volume alice")
        .set_value_accesskit(40.0);
    b.h.step();
    b.h.step();
    assert_eq!(stream_gain(&mut b.h, key, alice), Some(0.4));
    assert!(
        !b.h.state().model.stream_is_muted(key, alice),
        "right-click does not mute"
    );
    b.h.key_press(Key::Escape);
    b.h.step();
    b.h.step();
    step_until(&mut b.h, "popup closed", T, |h| {
        !has(h, "Stream volume alice")
    });
    // (g) Mute wins; unmuting restores the volume.
    click_button(&mut b.h, "Mute alice's stream");
    wait_label(&mut b.h, "Unmute alice's stream");
    assert_eq!(stream_gain(&mut b.h, key, alice), Some(0.0));
    click_button(&mut b.h, "Unmute alice's stream");
    wait_label(&mut b.h, "Mute alice's stream");
    assert_eq!(stream_gain(&mut b.h, key, alice), Some(0.4));
    // (f, popped out) The same popup from the Streams window.
    click_button_last(&mut b.h, "Pop out stream");
    wait_label(&mut b.h, "Streams");
    b.h.query_all_by_role_and_label(Role::Button, "Mute alice's stream")
        .last()
        .unwrap()
        .click_secondary();
    b.h.step();
    b.h.step();
    wait_label(&mut b.h, "Stream volume alice");
    b.h.query_all_by_role_and_label(Role::Slider, "Stream volume alice")
        .last()
        .unwrap()
        .set_value_accesskit(70.0);
    b.h.step();
    b.h.step();
    assert_eq!(stream_gain(&mut b.h, key, alice), Some(0.7));
}
