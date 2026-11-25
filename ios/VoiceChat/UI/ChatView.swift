import PhotosUI
import SwiftUI
import UIKit

/// Chat of my channel on one server: lines grouped by sender within 5 minutes,
/// UTC times, image thumbnails, and a composer with a 500-character limit.
struct ChatView: View {
    @Environment(VCCore.self) private var core
    let key: UInt64
    @State private var draft = ""
    @State private var error: String?
    @State private var pick: PhotosPickerItem?
    @State private var showPicker = false
    @State private var viewing: ViewedImage?
    @FocusState private var focused: Bool

    static let maxChars = 500

    private var server: Server? { core.snapshot.server(key) }

    var body: some View {
        VStack(spacing: 0) {
            messages
            composer
        }
        .background(Theme.bg)
        .navigationTitle(channelName)
        .navigationBarTitleDisplayMode(.inline)
        .photosPicker(isPresented: $showPicker, selection: $pick, matching: .images)
        .onChange(of: pick) { _, item in
            guard let item else { return }
            pick = nil
            Task { await send(item) }
        }
        .fullScreenCover(item: $viewing) { v in
            ImageViewer(image: v.image, title: v.title)
        }
        .onAppear {
            if core.snapshot.selected != key { core.send(.select(key: key)) }
        }
    }

    private var channelName: String {
        guard let s = server, let ch = s.channels.first(where: { $0.id == s.myChannel }) else { return "Chat" }
        return "# \(ch.name)"
    }

    private var messages: some View {
        let lines = server?.chat ?? []
        return ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    if lines.isEmpty {
                        Text("No messages yet.")
                            .foregroundStyle(Theme.dim)
                            .frame(maxWidth: .infinity)
                            .padding(.top, 40)
                    }
                    ForEach(Array(lines.enumerated()), id: \.offset) { i, line in
                        let head = i == 0 || lines[i - 1].from != line.from
                            || line.tsMs >= lines[i - 1].tsMs + 5 * 60 * 1000
                        ChatRow(key: key, line: line, head: head) { img in
                            viewing = ViewedImage(image: img, title: "Image from \(line.from)")
                        }
                        .id(i)
                    }
                    Color.clear.frame(height: 1).id("bottom")
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
            }
            .scrollDismissesKeyboard(.interactively)
            .defaultScrollAnchor(.bottom)
            .onChange(of: lines.count) { _, _ in
                withAnimation { proxy.scrollTo("bottom", anchor: .bottom) }
            }
            .onAppear { proxy.scrollTo("bottom", anchor: .bottom) }
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(Theme.red)
                    .accessibilityIdentifier("chat.error")
            }
            HStack(alignment: .bottom, spacing: 8) {
                Menu {
                    Button {
                        showPicker = true
                    } label: {
                        Label("Photo library", systemImage: "photo")
                    }
                    PasteButton(payloadType: Data.self) { items in
                        guard let data = items.first, let img = UIImage(data: data) else { return }
                        Task { @MainActor in upload(img) }
                    }
                } label: {
                    Image(systemName: "plus.circle.fill")
                        .font(.title2)
                        .foregroundStyle(Theme.muted)
                }
                .accessibilityLabel("Send image")
                .accessibilityIdentifier("chat.attach")
                TextField("Message", text: $draft, axis: .vertical)
                    .lineLimit(1...5)
                    .focused($focused)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 8)
                    .background(RoundedRectangle(cornerRadius: 10).fill(Theme.hover))
                    .onChange(of: draft) { _, new in
                        if new.count > Self.maxChars { draft = String(new.prefix(Self.maxChars)) }
                    }
                    .onSubmit(sendText)
                    .submitLabel(.send)
                    .accessibilityIdentifier("chat.input")
                Button(action: sendText) {
                    Image(systemName: "arrow.up.circle.fill")
                        .font(.title2)
                        .foregroundStyle(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? Theme.off : Theme.accent)
                }
                .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                .accessibilityLabel("Send")
                .accessibilityIdentifier("chat.send")
            }
            Text("\(draft.count)/\(Self.maxChars)")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(draft.count >= Self.maxChars ? Theme.amber : Theme.dim)
                .frame(maxWidth: .infinity, alignment: .trailing)
                .accessibilityIdentifier("chat.counter")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Theme.channels)
    }

    private func sendText() {
        let text = draft
        if let e = core.send(.sendChat(key: key, text: text)) {
            // Keep the draft so nothing typed is lost.
            error = e == "Not sent" ? "Not sent (not connected or rejected)" : e
            return
        }
        error = nil
        draft = ""
    }

    private func send(_ item: PhotosPickerItem) async {
        guard let data = try? await item.loadTransferable(type: Data.self), let img = UIImage(data: data) else {
            error = "Could not load the image"
            return
        }
        upload(img)
    }

    private func upload(_ img: UIImage) {
        error = core.upload(img, to: key)
    }
}

struct ViewedImage: Identifiable {
    let id = UUID()
    let image: CGImage
    let title: String
}

private struct ChatRow: View {
    @Environment(VCCore.self) private var core
    let key: UInt64
    let line: ChatLine
    let head: Bool
    let open: (CGImage) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            if head {
                HStack(spacing: 8) {
                    Avatar(nick: line.from, size: 22)
                    Text(line.from)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(Theme.avatarColor(line.from))
                    Text(Theme.utcTime(line.tsMs))
                        .font(.caption2)
                        .foregroundStyle(Theme.dim)
                }
                .padding(.top, 8)
            }
            content
                .padding(.leading, 30)
        }
        .contextMenu { menu }
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("chat.line")
    }

    @ViewBuilder
    private var content: some View {
        if let img = line.image {
            if img.available, let cg = core.chatImage(key: key, id: img.id) {
                let size = fit(w: Double(img.w), h: Double(img.h))
                Image(decorative: cg, scale: 1)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .frame(width: size.width, height: size.height)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .onTapGesture { open(cg) }
                    .accessibilityLabel("Image from \(line.from)")
                    .accessibilityIdentifier("chat.image")
            } else {
                Text("[image expired]")
                    .italic()
                    .foregroundStyle(Theme.dim)
                    .accessibilityIdentifier("chat.imageExpired")
            }
        } else {
            Text(line.text)
                .foregroundStyle(Theme.text)
                .textSelection(.enabled)
        }
    }

    @ViewBuilder
    private var menu: some View {
        if let img = line.image, img.available, let cg = core.chatImage(key: key, id: img.id) {
            Button {
                open(cg)
            } label: {
                Label("Open", systemImage: "arrow.up.left.and.arrow.down.right")
            }
            Button {
                UIPasteboard.general.image = UIImage(cgImage: cg)
            } label: {
                Label("Copy image", systemImage: "doc.on.doc")
            }
        }
        if !line.text.isEmpty {
            Button {
                UIPasteboard.general.string = line.text
            } label: {
                Label("Copy text", systemImage: "doc.on.doc")
            }
        }
        Button {
            UIPasteboard.general.string = Theme.utcTime(line.tsMs)
        } label: {
            Label("Copy time", systemImage: "clock")
        }
    }

    /// Thumbnail size: at most 300×240, never upscaled.
    private func fit(w: Double, h: Double) -> CGSize {
        guard w > 0, h > 0 else { return CGSize(width: 120, height: 90) }
        let s = min(1, min(300 / w, 240 / h))
        return CGSize(width: w * s, height: h * s)
    }
}
