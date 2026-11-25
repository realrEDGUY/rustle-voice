//! `vc-release`: create release keys, build + sign + publish client updates (Windows
//! and Linux together), and check what the live update host serves.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use vc_client::update::{self, selftest};
use vc_release::*;

#[derive(Parser)]
#[command(name = "vc-release", about = "Publish signed vc-client auto-updates")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new Ed25519 release key (keep it offline, never in the repository).
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Build the Windows and Linux (in WSL) release binaries for a channel, sign them
    /// and write the upload folder.
    Publish {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long, default_value_t = 180)]
        expires_days: u64,
        /// Emergency or recovery only: publish Windows without Linux. Linux clients
        /// stay on their current version until the next full publish.
        #[arg(long)]
        windows_only: bool,
    },
    /// Re-sign every target's current release with a fresh expiry (same binaries).
    Resign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long, default_value_t = 180)]
        expires_days: u64,
    },
    /// Rewrite index.html (the download page) for the stable release already in `out`.
    Index {
        #[arg(long)]
        out: PathBuf,
    },
    /// Fetch every target's live manifest and binary from the update host and verify
    /// them.
    Verify {
        #[arg(long, default_value = "stable")]
        channel: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen { out } => {
            let hex = keygen(&out, &repo_root())?;
            println!("Wrote private key {}", out.display());
            println!();
            println!("Add this line to crates/vc-client/update-keys.txt and commit it:");
            println!("{hex}");
            println!();
            println!("Back the key file up offline (a second USB stick or password manager).");
            println!("Anyone with it can ship code to every client; losing every key means a");
            println!("manual reinstall for everyone. Never copy it to the update host.");
            Ok(())
        }
        Cmd::Publish {
            key,
            out,
            channel,
            expires_days,
            windows_only,
        } => publish(&key, &out, &channel, expires_days, windows_only),
        Cmd::Resign {
            key,
            out,
            channel,
            expires_days,
        } => resign(&key, &out, &channel, expires_days),
        Cmd::Index { out } => {
            let (win, linux) = rewrite_index(&out)?;
            let linux = linux.map_or("no Linux release".to_string(), |l| {
                format!("Linux {}", l.version)
            });
            println!(
                "Wrote {} for stable Windows {} ({linux}). Upload it to https://{}/.",
                out.join("index.html").display(),
                win.version,
                update::HOST
            );
            Ok(())
        }
        Cmd::Verify { channel } => verify_live(&channel),
    }
}

/// WSL distribution that builds the Linux client (`VC_WSL_DISTRO`, default `Ubuntu`).
fn wsl_distro() -> String {
    std::env::var("VC_WSL_DISTRO")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "Ubuntu".to_string())
}

/// Builds the Linux client inside WSL. `$1` repo (Windows path), `$2` channel,
/// `$3` version. Cargo's output goes to stderr; stdout is only the Linux path of the
/// built binary. The build dir is Linux-native (fast, and never the Windows `target`),
/// and `--locked` keeps the shared `Cargo.lock` untouched. A copy goes to
/// `target/channel-<ch>-linux/` (never `dist/`) for the Windows side to read.
const LINUX_BUILD: &str = r#"set -euo pipefail
cd "$(wslpath -a "$1")"
export PATH="$HOME/.cargo/bin:$PATH"
tdir="$HOME/.cache/vc-release/channel-$2"
echo "Building vc-client $3 for channel $2 (Linux, in WSL) ..." >&2
VC_UPDATE_CHANNEL="$2" cargo build --locked --release -p vc-client --bin vc-client --target-dir "$tdir" >&2
mkdir -p "target/channel-$2-linux"
cp -f "$tdir/release/vc-client" "target/channel-$2-linux/vc-client"
printf '%s\n' "$tdir/release/vc-client"
"#;

/// `C:\...` without the `\\?\` prefix `canonicalize` adds (wslpath does not take it).
fn plain_windows_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Build the Linux client in WSL, self-test it there, and return its bytes.
fn build_linux(repo: &Path, channel: &str, version: &str) -> Result<Vec<u8>> {
    let distro = wsl_distro();
    let out = Command::new("wsl")
        .args(["-d", &distro, "-e", "bash", "-c", LINUX_BUILD, "vc"])
        .arg(plain_windows_path(repo))
        .args([channel, version])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("run wsl (is WSL installed?)")?;
    ensure!(
        out.status.success(),
        "Linux build in WSL distro {distro:?} failed ({}). Set VC_WSL_DISTRO to pick \
         another distro; see the README's Linux build setup",
        out.status
    );
    let linux_bin = String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .find(|l| l.starts_with('/'))
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the WSL build printed no binary path"))?;
    println!("Built {linux_bin} (WSL {distro})");
    // `timeout` inside Linux bounds the real process (killing wsl.exe would not).
    let mut cmd = Command::new("wsl");
    cmd.args(["-d", &distro, "-e", "timeout", "30"])
        .arg(&linux_bin);
    match selftest::run_command(cmd, version, channel, Duration::from_secs(90)) {
        selftest::SelfTest::Pass => {}
        other => bail!("the built Linux binary failed its self-test: {other:?}"),
    }
    let copy = repo
        .join("target")
        .join(format!("channel-{channel}-linux"))
        .join("vc-client");
    let bytes = std::fs::read(&copy).with_context(|| format!("read {}", copy.display()))?;
    ensure!(
        bytes.starts_with(b"\x7fELF"),
        "{} is not an ELF binary",
        copy.display()
    );
    Ok(bytes)
}

fn publish(
    key: &Path,
    out: &Path,
    channel: &str,
    expires_days: u64,
    windows_only: bool,
) -> Result<()> {
    check_channel(channel)?;
    ensure!(
        cfg!(windows),
        "publish runs on the Windows release PC (it builds Linux in WSL)"
    );
    let kp = load_key(key)?;
    let allowed = update::keys();
    // Fail before the long builds if the key or a version would be refused anyway.
    let version = env!("CARGO_PKG_VERSION");
    check_key_allowed(&kp, &allowed)?;
    check_ratchet(out, channel, &WINDOWS, version)?;
    if windows_only {
        eprintln!();
        eprintln!("WARNING: --windows-only: publishing Windows WITHOUT Linux.");
        eprintln!("         Linux clients stay on their current version and the download");
        eprintln!(
            "         page keeps the Linux release already in {}.",
            out.display()
        );
        eprintln!("         Use this only for an emergency or to recover from a failed");
        eprintln!("         publish; the next release must publish both.");
        eprintln!();
    } else {
        check_ratchet(out, channel, &LINUX, version)?;
    }

    let repo = repo_root();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // Channel builds get their own target dir, so the developer's `target\release`
    // exe stays a plain (never self-updating) build and is not rebuilt each time.
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target"));
    let tdir = base.join(format!("channel-{channel}"));
    println!("Building vc-client {version} for channel {channel} (Windows) ...");
    let st = Command::new(cargo)
        .current_dir(&repo)
        .env("VC_UPDATE_CHANNEL", channel)
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "vc-client",
            "--bin",
            "vc-client",
        ])
        .arg("--target-dir")
        .arg(&tdir)
        .status()
        .context("run cargo build")?;
    ensure!(st.success(), "cargo build failed");
    let exe = tdir.join("release").join("vc-client.exe");
    println!("Built {}", exe.display());
    match selftest::run(&exe, version, channel) {
        selftest::SelfTest::Pass => {}
        other => bail!("the built exe failed its self-test: {other:?}"),
    }
    let win_bytes = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
    let mut items = vec![(&WINDOWS, win_bytes)];
    if !windows_only {
        items.push((&LINUX, build_linux(&repo, channel, version)?));
    }

    let ms = publish_all(
        &kp,
        &allowed,
        &items,
        version,
        channel,
        out,
        update::now_unix(),
        expires_days,
    )?;
    for m in &ms {
        println!(
            "Published {} {} {} ({} bytes, sha256 {})",
            channel, m.target, m.version, m.size, m.sha256
        );
    }
    println!();
    println!("Upload to https://{}/ in this order:", update::HOST);
    let mut n = 0;
    let mut step = |s: String| {
        n += 1;
        println!("  {n}. {s}");
    };
    step(
        binary_path(out, channel, &WINDOWS, version)
            .display()
            .to_string(),
    );
    if !windows_only {
        step(
            binary_path(out, channel, &LINUX, version)
                .display()
                .to_string(),
        );
    }
    if writes_index(channel) {
        step(out.join("index.html").display().to_string());
    } else {
        step(format!(
            "(none) index.html left unchanged (channel {channel})"
        ));
    }
    if !windows_only {
        step(signed_path(out, channel, &LINUX).display().to_string());
    }
    step(format!(
        "{}   (last)",
        signed_path(out, channel, &WINDOWS).display()
    ));
    println!("Then run: cargo run -p vc-release -- verify --channel {channel}");
    Ok(())
}

fn resign(key: &Path, out: &Path, channel: &str, expires_days: u64) -> Result<()> {
    check_channel(channel)?;
    let kp = load_key(key)?;
    let allowed = update::keys();
    let now = update::now_unix();
    let mut done = Vec::new();
    for t in TARGETS {
        let p = signed_path(out, channel, t);
        if !p.exists() {
            println!(
                "{}: nothing published in {channel}, skipped",
                target_label(t)
            );
            continue;
        }
        let m = resign_core(&kp, &allowed, channel, t, out, now, expires_days)?;
        println!(
            "Re-signed {} {} {}: expires in {} days.",
            channel,
            target_label(t),
            m.version,
            expires_days
        );
        done.push(p);
    }
    ensure!(
        !done.is_empty(),
        "nothing published in {}",
        out.join(channel).display()
    );
    println!("Upload (Windows last):");
    for p in done.iter().rev() {
        println!("  {}", p.display());
    }
    Ok(())
}

#[cfg(windows)]
fn verify_live(channel: &str) -> Result<()> {
    use vc_client::update::apply::Fetch;
    use vc_client::update::http::WinHttp;
    use vc_client::update::manifest;
    check_channel(channel)?;
    let keys = update::keys();
    ensure!(!keys.is_empty(), "update-keys.txt lists no key");
    let fetch = WinHttp::production();
    let now = update::now_unix();
    // Every channel the release tool publishes to normally must carry Linux too.
    let linux_required = channel == "stable" || channel == "test";
    let mut failed = Vec::new();
    for t in TARGETS {
        let label = target_label(t);
        let mut signed = Vec::new();
        if let Err(e) = fetch.get(
            &t.manifest_path(channel),
            manifest::MAX_FILE as u64,
            &mut |c| {
                signed.extend_from_slice(c);
                Ok(())
            },
        ) {
            let e = format!("{e:#}");
            if *t == LINUX && !linux_required && e.contains("HTTP 404") {
                println!("{label}: no release in {channel} (not required there)");
                continue;
            }
            println!("{label}: FAILED to fetch the manifest: {e}");
            failed.push(label);
            continue;
        }
        let m = match manifest::verify_for(t.triple, &signed, &keys, now, channel) {
            Ok(m) => m,
            Err(e) => {
                println!("{label}: FAILED: live latest.signed: {e}");
                failed.push(label);
                continue;
            }
        };
        println!(
            "{label}: manifest OK: {} {} (issued {}, expires {})",
            channel, m.version, m.issued, m.expires
        );
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let got = fetch.get(
            &t.url_path(channel, &t.file_name(&m.version)),
            m.size,
            &mut |c| {
                hash.update(c);
                Ok(())
            },
        );
        match got {
            Ok(n) if n != m.size => {
                println!(
                    "{label}: FAILED: binary is {n} bytes, manifest says {}",
                    m.size
                );
                failed.push(label);
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                println!("{label}: FAILED to fetch the binary: {e:#}");
                failed.push(label);
                continue;
            }
        }
        if update::to_hex(hash.finish().as_ref()) != m.sha256 {
            println!("{label}: FAILED: binary SHA-256 does not match the manifest");
            failed.push(label);
            continue;
        }
        println!("{label}: binary OK: {} bytes, sha256 {}", m.size, m.sha256);
        let left = m.expires.saturating_sub(now) / DAY;
        if left < 30 {
            println!(
                "{label}: WARNING: the manifest expires in {left} days. Run `vc-release resign` and upload it."
            );
        } else {
            println!("{label}: expires in {left} days.");
        }
    }
    ensure!(
        failed.is_empty(),
        "verify failed for: {}",
        failed.join(", ")
    );
    Ok(())
}

#[cfg(not(windows))]
fn verify_live(_channel: &str) -> Result<()> {
    bail!("verify uses WinHTTP and runs on Windows only")
}
