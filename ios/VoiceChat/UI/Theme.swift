import SwiftUI

/// The desktop client's palette (crates/vc-client/src/gui/theme.rs).
enum Theme {
    static let rail = Color(hex: 0x0B0D10)
    static let channels = Color(hex: 0x121418)
    static let bg = Color(hex: 0x16191E)
    static let meBar = Color(hex: 0x0F1114)
    static let card = Color(hex: 0x181B20)
    static let stage = Color(hex: 0x0B0D10)
    static let border = Color(hex: 0x2A3038)
    static let hover = Color(hex: 0x1C2026)
    static let accent = Color(hex: 0x4ADE9A)
    static let red = Color(hex: 0xF26B6B)
    static let amber = Color(hex: 0xE5B94E)
    static let off = Color(hex: 0x5A626D)
    static let text = Color(hex: 0xE8EAED)
    static let muted = Color(hex: 0x9BA4AF)
    static let dim = Color(hex: 0x7A838E)

    private static let avatars: [Color] = [
        Color(hex: 0x5B8DEF), Color(hex: 0xE0805A), Color(hex: 0x9B7BE8),
        Color(hex: 0x3FB8AF), Color(hex: 0xD9A23B), Color(hex: 0xE46F9A),
        Color(hex: 0x7D8A99), Color(hex: 0x6FB35B), Color(hex: 0xC76BD1),
    ]

    /// FNV-1a of the nick picks one of 9 colours, as on the desktop.
    static func avatarColor(_ nick: String) -> Color {
        var h: UInt32 = 0x811C_9DC5
        for b in nick.utf8 {
            h ^= UInt32(b)
            h = h &* 0x0100_0193
        }
        return avatars[Int(h % UInt32(avatars.count))]
    }

    /// Up to two initials: first letters of the first two words, else the first
    /// two characters, uppercased.
    static func initials(_ name: String) -> String {
        let words = name.split(whereSeparator: { $0.isWhitespace })
        let s: String
        if words.count >= 2 {
            s = String(words.prefix(2).compactMap { $0.first })
        } else {
            s = String(name.trimmingCharacters(in: .whitespaces).prefix(2))
        }
        return s.uppercased()
    }

    static func statusColor(_ s: ConnState) -> Color {
        switch s.kind {
        case "connected": return accent
        case "connecting", "reconnecting": return amber
        case "failed": return red
        default: return off
        }
    }

    /// `HH:MM:SS UTC`, as on the desktop.
    static func utcTime(_ ms: UInt64) -> String {
        let day = (ms / 1000) % 86_400
        return String(format: "%02d:%02d:%02d UTC", Int(day / 3600), Int((day % 3600) / 60), Int(day % 60))
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255
        )
    }
}

/// A round avatar with initials and an optional speaking ring.
struct Avatar: View {
    var nick: String
    var size: CGFloat = 32
    var speaking = false

    var body: some View {
        Text(Theme.initials(nick))
            .font(.system(size: size * 0.4, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: size, height: size)
            .background(Circle().fill(Theme.avatarColor(nick)))
            .overlay(
                Circle()
                    .stroke(Theme.accent, lineWidth: speaking ? 2.5 : 0)
                    .padding(-3)
            )
            .accessibilityHidden(true)
    }
}

/// A small rounded label, like the desktop's chips.
struct Chip: View {
    var text: String
    var color: Color = Theme.muted

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .foregroundStyle(color)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(RoundedRectangle(cornerRadius: 6).fill(Theme.hover))
    }
}
