//! Right-click (and click) menus as data: pure builders return a `Menu`, and one
//! renderer (`show`) draws any of them. `attach` opens a menu on a widget with a
//! single popup, so a click and a right-click never fight over its id.

use egui::{Button, Popup, PopupCloseBehavior, Response, RichText, Role, SetOpenCommand, Ui};
use vc_proto::{ChannelId, UserInfo};

use super::{name, theme};
use crate::ServerKey;
use crate::app::{AppModel, UiCommand};
use crate::profile::InputMode;
use crate::session::{ChatLine, ConnStatus, format_ts_utc};

/// What a menu item does when clicked.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    Cmd(UiCommand),
    Copy(String),
    /// Copy the chat image with this id (resolved against the model on click).
    CopyImage(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    Item {
        label: String,
        hint: String,
        /// `Some` on toggles: shown as selected when true.
        checked: Option<bool>,
        enabled: bool,
        danger: bool,
        act: Option<Act>,
    },
    Sep,
    /// The per-stream sound volume slider.
    StreamVolume {
        key: ServerKey,
        nick: String,
    },
    /// The per-user volume slider.
    Volume {
        key: ServerKey,
        nick: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Menu {
    pub title: Option<String>,
    pub items: Vec<MenuItem>,
}

impl Menu {
    /// The item with this label (tests).
    #[cfg(test)]
    pub fn item(&self, label: &str) -> Option<&MenuItem> {
        self.items
            .iter()
            .find(|i| matches!(i, MenuItem::Item { label: l, .. } if l == label))
    }
}

fn item(label: impl Into<String>, act: Option<Act>) -> MenuItem {
    MenuItem::Item {
        label: label.into(),
        hint: String::new(),
        checked: None,
        enabled: true,
        danger: false,
        act,
    }
}

fn cmd(c: UiCommand) -> Option<Act> {
    Some(Act::Cmd(c))
}

/// Builder-style tweaks on an `Item`.
trait ItemExt {
    fn hint(self, h: impl Into<String>) -> Self;
    fn checked(self, c: bool) -> Self;
    fn enabled(self, e: bool) -> Self;
    fn danger(self) -> Self;
}

impl ItemExt for MenuItem {
    fn hint(mut self, h: impl Into<String>) -> Self {
        if let MenuItem::Item { hint, .. } = &mut self {
            *hint = h.into();
        }
        self
    }

    fn checked(mut self, c: bool) -> Self {
        if let MenuItem::Item { checked, .. } = &mut self {
            *checked = Some(c);
        }
        self
    }

    fn enabled(mut self, e: bool) -> Self {
        if let MenuItem::Item { enabled, .. } = &mut self {
            *enabled = e;
        }
        self
    }

    fn danger(mut self) -> Self {
        if let MenuItem::Item { danger, .. } = &mut self {
            *danger = true;
        }
        self
    }
}

/// The display name of a server: its Welcome name, else its saved label.
pub fn server_title(model: &AppModel, key: ServerKey) -> String {
    let label = model
        .saved_server(key)
        .map(|s| s.label.clone())
        .unwrap_or_default();
    match model.session(key) {
        Some(s) if !s.server_name.is_empty() && s.status == ConnStatus::Connected => {
            s.server_name.clone()
        }
        _ => label,
    }
}

pub fn server_menu(model: &AppModel, key: ServerKey) -> Menu {
    let status = model.status(key);
    let on = status == ConnStatus::Connected;
    let mut items = vec![
        item("Talk here", cmd(UiCommand::TalkHere(key)))
            .checked(model.talk_target == Some(key))
            .enabled(on),
    ];
    items.push(match status {
        ConnStatus::Connecting => item("Connecting…", None).enabled(false),
        ConnStatus::Connected | ConnStatus::Reconnecting { .. } => {
            item("Disconnect", cmd(UiCommand::Disconnect(key)))
        }
        ConnStatus::Disconnected | ConnStatus::Failed(_) => {
            item("Connect", cmd(UiCommand::Connect(key)))
        }
    });
    let ping = model
        .session(key)
        .filter(|_| on)
        .and_then(|s| s.ping_ms)
        .map_or_else(|| "–".to_string(), |ms| format!("{ms} ms"));
    items.push(item("Ping", None).hint(ping).enabled(false));
    items.push(MenuItem::Sep);
    items.push(item("Edit server…", cmd(UiCommand::OpenEditServer(key))));
    let addr = model
        .saved_server(key)
        .map(|s| s.addr.clone())
        .unwrap_or_default();
    items.push(item("Copy address", Some(Act::Copy(addr))));
    items.push(MenuItem::Sep);
    items.push(item("Remove server", cmd(UiCommand::RemoveServer(key))).danger());
    Menu {
        title: Some(server_title(model, key)),
        items,
    }
}

pub fn channel_menu(model: &AppModel, key: ServerKey, ch: ChannelId) -> Menu {
    let Some(s) = model.session(key) else {
        return Menu::default();
    };
    let here = s.my_channel() == Some(ch);
    let is_default = Some(ch) == s.default_channel;
    let join = if here {
        item("You are here", None).enabled(false)
    } else {
        item("Join channel", cmd(UiCommand::Join(ch)))
    };
    // Delete: the server always refuses the default channel. Rename/Delete of a
    // permanent channel is refused unless the server sets allow_channel_edit, which
    // the client cannot see, so those are offered and a refusal shows as a toast.
    let delete = item("Delete channel", cmd(UiCommand::DeleteChannel(ch)))
        .danger()
        .enabled(!is_default)
        .hint(if is_default { "default" } else { "" });
    Menu {
        title: s.channels.get(&ch).map(|c| c.name.clone()),
        items: vec![
            join,
            item("Rename…", cmd(UiCommand::OpenRenameChannel(ch))).hint("F2"),
            item("New channel", cmd(UiCommand::OpenNewChannel)),
            MenuItem::Sep,
            delete,
        ],
    }
}

pub fn user_menu(model: &AppModel, key: ServerKey, user: &UserInfo) -> Menu {
    let Some(s) = model.session(key) else {
        return Menu::default();
    };
    let nick = user.nick.clone();
    let mut items = vec![MenuItem::Volume {
        key,
        nick: nick.clone(),
    }];
    if s.can_watch(user) {
        let watching = s.watching.contains(&user.id);
        let label = if watching {
            "Stop watching"
        } else {
            "Watch stream"
        };
        items.push(item(
            label,
            cmd(UiCommand::WatchScreen {
                key,
                user: user.id,
                on: !watching,
            }),
        ));
        let muted = model.stream_is_muted(key, user.id);
        items.push(
            item(
                "Mute stream sound",
                cmd(UiCommand::MuteStream {
                    key,
                    user: user.id,
                    muted: !muted,
                }),
            )
            .checked(muted),
        );
        items.push(MenuItem::StreamVolume {
            key,
            nick: nick.clone(),
        });
    }
    items.push(MenuItem::Sep);
    let muted = model.user_is_muted(key, &nick);
    items.push(
        item(
            "Mute for me",
            cmd(UiCommand::MuteUser {
                key,
                nick: nick.clone(),
                muted: !muted,
            }),
        )
        .checked(muted),
    );
    items.push(
        item(
            "Reset volume",
            cmd(UiCommand::SetUserVolume {
                key,
                nick: nick.clone(),
                volume: 1.0,
            }),
        )
        .hint("100 %"),
    );
    items.push(MenuItem::Sep);
    items.push(item("Copy nickname", Some(Act::Copy(nick.clone()))));
    Menu {
        title: Some(nick.to_uppercase()),
        items,
    }
}

fn mode_items(model: &AppModel) -> Vec<MenuItem> {
    InputMode::ALL
        .iter()
        .map(|&m| {
            let hint = if m == InputMode::Ptt {
                format!("hold {}", model.settings.ptt_key)
            } else {
                String::new()
            };
            item(m.label(), cmd(UiCommand::SetInputMode(m)))
                .checked(model.settings.input_mode == m)
                .hint(hint)
        })
        .collect()
}

/// My nickname as shown in the me bar: the selected session's, else the default.
pub fn my_nick(model: &AppModel) -> String {
    model
        .selected_session()
        .and_then(|s| s.my_id.and_then(|id| s.nick_of(id)))
        .map(str::to_string)
        .or_else(|| {
            model
                .selected
                .and_then(|k| model.saved_server(k))
                .map(|s| s.nick.clone())
        })
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            if model.settings.nickname.trim().is_empty() {
                crate::app::DEFAULT_NICK.to_string()
            } else {
                model.settings.nickname.clone()
            }
        })
}

pub fn me_menu(model: &AppModel) -> Menu {
    let v = model.voice;
    let mut items = vec![
        item("Mute microphone", cmd(UiCommand::ToggleMute))
            .checked(v.muted || v.deafened)
            .hint("Ctrl+M"),
        item("Deafen", cmd(UiCommand::ToggleDeafen))
            .checked(v.deafened)
            .hint("Ctrl+D"),
        MenuItem::Sep,
    ];
    items.extend(mode_items(model));
    items.push(MenuItem::Sep);
    items.push(item("Settings…", cmd(UiCommand::OpenSettings)));
    Menu {
        title: Some(my_nick(model)),
        items,
    }
}

pub fn mic_menu(model: &AppModel) -> Menu {
    let mut items = mode_items(model);
    items.push(MenuItem::Sep);
    items.push(item("Voice settings…", cmd(UiCommand::OpenSettings)));
    Menu {
        title: Some("Input mode".into()),
        items,
    }
}

pub fn message_menu(line: &ChatLine) -> Menu {
    let mut items = Vec::new();
    if let Some(img) = line.image {
        items.push(item("Open image", cmd(UiCommand::ViewImage(img.id))).hint("Click"));
        items.push(item("Copy image", Some(Act::CopyImage(img.id))));
        items.push(MenuItem::Sep);
    }
    let text = if line.text.is_empty() {
        super::chat::chat_line(line)
    } else {
        line.text.clone()
    };
    items.push(item("Copy text", Some(Act::Copy(text))));
    items.push(item(
        "Copy time",
        Some(Act::Copy(format_ts_utc(line.ts_ms))),
    ));
    Menu {
        title: Some("Message".into()),
        items,
    }
}

/// Draw a menu built by one of the builders and run the clicked item.
pub fn show(ui: &mut Ui, model: &AppModel, menu: &Menu, cmds: &mut Vec<UiCommand>) {
    ui.set_min_width(220.0);
    if let Some(t) = &menu.title {
        ui.label(RichText::new(t).small().strong().color(theme::DIM));
    }
    for it in &menu.items {
        match it {
            MenuItem::Sep => {
                ui.separator();
            }
            MenuItem::Volume { key, nick } => volume_row(ui, model, *key, nick, cmds),
            MenuItem::StreamVolume { key, nick } => {
                let current = model.settings.stream_volume(*key, nick);
                stream_volume_slider(ui, *key, nick, current, cmds);
            }
            MenuItem::Item {
                label,
                hint,
                checked,
                enabled,
                danger,
                act,
            } => {
                let mut text = RichText::new(label);
                if *danger && *enabled {
                    text = text.color(theme::RED);
                }
                let mut b = Button::new(text).right_text(RichText::new(hint).color(theme::DIM));
                if let Some(c) = checked {
                    b = b.selected(*c);
                }
                let r = ui.add_enabled(*enabled, b);
                // Named by the visible label alone (the hint is not part of it).
                let (en, sel, label2) = (*enabled, *checked, label.clone());
                r.widget_info(|| {
                    let mut i = egui::WidgetInfo::labeled(Role::Button, en, &label2);
                    i.selected = sel;
                    i
                });
                if r.clicked() {
                    if let Some(a) = act {
                        run(ui.ctx(), model, a, cmds);
                    }
                    ui.close();
                }
            }
        }
    }
}

fn run(ctx: &egui::Context, model: &AppModel, act: &Act, cmds: &mut Vec<UiCommand>) {
    match act {
        Act::Cmd(c) => cmds.push(c.clone()),
        Act::Copy(t) => ctx.copy_text(t.clone()),
        Act::CopyImage(id) => {
            if let Some(img) = model.sessions.values().find_map(|s| s.images.get(id)) {
                ctx.copy_image((**img).clone());
            }
        }
    }
}

/// A 0-200 % volume slider under `heading`, named `slider_name`, with the
/// value shown as plain `text(pct)`. Returns the new gain on change.
fn pct_slider(
    ui: &mut Ui,
    heading: &str,
    current: f32,
    slider_name: String,
    text: impl Fn(f32) -> String,
) -> Option<f32> {
    ui.label(RichText::new(heading).small().color(theme::MUTED));
    let mut pct = (current * 100.0).round();
    let sl = ui.add(
        egui::Slider::new(&mut pct, 0.0..=200.0)
            .show_value(false)
            .text("Volume"),
    );
    let sl = name(sl, Role::Slider, slider_name);
    // Plain text, not an editable box: typing + Enter there closed
    // the menu without applying the value.
    ui.label(text(pct));
    sl.changed().then_some(pct / 100.0)
}

/// The per-user volume slider (0-200 %).
fn volume_row(
    ui: &mut Ui,
    model: &AppModel,
    key: ServerKey,
    nick: &str,
    cmds: &mut Vec<UiCommand>,
) {
    let current = model.settings.user_volume(key, nick);
    let changed = pct_slider(
        ui,
        "User volume",
        current,
        format!("Volume {nick}"),
        |pct| format!("{nick}: {pct:.0} %"),
    );
    if let Some(volume) = changed {
        cmds.push(UiCommand::SetUserVolume {
            key,
            nick: nick.to_string(),
            volume,
        });
    }
}

/// The per-stream sound volume slider (0-200 %), for the user menu and the
/// stream tile's speaker button. `current` is the stored volume.
pub fn stream_volume_slider(
    ui: &mut Ui,
    key: ServerKey,
    nick: &str,
    current: f32,
    cmds: &mut Vec<UiCommand>,
) {
    let changed = pct_slider(
        ui,
        "Stream volume",
        current,
        format!("Stream volume {nick}"),
        |pct| format!("{nick}'s stream: {pct:.0} %"),
    );
    if let Some(volume) = changed {
        cmds.push(UiCommand::SetStreamVolume {
            key,
            nick: nick.to_string(),
            volume,
        });
    }
}

/// Open a menu from `resp`: on right-click, and also on a plain click when
/// `left_click`. One popup per widget (never combined with `context_menu`, which
/// shares the popup id and closes it on a primary click). Build the menu inside
/// `body`, so it is only built while open.
pub fn attach(resp: &Response, left_click: bool, body: impl FnOnce(&mut Ui)) {
    let open = (left_click && resp.clicked()) || resp.secondary_clicked();
    Popup::menu(resp)
        .open_memory(open.then_some(SetOpenCommand::Bool(true)))
        .at_pointer_fixed()
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use crate::session::ClientSession;
    use vc_proto::{ChannelInfo, SessionId};

    fn user(id: u32, nick: &str, ch: u32, sharing: bool) -> UserInfo {
        UserInfo {
            id: SessionId(id),
            nick: nick.into(),
            channel: ChannelId(ch),
            muted: false,
            deafened: false,
            sharing,
        }
    }

    fn model() -> (AppModel, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let mut m = AppModel::load(Profile::new(d.path()));
        let mut s = ClientSession::new();
        s.status = ConnStatus::Connected;
        s.my_id = Some(SessionId(1));
        s.default_channel = Some(ChannelId(1));
        for (id, name) in [(1, "Lobby"), (2, "AFK")] {
            s.channels.insert(
                ChannelId(id),
                ChannelInfo {
                    id: ChannelId(id),
                    name: name.into(),
                    permanent: true,
                },
            );
        }
        for u in [
            user(1, "me", 1, false),
            user(2, "bob", 1, true),
            user(3, "carol", 1, false),
        ] {
            s.users.insert(u.id, u);
        }
        m.sessions.insert(1, s);
        m.saved.push(crate::profile::SavedServer {
            key: 1,
            label: "S".into(),
            addr: "127.0.0.1:1".into(),
            nick: "me".into(),
            password: None,
            fingerprint: None,
        });
        m.selected = Some(1);
        (m, d)
    }

    fn enabled(m: &Menu, label: &str) -> bool {
        match m.item(label) {
            Some(MenuItem::Item { enabled, .. }) => *enabled,
            other => panic!("{label}: {other:?}"),
        }
    }

    fn checked(m: &Menu, label: &str) -> Option<bool> {
        match m.item(label) {
            Some(MenuItem::Item { checked, .. }) => *checked,
            other => panic!("{label}: {other:?}"),
        }
    }

    #[test]
    fn delete_disabled_on_default_channel() {
        let (m, _d) = model();
        let lobby = channel_menu(&m, 1, ChannelId(1));
        assert!(!enabled(&lobby, "Delete channel"));
        let afk = channel_menu(&m, 1, ChannelId(2));
        assert!(enabled(&afk, "Delete channel"));
    }

    #[test]
    fn rename_enabled_on_default_channel() {
        let (m, _d) = model();
        let lobby = channel_menu(&m, 1, ChannelId(1));
        assert!(enabled(&lobby, "Rename…"));
        assert!(!enabled(&lobby, "Delete channel"));
        match lobby.item("Rename…") {
            Some(MenuItem::Item { hint, .. }) => assert_eq!(hint, "F2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn you_are_here_in_my_channel() {
        let (m, _d) = model();
        let lobby = channel_menu(&m, 1, ChannelId(1));
        assert!(!enabled(&lobby, "You are here"));
        assert!(lobby.item("Join channel").is_none());
        let afk = channel_menu(&m, 1, ChannelId(2));
        assert!(enabled(&afk, "Join channel"));
    }

    #[test]
    fn watch_only_for_sharers() {
        let (m, _d) = model();
        let s = m.session(1).unwrap();
        let bob = user_menu(&m, 1, &s.users[&SessionId(2)]);
        assert!(bob.item("Watch stream").is_some());
        assert!(bob.item("Mute stream sound").is_some());
        let carol = user_menu(&m, 1, &s.users[&SessionId(3)]);
        assert!(carol.item("Watch stream").is_none());
        assert!(carol.item("Mute stream sound").is_none());
        assert_eq!(carol.title.as_deref(), Some("CAROL"));
    }

    #[test]
    fn talk_here_checked_and_enabled() {
        let (mut m, _d) = model();
        let menu = server_menu(&m, 1);
        assert_eq!(checked(&menu, "Talk here"), Some(false));
        assert!(enabled(&menu, "Talk here"));
        assert!(menu.item("Disconnect").is_some());
        m.talk_target = Some(1);
        assert_eq!(checked(&server_menu(&m, 1), "Talk here"), Some(true));
        m.sessions.get_mut(&1).unwrap().status = ConnStatus::Disconnected;
        let menu = server_menu(&m, 1);
        assert!(!enabled(&menu, "Talk here"));
        assert!(menu.item("Connect").is_some());
        m.sessions.get_mut(&1).unwrap().status = ConnStatus::Connecting;
        assert!(!enabled(&server_menu(&m, 1), "Connecting…"));
    }

    #[test]
    fn ping_hint_shows_ms() {
        let (mut m, _d) = model();
        m.sessions.get_mut(&1).unwrap().ping_ms = Some(24);
        match server_menu(&m, 1).item("Ping") {
            Some(MenuItem::Item { hint, enabled, .. }) => {
                assert_eq!(hint, "24 ms");
                assert!(!enabled);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mute_for_me_reflects_user_muted() {
        let (mut m, _d) = model();
        let bob = m.session(1).unwrap().users[&SessionId(2)].clone();
        assert_eq!(checked(&user_menu(&m, 1, &bob), "Mute for me"), Some(false));
        m.user_muted.insert((1, "bob".into()));
        let menu = user_menu(&m, 1, &bob);
        assert_eq!(checked(&menu, "Mute for me"), Some(true));
        match menu.item("Mute for me") {
            Some(MenuItem::Item {
                act: Some(Act::Cmd(UiCommand::MuteUser { muted, .. })),
                ..
            }) => assert!(!muted, "clicking unmutes"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn me_menu_marks_mode_and_ptt_key() {
        let (mut m, _d) = model();
        m.settings.input_mode = InputMode::Ptt;
        m.settings.ptt_key = "V".into();
        let menu = me_menu(&m);
        assert_eq!(checked(&menu, InputMode::Ptt.label()), Some(true));
        assert_eq!(checked(&menu, InputMode::Vad.label()), Some(false));
        match menu.item(InputMode::Ptt.label()) {
            Some(MenuItem::Item { hint, .. }) => assert_eq!(hint, "hold V"),
            other => panic!("{other:?}"),
        }
        assert_eq!(menu.title.as_deref(), Some("me"));
    }
}
