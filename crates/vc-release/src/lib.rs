//! Release side of vc-client auto-updates: key generation, signing and publishing.
//!
//! Everything here reuses the client's own manifest code (`vc_client::update`), so the
//! tool can never produce a file the client would read differently.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ring::signature::{Ed25519KeyPair, KeyPair};
use vc_client::update::manifest::{self, Manifest, PRODUCT, TARGET};
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

fn check_key_allowed(key: &Ed25519KeyPair, allowed: &[[u8; 32]]) -> Result<()> {
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

fn signed_path(out: &Path, channel: &str) -> PathBuf {
    out.join(channel).join("latest.signed")
}

/// The manifest currently in `<out>/<channel>/latest.signed`, read without checking
/// the signature (it is the publisher's own output folder).
pub fn read_existing(out: &Path, channel: &str) -> Result<Option<Manifest>> {
    let p = signed_path(out, channel);
    let b = match fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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

/// Sign `m`, write `latest.signed` (atomically) and re-verify it with the client's
/// own `verify` and `allowed` keys.
fn write_signed(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    m: &Manifest,
    out: &Path,
    now: u64,
) -> Result<()> {
    let json = serde_json::to_vec_pretty(m)?;
    let file = manifest::sign(key, &json);
    let got = manifest::verify(&file, allowed, now, &m.channel)
        .map_err(|e| anyhow!("self-check of the signed manifest failed: {e}"))?;
    ensure!(
        &got == m,
        "self-check of the signed manifest returned other fields"
    );
    let p = signed_path(out, &m.channel);
    let tmp = p.with_extension("signed.tmp");
    fs::write(&tmp, &file)?;
    fs::rename(&tmp, &p)?;
    let back = fs::read(&p)?;
    manifest::verify(&back, allowed, now, &m.channel)
        .map_err(|e| anyhow!("written {} does not verify: {e}", p.display()))?;
    Ok(())
}

/// The public download page. Self-contained (no external assets), so the host
/// serves exactly one HTML file. `{{…}}` placeholders are filled by `write_index`.
const INDEX_TEMPLATE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Voicechat</title>
<meta name="description" content="Download Voicechat for Windows.">
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
ol{margin:0;padding-left:1.25rem}
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
Download for Windows</a>{{IOS}}
</div>
<p class="meta">Version {{VERSION}} &middot; {{SIZE}} MB &middot; Windows 10/11, 64-bit</p>
</div>
<h2>Install</h2>
<ol>
<li>Make a folder you own, for example <code>%LOCALAPPDATA%\Programs\Voicechat</code>.</li>
<li>Move <code>{{FILE}}</code> there and rename it to <code>vc-client.exe</code>.</li>
<li>Start it. If Windows SmartScreen warns, choose <em>More info</em> then <em>Run anyway</em> (the exe is not code-signed yet).</li>
</ol>
<p class="meta">After the first install you never download again: new versions arrive on their own and start the next time you open Voicechat. Every update is checked against a signing key built into the app, so a tampered download is refused.</p>
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

/// Rewrite `<out>/index.html` for the stable release already in `<out>` (no build, no
/// signing). For page-only changes between releases.
pub fn rewrite_index(out: &Path) -> Result<Manifest> {
    let m = read_existing(out, "stable")?.ok_or_else(|| {
        anyhow!(
            "{} not found: publish to stable first",
            signed_path(out, "stable").display()
        )
    })?;
    write_index(out, &m)?;
    Ok(m)
}

fn write_index(out: &Path, m: &Manifest) -> Result<()> {
    fs::write(out.join("index.html"), render_index(m, TESTFLIGHT_URL))?;
    Ok(())
}

fn render_index(m: &Manifest, testflight: &str) -> String {
    let file = manifest::file_name(&m.version);
    let date = civil_date(m.issued);
    INDEX_TEMPLATE
        .replace("{{HREF}}", &format!("{}/{}", m.channel, file))
        .replace("{{FILE}}", &file)
        .replace("{{VERSION}}", &m.version)
        .replace("{{SIZE}}", &format!("{:.1}", m.size as f64 / 1_048_576.0))
        .replace("{{SHA}}", &m.sha256)
        .replace("{{DATE}}", &date)
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

/// Write the exe and a freshly signed manifest for `version` into `<out>/<channel>/`.
#[allow(clippy::too_many_arguments)]
pub fn publish_core(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    exe: &[u8],
    version: &str,
    channel: &str,
    out: &Path,
    now: u64,
    expires_days: u64,
) -> Result<Manifest> {
    check_channel(channel)?;
    check_key_allowed(key, allowed)?;
    let v = parse_version(version)
        .ok_or_else(|| anyhow!("version {version} is not MAJOR.MINOR.PATCH"))?;
    ensure!(expires_days >= 1, "--expires-days must be at least 1");
    ensure!(!exe.is_empty(), "exe is empty");
    if let Some(old) = read_existing(out, channel)? {
        let ov = parse_version(&old.version).unwrap_or((0, 0, 0));
        ensure!(
            v > ov,
            "{channel} already has {} published; bump [workspace.package] version above it",
            old.version
        );
    }
    let m = Manifest {
        product: PRODUCT.into(),
        channel: channel.into(),
        target: TARGET.into(),
        version: version.into(),
        sha256: to_hex(ring::digest::digest(&ring::digest::SHA256, exe).as_ref()),
        size: exe.len() as u64,
        issued: now,
        expires: now + expires_days * DAY,
    };
    let dir = out.join(channel);
    fs::create_dir_all(&dir)?;
    let exe_path = dir.join(manifest::file_name(version));
    fs::write(&exe_path, exe)?;
    let back = fs::read(&exe_path)?;
    ensure!(
        to_hex(ring::digest::digest(&ring::digest::SHA256, &back).as_ref()) == m.sha256,
        "written exe does not match its hash"
    );
    write_signed(key, allowed, &m, out, now)?;
    if writes_index(channel) {
        write_index(out, &m)?;
    }
    Ok(m)
}

/// The channel the public download page (`index.html`) describes.
pub const INDEX_CHANNEL: &str = "stable";

/// Only a stable publish rewrites `index.html`, so a test-channel exe never lands on the
/// public download page.
pub fn writes_index(channel: &str) -> bool {
    channel == INDEX_CHANNEL
}

/// Re-sign the current manifest with a new `issued`/`expires` (same binary).
pub fn resign_core(
    key: &Ed25519KeyPair,
    allowed: &[[u8; 32]],
    channel: &str,
    out: &Path,
    now: u64,
    expires_days: u64,
) -> Result<Manifest> {
    check_channel(channel)?;
    check_key_allowed(key, allowed)?;
    ensure!(expires_days >= 1, "--expires-days must be at least 1");
    let old = read_existing(out, channel)?
        .ok_or_else(|| anyhow!("nothing published in {}", out.join(channel).display()))?;
    ensure!(
        now >= old.issued,
        "clock is behind the current manifest's issued time"
    );
    let m = Manifest {
        issued: now,
        expires: now + expires_days * DAY,
        ..old
    };
    write_signed(key, allowed, &m, out, now)?;
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
        let m = publish_core(
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
            manifest::verify(&signed, &allowed, 2_000, "stable").unwrap(),
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
        assert!(manifest::verify(&t, &allowed, 2_000, "stable").is_err());
        // Another key cannot verify.
        assert!(manifest::verify(&signed, &[public_key(&mem_key())], 2_000, "stable").is_err());
    }

    #[test]
    fn iphone_button_only_with_a_testflight_link() {
        let m = Manifest {
            product: PRODUCT.into(),
            target: TARGET.into(),
            version: "0.2.0".into(),
            channel: "stable".into(),
            size: 1_048_576,
            sha256: "ab".repeat(32),
            issued: 1_000,
            expires: 2_000,
        };
        let without = render_index(&m, "");
        assert!(!without.contains("TestFlight") && !without.contains("{{"));
        let with = render_index(&m, "https://testflight.apple.com/join/AbCd1234");
        assert!(with.contains(r#"href="https://testflight.apple.com/join/AbCd1234""#));
        assert!(with.contains("Download for Windows") && !with.contains("{{"));
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
        publish_core(&key, &allowed, b"t", "0.5.0", "test", d.path(), 1_000, 180).unwrap();
        assert_eq!(fs::read(&index).unwrap(), b"existing stable page");
        assert!(!writes_index("test") && writes_index("stable"));
        // Resign never writes it either.
        resign_core(&key, &allowed, "test", d.path(), 2_000, 180).unwrap();
        assert_eq!(fs::read(&index).unwrap(), b"existing stable page");
        publish_core(
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
                && !html.contains("{{"),
            "{html}"
        );
    }

    #[test]
    fn publish_refuses_lower_or_equal_version() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        publish_core(
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
            publish_core(
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
            publish_core(
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
            read_existing(d.path(), "stable").unwrap().unwrap().version,
            "0.2.0"
        );
        publish_core(
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
        publish_core(&key, &allowed, b"d", "0.1.0", "beta", d.path(), 1_003, 180).unwrap();
    }

    #[test]
    fn publish_refuses_unlisted_or_missing_keys() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let e = publish_core(&key, &[], b"a", "0.2.0", "stable", d.path(), 1_000, 180).unwrap_err();
        assert!(e.to_string().contains("lists no key"), "{e}");
        let other = [public_key(&mem_key())];
        assert!(publish_core(&key, &other, b"a", "0.2.0", "stable", d.path(), 1_000, 180).is_err());
        assert!(
            publish_core(
                &key,
                &[public_key(&key)],
                b"a",
                "0.2",
                "stable",
                d.path(),
                1_000,
                180
            )
            .is_err()
        );
        assert!(
            publish_core(
                &key,
                &[public_key(&key)],
                b"a",
                "0.2.0",
                "../x",
                d.path(),
                1_000,
                180
            )
            .is_err()
        );
        assert!(!d.path().join("stable").exists());
    }

    #[test]
    fn resign_refreshes_expiry_only() {
        let d = tempfile::tempdir().unwrap();
        let key = mem_key();
        let allowed = [public_key(&key)];
        let m = publish_core(&key, &allowed, b"a", "0.2.0", "stable", d.path(), 1_000, 1).unwrap();
        let r = resign_core(&key, &allowed, "stable", d.path(), 50_000, 180).unwrap();
        assert_eq!((r.issued, r.expires), (50_000, 50_000 + 180 * DAY));
        assert_eq!(
            (&r.version, &r.sha256, r.size),
            (&m.version, &m.sha256, m.size)
        );
        let signed = fs::read(d.path().join("stable").join("latest.signed")).unwrap();
        assert_eq!(
            manifest::verify(&signed, &allowed, 60_000, "stable").unwrap(),
            r
        );
    }
}
