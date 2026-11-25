//! On-disk profile: `servers.toml` and `settings.toml`.

use std::collections::BTreeMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::ServerKey;
use crate::capture::CaptureCfg;
use crate::media::{FRAME_MAX_H, FRAME_QUALITY, SCREEN_DISPLAY_MAX_SIDE};
use crate::probe::ProbeResult;

/// Frame rates offered for screen sharing.
pub const SCREEN_FPS_CHOICES: [u32; 10] = [1, 2, 5, 10, 15, 20, 30, 60, 90, 120];
/// Upload resolutions offered for screen sharing (height; 0 = native).
pub const SCREEN_HEIGHT_CHOICES: [u32; 5] = [480, 720, 1080, 1440, 0];
pub const SCREEN_FPS_MAX: u32 = vc_proto::SCREEN_FPS_LIMIT;
pub const SCREEN_QUALITY_MIN: u8 = 20;
pub const SCREEN_QUALITY_MAX: u8 = 95;

/// Label of a resolution choice (height; 0 = native).
pub fn screen_height_label(h: u32) -> String {
    match h {
        0 => format!("Native (max {SCREEN_DISPLAY_MAX_SIDE})"),
        h => format!("{h}p"),
    }
}

/// Time between grabs for a frame rate (clamped to 1-120), to the microsecond.
pub fn screen_interval(fps: u32) -> std::time::Duration {
    std::time::Duration::from_secs_f64(1.0 / fps.clamp(1, SCREEN_FPS_MAX) as f64)
}

/// Apply a server's caps (0 = no cap) to a frame rate and height setting. A
/// height cap replaces native (0) and any larger height.
pub fn clamp_screen(fps: u32, height: u32, cap_fps: u32, cap_h: u32) -> (u32, u32) {
    let fps = if cap_fps > 0 { fps.min(cap_fps) } else { fps };
    let height = if cap_h > 0 && (height == 0 || height > cap_h) {
        cap_h
    } else {
        height
    };
    (fps, height)
}

/// Which screen-share encoder to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ScreenEncoderPref {
    /// Hardware H.264 when the GPU has an encoder and every watcher decodes it.
    #[default]
    Auto,
    /// Always CPU JPEG.
    Jpeg,
}

/// Box a frame is fitted inside for a resolution setting: 16:9 boxes for the
/// listed heights ("fits in W×H"), a 2560 square for native, 720p otherwise.
pub fn screen_box(height: u32) -> (u32, u32) {
    match height {
        480 | 720 | 1080 | 1440 => ((height * 16).div_ceil(9), height),
        0 => (SCREEN_DISPLAY_MAX_SIDE, SCREEN_DISPLAY_MAX_SIDE),
        _ => screen_box(FRAME_MAX_H),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedServer {
    pub key: ServerKey,
    pub label: String,
    pub addr: String,
    pub nick: String,
    #[serde(default)]
    pub password: Option<String>,
    /// Pinned certificate fingerprint (hex).
    #[serde(default)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InputMode {
    #[default]
    Vad,
    Ptt,
    Open,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            InputMode::Vad => "Voice activity",
            InputMode::Ptt => "Push-to-talk",
            InputMode::Open => "Always on",
        }
    }

    pub const ALL: [InputMode; 3] = [InputMode::Vad, InputMode::Ptt, InputMode::Open];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_mode: InputMode,
    pub vad_threshold_db: f32,
    pub ptt_key: String,
    pub master_volume: f32,
    /// Default nickname for new servers.
    pub nickname: String,
    /// Per-user volume (0.0-2.0), keyed by `"<server key>/<nick>"`.
    pub user_volumes: BTreeMap<String, f32>,
    /// Per-stream sound volume (0.0-2.0), keyed `<server key>/<nick>`.
    pub stream_volumes: BTreeMap<String, f32>,
    /// Screen-share frames per second (clamped to 1-120).
    pub screen_fps: u32,
    /// Screen-share upload height (480/720/1080/1440; 0 = native up to 2560).
    pub screen_height: u32,
    /// Screen-share JPEG quality (clamped to 20-95).
    pub screen_quality: u8,
    /// The last "Test max" result, if any.
    pub screen_tested: Option<ProbeResult>,
    /// Share my system audio ("stream sound") with my watchers.
    pub screen_audio: bool,
    /// Hardware H.264 when available, or always JPEG.
    pub screen_encoder: ScreenEncoderPref,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            input_mode: InputMode::Vad,
            vad_threshold_db: -45.0,
            ptt_key: "Space".into(),
            master_volume: 1.0,
            nickname: String::new(),
            user_volumes: BTreeMap::new(),
            stream_volumes: BTreeMap::new(),
            screen_fps: 10,
            screen_height: FRAME_MAX_H,
            screen_quality: FRAME_QUALITY,
            screen_tested: None,
            screen_audio: false,
            screen_encoder: ScreenEncoderPref::Auto,
        }
    }
}

pub fn user_volume_key(key: ServerKey, nick: &str) -> String {
    format!("{key}/{nick}")
}

impl Settings {
    pub fn user_volume(&self, key: ServerKey, nick: &str) -> f32 {
        self.user_volumes
            .get(&user_volume_key(key, nick))
            .copied()
            .unwrap_or(1.0)
    }

    pub fn set_user_volume(&mut self, key: ServerKey, nick: &str, v: f32) {
        self.user_volumes
            .insert(user_volume_key(key, nick), v.clamp(0.0, 2.0));
    }

    /// `nick`'s stream sound volume on server `key` (1.0 when never set).
    pub fn stream_volume(&self, key: ServerKey, nick: &str) -> f32 {
        self.stream_volumes
            .get(&user_volume_key(key, nick))
            .copied()
            .unwrap_or(1.0)
    }

    pub fn set_stream_volume(&mut self, key: ServerKey, nick: &str, v: f32) {
        self.stream_volumes
            .insert(user_volume_key(key, nick), v.clamp(0.0, 2.0));
    }

    /// Capture settings for these preferences (always `active`, epoch 0; the app
    /// fills those in from the session).
    pub fn screen_capture_config(&self) -> CaptureCfg {
        let (max_w, max_h) = screen_box(self.screen_height);
        CaptureCfg {
            interval: screen_interval(self.screen_fps),
            max_w,
            max_h,
            quality: self
                .screen_quality
                .clamp(SCREEN_QUALITY_MIN, SCREEN_QUALITY_MAX),
            active: true,
            epoch: 0,
            video: false,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServersFile {
    #[serde(default)]
    servers: Vec<SavedServer>,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub dir: PathBuf,
}

/// Parse `host:port`, resolving names. Returns the first address.
pub fn validate_addr(s: &str) -> Result<SocketAddr> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty address"));
    }
    s.to_socket_addrs()
        .with_context(|| format!("invalid address {s:?}"))?
        .next()
        .ok_or_else(|| anyhow!("address {s:?} did not resolve"))
}

impl Profile {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `--profile` or `<config dir>/vc-client`.
    pub fn from_cli(dir: Option<&Path>) -> Self {
        match dir {
            Some(d) => Self::new(d),
            None => Self::new(
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("vc-client"),
            ),
        }
    }

    fn servers_path(&self) -> PathBuf {
        self.dir.join("servers.toml")
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    pub fn load_servers(&self) -> Result<Vec<SavedServer>> {
        let p = self.servers_path();
        if !p.exists() {
            return Ok(vec![]);
        }
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let f: ServersFile =
            toml::from_str(&text).with_context(|| format!("parse {}", p.display()))?;
        Ok(f.servers)
    }

    pub fn save_servers(&self, servers: &[SavedServer]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = toml::to_string_pretty(&ServersFile {
            servers: servers.to_vec(),
        })?;
        write_atomic(&self.servers_path(), &text)
    }

    pub fn load_settings(&self) -> Result<Settings> {
        let p = self.settings_path();
        if !p.exists() {
            return Ok(Settings::default());
        }
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", p.display()))
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_atomic(&self.settings_path(), &toml::to_string_pretty(s)?)
    }
}

/// Write `text` to `<path>.tmp`, flush it to disk, then rename it over `path`.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    f.write_all(text.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_servers_persist_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = Profile::new(d.path().join("sub"));
        assert!(p.load_servers().unwrap().is_empty());
        let list = vec![
            SavedServer {
                key: 1,
                label: "Home".into(),
                addr: "127.0.0.1:7878".into(),
                nick: "me".into(),
                password: None,
                fingerprint: Some("ab".repeat(32)),
            },
            SavedServer {
                key: 2,
                label: "Work".into(),
                addr: "localhost:9000".into(),
                nick: "me2".into(),
                password: Some("pw".into()),
                fingerprint: None,
            },
        ];
        p.save_servers(&list).unwrap();
        assert_eq!(p.load_servers().unwrap(), list);
        assert_eq!(
            Profile::new(d.path().join("sub")).load_servers().unwrap(),
            list
        );
    }

    #[test]
    fn settings_persist_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = Profile::new(d.path());
        assert_eq!(p.load_settings().unwrap(), Settings::default());
        let s = Settings {
            input_device: Some("{0.0.1.00000000}.{abc}".into()),
            output_device: Some("out-id".into()),
            input_mode: InputMode::Ptt,
            vad_threshold_db: -30.5,
            ptt_key: "F5".into(),
            master_volume: 1.5,
            nickname: "nick".into(),
            user_volumes: BTreeMap::new(),
            stream_volumes: BTreeMap::from([("1/alice".to_string(), 0.5)]),
            screen_fps: 120,
            screen_height: 1440,
            screen_quality: 90,
            screen_tested: Some(ProbeResult {
                height: 1080,
                fps: 60,
                encoder: "CPU (JPEG)".into(),
            }),
            screen_audio: true,
            screen_encoder: ScreenEncoderPref::Jpeg,
        };
        p.save_settings(&s).unwrap();
        assert_eq!(p.load_settings().unwrap(), s);
        // Partial files keep defaults for missing keys.
        std::fs::write(d.path().join("settings.toml"), "ptt_key = \"F1\"\n").unwrap();
        let l = p.load_settings().unwrap();
        assert_eq!(l.ptt_key, "F1");
        assert_eq!(l.vad_threshold_db, -45.0);
        // Old files without the screen-share keys load the defaults.
        assert_eq!(
            (l.screen_fps, l.screen_height, l.screen_quality),
            (10, 720, 60)
        );
        assert_eq!(l.screen_tested, None);
        assert!(!l.screen_audio, "stream sound is off by default");
        assert_eq!(l.screen_encoder, ScreenEncoderPref::Auto);
    }

    #[test]
    fn screen_capture_config_clamps_and_maps() {
        use std::time::Duration;
        let mut s = Settings::default();
        let c = s.screen_capture_config();
        assert_eq!(c.interval, Duration::from_millis(100));
        assert_eq!((c.max_w, c.max_h, c.quality), (1280, 720, 60));
        s.screen_fps = 0;
        assert_eq!(s.screen_capture_config().interval, Duration::from_secs(1));
        s.screen_fps = 100;
        assert_eq!(
            s.screen_capture_config().interval.as_micros(),
            10_000,
            "100 stays 100"
        );
        s.screen_fps = 500;
        assert_eq!(
            s.screen_capture_config().interval,
            screen_interval(120),
            "clamped to 120"
        );
        s.screen_fps = 120;
        assert_eq!(s.screen_capture_config().interval.as_micros(), 8_333);
        assert_eq!(SCREEN_FPS_CHOICES.last(), Some(&120));
        s.screen_quality = 5;
        assert_eq!(s.screen_capture_config().quality, 20);
        s.screen_quality = 255;
        assert_eq!(s.screen_capture_config().quality, 95);
        for (h, b) in [
            (480, (854, 480)),
            (720, (1280, 720)),
            (1080, (1920, 1080)),
            (1440, (2560, 1440)),
            (0, (2560, 2560)),
            (999, (1280, 720)),
        ] {
            s.screen_height = h;
            let c = s.screen_capture_config();
            assert_eq!((c.max_w, c.max_h), b, "{h}");
        }
        assert_eq!(screen_height_label(0), "Native (max 2560)");
        assert_eq!(screen_height_label(1080), "1080p");
    }

    #[test]
    fn clamp_screen_applies_caps() {
        // No caps.
        assert_eq!(clamp_screen(120, 0, 0, 0), (120, 0));
        assert_eq!(clamp_screen(60, 1440, 0, 0), (60, 1440));
        // fps cap only.
        assert_eq!(clamp_screen(120, 1080, 30, 0), (30, 1080));
        assert_eq!(clamp_screen(10, 1080, 30, 0), (10, 1080));
        // Height cap replaces native and larger heights, keeps smaller ones.
        assert_eq!(clamp_screen(30, 0, 0, 720), (30, 720));
        assert_eq!(clamp_screen(30, 1440, 0, 720), (30, 720));
        assert_eq!(clamp_screen(30, 480, 0, 720), (30, 480));
        assert_eq!(clamp_screen(30, 720, 0, 720), (30, 720));
        assert_eq!(clamp_screen(120, 0, 30, 720), (30, 720));
    }

    #[test]
    fn validate_addr_accepts_and_rejects() {
        assert_eq!(
            validate_addr("127.0.0.1:7878").unwrap().to_string(),
            "127.0.0.1:7878"
        );
        assert_eq!(validate_addr(" localhost:7878 ").unwrap().port(), 7878);
        assert!(validate_addr("[::1]:9").is_ok());
        for bad in [
            "",
            "nonsense",
            "127.0.0.1",
            "127.0.0.1:99999",
            "host:port",
            ":::",
        ] {
            assert!(validate_addr(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn user_volume_persist_by_server_and_nick() {
        let d = tempfile::tempdir().unwrap();
        let p = Profile::new(d.path());
        let mut s = Settings::default();
        assert_eq!(s.user_volume(1, "bob"), 1.0);
        s.set_user_volume(1, "bob", 1.5);
        s.set_user_volume(2, "bob", 0.25);
        s.set_user_volume(1, "eve", 9.0);
        p.save_settings(&s).unwrap();
        let l = p.load_settings().unwrap();
        assert_eq!(l.user_volume(1, "bob"), 1.5);
        assert_eq!(l.user_volume(2, "bob"), 0.25);
        assert_eq!(l.user_volume(1, "eve"), 2.0, "clamped");
        assert_eq!(l.user_volume(3, "bob"), 1.0);
    }

    #[test]
    fn stream_volume_persist_by_server_and_nick() {
        let d = tempfile::tempdir().unwrap();
        let p = Profile::new(d.path());
        let mut s = Settings::default();
        assert_eq!(s.stream_volume(1, "bob"), 1.0);
        s.set_stream_volume(1, "bob", 1.5);
        s.set_stream_volume(1, "eve", 9.0);
        s.set_stream_volume(2, "eve", -1.0);
        s.set_user_volume(1, "bob", 0.25);
        p.save_settings(&s).unwrap();
        let l = p.load_settings().unwrap();
        assert_eq!(l.stream_volume(1, "bob"), 1.5);
        assert_eq!(l.stream_volume(1, "eve"), 2.0, "clamped high");
        assert_eq!(l.stream_volume(2, "eve"), 0.0, "clamped low");
        assert_eq!(l.stream_volume(3, "bob"), 1.0);
        // Independent of the voice volume for the same nick.
        assert_eq!(l.user_volume(1, "bob"), 0.25);
        assert_eq!(l.user_volume(1, "eve"), 1.0);
    }
}
