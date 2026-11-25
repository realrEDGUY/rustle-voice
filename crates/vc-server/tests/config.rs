use clap::Parser;
use vc_server::{Cli, ServerConfig, VoicePreset, config};

#[test]
fn config_parse_defaults() {
    let cfg = ServerConfig::from_toml("").unwrap();
    assert_eq!(cfg, ServerConfig::default());
    assert_eq!(cfg.bind.to_string(), "127.0.0.1:7878");
    assert_eq!(cfg.channels, vec!["Lobby", "AFK"]);
    assert_eq!(cfg.max_users, 64);
    assert!(!cfg.allow_channel_edit);
    assert_eq!(cfg.password, None);
    assert_eq!(cfg.hello_timeout_ms, 5000);
    assert_eq!(cfg.idle_timeout_ms, 15000);
    assert_eq!(cfg.keep_alive_ms, 5000);
    assert_eq!(cfg.control_queue, 256);
    assert_eq!(cfg.max_image_bytes, 4 * 1024 * 1024);
    assert_eq!(cfg.voice_preset, VoicePreset::Balanced);
    // An empty CLI with no config file yields the same defaults.
    let loaded = config::load(Cli::parse_from(["vc-server"])).unwrap();
    assert_eq!(loaded, ServerConfig::default());
}

#[test]
fn config_toml_overrides() {
    let cfg = ServerConfig::from_toml(
        r#"
name = "My Server"
bind = "0.0.0.0:9000"
motd = "hi"
password = "pw"
max_users = 3
allow_channel_edit = true
channels = ["General", "Music", "Games"]
hello_timeout_ms = 100
"#,
    )
    .unwrap();
    assert_eq!(cfg.name, "My Server");
    assert_eq!(cfg.bind.to_string(), "0.0.0.0:9000");
    assert_eq!(cfg.password.as_deref(), Some("pw"));
    assert_eq!(cfg.max_users, 3);
    assert!(cfg.allow_channel_edit);
    assert_eq!(cfg.channels, vec!["General", "Music", "Games"]);
    assert_eq!(cfg.hello_timeout_ms, 100);
    // Unspecified fields keep defaults.
    assert_eq!(cfg.idle_timeout_ms, 15000);
    // Unknown keys are an error, not silently ignored.
    assert!(ServerConfig::from_toml("bogus = 1").is_err());
}

#[test]
fn cli_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::write(
        &path,
        "name = \"FromFile\"\nbind = \"127.0.0.1:1111\"\npassword = \"file\"\nmotd = \"file motd\"\n",
    )
    .unwrap();
    let cli = Cli::parse_from([
        "vc-server",
        "--config",
        path.to_str().unwrap(),
        "--bind",
        "127.0.0.1:2222",
        "--name",
        "FromCli",
        "--password",
        "cli",
        "--cert-dir",
        "certs-x",
    ]);
    let cfg = config::load(cli).unwrap();
    assert_eq!(cfg.name, "FromCli");
    assert_eq!(cfg.bind.to_string(), "127.0.0.1:2222");
    assert_eq!(cfg.password.as_deref(), Some("cli"));
    assert_eq!(cfg.cert_dir, std::path::PathBuf::from("certs-x"));
    assert_eq!(cfg.motd, "file motd", "non-overridden file value kept");
}

#[test]
fn default_config_found_in_search_dirs() {
    let empty = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let dirs = || {
        vec![
            empty.path().to_path_buf(),
            first.path().to_path_buf(),
            second.path().to_path_buf(),
        ]
    };
    assert_eq!(config::find_default_config(dirs()), None);

    // A directory named server.toml is not a config file.
    std::fs::create_dir(empty.path().join(config::DEFAULT_CONFIG_FILE)).unwrap();
    assert_eq!(config::find_default_config(dirs()), None);

    let in_second = second.path().join(config::DEFAULT_CONFIG_FILE);
    std::fs::write(&in_second, "name = \"Second\"\n").unwrap();
    assert_eq!(config::find_default_config(dirs()), Some(in_second.clone()));

    // Earlier directories win.
    let in_first = first.path().join(config::DEFAULT_CONFIG_FILE);
    std::fs::write(&in_first, "name = \"First\"\n").unwrap();
    let found = config::find_default_config(dirs()).unwrap();
    assert_eq!(found, in_first);

    let cli = Cli {
        config: Some(found),
        ..Cli::default()
    };
    assert_eq!(config::load(cli).unwrap().name, "First");
}

#[test]
fn config_rejects_no_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::write(&path, "channels = []\n").unwrap();
    let err = config::load(Cli::parse_from([
        "vc-server",
        "--config",
        path.to_str().unwrap(),
    ]))
    .unwrap_err();
    assert!(err.to_string().contains("at least one channel"), "{err}");
    // Also bad and duplicate names.
    std::fs::write(&path, "channels = [\"\"]\n").unwrap();
    assert!(
        config::load(Cli::parse_from([
            "vc-server",
            "--config",
            path.to_str().unwrap()
        ]))
        .is_err()
    );
    std::fs::write(&path, "channels = [\"A\", \"a\"]\n").unwrap();
    assert!(
        config::load(Cli::parse_from([
            "vc-server",
            "--config",
            path.to_str().unwrap()
        ]))
        .is_err()
    );
}

#[test]
fn screen_cap_defaults_and_validation() {
    let cfg = ServerConfig::default();
    assert_eq!((cfg.max_screen_fps, cfg.max_screen_height), (0, 0));
    let cfg = ServerConfig::from_toml("max_screen_fps = 30\nmax_screen_height = 720").unwrap();
    assert_eq!((cfg.max_screen_fps, cfg.max_screen_height), (30, 720));
    assert!(cfg.validate().is_ok());
    for ok in [0, 1, 120] {
        let cfg = ServerConfig::from_toml(&format!("max_screen_fps = {ok}")).unwrap();
        assert!(cfg.validate().is_ok(), "{ok}");
    }
    for ok in [0, 480, 720, 1080, 1440] {
        let cfg = ServerConfig::from_toml(&format!("max_screen_height = {ok}")).unwrap();
        assert!(cfg.validate().is_ok(), "{ok}");
    }
    let err = ServerConfig::from_toml("max_screen_fps = 121")
        .unwrap()
        .validate()
        .unwrap_err();
    assert!(err.to_string().contains("max_screen_fps"), "{err}");
    let err = ServerConfig::from_toml("max_screen_height = 600")
        .unwrap()
        .validate()
        .unwrap_err();
    assert!(err.to_string().contains("max_screen_height"), "{err}");
}

#[test]
fn max_image_bytes_knob() {
    let cfg = ServerConfig::from_toml("max_image_bytes = 1000").unwrap();
    assert_eq!(cfg.max_image_bytes, 1000);
    assert!(cfg.validate().is_ok());
    let max = ServerConfig::from_toml(&format!("max_image_bytes = {}", vc_proto::MAX_IMAGE_BYTES))
        .unwrap();
    assert!(max.validate().is_ok());
    for bad in [0, vc_proto::MAX_IMAGE_BYTES + 1] {
        let cfg = ServerConfig::from_toml(&format!("max_image_bytes = {bad}")).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("max_image_bytes"), "{err}");
    }
}

#[test]
fn voice_preset_knob() {
    let cfg = ServerConfig::from_toml(r#"voice_preset = "competitive""#).unwrap();
    assert_eq!(cfg.voice_preset, VoicePreset::Competitive);
    assert!(cfg.validate().is_ok());
    let t = cfg.voice_preset.tuning();
    assert_eq!(
        (t.preset.as_str(), t.jitter_frames, t.playback_keep_frames),
        ("competitive", 2, 2)
    );
    let t = VoicePreset::Smooth.tuning();
    assert_eq!(
        (t.preset.as_str(), t.jitter_frames, t.playback_keep_frames),
        ("smooth", 6, 4)
    );
    // Balanced is exactly the client's pre-Welcome default.
    assert_eq!(
        VoicePreset::Balanced.tuning(),
        vc_proto::VoiceTuning::default()
    );
    let err = ServerConfig::from_toml(r#"voice_preset = "turbo""#).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("competitive") && msg.contains("smooth"),
        "{msg}"
    );
}
