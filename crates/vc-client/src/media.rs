//! Image decoding and encoding for chat images and screen-share frames.
//!
//! Everything here is CPU work that runs off the UI thread (`spawn_blocking` on the
//! network runtime, or the capture thread).

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use egui::ColorImage;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageReader, Limits, RgbaImage,
};
use vc_proto::MAX_SCREEN_FRAME_BYTES;

/// Long side of an image as sent to the server.
pub const SEND_MAX_SIDE: u32 = 2560;
/// Fallback long side when a JPEG at `SEND_MAX_SIDE` is still over the cap.
pub const SEND_FALLBACK_SIDE: u32 = 1600;
/// Long side of a decoded image kept for display.
pub const DISPLAY_MAX_SIDE: u32 = 1280;
/// Largest file accepted by drag and drop (checked before reading it).
pub const DROP_FILE_MAX: u64 = 32 * 1024 * 1024;
/// Long side of a decoded screen frame kept for display (and the largest box a
/// sender uses: anything larger would be downscaled by every viewer anyway).
pub const SCREEN_DISPLAY_MAX_SIDE: u32 = 2560;
/// Default box screen frames are fitted inside before encoding.
pub const FRAME_MAX_W: u32 = 1280;
pub const FRAME_MAX_H: u32 = 720;
/// Rungs past the quality fallback halve the frame while it stays at least this wide.
pub const FRAME_MIN_W: u32 = 640;
/// Decode limits for local files and pastes we are about to send.
pub const SEND_DECODE_MAX_SIDE: u32 = 8192;
pub const SEND_DECODE_MAX_ALLOC: u64 = 512 * 1024 * 1024;
/// Decode limits for untrusted images received from peers.
pub const RECV_DECODE_MAX_SIDE: u32 = 4096;
pub const RECV_DECODE_MAX_ALLOC: u64 = 96 * 1024 * 1024;

const JPEG_QUALITY: u8 = 85;
/// Default screen-frame JPEG quality.
pub const FRAME_QUALITY: u8 = 60;
/// Quality the encode ladder drops to when a frame is over the cap.
pub const FRAME_FALLBACK_QUALITY: u8 = 35;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("image too large")]
    TooLarge,
    #[error("not a PNG or JPEG image")]
    NotImage,
    #[error("io: {0}")]
    Io(String),
}

impl UploadError {
    /// Text shown in the error toast.
    pub fn text(&self) -> &'static str {
        match self {
            UploadError::TooLarge => "Image too large",
            UploadError::NotImage => "Not a PNG or JPEG image",
            UploadError::Io(_) => "Could not read file",
        }
    }
}

/// Where an image to upload comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum UploadSource {
    /// Clipboard image (Ctrl+V).
    Pasted(Arc<ColorImage>),
    /// A file dropped on the window.
    File(PathBuf),
}

/// Decode a PNG or JPEG with dimension and allocation limits.
pub fn decode_image(bytes: &[u8], max_side: u32, max_alloc: u64) -> Result<RgbaImage, UploadError> {
    if vc_proto::sniff(bytes).is_none() {
        return Err(UploadError::NotImage);
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| UploadError::NotImage)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_side);
    limits.max_image_height = Some(max_side);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits.clone());
    let map = |e: image::ImageError| match e {
        image::ImageError::Limits(_) => UploadError::TooLarge,
        _ => UploadError::NotImage,
    };
    // Decode by hand (not `reader.decode()`) to read the EXIF orientation, so
    // portrait phone photos come out upright on both the send and receive paths.
    let mut dec = reader.into_decoder().map_err(map)?;
    // `reader.decode()` reserved the output buffer against `max_alloc`; keep that.
    limits.reserve(dec.total_bytes()).map_err(map)?;
    let orientation = dec.orientation().map_err(map)?;
    let mut img = DynamicImage::from_decoder(dec).map_err(map)?;
    img.apply_orientation(orientation);
    Ok(img.into_rgba8())
}

/// The size a frame of `(w, h)` is encoded at for the box `(max_w, max_h)`: the
/// same fit both encoders use (box capped at `SCREEN_DISPLAY_MAX_SIDE`).
pub fn fitted_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    fit(
        w,
        h,
        max_w.clamp(1, SCREEN_DISPLAY_MAX_SIDE),
        max_h.clamp(1, SCREEN_DISPLAY_MAX_SIDE),
    )
}

/// Size that fits `(w, h)` inside `(max_w, max_h)` keeping the aspect ratio (never upscales).
fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let scale = f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64);
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

thread_local! {
    /// Reused per thread (capture thread, decode threads): keeps its scratch buffers.
    static RESIZER: std::cell::RefCell<fast_image_resize::Resizer> =
        std::cell::RefCell::new(fast_image_resize::Resizer::new());
}

/// Bilinear SIMD resize (pure Rust); `image`'s Triangle filter as a fallback.
fn resize_rgba(img: &RgbaImage, w: u32, h: u32) -> RgbaImage {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType as F, PixelType, ResizeAlg, ResizeOptions};
    let fast = (|| {
        let src = ImageRef::new(img.width(), img.height(), img.as_raw(), PixelType::U8x4).ok()?;
        let mut dst = Image::new(w, h, PixelType::U8x4);
        let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(F::Bilinear));
        RESIZER
            .with(|r| r.borrow_mut().resize(&src, &mut dst, &opts))
            .ok()?;
        RgbaImage::from_raw(w, h, dst.into_vec())
    })();
    fast.unwrap_or_else(|| image::imageops::resize(img, w, h, FilterType::Triangle))
}

pub(crate) fn downscale(img: &RgbaImage, max_w: u32, max_h: u32) -> RgbaImage {
    let (w, h) = fit(img.width(), img.height(), max_w, max_h);
    if (w, h) == img.dimensions() {
        img.clone()
    } else {
        resize_rgba(img, w, h)
    }
}

/// Convert to an egui image no larger than `max_side`. `source_size` keeps the
/// dimensions before display downscaling.
pub fn to_color_image(img: &RgbaImage, max_side: u32) -> ColorImage {
    let small = downscale(img, max_side, max_side);
    let size = [small.width() as usize, small.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, small.as_raw())
        .with_source_size(egui::vec2(img.width() as f32, img.height() as f32))
}

fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, UploadError> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| UploadError::Io(e.to_string()))?;
    Ok(out)
}

/// JPEG has no alpha: the encoder only accepts L8 / Rgb8, so drop alpha first.
fn encode_jpeg(img: &RgbaImage, quality: u8) -> Result<Vec<u8>, UploadError> {
    let rgb = DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&rgb)
        .map_err(|e| UploadError::Io(e.to_string()))?;
    Ok(out)
}

/// Re-encode an image until it fits `cap`: PNG, then JPEG, then a smaller JPEG.
/// With `prefer_jpeg` (the source was a JPEG photo) the PNG step is skipped, so a
/// photo does not grow into a multi-MB PNG. Re-encoding carries no metadata over.
fn encode_ladder(img: &RgbaImage, cap: u32, prefer_jpeg: bool) -> Result<Vec<u8>, UploadError> {
    let cap = cap as usize;
    let big = downscale(img, SEND_MAX_SIDE, SEND_MAX_SIDE);
    if !prefer_jpeg {
        let png = encode_png(&big)?;
        if png.len() <= cap {
            return Ok(png);
        }
    }
    let jpeg = encode_jpeg(&big, JPEG_QUALITY)?;
    if jpeg.len() <= cap {
        return Ok(jpeg);
    }
    let small = downscale(&big, SEND_FALLBACK_SIDE, SEND_FALLBACK_SIDE);
    let jpeg = encode_jpeg(&small, JPEG_QUALITY)?;
    if jpeg.len() <= cap {
        return Ok(jpeg);
    }
    Err(UploadError::TooLarge)
}

fn color_to_rgba(img: &ColorImage) -> RgbaImage {
    let mut raw = Vec::with_capacity(img.pixels.len() * 4);
    for p in &img.pixels {
        raw.extend_from_slice(&p.to_srgba_unmultiplied());
    }
    RgbaImage::from_raw(img.size[0] as u32, img.size[1] as u32, raw)
        .expect("pixel count matches size")
}

/// Turn an upload source into PNG or JPEG bytes no larger than `cap`.
pub fn prepare_upload(src: UploadSource, cap: u32) -> Result<Vec<u8>, UploadError> {
    match src {
        UploadSource::File(path) => {
            let meta = std::fs::metadata(&path).map_err(|e| UploadError::Io(e.to_string()))?;
            if meta.len() > DROP_FILE_MAX {
                return Err(UploadError::TooLarge);
            }
            let bytes = std::fs::read(&path).map_err(|e| UploadError::Io(e.to_string()))?;
            // Never sent byte for byte: re-encoding strips EXIF (GPS, capture time,
            // device), PNG text and eXIf chunks. Orientation is applied by the decode.
            let img = decode_image(&bytes, SEND_DECODE_MAX_SIDE, SEND_DECODE_MAX_ALLOC)?;
            let prefer_jpeg = matches!(vc_proto::sniff(&bytes), Some(vc_proto::ImageKind::Jpeg));
            encode_ladder(&img, cap, prefer_jpeg)
        }
        UploadSource::Pasted(ci) => {
            if ci.size[0] == 0 || ci.size[1] == 0 {
                return Err(UploadError::NotImage);
            }
            encode_ladder(&color_to_rgba(&ci), cap, false)
        }
    }
}

/// The encode ladder for one frame: `(w, h, quality)` per rung.
/// Rung 0 is the frame fitted in the box at `quality`, rung 1 the same size at
/// `min(quality, 35)`, and each later rung halves the rung-0 size at 35 while the
/// width stays at least `FRAME_MIN_W`.
fn frame_ladder(w: u32, h: u32, max_w: u32, max_h: u32, quality: u8) -> Vec<(u32, u32, u8)> {
    let max_w = max_w.clamp(1, SCREEN_DISPLAY_MAX_SIDE);
    let max_h = max_h.clamp(1, SCREEN_DISPLAY_MAX_SIDE);
    let (fw, fh) = fit(w, h, max_w, max_h);
    let low = quality.min(FRAME_FALLBACK_QUALITY);
    let mut rungs = vec![(fw, fh, quality), (fw, fh, low)];
    let (mut rw, mut rh) = (fw / 2, fh / 2);
    while rw >= FRAME_MIN_W && rh >= 1 {
        rungs.push((rw, rh, low));
        rw /= 2;
        rh /= 2;
    }
    rungs
}

/// Encode one screen frame: fit inside `max_w`×`max_h` (capped at 2560), JPEG at
/// `quality`, at most 1 MiB. Starts at ladder rung `start_rung` (clamped to the
/// last rung) and walks down until the frame fits; returns the JPEG and the rung
/// used, so the caller can start there next time.
pub fn encode_screen_frame(
    img: &RgbaImage,
    max_w: u32,
    max_h: u32,
    quality: u8,
    start_rung: usize,
) -> Result<(Vec<u8>, usize), UploadError> {
    let quality = quality.clamp(1, 100);
    let max = MAX_SCREEN_FRAME_BYTES as usize;
    let rungs = frame_ladder(img.width(), img.height(), max_w, max_h, quality);
    let first = start_rung.min(rungs.len() - 1);
    for (i, (w, h, q)) in rungs.iter().copied().enumerate().skip(first) {
        let small = downscale(img, w, h);
        let jpeg = encode_jpeg(&small, q)?;
        if jpeg.len() <= max {
            return Ok((jpeg, i));
        }
    }
    Err(UploadError::TooLarge)
}

/// Settings a hardware encoder session was built for: output `(w, h)`, fps, quality.
type HwKey = (u32, u32, u32, u8);

/// Per-share screen encoder, owned by the capture thread: all-intra hardware H.264
/// when the config asks for video, else (and for any frame H.264 cannot carry) the
/// JPEG ladder. A hardware error switches it to JPEG for the rest of the share.
#[derive(Default)]
pub struct ScreenEncoder {
    hw: Option<(HwKey, crate::video::H264Intra)>,
    hw_broken: bool,
}

impl ScreenEncoder {
    /// Encode one frame for `cfg`. Returns the body and the JPEG ladder rung to start
    /// from next time (`rung` unchanged for H.264 frames).
    pub fn encode(
        &mut self,
        img: &RgbaImage,
        cfg: &crate::capture::CaptureCfg,
        rung: usize,
    ) -> Result<(Vec<u8>, usize), UploadError> {
        if cfg.video && !self.hw_broken {
            match self.encode_hw(img, cfg) {
                Ok(Some(au)) => return Ok((au, rung)),
                Ok(None) => {} // too large for one frame: this one goes as JPEG
                Err(e) => {
                    tracing::warn!("hardware encode failed, using JPEG for this share: {e}");
                    self.hw = None;
                    self.hw_broken = true;
                    crate::video::set_hw_failed(true);
                }
            }
        } else if !cfg.video {
            self.hw = None; // free the hardware session while JPEG is in use
        }
        encode_screen_frame(img, cfg.max_w, cfg.max_h, cfg.quality, rung)
    }

    fn encode_hw(
        &mut self,
        img: &RgbaImage,
        cfg: &crate::capture::CaptureCfg,
    ) -> Result<Option<Vec<u8>>, String> {
        let max_w = cfg.max_w.clamp(1, SCREEN_DISPLAY_MAX_SIDE);
        let max_h = cfg.max_h.clamp(1, SCREEN_DISPLAY_MAX_SIDE);
        let small = downscale(img, max_w, max_h);
        // H.264 4:2:0 needs even dimensions: crop the last row / column.
        let (w, h) = (small.width() & !1, small.height() & !1);
        if w < 2 || h < 2 {
            return Ok(None);
        }
        let key = (w, h, cfg.fps(), cfg.quality);
        if self.hw.as_ref().is_none_or(|(k, _)| *k != key) {
            self.hw = None; // only one hardware session open at a time
            let enc = crate::video::H264Intra::new(w, h, key.2, cfg.quality)?;
            self.hw = Some((key, enc));
        }
        let nv12 = crate::video::nv12::rgba_to_nv12(small.as_raw(), small.width(), w, h);
        let (_, enc) = self.hw.as_mut().expect("just built");
        let au = enc.encode(&nv12)?;
        if au.len() > MAX_SCREEN_FRAME_BYTES as usize {
            return Ok(None);
        }
        Ok(Some(au))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_proto::ImageKind;

    /// Deterministic pseudo-random noise (compresses badly, like a photo).
    pub(crate) fn noise(w: u32, h: u32, seed: u32) -> RgbaImage {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        RgbaImage::from_fn(w, h, |_, _| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let b = x.to_le_bytes();
            image::Rgba([b[0], b[1], b[2], 255])
        })
    }

    fn gradient(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        })
    }

    #[test]
    fn png_and_jpeg_roundtrip() {
        let img = gradient(64, 48);
        let png = encode_png(&img).unwrap();
        assert_eq!(vc_proto::sniff(&png), Some(ImageKind::Png));
        let back = decode_image(&png, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back, img);
        let jpeg = encode_jpeg(&img, 90).unwrap();
        assert_eq!(vc_proto::sniff(&jpeg), Some(ImageKind::Jpeg));
        let back = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back.dimensions(), (64, 48));
        assert!(back.pixels().all(|p| p.0[3] == 255));
    }

    #[test]
    fn garbage_is_not_image() {
        assert_eq!(
            decode_image(b"hello world", 4096, 1 << 26),
            Err(UploadError::NotImage)
        );
        // Right magic, broken body.
        let mut fake = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        fake.extend_from_slice(&[0; 64]);
        assert_eq!(
            decode_image(&fake, 4096, 1 << 26),
            Err(UploadError::NotImage)
        );
    }

    #[test]
    fn decompression_bomb_rejected() {
        // 9000 px wide: past the 8192 send limit and the 4096 receive limit.
        let png = encode_png(&RgbaImage::new(9000, 2)).unwrap();
        assert_eq!(
            decode_image(&png, SEND_DECODE_MAX_SIDE, SEND_DECODE_MAX_ALLOC),
            Err(UploadError::TooLarge)
        );
        assert_eq!(
            decode_image(&png, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC),
            Err(UploadError::TooLarge)
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &png).unwrap();
        assert_eq!(
            prepare_upload(UploadSource::File(file.path().to_path_buf()), 4 << 20),
            Err(UploadError::TooLarge)
        );
    }

    #[test]
    fn large_noise_paste_fits_cap() {
        let cap = 4 * 1024 * 1024;
        let img = noise(3840, 2160, 7);
        let ci = Arc::new(to_color_image(&img, 4096));
        let bytes = prepare_upload(UploadSource::Pasted(ci), cap).unwrap();
        assert!(bytes.len() <= cap as usize, "{} > cap", bytes.len());
        let back = decode_image(&bytes, SEND_DECODE_MAX_SIDE, SEND_DECODE_MAX_ALLOC).unwrap();
        assert!(back.width().max(back.height()) <= SEND_MAX_SIDE);
        // Noise does not compress as PNG, so this went down the JPEG path.
        assert_eq!(vc_proto::sniff(&bytes), Some(ImageKind::Jpeg));
    }

    #[test]
    fn small_paste_stays_png() {
        let ci = Arc::new(to_color_image(&gradient(64, 48), 4096));
        let bytes = prepare_upload(UploadSource::Pasted(ci), 4 << 20).unwrap();
        assert_eq!(vc_proto::sniff(&bytes), Some(ImageKind::Png));
        let back = decode_image(&bytes, 4096, 1 << 26).unwrap();
        assert_eq!(back, gradient(64, 48));
    }

    /// Little-endian TIFF block: IFD0 with Orientation (0x0112) and a GPS IFD
    /// pointer (0x8825), then a GPS IFD holding GPSVersionID.
    fn exif_with_gps(orientation: u16) -> Vec<u8> {
        let entry = |t: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]| {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&typ.to_le_bytes());
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&value);
        };
        let gps_ifd: u32 = 8 + 2 + 2 * 12 + 4;
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        let o = orientation.to_le_bytes();
        entry(&mut t, 0x0112, 3, 1, [o[0], o[1], 0, 0]); // SHORT
        entry(&mut t, 0x8825, 4, 1, gps_ifd.to_le_bytes()); // LONG offset
        t.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(t.len() as u32, gps_ifd);
        t.extend_from_slice(&1u16.to_le_bytes());
        entry(&mut t, 0x0000, 1, 4, [2, 3, 0, 0]); // GPSVersionID, BYTE x4
        t.extend_from_slice(&0u32.to_le_bytes());
        t
    }

    fn exif_of(jpeg: &[u8]) -> Option<Vec<u8>> {
        image::codecs::jpeg::JpegDecoder::new(Cursor::new(jpeg))
            .unwrap()
            .exif_metadata()
            .unwrap()
    }

    #[test]
    fn small_file_reencoded_without_metadata() {
        let rgb = DynamicImage::ImageRgba8(gradient(64, 48)).to_rgb8();
        let mut jpeg = Vec::new();
        let mut enc = JpegEncoder::new_with_quality(&mut jpeg, 90);
        enc.set_exif_metadata(exif_with_gps(6)).unwrap();
        enc.write_image(rgb.as_raw(), 64, 48, ExtendedColorType::Rgb8)
            .unwrap();
        assert!(exif_of(&jpeg).is_some(), "input carries EXIF");
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &jpeg).unwrap();
        let bytes = prepare_upload(UploadSource::File(file.path().to_path_buf()), 4 << 20).unwrap();
        assert_ne!(bytes, jpeg, "never sent byte for byte");
        assert_eq!(vc_proto::sniff(&bytes), Some(ImageKind::Jpeg), "stays JPEG");
        assert_eq!(exif_of(&bytes), None, "metadata stripped");
        // Orientation 6 was applied before re-encoding, so the result is upright.
        let back = decode_image(&bytes, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back.dimensions(), (48, 64));
    }

    #[test]
    fn small_png_file_stays_png() {
        let png = encode_png(&gradient(64, 48)).unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &png).unwrap();
        let bytes = prepare_upload(UploadSource::File(file.path().to_path_buf()), 4 << 20).unwrap();
        assert_eq!(vc_proto::sniff(&bytes), Some(ImageKind::Png));
        let back = decode_image(&bytes, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back, gradient(64, 48), "lossless");
    }

    #[test]
    fn screen_frame_fits_box_and_limit() {
        let (jpeg, _) = encode_screen_frame(
            &noise(2560, 1440, 3),
            FRAME_MAX_W,
            FRAME_MAX_H,
            FRAME_QUALITY,
            0,
        )
        .unwrap();
        assert!(jpeg.len() <= MAX_SCREEN_FRAME_BYTES as usize);
        assert_eq!(vc_proto::sniff(&jpeg), Some(ImageKind::Jpeg));
        let back = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert!(back.width() <= FRAME_MAX_W && back.height() <= FRAME_MAX_H);
        assert_eq!(back.dimensions(), (1280, 720));
    }

    #[test]
    fn screen_frame_honours_box_and_quality() {
        let img = gradient(1920, 1080);
        let (jpeg, rung) = encode_screen_frame(&img, 854, 480, 80, 0).unwrap();
        assert_eq!(rung, 0);
        let back = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back.dimensions(), (853, 480));
        // Never upscaled past the source.
        let (jpeg, _) = encode_screen_frame(&img, 2560, 2560, 60, 0).unwrap();
        let back = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back.dimensions(), (1920, 1080));
        // Higher quality, bigger file.
        let noisy = noise(640, 360, 5);
        let lo = encode_screen_frame(&noisy, 1280, 720, 20, 0).unwrap().0;
        let hi = encode_screen_frame(&noisy, 1280, 720, 95, 0).unwrap().0;
        assert!(hi.len() > lo.len());
        // Boxes past 2560 are capped.
        let big = gradient(3840, 2160);
        let (jpeg, _) = encode_screen_frame(&big, 9999, 9999, 60, 0).unwrap();
        let back = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(back.dimensions(), (2560, 1440));
    }

    #[test]
    fn screen_frame_ladder_stays_under_cap() {
        let img = noise(2560, 1440, 9);
        let (jpeg, rung) = encode_screen_frame(&img, 2560, 2560, 95, 0).unwrap();
        assert!(jpeg.len() <= MAX_SCREEN_FRAME_BYTES as usize);
        assert!(rung > 0, "noise at q95 needs the ladder");
        // Starting at the remembered rung gives the same or a lower rung's result.
        let (again, r2) = encode_screen_frame(&img, 2560, 2560, 95, rung).unwrap();
        assert_eq!(r2, rung);
        assert!(again.len() <= MAX_SCREEN_FRAME_BYTES as usize);
        // An out-of-range start rung is clamped, not a panic.
        assert!(encode_screen_frame(&img, 2560, 2560, 95, 99).is_ok());
    }

    #[test]
    fn frame_ladder_has_no_duplicate_rungs() {
        let l = frame_ladder(1000, 500, 2560, 2560, 60);
        assert_eq!(l, vec![(1000, 500, 60), (1000, 500, 35)]);
        let l = frame_ladder(2560, 1440, 2560, 2560, 90);
        assert_eq!(
            l,
            vec![
                (2560, 1440, 90),
                (2560, 1440, 35),
                (1280, 720, 35),
                (640, 360, 35)
            ]
        );
    }

    #[test]
    fn oversize_file_rejected_before_read() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(DROP_FILE_MAX + 1).unwrap();
        assert_eq!(
            prepare_upload(UploadSource::File(file.path().to_path_buf()), 4 << 20),
            Err(UploadError::TooLarge)
        );
        let txt = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(txt.path(), b"just text").unwrap();
        assert_eq!(
            prepare_upload(UploadSource::File(txt.path().to_path_buf()), 4 << 20),
            Err(UploadError::NotImage)
        );
        assert!(matches!(
            prepare_upload(UploadSource::File("does/not/exist.png".into()), 4 << 20),
            Err(UploadError::Io(_))
        ));
    }

    #[test]
    fn to_color_image_downscales_and_keeps_source_size() {
        let ci = to_color_image(&gradient(2000, 1000), DISPLAY_MAX_SIDE);
        assert_eq!(ci.size, [1280, 640]);
        assert_eq!(ci.source_size, egui::vec2(2000.0, 1000.0));
    }

    /// Minimal little-endian TIFF block with one Orientation (0x0112) entry.
    fn exif_orientation(value: u16) -> Vec<u8> {
        let mut t = b"II*\0".to_vec();
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD offset
        t.extend_from_slice(&1u16.to_le_bytes()); // one entry
        t.extend_from_slice(&0x0112u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        t.extend_from_slice(&1u32.to_le_bytes()); // count
        t.extend_from_slice(&value.to_le_bytes());
        t.extend_from_slice(&[0, 0]); // value padding
        t.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        t
    }

    #[test]
    fn exif_orientation_applied() {
        let src = gradient(40, 20);
        let rgb = DynamicImage::ImageRgba8(src).to_rgb8();
        let mut jpeg = Vec::new();
        let mut enc = JpegEncoder::new_with_quality(&mut jpeg, 90);
        // The encoder adds the `Exif\0\0` prefix itself.
        enc.set_exif_metadata(exif_orientation(6)).unwrap();
        enc.write_image(rgb.as_raw(), 40, 20, ExtendedColorType::Rgb8)
            .unwrap();
        let mut dec = image::codecs::jpeg::JpegDecoder::new(Cursor::new(&jpeg)).unwrap();
        assert_eq!(
            dec.orientation().unwrap(),
            image::metadata::Orientation::Rotate90
        );
        let img = decode_image(&jpeg, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(img.dimensions(), (20, 40), "rotated upright");
        // Without the tag the image keeps its stored size.
        let mut plain = Vec::new();
        JpegEncoder::new_with_quality(&mut plain, 90)
            .write_image(rgb.as_raw(), 40, 20, ExtendedColorType::Rgb8)
            .unwrap();
        let img = decode_image(&plain, RECV_DECODE_MAX_SIDE, RECV_DECODE_MAX_ALLOC).unwrap();
        assert_eq!(img.dimensions(), (40, 20));
    }
}
