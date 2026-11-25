import SwiftUI
import UIKit

/// Saved servers with their status, the talk target's mic badge and the ping.
struct ServerListView: View {
    @Environment(VCCore.self) private var core
    @Binding var selection: UInt64?
    @State private var form: ServerFormTarget?

    var body: some View {
        List(selection: $selection) {
            ForEach(core.snapshot.servers) { s in
                NavigationLink(value: s.key) {
                    ServerRow(server: s)
                }
                .swipeActions(edge: .trailing) {
                    Button(role: .destructive) {
                        remove(s)
                    } label: {
                        Label("Remove", systemImage: "trash")
                    }
                    Button {
                        form = .edit(s.key)
                    } label: {
                        Label("Edit", systemImage: "pencil")
                    }
                    .tint(Theme.off)
                }
                .swipeActions(edge: .leading) {
                    connectButton(s)
                }
                .contextMenu { menu(s) }
                .listRowBackground(Theme.channels)
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .background(Theme.rail)
        .overlay {
            if core.snapshot.servers.isEmpty {
                ContentUnavailableView {
                    Label("No servers", systemImage: "server.rack")
                } description: {
                    Text("Add a server to start talking.")
                } actions: {
                    Button("Add server") { form = .add }
                        .buttonStyle(.borderedProminent)
                        .accessibilityIdentifier("servers.addEmpty")
                }
            }
        }
        .navigationTitle("Servers")
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    form = .add
                } label: {
                    Label("Add server", systemImage: "plus")
                }
                .accessibilityIdentifier("servers.add")
            }
        }
        .sheet(item: $form) { target in
            ServerFormView(target: target) { key in
                if let key { selection = key }
            }
        }
    }

    @ViewBuilder
    private func connectButton(_ s: Server) -> some View {
        if s.status.isActive {
            Button {
                core.send(.disconnect(key: s.key))
            } label: {
                Label("Disconnect", systemImage: "bolt.slash")
            }
            .tint(Theme.amber)
        } else {
            Button {
                core.send(.connect(key: s.key))
            } label: {
                Label("Connect", systemImage: "bolt")
            }
            .tint(Theme.accent)
        }
    }

    @ViewBuilder
    private func menu(_ s: Server) -> some View {
        connectButton(s)
        if s.status.isConnected, core.snapshot.voice.talkTarget != s.key {
            Button {
                core.send(.talkHere(key: s.key))
            } label: {
                Label("Talk here", systemImage: "mic")
            }
        }
        Button {
            form = .edit(s.key)
        } label: {
            Label("Edit", systemImage: "pencil")
        }
        Button {
            UIPasteboard.general.string = s.addr
        } label: {
            Label("Copy address", systemImage: "doc.on.doc")
        }
        Button(role: .destructive) {
            remove(s)
        } label: {
            Label("Remove", systemImage: "trash")
        }
    }

    private func remove(_ s: Server) {
        if selection == s.key { selection = nil }
        core.send(.removeServer(key: s.key))
    }
}

private struct ServerRow: View {
    @Environment(VCCore.self) private var core
    var server: Server

    var body: some View {
        HStack(spacing: 12) {
            ZStack(alignment: .bottomTrailing) {
                Text(Theme.initials(server.label))
                    .font(.system(size: 15, weight: .bold))
                    .foregroundStyle(Theme.text)
                    .frame(width: 44, height: 44)
                    .background(RoundedRectangle(cornerRadius: 14).fill(Theme.hover))
                Circle()
                    .fill(Theme.statusColor(server.status))
                    .frame(width: 12, height: 12)
                    .overlay(Circle().stroke(Theme.rail, lineWidth: 2))
                    .offset(x: 3, y: 3)
            }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(server.label)
                        .font(.body.weight(.semibold))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                    if core.snapshot.voice.talkTarget == server.key {
                        Image(systemName: "mic.fill")
                            .font(.caption)
                            .foregroundStyle(Theme.accent)
                            .accessibilityLabel("Talking here")
                            .accessibilityIdentifier("server.micBadge")
                    }
                }
                Text(server.status.label)
                    .font(.caption)
                    .foregroundStyle(server.status.kind == "failed" ? Theme.red : Theme.muted)
                    .lineLimit(2)
            }
            Spacer()
            if server.status.isConnected, let ms = core.live.server(server.key)?.pingMs {
                Text("\(ms) ms")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(Theme.dim)
                    .accessibilityIdentifier("server.ping")
            }
        }
        .padding(.vertical, 4)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("server.\(server.label)")
    }
}
