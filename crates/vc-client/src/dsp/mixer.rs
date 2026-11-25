//! Mixes every speaker of every connected server into one mono output.

use std::collections::HashMap;

use vc_proto::{FRAME_SAMPLES, SessionId};

use super::jitter::{Frame, JitterBuffer};
use crate::ServerKey;

/// Sum `streams` (i16 samples with per-stream gain), apply `master` and soft clip.
pub fn mix(streams: &[(&[i16], f32)], master: f32, out: &mut [f32]) {
    for (i, o) in out.iter_mut().enumerate() {
        let mut acc = 0f32;
        for (s, g) in streams {
            if let Some(x) = s.get(i) {
                acc += *x as f32 / 32768.0 * g;
            }
        }
        *o = soft_clip(acc * master);
    }
}

/// Transparent below ~0.5, smoothly limited to (-1, 1) above.
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.5;
    if x.abs() <= KNEE {
        x
    } else {
        let s = x.signum();
        let over = x.abs() - KNEE;
        s * (KNEE + (1.0 - KNEE) * (over / (1.0 - KNEE)).tanh())
    }
}

#[derive(Debug, Default)]
pub struct Mixer {
    streams: HashMap<(ServerKey, SessionId), JitterBuffer>,
}

impl Mixer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, key: ServerKey, speaker: SessionId, seq: u32, frame: Frame) {
        self.streams
            .entry((key, speaker))
            .or_default()
            .push(seq, frame);
    }

    /// Drop every stream of a server (on disconnect).
    pub fn forget_server(&mut self, key: ServerKey) {
        self.streams.retain(|(k, _), _| *k != key);
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Mix the next 20 ms into `out`. `depth` is each server's jitter depth (from its
    /// voice tuning). Returns the speakers that contributed.
    pub fn mix_next(
        &mut self,
        depth: impl Fn(ServerKey) -> usize,
        gain: impl Fn(ServerKey, SessionId) -> f32,
        master: f32,
        deafened: bool,
        out: &mut [f32],
    ) -> Vec<(ServerKey, SessionId)> {
        let mut frames: Vec<((ServerKey, SessionId), Frame)> = Vec::new();
        for (k, j) in self.streams.iter_mut() {
            if let Some(f) = j.pop(depth(k.0)) {
                frames.push((*k, f));
            }
        }
        if deafened {
            out.iter_mut().for_each(|o| *o = 0.0);
            return Vec::new();
        }
        let refs: Vec<(&[i16], f32)> = frames
            .iter()
            .map(|((k, s), f)| (&f[..], gain(*k, *s)))
            .collect();
        let n = FRAME_SAMPLES.min(out.len());
        mix(&refs, master, &mut out[..n]);
        frames.into_iter().map(|(k, _)| k).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::jitter::DEFAULT_DEPTH;

    fn frame(v: i16) -> Frame {
        [v; FRAME_SAMPLES]
    }

    #[test]
    fn mixer_sums_with_gain() {
        let a = [3276i16; 4]; // ~0.1
        let b = [6553i16; 4]; // ~0.2
        let mut out = [0f32; 4];
        mix(&[(&a, 1.0), (&b, 0.5)], 1.0, &mut out);
        for o in out {
            assert!((o - 0.2).abs() < 0.001, "{o}");
        }
        mix(&[(&a, 0.0), (&b, 0.0)], 1.0, &mut out);
        assert!(out.iter().all(|o| *o == 0.0));
    }

    #[test]
    fn mixer_soft_clips() {
        let loud = [30000i16; 4];
        let mut out = [0f32; 4];
        mix(&[(&loud, 1.0), (&loud, 1.0), (&loud, 1.0)], 1.0, &mut out);
        for o in out {
            assert!(o < 1.0 && o > 0.9, "{o}");
        }
        assert!(soft_clip(-5.0) >= -1.0 && soft_clip(-5.0) < -0.99);
        assert!(soft_clip(1.2) < 1.0);
        assert_eq!(soft_clip(0.3), 0.3);
        // Monotonic.
        assert!(soft_clip(0.8) < soft_clip(0.9));
    }

    #[test]
    fn mixer_same_speaker_id_two_servers_separate() {
        let mut m = Mixer::new();
        let s = SessionId(1);
        for seq in 0..3 {
            m.push(10, s, seq, frame(1000));
            m.push(20, s, seq, frame(2000));
        }
        assert_eq!(m.stream_count(), 2);
        let mut out = [0f32; FRAME_SAMPLES];
        let active = m.mix_next(|_| DEFAULT_DEPTH, |_, _| 1.0, 1.0, false, &mut out);
        assert_eq!(active.len(), 2);
        // (Each burst fades in, so read the level at the end of the frame.)
        let level = out[FRAME_SAMPLES - 1];
        assert!((level - 3000.0 / 32768.0).abs() < 1e-4, "{level}");
        // Gain applies per (server, speaker).
        let _ = m.mix_next(
            |_| DEFAULT_DEPTH,
            |k, _| if k == 10 { 0.0 } else { 1.0 },
            1.0,
            false,
            &mut out,
        );
        assert!((out[0] - 2000.0 / 32768.0).abs() < 1e-4, "{}", out[0]);
        m.forget_server(10);
        assert_eq!(m.stream_count(), 1);
    }

    #[test]
    fn mixer_silent_when_deafened() {
        let mut m = Mixer::new();
        for seq in 0..3 {
            m.push(1, SessionId(1), seq, frame(9000));
        }
        let mut out = [0.5f32; FRAME_SAMPLES];
        let active = m.mix_next(|_| DEFAULT_DEPTH, |_, _| 1.0, 1.0, true, &mut out);
        assert!(active.is_empty());
        assert!(out.iter().all(|o| *o == 0.0));
        // Positive control: not deafened â†’ audible.
        let _ = m.mix_next(|_| DEFAULT_DEPTH, |_, _| 1.0, 1.0, false, &mut out);
        assert!(out[0] > 0.2);
    }

    #[test]
    fn mixer_depth_per_server() {
        let mut m = Mixer::new();
        let s = SessionId(1);
        let depth = |k: ServerKey| if k == 10 { 2 } else { 6 };
        let mut out = [0f32; FRAME_SAMPLES];
        let mut audible_at = HashMap::new();
        for tick in 1..=8u32 {
            m.push(10, s, tick, frame(1000));
            m.push(20, s, tick, frame(1000));
            for (k, _) in m.mix_next(depth, |_, _| 1.0, 1.0, false, &mut out) {
                audible_at.entry(k).or_insert(tick);
            }
        }
        assert_eq!(audible_at.get(&10), Some(&2));
        assert_eq!(audible_at.get(&20), Some(&6));
    }

    #[test]
    fn master_volume_applied() {
        let a = [3276i16; 2];
        let mut out = [0f32; 2];
        mix(&[(&a, 1.0)], 2.0, &mut out);
        assert!((out[0] - 0.2).abs() < 0.001);
        mix(&[(&a, 1.0)], 0.0, &mut out);
        assert_eq!(out[0], 0.0);
        mix(&[(&a, 1.0)], 0.5, &mut out);
        assert!((out[0] - 0.05).abs() < 0.001);
    }
}
