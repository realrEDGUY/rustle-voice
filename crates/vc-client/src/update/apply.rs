//! Check, download, verify, self-test and swap; plus launch-time cleanup and rollback.
//!
//! Invariant: the exe path is never written until the manifest signature, the exe's
//! size and SHA-256, and its self-test have all passed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use super::manifest::{self, Decision, MAX_FILE, decide, file_name, url_path, verify};
use super::selftest::SelfTest;
use super::state::{self, Loaded, Pending, State, Strikes};
use super::{Paths, SWAP_GUARD, parse_version};

/// An HTTP GET of `path` on the update host, streamed into `sink`. Fails unless the
/// status is 200, and aborts as soon as more than `max_len` bytes arrive.
pub trait Fetch {
    fn get(
        &self,
        path: &str,
        max_len: u64,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> anyhow::Result<u64>;
}

/// Runs the self-test of a staged exe: `(exe, expected version, channel)`.
pub type SelfTestFn<'a> = &'a dyn Fn(&Path, &str, &str) -> SelfTest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to do (up to date, replay, blacklisted, ...).
    Skip(&'static str),
    /// The new exe is in place; the next launch runs it.
    Staged(String),
    /// Something failed verification. Disk unchanged.
    Rejected(String),
    /// Network, IO or a transient self-test problem. Disk unchanged; retry later.
    Failed(String),
}

fn lock_file(paths: &Paths) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock)
}

/// How long background work waits for the cross-process update lock.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Wait up to `max` for the lock on an open lock file. `WouldBlock` once `max` passes.
fn wait_lock(f: File, max: Duration) -> io::Result<File> {
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= max {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Take the cross-process update lock, waiting at most `max` (never unbounded, so no
/// caller can hang window close on another process).
fn lock_within(paths: &Paths, max: Duration) -> io::Result<File> {
    wait_lock(lock_file(paths)?, max)
}

/// The higher of two version strings (an unparseable one counts as lowest).
fn max_version(a: &str, b: &str) -> String {
    let pa = parse_version(a).unwrap_or((0, 0, 0));
    let pb = parse_version(b).unwrap_or((0, 0, 0));
    if pb > pa {
        b.to_string()
    } else {
        a.to_string()
    }
}

/// One update cycle. See the module invariant.
pub fn check_and_stage(
    paths: &Paths,
    fetch: &dyn Fetch,
    keys: &[[u8; 32]],
    now: u64,
    running: &str,
    channel: &str,
    selftest: SelfTestFn,
) -> Outcome {
    check_and_stage_with(
        paths,
        fetch,
        keys,
        now,
        running,
        channel,
        selftest,
        &mut |a, b| fs::rename(a, b),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_and_stage_with(
    paths: &Paths,
    fetch: &dyn Fetch,
    keys: &[[u8; 32]],
    now: u64,
    running: &str,
    channel: &str,
    selftest: SelfTestFn,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> Outcome {
    let Some(st) = state::load(&paths.state).usable() else {
        return Outcome::Skip("update state unreadable");
    };

    // 1. Signed manifest.
    let mut signed = Vec::new();
    if let Err(e) = fetch.get(
        &url_path(channel, "latest.signed"),
        MAX_FILE as u64,
        &mut |c| {
            signed.extend_from_slice(c);
            Ok(())
        },
    ) {
        return Outcome::Failed(format!("{e:#}"));
    }
    let m = match verify(&signed, keys, now, channel) {
        Ok(m) => m,
        Err(e) => return Outcome::Rejected(e.to_string()),
    };
    if let Decision::Skip(why) = decide(&m, running, &st) {
        record_check(paths, &m, now);
        return Outcome::Skip(why);
    }

    let pid = std::process::id();
    let dl = paths.download(pid);
    let new = paths.new_exe(pid);
    let cleanup = || {
        let _ = fs::remove_file(&dl);
        let _ = fs::remove_file(&new);
    };

    // 2. Exe, hashed while streaming, capped at the signed size.
    if let Err(o) = download(fetch, channel, &m, &dl) {
        cleanup();
        return o;
    }

    // 3. Self-test the verified exe.
    if let Err(e) = fs::rename(&dl, &new) {
        cleanup();
        return Outcome::Failed(format!("stage: {e}"));
    }
    match selftest(&new, &m.version, channel) {
        SelfTest::Pass => {}
        SelfTest::Fail(why) => {
            cleanup();
            with_state(paths, |s| s.mark_bad(&m.version));
            return Outcome::Rejected(format!("{} failed its self-test: {why}", m.version));
        }
        SelfTest::Crashed(why) => {
            cleanup();
            let mut count = 0;
            let mut banned = false;
            with_state(paths, |s| {
                count = match &s.selftest_strikes {
                    Some(k) if k.version == m.version => k.count + 1,
                    _ => 1,
                };
                if count >= MAX_STRIKES {
                    s.mark_bad(&m.version);
                    s.selftest_strikes = None;
                    banned = true;
                } else {
                    s.selftest_strikes = Some(Strikes {
                        version: m.version.clone(),
                        count,
                    });
                }
            });
            if banned {
                return Outcome::Rejected(format!(
                    "{} crashed in {count} self-tests in a row: {why}",
                    m.version
                ));
            }
            return Outcome::Failed(format!("self-test crashed ({count}/{MAX_STRIKES}): {why}"));
        }
        SelfTest::Transient(why) => {
            cleanup();
            return Outcome::Failed(format!("self-test: {why}"));
        }
    }

    // 4. Swap. The file lock is taken first with a bounded wait; the process-exit guard
    // only around the state save and the renames, so window close never waits on
    // another process.
    let _lock = match lock_within(paths, LOCK_WAIT) {
        Ok(l) => l,
        Err(_) => {
            cleanup();
            return Outcome::Failed("update lock busy".into());
        }
    };
    let Some(prev) = state::load(&paths.state).usable() else {
        cleanup();
        return Outcome::Skip("update state unreadable");
    };
    if let Decision::Skip(why) = decide(&m, running, &prev) {
        // Another instance got there first.
        cleanup();
        return Outcome::Skip(why);
    }
    let _guard = SWAP_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    // 5. Record the swap before doing it. A passing self-test clears crash strikes.
    let next = State {
        installed: m.version.clone(),
        highest_issued: prev.highest_issued.max(m.issued),
        pending: Some(Pending {
            version: m.version.clone(),
            boots: 0,
            from: max_version(running, &prev.installed),
        }),
        last_check: now,
        selftest_strikes: None,
        ..prev.clone()
    };
    if let Err(e) = state::save(&paths.state, &next) {
        cleanup();
        return Outcome::Failed(format!("save state: {e:#}"));
    }
    // 6. Clear a stale rollback copy (fails harmlessly if an old instance runs from it).
    let _ = fs::remove_file(&paths.old);
    // 7. exe → old, new → exe.
    if let Err(e) = rename(&paths.exe, &paths.old) {
        let _ = state::save(&paths.state, &prev);
        cleanup();
        return Outcome::Failed(format!("move current exe aside: {e}"));
    }
    if let Err(e) = rename(&new, &paths.exe) {
        let restored = rename(&paths.old, &paths.exe);
        let _ = state::save(&paths.state, &prev);
        cleanup();
        if let Err(r) = restored {
            tracing::error!(
                "update: could not restore {} from {}: {r}",
                paths.exe.display(),
                paths.old.display()
            );
        }
        return Outcome::Failed(format!("move new exe in place: {e}"));
    }
    Outcome::Staged(m.version)
}

/// Stream the exe for `m` into `dest`, checking size and SHA-256, then flush to disk.
fn download(
    fetch: &dyn Fetch,
    channel: &str,
    m: &manifest::Manifest,
    dest: &Path,
) -> Result<(), Outcome> {
    let mut f = File::create(dest).map_err(|e| Outcome::Failed(format!("create download: {e}")))?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let got = fetch
        .get(
            &url_path(channel, &file_name(&m.version)),
            m.size,
            &mut |c| {
                hash.update(c);
                f.write_all(c)
            },
        )
        .map_err(|e| Outcome::Failed(format!("{e:#}")))?;
    if got != m.size {
        return Err(Outcome::Rejected(format!(
            "exe is {got} bytes, manifest says {}",
            m.size
        )));
    }
    if super::to_hex(hash.finish().as_ref()) != m.sha256 {
        return Err(Outcome::Rejected(
            "exe SHA-256 does not match the manifest".into(),
        ));
    }
    f.sync_all()
        .map_err(|e| Outcome::Failed(format!("sync download: {e}")))?;
    Ok(())
}

/// Remember a verified manifest's `issued` (replay ratchet) and the check time.
fn record_check(paths: &Paths, m: &manifest::Manifest, now: u64) {
    with_state(paths, |s| {
        s.highest_issued = s.highest_issued.max(m.issued);
        s.last_check = now;
    });
}

/// Self-tests of one version that may crash before it is blacklisted.
pub const MAX_STRIKES: u32 = 3;

/// Read-modify-write the state under the lock ([`LOCK_WAIT`]). Does nothing if the
/// lock is busy or the state is busy or corrupt.
fn with_state(paths: &Paths, f: impl FnOnce(&mut State)) {
    let Ok(_l) = lock_within(paths, LOCK_WAIT) else {
        return;
    };
    let Some(mut s) = state::load(&paths.state).usable() else {
        return;
    };
    f(&mut s);
    let _ = state::save(&paths.state, &s);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupOutcome {
    Ok,
    /// State was unreadable and has been reset; nothing deleted.
    Fresh,
    /// `bad` failed to become healthy in three launches; `to` is back in place.
    /// The caller relaunches the exe and exits.
    RolledBack {
        bad: String,
        to: String,
    },
    /// Another instance held the lock for 2 s; skipped this launch.
    LockBusy,
    /// The state file exists but could not be read; no housekeeping this launch.
    StateBusy,
    /// The install folder is not writable; the updater stays off.
    NotWritable(String),
}

/// Launches of a new version without a healthy confirm before it is rolled back.
pub const MAX_BOOTS: u32 = 3;

/// Launch-time housekeeping: remove leftovers, count boots of a pending update and
/// roll it back after [`MAX_BOOTS`] unhealthy launches.
pub fn startup(paths: &Paths, running: &str) -> StartupOutcome {
    // The swap writes beside the exe: check that folder takes new files.
    if let Err(e) = probe_writable(paths) {
        return StartupOutcome::NotWritable(e.to_string());
    }
    let f = match fs::create_dir_all(&paths.state_dir).and_then(|()| lock_file(paths)) {
        Ok(f) => f,
        Err(e) => return StartupOutcome::NotWritable(e.to_string()),
    };
    let Ok(_lock) = wait_lock(f, Duration::from_secs(2)) else {
        return StartupOutcome::LockBusy;
    };
    migrate_legacy(paths);

    let mut s = match state::load(&paths.state) {
        Loaded::Ok(s) => s,
        Loaded::Missing => State::default(),
        // A passing IO error must not wipe the ratchet, blacklist or rollback copy.
        Loaded::Busy(_) => return StartupOutcome::StateBusy,
        Loaded::Corrupt => {
            // Delete nothing. Keep a rollback copy alive until a healthy confirm.
            let pending = paths.old.exists().then(|| Pending {
                version: running.to_string(),
                boots: 1,
                from: "0.0.0".to_string(),
            });
            let fresh = State {
                installed: running.to_string(),
                pending,
                ..State::default()
            };
            let _ = state::save(&paths.state, &fresh);
            return StartupOutcome::Fresh;
        }
    };
    remove_leftovers(paths);
    match s.pending.take() {
        Some(mut p) if p.version == running => {
            p.boots += 1;
            if p.boots >= MAX_BOOTS && !paths.old.exists() {
                // No rollback copy: rolling back can never happen. Give up on this
                // version so the next release can install, and keep running.
                tracing::warn!(
                    "update: {} never became healthy and there is no rollback copy",
                    p.version
                );
                s.mark_bad(&p.version);
                s.installed = running.to_string();
                let _ = state::save(&paths.state, &s);
                return StartupOutcome::Ok;
            }
            if p.boots >= MAX_BOOTS && roll_back(paths) {
                s.mark_bad(&p.version);
                s.installed = p.from.clone();
                let _ = state::save(&paths.state, &s);
                return StartupOutcome::RolledBack {
                    bad: p.version,
                    to: p.from,
                };
            }
            // Not yet, or the rollback failed: retry next launch.
            s.pending = Some(p);
        }
        // The swap never landed or was undone.
        Some(_) => {}
        None => {
            let _ = fs::remove_file(&paths.old);
            let _ = fs::remove_file(&paths.bad);
        }
    }
    s.installed = running.to_string();
    let _ = state::save(&paths.state, &s);
    StartupOutcome::Ok
}

/// Create and delete `<stem>.<pid>.probe` beside the exe.
fn probe_writable(paths: &Paths) -> io::Result<()> {
    let p = paths
        .dir
        .join(format!("{}.{}.probe", paths.stem, std::process::id()));
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&p)?;
    let _ = fs::remove_file(&p);
    Ok(())
}

/// Older builds kept the state and lock beside the exe. Move the state into the
/// per-user folder (only if none is there yet) and drop the old lock. Called with the
/// update lock held. Failures leave the old file for the next launch.
fn migrate_legacy(paths: &Paths) {
    if paths.legacy_state.exists() && !paths.state.exists() {
        let moved = fs::rename(&paths.legacy_state, &paths.state).is_ok()
            // Different volume (exe on D:, profile on C:): copy, then delete.
            || (fs::copy(&paths.legacy_state, &paths.state).is_ok()
                && fs::remove_file(&paths.legacy_state).is_ok());
        if !moved {
            tracing::warn!(
                "update: could not move {} to {}",
                paths.legacy_state.display(),
                paths.state.display()
            );
        }
    } else if paths.state.exists() {
        let _ = fs::remove_file(&paths.legacy_state);
    }
    // Another old-version instance may still hold it; then removal fails harmlessly.
    let _ = fs::remove_file(&paths.legacy_lock);
}

/// exe → bad, old → exe. Returns false (and undoes the first move) on failure.
fn roll_back(paths: &Paths) -> bool {
    let _ = fs::remove_file(&paths.bad);
    if fs::rename(&paths.exe, &paths.bad).is_err() {
        return false;
    }
    if fs::rename(&paths.old, &paths.exe).is_err() {
        let _ = fs::rename(&paths.bad, &paths.exe);
        return false;
    }
    true
}

/// Delete this exe's `<stem>.<pid>.download`, `.new.exe` and `.probe` leftovers whose
/// process is gone. Files of a live process (including this one) are skipped: std opens
/// files with `FILE_SHARE_DELETE`, so deleting another instance's in-progress download
/// would succeed and waste its cycle.
fn remove_leftovers(paths: &Paths) {
    let Ok(rd) = fs::read_dir(&paths.dir) else {
        return;
    };
    let prefix = format!("{}.", paths.stem);
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let mid = rest
            .strip_suffix(".download")
            .or_else(|| rest.strip_suffix(".new.exe"))
            .or_else(|| rest.strip_suffix(".probe"));
        let Some(pid) = mid
            .filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|m| m.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || super::pid_alive(pid) {
            continue;
        }
        let _ = fs::remove_file(e.path());
    }
}

/// The running version has stayed up: clear its pending marker. Waits at most `max`
/// for the update lock. Returns true when it cleared the marker: update checks were
/// skipped while it was set, so the caller should ask for one now.
pub fn confirm_healthy(paths: &Paths, running: &str, max: Duration) -> bool {
    let Ok(_l) = lock_within(paths, max) else {
        return false;
    };
    let Some(mut s) = state::load(&paths.state).usable() else {
        return false;
    };
    if s.pending.as_ref().is_some_and(|p| p.version == running) {
        s.pending = None;
        return state::save(&paths.state, &s).is_ok();
    }
    false
}

/// Start the detached `vc-update` thread. It never blocks shutdown: only the swap
/// itself is guarded (see [`SWAP_GUARD`]).
#[cfg(windows)]
pub fn spawn_background(paths: Paths) {
    let Some(channel) = super::CHANNEL else {
        return;
    };
    let r = std::thread::Builder::new()
        .name("vc-update".into())
        .spawn(move || run_loop(paths, channel));
    if let Err(e) = r {
        tracing::warn!("update: could not start thread: {e}");
    }
}

#[cfg(windows)]
fn run_loop(paths: Paths, channel: &'static str) {
    use super::{RUNNING, Status, set_status};
    let keys = super::keys();
    let fetch = super::http::WinHttp::production();
    // First check 5–7 s after start.
    wait(Duration::from_secs(5 + random_below(3)));
    loop {
        set_status(Status::Checking);
        let out = check_and_stage(
            &paths,
            &fetch,
            &keys,
            super::now_unix(),
            RUNNING,
            channel,
            &super::selftest::run,
        );
        match out {
            Outcome::Staged(v) => {
                tracing::info!("update: {v} installed, it runs from the next launch");
                set_status(Status::Ready(v));
                return;
            }
            Outcome::Skip(why) => {
                tracing::debug!("update: {why}");
                // Another instance may already have installed a newer version.
                let s = state::load(&paths.state).usable().unwrap_or_default();
                if parse_version(&s.installed) > parse_version(RUNNING) {
                    set_status(Status::Ready(s.installed));
                    return;
                }
                set_status(Status::UpToDate);
            }
            Outcome::Rejected(e) => {
                tracing::warn!("update rejected: {e}");
                set_status(Status::Failed(e));
            }
            Outcome::Failed(e) => {
                tracing::info!("update check failed: {e}");
                set_status(Status::Failed(e));
            }
        }
        // 1 min ± 5 s.
        wait(Duration::from_secs(55 + random_below(10)));
    }
}

/// Sleep for `d`, or until [`super::request_check_now`].
#[cfg(windows)]
fn wait(d: Duration) {
    let (flag, cv) = &super::WAKE;
    let deadline = Instant::now() + d;
    let mut g = flag.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if *g {
            *g = false;
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        g = cv
            .wait_timeout(g, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

#[cfg(windows)]
fn random_below(n: u64) -> u64 {
    let mut b = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    u64::from_le_bytes(b) % n.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::tests::{manifest, test_key};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    struct FakeFetch {
        files: HashMap<String, Vec<u8>>,
        log: RefCell<Vec<String>>,
    }

    impl Fetch for FakeFetch {
        fn get(
            &self,
            path: &str,
            max_len: u64,
            sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
        ) -> anyhow::Result<u64> {
            self.log.borrow_mut().push(path.to_string());
            let body = self
                .files
                .get(path)
                .ok_or_else(|| anyhow::anyhow!("HTTP 404"))?;
            let mut total = 0u64;
            for c in body.chunks(7) {
                total += c.len() as u64;
                if total > max_len {
                    anyhow::bail!("body over limit");
                }
                sink(c)?;
            }
            Ok(total)
        }
    }

    const NEW: &[u8] = b"NEW EXE BYTES, version 0.2.0";
    const OLD: &[u8] = b"OLD EXE";

    struct Env {
        _d: tempfile::TempDir,
        paths: Paths,
        keys: Vec<[u8; 32]>,
        fetch: FakeFetch,
    }

    fn env_with(exe_bytes: &[u8], tweak: impl FnOnce(&mut manifest::Manifest)) -> Env {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("vc-client.exe");
        fs::write(&exe, OLD).unwrap();
        // Never the real per-user folder in tests.
        let state_dir = d.path().join("appdata");
        fs::create_dir(&state_dir).unwrap();
        let paths = Paths::new(&exe, &state_dir).unwrap();
        let (kp, pk) = test_key();
        let mut m = manifest("0.2.0");
        m.size = NEW.len() as u64;
        m.sha256 = crate::update::to_hex(ring::digest::digest(&ring::digest::SHA256, NEW).as_ref());
        tweak(&mut m);
        let signed = manifest::sign(&kp, &serde_json::to_vec(&m).unwrap());
        let mut files = HashMap::new();
        files.insert("/stable/latest.signed".to_string(), signed);
        files.insert(
            "/stable/vc-client-0.2.0-x86_64.exe".to_string(),
            exe_bytes.to_vec(),
        );
        Env {
            _d: d,
            paths,
            keys: vec![pk],
            fetch: FakeFetch {
                files,
                log: RefCell::new(Vec::new()),
            },
        }
    }

    fn env() -> Env {
        env_with(NEW, |_| {})
    }

    fn pass(exe: &Path, v: &str, ch: &str) -> SelfTest {
        assert_eq!(
            fs::read(exe).unwrap(),
            NEW,
            "self-test runs the verified file"
        );
        assert_eq!((v, ch), ("0.2.0", "stable"));
        SelfTest::Pass
    }

    fn run(e: &Env, st: SelfTestFn) -> Outcome {
        check_and_stage(&e.paths, &e.fetch, &e.keys, 1500, "0.1.0", "stable", st)
    }

    fn leftovers(e: &Env) -> Vec<String> {
        fs::read_dir(&e.paths.dir)
            .unwrap()
            .flatten()
            .map(|x| x.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".download") || n.ends_with(".new.exe"))
            .collect()
    }

    fn assert_untouched(e: &Env) {
        assert_eq!(fs::read(&e.paths.exe).unwrap(), OLD);
        assert!(!e.paths.old.exists());
        assert!(leftovers(e).is_empty(), "{:?}", leftovers(e));
    }

    #[test]
    fn happy_path_swaps_and_records_pending() {
        let e = env();
        assert_eq!(run(&e, &pass), Outcome::Staged("0.2.0".into()));
        assert_eq!(fs::read(&e.paths.exe).unwrap(), NEW);
        assert_eq!(fs::read(&e.paths.old).unwrap(), OLD);
        assert!(leftovers(&e).is_empty());
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.installed, "0.2.0");
        assert_eq!(s.highest_issued, 1000);
        assert_eq!(
            s.pending,
            Some(Pending {
                version: "0.2.0".into(),
                boots: 0,
                from: "0.1.0".into()
            })
        );
    }

    #[test]
    fn second_check_after_swap_skips_without_download() {
        let e = env();
        assert_eq!(run(&e, &pass), Outcome::Staged("0.2.0".into()));
        e.fetch.log.borrow_mut().clear();
        // The running process is still 0.1.0: the `installed` field stops a re-stage.
        assert_eq!(run(&e, &pass), Outcome::Skip("up to date"));
        // Even after the pending marker clears.
        confirm_healthy(&e.paths, "0.2.0", LOCK_WAIT);
        assert_eq!(run(&e, &pass), Outcome::Skip("up to date"));
        assert!(
            e.fetch
                .log
                .borrow()
                .iter()
                .all(|p| p.ends_with("latest.signed")),
            "{:?}",
            e.fetch.log.borrow()
        );
    }

    #[test]
    fn hash_mismatch_leaves_install_unchanged() {
        let mut bad = NEW.to_vec();
        bad[0] ^= 1;
        let e = env_with(&bad, |_| {});
        let never = |_: &Path, _: &str, _: &str| -> SelfTest { panic!("must not self-test") };
        assert!(matches!(run(&e, &never), Outcome::Rejected(r) if r.contains("SHA-256")));
        assert_untouched(&e);
    }

    #[test]
    fn body_over_size_aborted() {
        let mut long = NEW.to_vec();
        long.extend_from_slice(b"extra");
        let e = env_with(&long, |_| {});
        let never = |_: &Path, _: &str, _: &str| -> SelfTest { panic!("must not self-test") };
        assert!(matches!(run(&e, &never), Outcome::Failed(r) if r.contains("over limit")));
        assert_untouched(&e);
        // Short body is rejected too.
        let e = env_with(&NEW[..5], |_| {});
        assert!(matches!(run(&e, &never), Outcome::Rejected(r) if r.contains("bytes")));
        assert_untouched(&e);
    }

    #[test]
    fn bad_manifest_rejected() {
        let e = env_with(NEW, |m| m.channel = "beta".into());
        assert!(matches!(run(&e, &pass), Outcome::Rejected(_)));
        assert_untouched(&e);
        let mut e = env();
        e.keys = vec![test_key().1];
        assert!(matches!(run(&e, &pass), Outcome::Rejected(r) if r.contains("signature")));
        assert_untouched(&e);
    }

    #[test]
    fn selftest_fail_blacklists() {
        let e = env();
        let fail = |_: &Path, _: &str, _: &str| SelfTest::Fail("printed junk".into());
        assert!(matches!(run(&e, &fail), Outcome::Rejected(_)));
        assert_untouched(&e);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.bad_versions, vec!["0.2.0".to_string()]);
        assert_eq!(run(&e, &pass), Outcome::Skip("version blacklisted"));
    }

    #[test]
    fn selftest_transient_does_not_blacklist() {
        let e = env();
        let flaky = |_: &Path, _: &str, _: &str| SelfTest::Transient("timed out".into());
        assert!(matches!(run(&e, &flaky), Outcome::Failed(_)));
        assert_untouched(&e);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert!(s.bad_versions.is_empty());
        assert_eq!(run(&e, &pass), Outcome::Staged("0.2.0".into()));
    }

    #[test]
    fn second_rename_failure_restores_install_and_state() {
        let e = env();
        let before = State {
            installed: "0.1.0".into(),
            highest_issued: 7,
            ..State::default()
        };
        state::save(&e.paths.state, &before).unwrap();
        let calls = Cell::new(0);
        let mut rename = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                Err(io::Error::other("injected"))
            } else {
                fs::rename(a, b)
            }
        };
        let out = check_and_stage_with(
            &e.paths,
            &e.fetch,
            &e.keys,
            1500,
            "0.1.0",
            "stable",
            &pass,
            &mut rename,
        );
        assert!(matches!(out, Outcome::Failed(r) if r.contains("injected")));
        assert_eq!(calls.get(), 3);
        assert_untouched(&e);
        assert_eq!(state::load(&e.paths.state).usable(), Some(before));
    }

    #[test]
    fn first_rename_failure_restores_state() {
        let e = env();
        let mut rename = |_: &Path, _: &Path| Err(io::Error::other("locked"));
        let out = check_and_stage_with(
            &e.paths,
            &e.fetch,
            &e.keys,
            1500,
            "0.1.0",
            "stable",
            &pass,
            &mut rename,
        );
        assert!(matches!(out, Outcome::Failed(_)));
        assert_untouched(&e);
        assert_eq!(state::load(&e.paths.state).usable().unwrap().pending, None);
    }

    #[test]
    fn skip_records_ratchet_and_replay_is_skipped() {
        let e = env_with(NEW, |m| m.version = "0.1.0".into());
        assert_eq!(run(&e, &pass), Outcome::Skip("up to date"));
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!((s.highest_issued, s.last_check), (1000, 1500));
        // A manifest issued earlier than the ratchet is a replay.
        let e2 = env_with(NEW, |m| m.issued = 500);
        state::save(
            &e2.paths.state,
            &State {
                highest_issued: 1000,
                ..State::default()
            },
        )
        .unwrap();
        assert_eq!(run(&e2, &pass), Outcome::Skip("replayed manifest"));
        assert_untouched(&e2);
    }

    #[test]
    fn corrupt_state_means_no_action() {
        let e = env();
        fs::write(&e.paths.state, b"garbage").unwrap();
        assert_eq!(run(&e, &pass), Outcome::Skip("update state unreadable"));
        assert_untouched(&e);
        assert!(e.fetch.log.borrow().is_empty());
    }

    // ---- startup ----

    fn staged() -> Env {
        let e = env();
        assert_eq!(run(&e, &pass), Outcome::Staged("0.2.0".into()));
        e
    }

    #[test]
    fn boots_one_and_two_keep_new_exe_then_three_rolls_back() {
        let e = staged();
        for boot in 1..=2 {
            assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
            assert_eq!(fs::read(&e.paths.exe).unwrap(), NEW);
            assert!(
                e.paths.old.exists(),
                "boot {boot} must keep the rollback copy"
            );
            assert_eq!(
                state::load(&e.paths.state)
                    .usable()
                    .unwrap()
                    .pending
                    .unwrap()
                    .boots,
                boot
            );
        }
        assert_eq!(
            startup(&e.paths, "0.2.0"),
            StartupOutcome::RolledBack {
                bad: "0.2.0".into(),
                to: "0.1.0".into()
            }
        );
        assert_eq!(fs::read(&e.paths.exe).unwrap(), OLD);
        assert_eq!(fs::read(&e.paths.bad).unwrap(), NEW);
        assert!(!e.paths.old.exists());
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.bad_versions, vec!["0.2.0".to_string()]);
        assert_eq!(s.pending, None);
        assert_eq!(s.installed, "0.1.0");
        // Never fetched again.
        assert_eq!(run(&e, &pass), Outcome::Skip("version blacklisted"));
        // Next launch of the old exe cleans up the bad copy.
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        assert!(!e.paths.bad.exists());
    }

    #[test]
    fn healthy_confirm_then_next_launch_deletes_old() {
        let e = staged();
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        assert!(
            !confirm_healthy(&e.paths, "0.1.0", LOCK_WAIT),
            "another version's confirm clears nothing"
        );
        assert!(
            confirm_healthy(&e.paths, "0.2.0", LOCK_WAIT),
            "clearing the marker asks for a check"
        );
        assert_eq!(state::load(&e.paths.state).usable().unwrap().pending, None);
        assert!(
            !confirm_healthy(&e.paths, "0.2.0", LOCK_WAIT),
            "nothing left to clear"
        );
        assert!(e.paths.old.exists(), "confirm itself deletes nothing");
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        assert!(!e.paths.old.exists());
        assert_eq!(fs::read(&e.paths.exe).unwrap(), NEW);
    }

    #[test]
    fn corrupt_state_at_startup_deletes_nothing() {
        let e = staged();
        fs::write(&e.paths.state, b"{{{").unwrap();
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Fresh);
        assert!(e.paths.old.exists());
        assert_eq!(fs::read(&e.paths.exe).unwrap(), NEW);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.installed, "0.2.0");
        // The rollback copy is protected by a pending marker until a healthy confirm.
        assert_eq!(
            s.pending,
            Some(Pending {
                version: "0.2.0".into(),
                boots: 1,
                from: "0.0.0".into()
            })
        );
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        assert!(e.paths.old.exists());
        confirm_healthy(&e.paths, "0.2.0", LOCK_WAIT);
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        assert!(!e.paths.old.exists());
    }

    #[test]
    fn corrupt_state_without_old_has_no_pending() {
        let e = env();
        fs::write(&e.paths.state, b"{{{").unwrap();
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Fresh);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!((s.installed.as_str(), s.pending), ("0.1.0", None));
    }

    #[test]
    fn unreadable_state_means_state_busy_and_no_housekeeping() {
        let e = staged();
        let before = fs::read(&e.paths.state).unwrap();
        // Make the state path unreadable as a file (a directory), keeping the real
        // state aside to check it is untouched afterwards.
        let aside = e.paths.dir.join("aside.json");
        fs::rename(&e.paths.state, &aside).unwrap();
        fs::create_dir(&e.paths.state).unwrap();
        let dead = e.paths.dir.join("vc-client.4294967288.download");
        fs::write(&dead, b"x").unwrap();
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::StateBusy);
        assert!(e.paths.old.exists());
        assert!(
            dead.exists(),
            "no housekeeping while the state is unreadable"
        );
        assert!(e.paths.state.is_dir());
        // And background work skips too.
        assert_eq!(run(&e, &pass), Outcome::Skip("update state unreadable"));
        fs::remove_dir(&e.paths.state).unwrap();
        fs::rename(&aside, &e.paths.state).unwrap();
        assert_eq!(fs::read(&e.paths.state).unwrap(), before);
    }

    #[test]
    fn unhealthy_version_without_rollback_copy_is_given_up() {
        let e = env();
        state::save(
            &e.paths.state,
            &State {
                installed: "0.2.0".into(),
                pending: Some(Pending {
                    version: "0.2.0".into(),
                    boots: 2,
                    from: "0.1.0".into(),
                }),
                ..State::default()
            },
        )
        .unwrap();
        assert!(!e.paths.old.exists());
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.pending, None);
        assert_eq!(s.bad_versions, vec!["0.2.0".to_string()]);
        assert_eq!(s.installed, "0.2.0");
    }

    #[test]
    fn crashed_selftest_needs_three_strikes() {
        let e = env();
        let crash = |_: &Path, _: &str, _: &str| SelfTest::Crashed("exit 1".into());
        for n in 1..MAX_STRIKES {
            assert!(matches!(run(&e, &crash), Outcome::Failed(_)), "cycle {n}");
            assert_untouched(&e);
            let s = state::load(&e.paths.state).usable().unwrap();
            assert!(s.bad_versions.is_empty(), "cycle {n}");
            assert_eq!(s.selftest_strikes.unwrap().count, n);
        }
        assert!(matches!(run(&e, &crash), Outcome::Rejected(_)));
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.bad_versions, vec!["0.2.0".to_string()]);
        assert_eq!(s.selftest_strikes, None);
    }

    #[test]
    fn passing_selftest_clears_strikes() {
        let e = env();
        let crash = |_: &Path, _: &str, _: &str| SelfTest::Crashed("exit 1".into());
        assert!(matches!(run(&e, &crash), Outcome::Failed(_)));
        // Strikes for another version are reset, not added to.
        let mut s = state::load(&e.paths.state).usable().unwrap();
        s.selftest_strikes = Some(Strikes {
            version: "0.1.5".into(),
            count: 2,
        });
        state::save(&e.paths.state, &s).unwrap();
        assert!(matches!(run(&e, &crash), Outcome::Failed(_)));
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.selftest_strikes.as_ref().unwrap().count, 1);
        assert_eq!(run(&e, &pass), Outcome::Staged("0.2.0".into()));
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.selftest_strikes, None);
        assert!(s.bad_versions.is_empty());
    }

    #[test]
    fn busy_lock_fails_swap_without_holding_swap_guard() {
        let e = env();
        let holder = lock_file(&e.paths).unwrap();
        holder.lock().unwrap();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d2 = done.clone();
        // While the swap waits for the file lock, the exit guard must stay free.
        let probe = std::thread::spawn(move || {
            let (mut tries, mut free) = (0, 0);
            std::thread::sleep(Duration::from_millis(500));
            while !d2.load(std::sync::atomic::Ordering::SeqCst) {
                tries += 1;
                if SWAP_GUARD.try_lock().is_ok() {
                    free += 1;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            (tries, free)
        });
        let start = Instant::now();
        let out = run(&e, &pass);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let waited = start.elapsed();
        assert_eq!(out, Outcome::Failed("update lock busy".into()));
        assert!(
            waited >= LOCK_WAIT && waited < LOCK_WAIT + Duration::from_secs(5),
            "{waited:?}"
        );
        assert_untouched(&e);
        let (tries, free) = probe.join().unwrap();
        // Other tests may take the guard for an instant; allow one miss.
        assert!(tries >= 5 && free + 1 >= tries, "{free}/{tries}");
        drop(holder);
    }

    #[test]
    fn live_pid_leftovers_survive_dead_ones_go() {
        let e = env();
        let dir = &e.paths.dir;
        let mine = dir.join(format!("vc-client.{}.download", std::process::id()));
        let dead = dir.join("vc-client.4294967290.download");
        fs::write(&mine, b"x").unwrap();
        fs::write(&dead, b"x").unwrap();
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        assert!(mine.exists(), "a live process's download must survive");
        assert!(!dead.exists());
        #[cfg(windows)]
        {
            assert!(crate::update::pid_alive(std::process::id()));
            assert!(!crate::update::pid_alive(4_294_967_290));
        }
    }

    #[test]
    fn pending_for_other_version_is_cleared() {
        let e = staged();
        // The old exe launched instead (e.g. the swap was undone by hand).
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        let s = state::load(&e.paths.state).usable().unwrap();
        assert_eq!(s.pending, None);
        assert_eq!(s.installed, "0.1.0");
        assert!(
            e.paths.old.exists(),
            "only a pending-free launch deletes .old"
        );
    }

    #[test]
    fn startup_removes_only_numbered_leftovers() {
        let e = env();
        let dir = &e.paths.dir;
        for n in [
            // Pids that cannot exist (Windows pids are multiples of 4 below 2^32 - 8).
            "vc-client.4294967288.download",
            "vc-client.4294967292.new.exe",
            "vc-client.x.download",
            "other.1.download",
            "vc-client.new.exe",
        ] {
            fs::write(dir.join(n), b"x").unwrap();
        }
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        assert!(!dir.join("vc-client.4294967288.download").exists());
        assert!(!dir.join("vc-client.4294967292.new.exe").exists());
        assert!(dir.join("vc-client.x.download").exists());
        assert!(dir.join("other.1.download").exists());
        assert!(dir.join("vc-client.new.exe").exists());
        assert_eq!(fs::read(&e.paths.exe).unwrap(), OLD);
    }

    #[test]
    fn unwritable_folder_turns_updater_off() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(
            &d.path().join("missing-dir").join("vc-client.exe"),
            &d.path().join("appdata"),
        )
        .unwrap();
        assert!(matches!(
            startup(&p, "0.1.0"),
            StartupOutcome::NotWritable(_)
        ));
    }

    #[test]
    fn nothing_but_swap_files_lands_beside_the_exe() {
        let e = staged();
        assert_eq!(startup(&e.paths, "0.2.0"), StartupOutcome::Ok);
        confirm_healthy(&e.paths, "0.2.0", LOCK_WAIT);
        let mut beside: Vec<String> = fs::read_dir(&e.paths.dir)
            .unwrap()
            .flatten()
            .filter(|x| x.path().is_file())
            .map(|x| x.file_name().to_string_lossy().into_owned())
            .collect();
        beside.sort();
        assert_eq!(beside, vec!["vc-client.exe", "vc-client.old.exe"]);
        assert!(e.paths.state.exists() && e.paths.lock.exists());
    }

    #[test]
    fn legacy_state_beside_exe_is_moved() {
        let e = env();
        let old = State {
            installed: "0.1.0".into(),
            highest_issued: 42,
            bad_versions: vec!["0.1.5".into()],
            ..State::default()
        };
        state::save(&e.paths.legacy_state, &old).unwrap();
        fs::write(&e.paths.legacy_lock, b"").unwrap();
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        assert!(!e.paths.legacy_state.exists());
        assert!(!e.paths.legacy_lock.exists());
        assert_eq!(state::load(&e.paths.state).usable(), Some(old.clone()));

        // A stale legacy file never overwrites newer per-user state.
        state::save(&e.paths.legacy_state, &State::default()).unwrap();
        assert_eq!(startup(&e.paths, "0.1.0"), StartupOutcome::Ok);
        assert!(!e.paths.legacy_state.exists());
        assert_eq!(
            state::load(&e.paths.state).usable().unwrap().highest_issued,
            42
        );
    }

    #[test]
    fn max_version_picks_higher() {
        assert_eq!(max_version("0.1.0", "0.2.0"), "0.2.0");
        assert_eq!(max_version("0.3.0", "0.2.0"), "0.3.0");
        assert_eq!(max_version("0.1.0", ""), "0.1.0");
    }
}
