//! Audio pipeline: one `tick()` per 20 ms moves a capture frame to the network and
//! mixes received voice into the playback ring.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vc_proto::{FRAME_SAMPLES, SAMPLE_RATE, STREAM_BIT, SessionId};

use crate::ServerKey;
use crate::dsp::adpcm::{self, EncoderState};
use crate::dsp::jitter::DEFAULT_DEPTH;
use crate::dsp::mixer::Mixer;
use crate::dsp::vad::{Vad, rms_db};
use crate::profile::InputMode;

pub type Ring = Arc<Mutex<VecDeque<f32>>>;

/// More than this many queued samples (200 ms) drops the oldest.
pub const MAX_RING: usize = (SAMPLE_RATE as usize) / 5;
/// Capture backlog kept after a persistent backlog is trimmed.
pub const CAPTURE_KEEP: usize = 2 * FRAME_SAMPLES;
/// Playback backlog kept after a persistent backlog is trimmed.
pub const PLAYBACK_KEEP: usize = 3 * FRAME_SAMPLES;
/// Stream-sound frames quieter than this are all silence and not sent.
pub const STREAM_SILENCE_DB: f32 = -70.0;
/// Fixed playback gain under the output volume: the slider's 100 % plays at twice
/// the raw level (140 % shown = 2.8x).
pub const OUTPUT_BOOST: f32 = 2.0;
/// A backlog is trimmed only if it never fell to the keep level for this many ticks
/// (500 ms). Devices that deliver or pull audio in large chunks swing the ring level
/// every period; only the minimum over a window shows real, lasting latency.
pub const TRIM_WINDOW_TICKS: u32 = 25;

/// Drops a ring backlog that persisted for a whole `TRIM_WINDOW_TICKS` window.
#[derive(Debug, Clone)]
struct BacklogTrimmer {
    keep: usize,
    min_seen: usize,
    ticks: u32,
    trimmed: usize,
}

impl BacklogTrimmer {
    fn new(keep: usize) -> Self {
        Self {
            keep,
            min_seen: usize::MAX,
            ticks: 0,
            trimmed: 0,
        }
    }

    fn observe(&mut self, ring: &mut VecDeque<f32>) {
        self.min_seen = self.min_seen.min(ring.len());
        self.ticks += 1;
        if self.ticks == TRIM_WINDOW_TICKS {
            if self.min_seen > self.keep {
                let drop = (self.min_seen - self.keep).min(ring.len());
                ring.drain(..drop);
                self.trimmed += drop;
            }
            self.min_seen = usize::MAX;
            self.ticks = 0;
        }
    }
}

/// One server's voice tuning as the audio thread sees it (numbers only).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamTuning {
    /// Jitter buffer depth for this server's speakers, in frames.
    pub jitter_depth: usize,
    /// Playback backlog this server tolerates before trimming, in frames.
    pub playback_keep_frames: usize,
}

/// Per-tick control state, written by the UI thread.
#[derive(Debug, Clone)]
pub struct PipelineCtl {
    pub talk_target: Option<ServerKey>,
    /// Where my stream sound (system audio while sharing) goes, if anywhere.
    pub stream_target: Option<ServerKey>,
    pub muted: bool,
    pub deafened: bool,
    pub mode: InputMode,
    pub ptt_down: bool,
    pub vad_threshold_db: f32,
    pub master: f32,
    pub gains: HashMap<(ServerKey, SessionId), f32>,
    /// Voice tuning per Connected server. A server missing here (before Welcome)
    /// gets the defaults.
    pub voice_tuning: HashMap<ServerKey, StreamTuning>,
}

impl Default for PipelineCtl {
    fn default() -> Self {
        Self {
            talk_target: None,
            stream_target: None,
            muted: false,
            deafened: false,
            mode: InputMode::Vad,
            ptt_down: false,
            vad_threshold_db: -45.0,
            master: 1.0,
            gains: HashMap::new(),
            voice_tuning: HashMap::new(),
        }
    }
}

pub struct AudioPipeline {
    pub capture_ring: Ring,
    pub playback_ring: Ring,
    encoder: EncoderState,
    vad: Vad,
    seq: u32,
    mixer: Mixer,
    level_db: f32,
    transmitting: bool,
    capture_trim: BacklogTrimmer,
    playback_trim: BacklogTrimmer,
    /// System-audio loopback samples while stream sound is on.
    pub stream_ring: Ring,
    stream_trim: BacklogTrimmer,
    stream_encoder: EncoderState,
    stream_seq: u32,
    /// The previous capture frame, sent as pre-roll when VAD opens.
    prev_frame: Option<Vec<f32>>,
    /// Did the last captured frame go out? (false = the next one starts a burst)
    was_sending: bool,
    /// Was the last frame sent faded out as the end of the VAD hangover?
    tail_faded: bool,
}

/// Samples (5 ms) over which a talk burst ramps in or out at a hard gate edge.
pub const EDGE_FADE: usize = (SAMPLE_RATE as usize) / 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeDir {
    In,
    Out,
}

/// Raised-cosine ramp over the first (`In`) or last (`Out`) `len` samples.
pub fn fade(frame: &mut [f32], dir: FadeDir, len: usize) {
    let len = len.min(frame.len());
    if len == 0 {
        return;
    }
    let n = frame.len();
    for i in 0..len {
        let g = 0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 0.5) / len as f32).cos();
        match dir {
            FadeDir::In => frame[i] *= g,
            FadeDir::Out => frame[n - 1 - i] *= g,
        }
    }
}

fn push_capped(ring: &Ring, samples: &[f32]) {
    let mut r = ring.lock().expect("poisoned");
    r.extend(samples.iter().copied());
    let over = r.len().saturating_sub(MAX_RING);
    if over > 0 {
        r.drain(..over);
    }
}

impl Default for AudioPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPipeline {
    pub fn new() -> Self {
        Self {
            capture_ring: Arc::new(Mutex::new(VecDeque::new())),
            playback_ring: Arc::new(Mutex::new(VecDeque::new())),
            encoder: EncoderState::default(),
            vad: Vad::new(-45.0),
            seq: 0,
            mixer: Mixer::new(),
            level_db: -100.0,
            transmitting: false,
            capture_trim: BacklogTrimmer::new(CAPTURE_KEEP),
            playback_trim: BacklogTrimmer::new(PLAYBACK_KEEP),
            stream_ring: Arc::new(Mutex::new(VecDeque::new())),
            stream_trim: BacklogTrimmer::new(CAPTURE_KEEP),
            stream_encoder: EncoderState::default(),
            stream_seq: 0,
            prev_frame: None,
            was_sending: false,
            tail_faded: false,
        }
    }

    /// Callback for the system-audio loopback stream: pushes into the stream ring.
    pub fn stream_cb(&self) -> crate::audio::CaptureCb {
        let ring = self.stream_ring.clone();
        Box::new(move |s: &[f32]| push_capped(&ring, s))
    }

    /// Callback for a capture stream: pushes into the capture ring.
    pub fn capture_cb(&self) -> crate::audio::CaptureCb {
        let ring = self.capture_ring.clone();
        Box::new(move |s: &[f32]| push_capped(&ring, s))
    }

    /// Callback for a playback stream: pulls from the playback ring (zeros on underrun).
    pub fn playback_cb(&self) -> crate::audio::PlaybackCb {
        let ring = self.playback_ring.clone();
        Box::new(move |out: &mut [f32]| {
            let mut r = ring.lock().expect("poisoned");
            for o in out.iter_mut() {
                *o = r.pop_front().unwrap_or(0.0);
            }
        })
    }

    /// Samples dropped so far by backlog trimming: (capture, playback).
    pub fn trimmed_samples(&self) -> (usize, usize) {
        (self.capture_trim.trimmed, self.playback_trim.trimmed)
    }

    pub fn input_level_db(&self) -> f32 {
        self.level_db
    }

    /// Was the last tick transmitting?
    pub fn is_transmitting(&self) -> bool {
        self.transmitting
    }

    pub fn on_voice(&mut self, key: ServerKey, speaker: SessionId, seq: u32, payload: &[u8]) {
        if let Ok(frame) = adpcm::decode_frame(payload) {
            self.mixer.push(key, speaker, seq, frame);
        }
    }

    pub fn forget_server(&mut self, key: ServerKey) {
        self.mixer.forget_server(key);
    }

    /// One 20 ms step. Returns voice frames to send: `(server, seq, payload)`.
    pub fn tick(&mut self, ctl: &PipelineCtl) -> Vec<(ServerKey, u32, Vec<u8>)> {
        let mut out = Vec::new();
        // Capture.
        let frame: Option<Vec<f32>> = {
            let mut r = self.capture_ring.lock().expect("poisoned");
            self.capture_trim.observe(&mut r);
            if r.len() >= FRAME_SAMPLES {
                Some(r.drain(..FRAME_SAMPLES).collect())
            } else {
                None
            }
        };
        self.transmitting = false;
        if let Some(frame) = frame {
            self.level_db = rms_db(&frame);
            self.vad.threshold_db = ctl.vad_threshold_db;
            let vad_open = self.vad.update(&frame);
            let gate = if ctl.muted || ctl.deafened {
                false
            } else {
                match ctl.mode {
                    InputMode::Ptt => ctl.ptt_down,
                    InputMode::Vad => vad_open,
                    InputMode::Open => true,
                }
            };
            // Pre-roll is audio the VAD held back. Audio captured while muted is never
            // kept, so unmuting mid-word cannot send what was said before it.
            let prev = if ctl.muted || ctl.deafened {
                self.prev_frame = None;
                None
            } else {
                self.prev_frame.replace(frame.clone())
            };
            let tail_faded = std::mem::take(&mut self.tail_faded);
            if gate && let Some(key) = ctl.talk_target {
                let mut frames = Vec::with_capacity(2);
                if !self.was_sending {
                    // Burst start: fresh codec state (no jump from the last burst's
                    // predictor) and a ramp in from silence instead of a step.
                    self.encoder = EncoderState::default();
                    match prev {
                        // VAD opens on an already-loud frame: send the frame before it
                        // too (the word's attack), faded in across its full length.
                        Some(mut p) if ctl.mode == InputMode::Vad => {
                            let n = p.len();
                            fade(&mut p, FadeDir::In, n);
                            frames.push(p);
                            frames.push(frame);
                        }
                        _ => {
                            let mut f = frame;
                            fade(&mut f, FadeDir::In, EDGE_FADE);
                            frames.push(f);
                        }
                    }
                } else {
                    let mut f = frame;
                    if tail_faded {
                        // The frame before this one was ramped out as the last of the
                        // hangover, but speech came back in time to keep the gate open:
                        // ramp back in instead of stepping up from zero.
                        fade(&mut f, FadeDir::In, EDGE_FADE);
                    }
                    frames.push(f);
                }
                // Last hangover frame: ramp out so the gate does not cut mid-wave.
                if ctl.mode == InputMode::Vad
                    && self.vad.closing()
                    && let Some(f) = frames.last_mut()
                {
                    fade(f, FadeDir::Out, EDGE_FADE);
                    self.tail_faded = true;
                }
                for f in frames {
                    let mut pcm = [0i16; FRAME_SAMPLES];
                    for (p, s) in pcm.iter_mut().zip(&f) {
                        *p = adpcm::f32_to_i16(*s);
                    }
                    let enc = adpcm::encode_frame(&pcm, &mut self.encoder);
                    // Mic seq never carries the stream tag.
                    out.push((key, self.seq & !STREAM_BIT, enc.to_vec()));
                    self.seq = self.seq.wrapping_add(1) & !STREAM_BIT;
                }
                self.transmitting = true;
            }
            self.was_sending = self.transmitting;
        }
        // Stream sound: no mute / VAD gate (it is not my voice), silence skipped.
        let stream: Option<Vec<f32>> = {
            let mut r = self.stream_ring.lock().expect("poisoned");
            if ctl.stream_target.is_none() {
                r.clear();
                None
            } else {
                self.stream_trim.observe(&mut r);
                (r.len() >= FRAME_SAMPLES).then(|| r.drain(..FRAME_SAMPLES).collect())
            }
        };
        if let (Some(frame), Some(key)) = (stream, ctl.stream_target)
            && rms_db(&frame) >= STREAM_SILENCE_DB
        {
            let mut pcm = [0i16; FRAME_SAMPLES];
            for (p, s) in pcm.iter_mut().zip(&frame) {
                *p = adpcm::f32_to_i16(*s);
            }
            let enc = adpcm::encode_frame(&pcm, &mut self.stream_encoder);
            out.push((
                key,
                (self.stream_seq & !STREAM_BIT) | STREAM_BIT,
                enc.to_vec(),
            ));
            self.stream_seq = self.stream_seq.wrapping_add(1);
        }
        // Playback.
        let mut mixed = [0f32; FRAME_SAMPLES];
        let gains = &ctl.gains;
        let tuning = &ctl.voice_tuning;
        self.mixer.mix_next(
            |k| tuning.get(&k).map_or(DEFAULT_DEPTH, |t| t.jitter_depth),
            |k, s| gains.get(&(k, s)).copied().unwrap_or(1.0),
            ctl.master * OUTPUT_BOOST,
            ctl.deafened,
            &mut mixed,
        );
        // The ring is shared by every server: tolerate only the tightest backlog.
        self.playback_trim.keep = tuning
            .values()
            .map(|t| t.playback_keep_frames * FRAME_SAMPLES)
            .min()
            .unwrap_or(PLAYBACK_KEEP);
        self.playback_trim
            .observe(&mut self.playback_ring.lock().expect("poisoned"));
        push_capped(&self.playback_ring, &mixed);
        out
    }

    /// Production clock: a dedicated thread ticking every 20 ms against a deadline.
    pub fn spawn_thread(
        pipeline: Arc<Mutex<AudioPipeline>>,
        ctl: Arc<Mutex<PipelineCtl>>,
        send: impl Fn(Vec<(ServerKey, u32, Vec<u8>)>) + Send + 'static,
    ) -> PipelineThread {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let join = std::thread::Builder::new()
            .name("vc-audio".into())
            .spawn(move || {
                let period = Duration::from_millis(20);
                let mut next = Instant::now() + period;
                while !s2.load(Ordering::Relaxed) {
                    let c = ctl.lock().expect("poisoned").clone();
                    let frames = pipeline.lock().expect("poisoned").tick(&c);
                    if !frames.is_empty() {
                        send(frames);
                    }
                    let now = Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                        next += period;
                    } else {
                        // Fell behind by more than a frame: resync.
                        next = now + period;
                    }
                }
            })
            .expect("spawn audio thread");
        PipelineThread {
            stop,
            join: Some(join),
        }
    }
}

pub struct PipelineThread {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PipelineThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioBackend, FakeBackend, FakeMode};

    fn rig(
        mode: FakeMode,
    ) -> (
        AudioPipeline,
        crate::audio::FakeHandle,
        Vec<Box<dyn std::any::Any>>,
    ) {
        let mut b = FakeBackend::new(mode);
        let h = b.handle();
        let p = AudioPipeline::new();
        let c = b.start_capture(None, p.capture_cb()).unwrap();
        let pl = b.start_playback(None, p.playback_cb()).unwrap();
        (p, h, vec![c, pl])
    }

    fn ctl(mode: InputMode) -> PipelineCtl {
        PipelineCtl {
            talk_target: Some(1),
            mode,
            ..PipelineCtl::default()
        }
    }

    #[test]
    fn ptt_gate_blocks_until_pressed() {
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        let mut c = ctl(InputMode::Ptt);
        h.drive(960);
        assert!(p.tick(&c).is_empty());
        c.ptt_down = true;
        h.drive(960);
        let sent = p.tick(&c);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 1);
        assert_eq!(sent[0].2.len(), vc_proto::ADPCM_FRAME_BYTES);
        c.ptt_down = false;
        h.drive(960);
        assert!(p.tick(&c).is_empty());
    }

    #[test]
    fn mute_blocks_send() {
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        let mut c = ctl(InputMode::Open);
        h.drive(960);
        assert_eq!(p.tick(&c).len(), 1, "positive control");
        c.muted = true;
        h.drive(960);
        assert!(p.tick(&c).is_empty());
        c.muted = false;
        c.deafened = true;
        h.drive(960);
        assert!(p.tick(&c).is_empty(), "deafen implies mute");
    }

    #[test]
    fn open_mode_always_sends() {
        let (mut p, h, _s) = rig(FakeMode::Silent);
        let c = ctl(InputMode::Open);
        let mut seqs = Vec::new();
        for _ in 0..5 {
            h.drive(960);
            seqs.extend(p.tick(&c).into_iter().map(|f| f.1));
        }
        assert_eq!(seqs, [0, 1, 2, 3, 4]);
        // VAD mode with silence sends nothing.
        let (mut p, h, _s) = rig(FakeMode::Silent);
        h.drive(960);
        assert!(p.tick(&ctl(InputMode::Vad)).is_empty());
        // VAD mode with a loud sine sends.
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        h.drive(960);
        assert_eq!(p.tick(&ctl(InputMode::Vad)).len(), 1);
        assert!(p.input_level_db() > -10.0);
    }

    fn push_capture(p: &AudioPipeline, f: impl Fn(usize) -> f32) {
        let s: Vec<f32> = (0..FRAME_SAMPLES).map(f).collect();
        push_capped(&p.capture_ring, &s);
    }

    fn loud(i: usize) -> f32 {
        0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32 + 1.0).sin()
    }

    #[test]
    fn vad_onset_sends_faded_preroll_without_a_step() {
        let mut p = AudioPipeline::new();
        let c = ctl(InputMode::Vad);
        push_capture(&p, |_| 0.0005);
        assert!(p.tick(&c).is_empty(), "quiet frame stays gated");
        push_capture(&p, loud);
        let out = p.tick(&c);
        assert_eq!(out.len(), 2, "pre-roll + the frame that opened the gate");
        assert_eq!(out[1].1, out[0].1.wrapping_add(1));
        let pre = adpcm::decode_frame(&out[0].2).unwrap();
        let now = adpcm::decode_frame(&out[1].2).unwrap();
        assert!(pre[0].abs() < 50, "starts from silence: {}", pre[0]);
        // The loud frame starts at full level (sin(1) ~ 0.84 * 0.5), not faded.
        assert!(now.iter().map(|x| x.abs()).max().unwrap() > 10_000);
        // Next loud frame: one frame, no pre-roll again.
        push_capture(&p, loud);
        assert_eq!(p.tick(&c).len(), 1);
    }

    #[test]
    fn gate_open_fades_in_first_frame() {
        let mut p = AudioPipeline::new();
        let mut c = ctl(InputMode::Ptt);
        c.ptt_down = true;
        push_capture(&p, loud);
        let out = p.tick(&c);
        assert_eq!(out.len(), 1, "PTT sends no pre-roll");
        let f = adpcm::decode_frame(&out[0].2).unwrap();
        assert!(f[0].abs() < 300, "ramps in from silence: {}", f[0]);
        assert!(f[EDGE_FADE..].iter().map(|x| x.abs()).max().unwrap() > 10_000);
    }

    fn quiet(_: usize) -> f32 {
        0.0005
    }

    fn first_sample(sent: &(ServerKey, u32, Vec<u8>)) -> i16 {
        adpcm::decode_frame(&sent.2).unwrap()[0]
    }

    /// RMS of a sent frame's first 2.5 ms (half of `EDGE_FADE`), in i16 units.
    fn head_rms(sent: &(ServerKey, u32, Vec<u8>)) -> f32 {
        let head = &adpcm::decode_frame(&sent.2).unwrap()[..EDGE_FADE / 2];
        (head.iter().map(|x| (*x as f32).powi(2)).sum::<f32>() / head.len() as f32).sqrt()
    }

    #[test]
    fn speech_returning_on_the_faded_hangover_tail_ramps_back_in() {
        let mut p = AudioPipeline::new();
        let c = ctl(InputMode::Vad);
        push_capture(&p, quiet);
        p.tick(&c);
        push_capture(&p, loud);
        assert_eq!(p.tick(&c).len(), 2, "burst starts");
        // The whole hangover passes in silence: its last frame is ramped out.
        for i in 0..15 {
            push_capture(&p, quiet);
            assert_eq!(p.tick(&c).len(), 1, "hangover frame {i}");
        }
        // Speech returns on the very next frame, so the gate never shut.
        push_capture(&p, loud);
        let out = p.tick(&c);
        assert_eq!(out.len(), 1, "same burst: no pre-roll");
        let first = first_sample(&out[0]);
        assert!(first.abs() < 300, "ramps in after the faded tail: {first}");
        let ramped = head_rms(&out[0]);
        // The frame after that is mid-burst again and untouched: full level at once.
        push_capture(&p, loud);
        let plain = head_rms(&p.tick(&c)[0]);
        assert!(
            ramped < 5_000.0 && plain > 8_000.0,
            "first 2.5 ms: ramped {ramped}, plain {plain}"
        );
    }

    #[test]
    fn audio_captured_while_muted_is_never_sent_as_preroll() {
        let mut p = AudioPipeline::new();
        let mut c = ctl(InputMode::Vad);
        c.muted = true;
        push_capture(&p, loud);
        assert!(p.tick(&c).is_empty(), "muted");
        c.muted = false;
        push_capture(&p, loud);
        let out = p.tick(&c);
        assert_eq!(out.len(), 1, "the muted frame is not pre-roll");
        assert!(first_sample(&out[0]).abs() < 300, "and the burst ramps in");
        // Positive control: a frame the VAD held back is pre-roll.
        let mut p = AudioPipeline::new();
        push_capture(&p, quiet);
        assert!(p.tick(&c).is_empty());
        push_capture(&p, loud);
        assert_eq!(p.tick(&c).len(), 2);
    }

    /// A mic signal: a 440 Hz voice at 0.4 while talking (30 ms attack and release),
    /// a -70 dB floor otherwise. `talking` is in frames.
    fn speech(talking: &[std::ops::Range<usize>], frame: usize) -> Vec<f32> {
        const EDGE: f32 = 1440.0;
        (0..FRAME_SAMPLES)
            .map(|i| {
                let n = (frame * FRAME_SAMPLES + i) as f32;
                let env = talking
                    .iter()
                    .map(|r| {
                        let start = (r.start * FRAME_SAMPLES) as f32;
                        let end = (r.end * FRAME_SAMPLES) as f32;
                        ((n - start) / EDGE).min((end - n) / EDGE).clamp(0.0, 1.0)
                    })
                    .fold(0.0f32, f32::max);
                let tone = (n * 2.0 * std::f32::consts::PI * 440.0 / SAMPLE_RATE as f32).sin();
                (0.0003 + 0.4 * env) * tone
            })
            .collect()
    }

    /// A talker on voice activation and a listener, wired back to back: one capture
    /// frame per tick goes in, everything the listener's output device would play
    /// comes out.
    fn talk_and_listen(depth: usize, talking: &[std::ops::Range<usize>], ticks: usize) -> Vec<f32> {
        let mut talker = AudioPipeline::new();
        let mut listener = AudioPipeline::new();
        let talk = ctl(InputMode::Vad);
        let mut listen = ctl(InputMode::Ptt);
        listen.voice_tuning.insert(
            1,
            StreamTuning {
                jitter_depth: depth,
                playback_keep_frames: 3,
            },
        );
        let mut played = Vec::new();
        for t in 0..ticks {
            push_capped(&talker.capture_ring, &speech(talking, t));
            for (key, seq, payload) in talker.tick(&talk) {
                listener.on_voice(key, SessionId(2), seq, &payload);
            }
            listener.tick(&listen);
            played.extend(listener.playback_ring.lock().unwrap().drain(..));
        }
        played
    }

    #[test]
    fn listener_hears_whole_words_and_no_click_after_any_pause() {
        // The voice itself moves at most 0.023 per sample (0.4 at 440 Hz); a burst
        // that starts mid-wave steps by up to 0.4.
        // Scaled by the playback boost.
        const MAX_STEP: f32 = 0.05 * OUTPUT_BOOST;
        for depth in [2usize, 3, 6] {
            // The hangover keeps the gate open for 15 quiet frames, so these pauses
            // shut it for 0 to 35 frames: no gap, a short one, a long one.
            for pause in 10..=50 {
                let talking = [10..40, 40 + pause..70 + pause];
                let played = talk_and_listen(depth, &talking, 70 + pause + 30);
                let step = played
                    .windows(2)
                    .map(|w| (w[1] - w[0]).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    step < MAX_STEP,
                    "depth {depth}, pause {pause} frames: output steps by {step}"
                );
                let loud = played
                    .chunks(FRAME_SAMPLES)
                    .filter(|c| rms_db(c) > -26.0)
                    .count();
                assert!(
                    loud >= 59,
                    "depth {depth}, pause {pause} frames: {loud} of 60 talking frames heard"
                );
            }
        }
    }

    #[test]
    fn fade_shapes() {
        let mut f = vec![1.0f32; 100];
        fade(&mut f, FadeDir::In, 10);
        assert!(f[0] < 0.05 && f[9] > 0.95 && f[10] == 1.0);
        let mut f = vec![1.0f32; 100];
        fade(&mut f, FadeDir::Out, 10);
        assert!(f[99] < 0.05 && f[90] > 0.95 && f[89] == 1.0);
    }

    #[test]
    fn send_only_to_talk_target() {
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        let mut c = ctl(InputMode::Open);
        c.talk_target = Some(42);
        h.drive(960);
        let sent = p.tick(&c);
        assert_eq!(sent.iter().map(|s| s.0).collect::<Vec<_>>(), vec![42]);
        c.talk_target = None;
        h.drive(960);
        assert!(p.tick(&c).is_empty());
    }

    #[test]
    fn underrun_plays_silence() {
        let (p, h, _s) = rig(FakeMode::Silent);
        // Nothing ticked: playback ring empty → zeros.
        h.drive(480);
        assert_eq!(h.played(), vec![0.0; 480]);
        // Ring holding fewer samples than requested: rest is zero.
        p.playback_ring.lock().unwrap().extend([0.5f32; 10]);
        h.clear_played();
        h.drive(20);
        let played = h.played();
        assert_eq!(&played[..10], &[0.5; 10]);
        assert_eq!(&played[10..], &[0.0; 10]);
    }

    #[test]
    fn overrun_drops_oldest() {
        let p = AudioPipeline::new();
        let mut cb = p.capture_cb();
        let first: Vec<f32> = (0..MAX_RING).map(|i| i as f32).collect();
        cb(&first);
        cb(&[-1.0; 100]);
        let r = p.capture_ring.lock().unwrap();
        assert_eq!(r.len(), MAX_RING);
        assert_eq!(r[0], 100.0, "oldest 100 dropped");
        assert_eq!(*r.back().unwrap(), -1.0);
    }

    fn open_ctl() -> PipelineCtl {
        PipelineCtl {
            mode: InputMode::Open,
            talk_target: Some(1),
            ..PipelineCtl::default()
        }
    }

    #[test]
    fn capture_ring_catches_up() {
        // A mic that runs fast: 1000 samples per 960-sample tick.
        let mut p = AudioPipeline::new();
        let mut cb = p.capture_cb();
        let c = open_ctl();
        for _ in 0..200 {
            cb(&[0.1; 1000]);
            p.tick(&c);
        }
        assert!(
            p.trimmed_samples().0 > 0,
            "persistent backlog must be trimmed"
        );
        let left = p.capture_ring.lock().unwrap().len();
        assert!(
            left <= CAPTURE_KEEP + 2 * FRAME_SAMPLES,
            "{left} samples left"
        );
    }

    #[test]
    fn playback_ring_capped() {
        // A slow output device: 900 samples pulled per 960-sample tick.
        let mut p = AudioPipeline::new();
        let mut pull = p.playback_cb();
        let mut st = EncoderState::default();
        let pcm = [4000i16; FRAME_SAMPLES];
        let c = ctl(InputMode::Ptt);
        let mut out = vec![0f32; 900];
        for seq in 0..200 {
            p.on_voice(1, SessionId(2), seq, &adpcm::encode_frame(&pcm, &mut st));
            p.tick(&c);
            pull(&mut out);
        }
        assert!(
            p.trimmed_samples().1 > 0,
            "persistent backlog must be trimmed"
        );
        let n = p.playback_ring.lock().unwrap().len();
        assert!(n <= PLAYBACK_KEEP + 2 * FRAME_SAMPLES, "{n} samples queued");
    }

    #[test]
    fn bursty_capture_loses_no_samples() {
        // A device that delivers 100 ms chunks every 5 ticks.
        let mut p = AudioPipeline::new();
        let mut cb = p.capture_cb();
        let c = open_ctl();
        let mut frames_out = 0;
        for t in 0..50 {
            if t % 5 == 0 {
                cb(&[0.1; 4800]);
            }
            frames_out += p.tick(&c).len();
        }
        assert_eq!(p.trimmed_samples().0, 0, "a burst is not a backlog");
        let ring = p.capture_ring.lock().unwrap().len();
        assert_eq!(
            frames_out * FRAME_SAMPLES + ring,
            48_000,
            "every sample conserved"
        );
    }

    #[test]
    fn bursty_playback_pull_no_trim() {
        // An output device with a 100 ms period: pulls 4800 samples every 5 ticks.
        let mut p = AudioPipeline::new();
        let mut pull = p.playback_cb();
        let mut st = EncoderState::default();
        let pcm = [4000i16; FRAME_SAMPLES];
        let c = ctl(InputMode::Ptt);
        let mut out = vec![0f32; 4800];
        for t in 0..100u32 {
            p.on_voice(1, SessionId(2), t, &adpcm::encode_frame(&pcm, &mut st));
            p.tick(&c);
            if t % 5 == 4 {
                pull(&mut out);
            }
        }
        assert_eq!(p.trimmed_samples().1, 0);
    }

    /// Tick at which a stream fed one frame per tick first reaches the playback ring.
    fn first_audible_tick(c: &PipelineCtl) -> u32 {
        let mut p = AudioPipeline::new();
        let mut st = EncoderState::default();
        let pcm = [8000i16; FRAME_SAMPLES];
        for tick in 1..=10u32 {
            p.on_voice(7, SessionId(1), tick, &adpcm::encode_frame(&pcm, &mut st));
            p.tick(c);
            let mut r = p.playback_ring.lock().unwrap();
            let audible = r.back().is_some_and(|x| *x > 0.1);
            r.clear();
            if audible {
                return tick;
            }
        }
        panic!("never audible");
    }

    #[test]
    fn voice_tuning_depth_sets_delay() {
        let mut c = ctl(InputMode::Ptt);
        assert_eq!(first_audible_tick(&c), 3, "default depth 3");
        c.voice_tuning.insert(
            7,
            StreamTuning {
                jitter_depth: 2,
                playback_keep_frames: 2,
            },
        );
        assert_eq!(first_audible_tick(&c), 2, "competitive depth 2");
        // Another server's tuning does not affect this one.
        c.voice_tuning.clear();
        c.voice_tuning.insert(
            8,
            StreamTuning {
                jitter_depth: 6,
                playback_keep_frames: 4,
            },
        );
        assert_eq!(first_audible_tick(&c), 3);
    }

    #[test]
    fn playback_keep_is_min_over_servers() {
        let mut p = AudioPipeline::new();
        let mut c = ctl(InputMode::Ptt);
        p.tick(&c);
        assert_eq!(p.playback_trim.keep, PLAYBACK_KEEP, "empty map: default");
        c.voice_tuning.insert(
            1,
            StreamTuning {
                jitter_depth: 6,
                playback_keep_frames: 4,
            },
        );
        p.tick(&c);
        assert_eq!(p.playback_trim.keep, 4 * FRAME_SAMPLES);
        c.voice_tuning.insert(
            2,
            StreamTuning {
                jitter_depth: 2,
                playback_keep_frames: 2,
            },
        );
        p.tick(&c);
        assert_eq!(p.playback_trim.keep, 2 * FRAME_SAMPLES);
        c.voice_tuning.clear();
        p.tick(&c);
        assert_eq!(p.playback_trim.keep, PLAYBACK_KEEP);
    }

    #[test]
    fn voice_received_is_mixed_into_playback() {
        let (mut p, h, _s) = rig(FakeMode::Silent);
        let mut st = EncoderState::default();
        let pcm = [8000i16; FRAME_SAMPLES];
        for seq in 0..4 {
            p.on_voice(3, SessionId(1), seq, &adpcm::encode_frame(&pcm, &mut st));
        }
        let c = ctl(InputMode::Ptt);
        p.tick(&c);
        h.clear_played();
        h.drive(960);
        let played = h.played();
        // The burst fades in from silence, then plays at its level.
        assert!(played[0].abs() < 0.01, "{}", played[0]);
        assert!(
            played
                .iter()
                .skip(crate::dsp::jitter::START_RAMP)
                .all(|x| (x - OUTPUT_BOOST * 8000.0 / 32768.0).abs() < 0.05)
        );
    }

    fn feed_stream(p: &AudioPipeline, loud: bool) {
        let frame: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| {
                if loud {
                    (i as f32 * 0.2).sin() * 0.4
                } else {
                    0.0
                }
            })
            .collect();
        (p.stream_cb())(&frame);
    }

    fn stream_frames(sent: &[(ServerKey, u32, Vec<u8>)]) -> Vec<(ServerKey, u32)> {
        sent.iter()
            .filter(|(_, seq, _)| seq & STREAM_BIT != 0)
            .map(|(k, seq, _)| (*k, *seq))
            .collect()
    }

    #[test]
    fn stream_frames_sent_while_muted_and_tagged() {
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        let c = PipelineCtl {
            talk_target: Some(1),
            stream_target: Some(4),
            muted: true,
            mode: InputMode::Open,
            ..PipelineCtl::default()
        };
        for i in 0..3u32 {
            h.drive(960);
            feed_stream(&p, true);
            let sent = p.tick(&c);
            assert_eq!(stream_frames(&sent), vec![(4, i | STREAM_BIT)]);
            assert_eq!(sent.len(), 1, "muted mic sends nothing");
            assert_eq!(sent[0].2.len(), vc_proto::ADPCM_FRAME_BYTES);
        }
    }

    #[test]
    fn no_stream_frames_without_target() {
        let (mut p, h, _s) = rig(FakeMode::Silent);
        let c = ctl(InputMode::Ptt);
        h.drive(960);
        feed_stream(&p, true);
        assert!(p.tick(&c).is_empty());
        assert!(
            p.stream_ring.lock().unwrap().is_empty(),
            "stale audio dropped"
        );
    }

    #[test]
    fn silent_stream_frames_skipped() {
        let (mut p, h, _s) = rig(FakeMode::Silent);
        let c = PipelineCtl {
            stream_target: Some(1),
            ..PipelineCtl::default()
        };
        h.drive(960);
        feed_stream(&p, false);
        assert!(p.tick(&c).is_empty());
        feed_stream(&p, true);
        assert_eq!(stream_frames(&p.tick(&c)).len(), 1);
    }

    #[test]
    fn voice_seq_never_carries_stream_bit() {
        let (mut p, h, _s) = rig(FakeMode::Sine(440.0));
        let c = ctl(InputMode::Open);
        p.seq = STREAM_BIT - 1;
        for _ in 0..3 {
            h.drive(960);
            for (_, seq, _) in p.tick(&c) {
                assert_eq!(seq & STREAM_BIT, 0, "seq {seq:#x}");
            }
        }
    }
}
