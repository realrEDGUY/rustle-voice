import SwiftUI

/// The bottom bar: "Voice connected" (ping, channel / server, Leave voice), me,
/// the transmit light, Mute, Deafen, hold-to-talk in PTT mode, and Settings.
struct VoiceBar: View {
    @Environment(VCCore.self) private var core
    @Environment(\.scenePhase) private var scenePhase
    @Binding var showSettings: Bool
    @GestureState private var holding = false

    var body: some View {
        let snap = core.snapshot
        let voice = snap.voice
        VStack(spacing: 8) {
            if let target = voice.talkTarget {
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 6) {
                            Text("Voice connected")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(Theme.accent)
                            if let ms = core.live.server(target)?.pingMs {
                                Text("\(ms) ms")
                                    .font(.caption2.monospacedDigit())
                                    .foregroundStyle(Theme.dim)
                                    .accessibilityIdentifier("voice.ping")
                            }
                        }
                        Text([voice.channelName, voice.serverLabel].compactMap { $0 }.joined(separator: " / "))
                            .font(.caption)
                            .foregroundStyle(Theme.muted)
                            .lineLimit(1)
                            .accessibilityIdentifier("voice.where")
                    }
                    Spacer()
                    Button {
                        core.send(.disconnect(key: target))
                    } label: {
                        Image(systemName: "phone.down.fill")
                            .foregroundStyle(Theme.red)
                            .padding(8)
                            .background(Circle().fill(Theme.hover))
                    }
                    .accessibilityLabel("Leave voice")
                    .accessibilityIdentifier("voice.leave")
                }
            }
            HStack(spacing: 10) {
                let nick = myNick ?? (snap.settings.nickname.isEmpty ? snap.defaultNick : snap.settings.nickname)
                ZStack(alignment: .bottomTrailing) {
                    Avatar(nick: nick, size: 30, speaking: core.live.transmitting)
                    Circle()
                        .fill(core.live.transmitting ? Theme.accent : Theme.off)
                        .frame(width: 9, height: 9)
                        .accessibilityLabel(core.live.transmitting ? "Transmitting" : "Not transmitting")
                        .accessibilityIdentifier("voice.txLight")
                }
                VStack(alignment: .leading, spacing: 0) {
                    Text(nick)
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    Text(modeLabel(snap.settings.inputMode))
                        .font(.caption2)
                        .foregroundStyle(Theme.dim)
                }
                Spacer()
                if snap.settings.inputMode == "Ptt" {
                    pttButton
                }
                muteButton(voice)
                Button {
                    core.send(.toggleDeafen)
                } label: {
                    barIcon(voice.deafened ? "headphones.slash" : "headphones", active: voice.deafened)
                }
                .accessibilityLabel(voice.deafened ? "Undeafen" : "Deafen")
                .accessibilityIdentifier("voice.deafen")
                Button {
                    showSettings = true
                } label: {
                    barIcon("gearshape.fill", active: false)
                }
                .accessibilityLabel("Settings")
                .accessibilityIdentifier("voice.settings")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        // The colour runs under the home indicator; the controls stay in the safe area.
        .background(Theme.meBar.ignoresSafeArea(edges: .bottom))
        .overlay(alignment: .top) { Divider().background(Theme.border) }
        .onChange(of: holding) { _, down in core.setPTT(down) }
        .onChange(of: scenePhase) { _, phase in
            if phase != .active { core.setPTT(false) }
        }
        .onDisappear { core.setPTT(false) }
    }

    private var myNick: String? {
        guard let k = core.snapshot.voice.talkTarget ?? core.snapshot.selected,
              let s = core.snapshot.server(k)
        else { return nil }
        return s.nick
    }

    private func modeLabel(_ m: String) -> String {
        switch m {
        case "Ptt": return "Push-to-talk"
        case "Open": return "Always on"
        default: return "Voice activity"
        }
    }

    private func barIcon(_ name: String, active: Bool) -> some View {
        Image(systemName: name)
            .font(.system(size: 17, weight: .semibold))
            .foregroundStyle(active ? Theme.red : Theme.text)
            .frame(width: 40, height: 40)
            .background(Circle().fill(active ? Theme.red.opacity(0.18) : Theme.hover))
    }

    private func muteButton(_ voice: VoiceState) -> some View {
        Menu {
            Picker("Input mode", selection: Binding(
                get: { core.snapshot.settings.inputMode },
                set: { core.send(.setInputMode(mode: $0)) }
            )) {
                Text("Voice activity").tag("Vad")
                Text("Push-to-talk").tag("Ptt")
                Text("Always on").tag("Open")
            }
        } label: {
            barIcon(voice.muted ? "mic.slash.fill" : "mic.fill", active: voice.muted)
        } primaryAction: {
            core.send(.toggleMute)
        }
        .accessibilityLabel(voice.muted ? "Unmute" : "Mute")
        .accessibilityHint("Long-press to change the input mode")
        .accessibilityIdentifier("voice.mute")
    }

    /// Hold to talk. The gesture state resets itself when the touch ends or is
    /// cancelled, so a lost touch never leaves the mic open.
    private var pttButton: some View {
        Text(holding ? "Talking" : "Hold to talk")
            .font(.footnote.weight(.semibold))
            .foregroundStyle(holding ? Theme.rail : Theme.text)
            .padding(.horizontal, 12)
            .frame(height: 40)
            .background(Capsule().fill(holding ? Theme.accent : Theme.hover))
            .contentShape(Capsule())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .updating($holding) { _, state, _ in state = true }
            )
            .accessibilityLabel("Hold to talk")
            .accessibilityAddTraits(.isButton)
            .accessibilityIdentifier("voice.ptt")
    }
}
