import SwiftUI
import UIKit

/// The user menu's actions, shared by the context menu and the sheet.
struct UserMenuItems: View {
    @Environment(VCCore.self) private var core
    let key: UInt64
    let user: User

    var body: some View {
        if user.sharing {
            Button {
                core.send(.watchScreen(key: key, user: user.id, on: !user.watching))
            } label: {
                Label(user.watching ? "Stop watching" : "Watch stream",
                      systemImage: user.watching ? "eye.slash" : "play.rectangle")
            }
            if user.watching {
                Button {
                    core.send(.muteStream(key: key, user: user.id, muted: !user.streamMuted))
                } label: {
                    Label(user.streamMuted ? "Unmute stream sound" : "Mute stream sound",
                          systemImage: user.streamMuted ? "speaker.wave.2" : "speaker.slash")
                }
            }
        }
        Button {
            core.send(.muteUser(key: key, nick: user.nick, muted: !user.mutedForMe))
        } label: {
            Label(user.mutedForMe ? "Unmute for me" : "Mute for me",
                  systemImage: user.mutedForMe ? "speaker.wave.2" : "speaker.slash")
        }
        if user.volume != 1 {
            Button {
                core.send(.setUserVolume(key: key, nick: user.nick, volume: 1))
            } label: {
                Label("Reset volume", systemImage: "arrow.counterclockwise")
            }
        }
        Button {
            UIPasteboard.general.string = user.nick
        } label: {
            Label("Copy nickname", systemImage: "doc.on.doc")
        }
    }
}

/// A sheet for one user: volume slider plus the menu's actions.
struct UserMenuSheet: View {
    @Environment(VCCore.self) private var core
    @Environment(\.dismiss) private var dismiss
    let key: UInt64
    let userID: UInt32

    private var user: User? { core.snapshot.server(key)?.user(userID) }

    var body: some View {
        NavigationStack {
            if let u = user {
                List {
                    Section {
                        HStack(spacing: 12) {
                            Avatar(nick: u.nick, size: 44, speaking: core.live.isSpeaking(key, u.id))
                            VStack(alignment: .leading) {
                                Text(u.nick).font(.headline)
                                if u.sharing {
                                    Text(u.watching ? "Sharing their screen (watching)" : "Sharing their screen")
                                        .font(.caption)
                                        .foregroundStyle(Theme.red)
                                }
                            }
                        }
                    }
                    Section("Volume") {
                        HStack {
                            Image(systemName: "speaker.fill").foregroundStyle(Theme.dim)
                            // Bound straight to the core: every change is sent (and saved,
                            // as on the desktop) and the snapshot is current at once.
                            Slider(
                                value: Binding(
                                    get: { u.volume },
                                    set: { core.send(.setUserVolume(key: key, nick: u.nick, volume: $0)) }
                                ),
                                in: 0...2,
                                step: 0.05
                            )
                            .accessibilityIdentifier("user.volume")
                            Text("\(Int((u.volume * 100).rounded())) %")
                                .font(.caption.monospacedDigit())
                                .frame(width: 52, alignment: .trailing)
                        }
                    }
                    Section {
                        UserMenuItems(key: key, user: u)
                    }
                }
                .navigationTitle(u.nick)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { dismiss() }
                    }
                }
            } else {
                ContentUnavailableView("User left", systemImage: "person.slash")
            }
        }
    }
}
