//! Linear resampling and channel conversion.

/// Streaming linear resampler (mono).
#[derive(Debug, Clone)]
pub struct Resampler {
    from: u32,
    to: u32,
    pos: f64,
    last: f32,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            from,
            to,
            pos: 0.0,
            last: 0.0,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.from == self.to
    }

    /// Resample one chunk, keeping phase across calls.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let step = self.from as f64 / self.to as f64;
        // Sample index -1 is `self.last`; positions are relative to input[0].
        let get = |i: isize, last: f32| -> f32 { if i < 0 { last } else { input[i as usize] } };
        while self.pos < input.len() as f64 - 1.0 + 1.0 {
            let base = self.pos.floor();
            let frac = (self.pos - base) as f32;
            let i = base as isize - 1;
            let a = get(i, self.last);
            let b = get(i + 1, self.last);
            out.push(a + (b - a) * frac);
            self.pos += step;
        }
        self.pos -= input.len() as f64;
        if let Some(l) = input.last() {
            self.last = *l;
        }
    }
}

/// One-shot resample.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let mut r = Resampler::new(from, to);
    let mut out = Vec::with_capacity(input.len() * to as usize / from.max(1) as usize + 2);
    r.process(input, &mut out);
    out
}

pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|c| c.iter().sum::<f32>() / channels as f32)
        .collect()
}

pub fn upmix_from_mono(mono: &[f32], channels: usize, out: &mut [f32]) {
    let channels = channels.max(1);
    for (frame, s) in out.chunks_exact_mut(channels).zip(mono) {
        frame.iter_mut().for_each(|o| *o = *s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_crossing_hz(x: &[f32], rate: f32) -> f32 {
        let crossings = x
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f32 / 2.0 / (x.len() as f32 / rate)
    }

    #[test]
    fn resample_44k1_to_48k_length_and_freq() {
        let input: Vec<f32> = (0..44_100)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44_100.0).sin())
            .collect();
        // Chunked processing must match a 1 s input → ~48000 samples.
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for c in input.chunks(441) {
            r.process(c, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "len {}", out.len());
        let hz = zero_crossing_hz(&out, 48_000.0);
        assert!((hz - 1000.0).abs() < 5.0, "{hz}");
        let one = resample(&input, 44_100, 48_000);
        assert!((one.len() as i64 - 48_000).abs() <= 2);
        assert_eq!(resample(&[1.0, 2.0], 48_000, 48_000), vec![1.0, 2.0]);
    }

    #[test]
    fn downmix_stereo() {
        let st = [1.0, 0.0, 0.5, 0.5, -1.0, 1.0];
        assert_eq!(downmix_to_mono(&st, 2), vec![0.5, 0.5, 0.0]);
        assert_eq!(downmix_to_mono(&st, 1), st.to_vec());
    }

    #[test]
    fn upmix_mono_to_stereo() {
        let mut out = [0.0f32; 6];
        upmix_from_mono(&[0.1, 0.2, 0.3], 2, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }
}
