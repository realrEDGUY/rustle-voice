import SwiftUI

/// One server: header (name, MOTD, chips), watched streams, channels with their
/// users. Tapping a channel joins it.
struct ServerView: View {
    @Environment(VCCore.self) private var core
    let key: UInt64
    @State private var newChannel = false
    @State private var renaming: Channel?
    @State private var nameDraft = ""
    @State private var menuUser: User?

    private var server: Server? { core.snapshot.server(key) }

    var body: some View {
        Group {
            if let s = server {
                content(s)
            }
        }
        .background(Theme.channels)
        .navigationTitle(server?.label ?? "")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if server?.status.isConnected == true {
                ToolbarItem(placement: .primaryAction) {
                    NavigationLink {
                        ChatView(key: key)
                    } label: {
                        Label("Chat", systemImage: "bubble.left.and.bubble.right")
                    }
                    .accessibilityIdentifier("server.chat")
                }
                ToolbarItem(placement: .secondaryAction) {
                    Button {
                        nameDraft = ""
                        newChannel = true
                    } label: {
                        Label("New channel", systemImage: "plus")
                    }
                    .accessibilityIdentifier("server.newChannel")
                }
            }
        }
        .alert("New channel", isPresented: $newChannel) {
            TextField("Name", text: $nameDraft)
                .accessibilityIdentifier("channel.nameField")
            Button("Create") { core.send(.createChannel(key: key, name: nameDraft)) }
                .accessibilityIdentifier("channel.create")
            Button("Cancel", role: .cancel) {}
        }
        .alert(
            "Rename channel",
            isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } }),
            presenting: renaming
        ) { ch in
            TextField("Name", text: $nameDraft)
                .accessibilityIdentifier("channel.renameField")
            Button("Rename") { core.send(.renameChannel(key: key, channel: ch.id, name: nameDraft)) }
                .accessibilityIdentifier("channel.rename")
            Button("Cancel", role: .cancel) {}
        }
        .sheet(item: $menuUser) { u in
            UserMenuSheet(key: key, userID: u.id)
                .presentationDetents([.medium, .large])
        }
    }

    @ViewBuilder
    private func content(_ s: Server) -> some View {
        List {
            Section {
                header(s)
            }
            .listRowBackground(Theme.channels)
            if !core.snapshot.streams.isEmpty {
                Section {
                    StreamsView(compact: true)
                        .listRowInsets(EdgeInsets())
                }
                .listRowBackground(Theme.stage)
            }
            if s.status.isConnected {
                Section("Voice channels") {
                    ForEach(s.channels) { ch in
                        channelRow(s, ch)
                        ForEach(ch.users) { u in
                            userRow(s, u)
                        }
                    }
                }
                .listRowBackground(Theme.channels)
            } else {
                Section {
                    notConnected(s)
                }
                .listRowBackground(Theme.card)
            }
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
    }

    private func header(_ s: Server) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(s.serverName.isEmpty ? s.label : s.serverName)
                .font(.headline)
                .foregroundStyle(Theme.text)
                .accessibilityIdentifier("server.name")
            if !s.motd.isEmpty {
                Text(s.motd)
                    .font(.subheadline)
                    .foregroundStyle(Theme.muted)
                    .accessibilityIdentifier("server.motd")
            }
            HStack(spacing: 6) {
                Chip(text: s.status.label, color: Theme.statusColor(s.status))
                    .accessibilityIdentifier("server.status")
                if s.status.isConnected, !s.voicePreset.isEmpty {
                    Chip(text: "Voice: \(s.voicePreset)")
                        .accessibilityIdentifier("server.preset")
                }
                if s.status.isConnected, let ms = core.live.server(key)?.pingMs {
                    Chip(text: "\(ms) ms", color: Theme.dim)
                }
            }
        }
        .padding(.vertical, 4)
    }

    private func notConnected(_ s: Server) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(s.status.kind == "failed" ? s.status.label : "Connect to see channels.")
                .foregroundStyle(s.status.kind == "failed" ? Theme.red : Theme.muted)
            if s.status.isActive {
                Button("Cancel") { core.send(.disconnect(key: key)) }
                    .buttonStyle(.bordered)
            } else {
                Button("Connect") { core.send(.connect(key: key)) }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("server.connect")
            }
        }
        .padding(.vertical, 6)
    }

    private func channelRow(_ s: Server, _ ch: Channel) -> some View {
        let mine = s.myChannel == ch.id
        return Button {
            if !mine { core.send(.join(key: key, channel: ch.id)) }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "speaker.wave.2.fill")
                    .font(.caption)
                    .foregroundStyle(mine ? Theme.accent : Theme.dim)
                Text(ch.name)
                    .font(.body.weight(mine ? .semibold : .regular))
                    .foregroundStyle(mine ? Theme.text : Theme.muted)
                if !ch.permanent {
                    Chip(text: "temp", color: Theme.amber)
                        .accessibilityIdentifier("channel.temp")
                }
                Spacer()
                if !ch.users.isEmpty {
                    Text("\(ch.users.count)")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(Theme.dim)
                }
            }
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("channel.\(ch.name)")
        .contextMenu {
            if !mine {
                Button {
                    core.send(.join(key: key, channel: ch.id))
                } label: {
                    Label("Join", systemImage: "arrow.right.circle")
                }
            }
            Button {
                nameDraft = ch.name
                renaming = ch
            } label: {
                Label("Rename", systemImage: "pencil")
            }
            Button {
                nameDraft = ""
                newChannel = true
            } label: {
                Label("New channel", systemImage: "plus")
            }
            Button(role: .destructive) {
                core.send(.deleteChannel(key: key, channel: ch.id))
            } label: {
                Label("Delete", systemImage: "trash")
            }
            .disabled(ch.id == s.defaultChannel)
        }
    }

    private func userRow(_ s: Server, _ u: User) -> some View {
        let speaking = core.live.isSpeaking(key, u.id)
        return HStack(spacing: 10) {
            Avatar(nick: u.nick, size: 28, speaking: speaking)
            Text(u.me ? "\(u.nick) (you)" : u.nick)
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            if u.sharing {
                Text("LIVE")
                    .font(.caption2.weight(.heavy))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(RoundedRectangle(cornerRadius: 4).fill(Theme.red))
                    .accessibilityIdentifier("user.live")
            }
            Spacer()
            if u.mutedForMe {
                Image(systemName: "speaker.slash.fill")
                    .foregroundStyle(Theme.amber)
                    .accessibilityLabel("Muted for me")
            }
            if u.muted {
                Image(systemName: "mic.slash.fill")
                    .foregroundStyle(Theme.red)
                    .accessibilityLabel("Muted")
            }
            if u.deafened {
                Image(systemName: "headphones")
                    .foregroundStyle(Theme.red)
                    .accessibilityLabel("Deafened")
            }
        }
        .font(.subheadline)
        .padding(.leading, 18)
        .contentShape(Rectangle())
        .onTapGesture {
            if !u.me { menuUser = u }
        }
        .contextMenu {
            if !u.me { UserMenuItems(key: key, user: u) }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(userLabel(u, speaking))
        .accessibilityIdentifier("user.\(u.nick)")
    }

    private func userLabel(_ u: User, _ speaking: Bool) -> String {
        var parts = [u.me ? "\(u.nick) (you)" : u.nick]
        if u.sharing { parts.append("live") }
        if u.muted { parts.append("muted") }
        if u.deafened { parts.append("deafened") }
        if u.mutedForMe { parts.append("muted for me") }
        if speaking { parts.append("speaking") }
        return parts.joined(separator: ", ")
    }
}
