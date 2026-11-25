import SwiftUI
import UIKit

/// A full-screen image with pinch-zoom, drag, double-tap reset and a share button.
struct ImageViewer: View {
    @Environment(\.dismiss) private var dismiss
    let image: CGImage
    let title: String
    @State private var scale: CGFloat = 1
    @State private var lastScale: CGFloat = 1
    @State private var offset: CGSize = .zero
    @State private var lastOffset: CGSize = .zero

    var body: some View {
        NavigationStack {
            GeometryReader { _ in
                Image(decorative: image, scale: 1)
                    .resizable()
                    .aspectRatio(contentMode: .fit)
                    .scaleEffect(scale)
                    .offset(offset)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .gesture(
                        MagnifyGesture()
                            .onChanged { v in scale = max(1, min(8, lastScale * v.magnification)) }
                            .onEnded { _ in lastScale = scale }
                            .simultaneously(with: DragGesture()
                                .onChanged { v in
                                    offset = CGSize(width: lastOffset.width + v.translation.width,
                                                    height: lastOffset.height + v.translation.height)
                                }
                                .onEnded { _ in lastOffset = offset })
                    )
                    .onTapGesture(count: 2) {
                        withAnimation {
                            scale = 1; lastScale = 1; offset = .zero; lastOffset = .zero
                        }
                    }
                    .accessibilityIdentifier("viewer.image")
            }
            .background(Color.black.ignoresSafeArea())
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                        .accessibilityIdentifier("viewer.close")
                }
                ToolbarItemGroup(placement: .primaryAction) {
                    Button {
                        UIPasteboard.general.image = UIImage(cgImage: image)
                    } label: {
                        Label("Copy", systemImage: "doc.on.doc")
                    }
                    .accessibilityIdentifier("viewer.copy")
                    ShareLink(
                        item: Image(decorative: image, scale: 1),
                        preview: SharePreview(title, image: Image(decorative: image, scale: 1))
                    )
                    .accessibilityIdentifier("viewer.share")
                }
            }
        }
    }
}
