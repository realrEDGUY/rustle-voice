import SwiftUI

/// Watched streams from every connected server, in a grid of `ceil(sqrt(n))`
/// columns (the desktop's `grid_dims`). Tap a tile for full screen. The app can
/// watch but never share: there is no share control anywhere.
struct StreamsView: View {
    @Environment(VCCore.self) private var core
    var compact = false
    @State private var fullScreen: StreamItem?

    static func gridDims(_ n: Int) -> (cols: Int, rows: Int) {
        guard n > 0 else { return (0, 0) }
        var cols = 1
        while cols * cols < n { cols += 1 }
        return (cols, (n + cols - 1) / cols)
    }

    var body: some View {
        let streams = core.snapshot.streams
        let cols = max(1, Self.gridDims(streams.count).cols)
        LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: cols), spacing: 6) {
            ForEach(streams) { s in
                StreamTile(stream: s, compact: compact)
                    .onTapGesture { fullScreen = s }
            }
        }
        .padding(6)
        .fullScreenCover(item: $fullScreen) { s in
            StreamFullScreen(id: s.id)
        }
    }
}

struct StreamTile: View {
    @Environment(VCCore.self) private var core
    let stream: StreamItem
    var compact = false

    var body: some View {
        let frame = core.frames[stream.id]
        VStack(alignment: .leading, spacing: 4) {
            ZStack {
                Rectangle().fill(Theme.stage)
                if let f = frame {
                    Image(decorative: f.image, scale: 1)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                        .accessibilityIdentifier("stream.frame")
                } else {
                    Text("Waiting for screen…")
                        .font(.caption)
                        .foregroundStyle(Theme.dim)
                        .accessibilityIdentifier("stream.waiting")
                }
            }
            .aspectRatio(16 / 9, contentMode: .fit)
            .overlay(alignment: .topLeading) {
                Text("LIVE")
                    .font(.caption2.weight(.heavy))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(RoundedRectangle(cornerRadius: 4).fill(Theme.red))
                    .padding(6)
            }
            .clipShape(RoundedRectangle(cornerRadius: 8))
            HStack(spacing: 6) {
                VStack(alignment: .leading, spacing: 0) {
                    Text("Screen: \(stream.nick) (\(stream.server))")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(Theme.text)
                        .lineLimit(1)
                        .accessibilityIdentifier("stream.title")
                    if let f = frame {
                        Text("\(f.width)×\(f.height)")
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(Theme.dim)
                            .accessibilityIdentifier("stream.size")
                    }
                }
                Spacer()
                StreamControls(stream: stream)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("stream.\(stream.nick)")
    }
}

struct StreamControls: View {
    @Environment(VCCore.self) private var core
    let stream: StreamItem

    var body: some View {
        HStack(spacing: 10) {
            Button {
                core.send(.muteStream(key: stream.key, user: stream.user, muted: !stream.soundMuted))
            } label: {
                Image(systemName: stream.soundMuted ? "speaker.slash.fill" : "speaker.wave.2.fill")
                    .foregroundStyle(stream.soundMuted ? Theme.amber : Theme.muted)
            }
            .accessibilityLabel(stream.soundMuted ? "Unmute stream sound" : "Mute stream sound")
            .accessibilityIdentifier("stream.mute")
            Button {
                core.send(.watchScreen(key: stream.key, user: stream.user, on: false))
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(Theme.muted)
            }
            .accessibilityLabel("Stop watching")
            .accessibilityIdentifier("stream.stop")
        }
        .buttonStyle(.plain)
        .font(.body)
    }
}

/// One stream filling the screen; closes itself when the stream ends.
private struct StreamFullScreen: View {
    @Environment(VCCore.self) private var core
    @Environment(\.dismiss) private var dismiss
    let id: String

    var body: some View {
        let stream = core.snapshot.streams.first { $0.id == id }
        NavigationStack {
            ZStack {
                Color.black.ignoresSafeArea()
                if let f = core.frames[id] {
                    Image(decorative: f.image, scale: 1)
                        .resizable()
                        .aspectRatio(contentMode: .fit)
                } else {
                    Text("Waiting for screen…").foregroundStyle(Theme.dim)
                }
            }
            .navigationTitle(stream.map { "Screen: \($0.nick) (\($0.server))" } ?? "Stream ended")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                }
                if let stream {
                    ToolbarItem(placement: .primaryAction) {
                        StreamControls(stream: stream)
                    }
                }
            }
        }
        .onChange(of: stream == nil) { _, gone in
            if gone { dismiss() }
        }
    }
}
