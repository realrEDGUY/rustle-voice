//! Per-speaker jitter buffer keyed by sequence number.
//!
//! Voice arrives in talk bursts: nothing is sent while the speaker's gate is shut, and
//! seq carries on from where the last burst stopped. A queue that runs dry is therefore
//! not packet loss. Playback stops there and the buffer fills up again, so the next
//! burst (or the rest of this one, after a network stall) plays from its first frame
//! with the full depth queued. Only a hole with later frames already queued is a lost
//! frame.
//!
//! The target depth is not stored: the server's voice preset can change it at any
//! time, so `pop` takes it on every call.

use std::collections::BTreeMap;

use vc_proto::FRAME_SAMPLES;

pub type Frame = [i16; FRAME_SAMPLES];

/// Frames held before playback starts when the server sent no tuning (balanced).
pub const DEFAULT_DEPTH: usize = 3;
/// `push` never holds more than this many frames, whatever the depth: a burst beyond
/// it drops the oldest. `pop` enforces the tighter per-depth cap `depth + 3`.
pub const HARD_CAP: usize = vc_proto::MAX_VOICE_FRAMES as usize + 3;
/// After this many pops (1 s) with nothing queued the buffer forgets which seq was
/// due, so a speaker whose seq starts over is heard again.
pub const FORGET_AFTER: u32 = 50;
/// Pops (500 ms) over which a playing buffer's minimum queue is watched; a queue
/// that never fell to the depth is trimmed back to it.
const EXCESS_WINDOW: u32 = 25;
/// Samples (10 ms at 48 kHz) over which a burst fades in from silence.
pub const START_RAMP: usize = 480;
/// Samples (5 ms) over which playback ramps out of a stream that stops, and across
/// a skip inside one.
pub const GAP_RAMP: usize = 240;

#[derive(Debug)]
pub struct JitterBuffer {
    frames: BTreeMap<u32, Frame>,
    /// Playing a burst? False while the queue fills: before a burst, and again
    /// after it ran dry.
    playing: bool,
    /// The seq due next. Older frames were played or skipped, so they are dropped
    /// on arrival. `None` before the first frame and after a long silence.
    next: Option<u32>,
    /// Pops since the queue ran dry with nothing arriving.
    dry_pops: u32,
    /// Pops spent filling with frames queued.
    filling_pops: u32,
    /// Smallest pre-pop queue length seen in the current excess window.
    min_len: usize,
    /// Pops counted in the current excess window.
    window: u32,
    /// Last sample handed out. A frame that does not follow it is blended in from it.
    last: i16,
    /// Samples over which the next frame is blended in (0: it follows the last one).
    blend: usize,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            frames: BTreeMap::new(),
            playing: false,
            next: None,
            dry_pops: 0,
            filling_pops: 0,
            min_len: usize::MAX,
            window: 0,
            last: 0,
            blend: 0,
        }
    }
}

/// Raised-cosine weight of sample `i` in a ramp of `len` samples, rising 0 → 1.
fn ramp(i: usize, len: usize) -> f32 {
    0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 0.5) / len as f32).cos()
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queued frame count.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Is the buffer playing a burst (not filling or idle)?
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn push(&mut self, seq: u32, frame: Frame) {
        if self.next.is_some_and(|next| seq < next) {
            return; // late
        }
        self.frames.entry(seq).or_insert(frame); // duplicates ignored
        self.cap(HARD_CAP);
    }

    /// Drop the oldest frames beyond `max`; a playing buffer carries on from the
    /// oldest kept one instead of playing a gap.
    fn cap(&mut self, max: usize) {
        if self.frames.len() <= max {
            return;
        }
        while self.frames.len() > max {
            self.frames.pop_first();
        }
        if self.playing
            && let Some(next) = self.next
        {
            self.next = Some(self.skip_to_oldest(next));
        }
    }

    /// Frames before the oldest queued one are gone for good: the seq to carry on
    /// from. The frame played there is blended in, since it does not follow the
    /// last one.
    fn skip_to_oldest(&mut self, next: u32) -> u32 {
        match self.frames.keys().next() {
            Some(&first) if first != next => {
                self.blend = self.blend.max(GAP_RAMP);
                first
            }
            _ => next,
        }
    }

    /// Next frame to play at target `depth` (0 acts as 1): `Some(frame)` (a ramp to
    /// silence where the stream stops or a frame is lost) or `None` when idle.
    pub fn pop(&mut self, depth: usize) -> Option<Frame> {
        let depth = depth.max(1);
        self.cap(depth + 3);
        if !self.playing && !self.start(depth) {
            return None;
        }
        let mut next = self.next?;
        // Trim a queue that stayed above the depth for a whole window: over-priming
        // (primed at the default before Welcome lowered the depth, or frames that
        // arrived in a clump) or a sender clock running faster than ours. A correctly
        // primed stream sits at exactly `depth` before each pop, so it is never
        // trimmed.
        self.min_len = self.min_len.min(self.frames.len());
        self.window += 1;
        if self.window >= EXCESS_WINDOW {
            if self.min_len > depth {
                for _ in 0..self.min_len - depth {
                    self.frames.pop_first();
                }
                next = self.skip_to_oldest(next);
            }
            self.min_len = usize::MAX;
            self.window = 0;
        }
        // A hole longer than the queue can ever hold is a jump in seq, not loss:
        // carry on from the frames that are here.
        if let Some(&first) = self.frames.keys().next()
            && first.wrapping_sub(next) as usize > depth + 3
        {
            next = self.skip_to_oldest(next);
        }
        if let Some(mut f) = self.frames.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            if self.blend > 0 {
                Self::blend_in(&mut f, self.last, self.blend);
                self.blend = 0;
            }
            self.last = f[FRAME_SAMPLES - 1];
            return Some(f);
        }
        if self.frames.is_empty() {
            // Ran dry: the talk burst is over, or the network stalled for longer than
            // the queue covers. Stop here and fill up again. `next` stays put, so
            // whatever comes now (the next burst carries on from this seq) is on time
            // and plays from its first frame.
            self.playing = false;
            self.next = Some(next);
            self.dry_pops = 0;
            self.filling_pops = 0;
        } else {
            // Later frames are queued, so this one is lost: play its 20 ms as silence.
            self.next = Some(next.wrapping_add(1));
            self.blend = GAP_RAMP;
        }
        Some(self.fade_out())
    }

    /// While not playing: can a burst start? Yes once `depth` frames are queued.
    /// Frames that have waited as long as that takes start anyway, so a burst
    /// shorter than the depth (a tap on push-to-talk) is played now and not in
    /// front of the next burst.
    fn start(&mut self, depth: usize) -> bool {
        let Some(&first) = self.frames.keys().next() else {
            self.filling_pops = 0;
            self.dry_pops = self.dry_pops.saturating_add(1);
            if self.dry_pops > FORGET_AFTER {
                self.next = None;
            }
            return false;
        };
        self.dry_pops = 0;
        self.filling_pops += 1;
        if self.frames.len() < depth && self.filling_pops <= depth as u32 {
            return false;
        }
        self.playing = true;
        self.next = Some(first);
        self.filling_pops = 0;
        // From silence, whatever the sender did: an old client starts mid-wave, and
        // after a stall this is the middle of a word.
        self.blend = START_RAMP;
        self.min_len = usize::MAX;
        self.window = 0;
        true
    }

    /// Blend the first `len` samples of `frame` in from `from` (the last sample
    /// played), so the output has no step where the stream has one.
    fn blend_in(frame: &mut Frame, from: i16, len: usize) {
        let from = from as f32;
        for (i, x) in frame.iter_mut().take(len).enumerate() {
            let w = ramp(i, len);
            *x = (from * (1.0 - w) + *x as f32 * w) as i16;
        }
    }

    /// 20 ms of silence that ramps down from the last sample played.
    fn fade_out(&mut self) -> Frame {
        let mut f = [0; FRAME_SAMPLES];
        let from = self.last as f32;
        for (i, x) in f.iter_mut().take(GAP_RAMP).enumerate() {
            *x = (from * (1.0 - ramp(i, GAP_RAMP))) as i16;
        }
        self.last = 0;
        f
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: i16) -> Frame {
        [v; FRAME_SAMPLES]
    }

    /// The level a popped frame settles at (its start may be blended in).
    fn level(popped: Option<Frame>) -> i16 {
        popped.expect("a frame")[FRAME_SAMPLES - 1]
    }

    fn max_step(frames: &[Frame]) -> i32 {
        let all: Vec<i32> = frames.iter().flatten().map(|x| *x as i32).collect();
        all.windows(2).map(|w| (w[1] - w[0]).abs()).max().unwrap()
    }

    #[test]
    fn jitter_reorders() {
        let mut j = JitterBuffer::new();
        j.push(2, f(2));
        j.push(0, f(0));
        assert_eq!(j.pop(DEFAULT_DEPTH), None, "not primed below target depth");
        j.push(1, f(1));
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 0);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 1);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 2);
    }

    #[test]
    fn jitter_drops_late_and_dupes() {
        let mut j = JitterBuffer::new();
        for s in 10..13 {
            j.push(s, f(s as i16));
        }
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 10);
        j.push(10, f(99)); // late
        j.push(11, f(99)); // duplicate of a queued frame
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 11);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 12);
        assert!(j.is_empty());
    }

    #[test]
    fn jitter_burst_start_fades_in() {
        let mut j = JitterBuffer::new();
        for s in 0..3 {
            j.push(s, f(20000));
        }
        let first = j.pop(DEFAULT_DEPTH).unwrap();
        assert!(first[0].abs() < 10, "starts from silence: {}", first[0]);
        assert!(first[..START_RAMP].windows(2).all(|w| w[1] >= w[0]));
        assert!(first[START_RAMP..].iter().all(|&x| x == 20000));
        let second = j.pop(DEFAULT_DEPTH).unwrap();
        assert_eq!(second, f(20000), "only the first frame is faded");
        let step = max_step(&[first, second]);
        assert!(step < 100, "no hard step into the burst: {step}");
    }

    #[test]
    fn jitter_gap_yields_silence() {
        let mut j = JitterBuffer::new();
        j.push(0, f(5));
        j.push(1, f(5));
        j.push(3, f(7));
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 5);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 5);
        // Gap at seq 2: ramps down from the last sample, then silence (no step).
        let gap = j.pop(DEFAULT_DEPTH).unwrap();
        assert!(gap[0] <= 5 && gap[0] >= 4, "starts at the last sample");
        assert!(gap.windows(2).all(|w| w[1] <= w[0]), "ramps down");
        assert!(gap[GAP_RAMP..].iter().all(|&x| x == 0), "then silence");
        // The frame after the gap fades in, then plays as sent.
        let after = j.pop(DEFAULT_DEPTH).unwrap();
        assert_eq!(after[0], 0);
        assert_eq!(after[GAP_RAMP], 7);
        assert_eq!(after[FRAME_SAMPLES - 1], 7);
    }

    #[test]
    fn jitter_gap_ramps_from_loud_sample() {
        let mut j = JitterBuffer::new();
        j.push(0, f(20000));
        j.push(2, f(20000));
        assert_eq!(level(j.pop(1)), 20000);
        let gap = j.pop(1).unwrap();
        let step = max_step(&[gap]);
        assert!(step < 200, "no hard step into the gap: {step}");
        assert!((20000 - gap[0] as i32) < 200);
    }

    #[test]
    fn jitter_caps_depth_after_burst() {
        let mut j = JitterBuffer::new();
        // Primed and playing.
        for s in 0..DEFAULT_DEPTH as u32 {
            j.push(s, f(s as i16));
        }
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 0);
        // A burst of 100 frames arrives at once.
        for s in 1..=100u32 {
            j.push(s, f(s as i16));
            assert!(j.len() <= HARD_CAP, "len {} after seq {s}", j.len());
        }
        assert_eq!(j.len(), HARD_CAP);
        let first = level(j.pop(DEFAULT_DEPTH)) as u32;
        assert!(j.len() <= DEFAULT_DEPTH + 2, "len {}", j.len());
        let cap = DEFAULT_DEPTH as u32 + 3;
        assert!(
            first > 100 - cap,
            "pop returned seq {first}, not one of the newest {cap}"
        );
        // And it plays on without a gap.
        assert_eq!(level(j.pop(DEFAULT_DEPTH)) as u32, first + 1);
        // Unprimed buffers are capped too.
        let mut k = JitterBuffer::new();
        for s in 0..100u32 {
            k.push(s, f(1));
        }
        assert_eq!(k.len(), HARD_CAP);
        k.pop(DEFAULT_DEPTH).unwrap();
        assert_eq!(k.len(), DEFAULT_DEPTH + 2);
    }

    #[test]
    fn jitter_skip_is_blended_not_stepped() {
        let mut j = JitterBuffer::new();
        for s in 0..DEFAULT_DEPTH as u32 {
            j.push(s, f(20000));
        }
        let before = j.pop(DEFAULT_DEPTH).unwrap();
        // A clump overflows the cap: playback skips ahead to very different audio.
        for s in 1..=100u32 {
            j.push(s, f(-20000));
        }
        let after = j.pop(DEFAULT_DEPTH).unwrap();
        assert_eq!(after[FRAME_SAMPLES - 1], -20000);
        let step = max_step(&[before, after]);
        assert!(step < 400, "the skip is blended over 5 ms: {step}");
    }

    #[test]
    fn jitter_stops_when_dry_and_waits() {
        let mut j = JitterBuffer::new();
        for s in 0..3 {
            j.push(s, f(900));
        }
        for _ in 0..3 {
            assert_eq!(level(j.pop(DEFAULT_DEPTH)), 900);
        }
        // Dry: one ramp down from the last sample, then idle. No gap frames pile up.
        let tail = j.pop(DEFAULT_DEPTH).unwrap();
        assert!(tail[0] > 890 && tail[GAP_RAMP..].iter().all(|&x| x == 0));
        assert!(!j.is_playing());
        for _ in 0..20 {
            assert_eq!(j.pop(DEFAULT_DEPTH), None);
        }
        // Still late: a straggler from the burst that was played.
        j.push(2, f(1));
        assert!(j.is_empty());
        // The next burst carries on at seq 3 and is played from that frame.
        for s in 3..6 {
            j.push(s, f(s as i16));
        }
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 3);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 4);
    }

    #[test]
    fn jitter_seq_jump_plays_on_without_a_long_silence() {
        let mut j = JitterBuffer::new();
        for s in 0..3 {
            j.push(s, f(1));
        }
        for _ in 0..3 {
            j.pop(DEFAULT_DEPTH).unwrap();
        }
        j.pop(DEFAULT_DEPTH).unwrap(); // the ramp down
        // The speaker talked elsewhere meanwhile: seq is far ahead when they return.
        for s in 1000..1003 {
            j.push(s, f(9));
        }
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 9);
        // The same jump while frames are still queued: no frame-by-frame silence.
        j.push(5000, f(7));
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 9);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 9);
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 7);
    }

    #[test]
    fn jitter_forgets_seq_after_long_silence() {
        let mut j = JitterBuffer::new();
        for s in 5000..5003 {
            j.push(s, f(1));
        }
        for _ in 0..3 {
            j.pop(DEFAULT_DEPTH).unwrap();
        }
        j.pop(DEFAULT_DEPTH).unwrap(); // the ramp down
        for _ in 0..FORGET_AFTER {
            assert_eq!(j.pop(DEFAULT_DEPTH), None);
        }
        // Within the window a lower seq is still late.
        j.push(0, f(3));
        assert!(j.is_empty());
        assert_eq!(j.pop(DEFAULT_DEPTH), None);
        // After it, a stream that starts over primes from its own seq.
        for s in 0..3 {
            j.push(s, f(3));
        }
        assert_eq!(level(j.pop(DEFAULT_DEPTH)), 3);
    }

    #[test]
    fn jitter_short_burst_is_played_not_held() {
        let mut j = JitterBuffer::new();
        // One frame (a tap on push-to-talk) never reaches depth 3.
        j.push(0, f(4));
        for _ in 0..DEFAULT_DEPTH {
            assert_eq!(j.pop(DEFAULT_DEPTH), None, "still waiting for the depth");
        }
        assert_eq!(
            level(j.pop(DEFAULT_DEPTH)),
            4,
            "played once the wait is over"
        );
        j.pop(DEFAULT_DEPTH).unwrap(); // the ramp down
        assert!(j.is_empty() && !j.is_playing());
    }

    #[test]
    fn jitter_depth_zero_acts_as_one() {
        let mut j = JitterBuffer::new();
        assert_eq!(j.pop(0), None, "empty buffer must not panic");
        j.push(4, f(4));
        assert_eq!(level(j.pop(0)), 4, "primes on the first frame");
    }

    #[test]
    fn jitter_depth_controls_priming_and_cap() {
        let mut j = JitterBuffer::new();
        j.push(0, f(0));
        assert_eq!(j.pop(2), None);
        j.push(1, f(1));
        assert_eq!(level(j.pop(2)), 0, "depth 2 primes on the 2nd frame");

        let mut k = JitterBuffer::new();
        for s in 0..5u32 {
            k.push(s, f(s as i16));
            assert_eq!(k.pop(6), None, "depth 6 waits for 6 frames");
        }
        k.push(5, f(5));
        assert_eq!(level(k.pop(6)), 0);

        let mut c = JitterBuffer::new();
        for s in 0..50u32 {
            c.push(s, f(1));
        }
        c.pop(6).unwrap();
        assert_eq!(c.len(), 8, "depth 6 caps at 9 before the pop");
    }

    #[test]
    fn jitter_over_primed_drains_to_depth() {
        let mut j = JitterBuffer::new();
        for s in 0..3u32 {
            j.push(s, f(1));
        }
        // Primed at 3, then the server says depth 2; one frame arrives per tick.
        let mut lens = Vec::new();
        for seq in 3..43u32 {
            lens.push(j.len());
            assert!(j.pop(2).is_some());
            j.push(seq, f(1));
        }
        assert_eq!(lens[0], 3);
        assert!(
            lens[EXCESS_WINDOW as usize..].iter().all(|l| *l == 2),
            "pre-pop lengths {lens:?}"
        );
        assert!(j.is_playing());
    }

    /// A frame whose every sample names its seq.
    fn tagged(seq: u32) -> Frame {
        f(seq as i16 + 1)
    }

    /// Which seq a popped frame carries (`None` for a ramp to silence or an idle pop).
    fn seq_of(popped: Option<Frame>) -> Option<u32> {
        let last = popped?[FRAME_SAMPLES - 1];
        (last != 0).then(|| last as u32 - 1)
    }

    /// One talk burst of `first` frames, a pause in which the sender's gate is shut
    /// for `closed` frame times, then a second burst of 30 frames. Like the real
    /// sender, each burst starts with two frames at once (pre-roll + the frame that
    /// opened the gate) and seq keeps counting across the pause. Returns what each
    /// pop played from the moment the second burst starts arriving.
    fn second_burst(depth: usize, first: u32, closed: usize) -> Vec<Option<u32>> {
        let mut j = JitterBuffer::new();
        j.push(0, tagged(0));
        for seq in 1..first {
            j.push(seq, tagged(seq));
            j.pop(depth);
        }
        for _ in 0..closed {
            j.pop(depth);
        }
        let mut played = Vec::new();
        j.push(first, tagged(first));
        for seq in first + 1..first + 30 {
            j.push(seq, tagged(seq));
            played.push(seq_of(j.pop(depth)));
        }
        for _ in 0..depth + 2 {
            played.push(seq_of(j.pop(depth)));
        }
        played
    }

    #[test]
    fn jitter_plays_whole_burst_after_any_pause() {
        // The three server presets. (At depth 1 the pre-roll pair over-primes the
        // buffer by one frame, which the excess trim removes on purpose.)
        for depth in [2usize, 3, 6] {
            for closed in 0..80 {
                let first = 20u32;
                let played = second_burst(depth, first, closed);
                let got: Vec<u32> = played
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|s| *s >= first)
                    .collect();
                let want: Vec<u32> = (first..first + 30).collect();
                assert_eq!(
                    got, want,
                    "depth {depth}, gate shut {closed} frames: second burst played {played:?}"
                );
            }
        }
    }

    #[test]
    fn jitter_correct_depth_never_trimmed() {
        let mut j = JitterBuffer::new();
        for seq in 0..DEFAULT_DEPTH as u32 {
            j.push(seq, f(seq as i16));
        }
        for expect in 0..100u32 {
            assert_eq!(j.len(), DEFAULT_DEPTH);
            let fr = j.pop(DEFAULT_DEPTH).unwrap();
            assert_eq!(level(Some(fr)) as u32, expect, "no frame skipped");
            let seq = expect + DEFAULT_DEPTH as u32;
            j.push(seq, f(seq as i16));
        }
    }
}
