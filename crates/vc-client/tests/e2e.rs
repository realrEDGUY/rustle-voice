//! In-process server(s) + headless clients on fake audio devices.

mod common;

use common::*;
use vc_client::app::UiCommand;
use vc_client::audio::{FAKE_LOOPBACK_HZ, FakeMode};
use vc_client::profile::InputMode;
use vc_client::session::ConnStatus;
use vc_proto::ChannelId;

const AFK: ChannelId = ChannelId(2);

fn heard(c: &Client) -> f32 {
    let p = c.fake.played();
    let tail = &p[p.len().saturating_sub(48_000 / 2)..];
    rms_db(tail)
}

#[test]
fn sine_from_a_heard_by_b_same_channel() {
    let srv = TestServer::start(|_| {});
    let mut a = client_with(FakeMode::Sine(440.0));
    let mut b = client_with(FakeMode::Silent);
    let ka = a.add("S", srv.addr(), "alice");
    let kb = b.add("S", srv.addr(), "bob");
    a.connect(ka);
    b.connect(kb);
    a.mode(InputMode::Open);
    b.wait(
        |x| x.model.session(kb).unwrap().users.len() == 2,
        "both users",
    );
    b.fake.clear_played();
    run_audio(&mut [&mut a, &mut b], 60);
    let p = b.fake.played();
    let db = heard(&b);
    assert!(db > -30.0, "B playback RMS {db:.1} dB");
    let (n, hz) = loud_frames_hz(&p);
    assert!(n >= 10, "only {n} loud frames");
    assert!((hz - 440.0).abs() <= 15.0, "frequency {hz:.1} Hz");
    // B's UI saw alice speaking.
    let alice = b
        .app
        .model
        .session(kb)
        .unwrap()
        .users
        .values()
        .find(|u| u.nick == "alice")
        .unwrap()
        .id;
    assert!(
        b.app
            .model
            .session(kb)
            .unwrap()
            .last_voice
            .contains_key(&alice),
        "speaking noted"
    );
}

#[test]
fn not_heard_in_other_channel() {
    let srv = TestServer::start(|_| {});
    let mut a = client_with(FakeMode::Sine(440.0));
    let mut b = client_with(FakeMode::Silent);
    let mut c = client_with(FakeMode::Silent);
    let ka = a.add("S", srv.addr(), "alice");
    let kb = b.add("S", srv.addr(), "bob");
    let kc = c.add("S", srv.addr(), "carol");
    a.connect(ka);
    b.connect(kb);
    c.connect(kc);
    a.mode(InputMode::Open);
    b.app
        .dispatch(vec![UiCommand::Select(kb), UiCommand::Join(AFK)]);
    b.wait(
        |x| x.model.session(kb).unwrap().my_channel() == Some(AFK),
        "bob in AFK",
    );
    c.wait(
        |x| x.model.session(kc).unwrap().users.len() == 3,
        "carol sees all",
    );
    b.fake.clear_played();
    c.fake.clear_played();
    run_audio(&mut [&mut a, &mut b, &mut c], 50);
    assert!(
        heard(&c) > -30.0,
        "positive control: carol (same channel) hears"
    );
    assert!(heard(&b) < -90.0, "bob in AFK hears nothing: {}", heard(&b));
}

#[test]
fn deafened_hears_nothing() {
    let srv = TestServer::start(|_| {});
    let mut a = client_with(FakeMode::Sine(440.0));
    let mut b = client_with(FakeMode::Silent);
    let mut c = client_with(FakeMode::Silent);
    let ka = a.add("S", srv.addr(), "alice");
    let kb = b.add("S", srv.addr(), "bob");
    let kc = c.add("S", srv.addr(), "carol");
    a.connect(ka);
    b.connect(kb);
    c.connect(kc);
    a.mode(InputMode::Open);
    b.app.dispatch(vec![UiCommand::ToggleDeafen]);
    a.wait(
        |x| {
            x.model
                .session(ka)
                .unwrap()
                .users
                .values()
                .any(|u| u.nick == "bob" && u.deafened && u.muted)
        },
        "alice sees bob deafened",
    );
    b.fake.clear_played();
    c.fake.clear_played();
    run_audio(&mut [&mut a, &mut b, &mut c], 50);
    assert!(heard(&c) > -30.0, "positive control: carol hears");
    assert!(heard(&b) < -90.0, "deafened bob hears nothing");
    // Undeafen: bob hears again.
    b.app.dispatch(vec![UiCommand::ToggleDeafen]);
    b.fake.clear_played();
    run_audio(&mut [&mut a, &mut b, &mut c], 50);
    assert!(heard(&b) > -30.0, "bob hears after undeafen");
}

#[test]
fn multiserver_connect_two_servers_mixed_playback() {
    let s1 = TestServer::start(|c| c.name = "One".into());
    let s2 = TestServer::start(|c| c.name = "Two".into());
    let mut t1 = client_with(FakeMode::Sine(440.0));
    let mut t2 = client_with(FakeMode::Sine(880.0));
    let mut l = client_with(FakeMode::Silent);
    let k1 = t1.add("One", s1.addr(), "t1");
    let k2 = t2.add("Two", s2.addr(), "t2");
    let l1 = l.add("One", s1.addr(), "listener");
    let l2 = l.add("Two", s2.addr(), "listener");
    t1.connect(k1);
    t2.connect(k2);
    l.connect(l1);
    l.connect(l2);
    assert_eq!(l.app.model.status(l1), ConnStatus::Connected);
    assert_eq!(l.app.model.status(l2), ConnStatus::Connected);
    assert_eq!(l.app.model.session(l1).unwrap().server_name, "One");
    assert_eq!(l.app.model.session(l2).unwrap().server_name, "Two");
    t1.mode(InputMode::Open);
    t2.mode(InputMode::Open);
    l.fake.clear_played();
    run_audio(&mut [&mut t1, &mut t2, &mut l], 60);
    let p = l.fake.played();
    let tail = &p[p.len() - 24_000..];
    let p440 = tone_power(tail, 440.0);
    let p880 = tone_power(tail, 880.0);
    let p660 = tone_power(tail, 660.0);
    assert!(p440 > 50.0 * p660, "440 Hz present: {p440} vs {p660}");
    assert!(p880 > 50.0 * p660, "880 Hz present: {p880} vs {p660}");
}

#[test]
fn mic_goes_only_to_active_voice_server() {
    let s1 = TestServer::start(|_| {});
    let s2 = TestServer::start(|_| {});
    let mut t = client_with(FakeMode::Sine(440.0));
    let mut l1 = client_with(FakeMode::Silent);
    let mut l2 = client_with(FakeMode::Silent);
    let t1 = t.add("One", s1.addr(), "talker");
    let t2 = t.add("Two", s2.addr(), "talker");
    let a = l1.add("One", s1.addr(), "l1");
    let b = l2.add("Two", s2.addr(), "l2");
    l1.connect(a);
    l2.connect(b);
    t.connect(t1);
    t.connect(t2);
    assert_eq!(
        t.app.model.talk_target,
        Some(t1),
        "first connected is the target"
    );
    t.mode(InputMode::Open);
    l1.fake.clear_played();
    l2.fake.clear_played();
    run_audio(&mut [&mut t, &mut l1, &mut l2], 50);
    assert!(heard(&l1) > -30.0, "server One listener hears");
    assert!(heard(&l2) < -90.0, "server Two listener hears nothing");
    t.app.dispatch(vec![UiCommand::TalkHere(t2)]);
    l1.fake.clear_played();
    l2.fake.clear_played();
    run_audio(&mut [&mut t, &mut l1, &mut l2], 50);
    assert!(heard(&l2) > -30.0, "after Talk here, server Two hears");
    assert!(heard(&l1) < -90.0, "and server One no longer does");
}

#[test]
fn reconnect_after_server_restart() {
    let mut srv = TestServer::start(|_| {});
    let mut a = client_with(FakeMode::Silent);
    let k = a.add("S", srv.addr(), "alice");
    a.connect(k);
    let fp = a.app.model.saved_server(k).unwrap().fingerprint.clone();
    assert!(fp.is_some());
    srv.restart(); // same port, same cert dir
    let start = std::time::Instant::now();
    a.wait(
        |x| {
            matches!(
                x.model.status(k),
                ConnStatus::Reconnecting { .. } | ConnStatus::Connecting
            )
        },
        "noticed the restart",
    );
    a.wait(
        |x| x.model.status(k) == ConnStatus::Connected,
        "reconnected",
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(a.app.model.saved_server(k).unwrap().fingerprint, fp);
    assert!(srv.snapshot().users.iter().any(|u| u.nick == "alice"));
    assert!(a.app.model.ui.tofu_prompt.is_none());
}

#[test]
fn auto_reconnect_stops_on_bad_password() {
    let srv = TestServer::start(|c| c.password = Some("right".into()));
    let mut a = client_with(FakeMode::Silent);
    let k = vc_client_add_pw(&mut a, srv.addr(), "wrong");
    a.app.dispatch(vec![UiCommand::Connect(k)]);
    a.wait(
        |x| matches!(x.model.status(k), ConnStatus::Failed(_)),
        "failed",
    );
    assert_eq!(
        a.app.model.status(k),
        ConnStatus::Failed("Wrong server password".into())
    );
    // Stays failed: no retries.
    std::thread::sleep(std::time::Duration::from_millis(800));
    a.app.poll();
    assert!(matches!(a.app.model.status(k), ConnStatus::Failed(_)));
    assert!(srv.snapshot().users.is_empty());
    // Positive control: right password connects.
    let mut b = client_with(FakeMode::Silent);
    let kb = vc_client_add_pw(&mut b, srv.addr(), "right");
    b.connect(kb);
}

fn vc_client_add_pw(c: &mut Client, addr: std::net::SocketAddr, pw: &str) -> vc_client::ServerKey {
    add_server(&mut c.app, "S", addr, "alice", Some(pw))
}

#[test]
fn production_audio_thread_sine_heard() {
    use vc_client::audio::FakeBackend;
    use vc_client::gui::VoiceApp;
    use vc_client::profile::Profile;

    let srv = TestServer::start(|_| {});
    let dir = tempfile::tempdir().unwrap();
    // Production wiring: VoiceApp::new = audio clock thread on, real-time fake devices.
    let mut talker = VoiceApp::new(
        egui::Context::default(),
        Box::new(FakeBackend::new(FakeMode::Sine(440.0)).realtime()),
        Profile::new(dir.path()),
        vec![],
    );
    let tk = add_server(&mut talker, "S", srv.addr(), "alice", None);
    talker.dispatch(vec![
        UiCommand::SetInputMode(InputMode::Open),
        UiCommand::Connect(tk),
    ]);
    wait_app(
        &mut talker,
        |a| a.model.status(tk) == ConnStatus::Connected,
        T,
        "talker connected",
    );
    let mut l = client_with(FakeMode::Silent);
    let lk = l.add("S", srv.addr(), "bob");
    l.connect(lk);
    l.fake.clear_played();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        let start = std::time::Instant::now();
        // What `VoiceApp::frame` does each frame: poll events, refresh the pipeline control.
        talker.poll();
        talker.dispatch(vec![]);
        l.audio_step();
        if l.fake.played().len() >= 48_000 / 2 && heard(&l) > -30.0 {
            ok = true;
            break;
        }
        let el = start.elapsed();
        if el < std::time::Duration::from_millis(20) {
            std::thread::sleep(std::time::Duration::from_millis(20) - el);
        }
    }
    assert!(ok, "listener RMS {:.1} dB after 3 s", heard(&l));
    let p = l.fake.played();
    let tail = &p[p.len() - 48_000 / 2..];
    let (n, hz) = loud_frames_hz(tail);
    assert!(n >= 10, "only {n} loud frames");
    assert!((hz - 440.0).abs() <= 15.0, "frequency {hz:.1} Hz");
}

/// Played-back power of the sharer's fake system audio vs its mic sine, in the
/// last half second.
fn stream_and_mic(c: &Client) -> (f32, f32, f32) {
    let p = c.fake.played();
    let tail = &p[p.len().saturating_sub(24_000)..];
    (
        tone_power(tail, FAKE_LOOPBACK_HZ),
        tone_power(tail, 440.0),
        tone_power(tail, 660.0),
    )
}

#[test]
fn stream_sound_reaches_watchers_only_and_mutes_locally() {
    let srv = TestServer::start(|_| {});
    let mut a = client_with(FakeMode::Sine(440.0));
    let mut b = client_with(FakeMode::Silent);
    let mut c = client_with(FakeMode::Silent);
    let ka = a.add("S", srv.addr(), "alice");
    let kb = b.add("S", srv.addr(), "bob");
    let kc = c.add("S", srv.addr(), "carol");
    a.connect(ka);
    b.connect(kb);
    c.connect(kc);
    a.mode(InputMode::Open);
    a.app.dispatch(vec![
        UiCommand::Select(ka),
        UiCommand::ToggleScreenShare,
        UiCommand::SetStreamSound { on: true },
    ]);
    b.wait(
        |x| x.model.session(kb).unwrap().sharers_here().len() == 1,
        "bob sees alice sharing",
    );
    let alice = b.app.model.session(kb).unwrap().sharers_here()[0].id;
    b.app.dispatch(vec![UiCommand::WatchScreen {
        key: kb,
        user: alice,
        on: true,
    }]);
    a.wait(
        |x| x.loopback_running(),
        "alice's loopback starts once watched",
    );
    assert!(a.fake.loopback_running());
    b.fake.clear_played();
    c.fake.clear_played();
    run_audio(&mut [&mut a, &mut b, &mut c], 60);
    let (s, mic, off) = stream_and_mic(&b);
    assert!(s > 50.0 * off, "watcher hears stream sound: {s} vs {off}");
    assert!(
        mic > 50.0 * off,
        "watcher still hears the mic: {mic} vs {off}"
    );
    let (s, mic, off) = stream_and_mic(&c);
    assert!(mic > 50.0 * off, "positive control: carol hears the mic");
    assert!(
        s < 2.0 * off,
        "non-watcher hears no stream sound: {s} vs {off}"
    );
    // Bob mutes alice's stream: the mic stays, the stream goes.
    b.app.dispatch(vec![UiCommand::MuteStream {
        key: kb,
        user: alice,
        muted: true,
    }]);
    run_audio(&mut [&mut a, &mut b, &mut c], 10);
    b.fake.clear_played();
    run_audio(&mut [&mut a, &mut b, &mut c], 40);
    let (s, mic, off) = stream_and_mic(&b);
    assert!(mic > 50.0 * off, "mic still heard after muting the stream");
    assert!(s < 2.0 * off, "muted stream is silent: {s} vs {off}");
    // Alice turns stream sound off: the loopback stops.
    a.app
        .dispatch(vec![UiCommand::SetStreamSound { on: false }]);
    assert!(!a.app.loopback_running() && !a.fake.loopback_running());
}

/// Steps from A's first transmitted frame until B's playback output turns loud,
/// on a server running `preset`. Also returns B's stored jitter depth.
fn steps_to_audible(preset: vc_server::VoicePreset) -> (usize, u8) {
    let srv = TestServer::start(|c| c.voice_preset = preset);
    let mut a = client_with(FakeMode::Sine(440.0));
    let mut b = client_with(FakeMode::Silent);
    let ka = a.add("S", srv.addr(), "alice");
    let kb = b.add("S", srv.addr(), "bob");
    a.connect(ka);
    b.connect(kb);
    b.wait(
        |x| x.model.session(kb).unwrap().users.len() == 2,
        "both users",
    );
    let depth = b.app.model.session(kb).unwrap().voice_tuning.jitter_frames;
    a.mode(InputMode::Open);
    for step in 1..=40 {
        let start = std::time::Instant::now();
        b.fake.clear_played();
        a.audio_step();
        b.audio_step();
        if rms_db(&b.fake.played()) > -30.0 {
            return (step, depth);
        }
        let el = start.elapsed();
        if el < std::time::Duration::from_millis(20) {
            std::thread::sleep(std::time::Duration::from_millis(20) - el);
        }
    }
    panic!("B never heard A on {preset:?}");
}

#[test]
fn voice_preset_changes_steps_to_audible() {
    let (competitive, depth) = steps_to_audible(vc_server::VoicePreset::Competitive);
    assert_eq!(depth, 2, "B stores the competitive jitter depth");
    let (smooth, depth) = steps_to_audible(vc_server::VoicePreset::Smooth);
    assert_eq!(depth, 6);
    assert!(
        smooth >= competitive + 3,
        "smooth {smooth} steps vs competitive {competitive}"
    );
}
