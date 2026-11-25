//! Energy voice-activity detection with hangover.

#[derive(Debug, Clone)]
pub struct Vad {
    pub threshold_db: f32,
    pub hangover_frames: u32,
    remaining: u32,
}

/// RMS level in dBFS (floor -100).
pub fn rms_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let ms = frame.iter().map(|x| x * x).sum::<f32>() / frame.len() as f32;
    if ms <= 1e-10 {
        return -100.0;
    }
    (10.0 * ms.log10()).max(-100.0)
}

impl Vad {
    pub fn new(threshold_db: f32) -> Self {
        Self {
            threshold_db,
            hangover_frames: 15,
            remaining: 0,
        }
    }

    /// True while the frame is above threshold or within the hangover after it.
    pub fn update(&mut self, frame: &[f32]) -> bool {
        if rms_db(frame) >= self.threshold_db {
            self.remaining = self.hangover_frames;
            true
        } else if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    /// After an `update` that returned true: was that the last hangover frame (the
    /// gate closes on the next quiet frame)?
    pub fn closing(&self) -> bool {
        self.hangover_frames > 0 && self.remaining == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_threshold_and_hangover() {
        let loud = vec![0.1f32; 960]; // -20 dB
        let quiet = vec![0.001f32; 960]; // -60 dB
        assert!((rms_db(&loud) + 20.0).abs() < 0.01);
        assert!((rms_db(&quiet) + 60.0).abs() < 0.01);
        assert_eq!(rms_db(&[0.0; 10]), -100.0);
        let mut v = Vad::new(-45.0);
        assert!(!v.update(&quiet));
        assert!(v.update(&loud));
        for i in 0..15 {
            assert!(v.update(&quiet), "hangover frame {i}");
        }
        assert!(!v.update(&quiet));
        // Threshold change takes effect.
        v.threshold_db = -70.0;
        assert!(v.update(&quiet));
    }
}
