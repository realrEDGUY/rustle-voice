import SwiftUI
import UIKit

/// Servers → server (channels) → chat, with the voice bar pinned at the bottom,
/// toasts, banners and the TOFU alert on top. A stack on iPhone, columns on iPad.
struct RootView: View {
    @Environment(VCCore.self) private var core
    @Environment(\.horizontalSizeClass) private var sizeClass
    @State private var selection: UInt64?
    @State private var showSettings = false
    /// The keyboard is up: the voice bar steps aside so the chat composer sits on it.
    @State private var keyboardShown = false

    var body: some View {
        // Banners, the navigation and the voice bar are stacked, not inset: on a
        // compact phone a safe-area inset on the split view does not reach the
        // pushed screens, and the bar would cover their bottom (the composer).
        VStack(spacing: 0) {
            Banners()
            NavigationSplitView {
                ServerListView(selection: $selection)
            } detail: {
                NavigationStack {
                    if let key = selection, core.snapshot.server(key) != nil {
                        ServerView(key: key)
                            .id(key)
                    } else {
                        EmptyServerView()
                    }
                }
            }
            .navigationSplitViewStyle(.balanced)
            if !keyboardShown {
                VoiceBar(showSettings: $showSettings)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { _ in
            withAnimation(.easeOut(duration: 0.2)) { keyboardShown = true }
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)) { _ in
            withAnimation(.easeOut(duration: 0.2)) { keyboardShown = false }
        }
        .overlay(alignment: .top) { ToastOverlay() }
        .sheet(isPresented: $showSettings) {
            SettingsView()
        }
        .alert(
            tofuTitle,
            isPresented: Binding(
                get: { core.snapshot.tofu != nil },
                // The two buttons answer the prompt; the core then clears it.
                set: { _ in }
            ),
            presenting: core.snapshot.tofu
        ) { _ in
            Button("Trust new identity", role: .destructive) { core.send(.trustNewIdentity) }
                .accessibilityIdentifier("tofu.trust")
            Button("Stay safe, cancel", role: .cancel) { core.send(.cancelTofu) }
                .accessibilityIdentifier("tofu.cancel")
        } message: { t in
            Text("The certificate of \(t.label) is not the one seen before. Only trust it if the server owner regenerated it.\n\nNew fingerprint:\n\(t.fingerprint)")
        }
        .onChange(of: core.snapshot.selected) { _, new in
            // Follow the core's selection (a new server is selected when added).
            if let new, selection != new, core.snapshot.server(new) != nil, selection == nil || core.snapshot.server(selection) == nil {
                selection = new
            }
        }
        .onAppear {
            // Side by side (iPad), open the selected server at once; on a phone the
            // list comes first.
            if sizeClass == .regular, selection == nil {
                selection = core.snapshot.selected
            }
        }
        .onChange(of: selection) { _, new in
            if let new, new != core.snapshot.selected {
                core.send(.select(key: new))
            }
        }
        .background(Theme.bg.ignoresSafeArea())
    }

    private var tofuTitle: String {
        "\(core.snapshot.tofu?.label ?? "Server") has a new identity"
    }
}

/// Audio, bridge and poisoned-core banners.
private struct Banners: View {
    @Environment(VCCore.self) private var core

    var body: some View {
        VStack(spacing: 0) {
            if core.poisoned {
                banner("Voice core stopped, restart the app", Theme.red, "banner.poisoned")
            }
            if let e = core.bridgeError {
                banner("Bridge decode error: \(e)", Theme.red, "banner.bridge")
            }
            if let w = core.snapshot.audioWarning {
                banner(w, Theme.amber, "banner.audio")
            }
        }
    }

    private func banner(_ text: String, _ color: Color, _ id: String) -> some View {
        Text(text)
            .font(.footnote)
            .foregroundStyle(color)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(Theme.rail)
            .accessibilityIdentifier(id)
    }
}

/// Toasts at the top; each dismisses itself after 5 s.
private struct ToastOverlay: View {
    @Environment(VCCore.self) private var core

    var body: some View {
        VStack(spacing: 6) {
            ForEach(core.snapshot.toasts, id: \.self) { t in
                Toast(text: t)
            }
        }
        .padding(.top, 8)
        .padding(.horizontal, 12)
        .animation(.default, value: core.snapshot.toasts)
    }
}

private struct Toast: View {
    @Environment(VCCore.self) private var core
    var text: String

    var body: some View {
        Text(text)
            .font(.footnote)
            .foregroundStyle(text.hasPrefix("Error") ? Theme.red : Theme.text)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 10).fill(Theme.card))
            .overlay(RoundedRectangle(cornerRadius: 10).stroke(Theme.border))
            .onTapGesture { core.send(.dismissToast(text: text)) }
            .task {
                try? await Task.sleep(for: .seconds(5))
                core.send(.dismissToast(text: text))
            }
            .accessibilityIdentifier("toast")
            .accessibilityLabel(text)
    }
}

private struct EmptyServerView: View {
    @Environment(VCCore.self) private var core

    var body: some View {
        ContentUnavailableView {
            Label("No server selected", systemImage: "server.rack")
        } description: {
            Text(core.snapshot.servers.isEmpty
                ? "Add a server with the + button to start talking."
                : "Pick a server from the list.")
        }
        .background(Theme.bg)
    }
}
