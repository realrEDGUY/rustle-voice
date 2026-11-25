//! Screen-share texts shared by the voice card, the me bar and the stream tiles.
//! Watching is opted into from a sharer's user menu ("Watch stream"); watched
//! streams are drawn by `streams.rs`.

/// Accessible name of the me bar's LIVE badge, shown whenever my screen is
/// shared, whichever server is selected, so the desktop is never streamed with
/// nothing on screen saying so.
pub const SHARING_SELF: &str = "You are sharing your screen";
pub const WAITING: &str = "Waiting for screen…";

/// How many users watch my share.
pub fn watching_label(n: u32) -> String {
    format!("{n} watching")
}

pub fn stop_watching_label(nick: &str) -> String {
    format!("Stop watching {nick}")
}

/// Accessible name of a sharer's clickable LIVE badge in the channel list.
pub fn watch_stream_label(nick: &str) -> String {
    format!("Watch {nick}'s stream")
}

/// Accessible name of a watched sharer's LIVE badge while the streams are popped out.
pub fn show_stream_label(nick: &str) -> String {
    format!("Show {nick}'s stream")
}
