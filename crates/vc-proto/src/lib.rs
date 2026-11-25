//! Wire format shared by the voicechat server and client. No IO lives here.

pub mod blob;
pub mod control;
pub mod frame;
pub mod ids;
pub mod voice;

pub use blob::{BlobError, ClientBlob, ImageKind, ScreenKind, ServerBlob, sniff, sniff_screen};
pub use control::{
    ChannelInfo, ClientMsg, ErrorCode, MAX_PRESET_NAME, MAX_VOICE_FRAMES, ServerMsg, UserInfo,
    VoiceTuning, all_client_samples, all_server_samples,
};
pub use frame::{FrameDecoder, FrameError, encode};
pub use ids::{ChannelId, SessionId};
pub use voice::{ClientVoice, STREAM_BIT, ServerVoice, VoiceError};

/// Protocol version carried in `Hello`.
pub const PROTOCOL_VERSION: u32 = 4;
/// Highest screen-share frame rate any client may offer or any server may allow.
pub const SCREEN_FPS_LIMIT: u32 = 120;
/// QUIC ALPN identifier. Stays `vc/1` across protocol versions so an old client
/// gets a clean `BadVersion` error instead of a TLS handshake failure.
pub const ALPN: &[u8] = b"vc/1";
/// Audio sample rate on the wire.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// Bytes of one IMA-ADPCM encoded frame (4-byte header + 480 bytes of nibbles).
pub const ADPCM_FRAME_BYTES: usize = 484;
/// Largest accepted control frame payload.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;
/// Max nickname length (chars, after trim).
pub const MAX_NICK: usize = 32;
/// Max channel name length (chars, after trim).
pub const MAX_CHANNEL_NAME: usize = 32;
/// Max chat message length (chars).
pub const MAX_CHAT: usize = 500;
/// Hard ceiling for one chat image body (the server knob may only lower it).
pub const MAX_IMAGE_BYTES: u32 = 8 * 1024 * 1024;
/// Fixed ceiling for one screen-share frame body.
pub const MAX_SCREEN_FRAME_BYTES: u32 = 1024 * 1024;
/// Largest accepted blob stream JSON header.
pub const MAX_BLOB_HEADER: usize = 1024;
/// Concurrent client-initiated blob (uni) streams per connection.
pub const MAX_BLOB_STREAMS: u32 = 4;
