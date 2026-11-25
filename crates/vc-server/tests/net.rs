//! Real quinn over 127.0.0.1:0.

mod common;

use std::time::Duration;

use common::raw_client::{self, Opts, join};
use common::{BlackholeRelay, test_config};
use quinn::ConnectionError;
use vc_proto::{ChannelId, ClientBlob, ClientMsg, ErrorCode, ServerBlob, ServerMsg};
use vc_server::{DisconnectReason, ServerConfig, net};

const T: Duration = Duration::from_secs(5);
const SHORT: Duration = Duration::from_millis(400);

async fn start(cfg: impl FnOnce(&mut ServerConfig)) -> (net::ServerHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut c = test_config(dir.path());
    cfg(&mut c);
    (net::run(c).await.unwrap(), dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn connect_handshake_over_quic() {
    let (srv, _d) = start(|_| {}).await;
    assert_eq!(srv.addr.ip().to_string(), "127.0.0.1");
    let mut c = raw_client::connect(srv.addr, "alice", None).await.unwrap();
    match c.recv_timeout(T).await {
        Some(ServerMsg::Welcome {
            channels, users, ..
        }) => {
            assert_eq!(channels.len(), 2);
            assert_eq!(users.len(), 1);
            assert_eq!(users[0].nick, "alice");
        }
        other => panic!("{other:?}"),
    }
    c.send(&ClientMsg::Ping { nonce: 5 }).await.unwrap();
    assert_eq!(c.recv_timeout(T).await, Some(ServerMsg::Pong { nonce: 5 }));
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn alpn_mismatch_rejected() {
    let (srv, _d) = start(|_| {}).await;
    let bad = raw_client::open(
        srv.addr,
        Opts {
            alpn: b"nope/9".to_vec(),
            ..Opts::default()
        },
    )
    .await;
    assert!(bad.is_err(), "connection with wrong ALPN must fail");
    // Positive control: correct ALPN works against the same server.
    let _ok = join(srv.addr, "ok").await;
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_see_each_other() {
    let (srv, _d) = start(|_| {}).await;
    let mut a = join(srv.addr, "a").await;
    let mut b = raw_client::connect(srv.addr, "b", None).await.unwrap();
    match b.recv_timeout(T).await {
        Some(ServerMsg::Welcome { users, .. }) => {
            let nicks: Vec<_> = users.iter().map(|u| u.nick.clone()).collect();
            assert_eq!(nicks, ["a", "b"]);
        }
        other => panic!("{other:?}"),
    }
    let got = a
        .recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await;
    assert!(matches!(got, Some(ServerMsg::UserJoined(u)) if u.nick == "b"));
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn voice_datagram_relayed_between_clients() {
    let (srv, _d) = start(|_| {}).await;
    let a = join(srv.addr, "a").await;
    let b = join(srv.addr, "b").await;
    common::sleep_ms(100).await;
    a.send_voice(77);
    let got = b.recv_voice_timeout(T).await;
    assert_eq!(got, Some((a.id.unwrap(), 77)));
    // Not echoed back to the speaker.
    assert_eq!(a.recv_voice_timeout(SHORT).await, None);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_who_never_speaks_hears() {
    let (srv, _d) = start(|_| {}).await;
    // The listener joins first and never sends a single datagram.
    let listener = join(srv.addr, "listener").await;
    let talker = join(srv.addr, "talker").await;
    common::sleep_ms(100).await;
    for seq in 0..3 {
        talker.send_voice(seq);
    }
    let got = listener.recv_voice_timeout(T).await;
    assert_eq!(got.map(|g| g.0), talker.id);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_over_quic_channel_scoped() {
    let (srv, _d) = start(|_| {}).await;
    let mut a = join(srv.addr, "a").await;
    let mut b = join(srv.addr, "b").await;
    let mut c = join(srv.addr, "c").await;
    c.send(&ClientMsg::Join {
        channel: ChannelId(2),
    })
    .await
    .unwrap();
    // Wait until a sees c move, so ordering is settled.
    a.recv_until(T, |m| matches!(m, ServerMsg::UserMoved { .. }))
        .await
        .unwrap();
    a.send(&ClientMsg::Chat {
        text: "hello lobby".into(),
    })
    .await
    .unwrap();
    let got = b
        .recv_until(T, |m| matches!(m, ServerMsg::Chat { .. }))
        .await;
    assert!(matches!(got, Some(ServerMsg::Chat { text, .. }) if text == "hello lobby"));
    let none = c
        .recv_until(SHORT, |m| matches!(m, ServerMsg::Chat { .. }))
        .await;
    assert_eq!(none, None);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_disconnects_dead_client() {
    let (srv, _d) = start(|c| {
        c.idle_timeout_ms = 500;
        c.keep_alive_ms = 100;
    })
    .await;
    let mut watcher = join(srv.addr, "watcher").await;
    let relay = BlackholeRelay::start(srv.addr).await;
    let dead = join(relay.addr, "dead").await;
    let dead_id = dead.id.unwrap();
    watcher
        .recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await
        .unwrap();
    relay.stop();
    let start = std::time::Instant::now();
    let left = watcher
        .recv_until(
            Duration::from_secs(3),
            |m| matches!(m, ServerMsg::UserLeft { user } if *user == dead_id),
        )
        .await;
    assert!(left.is_some(), "UserLeft within 3 s");
    assert!(start.elapsed() < Duration::from_secs(3));
    let reasons = srv.disconnects();
    assert!(
        reasons
            .iter()
            .any(|(s, r)| *s == dead_id && *r == DisconnectReason::Timeout),
        "server-side reason must be a timeout: {reasons:?}"
    );
    // The live watcher (keepalives flowing) is still connected.
    watcher.send(&ClientMsg::Ping { nonce: 1 }).await.unwrap();
    assert_eq!(
        watcher
            .recv_until(T, |m| matches!(m, ServerMsg::Pong { .. }))
            .await,
        Some(ServerMsg::Pong { nonce: 1 })
    );
    drop(dead);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_consumer_disconnected() {
    let (srv, _d) = start(|c| c.control_queue = 4).await;
    let mut flooder = join(srv.addr, "flooder").await;
    let mut slow = raw_client::connect_opts(
        srv.addr,
        "slow",
        None,
        Opts {
            read: false,
            small_window: true,
            ..Opts::default()
        },
    )
    .await
    .unwrap();
    flooder
        .recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await
        .unwrap();
    let text = "x".repeat(450);
    let mut left = None;
    for _ in 0..400 {
        flooder
            .send(&ClientMsg::Chat { text: text.clone() })
            .await
            .unwrap();
        if let Some(m) = flooder
            .recv_until(Duration::from_millis(5), |m| {
                matches!(m, ServerMsg::UserLeft { .. })
            })
            .await
        {
            left = Some(m);
            break;
        }
    }
    let left = match left {
        Some(l) => Some(l),
        None => {
            flooder
                .recv_until(T, |m| matches!(m, ServerMsg::UserLeft { .. }))
                .await
        }
    };
    assert!(left.is_some(), "slow consumer must be kicked");
    let reason = tokio::time::timeout(T, slow.conn.closed()).await.unwrap();
    match reason {
        ConnectionError::ApplicationClosed(c) => {
            assert_eq!(
                c.error_code.into_inner(),
                ErrorCode::SlowConsumer.as_u32() as u64
            )
        }
        other => panic!("unexpected close: {other:?}"),
    }
    // The flooder (who reads) is unaffected.
    flooder.send(&ClientMsg::Ping { nonce: 3 }).await.unwrap();
    assert!(
        flooder
            .recv_until(T, |m| matches!(m, ServerMsg::Pong { nonce: 3 }))
            .await
            .is_some()
    );
    let _ = slow.send(&ClientMsg::Ping { nonce: 0 }).await;
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_over_quic() {
    let (srv, _d) = start(|c| c.password = Some("pw".into())).await;
    let mut bad = raw_client::connect(srv.addr, "bad", Some("nope"))
        .await
        .unwrap();
    let m = bad.recv_timeout(T).await;
    assert!(
        matches!(
            m,
            Some(ServerMsg::Error {
                code: ErrorCode::BadPassword,
                ..
            })
        ),
        "{m:?}"
    );
    let reason = tokio::time::timeout(T, bad.conn.closed()).await.unwrap();
    assert!(
        matches!(&reason, ConnectionError::ApplicationClosed(c)
            if c.error_code.into_inner() == ErrorCode::BadPassword.as_u32() as u64),
        "{reason:?}"
    );
    let mut good = raw_client::connect(srv.addr, "good", Some("pw"))
        .await
        .unwrap();
    assert!(matches!(
        good.recv_timeout(T).await,
        Some(ServerMsg::Welcome { .. })
    ));
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_closes_clients() {
    let (srv, _d) = start(|_| {}).await;
    let addr = srv.addr;
    let a = join(addr, "a").await;
    srv.shutdown().await;
    let reason = tokio::time::timeout(T, a.conn.closed()).await.unwrap();
    assert!(
        matches!(reason, ConnectionError::ApplicationClosed(_)),
        "{reason:?}"
    );
    // The port can be bound again.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.bind = addr;
    let again = net::run(cfg).await.expect("rebind same port");
    let _c = join(again.addr, "b").await;
    again.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_frame_closes_with_bad_version_code() {
    let (srv, _d) = start(|_| {}).await;
    let mut good = join(srv.addr, "good").await;
    let mut bad = join(srv.addr, "bad").await;
    let bad_id = bad.id.unwrap();
    good.recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await
        .unwrap();
    // Valid length prefix, invalid JSON body.
    let body = b"{this is not json";
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(body);
    bad.send_raw(&frame).await.unwrap();
    let reason = tokio::time::timeout(T, bad.conn.closed()).await.unwrap();
    match &reason {
        ConnectionError::ApplicationClosed(c) => assert_eq!(
            c.error_code.into_inner(),
            ErrorCode::BadVersion.as_u32() as u64
        ),
        other => panic!("unexpected close: {other:?}"),
    }
    // The user is gone from the server.
    let left = good
        .recv_until(
            T,
            |m| matches!(m, ServerMsg::UserLeft { user } if *user == bad_id),
        )
        .await;
    assert!(left.is_some(), "others see the bad client leave");
    let snap = srv.snapshot().await.unwrap();
    assert!(snap.users.iter().all(|u| u.id != bad_id), "{snap:?}");
    // Positive control: the well-behaved client is still connected.
    good.send(&ClientMsg::Ping { nonce: 9 }).await.unwrap();
    assert_eq!(
        good.recv_until(T, |m| matches!(m, ServerMsg::Pong { .. }))
            .await,
        Some(ServerMsg::Pong { nonce: 9 })
    );
    assert!(snap.users.iter().any(|u| u.nick == "good"));
    srv.shutdown().await;
}

// ---------- blob streams (images and screen frames) ----------

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// A body the server accepts: the right magic bytes, then `len` pseudo-random bytes.
fn fake_image(magic: &[u8], len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.max(1);
    let mut v = magic.to_vec();
    v.extend((0..len).map(|_| {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x as u8
    }));
    v
}

async fn pong(c: &mut raw_client::RawClient, nonce: u64) {
    c.send(&ClientMsg::Ping { nonce }).await.unwrap();
    assert_eq!(
        c.recv_until(T, |m| matches!(m, ServerMsg::Pong { .. }))
            .await,
        Some(ServerMsg::Pong { nonce })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn uni_stream_refused_before_welcome() {
    let (srv, _d) = start(|c| c.hello_timeout_ms = 5000).await;
    let mut c = raw_client::open(srv.addr, Opts::default()).await.unwrap();
    // No Hello yet: the server grants zero uni streams, so opening one cannot complete.
    assert!(
        tokio::time::timeout(SHORT, c.conn.open_uni())
            .await
            .is_err(),
        "uni stream must not open before Welcome"
    );
    c.send(&ClientMsg::Hello {
        version: vc_proto::PROTOCOL_VERSION,
        nick: "late".into(),
        password: None,
        h264: false,
    })
    .await
    .unwrap();
    assert!(matches!(
        c.recv_timeout(T).await,
        Some(ServerMsg::Welcome { max_image_bytes, voice_tuning, .. })
            if max_image_bytes == 4 * 1024 * 1024 && voice_tuning == vc_proto::VoiceTuning::default()
    ));
    let s = tokio::time::timeout(T, c.conn.open_uni()).await;
    assert!(matches!(s, Ok(Ok(_))), "uni stream opens after Welcome");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn image_over_quic_channel_scoped() {
    let (srv, _d) = start(|_| {}).await;
    let a = join(srv.addr, "a").await;
    let mut b = join(srv.addr, "b").await;
    let mut c = join(srv.addr, "c").await;
    c.send(&ClientMsg::Join {
        channel: ChannelId(2),
    })
    .await
    .unwrap();
    b.recv_until(T, |m| matches!(m, ServerMsg::UserMoved { .. }))
        .await
        .unwrap();
    let img = fake_image(&PNG_MAGIC, 300 * 1024, 7);
    a.send_blob(&ClientBlob::Image, &img).await.unwrap();
    for who in [&a, &b] {
        let (header, body) = who.recv_blob_timeout(T).await.expect("image arrives");
        match header {
            ServerBlob::Image { from, channel, .. } => {
                assert_eq!(Some(from), a.id);
                assert_eq!(channel, ChannelId(1));
            }
            other => panic!("{other:?}"),
        }
        assert!(body == img, "byte-identical");
    }
    assert!(c.recv_blob_timeout(SHORT).await.is_none(), "other channel");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversize_image_rejected_connection_survives() {
    let (srv, _d) = start(|c| c.max_image_bytes = 100_000).await;
    let mut a = join(srv.addr, "a").await;
    let b = join(srv.addr, "b").await;
    a.send_blob(&ClientBlob::Image, &fake_image(&PNG_MAGIC, 200_000, 3))
        .await
        .unwrap();
    let e = a
        .recv_until(T, |m| matches!(m, ServerMsg::Error { .. }))
        .await;
    assert!(
        matches!(
            e,
            Some(ServerMsg::Error {
                code: ErrorCode::ImageTooLarge,
                ..
            })
        ),
        "{e:?}"
    );
    assert!(b.recv_blob_timeout(SHORT).await.is_none(), "not forwarded");
    pong(&mut a, 11).await;
    // A small image still goes through on the same connection.
    a.send_blob(&ClientBlob::Image, &fake_image(&JPEG_MAGIC, 1000, 4))
        .await
        .unwrap();
    assert!(b.recv_blob_timeout(T).await.is_some());
    assert!(srv.disconnects().is_empty());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_blob_header_drops_stream_only() {
    let (srv, _d) = start(|_| {}).await;
    let mut a = join(srv.addr, "a").await;
    let b = join(srv.addr, "b").await;
    // Header length past the limit, then valid length with garbage JSON, then an
    // unknown blob kind, then a non-image body.
    a.send_blob_raw(&[0xFF, 0xFF, 0, 0, 1, 2, 3]).await.unwrap();
    let mut junk = 5u32.to_le_bytes().to_vec();
    junk.extend_from_slice(b"{nope");
    a.send_blob_raw(&junk).await.unwrap();
    let kind = br#"{"t":"video"}"#;
    let mut unknown = (kind.len() as u32).to_le_bytes().to_vec();
    unknown.extend_from_slice(kind);
    unknown.extend_from_slice(&PNG_MAGIC);
    a.send_blob_raw(&unknown).await.unwrap();
    a.send_blob(&ClientBlob::Image, b"GIF89a not ours")
        .await
        .unwrap();
    pong(&mut a, 12).await;
    assert!(b.recv_blob_timeout(SHORT).await.is_none());
    // The connection is fine: a real image is forwarded.
    a.send_blob(&ClientBlob::Image, &fake_image(&PNG_MAGIC, 64, 1))
        .await
        .unwrap();
    assert!(b.recv_blob_timeout(T).await.is_some());
    assert!(srv.disconnects().is_empty());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_not_blocked_by_stalled_image() {
    let (srv, _d) = start(|_| {}).await;
    let mut a = join(srv.addr, "a").await;
    // B reads its control stream but never accepts uni streams, with a 16 KiB
    // window per stream: an image to B stalls after 16 KiB.
    let mut b = raw_client::connect_opts(
        srv.addr,
        "b",
        None,
        Opts {
            stream_window: Some(16 * 1024),
            ..Opts::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        b.recv_timeout(T).await,
        Some(ServerMsg::Welcome { .. })
    ));
    a.recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await
        .unwrap();
    let img = fake_image(&PNG_MAGIC, 3 * 1024 * 1024, 9);
    a.send_blob(&ClientBlob::Image, &img).await.unwrap();
    // A's own echo proves the server accepted and fanned out the image.
    assert!(a.recv_blob_timeout(T).await.is_some());
    a.send(&ClientMsg::Chat {
        text: "after image".into(),
    })
    .await
    .unwrap();
    let got = b
        .recv_until(Duration::from_secs(2), |m| {
            matches!(m, ServerMsg::Chat { .. })
        })
        .await;
    assert!(
        matches!(got, Some(ServerMsg::Chat { ref text, .. }) if text == "after image"),
        "chat must not wait behind the image: {got:?}"
    );
    // The image is still there, unread, and arrives whole once B reads it.
    let (_, body) = b.recv_blob_timeout(T).await.expect("image still pending");
    assert!(body == img);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn screen_frames_dropped_not_kicked_for_slow_viewer() {
    let (srv, _d) = start(|_| {}).await;
    let mut sharer = join(srv.addr, "sharer").await;
    let mut viewer = raw_client::connect_opts(
        srv.addr,
        "viewer",
        None,
        Opts {
            stream_window: Some(16 * 1024),
            ..Opts::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        viewer.recv_timeout(T).await,
        Some(ServerMsg::Welcome { .. })
    ));
    sharer
        .recv_until(T, |m| matches!(m, ServerMsg::UserJoined(_)))
        .await
        .unwrap();
    sharer
        .send(&ClientMsg::SetScreenShare { on: true })
        .await
        .unwrap();
    let on = viewer
        .recv_until(T, |m| matches!(m, ServerMsg::ScreenShare { .. }))
        .await;
    assert!(matches!(on, Some(ServerMsg::ScreenShare { on: true, .. })));
    viewer
        .send(&ClientMsg::WatchScreen {
            user: sharer.id.unwrap(),
            on: true,
        })
        .await
        .unwrap();
    let count = sharer
        .recv_until(T, |m| matches!(m, ServerMsg::ScreenWatchers { .. }))
        .await;
    assert!(matches!(
        count,
        Some(ServerMsg::ScreenWatchers { count: 1, .. })
    ));
    for i in 0..20 {
        sharer
            .send_blob(
                &ClientBlob::ScreenFrame,
                &fake_image(&JPEG_MAGIC, 200 * 1024, i + 1),
            )
            .await
            .unwrap();
    }
    pong(&mut sharer, 13).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let vid = viewer.id;
    assert!(
        srv.disconnects().is_empty(),
        "viewer {vid:?} must not be kicked: {:?}",
        srv.disconnects()
    );
    sharer
        .send(&ClientMsg::Chat {
            text: "still here".into(),
        })
        .await
        .unwrap();
    let got = viewer
        .recv_until(T, |m| matches!(m, ServerMsg::Chat { .. }))
        .await;
    assert!(matches!(got, Some(ServerMsg::Chat { ref text, .. }) if text == "still here"));
    srv.shutdown().await;
}
