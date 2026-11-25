import AVFoundation
import Foundation

/// The phone's audio, feeding the Rust core's `ExternalBackend` through a
/// `VcAudio` handle. The render and sink blocks run on real-time threads and only
/// call `vc_audio_capture` / `vc_audio_render`; they never touch `VcApp`.
///
/// Modes (launch setting `vcAudio`):
/// - `external`: AVAudioSession `.playAndRecord` / `.voiceChat`, the microphone
///   with voice processing (echo cancellation), and playback.
/// - `engine-synth`: no microphone at all. One source node makes a 440 Hz tone,
///   feeds it to the core as if captured, then renders the core's playback.
///   It exercises the whole AVAudioEngine path without any permission prompt
///   (the simulator default).
/// - `fake-sine`, `none`: no engine (the core makes its own tone, or nothing).
final class AudioEngine {
    enum Mode: String {
        case external
        case engineSynth = "engine-synth"
        case fakeSine = "fake-sine"
        case none
    }

    /// The text the core's banner starts with (`gui::NO_AUDIO_BANNER`).
    static let noAudioBanner = "No audio device: running without audio"

    let mode: Mode
    private let audio: OpaquePointer?
    private var engine: AVAudioEngine?
    /// Scratch for down-mixing interleaved input and for the synth tone. Allocated
    /// once, outside the real-time threads.
    private let scratch: UnsafeMutablePointer<Float>
    private let scratchLen = 8192
    private let phase: UnsafeMutablePointer<Double>
    /// Called on the main thread with `nil` when audio runs, else the reason.
    var onState: ((String?) -> Void)?
    /// Voice processing (echo cancellation) could not be turned on.
    private(set) var voiceProcessingNote: String?
    private var observers: [NSObjectProtocol] = []
    private var restartPending = false

    init(mode: Mode, audio: OpaquePointer?) {
        self.mode = mode
        self.audio = audio
        scratch = .allocate(capacity: scratchLen)
        scratch.initialize(repeating: 0, count: scratchLen)
        phase = .allocate(capacity: 1)
        phase.initialize(to: 0)
        observe()
    }

    deinit {
        for o in observers { NotificationCenter.default.removeObserver(o) }
        stop()
        scratch.deallocate()
        phase.deallocate()
    }

    static var echoCancel: Bool {
        UserDefaults.standard.object(forKey: "vcEchoCancel") as? Bool ?? true
    }

    /// Play from the loudspeaker (only ever with echo cancellation on).
    static var speakerOn: Bool {
        UserDefaults.standard.object(forKey: "vcSpeaker") as? Bool ?? true
    }

    func start() {
        guard audio != nil else { return }
        switch mode {
        case .external:
            AVAudioApplication.requestRecordPermission { granted in
                DispatchQueue.main.async {
                    self.startExternal(micAllowed: granted)
                }
            }
        case .engineSynth:
            startSynth()
        case .fakeSine, .none:
            break
        }
    }

    func stop() {
        engine?.stop()
        engine = nil
    }

    func restart() {
        stop()
        start()
    }

    // MARK: - external (real microphone)

    private func startExternal(micAllowed: Bool) {
        stop()
        let session = AVAudioSession.sharedInstance()
        guard micAllowed else {
            startOutputOnly(reason: "microphone access denied")
            return
        }
        let wantEcho = Self.echoCancel
        voiceProcessingNote = nil
        do {
            try configureSession(session, speaker: wantEcho && Self.speakerOn)
            let engine = AVAudioEngine()
            let input = engine.inputNode
            var echo = wantEcho
            do {
                try input.setVoiceProcessingEnabled(wantEcho)
            } catch {
                echo = false
                voiceProcessingNote = "Echo cancellation unavailable: \(error.localizedDescription)"
            }
            if !echo && wantEcho {
                // Without echo cancellation the loudspeaker would feed back: use the earpiece.
                try configureSession(session, speaker: false)
            }
            let inFormat = input.outputFormat(forBus: 0)
            guard inFormat.sampleRate > 0, inFormat.channelCount > 0 else {
                startOutputOnly(reason: "no microphone input")
                return
            }
            let a = audio
            let rate = UInt32(inFormat.sampleRate)
            let interleaved = inFormat.isInterleaved
            let channels = Int(inFormat.channelCount)
            let scratch = self.scratch
            let scratchLen = self.scratchLen
            let sink = AVAudioSinkNode { _, frames, abl -> OSStatus in
                let list = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: abl))
                guard let first = list.first, let raw = first.mData else { return noErr }
                let samples = raw.assumingMemoryBound(to: Float.self)
                let n = Int(frames)
                if interleaved && channels > 1 {
                    let m = min(n, scratchLen)
                    for i in 0..<m { scratch[i] = samples[i * channels] }
                    vc_audio_capture(a, scratch, m, rate)
                } else {
                    vc_audio_capture(a, samples, n, rate)
                }
                return noErr
            }
            engine.attach(sink)
            engine.connect(input, to: sink, format: inFormat)
            try attachPlayback(engine)
            engine.prepare()
            try engine.start()
            self.engine = engine
            onState?(nil)
        } catch {
            self.engine = nil
            startOutputOnly(reason: error.localizedDescription)
        }
    }

    private func configureSession(_ session: AVAudioSession, speaker: Bool) throws {
        var opts: AVAudioSession.CategoryOptions = [.allowBluetoothHFP]
        if speaker { opts.insert(.defaultToSpeaker) }
        try session.setCategory(.playAndRecord, mode: .voiceChat, options: opts)
        try? session.setPreferredIOBufferDuration(0.01)
        try session.setActive(true)
    }

    /// Playback only (no microphone), after a failure; the banner says why.
    private func startOutputOnly(reason: String) {
        stop()
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [])
            try session.setActive(true)
            let engine = AVAudioEngine()
            try attachPlayback(engine)
            engine.prepare()
            try engine.start()
            self.engine = engine
        } catch {
            self.engine = nil
        }
        onState?("\(Self.noAudioBanner) (\(reason))")
    }

    /// A mono source node at the output's rate that pulls the core's playback.
    private func attachPlayback(_ engine: AVAudioEngine) throws {
        let outRate = engine.outputNode.outputFormat(forBus: 0).sampleRate
        let rate = outRate > 0 ? outRate : 48_000
        guard let format = AVAudioFormat(standardFormatWithSampleRate: rate, channels: 1) else {
            throw NSError(domain: "VoiceChat", code: 1, userInfo: [NSLocalizedDescriptionKey: "no output format"])
        }
        let a = audio
        let r = UInt32(rate)
        let source = AVAudioSourceNode(format: format) { _, _, frames, abl -> OSStatus in
            let list = UnsafeMutableAudioBufferListPointer(abl)
            for buf in list {
                guard let raw = buf.mData else { continue }
                vc_audio_render(a, raw.assumingMemoryBound(to: Float.self), Int(frames), r)
            }
            return noErr
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
    }

    // MARK: - engine-synth (no microphone)

    private func startSynth() {
        stop()
        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default, options: [])
            try session.setActive(true)
            let engine = AVAudioEngine()
            let outRate = engine.outputNode.outputFormat(forBus: 0).sampleRate
            let rate = outRate > 0 ? outRate : 48_000
            guard let format = AVAudioFormat(standardFormatWithSampleRate: rate, channels: 1) else {
                throw NSError(domain: "VoiceChat", code: 1, userInfo: [NSLocalizedDescriptionKey: "no output format"])
            }
            let a = audio
            let r = UInt32(rate)
            let scratch = self.scratch
            let scratchLen = self.scratchLen
            let phase = self.phase
            let step = 2.0 * Double.pi * 440.0 / rate
            let source = AVAudioSourceNode(format: format) { _, _, frames, abl -> OSStatus in
                // The "captured" tone first, then the core's playback.
                var done = 0
                let n = Int(frames)
                while done < n {
                    let m = min(n - done, scratchLen)
                    for i in 0..<m {
                        scratch[i] = Float(0.3 * sin(phase.pointee))
                        phase.pointee += step
                        if phase.pointee > 2.0 * Double.pi { phase.pointee -= 2.0 * Double.pi }
                    }
                    vc_audio_capture(a, scratch, m, r)
                    done += m
                }
                let list = UnsafeMutableAudioBufferListPointer(abl)
                for buf in list {
                    guard let raw = buf.mData else { continue }
                    vc_audio_render(a, raw.assumingMemoryBound(to: Float.self), n, r)
                }
                return noErr
            }
            engine.attach(source)
            engine.connect(source, to: engine.mainMixerNode, format: format)
            engine.prepare()
            try engine.start()
            self.engine = engine
            onState?(nil)
        } catch {
            self.engine = nil
            onState?("\(Self.noAudioBanner) (\(error.localizedDescription))")
        }
    }

    // MARK: - interruptions and route changes

    private func observe() {
        let nc = NotificationCenter.default
        observers.append(nc.addObserver(forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  AVAudioSession.InterruptionType(rawValue: raw) == .ended
            else { return }
            self?.scheduleRestart()
        })
        observers.append(nc.addObserver(forName: AVAudioSession.routeChangeNotification, object: nil, queue: .main) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt,
                  let reason = AVAudioSession.RouteChangeReason(rawValue: raw)
            else { return }
            switch reason {
            case .newDeviceAvailable, .oldDeviceUnavailable:
                self?.scheduleRestart()
            default:
                break
            }
        })
        observers.append(nc.addObserver(forName: .AVAudioEngineConfigurationChange, object: nil, queue: .main) { [weak self] note in
            guard let self, let e = note.object as? AVAudioEngine, e === self.engine else { return }
            self.scheduleRestart()
        })
    }

    /// Restart once after a burst of notifications settles.
    private func scheduleRestart() {
        guard engine != nil || mode == .external || mode == .engineSynth else { return }
        guard !restartPending else { return }
        restartPending = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            guard let self else { return }
            self.restartPending = false
            self.restart()
        }
    }
}
