//! Rust voicechat server: config, sans-IO state, TLS and the QUIC net layer.

pub mod config;
pub mod net;
pub mod state;
pub mod tls;

pub use config::{Cli, ServerConfig, VoicePreset};
pub use net::{DisconnectReason, ServerHandle, run};
pub use state::{Action, Event, ServerState, Snapshot};
