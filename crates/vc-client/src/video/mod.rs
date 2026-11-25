//! Hardware video for screen sharing: all-intra H.264 through Media Foundation
//! (vendor neutral: NVENC, Quick Sync, AMF are all exposed as hardware MFTs), with
//! a pure-Rust NV12 converter. Every encoded access unit is SPS + PPS + IDR, so each
//! frame decodes on its own and today's drop-when-busy frame rules stay valid.
//!
//! Media Foundation is never touched on the GUI thread: the encoder lives on the
//! capture thread, the decoder on the network's blocking threads, and `detect`
//! runs once on a short helper thread.
//!
//! Non-Windows builds get stubs: no encoder is detected and no decoder exists, so
//! such clients announce `Hello.h264 = false` and always get JPEG.

pub mod nv12;

#[cfg(windows)]
mod mf;

use std::sync::atomic::{AtomicBool, Ordering};

/// A hardware H.264 encoder found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwEncoder {
    /// The MFT's friendly name, e.g. "NVIDIA H.264 Encoder MFT".
    pub name: String,
}

/// A decoded frame: `(width, height, rgba)`.
pub type Decoded = (u32, u32, Vec<u8>);

static HW_FAILED: AtomicBool = AtomicBool::new(false);

/// The hardware encoder failed during a share (that share fell back to JPEG).
pub fn hw_failed() -> bool {
    HW_FAILED.load(Ordering::Relaxed)
}

pub fn set_hw_failed(v: bool) {
    HW_FAILED.store(v, Ordering::Relaxed);
}

/// Initialise COM (multithreaded) on the calling thread, once (Windows only).
#[cfg(windows)]
pub fn ensure_com_thread() -> Result<(), String> {
    mf::ensure_thread()
}

/// The first hardware H.264 encoder (cached; detected once off the calling thread).
#[cfg(windows)]
pub fn detect() -> Option<HwEncoder> {
    mf::detect()
}

#[cfg(not(windows))]
pub fn detect() -> Option<HwEncoder> {
    None
}

/// This client can decode H.264 screen frames (cached).
#[cfg(windows)]
pub fn decoder_available() -> bool {
    mf::decoder_available()
}

#[cfg(not(windows))]
pub fn decoder_available() -> bool {
    false
}

/// All-intra hardware H.264 encoder session. Create, use and drop it on one thread.
#[cfg(windows)]
pub struct H264Intra(mf::Encoder);

#[cfg(windows)]
impl H264Intra {
    /// Open a session for `w`×`h` (even) NV12 input at `fps`, JPEG-style `quality`
    /// (20-95) mapped to the encoder's quality or bitrate.
    pub fn new(w: u32, h: u32, fps: u32, quality: u8) -> Result<Self, String> {
        mf::Encoder::new(w, h, fps, quality).map(Self)
    }

    /// Encode one NV12 frame into one self-contained access unit.
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<u8>, String> {
        self.0.encode(nv12)
    }
}

#[cfg(not(windows))]
pub struct H264Intra {
    _never: std::convert::Infallible,
}

#[cfg(not(windows))]
impl H264Intra {
    pub fn new(_w: u32, _h: u32, _fps: u32, _quality: u8) -> Result<Self, String> {
        Err("hardware video is only supported on Windows".into())
    }

    pub fn encode(&mut self, _nv12: &[u8]) -> Result<Vec<u8>, String> {
        Err("hardware video is only supported on Windows".into())
    }
}

/// Decode one H.264 access unit to `(w, h, rgba)` on the decoder worker for
/// `slot` (a stable per-sharer value; see `mf::DECODE_WORKERS`).
#[cfg(windows)]
pub fn decode_h264(slot: u64, au: &[u8]) -> Result<Decoded, String> {
    mf::decode(slot, au)
}

#[cfg(not(windows))]
pub fn decode_h264(_slot: u64, _au: &[u8]) -> Result<Decoded, String> {
    Err("H.264 decoding is only supported on Windows".into())
}

/// Mean bitrate (bits/s) for the bitrate fallback: grows with size, rate and quality,
/// clamped to 1-40 Mbps.
pub fn bitrate_for(w: u32, h: u32, fps: u32, quality: u8) -> u32 {
    let q = quality.clamp(20, 95) as f64;
    let bpp = 0.04 + 0.12 * (q - 20.0) / 75.0;
    let bits = w as f64 * h as f64 * fps.max(1) as f64 * bpp;
    bits.clamp(1_000_000.0, 40_000_000.0) as u32
}

/// Split an Annex-B byte stream into NAL units (without start codes).
pub fn split_nals(bytes: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(bytes.len(), |n| n - 3);
        // Trailing zero belongs to the next 4-byte start code.
        while end > s && bytes[end - 1] == 0 && k + 1 < starts.len() {
            end -= 1;
        }
        if end > s {
            nals.push(&bytes[s..end]);
        }
    }
    nals
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// Make an encoder access unit self-contained: SPS and PPS first (from the unit
/// itself, else from `header`, the encoder's cached sequence header), then the
/// other NALs, each with a 4-byte start code. Access unit delimiters are dropped.
/// `None` when no SPS/PPS is available or the unit has no IDR slice (NAL 5): a
/// non-IDR frame depends on earlier ones, which viewers may have dropped.
pub fn normalize_au(au: &[u8], header: &[u8]) -> Option<Vec<u8>> {
    let nals = split_nals(au);
    let hdr = split_nals(header);
    let pick = |t: u8| -> Vec<&[u8]> {
        let own: Vec<&[u8]> = nals.iter().copied().filter(|n| nal_type(n) == t).collect();
        if own.is_empty() {
            hdr.iter().copied().filter(|n| nal_type(n) == t).collect()
        } else {
            own
        }
    };
    let (sps, pps) = (pick(7), pick(8));
    let rest: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|n| !matches!(nal_type(n), 7..=9))
        .collect();
    if sps.is_empty() || pps.is_empty() || !rest.iter().any(|n| nal_type(n) == 5) {
        return None;
    }
    let mut out = Vec::with_capacity(au.len() + header.len() + 16);
    for n in sps.iter().chain(&pps).chain(&rest) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x1F];
    const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    const IDR: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x21];
    const SEI: &[u8] = &[0x06, 0x05, 0x01];

    fn annexb(nals: &[&[u8]], four: bool) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            if four {
                v.push(0);
            }
            v.extend_from_slice(&[0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    }

    #[test]
    fn split_nals_both_start_codes() {
        let b = annexb(&[SPS, PPS, IDR], true);
        assert_eq!(split_nals(&b), vec![SPS, PPS, IDR]);
        let b = annexb(&[SPS, PPS, IDR], false);
        assert_eq!(split_nals(&b), vec![SPS, PPS, IDR]);
        assert!(split_nals(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn normalize_keeps_complete_unit() {
        let au = annexb(&[SPS, PPS, IDR], true);
        let n = normalize_au(&au, &[]).unwrap();
        assert_eq!(n, au);
        assert_eq!(vc_proto::sniff_screen(&n), Some(vc_proto::ScreenKind::H264));
    }

    #[test]
    fn normalize_prepends_cached_header() {
        let header = annexb(&[SPS, PPS], true);
        // IDR only, behind an AUD and an SEI.
        let au = annexb(&[&[0x09, 0xF0], SEI, IDR], false);
        let n = normalize_au(&au, &header).unwrap();
        assert_eq!(split_nals(&n), vec![SPS, PPS, SEI, IDR]);
        assert_eq!(vc_proto::sniff_screen(&n), Some(vc_proto::ScreenKind::H264));
    }

    #[test]
    fn normalize_moves_sps_first() {
        let au = annexb(&[SEI, SPS, PPS, IDR], true);
        let n = normalize_au(&au, &[]).unwrap();
        assert_eq!(split_nals(&n), vec![SPS, PPS, SEI, IDR]);
    }

    #[test]
    fn normalize_needs_parameter_sets_and_a_picture() {
        assert_eq!(normalize_au(&annexb(&[IDR], true), &[]), None);
        assert_eq!(normalize_au(&annexb(&[SPS, PPS], true), &[]), None);
        // A P-slice (NAL 1) is not self-contained: rejected even with SPS and PPS.
        const P_SLICE: &[u8] = &[0x41, 0x9A, 0x02];
        assert_eq!(normalize_au(&annexb(&[SPS, PPS, P_SLICE], true), &[]), None);
        assert!(normalize_au(&annexb(&[SPS, PPS, IDR], true), &[]).is_some());
    }

    #[test]
    fn bitrate_clamped_and_monotonic() {
        assert_eq!(bitrate_for(16, 16, 1, 20), 1_000_000);
        assert_eq!(bitrate_for(3840, 2160, 120, 95), 40_000_000);
        assert!(bitrate_for(1920, 1080, 30, 80) > bitrate_for(1920, 1080, 30, 40));
    }
}
