import SwiftUI

@main
struct VoiceChatApp: App {
    @State private var core = VCCore()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(core)
                .preferredColorScheme(.dark)
                .tint(Theme.accent)
        }
    }
}
