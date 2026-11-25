//! Client command line.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum AudioChoice {
    /// Real devices (falls back to `null` with a banner if unavailable).
    #[default]
    Cpal,
    /// Fake 440 Hz microphone, discarded output.
    FakeSine,
    /// Fake silent microphone, discarded output.
    FakeSilent,
    /// No audio at all.
    Null,
}

#[derive(Debug, Clone, Default, clap::Parser)]
#[command(name = "vc-client", about = "Rust voicechat client")]
pub struct ClientCli {
    /// Profile directory holding servers.toml and settings.toml.
    #[arg(long)]
    pub profile: Option<PathBuf>,
    /// Audio backend.
    #[arg(long, value_enum, default_value_t = AudioChoice::Cpal)]
    pub audio: AudioChoice,
    /// Connect to the saved server with this label at startup (repeatable).
    #[arg(long)]
    pub autoconnect: Vec<String>,
    /// Do not check for or install automatic updates (same as setting VC_NO_UPDATE).
    #[arg(long)]
    pub no_update: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_parse_profile_audio_autoconnect() {
        let c = ClientCli::parse_from([
            "vc-client",
            "--profile",
            "C:\\tmp\\p",
            "--audio",
            "fake-sine",
            "--autoconnect",
            "Home",
            "--autoconnect",
            "Work",
        ]);
        assert_eq!(c.profile, Some(PathBuf::from("C:\\tmp\\p")));
        assert_eq!(c.audio, AudioChoice::FakeSine);
        assert_eq!(c.autoconnect, vec!["Home", "Work"]);
        assert!(!c.no_update);
        assert!(ClientCli::parse_from(["vc-client", "--no-update"]).no_update);
        for (s, v) in [
            ("cpal", AudioChoice::Cpal),
            ("fake-silent", AudioChoice::FakeSilent),
            ("null", AudioChoice::Null),
        ] {
            assert_eq!(ClientCli::parse_from(["vc-client", "--audio", s]).audio, v);
        }
        assert!(ClientCli::try_parse_from(["vc-client", "--audio", "bogus"]).is_err());
    }

    #[test]
    fn cli_defaults() {
        let c = ClientCli::parse_from(["vc-client"]);
        assert_eq!(c.profile, None);
        assert_eq!(c.audio, AudioChoice::Cpal);
        assert!(c.autoconnect.is_empty());
    }
}
