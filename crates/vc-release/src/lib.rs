//! Release side of vc-client auto-updates: key generation, signing and publishing.
//!
//! Everything here reuses the client's own manifest code (`vc_client::update`), so the
//! tool can never produce a file the client would read differently.
//!
//! A release has one build per [`Target`] (Windows and Linux). Each target has its own
//! `latest.signed` (Windows at `<channel>/latest.signed`, where every shipped Windows
//! client looks; Linux at `<channel>/linux/latest.signed`). Release code always names
//! the target explicitly and never uses the client's host-only helpers
//! (`manifest::verify`, `manifest::file_name`, ...), which describe the machine the
//! tool happens to run on.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ring::signature::{Ed25519KeyPair, KeyPair};
use vc_client::update::manifest::{self, Manifest, PRODUCT};
pub use vc_client::update::manifest::{LINUX, TARGETS, Target, WINDOWS};
use vc_client::update::{parse_version, to_hex};

pub const DAY: u64 = 24 * 3600;

/// The repository root (where the workspace `Cargo.toml` lives).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repository root exists")
}

fn absolute(p: &Path) -> Result<PathBuf> {
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    })
}

/// Create a new Ed25519 signing key at `out` (PKCS#8). Refuses paths inside `repo`
/// and existing files. Returns the public key as hex.
pub fn keygen(out: &Path, repo: &Path) -> Result<String> {
    let out = absolute(out)?;
    let parent = out
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent folder", out.display()))?;
    let parent = parent
        .canonicalize()
        .with_context(|| format!("folder {} must exist", parent.display()))?;
    let repo = repo.canonicalize()?;
    if parent.starts_with(&repo) {
        bail!(
            "refusing to write a private key inside the repository ({}); keep it offline",
            repo.display()
        );
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 =
        Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| anyhow!("key generation failed"))?;
    let kp =
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| anyhow!("bad generated key"))?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out)
        .with_context(|| format!("create {} (it must not exist yet)", out.display()))?;
    f.write_all(pkcs8.as_ref())?;
    f.sync_all()?;
    Ok(to_hex(kp.public_key().as_ref()))
}

pub fn load_key(path: &Path) -> Result<Ed25519KeyPair> {
    let b = fs::read(path).with_context(|| format!("read key {}", path.display()))?;
    Ed25519KeyPair::from_pkcs8(&b)
        .map_err(|_| anyhow!("{} is not an Ed25519 PKCS#8 key", path.display()))
}

pub fn public_key(kp: &Ed25519KeyPair) -> [u8; 32] {
    kp.public_key()
        .as_ref()
        .try_into()
        .expect("Ed25519 public keys are 32 bytes")
}

/// Channel names become URL path segments and folder names.
pub fn check_channel(channel: &str) -> Result<()> {
    ensure!(
        !channel.is_empty()
            && channel.len() <= 32
            && channel
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "channel must be 1-32 chars of a-z, 0-9 or -"
    );
    Ok(())
}

pub fn check_key_allowed(key: &Ed25519KeyPair, allowed: &[[u8; 32]]) -> Result<()> {
    ensure!(
        !allowed.is_empty(),
        "crates/vc-client/update-keys.txt lists no key: clients would ignore this release. \
         Add your public key there (vc-release keygen prints it), commit, and publish again"
    );
    ensure!(
        allowed.contains(&public_key(key)),
        "this key's public half ({}) is not in crates/vc-client/update-keys.txt",
        to_hex(&public_key(key))
    );
    Ok(())
}

/// Short name of a target for messages.
pub fn target_label(t: &Target) -> &'static str {
    if t.dir.is_empty() { "windows" } else { t.dir }
}

/// `<out>/<channel>` or `<out>/<channel>/<dir>`: where `t`'s files go.
pub fn target_dir(out: &Path, channel: &str, t: &Target) -> PathBuf {
    let d = out.join(channel);
    if t.dir.is_empty() { d } else { d.join(t.dir) }
}

/// `t`'s `latest.signed` in `<out>/<channel>`.
pub fn signed_path(out: &Path, channel: &str, t: &Target) -> PathBuf {
    target_dir(out, channel, t).join("latest.signed")
}

/// `t`'s binary for `version` in `<out>/<channel>`.
pub fn binary_path(out: &Path, channel: &str, t: &Target, version: &str) -> PathBuf {
    target_dir(out, channel, t).join(t.file_name(version))
}

/// The manifest currently in `t`'s `latest.signed`, read without checking the
/// signature (it is the publisher's own output folder).
pub fn read_existing(out: &Path, channel: &str, t: &Target) -> Result<Option<Manifest>> {
    let p = signed_path(out, channel, t);
    let b = match fs::read(&p) {
        Ok(b) => b,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", p.display())),
    };
    let nl = b
        .iter()
        .position(|&c| c == b'\n')
        .ok_or_else(|| anyhow!("{} is malformed", p.display()))?;
    Ok(Some(
        serde_json::from_slice(&b[nl + 1..]).with_context(|| format!("parse {}", p.display()))?,
    ))
}

/// Refuse `version` unless it is above what `t` already has in `channel`.
pub fn check_ratchet(out: &Path, channel: &str, t: &Target, version: &str) -> Result<()> {
    let v = parse_version(version)
        .ok_or_else(|| anyhow!("version {version} is not MAJOR.MINOR.PATCH"))?;
    if let Some(old) = read_existing(out, channel, t)? {
        let ov = parse_version(&old.version).unwrap_or((0, 0, 0));
        ensure!(
            v > ov,
            "{channel} already has {} {} published; bump [workspace.package] version above it",
            target_label(t),
            old.version
        );
    }
    Ok(())
}

/// Sign `m` for `t` and check the result with the client's own checks for that target,
/// in memory (nothing is written).
fn sign_checked(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    m: &Manifest,
    t: &Target,
    now: u64,
) -> Result<Vec<u8>> {
    ensure!(
        m.target == t.triple,
        "manifest target {} is not {}",
        m.target,
        t.triple
    );
    let json = serde_json::to_vec_pretty(m)?;
    let file = manifest::sign(key, &json);
    let got = manifest::verify_for(t.triple, &file, allowed, now, &m.channel)
        .map_err(|e| anyhow!("self-check of the signed {} manifest failed: {e}", t.triple))?;
    ensure!(
        &got == m,
        "self-check of the signed manifest returned other fields"
    );
    Ok(file)
}

/// Write signed manifest bytes to `t`'s `latest.signed` (atomically) and re-verify
/// what landed on disk.
fn write_signed_file(
    file: &[u8],
    allowed: &[[u8; 32]],
    channel: &str,
    t: &Target,
    out: &Path,
    now: u64,
) -> Result<()> {
    let p = signed_path(out, channel, t);
    fs::create_dir_all(target_dir(out, channel, t))?;
    let tmp = p.with_extension("signed.tmp");
    fs::write(&tmp, file)?;
    fs::rename(&tmp, &p)?;
    let back = fs::read(&p)?;
    manifest::verify_for(t.triple, &back, allowed, now, channel)
        .map_err(|e| anyhow!("written {} does not verify: {e}", p.display()))?;
    Ok(())
}

/// Write `bytes` to `path`, never replacing a different file: a published binary is
/// immutable. An identical file already there (a rerun after a failed publish) is fine.
fn write_binary(path: &Path, bytes: &[u8]) -> Result<()> {
    let sha = |b: &[u8]| to_hex(ring::digest::digest(&ring::digest::SHA256, b).as_ref());
    match fs::read(path) {
        Ok(existing) => {
            ensure!(
                existing == bytes,
                "{} already exists with other contents; a published binary is never \
                 overwritten. Bump the version",
                path.display()
            );
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    let back = fs::read(path)?;
    ensure!(
        sha(&back) == sha(bytes),
        "written {} does not match its hash",
        path.display()
    );
    Ok(())
}

/// The public download page. Self-contained (no external assets), so the host
/// serves exactly one HTML file. `{{…}}` placeholders are filled by `render_index`.
const INDEX_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Voicechat</title>
<meta name="description" content="Download Voicechat for Windows and Linux.">
<style>
:root{--bg:#f6f5f2;--card:#ffffff;--fg:#1c1b19;--muted:#6b6860;--line:#e4e1da;--accent:#4f46e5;--accent-fg:#ffffff;--code:#f0eee9}
@media (prefers-color-scheme:dark){:root{--bg:#121214;--card:#1b1b1f;--fg:#ecebe8;--muted:#9a978f;--line:#2b2b31;--accent:#818cf8;--accent-fg:#0d0d12;--code:#242429}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif;-webkit-font-smoothing:antialiased}
main{max-width:40rem;margin:0 auto;padding:4.5rem 1rem 3rem}
h1{font-size:2.25rem;line-height:1.1;margin:0 0 .5rem;letter-spacing:-.02em}
.lead{color:var(--muted);margin:0 0 2rem;font-size:1.1rem}
.card{background:var(--card);border:1px solid var(--line);border-radius:14px;padding:1.5rem}
.btn{display:inline-flex;align-items:center;gap:.6rem;background:var(--accent);color:var(--accent-fg);text-decoration:none;font-weight:600;padding:.85rem 1.3rem;border-radius:10px;font-size:1.05rem}
.btn:hover{filter:brightness(1.08)}
.btns{display:flex;flex-wrap:wrap;gap:.75rem}
.btn.alt{background:transparent;color:var(--fg);border:1px solid var(--line)}
.btn svg{width:1.15rem;height:1.15rem}
.meta{color:var(--muted);font-size:.92rem;margin:.9rem 0 0}
h2{font-size:1rem;margin:2.2rem 0 .6rem}
ol,ul{margin:0;padding-left:1.25rem}
li{margin:.3rem 0}
code{background:var(--code);border-radius:5px;padding:.1rem .35rem;font-size:.88em;font-family:ui-monospace,Consolas,monospace}
.sha{word-break:break-all;display:block;padding:.6rem .7rem;margin-top:.4rem}
footer{color:var(--muted);font-size:.85rem;margin-top:2.5rem}
</style>
</head>
<body>
<main>
<h1>Voicechat</h1>
<p class="lead">Voice and screen sharing for your own server. Updates itself quietly in the background.</p>
<div class="card">
<div class="btns">
<a class="btn" href="{{HREF}}" download>
<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M5 21h14"/></svg>
Download for Windows</a>{{LINUX}}{{IOS}}
</div>
<p class="meta">Version {{VERSION}} &middot; {{SIZE}} MB &middot; Windows 10/11, 64-bit</p>
</div>
<h2>Coming soon</h2>
<ul>
<li><strong>Client</strong> for macOS.</li>
<li><strong>Server</strong> for macOS and Linux.</li>
</ul>
<h2>Install on Windows</h2>
<ol>
<li>Make a folder you own, for example <code>%LOCALAPPDATA%\Programs\Voicechat</code>.</li>
<li>Move <code>{{FILE}}</code> there and rename it to <code>vc-client.exe</code>.</li>
<li>Start it. If Windows SmartScreen warns, choose <em>More info</em> then <em>Run anyway</em> (the exe is not code-signed yet).</li>
</ol>
<p class="meta">After the first install you never download again: new versions arrive on their own and start the next time you open Voicechat. Every update is checked against a signing key built into the app, so a tampered download is refused.</p>{{LINUX_INSTALL}}
<h2>Verify (optional)</h2>
<p class="meta">SHA-256 of {{FILE}}:<code class="sha">{{SHA}}</code></p>
<footer>Published {{DATE}}</footer>
</main>
</body>
</html>
"##;

/// Public TestFlight link of the iOS app (Rustle Voice). While it is empty the
/// download page shows no iPhone button.
const TESTFLIGHT_URL: &str = "https://testflight.apple.com/join/aqjA44Pk";

/// The iPhone button for the download page, or nothing when there is no link.
fn ios_button(testflight: &str) -> String {
    if testflight.is_empty() {
        return String::new();
    }
    format!(
        "\n<a class=\"btn alt\" href=\"{testflight}\" rel=\"noopener\">\n<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2.2\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><rect x=\"6\" y=\"2\" width=\"12\" height=\"20\" rx=\"2.5\"/><path d=\"M11 18h2\"/></svg>\nGet it on iPhone (TestFlight)</a>"
    )
}

/// Page-relative link to `t`'s binary for `m` (`stable/…` or `stable/linux/…`).
fn href(t: &Target, m: &Manifest) -> String {
    t.url_path(&m.channel, &t.file_name(&m.version))
        .trim_start_matches('/')
        .to_string()
}

fn size_mb(m: &Manifest) -> String {
    format!("{:.1}", m.size as f64 / 1_048_576.0)
}

/// The Linux download button, or nothing without a Linux release.
fn linux_button(linux: Option<&Manifest>) -> String {
    let Some(m) = linux else {
        return String::new();
    };
    format!(
        "\n<a class=\"btn\" href=\"{}\" download>\n<svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2.2\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\"><path d=\"M12 3v12\"/><path d=\"m7 10 5 5 5-5\"/><path d=\"M5 21h14\"/></svg>\nDownload for Linux</a>",
        href(&LINUX, m)
    )
}

/// The "Install on Linux" section (own version, size and SHA-256, since a Linux
/// release can lag behind Windows), or nothing without a Linux release.
fn linux_install(linux: Option<&Manifest>) -> String {
    let Some(m) = linux else {
        return String::new();
    };
    let file = LINUX.file_name(&m.version);
    format!(
        r#"
<h2>Install on Linux</h2>
<p class="meta">Version {version} &middot; {size} MB &middot; x86-64 with glibc 2.39 or newer (for example Ubuntu 24.04). Needs ALSA (<code>libasound2</code>), xkbcommon, OpenGL (Mesa) and, for updates, <code>curl</code>.</p>
<ol>
<li>Make a folder you own: <code>mkdir -p ~/.local/opt/voicechat</code></li>
<li>Move <code>{file}</code> there and rename it to <code>vc-client</code>: <code>mv ~/Downloads/{file} ~/.local/opt/voicechat/vc-client</code></li>
<li>Make it executable: <code>chmod +x ~/.local/opt/voicechat/vc-client</code></li>
<li>Start it: <code>~/.local/opt/voicechat/vc-client</code></li>
</ol>
<p class="meta">It updates itself in place, like the Windows app. Screen sharing is not available on Linux yet: you can talk, chat and watch other people's screens.</p>
<p class="meta">SHA-256 of {file}:<code class="sha">{sha}</code></p>"#,
        version = m.version,
        size = size_mb(m),
        sha = m.sha256,
    )
}

/// Rewrite `<out>/index.html` for the stable release already in `<out>` (no build, no
/// signing). For page-only changes between releases. Returns the Windows and (if any)
/// Linux manifests shown.
pub fn rewrite_index(out: &Path) -> Result<(Manifest, Option<Manifest>)> {
    let win = read_existing(out, INDEX_CHANNEL, &WINDOWS)?.ok_or_else(|| {
        anyhow!(
            "{} not found: publish to stable first",
            signed_path(out, INDEX_CHANNEL, &WINDOWS).display()
        )
    })?;
    let linux = read_existing(out, INDEX_CHANNEL, &LINUX)?;
    write_index(out, &win, linux.as_ref())?;
    Ok((win, linux))
}

fn write_index(out: &Path, win: &Manifest, linux: Option<&Manifest>) -> Result<()> {
    fs::write(
        out.join("index.html"),
        render_index(win, linux, TESTFLIGHT_URL),
    )?;
    Ok(())
}

fn render_index(win: &Manifest, linux: Option<&Manifest>, testflight: &str) -> String {
    let file = WINDOWS.file_name(&win.version);
    let date = civil_date(linux.map_or(win.issued, |l| l.issued.max(win.issued)));
    INDEX_TEMPLATE
        .replace("{{HREF}}", &href(&WINDOWS, win))
        .replace("{{FILE}}", &file)
        .replace("{{VERSION}}", &win.version)
        .replace("{{SIZE}}", &size_mb(win))
        .replace("{{SHA}}", &win.sha256)
        .replace("{{DATE}}", &date)
        .replace("{{LINUX}}", &linux_button(linux))
        .replace("{{LINUX_INSTALL}}", &linux_install(linux))
        .replace("{{IOS}}", &ios_button(testflight))
}

/// `YYYY-MM-DD` (UTC) for a unix timestamp, without a date crate.
fn civil_date(unix: u64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Publish `version` for every target in `items` (`(target, binary bytes)`) into
/// `<out>/<channel>/`, all or nothing as far as clients can tell:
///
/// 1. every input and every target's version ratchet is checked;
/// 2. every manifest is built, signed and verified in memory (nothing written yet);
/// 3. the binaries are written (an existing different file is refused);
/// 4. `index.html` is rewritten (stable only);
/// 5. the manifests are written, Linux first and Windows **last**, so a failure part
///    way leaves the Windows release (what most clients read) as it was.
///
/// `items` must include Windows: the download page and the Windows ratchet describe
/// every release. A Windows-only publish (`--windows-only`) passes just Windows; the
/// page then keeps showing the Linux release already in `out`, if any.
#[allow(clippy::too_many_arguments)]
pub fn publish_all(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    items: &[(&Target, Vec<u8>)],
    version: &str,
    channel: &str,
    out: &Path,
    now: u64,
    expires_days: u64,
) -> Result<Vec<Manifest>> {
    // 1. Inputs and ratchets.
    check_channel(channel)?;
    check_key_allowed(key, allowed)?;
    parse_version(version).ok_or_else(|| anyhow!("version {version} is not MAJOR.MINOR.PATCH"))?;
    ensure!(expires_days >= 1, "--expires-days must be at least 1");
    ensure!(
        items.iter().any(|(t, _)| **t == WINDOWS),
        "every release includes the Windows build"
    );
    for (i, (t, bytes)) in items.iter().enumerate() {
        ensure!(
            !items[..i].iter().any(|(u, _)| u == t),
            "{} listed twice",
            t.triple
        );
        ensure!(!bytes.is_empty(), "{} binary is empty", target_label(t));
        check_ratchet(out, channel, t, version)?;
    }

    // 2. Sign and verify every manifest in memory.
    let mut signed = Vec::with_capacity(items.len());
    for (t, bytes) in items {
        let m = Manifest {
            product: PRODUCT.into(),
            channel: channel.into(),
            target: t.triple.into(),
            version: version.into(),
            sha256: to_hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref()),
            size: bytes.len() as u64,
            issued: now,
            expires: now + expires_days * DAY,
        };
        let file = sign_checked(key, allowed, &m, t, now)?;
        signed.push((*t, m, file));
    }

    // 3. Binaries.
    for (t, bytes) in items {
        fs::create_dir_all(target_dir(out, channel, t))
            .with_context(|| format!("create {}", target_dir(out, channel, t).display()))?;
        write_binary(&binary_path(out, channel, t, version), bytes)?;
    }

    // 4. Download page.
    if writes_index(channel) {
        let find = |want: &Target| signed.iter().find(|(t, _, _)| *t == want).map(|s| &s.1);
        let win = find(&WINDOWS).expect("Windows is in items");
        let linux = match find(&LINUX) {
            Some(m) => Some(m.clone()),
            None => read_existing(out, channel, &LINUX)?,
        };
        write_index(out, win, linux.as_ref())?;
    }

    // 5. Manifests, Windows last.
    signed.sort_by_key(|(t, _, _)| **t == WINDOWS);
    for (t, _, file) in &signed {
        write_signed_file(file, allowed, channel, t, out, now)?;
    }
    Ok(signed.into_iter().map(|(_, m, _)| m).collect())
}

/// The channel the public download page (`index.html`) describes.
pub const INDEX_CHANNEL: &str = "stable";

/// Only a stable publish rewrites `index.html`, so a test-channel exe never lands on the
/// public download page.
pub fn writes_index(channel: &str) -> bool {
    channel == INDEX_CHANNEL
}

/// Re-sign `t`'s current manifest with a new `issued`/`expires` (same binary).
pub fn resign_core(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    channel: &str,
    t: &Target,
    out: &Path,
    now: u64,
    expires_days: u64,
) -> Result<Manifest> {
    check_channel(channel)?;
    check_key_allowed(key, allowed)?;
    ensure!(expires_days >= 1, "--expires-days must be at least 1");
    let old = read_existing(out, channel, t)?.ok_or_else(|| {
        anyhow!(
            "nothing published in {}",
            target_dir(out, channel, t).display()
        )
    })?;
    ensure!(
        old.target == t.triple,
        "{} names target {}, expected {}",
        signed_path(out, channel, t).display(),
        old.target,
        t.triple
    );
    ensure!(
        now >= old.issued,
        "clock is behind the current manifest's issued time"
    );
    let m = Manifest {
        issued: now,
        expires: now + expires_days * DAY,
        ..old
    };
    let file = sign_checked(key, allowed, &m, t, now)?;
    write_signed_file(&file, allowed, channel, t, out, now)?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_key() -> Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    /// One-target publish of Windows, as the old single-target tool did.
    #[allow(clippy::too_many_arguments)]
    fn publish_win(
        key: &Ed25519KeyPair,
        allowed: &[[u8; 32]],
        exe: &[u8],
        version: &str,
        channel: &str,
        out: &Path,
        now: u64,
        expires_days: u64,
    ) -> Result<Manifest> {
        publish_all(
            key,
            allowed,
            &[(&WINDOWS, exe.to_vec())],
            version,
            channel,
            out,
            now,
            expires_days,
        )
        .map(|mut v| v.remove(0))
    }

    fn both(win: &[u8], linux: &[u8]) -> Vec<(&'static Target, Vec<u8>)> {
        vec![(&WINDOWS, win.to_vec()), (&LINUX, linux.to_vec())]
    }

    fn page_manifest(t: &Target, version: &str) -> Manifest {
        Manifest {
            product: PRODUCT.into(),
            target: t.triple.into(),
            version: version.into(),
            channel: "stable".into(),
            size: 1_048_576,
            sha256: "ab".repeat(32),
            issued: 1_000,
            expires: 2_000,
        }
    }

    #[test]
    fn keygen_into_tempdir_and_no_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("release.pkcs8");
        let hex = keygen(&p, &repo_root()).unwrap();
        assert_eq!(hex.len(), 64);
        let kp = load_key(&p).unwrap();
        assert_eq!(to_hex(&public_key(&kp)), hex);
        // The printed line is what update-keys.txt accepts.
        assert_eq!(
            vc_client::update::parse_keys(&hex).unwrap(),
            vec![public_key(&kp)]
        );
        let before = fs::read(&p).unwrap();
        assert!(keygen(&p, &repo_root()).is_err());
        assert_eq!(fs::read(&p).unwrap(), before);
    }

    #[test]
    fn keygen_into_repo_refused() {
        let repo = repo_root();
        let p = repo
            .join("crates")
            .join("vc-release")
            .join("must-not-exist.pkcs8");
        let e = keygen(&p, &repo).unwrap_err();
        assert!(e.to_string().contains("inside the repository"), "{e}");
        assert!(!p.exists());
        // A fake repo root: same check.
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("sub");
        fs::create_dir(&sub).unwrap();
        assert!(keygen(&sub.join("k.pkcs8"), d.path()).is_err());
        assert!(!sub.join("k.pkcs8").exists());
    }

    #[test]
    fn publish_round_trip_and_tamper() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let exe = b"MZ fake exe bytes".to_vec();
        let m = publish_win(
            &key,
            &allowed,
            &exe,
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        assert_eq!(m.expires, 1_000 + 180 * DAY);
        let signed = fs::read(d.path().join("stable").join("latest.signed")).unwrap();
        assert_eq!(
            manifest::verify_for(WINDOWS.triple, &signed, &allowed, 2_000, "stable").unwrap(),
            m
        );
        let on_disk = fs::read(d.path().join("stable").join("vc-client-0.2.0-x86_64.exe")).unwrap();
        assert_eq!(on_disk, exe);
        let html = fs::read_to_string(d.path().join("index.html")).unwrap();
        assert!(html.contains("0.2.0") && html.contains(&m.sha256));
        // Tampered manifest is rejected by the client verify.
        let mut t = signed.clone();
        let i = t.len() - 3;
        t[i] ^= 1;
        assert!(manifest::verify_for(WINDOWS.triple, &t, &allowed, 2_000, "stable").is_err());
        // Another key cannot verify.
        assert!(
            manifest::verify_for(
                WINDOWS.triple,
                &signed,
                &[public_key(&mem_key())],
                2_000,
                "stable"
            )
            .is_err()
        );
    }

    #[test]
    fn both_targets_publish_in_their_own_layout() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let ms = publish_all(
            &key,
            &allowed,
            &both(b"MZ win", b"\x7fELF linux"),
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        assert_eq!(ms.len(), 2);
        let st = d.path().join("stable");
        // Windows: byte-for-byte the layout shipped clients read.
        assert_eq!(
            fs::read(st.join("vc-client-0.2.0-x86_64.exe")).unwrap(),
            b"MZ win"
        );
        let win = fs::read(st.join("latest.signed")).unwrap();
        let wm = manifest::verify_for(WINDOWS.triple, &win, &allowed, 2_000, "stable").unwrap();
        assert_eq!(wm.target, "x86_64-pc-windows-msvc");
        assert_eq!(wm.size, 6);
        // Linux: its own folder and triple.
        assert_eq!(
            fs::read(st.join("linux").join("vc-client-0.2.0-x86_64-linux")).unwrap(),
            b"\x7fELF linux"
        );
        let lin = fs::read(st.join("linux").join("latest.signed")).unwrap();
        let lm = manifest::verify_for(LINUX.triple, &lin, &allowed, 2_000, "stable").unwrap();
        assert_eq!(lm.target, "x86_64-unknown-linux-gnu");
        // Neither manifest passes as the other.
        assert!(manifest::verify_for(LINUX.triple, &win, &allowed, 2_000, "stable").is_err());
        assert!(manifest::verify_for(WINDOWS.triple, &lin, &allowed, 2_000, "stable").is_err());
        // Nothing else lands in the channel folder.
        let mut names: Vec<String> = fs::read_dir(&st)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["latest.signed", "linux", "vc-client-0.2.0-x86_64.exe"]
        );
        // The page offers both, each with its own link and hash.
        let html = fs::read_to_string(d.path().join("index.html")).unwrap();
        assert!(
            html.contains("href=\"stable/vc-client-0.2.0-x86_64.exe\"")
                && html.contains("href=\"stable/linux/vc-client-0.2.0-x86_64-linux\"")
                && html.contains("Download for Linux")
                && html.contains(&lm.sha256)
                && html.contains(&wm.sha256)
                && !html.contains("{{"),
            "{html}"
        );
    }

    #[test]
    fn ratchet_is_per_target() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        // Windows alone first (as with --windows-only).
        publish_win(
            &key,
            &allowed,
            b"w1",
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        // Both at 0.2.0 again: Windows already has it.
        let e = publish_all(
            &key,
            &allowed,
            &both(b"w2", b"l2"),
            "0.2.0",
            "stable",
            d.path(),
            1_001,
            180,
        )
        .unwrap_err();
        assert!(e.to_string().contains("windows 0.2.0"), "{e}");
        publish_all(
            &key,
            &allowed,
            &both(b"w3", b"l3"),
            "0.3.0",
            "stable",
            d.path(),
            1_002,
            180,
        )
        .unwrap();
        // The Linux ratchet alone blocks the pair, before anything is written: with the
        // Windows manifest gone, only Linux (at 0.3.0) refuses 0.3.0.
        let win_signed = signed_path(d.path(), "stable", &WINDOWS);
        fs::remove_file(&win_signed).unwrap();
        let linux_signed = signed_path(d.path(), "stable", &LINUX);
        let before = fs::read(&linux_signed).unwrap();
        let e = publish_all(
            &key,
            &allowed,
            &both(b"w4", b"l4"),
            "0.3.0",
            "stable",
            d.path(),
            1_003,
            180,
        )
        .unwrap_err();
        assert!(e.to_string().contains("linux 0.3.0"), "{e}");
        assert_eq!(fs::read(&linux_signed).unwrap(), before);
        assert!(!win_signed.exists());
        assert_eq!(
            fs::read(binary_path(d.path(), "stable", &WINDOWS, "0.3.0")).unwrap(),
            b"w3"
        );
        assert_eq!(
            read_existing(d.path(), "stable", &LINUX)
                .unwrap()
                .unwrap()
                .version,
            "0.3.0"
        );
    }

    /// The Linux write fails part way: the Windows manifest (what every installed
    /// Windows client reads) is neither written nor changed, and a Windows-only rerun
    /// succeeds.
    #[test]
    fn failed_linux_write_leaves_windows_manifest_alone() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        publish_win(
            &key,
            &allowed,
            b"w1",
            "0.1.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        let win_signed = signed_path(d.path(), "stable", &WINDOWS);
        let before = fs::read(&win_signed).unwrap();
        // `linux` exists as a file, so the Linux folder cannot be created.
        fs::write(d.path().join("stable").join("linux"), b"in the way").unwrap();
        assert!(
            publish_all(
                &key,
                &allowed,
                &both(b"w2", b"l2"),
                "0.2.0",
                "stable",
                d.path(),
                1_001,
                180
            )
            .is_err()
        );
        assert_eq!(fs::read(&win_signed).unwrap(), before);
        // Also with no Windows manifest at all beforehand.
        let d2 = tempfile::tempdir().unwrap();
        fs::create_dir_all(d2.path().join("stable")).unwrap();
        fs::write(d2.path().join("stable").join("linux"), b"in the way").unwrap();
        assert!(
            publish_all(
                &key,
                &allowed,
                &both(b"w2", b"l2"),
                "0.2.0",
                "stable",
                d2.path(),
                1_001,
                180
            )
            .is_err()
        );
        assert!(!signed_path(d2.path(), "stable", &WINDOWS).exists());
        // Recovery: Windows alone, at the same version (its binary may already be there
        // from the failed run; identical bytes are accepted).
        let m = publish_win(
            &key,
            &allowed,
            b"w2",
            "0.2.0",
            "stable",
            d.path(),
            1_002,
            180,
        )
        .unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(
            read_existing(d.path(), "stable", &WINDOWS)
                .unwrap()
                .unwrap()
                .version,
            "0.2.0"
        );
    }

    #[test]
    fn published_binary_is_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let exe = binary_path(d.path(), "stable", &WINDOWS, "0.2.0");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"someone else's").unwrap();
        let e = publish_win(
            &key,
            &allowed,
            b"mine",
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap_err();
        assert!(e.to_string().contains("never"), "{e}");
        assert_eq!(fs::read(&exe).unwrap(), b"someone else's");
        assert!(!signed_path(d.path(), "stable", &WINDOWS).exists());
    }

    #[test]
    fn windows_is_required_and_targets_are_unique() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let only_linux = [(&LINUX, b"l".to_vec())];
        assert!(
            publish_all(
                &key,
                &allowed,
                &only_linux,
                "0.2.0",
                "stable",
                d.path(),
                1,
                180
            )
            .is_err()
        );
        let twice = [(&WINDOWS, b"a".to_vec()), (&WINDOWS, b"b".to_vec())];
        assert!(publish_all(&key, &allowed, &twice, "0.2.0", "stable", d.path(), 1, 180).is_err());
        let empty_linux = both(b"w", b"");
        assert!(
            publish_all(
                &key,
                &allowed,
                &empty_linux,
                "0.2.0",
                "stable",
                d.path(),
                1,
                180
            )
            .is_err()
        );
        assert!(!d.path().join("stable").exists());
    }

    #[test]
    fn iphone_button_only_with_a_testflight_link() {
        let m = page_manifest(&WINDOWS, "0.2.0");
        let without = render_index(&m, None, "");
        assert!(!without.contains("TestFlight") && !without.contains("{{"));
        let with = render_index(&m, None, "https://testflight.apple.com/join/AbCd1234");
        assert!(with.contains(r#"href="https://testflight.apple.com/join/AbCd1234""#));
        assert!(with.contains("Download for Windows") && !with.contains("{{"));
    }

    #[test]
    fn linux_section_only_with_a_linux_release() {
        let win = page_manifest(&WINDOWS, "0.3.0");
        let without = render_index(&win, None, "");
        assert!(
            !without.contains("Download for Linux")
                && !without.contains("Install on Linux")
                && without.contains("Install on Windows")
                && without.contains("for Windows and Linux")
                && !without.contains("{{"),
            "{without}"
        );
        // Linux one version behind: shown honestly with its own numbers.
        let mut lin = page_manifest(&LINUX, "0.2.0");
        lin.size = 3 * 1_048_576 + 524_288;
        lin.sha256 = "cd".repeat(32);
        let with = render_index(&win, Some(&lin), "");
        assert!(
            with.contains("href=\"stable/linux/vc-client-0.2.0-x86_64-linux\"")
                && with.contains("Download for Linux")
                && with.contains("Install on Linux")
                && with.contains("Version 0.2.0 &middot; 3.5 MB")
                && with.contains("Version 0.3.0 &middot; 1.0 MB")
                && with.contains(&"cd".repeat(32))
                && with.contains("rename it to <code>vc-client</code>")
                && with.contains("chmod +x ~/.local/opt/voicechat/vc-client")
                && with.contains("glibc 2.39")
                && with.contains("Screen sharing is not available on Linux yet")
                && !with.contains("{{"),
            "{with}"
        );
    }

    #[test]
    fn civil_date_matches_known_days() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_795_996_800), "2026-11-30");
    }

    #[test]
    fn only_stable_publish_writes_index() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let index = d.path().join("index.html");
        fs::write(&index, b"existing stable page").unwrap();
        publish_all(
            &key,
            &allowed,
            &both(b"t", b"tl"),
            "0.5.0",
            "test",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        assert_eq!(fs::read(&index).unwrap(), b"existing stable page");
        assert!(!writes_index("test") && writes_index("stable"));
        // Resign never writes it either.
        for t in TARGETS {
            resign_core(&key, &allowed, "test", t, d.path(), 2_000, 180).unwrap();
        }
        assert_eq!(fs::read(&index).unwrap(), b"existing stable page");
        publish_win(
            &key,
            &allowed,
            b"s",
            "0.2.0",
            "stable",
            d.path(),
            3_000,
            180,
        )
        .unwrap();
        let html = fs::read_to_string(&index).unwrap();
        assert!(
            html.contains("Version 0.2.0")
                && html.contains("href=\"stable/vc-client-0.2.0-x86_64.exe\"")
                && html.contains("<strong>Client</strong> for macOS.")
                && html.contains("<strong>Server</strong> for macOS and Linux")
                && !html.contains("Download for Linux")
                && !html.contains("{{"),
            "{html}"
        );
        // A Windows-only stable publish keeps showing the Linux release already there.
        publish_all(
            &key,
            &allowed,
            &both(b"s3", b"l3"),
            "0.3.0",
            "stable",
            d.path(),
            4_000,
            180,
        )
        .unwrap();
        publish_win(
            &key,
            &allowed,
            b"s4",
            "0.4.0",
            "stable",
            d.path(),
            5_000,
            180,
        )
        .unwrap();
        let html = fs::read_to_string(&index).unwrap();
        assert!(
            html.contains("href=\"stable/vc-client-0.4.0-x86_64.exe\"")
                && html.contains("href=\"stable/linux/vc-client-0.3.0-x86_64-linux\""),
            "{html}"
        );
        // `index` rewrites the same page from what is on disk.
        fs::write(&index, b"x").unwrap();
        let (w, l) = rewrite_index(d.path()).unwrap();
        assert_eq!(
            (w.version.as_str(), l.unwrap().version.as_str()),
            ("0.4.0", "0.3.0")
        );
        assert_eq!(fs::read_to_string(&index).unwrap(), html);
    }

    #[test]
    fn publish_refuses_lower_or_equal_version() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        publish_win(
            &key,
            &allowed,
            b"a",
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            180,
        )
        .unwrap();
        assert!(
            publish_win(
                &key,
                &allowed,
                b"b",
                "0.2.0",
                "stable",
                d.path(),
                1_001,
                180
            )
            .is_err()
        );
        assert!(
            publish_win(
                &key,
                &allowed,
                b"b",
                "0.1.9",
                "stable",
                d.path(),
                1_001,
                180
            )
            .is_err()
        );
        assert_eq!(
            read_existing(d.path(), "stable", &WINDOWS)
                .unwrap()
                .unwrap()
                .version,
            "0.2.0"
        );
        publish_win(
            &key,
            &allowed,
            b"c",
            "0.3.0",
            "stable",
            d.path(),
            1_002,
            180,
        )
        .unwrap();
        // Other channels are independent.
        publish_win(&key, &allowed, b"d", "0.1.0", "beta", d.path(), 1_003, 180).unwrap();
    }

    #[test]
    fn publish_refuses_unlisted_or_missing_keys() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let e = publish_win(&key, &[], b"a", "0.2.0", "stable", d.path(), 1_000, 180).unwrap_err();
        assert!(e.to_string().contains("lists no key"), "{e}");
        let other = [public_key(&mem_key())];
        assert!(publish_win(&key, &other, b"a", "0.2.0", "stable", d.path(), 1_000, 180).is_err());
        let mine = [public_key(&key)];
        assert!(publish_win(&key, &mine, b"a", "0.2", "stable", d.path(), 1_000, 180).is_err());
        assert!(publish_win(&key, &mine, b"a", "0.2.0", "../x", d.path(), 1_000, 180).is_err());
        assert!(!d.path().join("stable").exists());
    }

    #[test]
    fn resign_refreshes_expiry_only() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let ms = publish_all(
            &key,
            &allowed,
            &both(b"a", b"la"),
            "0.2.0",
            "stable",
            d.path(),
            1_000,
            1,
        )
        .unwrap();
        for (t, m) in [(&LINUX, &ms[0]), (&WINDOWS, &ms[1])] {
            assert_eq!(m.target, t.triple);
            let r = resign_core(&key, &allowed, "stable", t, d.path(), 50_000, 180).unwrap();
            assert_eq!((r.issued, r.expires), (50_000, 50_000 + 180 * DAY));
            assert_eq!(
                (&r.version, &r.sha256, r.size, &r.target),
                (&m.version, &m.sha256, m.size, &m.target)
            );
            let signed = fs::read(signed_path(d.path(), "stable", t)).unwrap();
            assert_eq!(
                manifest::verify_for(t.triple, &signed, &allowed, 60_000, "stable").unwrap(),
                r
            );
        }
        // Nothing to re-sign in a channel without that target.
        assert!(resign_core(&key, &allowed, "beta", &LINUX, d.path(), 60_000, 180).is_err());
    }

    /// Release code describes every target explicitly. The client's host-only helpers
    /// would silently describe whatever machine the tool runs on.
    #[test]
    fn release_code_never_uses_host_helpers() {
        let banned_calls = [
            concat!("manifest::", "file_name("),
            concat!("manifest::", "url_path("),
            concat!("manifest::", "verify("),
            concat!("update::", "verify("),
            concat!("manifest::", "this_target("),
        ];
        let banned_ident = concat!("TAR", "GET");
        for (name, src) in [
            ("lib.rs", include_str!("lib.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            for b in banned_calls {
                assert!(!src.contains(b), "{name} uses {b}");
            }
            assert!(
                !src.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|w| w == banned_ident),
                "{name} uses the host {banned_ident}"
            );
        }
    }
}
