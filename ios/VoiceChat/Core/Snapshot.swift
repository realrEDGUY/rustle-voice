import Foundation

// Codable mirrors of crates/vc-mobile/src/view.rs. The JSON uses snake_case keys;
// the decoder converts them (`VCJSON.decoder`). The golden files in
// crates/vc-mobile/tests/golden keep both sides in step (BridgeGoldenTests).

enum VCJSON {
    static var decoder: JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    static var encoder: JSONEncoder {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.sortedKeys]
        return e
    }
}

struct Snapshot: Codable, Equatable {
    var selected: UInt64?
    var defaultNick: String
    var settings: SettingsState
    var audioWarning: String?
    var toasts: [String]
    var tofu: TofuPrompt?
    var voice: VoiceState
    var servers: [Server]
    var streams: [StreamItem]

    static let empty = Snapshot(
        selected: nil,
        defaultNick: "user",
        settings: SettingsState(inputMode: "Vad", vadThresholdDb: -45, masterVolume: 1, nickname: ""),
        audioWarning: nil,
        toasts: [],
        tofu: nil,
        voice: VoiceState(talkTarget: nil, muted: false, deafened: false, serverLabel: nil, channelName: nil),
        servers: [],
        streams: []
    )

    func server(_ key: UInt64?) -> Server? {
        guard let key else { return nil }
        return servers.first { $0.key == key }
    }
}

struct SettingsState: Codable, Equatable {
    /// "Vad", "Ptt" or "Open".
    var inputMode: String
    var vadThresholdDb: Double
    var masterVolume: Double
    var nickname: String
}

struct TofuPrompt: Codable, Equatable {
    var key: UInt64
    var label: String
    var fingerprint: String
}

struct VoiceState: Codable, Equatable {
    var talkTarget: UInt64?
    var muted: Bool
    var deafened: Bool
    var serverLabel: String?
    var channelName: String?
}

struct ConnState: Codable, Equatable {
    /// disconnected, connecting, connected, reconnecting or failed.
    var kind: String
    var label: String

    var isConnected: Bool { kind == "connected" }
    /// A connection is wanted (connected, connecting or retrying).
    var isActive: Bool { kind == "connected" || kind == "connecting" || kind == "reconnecting" }
}

struct Server: Codable, Equatable, Identifiable {
    var key: UInt64
    var label: String
    var addr: String
    var nick: String
    var password: String
    var status: ConnState
    var serverName: String
    var motd: String
    var voicePreset: String
    var myId: UInt32?
    var myChannel: UInt32?
    var defaultChannel: UInt32?
    var channels: [Channel]
    var chat: [ChatLine]

    var id: UInt64 { key }

    func user(_ id: UInt32) -> User? {
        for c in channels {
            if let u = c.users.first(where: { $0.id == id }) { return u }
        }
        return nil
    }
}

struct Channel: Codable, Equatable, Identifiable {
    var id: UInt32
    var name: String
    var permanent: Bool
    var users: [User]
}

struct User: Codable, Equatable, Identifiable {
    var id: UInt32
    var nick: String
    var me: Bool
    var muted: Bool
    var deafened: Bool
    var sharing: Bool
    /// My volume for this user, 0.0-2.0.
    var volume: Double
    var mutedForMe: Bool
    var watching: Bool
    var streamMuted: Bool
}

struct ChatLine: Codable, Equatable {
    var from: String
    var text: String
    var tsMs: UInt64
    var image: ChatImage?
}

struct ChatImage: Codable, Equatable {
    var id: UInt64
    var w: UInt32
    var h: UInt32
    /// The pixels are still kept (else "[image expired]").
    var available: Bool
}

struct StreamItem: Codable, Equatable, Identifiable {
    var key: UInt64
    var server: String
    var user: UInt32
    var nick: String
    var soundMuted: Bool

    var id: String { "\(key)/\(user)" }
}
