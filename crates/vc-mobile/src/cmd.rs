//! Commands from the Swift UI, as JSON tagged with `"t"`. Each one is a whole
//! submit (Swift owns its text drafts) and maps onto the desktop's `UiCommand`s.
//! There is no share, stream-sound-send, probe or pop-out command: the iOS app
//! can watch streams but never share.

use serde::Deserialize;
use vc_client::ServerKey;
use vc_client::profile::InputMode;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "t")]
pub enum MobileCmd {
    Select {
        key: ServerKey,
    },
    Connect {
        key: ServerKey,
    },
    Disconnect {
        key: ServerKey,
    },
    TalkHere {
        key: ServerKey,
    },
    RemoveServer {
        key: ServerKey,
    },
    /// Add (`editing: null`) or edit a saved server.
    SaveServer {
        editing: Option<ServerKey>,
        label: String,
        addr: String,
        nick: String,
        password: String,
        connect_now: bool,
    },
    Join {
        key: ServerKey,
        channel: u32,
    },
    CreateChannel {
        key: ServerKey,
        name: String,
    },
    RenameChannel {
        key: ServerKey,
        channel: u32,
        name: String,
    },
    DeleteChannel {
        key: ServerKey,
        channel: u32,
    },
    SendChat {
        key: ServerKey,
        text: String,
    },
    ToggleMute,
    ToggleDeafen,
    SetInputMode {
        mode: InputMode,
    },
    SetVadThreshold {
        db: f32,
        persist: bool,
    },
    SetMasterVolume {
        volume: f32,
        persist: bool,
    },
    SetNickname {
        nick: String,
    },
    SetUserVolume {
        key: ServerKey,
        nick: String,
        volume: f32,
    },
    MuteUser {
        key: ServerKey,
        nick: String,
        muted: bool,
    },
    WatchScreen {
        key: ServerKey,
        user: u32,
        on: bool,
    },
    MuteStream {
        key: ServerKey,
        user: u32,
        muted: bool,
    },
    /// Send the image file at `path` (a PNG or JPEG) to `key`'s current channel.
    UploadFile {
        key: ServerKey,
        path: String,
    },
    TrustNewIdentity,
    CancelTofu,
    DismissToast {
        text: String,
    },
}

pub fn parse(json: &str) -> Result<MobileCmd, String> {
    serde_json::from_str(json).map_err(|e| format!("bad command: {e}"))
}
