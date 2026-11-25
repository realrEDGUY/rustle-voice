//! Server configuration: TOML file plus CLI overrides.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use vc_proto::{MAX_CHANNEL_NAME, MAX_IMAGE_BYTES, SCREEN_FPS_LIMIT, VoiceTuning};

/// Allowed `max_screen_height` values (0 = no cap).
pub const SCREEN_HEIGHT_CAPS: [u32; 5] = [0, 480, 720, 1080, 1440];

/// Server-wide voice latency/smoothness preset. Only its numbers go on the wire
/// (`VoiceTuning`), so presets can change without a client release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoicePreset {
    /// Lowest delay; more dropouts on jittery links.
    Competitive,
    /// Today's behaviour.
    #[default]
    Balanced,
    /// More delay; rides out wifi jitter.
    Smooth,
}

impl VoicePreset {
    /// Lowercase name, as written in `server.toml`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Competitive => "competitive",
            Self::Balanced => "balanced",
            Self::Smooth => "smooth",
        }
    }

    /// The tuning clients receive in Welcome.
    pub fn tuning(self) -> VoiceTuning {
        let (jitter_frames, playback_keep_frames) = match self {
            Self::Competitive => (2, 2),
            Self::Balanced => (3, 3),
            Self::Smooth => (6, 4),
        };
        VoiceTuning {
            preset: self.name().into(),
            jitter_frames,
            playback_keep_frames,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub name: String,
    pub bind: SocketAddr,
    pub motd: String,
    pub password: Option<String>,
    pub max_users: usize,
    pub allow_channel_edit: bool,
    /// Permanent channels. The first one is the default channel.
    pub channels: Vec<String>,
    pub cert_dir: PathBuf,
    pub hello_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub keep_alive_ms: u64,
    pub control_queue: usize,
    /// Largest chat image body accepted, in bytes (1 to 8 MiB).
    pub max_image_bytes: u32,
    /// Highest screen-share frame rate allowed (0 = no cap, up to 120). Clients
    /// clamp to it and the server drops frames that arrive too fast.
    pub max_screen_fps: u32,
    /// Highest screen-share frame height allowed (0 = no cap, or one of 480, 720,
    /// 1080, 1440). Enforced by the client only.
    pub max_screen_height: u32,
    /// Voice latency/smoothness preset sent to clients: competitive | balanced | smooth.
    pub voice_preset: VoicePreset,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "Rust Voicechat".into(),
            bind: "127.0.0.1:7878".parse().expect("static addr"),
            motd: "Welcome to Rust Voicechat".into(),
            password: None,
            max_users: 64,
            allow_channel_edit: false,
            channels: vec!["Lobby".into(), "AFK".into()],
            cert_dir: PathBuf::from("./server-data"),
            hello_timeout_ms: 5000,
            idle_timeout_ms: 15000,
            keep_alive_ms: 5000,
            control_queue: 256,
            max_image_bytes: 4 * 1024 * 1024,
            max_screen_fps: 0,
            max_screen_height: 0,
            voice_preset: VoicePreset::Balanced,
        }
    }
}

#[derive(Debug, Clone, Default, clap::Parser)]
#[command(name = "vc-server", about = "Rust voicechat server")]
pub struct Cli {
    /// Path to a TOML config file.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Address to bind, e.g. 127.0.0.1:7878 (use 0.0.0.0:7878 to accept remote clients).
    #[arg(long)]
    pub bind: Option<SocketAddr>,
    /// Server name shown to clients.
    #[arg(long)]
    pub name: Option<String>,
    /// Require this password from clients.
    #[arg(long)]
    pub password: Option<String>,
    /// Directory holding cert.der / key.der.
    #[arg(long)]
    pub cert_dir: Option<PathBuf>,
}

/// Trimmed-name validity shared by config and runtime channel rules.
pub fn valid_channel_name(name: &str) -> bool {
    let n = name.trim().chars().count();
    (1..=MAX_CHANNEL_NAME).contains(&n)
}

impl ServerConfig {
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: ServerConfig = toml::from_str(text).context("parse server config")?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.channels.is_empty() {
            bail!("config must list at least one channel");
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.channels {
            if !valid_channel_name(c) {
                bail!("invalid channel name {c:?} (1-{MAX_CHANNEL_NAME} chars)");
            }
            if !seen.insert(c.trim().to_lowercase()) {
                bail!("duplicate channel name {c:?}");
            }
        }
        if self.max_users == 0 {
            bail!("max_users must be at least 1");
        }
        if self.control_queue == 0 {
            bail!("control_queue must be at least 1");
        }
        if !(1..=MAX_IMAGE_BYTES).contains(&self.max_image_bytes) {
            bail!("max_image_bytes must be between 1 and {MAX_IMAGE_BYTES}");
        }
        if self.max_screen_fps > SCREEN_FPS_LIMIT {
            bail!("max_screen_fps must be between 0 (no cap) and {SCREEN_FPS_LIMIT}");
        }
        if !SCREEN_HEIGHT_CAPS.contains(&self.max_screen_height) {
            bail!(
                "max_screen_height must be 0 (no cap), 480, 720, 1080 or 1440, got {}",
                self.max_screen_height
            );
        }
        Ok(())
    }
}

/// Config file picked up automatically when `--config` is not given.
pub const DEFAULT_CONFIG_FILE: &str = "server.toml";

/// Where the binary looks for [`DEFAULT_CONFIG_FILE`]: the working directory,
/// then the folder holding the executable.
pub fn default_config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        && !dirs.contains(&exe_dir)
    {
        dirs.push(exe_dir);
    }
    dirs
}

/// The first `server.toml` found in `dirs`, in order.
pub fn find_default_config(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join(DEFAULT_CONFIG_FILE))
        .find(|path| path.is_file())
}

/// TOML first, then CLI flags override it. Validates the result.
pub fn load(cli: Cli) -> Result<ServerConfig> {
    let mut cfg = match &cli.config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?;
            ServerConfig::from_toml(&text)?
        }
        None => ServerConfig::default(),
    };
    if let Some(b) = cli.bind {
        cfg.bind = b;
    }
    if let Some(n) = cli.name {
        cfg.name = n;
    }
    if let Some(p) = cli.password {
        cfg.password = Some(p);
    }
    if let Some(d) = cli.cert_dir {
        cfg.cert_dir = d;
    }
    cfg.validate()?;
    Ok(cfg)
}
