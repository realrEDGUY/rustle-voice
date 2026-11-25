import CoreGraphics
import Foundation
import Observation
import UIKit

/// A watched stream's latest frame.
struct StreamFrame {
    var image: CGImage
    var width: Int
    var height: Int
    var generation: UInt64
}

/// Owns the Rust core (`VcApp`) and its audio handle, ticks it from a main-thread
/// timer, and publishes the decoded snapshot and live views to SwiftUI.
///
/// Launch settings come from `UserDefaults.standard`, which also reads `-vcX value`
/// launch arguments:
/// - `vcAudio`: `external`, `engine-synth`, `fake-sine` or `none`. The default is
///   `engine-synth` on the simulator (never opens the microphone) and `external`
///   on a device. Under XCTest it is always `none`.
/// - `vcProfile`: the profile folder (default Application Support/vc-client).
/// - `vcSeed`: `label=addr=nick`; adds and connects that server if no saved
///   server has the label.
@Observable
@MainActor
final class VCCore {
    private(set) var snapshot = Snapshot.empty
    private(set) var live = Live.empty
    /// Latest frame per watched stream, keyed by `StreamItem.id`.
    private(set) var frames: [String: StreamFrame] = [:]
    /// The last snapshot or live JSON could not be decoded (the last good state stays).
    private(set) var bridgeError: String?
    /// The core caught a panic and stopped.
    private(set) var poisoned = false
    /// Why echo cancellation is off, if it could not be turned on.
    private(set) var audioNote: String?
    let audioMode: AudioEngine.Mode

    @ObservationIgnored private var app: OpaquePointer?
    @ObservationIgnored private var audioHandle: OpaquePointer?
    @ObservationIgnored private var engine: AudioEngine?
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private var timerFast = false
    @ObservationIgnored private var images: [String: CGImage] = [:]

    static let uploadDir = FileManager.default.temporaryDirectory.appendingPathComponent("vc-uploads", isDirectory: true)

    init() {
        let defaults = UserDefaults.standard
        let testing = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
        let mode: AudioEngine.Mode
        if testing {
            mode = .none
        } else if let raw = defaults.string(forKey: "vcAudio"), let m = AudioEngine.Mode(rawValue: raw) {
            mode = m
        } else {
            #if targetEnvironment(simulator)
            mode = .engineSynth
            #else
            mode = .external
            #endif
        }
        audioMode = mode
        let profile = Self.profileDir(defaults.string(forKey: "vcProfile"))
        try? FileManager.default.removeItem(at: Self.uploadDir)
        try? FileManager.default.createDirectory(at: Self.uploadDir, withIntermediateDirectories: true)
        let rustAudio = mode == .fakeSine ? "fake-sine" : "external"
        app = vc_app_new(profile.path, rustAudio)
        guard app != nil else {
            poisoned = true
            return
        }
        audioHandle = vc_app_audio(app)
        if mode == .external || mode == .engineSynth {
            let e = AudioEngine(mode: mode, audio: audioHandle)
            e.onState = { [weak self] warning in
                MainActor.assumeIsolated {
                    self?.setAudioWarning(warning)
                }
            }
            engine = e
            e.start()
        }
        tick()
        if let seed = defaults.string(forKey: "vcSeed") {
            applySeed(seed)
        }
        schedule(fast: false)
    }

    private static func profileDir(_ custom: String?) -> URL {
        if let custom, !custom.isEmpty {
            return URL(fileURLWithPath: custom, isDirectory: true)
        }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        let dir = base.appendingPathComponent("vc-client", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private func applySeed(_ seed: String) {
        let parts = seed.split(separator: "=", maxSplits: 2).map(String.init)
        guard parts.count == 3 else { return }
        if snapshot.servers.contains(where: { $0.label == parts[0] }) { return }
        _ = send(.saveServer(editing: nil, label: parts[0], addr: parts[1], nick: parts[2], password: "", connectNow: true))
    }

    // MARK: - ticking

    private func schedule(fast: Bool) {
        timer?.invalidate()
        timerFast = fast
        let t = Timer(timeInterval: fast ? 1.0 / 30.0 : 1.0 / 10.0, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.tick()
            }
        }
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    func tick() {
        guard let app, !poisoned else { return }
        vc_app_step(app)
        if checkPoison() { return }
        if let s = takeString(vc_app_snapshot(app)) {
            do {
                snapshot = try VCJSON.decoder.decode(Snapshot.self, from: Data(s.utf8))
                bridgeError = nil
                pruneImages()
            } catch {
                bridgeError = "snapshot: \(error)"
            }
        }
        if checkPoison() { return }
        if let s = takeString(vc_app_live(app)) {
            do {
                live = try VCJSON.decoder.decode(Live.self, from: Data(s.utf8))
            } catch {
                bridgeError = "live: \(error)"
            }
        }
        if checkPoison() { return }
        updateFrames()
        if checkPoison() { return }
        let wantFast = !snapshot.streams.isEmpty
        if wantFast != timerFast {
            schedule(fast: wantFast)
        }
    }

    private func takeString(_ p: UnsafeMutablePointer<CChar>?) -> String? {
        guard let p else { return nil }
        defer { vc_string_free(p) }
        return String(cString: p)
    }

    private func updateFrames() {
        guard let app else { return }
        var next: [String: StreamFrame] = [:]
        for s in snapshot.streams {
            var px = VcPixels()
            if vc_app_screen(app, s.key, s.user, &px) {
                if let old = frames[s.id], old.generation == px.generation {
                    vc_pixels_release(px.handle)
                    next[s.id] = old
                } else if let img = makeCGImage(px) {
                    next[s.id] = StreamFrame(image: img, width: Int(px.width), height: Int(px.height), generation: px.generation)
                }
            }
        }
        if next.count != frames.count || next.contains(where: { frames[$0.key]?.generation != $0.value.generation }) {
            frames = next
        }
    }

    // MARK: - commands

    /// Run a command; returns the core's error text, or nil when it worked.
    @discardableResult
    func send(_ c: Command) -> String? {
        guard let app else { return "Voice core stopped, restart the app" }
        if poisoned { return "Voice core stopped, restart the app" }
        let err = takeString(vc_app_command(app, c.json()))
        if checkPoison() { return err ?? "Voice core stopped, restart the app" }
        tick()
        return err
    }

    func setPTT(_ down: Bool) {
        guard let app, !poisoned else { return }
        vc_app_set_ptt(app, down)
        checkPoison()
    }

    private func setAudioWarning(_ warning: String?) {
        guard let app, !poisoned else { return }
        audioNote = engine?.voiceProcessingNote
        if let warning {
            vc_app_set_audio_warning(app, warning)
        } else {
            vc_app_set_audio_warning(app, nil)
        }
        if checkPoison() { return }
        tick()
    }

    /// Restart the audio engine (after the echo-cancellation toggle changes).
    func restartEngine() {
        engine?.restart()
    }

    // MARK: - images

    /// A chat image, decoded once and cached while the core keeps it.
    func chatImage(key: UInt64, id: UInt64) -> CGImage? {
        let k = "\(key)/\(id)"
        if let img = images[k] { return img }
        guard let app else { return nil }
        var px = VcPixels()
        guard vc_app_image(app, key, id, &px), let img = makeCGImage(px) else { return nil }
        images[k] = img
        return img
    }

    private func pruneImages() {
        var keep = Set<String>()
        for s in snapshot.servers {
            for line in s.chat {
                if let i = line.image, i.available { keep.insert("\(s.key)/\(i.id)") }
            }
        }
        images = images.filter { keep.contains($0.key) }
    }

    /// Write `image` as JPEG into the upload folder and send it.
    func upload(_ image: UIImage, to key: UInt64) -> String? {
        guard let data = image.jpegData(compressionQuality: 0.9) else { return "Could not encode the image" }
        let url = Self.uploadDir.appendingPathComponent(UUID().uuidString + ".jpg")
        do {
            try data.write(to: url)
        } catch {
            return "Could not save the image: \(error.localizedDescription)"
        }
        return send(.uploadFile(key: key, path: url.path))
    }

    var version: String {
        String(cString: vc_version())
    }

    /// After a caught panic the core has already stopped its network and audio
    /// pipeline; stop the engine and free the handles too, so nothing keeps
    /// running. Returns `true` when the core is (now) stopped: the caller must not
    /// use its local copy of `app` any more.
    @discardableResult
    private func checkPoison() -> Bool {
        if poisoned { return true }
        guard let app, vc_app_poisoned(app) else { return false }
        poisoned = true
        shutdown()
        return true
    }

    /// Stop the engine before freeing the handles it uses. Safe to call twice.
    func shutdown() {
        timer?.invalidate()
        timer = nil
        engine?.stop()
        engine = nil
        if let a = audioHandle { vc_audio_free(a) }
        audioHandle = nil
        if let app { vc_app_free(app) }
        app = nil
    }
}
