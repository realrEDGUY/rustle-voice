// Release builds are GUI-subsystem programs, so a double-click opens no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use tracing_subscriber::EnvFilter;
use vc_client::audio::{AudioBackend, CpalBackend, FakeBackend, FakeMode, NullBackend};
use vc_client::cli::{AudioChoice, ClientCli};
use vc_client::gui::VoiceApp;
use vc_client::profile::Profile;
use vc_client::update;

/// When started from a terminal, reuse that terminal so logs and `--help` stay visible.
/// When started by double-click there is no parent console and this does nothing.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: plain Win32 call with no pointers; failure is harmless and ignored.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Title bar and taskbar icon (the exe's own icon is embedded by `build.rs`).
fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png")).ok()?;
    let rgba = img.to_rgba8();
    Some(egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn main() -> eframe::Result<()> {
    // The updater runs a freshly downloaded exe with this flag before installing it.
    // Handled first so nothing else (console, logging, GUI) can get in the way.
    if update::selftest::handle_selftest_arg() {
        std::process::exit(0);
    }
    #[cfg(all(windows, not(debug_assertions)))]
    attach_parent_console();
    let cli = ClientCli::parse();

    // Update housekeeping runs before anything that could fail, so a new version that
    // dies early is still counted and rolled back.
    let mut update_paths = None;
    let mut update_log = None;
    match update::enabled(cli.no_update) {
        Ok(()) => match std::env::current_exe()
            .ok()
            .and_then(|e| update::Paths::from_exe(&e))
        {
            Some(paths) => match update::apply::startup(&paths, update::RUNNING) {
                update::apply::StartupOutcome::RolledBack { .. } => {
                    // The previous version is back in place: start it with the same args.
                    let _ = std::process::Command::new(&paths.exe)
                        .args(std::env::args_os().skip(1))
                        .spawn();
                    std::process::exit(0);
                }
                update::apply::StartupOutcome::NotWritable(e) => {
                    update::set_status(update::Status::Off("install folder not writable"));
                    update_log = Some(format!("update: install folder not writable: {e}"));
                }
                o => {
                    update_log = Some(format!("update: startup {o:?}"));
                    update_paths = Some(paths);
                }
            },
            None => update::set_status(update::Status::Off("exe path unknown")),
        },
        Err(why) => update::set_status(update::Status::Off(why)),
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,wgpu=warn")),
        )
        .init();
    if let Some(l) = update_log {
        tracing::info!("{l}");
    }
    if let Some(paths) = &update_paths {
        #[cfg(windows)]
        update::apply::spawn_background(paths.clone());
        // Healthy after 30 s of uptime: clear the rollback counter.
        let p = paths.clone();
        let _ = std::thread::Builder::new()
            .name("vc-update-health".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                update::apply::confirm_healthy(&p, update::RUNNING, update::apply::LOCK_WAIT);
            });
    }

    let profile = Profile::from_cli(cli.profile.as_deref());
    tracing::info!("profile directory: {}", profile.dir.display());
    let backend: Box<dyn AudioBackend> = match cli.audio {
        AudioChoice::Cpal => Box::new(CpalBackend::new()),
        AudioChoice::FakeSine => Box::new(FakeBackend::new(FakeMode::Sine(440.0)).realtime()),
        AudioChoice::FakeSilent => Box::new(FakeBackend::new(FakeMode::Silent).realtime()),
        AudioChoice::Null => Box::new(NullBackend),
    };
    let title = format!("Rust Voicechat ({})", profile.dir.display());
    let mut viewport = egui::ViewportBuilder::default();
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: viewport
            .with_title(title)
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([700.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    let autoconnect = cli.autoconnect.clone();
    let result = eframe::run_native(
        "Rust Voicechat",
        options,
        Box::new(move |cc| {
            Ok(Box::new(VoiceApp::new(
                cc.egui_ctx.clone(),
                backend,
                profile,
                autoconnect,
            )))
        }),
    );
    if let (Ok(()), Some(paths)) = (&result, &update_paths) {
        update::apply::confirm_healthy(paths, update::RUNNING, std::time::Duration::from_secs(2));
    }
    // Never exit between the two renames of an update swap.
    let _swap = update::SWAP_GUARD.lock();
    result
}
