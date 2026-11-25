import Foundation

/// Commands for the Rust core, mirroring `MobileCmd` (crates/vc-mobile/src/cmd.rs).
/// Encoded by hand as JSON tagged with "t". Each one is a whole submit: Swift owns
/// its text drafts. There is deliberately no share command.
enum Command: Encodable, Equatable {
    case select(key: UInt64)
    case connect(key: UInt64)
    case disconnect(key: UInt64)
    case talkHere(key: UInt64)
    case removeServer(key: UInt64)
    case saveServer(editing: UInt64?, label: String, addr: String, nick: String, password: String, connectNow: Bool)
    case join(key: UInt64, channel: UInt32)
    case createChannel(key: UInt64, name: String)
    case renameChannel(key: UInt64, channel: UInt32, name: String)
    case deleteChannel(key: UInt64, channel: UInt32)
    case sendChat(key: UInt64, text: String)
    case toggleMute
    case toggleDeafen
    /// "Vad", "Ptt" or "Open".
    case setInputMode(mode: String)
    case setVadThreshold(db: Double, persist: Bool)
    case setMasterVolume(volume: Double, persist: Bool)
    case setNickname(nick: String)
    case setUserVolume(key: UInt64, nick: String, volume: Double)
    case muteUser(key: UInt64, nick: String, muted: Bool)
    case watchScreen(key: UInt64, user: UInt32, on: Bool)
    case muteStream(key: UInt64, user: UInt32, muted: Bool)
    case uploadFile(key: UInt64, path: String)
    case trustNewIdentity
    case cancelTofu
    case dismissToast(text: String)

    private enum K: String, CodingKey {
        case t, key, editing, label, addr, nick, password, channel, name, text, mode, db
        case persist, volume, muted, user, on, path
        case connectNow = "connect_now"
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        func tag(_ t: String) throws { try c.encode(t, forKey: .t) }
        switch self {
        case let .select(key):
            try tag("Select"); try c.encode(key, forKey: .key)
        case let .connect(key):
            try tag("Connect"); try c.encode(key, forKey: .key)
        case let .disconnect(key):
            try tag("Disconnect"); try c.encode(key, forKey: .key)
        case let .talkHere(key):
            try tag("TalkHere"); try c.encode(key, forKey: .key)
        case let .removeServer(key):
            try tag("RemoveServer"); try c.encode(key, forKey: .key)
        case let .saveServer(editing, label, addr, nick, password, connectNow):
            try tag("SaveServer")
            if let editing {
                try c.encode(editing, forKey: .editing)
            } else {
                try c.encodeNil(forKey: .editing)
            }
            try c.encode(label, forKey: .label)
            try c.encode(addr, forKey: .addr)
            try c.encode(nick, forKey: .nick)
            try c.encode(password, forKey: .password)
            try c.encode(connectNow, forKey: .connectNow)
        case let .join(key, channel):
            try tag("Join"); try c.encode(key, forKey: .key); try c.encode(channel, forKey: .channel)
        case let .createChannel(key, name):
            try tag("CreateChannel"); try c.encode(key, forKey: .key); try c.encode(name, forKey: .name)
        case let .renameChannel(key, channel, name):
            try tag("RenameChannel")
            try c.encode(key, forKey: .key)
            try c.encode(channel, forKey: .channel)
            try c.encode(name, forKey: .name)
        case let .deleteChannel(key, channel):
            try tag("DeleteChannel"); try c.encode(key, forKey: .key); try c.encode(channel, forKey: .channel)
        case let .sendChat(key, text):
            try tag("SendChat"); try c.encode(key, forKey: .key); try c.encode(text, forKey: .text)
        case .toggleMute:
            try tag("ToggleMute")
        case .toggleDeafen:
            try tag("ToggleDeafen")
        case let .setInputMode(mode):
            try tag("SetInputMode"); try c.encode(mode, forKey: .mode)
        case let .setVadThreshold(db, persist):
            try tag("SetVadThreshold"); try c.encode(db, forKey: .db); try c.encode(persist, forKey: .persist)
        case let .setMasterVolume(volume, persist):
            try tag("SetMasterVolume"); try c.encode(volume, forKey: .volume); try c.encode(persist, forKey: .persist)
        case let .setNickname(nick):
            try tag("SetNickname"); try c.encode(nick, forKey: .nick)
        case let .setUserVolume(key, nick, volume):
            try tag("SetUserVolume")
            try c.encode(key, forKey: .key)
            try c.encode(nick, forKey: .nick)
            try c.encode(volume, forKey: .volume)
        case let .muteUser(key, nick, muted):
            try tag("MuteUser")
            try c.encode(key, forKey: .key)
            try c.encode(nick, forKey: .nick)
            try c.encode(muted, forKey: .muted)
        case let .watchScreen(key, user, on):
            try tag("WatchScreen")
            try c.encode(key, forKey: .key)
            try c.encode(user, forKey: .user)
            try c.encode(on, forKey: .on)
        case let .muteStream(key, user, muted):
            try tag("MuteStream")
            try c.encode(key, forKey: .key)
            try c.encode(user, forKey: .user)
            try c.encode(muted, forKey: .muted)
        case let .uploadFile(key, path):
            try tag("UploadFile"); try c.encode(key, forKey: .key); try c.encode(path, forKey: .path)
        case .trustNewIdentity:
            try tag("TrustNewIdentity")
        case .cancelTofu:
            try tag("CancelTofu")
        case let .dismissToast(text):
            try tag("DismissToast"); try c.encode(text, forKey: .text)
        }
    }

    /// The JSON the core parses.
    func json() -> String {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        guard let data = try? e.encode(self), let s = String(data: data, encoding: .utf8) else {
            return "{}"
        }
        return s
    }
}
