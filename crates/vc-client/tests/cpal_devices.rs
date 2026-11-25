//! Real-device tests. Run with `cargo test -p vc-client --test cpal_devices -- --ignored`.
// cpal is not built for iOS (the app's audio comes from AVAudioEngine).
#![cfg(not(target_os = "ios"))]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use vc_client::audio::{AudioBackend, CpalBackend};

#[test]
#[ignore = "needs real audio devices"]
fn cpal_lists_devices() {
    let b = CpalBackend::new();
    let ins = b.inputs();
    let outs = b.outputs();
    println!("inputs: {ins:#?}\noutputs: {outs:#?}");
    assert!(!ins.is_empty(), "at least one input device");
    assert!(!outs.is_empty(), "at least one output device");
    for d in ins.iter().chain(&outs) {
        assert!(!d.id.is_empty());
        assert!(!d.name.is_empty());
    }
}

#[test]
#[ignore = "needs real audio devices"]
fn cpal_default_input_delivers_frames() {
    let mut b = CpalBackend::new();
    let got = Arc::new(Mutex::new(0usize));
    let g = got.clone();
    let stream = b
        .start_capture(None, Box::new(move |s| *g.lock().unwrap() += s.len()))
        .expect("start capture");
    std::thread::sleep(Duration::from_millis(500));
    drop(stream);
    let n = *got.lock().unwrap();
    println!("captured {n} samples in 500 ms");
    // 48 kHz mono after conversion: expect well over 100 ms worth.
    assert!(n > 4_800, "captured only {n} samples");
}

#[test]
#[ignore = "needs real audio devices"]
fn cpal_default_output_plays_200ms() {
    let mut b = CpalBackend::new();
    let pulled = Arc::new(Mutex::new(0usize));
    let p = pulled.clone();
    let mut phase = 0u64;
    let stream = b
        .start_playback(
            None,
            Box::new(move |out| {
                for o in out.iter_mut() {
                    *o =
                        0.05 * (2.0 * std::f32::consts::PI * 440.0 * phase as f32 / 48_000.0).sin();
                    phase += 1;
                }
                *p.lock().unwrap() += out.len();
            }),
        )
        .expect("start playback");
    std::thread::sleep(Duration::from_millis(200));
    drop(stream);
    let n = *pulled.lock().unwrap();
    println!("played {n} samples in 200 ms");
    assert!(n > 2_400, "pulled only {n} samples");
}

#[test]
#[ignore = "needs real audio devices"]
fn cpal_device_id_roundtrip() {
    let mut b = CpalBackend::new();
    let (din, dout) = b.default_ids();
    let din = din.expect("default input id");
    let dout = dout.expect("default output id");
    assert!(
        b.inputs().iter().any(|d| d.id == din),
        "default input listed by id"
    );
    assert!(
        b.outputs().iter().any(|d| d.id == dout),
        "default output listed by id"
    );
    // A persisted id string opens that device again.
    let s = b
        .start_capture(Some(&din), Box::new(|_| {}))
        .expect("capture by id");
    drop(s);
    let s = b
        .start_playback(Some(&dout), Box::new(|_| {}))
        .expect("playback by id");
    drop(s);
    // An unknown id falls back to the default device instead of failing.
    let s = b
        .start_capture(Some("no-such-device"), Box::new(|_| {}))
        .expect("fallback");
    drop(s);
}

#[test]
#[ignore = "needs Windows 10 2004+ audio (process loopback)"]
fn loopback_captures() {
    let mut b = CpalBackend::new();
    let got = Arc::new(Mutex::new(0usize));
    let g = got.clone();
    let lb = b
        .start_loopback(Box::new(move |s| *g.lock().unwrap() += s.len()))
        .expect("start process loopback");
    std::thread::sleep(Duration::from_millis(500));
    let t = std::time::Instant::now();
    drop(lb);
    assert!(t.elapsed() < Duration::from_secs(1), "stops promptly");
    // Our own process is excluded, so this is only non-zero while another app
    // plays sound; starting and stopping cleanly is what is checked.
    println!(
        "loopback captured {} samples in 500 ms",
        *got.lock().unwrap()
    );
}
