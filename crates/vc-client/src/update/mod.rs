//! Silent, signed auto-updates for the Windows and Linux clients.
//!
//! Trust model: the update host (`voicechat.owl.gay`) is not trusted. Every update is
//! described by `latest.signed`, a manifest signed with an offline Ed25519 key whose
//! public half is compiled into the exe (`update-keys.txt`). The manifest pins the
//! exact SHA-256 and size of the new exe, so a hijacked host, DNS or CA can at worst
//! withhold updates, never run code.
//!
//! Flow: a background thread checks a while after start, downloads and verifies the
//! new exe beside the running one, runs it once with `--update-selftest`, then swaps
//! the files (`exe` → `<stem>.old.exe`, or `<name>.old` on Linux; new → `exe`). The running process carries on
//! untouched and the next launch is the new version. A new version that never reaches
//! a healthy state within three launches is rolled back and blacklisted.
//!
//! The updater is compiled in but inert unless the build set `VC_UPDATE_CHANNEL` and
//! `update-keys.txt` lists at least one key.

pub mod apply;
#[cfg(target_os = "linux")]
pub mod curl;
#[cfg(windows)]
pub mod http;
pub mod manifest;
pub mod selftest;
pub mod state;

use std::env::consts::EXE_SUFFIX;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

pub use manifest::{Manifest, Reject, verify};

/// Release channel baked in at build time (`VC_UPDATE_CHANNEL=stable cargo build ...`).
/// `None` (any plain `cargo build`/`cargo test`) keeps the updater off.
pub const CHANNEL: Option<&str> = option_env!("VC_UPDATE_CHANNEL");

/// Version of the running exe.
pub const RUNNING: &str = env!("CARGO_PKG_VERSION");

/// Host that serves `/{channel}/latest.signed` and `/{channel}/vc-client-*.exe` (and
/// the Linux builds under `/{channel}/linux/`).
pub const HOST: &str = "voicechat.owl.gay";

const SHIPPED_KEYS: &str = include_str!("../../update-keys.txt");

/// Largest exe an update may carry.
pub const MAX_EXE_SIZE: u64 = 256 * 1024 * 1024;

/// Parse a key file: one 64-hex Ed25519 public key per line, `#` comments allowed.
pub fn parse_keys(text: &str) -> Result<Vec<[u8; 32]>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = crate::tofu::hex_decode(line)
            .and_then(|v| <[u8; 32]>::try_from(v).ok())
            .ok_or_else(|| format!("update-keys.txt line {}: not a 64-hex key", i + 1))?;
        out.push(key);
    }
    Ok(out)
}

/// Release public keys compiled into this exe. Empty means the updater is off.
pub fn keys() -> Vec<[u8; 32]> {
    parse_keys(SHIPPED_KEYS).unwrap_or_default()
}

/// Whether the updater may run in this process, or why not.
pub fn enabled(cli_no_update: bool) -> Result<(), &'static str> {
    enabled_with(
        cfg!(any(windows, target_os = "linux")) && manifest::this_target().is_some(),
        CHANNEL,
        keys().len(),
        cli_no_update,
        std::env::var_os("VC_NO_UPDATE").is_some(),
    )
}

fn enabled_with(
    supported: bool,
    channel: Option<&str>,
    keys: usize,
    cli_no_update: bool,
    env_no_update: bool,
) -> Result<(), &'static str> {
    if !supported {
        return Err("not supported on this platform");
    }
    if channel.is_none() {
        return Err("development build");
    }
    if keys == 0 {
        return Err("no release keys in this build");
    }
    if cli_no_update {
        return Err("--no-update");
    }
    if env_no_update {
        return Err("VC_NO_UPDATE is set");
    }
    Ok(())
}

/// Where the updater keeps its files. The exe swap files (`old`, `bad`, downloads)
/// sit beside the exe so the renames stay on one volume. The bookkeeping (`state`,
/// `lock`) lives in a per-user folder so it never clutters wherever the exe was put
/// (a Desktop, Downloads, ...).
#[derive(Debug, Clone)]
pub struct Paths {
    pub exe: PathBuf,
    pub dir: PathBuf,
    pub stem: String,
    pub old: PathBuf,
    pub bad: PathBuf,
    /// Per-user folder holding `state` and `lock`.
    pub state_dir: PathBuf,
    pub state: PathBuf,
    pub lock: PathBuf,
    /// Where older builds kept `state` and `lock`: beside the exe. Migrated at startup.
    pub legacy_state: PathBuf,
    pub legacy_lock: PathBuf,
}

/// `%LOCALAPPDATA%\vc-client\update` (Windows), `~/.local/share/vc-client/update`
/// (Linux).
pub fn default_state_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("vc-client").join("update"))
}

impl Paths {
    /// Paths for `exe` with bookkeeping in [`default_state_dir`].
    pub fn from_exe(exe: &Path) -> Option<Paths> {
        Paths::new(exe, &default_state_dir()?)
    }

    /// Paths for `exe` with bookkeeping in `state_dir`. Those files are keyed by the
    /// exe's full path, so two copies of the client in different folders never share
    /// state.
    pub fn new(exe: &Path, state_dir: &Path) -> Option<Paths> {
        let dir = exe.parent()?.to_path_buf();
        // Windows: `vc-client.exe` → stem `vc-client`. Where binaries have no extension
        // (Linux) the whole name is the stem, so a dotted name such as
        // `vc-client-0.1.7-x86_64-linux` keeps its own swap files.
        let stem = if EXE_SUFFIX.is_empty() {
            exe.file_name()?
        } else {
            exe.file_stem()?
        }
        .to_str()?
        .to_string();
        let f = |suffix: &str| dir.join(format!("{stem}{suffix}"));
        let key = install_key(exe);
        let s = |suffix: &str| state_dir.join(format!("{stem}-{key}{suffix}"));
        Some(Paths {
            exe: exe.to_path_buf(),
            old: f(&format!(".old{EXE_SUFFIX}")),
            bad: f(&format!(".bad{EXE_SUFFIX}")),
            state_dir: state_dir.to_path_buf(),
            state: s(".update.json"),
            lock: s(".update.lock"),
            legacy_state: f(".update.json"),
            legacy_lock: f(".update.lock"),
            dir,
            stem,
        })
    }

    /// Per-process download target, so two instances never write the same file.
    pub fn download(&self, pid: u32) -> PathBuf {
        self.dir.join(format!("{}.{pid}.download", self.stem))
    }

    /// Per-process verified exe awaiting its self-test and the swap.
    pub fn new_exe(&self, pid: u32) -> PathBuf {
        self.dir
            .join(format!("{}.{pid}.new{EXE_SUFFIX}", self.stem))
    }
}

/// First 8 hex of SHA-256 over the exe path. Lowercased on Windows, whose paths are
/// case-insensitive; Linux paths are case-sensitive and used as they are.
fn install_key(exe: &Path) -> String {
    let p = exe.to_string_lossy();
    #[cfg(windows)]
    let p = p.to_lowercase();
    to_hex(&ring::digest::digest(&ring::digest::SHA256, p.as_bytes()).as_ref()[..4])
}

/// Strict `MAJOR.MINOR.PATCH`: no pre-release, build metadata, sign or leading zeros.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.');
    let mut part = || -> Option<u64> {
        let p = it.next()?;
        if p.is_empty()
            || !p.bytes().all(|b| b.is_ascii_digit())
            || (p.len() > 1 && p.starts_with('0'))
        {
            return None;
        }
        p.parse().ok()
    };
    let v = (part()?, part()?, part()?);
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

/// What the updater is doing, shown read-only in Settings → Profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Off(&'static str),
    UpToDate,
    Checking,
    Ready(String),
    Failed(String),
}

fn status_slot() -> &'static Mutex<Option<Status>> {
    static STATUS: OnceLock<Mutex<Option<Status>>> = OnceLock::new();
    STATUS.get_or_init(|| Mutex::new(None))
}

pub fn set_status(s: Status) {
    *status_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
}

pub fn status() -> Option<Status> {
    status_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// One line for Settings, e.g. `Version 0.1.0 · up to date`.
pub fn status_text() -> String {
    status_line(RUNNING, status().as_ref())
}

fn status_line(running: &str, s: Option<&Status>) -> String {
    match s {
        None => format!("Version {running}"),
        Some(Status::Off(why)) => format!("Version {running} · automatic updates off ({why})"),
        Some(Status::UpToDate) => format!("Version {running} · up to date"),
        Some(Status::Checking) => format!("Version {running} · checking for updates"),
        Some(Status::Ready(v)) => {
            format!("Version {running} · update {v} ready, starts next launch")
        }
        Some(Status::Failed(_)) => format!("Version {running} · last update check failed"),
    }
}

/// Text for a protocol mismatch with the server. Also wakes the updater, since a
/// newer server is the case where updating matters most.
pub fn bad_version_text(base: &str) -> String {
    request_check_now();
    bad_version_line(base, status().as_ref())
}

fn bad_version_line(base: &str, s: Option<&Status>) -> String {
    match s {
        Some(Status::Ready(v)) => {
            format!("{base} — update {v} is installed, restart Voicechat to finish updating")
        }
        _ => base.to_string(),
    }
}

/// Wakes the updater thread for an immediate check.
pub(crate) static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

pub fn request_check_now() {
    let (flag, cv) = &WAKE;
    *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
    cv.notify_all();
}

/// Held for the duration of a file swap. `main` takes it before returning so the
/// process never exits between the two renames.
pub static SWAP_GUARD: Mutex<()> = Mutex::new(());

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether process `pid` still exists. Errs on the side of "alive": only a pid the OS
/// positively does not know is reported dead.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut code = 0u32;
                let alive = GetExitCodeProcess(h, &mut code).is_err() || code == STILL_ACTIVE;
                let _ = CloseHandle(h);
                alive
            }
            // The process exists but belongs to someone we may not inspect.
            Err(e) => e.code() == ERROR_ACCESS_DENIED.to_hresult(),
        }
    }
}

/// Linux: a pid is alive while `/proc/<pid>` exists.
#[cfg(target_os = "linux")]
pub fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Never delete another process's files where liveness cannot be checked.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}

/// Lowercase hex.
pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_keys_parse_and_plain_builds_stay_off() {
        let k = parse_keys(SHIPPED_KEYS).expect("update-keys.txt must parse");
        // A primary and an offline backup key, never the same key twice.
        assert!(k.len() >= 2, "ship a primary and a backup release key");
        let mut uniq = k.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), k.len(), "duplicate key in update-keys.txt");
        assert_eq!(keys(), k);
        // Empty keys switch the updater off even with a channel set.
        assert_eq!(
            enabled_with(true, Some("stable"), 0, false, false),
            Err("no release keys in this build")
        );
        // Test and dev builds have no channel compiled in, so they never self-update.
        if CHANNEL.is_none() {
            assert!(enabled(false).is_err());
        }
    }

    #[test]
    fn enabled_reasons() {
        assert_eq!(enabled_with(true, Some("stable"), 1, false, false), Ok(()));
        assert!(enabled_with(false, Some("stable"), 1, false, false).is_err());
        assert_eq!(
            enabled_with(true, None, 1, false, false),
            Err("development build")
        );
        assert_eq!(
            enabled_with(true, Some("stable"), 1, true, false),
            Err("--no-update")
        );
        assert_eq!(
            enabled_with(true, Some("stable"), 1, false, true),
            Err("VC_NO_UPDATE is set")
        );
    }

    #[test]
    fn key_file_parsing() {
        let k = "a".repeat(64);
        let text = format!("# c\n\n  {k}  \n{}\n", "B".repeat(64));
        assert_eq!(parse_keys(&text).unwrap().len(), 2);
        assert!(parse_keys("abcd").is_err());
        assert!(parse_keys(&"g".repeat(64)).is_err());
        assert!(parse_keys(&"a".repeat(66)).is_err());
    }

    #[test]
    fn version_parser() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("10.20.300"), Some((10, 20, 300)));
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.3-beta",
            "1.2.3+b",
            "+1.2.3",
            "-1.2.3",
            "01.2.3",
            "1.02.3",
            "1..3",
            "1.2.",
            " 1.2.3",
            "a.b.c",
            "1.2.99999999999999999999",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(parse_version("0.1.10") > parse_version("0.1.9"));
    }

    #[cfg(windows)]
    #[test]
    fn paths_derive_from_exe_name() {
        let state = Path::new(r"C:\Users\u\AppData\Local\vc-client\update");
        let p = Paths::new(Path::new(r"C:\Apps\Voicechat\vc-client.exe"), state).unwrap();
        assert_eq!(p.old, Path::new(r"C:\Apps\Voicechat\vc-client.old.exe"));
        assert_eq!(p.bad, Path::new(r"C:\Apps\Voicechat\vc-client.bad.exe"));
        // Bookkeeping goes to the per-user folder, never beside the exe.
        assert_eq!(p.state.parent(), Some(state));
        assert_eq!(p.lock.parent(), Some(state));
        let name = p.state.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("vc-client-") && name.ends_with(".update.json"),
            "{name}"
        );
        assert_eq!(
            p.legacy_state,
            Path::new(r"C:\Apps\Voicechat\vc-client.update.json")
        );
        assert_eq!(
            p.legacy_lock,
            Path::new(r"C:\Apps\Voicechat\vc-client.update.lock")
        );
        // The same install in any case shares state; another folder does not.
        let same = Paths::new(Path::new(r"c:\apps\voicechat\vc-client.exe"), state).unwrap();
        assert_eq!(same.state, p.state);
        let other = Paths::new(Path::new(r"C:\Users\u\Desktop\vc-client.exe"), state).unwrap();
        assert_ne!(other.state, p.state);
        assert_eq!(
            p.download(7),
            Path::new(r"C:\Apps\Voicechat\vc-client.7.download")
        );
        assert_eq!(
            p.new_exe(7),
            Path::new(r"C:\Apps\Voicechat\vc-client.7.new.exe")
        );
    }

    #[cfg(unix)]
    #[test]
    fn paths_derive_from_exe_name_unix() {
        let state = Path::new("/home/u/.local/share/vc-client/update");
        let p = Paths::new(Path::new("/home/u/apps/vc-client"), state).unwrap();
        assert_eq!(p.stem, "vc-client");
        assert_eq!(p.old, Path::new("/home/u/apps/vc-client.old"));
        assert_eq!(p.bad, Path::new("/home/u/apps/vc-client.bad"));
        assert_eq!(p.state.parent(), Some(state));
        assert_eq!(p.lock.parent(), Some(state));
        let name = p.state.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("vc-client-") && name.ends_with(".update.json"),
            "{name}"
        );
        assert_eq!(
            p.legacy_state,
            Path::new("/home/u/apps/vc-client.update.json")
        );
        // Linux paths are case-sensitive: another case is another install.
        let other_case = Paths::new(Path::new("/home/u/Apps/vc-client"), state).unwrap();
        assert_ne!(other_case.state, p.state);
        assert_eq!(
            p.download(7),
            Path::new("/home/u/apps/vc-client.7.download")
        );
        assert_eq!(p.new_exe(7), Path::new("/home/u/apps/vc-client.7.new"));
    }

    /// A downloaded Linux build that was not renamed keeps its dotted name as the stem,
    /// so it never shares swap files with another version beside it.
    #[cfg(unix)]
    #[test]
    fn dotted_linux_name_is_its_own_stem() {
        let state = Path::new("/tmp/state");
        let p = Paths::new(
            Path::new("/home/u/Downloads/vc-client-0.1.7-x86_64-linux"),
            state,
        )
        .unwrap();
        assert_eq!(p.stem, "vc-client-0.1.7-x86_64-linux");
        assert_eq!(
            p.old,
            Path::new("/home/u/Downloads/vc-client-0.1.7-x86_64-linux.old")
        );
        let q = Paths::new(
            Path::new("/home/u/Downloads/vc-client-0.1.8-x86_64-linux"),
            state,
        )
        .unwrap();
        assert_ne!(p.old, q.old);
        assert_ne!(p.state, q.state);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_alive_uses_proc() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(4_294_967_290));
    }

    #[test]
    fn status_and_bad_version_text() {
        assert_eq!(status_line("0.1.0", None), "Version 0.1.0");
        assert_eq!(
            status_line("0.1.0", Some(&Status::UpToDate)),
            "Version 0.1.0 · up to date"
        );
        assert_eq!(
            status_line("0.1.0", Some(&Status::Ready("0.1.1".into()))),
            "Version 0.1.0 · update 0.1.1 ready, starts next launch"
        );
        assert!(status_line("0.1.0", Some(&Status::Off("x"))).contains("off (x)"));
        let base = "Protocol version mismatch";
        assert_eq!(bad_version_line(base, None), base);
        assert_eq!(bad_version_line(base, Some(&Status::UpToDate)), base);
        assert_eq!(
            bad_version_line(base, Some(&Status::Ready("0.2.0".into()))),
            "Protocol version mismatch — update 0.2.0 is installed, restart Voicechat to finish updating"
        );
    }
}
