//! Screen capture for screen sharing: a `ScreenSource` grabs the primary monitor,
//! and `CaptureThread` turns it into a stream of frames (JPEG, or all-intra hardware
//! H.264 when `CaptureCfg::video` is set). Rate, size, quality and encoder come from
//! a live `CaptureCfg` handle, read on every iteration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use image::RgbaImage;
use tokio::sync::mpsc::UnboundedSender;

use crate::ServerKey;
use crate::media::{FRAME_MAX_H, FRAME_MAX_W, FRAME_QUALITY, ScreenEncoder};
use crate::net::NetCmd;

/// Default time between grabs (10 fps); the live value is `CaptureCfg::interval`.
pub const CAPTURE_INTERVAL: Duration = Duration::from_millis(100);
/// An unchanged screen is still resent this often, so late joiners get a frame.
pub const CAPTURE_REFRESH: Duration = Duration::from_secs(3);
/// Sleep between config checks while nobody watches.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// Longest single park while waiting for the next grab, so a faster rate applies soon.
const WAIT_SLICE: Duration = Duration::from_millis(100);
/// How often the encode ladder tries one rung better than the one that last fit.
const RUNG_RECOVER: Duration = Duration::from_secs(5);
/// Frames sent without the unchanged-screen check after a new watcher arrives.
const FORCE_FRAMES: u32 = 3;

/// Live capture settings, shared between the UI and the capture thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureCfg {
    /// Time between grabs.
    pub interval: Duration,
    /// Box each frame is fitted inside before encoding.
    pub max_w: u32,
    pub max_h: u32,
    /// JPEG quality before the encode ladder steps in.
    pub quality: u8,
    /// Someone watches: grab and send. When false the thread only idles.
    pub active: bool,
    /// Changes whenever a new watcher arrives; forces fresh frames out.
    pub epoch: u64,
    /// Encode with the hardware H.264 encoder (falls back to JPEG per frame).
    pub video: bool,
}

impl Default for CaptureCfg {
    fn default() -> Self {
        Self {
            interval: CAPTURE_INTERVAL,
            max_w: FRAME_MAX_W,
            max_h: FRAME_MAX_H,
            quality: FRAME_QUALITY,
            active: true,
            epoch: 0,
            video: false,
        }
    }
}

impl CaptureCfg {
    /// Equal apart from `active` and `epoch` (the encode settings).
    fn same_encoding(&self, o: &CaptureCfg) -> bool {
        (
            self.interval,
            self.max_w,
            self.max_h,
            self.quality,
            self.video,
        ) == (o.interval, o.max_w, o.max_h, o.quality, o.video)
    }

    /// Frames per second this config asks for (rounded; at least 1).
    pub fn fps(&self) -> u32 {
        let secs = self.interval.as_secs_f64();
        if secs <= 0.0 {
            vc_proto::SCREEN_FPS_LIMIT
        } else {
            ((1.0 / secs).round() as u32).max(1)
        }
    }
}

/// Shared handle the UI writes and the capture thread reads.
pub type CaptureHandle = Arc<Mutex<CaptureCfg>>;

fn read_cfg(cfg: &CaptureHandle) -> CaptureCfg {
    *cfg.lock().unwrap_or_else(|e| e.into_inner())
}

pub trait ScreenSource: Send {
    fn grab(&mut self) -> Result<RgbaImage, String>;
}

/// Swap GDI's BGRA to RGBA in place and force full opacity: 32 bpp `GetDIBits`
/// leaves the alpha byte at 0, which would draw a fully transparent frame.
pub fn bgra_to_rgba(px: &mut [u8]) {
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
}

/// Whether this build can send its screen. Only Windows has a capture backend
/// (GDI); elsewhere sharing is hidden in the UI and the client only watches.
pub const SUPPORTED: bool = cfg!(windows);

/// The primary monitor, captured with GDI.
#[derive(Debug, Default)]
pub struct GdiScreen;

#[cfg(windows)]
mod gdi {
    use std::ptr::null_mut;

    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
        SRCCOPY, SelectObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    /// Owns every GDI object of one grab; `Drop` releases them in reverse order on
    /// every path, including early returns.
    struct Guard {
        screen: HDC,
        mem: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: each handle was obtained in `grab` below and is released exactly
            // once here; null handles (failed creation) are skipped.
            unsafe {
                if !self.mem.is_null() && !self.old.is_null() {
                    SelectObject(self.mem, self.old);
                }
                if !self.bitmap.is_null() {
                    DeleteObject(self.bitmap);
                }
                if !self.mem.is_null() {
                    DeleteDC(self.mem);
                }
                if !self.screen.is_null() {
                    ReleaseDC(null_mut(), self.screen);
                }
            }
        }
    }

    pub fn grab() -> Result<image::RgbaImage, String> {
        // SAFETY: plain Win32 calls; every handle is checked for null before use and
        // owned by `Guard`, and the pixel buffer is sized for `w * h` 32-bit pixels,
        // matching the BITMAPINFO passed to GetDIBits.
        unsafe {
            let w = GetSystemMetrics(SM_CXSCREEN);
            let h = GetSystemMetrics(SM_CYSCREEN);
            if w <= 0 || h <= 0 {
                return Err("no primary monitor".into());
            }
            let mut g = Guard {
                screen: GetDC(null_mut()),
                mem: null_mut(),
                bitmap: null_mut(),
                old: null_mut(),
            };
            if g.screen.is_null() {
                return Err("GetDC failed".into());
            }
            g.mem = CreateCompatibleDC(g.screen);
            if g.mem.is_null() {
                return Err("CreateCompatibleDC failed".into());
            }
            g.bitmap = CreateCompatibleBitmap(g.screen, w, h);
            if g.bitmap.is_null() {
                return Err("CreateCompatibleBitmap failed".into());
            }
            g.old = SelectObject(g.mem, g.bitmap);
            if g.old.is_null() {
                return Err("SelectObject failed".into());
            }
            if BitBlt(g.mem, 0, 0, w, h, g.screen, 0, 0, SRCCOPY) == 0 {
                return Err("BitBlt failed".into());
            }
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // negative: top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u8; w as usize * h as usize * 4];
            // The bitmap must not be selected into a DC while GetDIBits reads it.
            SelectObject(g.mem, g.old);
            g.old = null_mut();
            let lines = GetDIBits(
                g.mem,
                g.bitmap,
                0,
                h as u32,
                px.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );
            if lines != h {
                return Err("GetDIBits failed".into());
            }
            super::bgra_to_rgba(&mut px);
            image::RgbaImage::from_raw(w as u32, h as u32, px).ok_or_else(|| "bad size".into())
        }
    }
}

impl ScreenSource for GdiScreen {
    #[cfg(windows)]
    fn grab(&mut self) -> Result<RgbaImage, String> {
        gdi::grab()
    }

    #[cfg(not(windows))]
    fn grab(&mut self) -> Result<RgbaImage, String> {
        Err("not supported".into())
    }
}

/// Test source: a 320×180 gradient that shifts on each grab.
#[derive(Debug, Default)]
pub struct FakeScreen {
    step: u32,
}

impl ScreenSource for FakeScreen {
    fn grab(&mut self) -> Result<RgbaImage, String> {
        let s = self.step;
        self.step = self.step.wrapping_add(8);
        Ok(RgbaImage::from_fn(320, 180, |x, y| {
            image::Rgba([(x + s) as u8, (y + s) as u8, 160, 255])
        }))
    }
}

/// Same size and identical pixels (a plain memcmp, fast at any opt level).
fn same_pixels(a: &RgbaImage, b: &RgbaImage) -> bool {
    a.dimensions() == b.dimensions() && a.as_raw() == b.as_raw()
}

/// Background thread grabbing and sending frames until dropped.
pub struct CaptureThread {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CaptureThread {
    /// Start capturing with the settings in `cfg` (read live on every iteration),
    /// resending an unchanged screen every 3 s. `wake` is asked to repaint the UI
    /// when the thread stops on an error.
    pub fn start(
        source: Box<dyn ScreenSource>,
        key: ServerKey,
        tx: UnboundedSender<NetCmd>,
        wake: Option<egui::Context>,
        cfg: CaptureHandle,
    ) -> Self {
        Self::spawn(source, key, tx, wake, cfg, CAPTURE_REFRESH)
    }

    /// Fixed timing, always active, default encoding (tests).
    pub fn with_timing(
        source: Box<dyn ScreenSource>,
        key: ServerKey,
        tx: UnboundedSender<NetCmd>,
        wake: Option<egui::Context>,
        interval: Duration,
        refresh: Duration,
    ) -> Self {
        let cfg = Arc::new(Mutex::new(CaptureCfg {
            interval,
            ..CaptureCfg::default()
        }));
        Self::spawn(source, key, tx, wake, cfg, refresh)
    }

    pub(crate) fn spawn(
        mut source: Box<dyn ScreenSource>,
        key: ServerKey,
        tx: UnboundedSender<NetCmd>,
        wake: Option<egui::Context>,
        cfg: CaptureHandle,
        refresh: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let (t_stop, t_error) = (stop.clone(), error.clone());
        let thread = std::thread::Builder::new()
            .name("vc-capture".into())
            .spawn(move || {
                let mut last: Option<(RgbaImage, Instant)> = None;
                let mut seen: Option<CaptureCfg> = None;
                let mut force = 0u32;
                let mut rung = 0usize;
                let mut rung_at = Instant::now();
                // Lives (and drops) on this thread: the hardware encoder is a COM object.
                let mut encoder = ScreenEncoder::default();
                while !t_stop.load(Ordering::Acquire) {
                    let started = Instant::now();
                    let c = read_cfg(&cfg);
                    if !c.active {
                        last = None;
                        std::thread::park_timeout(IDLE_POLL);
                        continue;
                    }
                    if let Some(prev) = seen {
                        if prev.epoch != c.epoch {
                            force = FORCE_FRAMES;
                        }
                        if !prev.same_encoding(&c) {
                            rung = 0;
                        }
                    }
                    seen = Some(c);
                    if started.duration_since(rung_at) >= RUNG_RECOVER {
                        rung = rung.saturating_sub(1);
                        rung_at = started;
                    }
                    let refresh = refresh.max(c.interval);
                    let res = source.grab().and_then(|img| {
                        let fresh = force == 0
                            && last.as_ref().is_some_and(|(prev, at)| {
                                started.duration_since(*at) < refresh && same_pixels(prev, &img)
                            });
                        if fresh {
                            return Ok(());
                        }
                        let (jpeg, used) =
                            encoder.encode(&img, &c, rung).map_err(|e| e.to_string())?;
                        rung = used;
                        force = force.saturating_sub(1);
                        let _ = tx.send(NetCmd::SendScreenFrame { key, jpeg });
                        // Keep the frame (moved, not copied) for the next change check.
                        last = Some((img, started));
                        Ok(())
                    });
                    if let Err(e) = res {
                        tracing::warn!("screen capture failed: {e}");
                        *t_error.lock().expect("poisoned") = Some(e);
                        if let Some(ctx) = &wake {
                            ctx.request_repaint();
                        }
                        return;
                    }
                    loop {
                        if t_stop.load(Ordering::Acquire) {
                            return;
                        }
                        // Re-read the rate so a change applies without waiting out
                        // a long old interval.
                        let deadline = started + read_cfg(&cfg).interval;
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let remaining = deadline - now;
                        if remaining < WAIT_SLICE {
                            // A plain sleep is precise enough for 120 fps; parking can
                            // wake early or late by far more than 8 ms.
                            std::thread::sleep(remaining);
                        } else {
                            std::thread::park_timeout(WAIT_SLICE);
                        }
                    }
                }
            })
            .expect("spawn capture thread");
        Self {
            stop,
            error,
            thread: Some(thread),
        }
    }

    /// The error that stopped the thread, once.
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().expect("poisoned").take()
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_swaps_and_sets_alpha() {
        let mut px = vec![1, 2, 3, 0, 10, 20, 30, 0];
        bgra_to_rgba(&mut px);
        assert_eq!(px, vec![3, 2, 1, 255, 30, 20, 10, 255]);
    }

    fn frames(rx: &mut tokio::sync::mpsc::UnboundedReceiver<NetCmd>) -> usize {
        let mut n = 0;
        while let Ok(c) = rx.try_recv() {
            match c {
                NetCmd::SendScreenFrame { key, jpeg } => {
                    assert_eq!(key, 7);
                    assert_eq!(vc_proto::sniff(&jpeg), Some(vc_proto::ImageKind::Jpeg));
                    n += 1;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        n
    }

    #[test]
    fn fake_screen_emits_frames() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = CaptureThread::with_timing(
            Box::new(FakeScreen::default()),
            7,
            tx,
            None,
            Duration::from_millis(10),
            Duration::from_secs(60),
        );
        std::thread::sleep(Duration::from_millis(300));
        drop(t);
        assert!(frames(&mut rx) >= 3);
    }

    struct Still;
    impl ScreenSource for Still {
        fn grab(&mut self) -> Result<RgbaImage, String> {
            Ok(RgbaImage::from_pixel(64, 36, image::Rgba([9, 9, 9, 255])))
        }
    }

    #[test]
    fn unchanged_screen_skipped_then_refreshed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = CaptureThread::with_timing(
            Box::new(Still),
            7,
            tx,
            None,
            Duration::from_millis(10),
            Duration::from_millis(200),
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(frames(&mut rx), 1, "unchanged frames are skipped");
        std::thread::sleep(Duration::from_millis(250));
        assert!(frames(&mut rx) >= 1, "resent after the refresh interval");
        drop(t);
    }

    /// A still screen whose pixels change once `flip` is set.
    struct Flipping(Arc<AtomicBool>);
    impl ScreenSource for Flipping {
        fn grab(&mut self) -> Result<RgbaImage, String> {
            let v = if self.0.load(Ordering::Relaxed) {
                200
            } else {
                9
            };
            Ok(RgbaImage::from_pixel(64, 36, image::Rgba([v, v, v, 255])))
        }
    }

    #[test]
    fn static_screen_sends_once_then_change_sends_immediately() {
        let flip = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let t = CaptureThread::with_timing(
            Box::new(Flipping(flip.clone())),
            7,
            tx,
            None,
            Duration::from_millis(10),
            Duration::from_secs(60),
        );
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(&mut rx), 1, "static screen: one frame, then nothing");
        flip.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(&mut rx), 1, "a change is sent at once, once");
        drop(t);
    }

    struct Broken;
    impl ScreenSource for Broken {
        fn grab(&mut self) -> Result<RgbaImage, String> {
            Err("boom".into())
        }
    }

    #[test]
    fn error_stops_thread_and_is_reported_once() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let t = CaptureThread::start(
            Box::new(Broken),
            1,
            tx,
            None,
            Arc::new(Mutex::new(CaptureCfg::default())),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = None;
        while got.is_none() && Instant::now() < deadline {
            got = t.take_error();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got.as_deref(), Some("boom"));
        assert_eq!(t.take_error(), None);
    }

    #[test]
    fn drop_joins_promptly() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let t = CaptureThread::with_timing(
            Box::new(FakeScreen::default()),
            1,
            tx,
            None,
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(t);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drop must unpark"
        );
    }

    type Rx = tokio::sync::mpsc::UnboundedReceiver<NetCmd>;

    fn handle(interval_ms: u64, active: bool) -> CaptureHandle {
        Arc::new(Mutex::new(CaptureCfg {
            interval: Duration::from_millis(interval_ms),
            active,
            ..CaptureCfg::default()
        }))
    }

    fn start_with(source: Box<dyn ScreenSource>, h: &CaptureHandle) -> (CaptureThread, Rx) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            CaptureThread::spawn(source, 7, tx, None, h.clone(), Duration::from_secs(60)),
            rx,
        )
    }

    /// Counts grabs, so a test can see the thread doing no work.
    struct Counting(Arc<std::sync::atomic::AtomicUsize>);
    impl ScreenSource for Counting {
        fn grab(&mut self) -> Result<RgbaImage, String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Still.grab()
        }
    }

    #[test]
    fn inactive_does_not_grab_or_send() {
        let grabs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = handle(10, false);
        let (t, mut rx) = start_with(Box::new(Counting(grabs.clone())), &h);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(grabs.load(Ordering::Relaxed), 0, "no grab while inactive");
        assert_eq!(frames(&mut rx), 0);
        h.lock().unwrap().active = true;
        std::thread::sleep(Duration::from_millis(300));
        assert!(frames(&mut rx) >= 1, "becoming active sends a frame");
        drop(t);
    }

    #[test]
    fn epoch_bump_forces_frames_on_still_screen() {
        let h = handle(10, true);
        let (t, mut rx) = start_with(Box::new(Still), &h);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(&mut rx), 1, "still screen sent once");
        h.lock().unwrap().epoch += 1;
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            frames(&mut rx),
            FORCE_FRAMES as usize,
            "new watcher forces frames"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(frames(&mut rx), 0, "then back to skipping");
        drop(t);
    }

    #[test]
    fn pacing_reaches_120_fps() {
        let h = Arc::new(Mutex::new(CaptureCfg {
            interval: crate::profile::screen_interval(120),
            ..CaptureCfg::default()
        }));
        let (t, mut rx) = start_with(Box::new(FakeScreen::default()), &h);
        std::thread::sleep(Duration::from_millis(1100));
        drop(t);
        let n = frames(&mut rx);
        assert!(n >= 90, "only {n} frames in ~1 s at 120 fps");
    }

    #[test]
    fn cfg_fps_from_interval() {
        let mut c = CaptureCfg::default();
        assert_eq!(c.fps(), 10);
        c.interval = crate::profile::screen_interval(120);
        assert_eq!(c.fps(), 120);
        c.interval = Duration::ZERO;
        assert_eq!(c.fps(), 120);
    }

    #[test]
    fn interval_change_is_picked_up_live() {
        let h = handle(5_000, true);
        let (t, mut rx) = start_with(Box::new(FakeScreen::default()), &h);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(frames(&mut rx), 1, "slow: one frame");
        h.lock().unwrap().interval = Duration::from_millis(10);
        std::thread::sleep(Duration::from_millis(600));
        assert!(frames(&mut rx) >= 5, "fast right after the change");
        drop(t);
    }

    #[test]
    #[ignore = "timing run: cargo test -p vc-client --lib capture_pass_timing -- --ignored --nocapture"]
    fn capture_pass_timing() {
        let (w, h) = (2560u32, 1440u32);
        let mut x = 0x9E37_79B9u32;
        let raw: Vec<u8> = (0..w * h * 4)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let mut px = raw.clone();
        let t = Instant::now();
        bgra_to_rgba(&mut px);
        let swizzle = t.elapsed();
        let img = RgbaImage::from_raw(w, h, px).unwrap();
        // Worst case for the check: identical frames, every byte compared.
        let prev = img.clone();
        let t = Instant::now();
        let same = same_pixels(&prev, &img);
        let check = t.elapsed();
        let t = Instant::now();
        let (jpeg, rung) = crate::media::encode_screen_frame(&img, 1280, 720, 60, 0).unwrap();
        let encode = t.elapsed();
        println!(
            "TIMING swizzle={swizzle:?} change_check={check:?} (same={same})              encode_720p_q60={encode:?} ({} bytes, rung {rung})",
            jpeg.len()
        );
        // Warm the per-thread resizer once, then time each target box.
        let _ = crate::media::downscale(&img, 1920, 1080);
        for (bw, bh) in [(1920u32, 1080u32), (1280, 720)] {
            let t = Instant::now();
            let small = crate::media::downscale(&img, bw, bh);
            println!(
                "TIMING downscale_1440p_to_{}p={:?}",
                small.height(),
                t.elapsed()
            );
        }
    }
}
