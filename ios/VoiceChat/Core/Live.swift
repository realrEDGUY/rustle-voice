import Foundation

// Codable mirror of `view::Live` (crates/vc-mobile/src/view.rs): the values that
// change many times a second, kept apart so the structural snapshot stays quiet.

struct Live: Codable, Equatable {
    var transmitting: Bool
    /// Rounded to a whole dB (-100 to 0).
    var inputLevelDb: Int
    var servers: [LiveServer]

    static let empty = Live(transmitting: false, inputLevelDb: -100, servers: [])

    func server(_ key: UInt64?) -> LiveServer? {
        guard let key else { return nil }
        return servers.first { $0.key == key }
    }

    func isSpeaking(_ key: UInt64, _ user: UInt32) -> Bool {
        server(key)?.speaking.contains(user) ?? false
    }
}

struct LiveServer: Codable, Equatable {
    var key: UInt64
    var pingMs: UInt64?
    var speaking: [UInt32]
}
