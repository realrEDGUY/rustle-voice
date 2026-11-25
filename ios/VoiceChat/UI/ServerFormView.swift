import SwiftUI

enum ServerFormTarget: Identifiable, Hashable {
    case add
    case edit(UInt64)

    var id: String {
        switch self {
        case .add: return "add"
        case let .edit(k): return "edit-\(k)"
        }
    }
}

/// Add or edit a saved server. The drafts stay here; `SaveServer` submits them
/// and its error shows inline.
struct ServerFormView: View {
    @Environment(VCCore.self) private var core
    @Environment(\.dismiss) private var dismiss
    var target: ServerFormTarget
    /// Called with the server's key after a successful save.
    var onSaved: (UInt64?) -> Void

    @State private var addr = "127.0.0.1:7878"
    @State private var nick = ""
    @State private var label = ""
    @State private var password = ""
    @State private var connectNow = true
    @State private var error: String?
    @State private var loaded = false

    private var editing: UInt64? {
        if case let .edit(k) = target { return k }
        return nil
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Address", text: $addr, prompt: Text("host:port"))
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                        .accessibilityIdentifier("form.addr")
                    TextField("Nickname", text: $nick)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .accessibilityIdentifier("form.nick")
                } header: {
                    Text("Server")
                } footer: {
                    if let error {
                        Text(error)
                            .foregroundStyle(Theme.red)
                            .accessibilityIdentifier("form.error")
                    }
                }
                Section("More") {
                    TextField("Label (optional)", text: $label)
                        .accessibilityIdentifier("form.label")
                    SecureField("Password (optional)", text: $password)
                        .accessibilityIdentifier("form.password")
                }
                Section {
                    Toggle("Connect now", isOn: $connectNow)
                        .accessibilityIdentifier("form.connectNow")
                }
            }
            .scrollContentBackground(.hidden)
            .background(Theme.bg)
            .navigationTitle(editing == nil ? "Add server" : "Edit server")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                        .accessibilityIdentifier("form.cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(editing == nil ? "Add" : "Save") { save() }
                        .accessibilityIdentifier("form.save")
                }
            }
            .onSubmit { save() }
            .onAppear(perform: load)
        }
    }

    private func load() {
        guard !loaded else { return }
        loaded = true
        if let k = editing, let s = core.snapshot.server(k) {
            addr = s.addr
            nick = s.nick
            label = s.label
            password = s.password
            connectNow = true
        } else {
            nick = core.snapshot.defaultNick
        }
    }

    private func save() {
        let before = Set(core.snapshot.servers.map(\.key))
        if let e = core.send(.saveServer(editing: editing, label: label, addr: addr, nick: nick, password: password, connectNow: connectNow)) {
            error = e
            return
        }
        let added = core.snapshot.servers.map(\.key).first { !before.contains($0) }
        onSaved(editing ?? added)
        dismiss()
    }
}
