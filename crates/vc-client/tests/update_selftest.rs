//! The real exe answers `--update-selftest` on piped stdout before any GUI starts.
//! In release (`cargo test --release`) this also covers the GUI subsystem path.

use std::process::{Command, Stdio};

#[test]
fn exe_reports_identity_for_selftest() {
    let out = Command::new(env!("CARGO_BIN_EXE_vc-client"))
        .args(["--update-selftest", "abc123"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("run vc-client");
    assert!(out.status.success(), "{:?}", out.status);
    let channel = option_env!("VC_UPDATE_CHANNEL").unwrap_or("none");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("vc-client {} {channel} abc123\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn selftest_runner_passes_the_real_exe() {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_vc-client"));
    let channel = option_env!("VC_UPDATE_CHANNEL").unwrap_or("none");
    let r = vc_client::update::selftest::run(exe, env!("CARGO_PKG_VERSION"), channel);
    assert_eq!(r, vc_client::update::selftest::SelfTest::Pass);
    // Wrong expectations fail rather than pass.
    let r = vc_client::update::selftest::run(exe, "9.9.9", channel);
    assert!(
        matches!(r, vc_client::update::selftest::SelfTest::Fail(_)),
        "{r:?}"
    );
}
