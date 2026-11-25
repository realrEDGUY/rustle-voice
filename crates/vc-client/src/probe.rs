//! "Test max": measure how fast this machine can capture and encode the screen at
//! each resolution, through the same capture thread and encoder a live share uses,
//! and pick the best setting. Local encode limit only: the network is not measured.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::RgbaImage;
use serde::{Deserialize, Serialize};

use crate::capture::{CAPTURE_REFRESH, CaptureCfg, CaptureThread, ScreenSource};
use crate::net::NetCmd;
use crate::profile::{SCREEN_FPS_CHOICES, screen_box};

/// Heights measured, best first (each is skipped when above the screen's height).
pub const PROBE_HEIGHTS: [u32; 4] = [1440, 1080, 720, 480];
/// Measuring time per height.
pub const PROBE_SPAN: Duration = Duration::from_millis(800);
/// Longest wait for the first frame at one height (encoder start-up).
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(3);
/// Fraction of the measured rate that counts as sustainable.
pub const SUSTAINABLE: f64 = 0.85;
/// The rate a height must sustain to be picked over a lower one.
pub const TARGET_FPS: f64 = 30.0;
/// Encoder label for the CPU path.
pub const CPU_ENCODER: &str = "CPU (JPEG)";

/// A "Test max" outcome, saved in `settings.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Resolution setting (height).
    pub height: u32,
    /// Frame rate setting (one of `SCREEN_FPS_CHOICES`).
    pub fps: u32,
    /// Encoder the measurement used, e.g. "CPU (JPEG)".
    pub encoder: String,
}

/// Choose a setting from `(height, measured fps)` pairs: the highest height whose
/// sustainable rate (85 % of measured) reaches 30 fps, at the largest offered rate
/// not above it. When none reaches 30, the height with the best offered rate (the
/// `SCREEN_FPS_CHOICES` value its sustainable rate allows; the higher height on a
/// tie), at that rate (>= 1).
pub fn pick(meas: &[(u32, f64)], encoder: &str) -> Option<ProbeResult> {
    let best_choice = |fps: f64| {
        SCREEN_FPS_CHOICES
            .iter()
            .copied()
            .filter(|c| *c as f64 <= fps)
            .max()
            .unwrap_or(1)
    };
    let mut sorted: Vec<(u32, f64)> = meas.to_vec();
    sorted.sort_by_key(|m| std::cmp::Reverse(m.0));
    // Rank by the rate that will actually be set. `sorted` is highest height
    // first, so on a tie the first (higher) one is kept.
    let best_rate = || {
        sorted
            .iter()
            .fold(None::<&(u32, f64)>, |best, m| match best {
                Some(b) if best_choice(b.1 * SUSTAINABLE) >= best_choice(m.1 * SUSTAINABLE) => {
                    Some(b)
                }
                _ => Some(m),
            })
    };
    let chosen = sorted
        .iter()
        .find(|(_, f)| f * SUSTAINABLE >= TARGET_FPS)
        .or_else(best_rate)?;
    Some(ProbeResult {
        height: chosen.0,
        fps: best_choice(chosen.1 * SUSTAINABLE),
        encoder: encoder.to_string(),
    })
}

/// Wraps a source and flips one pixel per grab, so the unchanged-frame skip never
/// fires and every pass is captured and encoded.
pub struct NoisyScreen<S: ScreenSource> {
    inner: S,
    flip: bool,
}

impl<S: ScreenSource> NoisyScreen<S> {
    pub fn new(inner: S) -> Self {
        Self { inner, flip: false }
    }
}

impl<S: ScreenSource> ScreenSource for NoisyScreen<S> {
    fn grab(&mut self) -> Result<RgbaImage, String> {
        let mut img = self.inner.grab()?;
        self.flip = !self.flip;
        if img.width() > 0 && img.height() > 0 {
            let p = img.get_pixel_mut(0, 0);
            p.0[0] ^= if self.flip { 1 } else { 2 };
        }
        Ok(img)
    }
}

impl ScreenSource for Box<dyn ScreenSource> {
    fn grab(&mut self) -> Result<RgbaImage, String> {
        (**self).grab()
    }
}

/// Height of a JPEG/PNG body (header read only); `None` for H.264.
fn image_height(body: &[u8]) -> Option<u32> {
    vc_proto::sniff(body)?;
    image::ImageReader::new(std::io::Cursor::new(body))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
        .map(|(_, h)| h)
}

/// One measurement: frames per second at `height`, `None` when the JPEG ladder had
/// to shrink most frames below the target (that height does not really fit), plus
/// whether H.264 was used.
fn measure(
    source: Box<dyn ScreenSource>,
    height: u32,
    (native_w, native_h): (u32, u32),
    quality: u8,
    video: bool,
    stop: &AtomicBool,
) -> Result<(Option<f64>, bool), String> {
    let (max_w, max_h) = screen_box(height);
    let cfg = Arc::new(Mutex::new(CaptureCfg {
        interval: Duration::ZERO,
        max_w,
        max_h,
        quality,
        active: true,
        epoch: 0,
        video,
    }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let t = CaptureThread::spawn(
        Box::new(NoisyScreen::new(source)),
        0,
        tx,
        None,
        cfg,
        CAPTURE_REFRESH,
    );
    // The height the encoder's own fit gives for this box (aspect kept, e.g. 535 for
    // a 3440x1440 screen in the 720p box), never more than the screen's height.
    let expect_h = crate::media::fitted_size(native_w, native_h, max_w, max_h)
        .1
        .min(native_h);
    let first_by = Instant::now() + FIRST_FRAME_WAIT;
    let mut started: Option<Instant> = None;
    let (mut frames, mut shrunk, mut h264) = (0u32, 0u32, false);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        if let Some(e) = t.take_error() {
            return Err(e);
        }
        let now = Instant::now();
        match started {
            Some(s) if now.duration_since(s) >= PROBE_SPAN => break,
            None if now >= first_by => return Err("no frame captured".into()),
            _ => {}
        }
        match rx.try_recv() {
            Ok(NetCmd::SendScreenFrame { jpeg, .. }) => {
                if started.is_none() {
                    // Encoder start-up is not part of the rate.
                    started = Some(Instant::now());
                    continue;
                }
                frames += 1;
                match image_height(&jpeg) {
                    Some(h) if h + 1 < expect_h => shrunk += 1,
                    Some(_) => {}
                    None => h264 = true,
                }
            }
            Ok(_) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    let elapsed = started.map_or(PROBE_SPAN, |s| s.elapsed()).as_secs_f64();
    drop(t); // stop and join: only one hardware session at a time
    if frames > 0 && shrunk * 2 > frames {
        return Ok((None, h264));
    }
    Ok((Some(frames as f64 / elapsed), h264))
}

/// Measure every height (`PROBE_HEIGHTS` up to the screen's own height, or the
/// lowest one for a small screen) and pick a setting.
pub fn run(
    make_source: &dyn Fn() -> Box<dyn ScreenSource>,
    quality: u8,
    video: bool,
    stop: &AtomicBool,
) -> Result<ProbeResult, String> {
    let (native_w, native_h) = make_source().grab()?.dimensions();
    let mut heights: Vec<u32> = PROBE_HEIGHTS
        .iter()
        .copied()
        .filter(|h| *h <= native_h)
        .collect();
    if heights.is_empty() {
        heights.push(*PROBE_HEIGHTS.last().expect("non-empty"));
    }
    let mut meas = Vec::new();
    let mut used_h264 = false;
    for h in heights {
        let (fps, h264) = measure(make_source(), h, (native_w, native_h), quality, video, stop)?;
        used_h264 |= h264;
        tracing::info!("test max: {h}p -> {fps:?} fps (h264 {h264})");
        if let Some(f) = fps {
            meas.push((h, f));
        }
    }
    let encoder = if used_h264 {
        crate::video::detect()
            .map(|e| format!("{} (hardware)", e.name))
            .unwrap_or_else(|| "Hardware H.264".into())
    } else {
        CPU_ENCODER.to_string()
    };
    pick(&meas, &encoder).ok_or_else(|| "no resolution could be measured".to_string())
}

/// A probe running on its own thread.
pub struct ProbeHandle {
    result: Arc<Mutex<Option<Result<ProbeResult, String>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProbeHandle {
    /// Start `run` in the background; `wake` is asked to repaint when it finishes.
    pub fn spawn(
        make_source: Box<dyn Fn() -> Box<dyn ScreenSource> + Send>,
        quality: u8,
        video: bool,
        wake: Option<egui::Context>,
    ) -> Self {
        let result = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s) = (result.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("vc-probe".into())
            .spawn(move || {
                let res = run(&*make_source, quality, video, &s);
                *r.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
                if let Some(ctx) = wake {
                    ctx.request_repaint();
                }
            })
            .expect("spawn probe thread");
        Self {
            result,
            stop,
            thread: Some(thread),
        }
    }

    /// The outcome, once (`None` while still running).
    pub fn take_result(&self) -> Option<Result<ProbeResult, String>> {
        self.result.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl Drop for ProbeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::FakeScreen;

    fn r(height: u32, fps: u32) -> Option<ProbeResult> {
        Some(ProbeResult {
            height,
            fps,
            encoder: CPU_ENCODER.into(),
        })
    }

    #[test]
    fn pick_table() {
        type Case<'a> = (&'a [(u32, f64)], Option<ProbeResult>);
        let cases: &[Case] = &[
            // 1080p sustains 21 (< 30), 720p sustains 34: 720p at 30.
            (&[(1080, 25.0), (720, 40.0)], r(720, 30)),
            // Highest height wins when it holds 30.
            (&[(1440, 80.0), (1080, 120.0), (720, 200.0)], r(1440, 60)),
            // Very fast: capped at the largest choice.
            (&[(1080, 500.0)], r(1080, 120)),
            // Nothing reaches 30: the best sustainable rate wins.
            (&[(1080, 10.0), (720, 20.0), (480, 25.0)], r(480, 20)),
            // Real RTX-box run: resize cost makes lower heights slower.
            (
                &[(1440, 22.5), (1080, 6.2), (720, 12.5), (480, 16.2)],
                r(1440, 15),
            ),
            // All equal: the highest height.
            (&[(480, 20.0), (1080, 20.0), (720, 20.0)], r(1080, 15)),
            // Same offered rate (20) from different raw rates: keep the higher height.
            (&[(1440, 26.0), (720, 28.0)], r(1440, 20)),
            // Order of the input does not matter.
            (&[(720, 40.0), (1080, 25.0)], r(720, 30)),
            // Under one frame per second still gives 1.
            (&[(480, 0.5)], r(480, 1)),
            (&[], None),
        ];
        for (meas, want) in cases {
            assert_eq!(&pick(meas, CPU_ENCODER), want, "{meas:?}");
        }
    }

    #[test]
    fn noisy_screen_always_changes() {
        let mut s = NoisyScreen::new(FakeScreen::default());
        let a = s.grab().unwrap();
        struct Still;
        impl ScreenSource for Still {
            fn grab(&mut self) -> Result<RgbaImage, String> {
                Ok(RgbaImage::from_pixel(8, 8, image::Rgba([9, 9, 9, 255])))
            }
        }
        let mut still = NoisyScreen::new(Still);
        let b1 = still.grab().unwrap();
        let b2 = still.grab().unwrap();
        assert_ne!(b1, b2, "a still screen still differs grab to grab");
        assert_eq!(a.dimensions(), (320, 180));
    }

    /// A changing gradient of any size (the probe adds its own per-grab noise).
    struct Sized(u32, u32);
    impl ScreenSource for Sized {
        fn grab(&mut self) -> Result<RgbaImage, String> {
            Ok(RgbaImage::from_fn(self.0, self.1, |x, y| {
                image::Rgba([(x / 8) as u8, (y / 8) as u8, 128, 255])
            }))
        }
    }

    fn probe_size(w: u32, h: u32) -> ProbeResult {
        let stop = AtomicBool::new(false);
        let make = move || Box::new(Sized(w, h)) as Box<dyn ScreenSource>;
        run(&make, 60, false, &stop).unwrap_or_else(|e| panic!("{w}x{h}: {e}"))
    }

    #[test]
    fn probe_ultrawide_and_portrait_screens_measure() {
        for (w, h) in [(3440, 1440), (1080, 1920), (1920, 1080)] {
            let res = probe_size(w, h);
            assert!(PROBE_HEIGHTS.contains(&res.height), "{w}x{h}: {res:?}");
            assert!(res.height <= h, "{w}x{h}: {res:?}");
            assert!(SCREEN_FPS_CHOICES.contains(&res.fps), "{w}x{h}: {res:?}");
        }
    }

    #[test]
    fn fake_screen_probe_returns_a_valid_choice() {
        let stop = AtomicBool::new(false);
        let make = || Box::new(FakeScreen::default()) as Box<dyn ScreenSource>;
        let res = run(&make, 60, false, &stop).unwrap();
        // 180p fake screen: only the lowest height is measured.
        assert_eq!(res.height, 480);
        assert!(SCREEN_FPS_CHOICES.contains(&res.fps), "{res:?}");
        assert_eq!(res.encoder, CPU_ENCODER);
    }

    #[test]
    fn probe_handle_reports_once() {
        let h = ProbeHandle::spawn(
            Box::new(|| Box::new(FakeScreen::default()) as Box<dyn ScreenSource>),
            60,
            false,
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = None;
        while got.is_none() && Instant::now() < deadline {
            got = h.take_result();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got.expect("probe finished").is_ok());
        assert!(h.take_result().is_none());
    }
}
