//! `vc-release`: create release keys, build + sign + publish client updates, and
//! check what the live update host serves.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Build the release exe for a channel, sign it and write the upload folder.
    Publish {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "stable")]
        channel: String,
        #[arg(long, default_value_t = 180)]
        expires_days: u64,
    },
    /// Re-sign the current release with a fresh expiry (same exe).
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
    /// Fetch the live manifest and exe from the update host and verify both.
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
        } => publish(&key, &out, &channel, expires_days),
        Cmd::Resign {
            key,
            out,
            channel,
            expires_days,
        } => {
            let kp = load_key(&key)?;
            let m = resign_core(
                &kp,
                &update::keys(),
                &channel,
                &out,
                update::now_unix(),
                expires_days,
            )?;
            println!(
                "Re-signed {} {}: expires in {} days. Upload {}.",
                channel,
                m.version,
                expires_days,
                out.join(&channel).join("latest.signed").display()
            );
            Ok(())
        }
        Cmd::Index { out } => {
            let m = rewrite_index(&out)?;
            println!(
                "Wrote {} for stable {}. Upload it to https://{}/.",
                out.join("index.html").display(),
                m.version,
                update::HOST
            );
            Ok(())
        }
        Cmd::Verify { channel } => verify_live(&channel),
    }
}

fn publish(key: &Path, out: &Path, channel: &str, expires_days: u64) -> Result<()> {
    check_channel(channel)?;
    let kp = load_key(key)?;
    let allowed = update::keys();
    // Fail before the long build if the key or version would be refused anyway.
    let version = env!("CARGO_PKG_VERSION");
    if let Some(old) = read_existing(out, channel)? {
        ensure!(
            update::parse_version(version) > update::parse_version(&old.version),
            "{channel} already has {} published; bump [workspace.package] version",
            old.version
        );
    }
    ensure!(
        !allowed.is_empty() && allowed.contains(&public_key(&kp)),
        "the key is not listed in crates/vc-client/update-keys.txt (or the file is empty)"
    );

    let repo = repo_root();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    // Channel builds get their own target dir, so the developer's `target\release`
    // exe stays a plain (never self-updating) build and is not rebuilt each time.
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target"));
    let tdir = base.join(format!("channel-{channel}"));
    println!("Building vc-client {version} for channel {channel} ...");
    let st = Command::new(cargo)
        .current_dir(&repo)
        .env("VC_UPDATE_CHANNEL", channel)
        .args([
            "build",
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
    let bytes = std::fs::read(&exe).with_context(|| format!("read {}", exe.display()))?;
    let m = publish_core(
        &kp,
        &allowed,
        &bytes,
        version,
        channel,
        out,
        update::now_unix(),
        expires_days,
    )?;
    let dir = out.join(channel);
    println!(
        "Published {} {} ({} bytes, sha256 {})",
        channel, m.version, m.size, m.sha256
    );
    println!();
    println!("Upload to https://{}/ in this order:", update::HOST);
    println!(
        "  1. {}",
        dir.join(vc_client::update::manifest::file_name(&m.version))
            .display()
    );
    if writes_index(channel) {
        println!("  2. {}", out.join("index.html").display());
    } else {
        println!("  2. (none) index.html left unchanged (channel {channel})");
    }
    println!("  3. {}   (last)", dir.join("latest.signed").display());
    println!("Then run: cargo run -p vc-release -- verify --channel {channel}");
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
    let mut signed = Vec::new();
    fetch.get(
        &manifest::url_path(channel, "latest.signed"),
        manifest::MAX_FILE as u64,
        &mut |c| {
            signed.extend_from_slice(c);
            Ok(())
        },
    )?;
    let now = update::now_unix();
    let m = manifest::verify(&signed, &keys, now, channel)
        .map_err(|e| anyhow::anyhow!("live latest.signed: {e}"))?;
    println!(
        "Manifest OK: {} {} (issued {}, expires {})",
        channel, m.version, m.issued, m.expires
    );
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let n = fetch.get(
        &manifest::url_path(channel, &manifest::file_name(&m.version)),
        m.size,
        &mut |c| {
            hash.update(c);
            Ok(())
        },
    )?;
    ensure!(n == m.size, "exe is {n} bytes, manifest says {}", m.size);
    ensure!(
        update::to_hex(hash.finish().as_ref()) == m.sha256,
        "exe SHA-256 does not match the manifest"
    );
    println!("Exe OK: {n} bytes, sha256 {}", m.sha256);
    let left = m.expires.saturating_sub(now) / DAY;
    if left < 30 {
        println!(
            "WARNING: the manifest expires in {left} days. Run `vc-release resign` and upload latest.signed."
        );
    } else {
        println!("Expires in {left} days.");
    }
    Ok(())
}

#[cfg(not(windows))]
fn verify_live(_channel: &str) -> Result<()> {
    bail!("verify uses WinHTTP and runs on Windows only")
}
