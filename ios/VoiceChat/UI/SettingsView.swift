import AVKit
import SwiftUI

/// Voice and profile settings. There is no screen-share, shortcuts or update
/// section on the phone.
struct SettingsView: View {
    @Environment(VCCore.self) private var core
    @Environment(\.dismiss) private var dismiss
    @State private var vad: Double?
    @State private var volume: Double?
    @State private var nick = ""
    @State private var nickLoaded = false
    @AppStorage("vcEchoCancel") private var echoCancel = true
    @AppStorage("vcSpeaker") private var speaker = true

    var body: some View {
        let s = core.snapshot.settings
        NavigationStack {
            Form {
                Section("Input") {
                    Picker("Input mode", selection: Binding(
                        get: { core.snapshot.settings.inputMode },
                        set: { core.send(.setInputMode(mode: $0)) }
                    )) {
                        Text("Voice activity").tag("Vad")
                        Text("Push-to-talk").tag("Ptt")
                        Text("Always on").tag("Open")
                    }
                    .accessibilityIdentifier("settings.inputMode")
                    VStack(alignment: .leading, spacing: 6) {
                        let threshold = vad ?? s.vadThresholdDb
                        HStack {
                            Text("Voice activity threshold")
                            Spacer()
                            Text("\(Int(threshold.rounded())) dB")
                                .font(.callout.monospacedDigit())
                                .foregroundStyle(Theme.muted)
                        }
                        LevelMeter(level: Double(core.live.inputLevelDb), threshold: threshold)
                            .frame(height: 8)
                            .accessibilityIdentifier("settings.meter")
                        Slider(value: Binding(
                            get: { threshold },
                            set: { v in
                                vad = v
                                core.send(.setVadThreshold(db: v, persist: false))
                            }
                        ), in: -60...0, step: 1) { editing in
                            if !editing, let v = vad {
                                core.send(.setVadThreshold(db: v, persist: true))
                                vad = nil
                            }
                        }
                        .accessibilityIdentifier("settings.vad")
                    }
                }
                Section("Output") {
                    let v = volume ?? s.masterVolume
                    HStack {
                        Text("Volume")
                        Slider(value: Binding(
                            get: { v },
                            set: { nv in
                                volume = nv
                                core.send(.setMasterVolume(volume: nv, persist: false))
                            }
                        ), in: 0...2, step: 0.05) { editing in
                            if !editing, let nv = volume {
                                core.send(.setMasterVolume(volume: nv, persist: true))
                                volume = nil
                            }
                        }
                        .accessibilityIdentifier("settings.volume")
                        Text("\(Int((v * 100).rounded())) %")
                            .font(.callout.monospacedDigit())
                            .frame(width: 56, alignment: .trailing)
                    }
                    HStack {
                        Text("Audio route")
                        Spacer()
                        RoutePicker()
                            .frame(width: 36, height: 36)
                            .accessibilityIdentifier("settings.route")
                    }
                }
                Section {
                    Toggle("Echo cancellation", isOn: $echoCancel)
                        .accessibilityIdentifier("settings.echo")
                        .onChange(of: echoCancel) { _, _ in core.restartEngine() }
                    Toggle("Use the loudspeaker", isOn: $speaker)
                        .disabled(!echoCancel)
                        .accessibilityIdentifier("settings.speaker")
                        .onChange(of: speaker) { _, _ in core.restartEngine() }
                } header: {
                    Text("Phone audio")
                } footer: {
                    VStack(alignment: .leading, spacing: 4) {
                        if let note = core.audioNote {
                            Text(note).foregroundStyle(Theme.amber)
                        }
                        if core.audioMode != .external {
                            Text("Audio mode: \(core.audioMode.rawValue) (no microphone).")
                        }
                        Text("Without echo cancellation, sound plays from the earpiece so the microphone does not hear it.")
                    }
                }
                Section("Profile") {
                    TextField("Nickname", text: $nick)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .onSubmit(commitNick)
                        .accessibilityIdentifier("settings.nick")
                }
                Section("About") {
                    LabeledContent("Version", value: core.version)
                        .accessibilityIdentifier("settings.version")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        commitNick()
                        dismiss()
                    }
                    .accessibilityIdentifier("settings.done")
                }
            }
            .onAppear {
                if !nickLoaded {
                    nickLoaded = true
                    nick = s.nickname.isEmpty ? core.snapshot.defaultNick : s.nickname
                }
            }
            .onDisappear(perform: commitNick)
        }
    }

    private func commitNick() {
        let n = nick.trimmingCharacters(in: .whitespaces)
        guard !n.isEmpty, n != core.snapshot.settings.nickname else { return }
        core.send(.setNickname(nick: n))
    }
}

/// The input level against the VAD threshold.
private struct LevelMeter: View {
    var level: Double
    var threshold: Double

    var body: some View {
        GeometryReader { g in
            let frac = { (db: Double) in CGFloat(max(0, min(1, (db + 60) / 60))) }
            ZStack(alignment: .leading) {
                Capsule().fill(Theme.hover)
                Capsule()
                    .fill(level >= threshold ? Theme.accent : Theme.off)
                    .frame(width: g.size.width * frac(level))
                Rectangle()
                    .fill(Theme.amber)
                    .frame(width: 2)
                    .offset(x: g.size.width * frac(threshold) - 1)
            }
        }
        .accessibilityLabel("Input level \(Int(level)) dB")
    }
}

/// The system audio route picker (speaker, Bluetooth, AirPlay).
private struct RoutePicker: UIViewRepresentable {
    func makeUIView(context: Context) -> AVRoutePickerView {
        let v = AVRoutePickerView()
        v.tintColor = UIColor(Theme.muted)
        v.activeTintColor = UIColor(Theme.accent)
        return v
    }

    func updateUIView(_ uiView: AVRoutePickerView, context: Context) {}
}
