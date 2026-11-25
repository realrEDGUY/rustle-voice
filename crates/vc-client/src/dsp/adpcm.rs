//! IMA-ADPCM, one self-contained frame per datagram.
//!
//! Frame layout: `[predictor i16 LE][step index u8][pad u8]` + 480 bytes of nibbles
//! (960 samples, low nibble first). Every frame decodes on its own.

use vc_proto::{ADPCM_FRAME_BYTES, FRAME_SAMPLES};

const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

const INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AdpcmError {
    #[error("ADPCM frame must be {ADPCM_FRAME_BYTES} bytes, got {0}")]
    BadLength(usize),
    #[error("ADPCM step index {0} out of range")]
    BadIndex(u8),
}

/// Encoder state carried between frames (each frame header resets the decoder).
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderState {
    predictor: i32,
    index: i32,
}

fn decode_nibble(nib: u8, predictor: &mut i32, index: &mut i32) -> i16 {
    let step = STEP_TABLE[*index as usize];
    let mut diff = step >> 3;
    if nib & 4 != 0 {
        diff += step;
    }
    if nib & 2 != 0 {
        diff += step >> 1;
    }
    if nib & 1 != 0 {
        diff += step >> 2;
    }
    if nib & 8 != 0 {
        *predictor -= diff;
    } else {
        *predictor += diff;
    }
    *predictor = (*predictor).clamp(-32768, 32767);
    *index = (*index + INDEX_TABLE[nib as usize]).clamp(0, 88);
    *predictor as i16
}

fn encode_sample(sample: i16, predictor: &mut i32, index: &mut i32) -> u8 {
    let step = STEP_TABLE[*index as usize];
    let mut diff = sample as i32 - *predictor;
    let mut nib = 0u8;
    if diff < 0 {
        nib = 8;
        diff = -diff;
    }
    let mut s = step;
    if diff >= s {
        nib |= 4;
        diff -= s;
    }
    s >>= 1;
    if diff >= s {
        nib |= 2;
        diff -= s;
    }
    s >>= 1;
    if diff >= s {
        nib |= 1;
    }
    // Track the decoder exactly so encoder and decoder never drift.
    decode_nibble(nib, predictor, index);
    nib
}

/// Encode one 960-sample frame.
pub fn encode_frame(pcm: &[i16; FRAME_SAMPLES], st: &mut EncoderState) -> [u8; ADPCM_FRAME_BYTES] {
    let mut out = [0u8; ADPCM_FRAME_BYTES];
    let pred = st.predictor.clamp(-32768, 32767) as i16;
    out[0..2].copy_from_slice(&pred.to_le_bytes());
    out[2] = st.index as u8;
    out[3] = 0;
    let mut predictor = pred as i32;
    let mut index = st.index;
    for (i, pair) in pcm.chunks_exact(2).enumerate() {
        let lo = encode_sample(pair[0], &mut predictor, &mut index);
        let hi = encode_sample(pair[1], &mut predictor, &mut index);
        out[4 + i] = lo | (hi << 4);
    }
    st.predictor = predictor;
    st.index = index;
    out
}

/// Decode one frame.
pub fn decode_frame(bytes: &[u8]) -> Result<[i16; FRAME_SAMPLES], AdpcmError> {
    if bytes.len() != ADPCM_FRAME_BYTES {
        return Err(AdpcmError::BadLength(bytes.len()));
    }
    let mut predictor = i16::from_le_bytes([bytes[0], bytes[1]]) as i32;
    if bytes[2] > 88 {
        return Err(AdpcmError::BadIndex(bytes[2]));
    }
    let mut index = bytes[2] as i32;
    let mut out = [0i16; FRAME_SAMPLES];
    for (i, b) in bytes[4..].iter().enumerate() {
        out[2 * i] = decode_nibble(b & 0x0f, &mut predictor, &mut index);
        out[2 * i + 1] = decode_nibble(b >> 4, &mut predictor, &mut index);
    }
    Ok(out)
}

/// f32 [-1, 1] → i16.
pub fn f32_to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// i16 → f32 [-1, 1].
pub fn i16_to_f32(x: i16) -> f32 {
    x as f32 / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, amp: f32, offset: usize) -> [i16; FRAME_SAMPLES] {
        let mut f = [0i16; FRAME_SAMPLES];
        for (i, s) in f.iter_mut().enumerate() {
            let t = (offset + i) as f32 / 48_000.0;
            *s = (amp * 32767.0 * (2.0 * std::f32::consts::PI * freq * t).sin()) as i16;
        }
        f
    }

    fn snr_db(orig: &[i16], dec: &[i16]) -> f64 {
        let mut sig = 0f64;
        let mut noise = 0f64;
        for (a, b) in orig.iter().zip(dec) {
            sig += (*a as f64).powi(2);
            noise += (*a as f64 - *b as f64).powi(2);
        }
        10.0 * (sig / noise.max(1e-9)).log10()
    }

    // Measured on this implementation: 440 Hz sine at 0.5 amplitude over
    // 50 frames, first 8 samples of each frame skipped -> SNR = 46.49 dB
    // (cargo test -- --nocapture adpcm_roundtrip). Threshold = measured - 3 dB.
    const MEASURED_SNR_DB: f64 = 46.49;

    #[test]
    fn adpcm_roundtrip_sine_snr_above_threshold() {
        let mut st = EncoderState::default();
        let mut orig = Vec::new();
        let mut dec = Vec::new();
        for f in 0..50 {
            let pcm = sine(440.0, 0.5, f * FRAME_SAMPLES);
            let enc = encode_frame(&pcm, &mut st);
            let out = decode_frame(&enc).unwrap();
            orig.extend_from_slice(&pcm[8..]);
            dec.extend_from_slice(&out[8..]);
        }
        let snr = snr_db(&orig, &dec);
        println!("ADPCM SNR = {snr:.2} dB");
        assert!(snr > MEASURED_SNR_DB - 3.0, "SNR {snr:.2} dB too low");
    }

    #[test]
    fn adpcm_frame_independent() {
        let mut st = EncoderState::default();
        let frames: Vec<_> = (0..5)
            .map(|f| encode_frame(&sine(300.0, 0.4, f * FRAME_SAMPLES), &mut st))
            .collect();
        // Decoding frame 3 alone equals decoding it after 0..3.
        let alone = decode_frame(&frames[3]).unwrap();
        for f in &frames[..3] {
            decode_frame(f).unwrap();
        }
        let seq = decode_frame(&frames[3]).unwrap();
        assert_eq!(alone, seq);
        // And frame 3 alone is close to the original even without frames 0..2.
        let orig = sine(300.0, 0.4, 3 * FRAME_SAMPLES);
        assert!(snr_db(&orig[8..], &alone[8..]) > 20.0);
    }

    #[test]
    fn adpcm_frame_fits_datagram() {
        let mut st = EncoderState::default();
        let enc = encode_frame(&sine(1000.0, 0.9, 0), &mut st);
        assert_eq!(enc.len(), ADPCM_FRAME_BYTES);
        assert_eq!(ADPCM_FRAME_BYTES, 4 + FRAME_SAMPLES / 2);
        // 8-byte server header + frame is well below QUIC's 1200-byte minimum datagram.
        const { assert!(8 + ADPCM_FRAME_BYTES < 1200) };
        assert_eq!(decode_frame(&enc[..100]), Err(AdpcmError::BadLength(100)));
        let mut bad = enc;
        bad[2] = 200;
        assert_eq!(decode_frame(&bad), Err(AdpcmError::BadIndex(200)));
    }

    #[test]
    fn adpcm_silence_stays_silent() {
        let mut st = EncoderState::default();
        let silent = [0i16; FRAME_SAMPLES];
        for _ in 0..3 {
            let out = decode_frame(&encode_frame(&silent, &mut st)).unwrap();
            assert!(
                out.iter().all(|s| s.abs() <= 8),
                "max {:?}",
                out.iter().max()
            );
        }
    }
}
