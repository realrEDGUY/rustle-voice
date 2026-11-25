//! `--update-selftest`: prove a freshly downloaded exe starts and is the expected build.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const FLAG: &str = "--update-selftest";
/// How long a self-test may take before it counts as a (retryable) timeout.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Handle `vc-client --update-selftest <nonce>`: print the identity line and return
/// true (the caller exits 0). Runs before anything else in `main`.
pub fn handle_selftest_arg() -> bool {
    let mut args = std::env::args_os();
    if args.nth(1).as_deref() != Some(std::ffi::OsStr::new(FLAG)) {
        return false;
    }
    let nonce = args
        .next()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", identity_line(&nonce));
    let _ = out.flush();
    true
}

fn identity_line(nonce: &str) -> String {
    format!(
        "vc-client {} {} {nonce}",
        super::RUNNING,
        super::CHANNEL.unwrap_or("none")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfTest {
    Pass,
    /// The exe ran our code and reported the wrong identity. Blacklist it.
    Fail(String),
    /// The exe exited without a usable identity line (non-zero exit, no output).
    /// Counted as a strike; blacklisted after several in a row.
    Crashed(String),
    /// Could not judge (spawn error, timeout). Retry next cycle.
    Transient(String),
}

/// Run `exe --update-selftest <nonce>` hidden and check it reports `expect_version`
/// and `channel`.
pub fn run(exe: &Path, expect_version: &str, channel: &str) -> SelfTest {
    run_with_timeout(exe, expect_version, channel, TIMEOUT)
}

pub fn run_with_timeout(
    exe: &Path,
    expect_version: &str,
    channel: &str,
    timeout: Duration,
) -> SelfTest {
    run_command(Command::new(exe), expect_version, channel, timeout)
}

/// [`run_with_timeout`] for a prepared command, such as the release tool's
/// `wsl -d <distro> -e timeout 30 <linux binary>`. The flag and the nonce are appended
/// here, so the command must end with the binary to test.
pub fn run_command(
    mut cmd: Command,
    expect_version: &str,
    channel: &str,
    timeout: Duration,
) -> SelfTest {
    let mut nonce = [0u8; 8];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce).is_err() {
        return SelfTest::Transient("no randomness".into());
    }
    let nonce = super::to_hex(&nonce);
    cmd.arg(FLAG)
        .arg(&nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return SelfTest::Transient(format!("spawn: {e}")),
    };
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return SelfTest::Transient("self-test timed out".into());
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return SelfTest::Transient(format!("wait: {e}"));
            }
        }
    };
    let mut out = Vec::new();
    if let Some(s) = child.stdout.take() {
        let _ = s.take(4096).read_to_end(&mut out);
    }
    let expected = format!("vc-client {expect_version} {channel} {nonce}");
    classify(status.success(), &status.to_string(), &out, &expected)
}

/// Judge a finished self-test from its exit and stdout.
///
/// - exit 0 and exactly the expected line: `Pass`;
/// - a first line that starts `vc-client ` but is not the expected one, with any exit
///   code: `Fail` (our code ran, and it is the wrong build);
/// - anything else, such as a non-zero exit with no identity line (antivirus kill, a
///   missing DLL, a crash before `main`): `Crashed`, which is retried a few times.
fn classify(success: bool, status: &str, out: &[u8], expected: &str) -> SelfTest {
    let got = String::from_utf8_lossy(out);
    let first = got.lines().next().unwrap_or("").trim_end_matches('\r');
    if success && got.trim_end_matches(['\r', '\n']) == expected {
        return SelfTest::Pass;
    }
    if first.starts_with("vc-client ") && first != expected {
        return SelfTest::Fail(format!("self-test printed {:?}", got.trim()));
    }
    SelfTest::Crashed(format!(
        "self-test exited with {status}, printed {:?}",
        got.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_line_format() {
        assert_eq!(
            identity_line("abc"),
            format!("vc-client {} none abc", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn missing_exe_is_transient() {
        let d = tempfile::tempdir().unwrap();
        let r = run(&d.path().join("nope.exe"), "0.1.0", "stable");
        assert!(matches!(r, SelfTest::Transient(_)), "{r:?}");
    }

    #[cfg(windows)]
    #[test]
    fn foreign_exe_never_passes() {
        // cmd.exe ignores our flag and prints its own banner: not ours, so a crash strike.
        let cmd = std::path::PathBuf::from(std::env::var_os("ComSpec").unwrap());
        let r = run(&cmd, "0.1.0", "stable");
        assert!(
            matches!(r, SelfTest::Crashed(_) | SelfTest::Transient(_)),
            "{r:?}"
        );
    }

    #[test]
    fn classification() {
        let exp = "vc-client 0.2.0 stable n1";
        assert_eq!(
            classify(true, "0", b"vc-client 0.2.0 stable n1\r\n", exp),
            SelfTest::Pass
        );
        // Non-zero exit with no output: crashed, not blacklisted outright.
        assert!(matches!(
            classify(false, "1", b"", exp),
            SelfTest::Crashed(_)
        ));
        assert!(matches!(
            classify(true, "0", b"", exp),
            SelfTest::Crashed(_)
        ));
        assert!(matches!(
            classify(false, "1", b"some loader error\n", exp),
            SelfTest::Crashed(_)
        ));
        // The right line but a non-zero exit is not a pass.
        assert!(matches!(
            classify(false, "1", b"vc-client 0.2.0 stable n1\n", exp),
            SelfTest::Crashed(_)
        ));
        // Our code ran and is the wrong build: fail, with any exit code.
        for (ok, out) in [
            (true, &b"vc-client 0.1.9 stable n1\n"[..]),
            (true, b"vc-client 0.2.0 none n1\n"),
            (true, b"vc-client 0.2.0 stable other\n"),
            (false, b"vc-client 0.2.0 stable n2\n"),
        ] {
            assert!(
                matches!(classify(ok, "x", out, exp), SelfTest::Fail(_)),
                "{out:?}"
            );
        }
    }
}
