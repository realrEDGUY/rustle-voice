//! Rust voicechat client: sans-IO session/app model, network layer, audio and egui GUI.

pub mod app;
pub mod audio;
pub mod capture;
pub mod cli;
pub mod dsp;
pub mod gui;
#[cfg(windows)]
pub mod loopback;
pub mod media;
pub mod net;
pub mod pipeline;
pub mod probe;
pub mod profile;
pub mod session;
pub mod tofu;
pub mod update;
pub mod video;

/// Stable id of a saved server, persisted in `servers.toml`.
pub type ServerKey = u64;
