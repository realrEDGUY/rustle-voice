//! Audio device seam: real cpal devices, a fake (sine/silence) for tests, a null
//! backend, and an external one fed by a host app (iOS AVAudioEngine).
//!
//! Callbacks always deal in 48 kHz mono f32; the cpal adapter converts formats,
//! channel counts and sample rates.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use vc_proto::SAMPLE_RATE;

use crate::dsp::resample::{Resampler, downmix_to_mono};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable id (cpal `DeviceId` string) persisted in settings.
    pub id: String,
    /// Human name shown in the UI.
    pub name: String,
}

pub type CaptureCb = Box<dyn FnMut(&[f32]) + Send + 'static>;
pub type PlaybackCb = Box<dyn FnMut(&mut [f32]) + Send + 'static>;

pub trait AudioBackend: Send {
    fn inputs(&self) -> Vec<DeviceInfo>;
    fn outputs(&self) -> Vec<DeviceInfo>;
    /// Start capture. The returned box keeps the stream alive.
    fn start_capture(&mut self, id: Option<&str>, cb: CaptureCb) -> Result<Box<dyn Any>>;
    /// Start playback. The returned box keeps the stream alive.
    fn start_playback(&mut self, id: Option<&str>, cb: PlaybackCb) -> Result<Box<dyn Any>>;
    /// Start capturing system audio ("stream sound"), excluding this app's own
    /// output. The returned box keeps the capture alive.
    fn start_loopback(&mut self, cb: CaptureCb) -> Result<Box<dyn Any>> {
        drop(cb);
        Err(anyhow!("stream sound not supported"))
    }
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------- null

/// No devices; starting streams fails.
#[derive(Debug, Default)]
pub struct NullBackend;

impl AudioBackend for NullBackend {
    fn inputs(&self) -> Vec<DeviceInfo> {
        vec![]
    }
    fn outputs(&self) -> Vec<DeviceInfo> {
        vec![]
    }
    fn start_capture(&mut self, _: Option<&str>, _: CaptureCb) -> Result<Box<dyn Any>> {
        Err(anyhow!("no audio input device"))
    }
    fn start_playback(&mut self, _: Option<&str>, _: PlaybackCb) -> Result<Box<dyn Any>> {
        Err(anyhow!("no audio output device"))
    }
    fn name(&self) -> &'static str {
        "null"
    }
}

// ---------------------------------------------------------------- fake

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FakeMode {
    Sine(f32),
    Silent,
}

/// The fake system-audio loopback always plays this tone (distinct from mic sines).
pub const FAKE_LOOPBACK_HZ: f32 = 1500.0;

#[derive(Default)]
struct FakeShared {
    capture: Option<CaptureCb>,
    playback: Option<PlaybackCb>,
    loopback: Option<CaptureCb>,
    phase: u64,
    played: Vec<f32>,
    selected_in: Option<String>,
    selected_out: Option<String>,
}

/// Deterministic fake devices. Drive it with [`FakeHandle::drive`] in tests,
/// or let it run in real time with [`FakeBackend::realtime`].
pub struct FakeBackend {
    mode: FakeMode,
    shared: Arc<Mutex<FakeShared>>,
    realtime: bool,
    stop: Arc<AtomicBool>,
}

/// Test handle onto a fake backend.
#[derive(Clone)]
pub struct FakeHandle {
    mode: FakeMode,
    shared: Arc<Mutex<FakeShared>>,
}

impl FakeHandle {
    /// Feed `n` captured samples to the capture callback and pull `n` samples from playback.
    pub fn drive(&self, n: usize) {
        let mut s = self.shared.lock().expect("poisoned");
        let start = s.phase;
        s.phase += n as u64;
        let input: Vec<f32> = (0..n)
            .map(|i| match self.mode {
                FakeMode::Sine(hz) => {
                    let t = (start + i as u64) as f64 / SAMPLE_RATE as f64;
                    (0.5 * (2.0 * std::f64::consts::PI * hz as f64 * t).sin()) as f32
                }
                FakeMode::Silent => 0.0,
            })
            .collect();
        if let Some(cb) = s.capture.as_mut() {
            cb(&input);
        }
        if s.loopback.is_some() {
            let tone: Vec<f32> = (0..n)
                .map(|i| {
                    let t = (start + i as u64) as f64 / SAMPLE_RATE as f64;
                    (0.4 * (2.0 * std::f64::consts::PI * FAKE_LOOPBACK_HZ as f64 * t).sin()) as f32
                })
                .collect();
            if let Some(cb) = s.loopback.as_mut() {
                cb(&tone);
            }
        }
        let mut out = vec![0f32; n];
        if let Some(cb) = s.playback.as_mut() {
            cb(&mut out);
            s.played.extend_from_slice(&out);
        }
    }

    /// Everything played so far.
    pub fn played(&self) -> Vec<f32> {
        self.shared.lock().expect("poisoned").played.clone()
    }

    pub fn clear_played(&self) {
        self.shared.lock().expect("poisoned").played.clear();
    }

    pub fn capture_running(&self) -> bool {
        self.shared.lock().expect("poisoned").capture.is_some()
    }

    pub fn playback_running(&self) -> bool {
        self.shared.lock().expect("poisoned").playback.is_some()
    }

    pub fn loopback_running(&self) -> bool {
        self.shared.lock().expect("poisoned").loopback.is_some()
    }

    pub fn selected_devices(&self) -> (Option<String>, Option<String>) {
        let s = self.shared.lock().expect("poisoned");
        (s.selected_in.clone(), s.selected_out.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FakeKind {
    Capture,
    Playback,
    Loopback,
}

struct FakeStream {
    shared: Arc<Mutex<FakeShared>>,
    kind: FakeKind,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        if let Ok(mut s) = self.shared.lock() {
            match self.kind {
                FakeKind::Capture => s.capture = None,
                FakeKind::Playback => s.playback = None,
                FakeKind::Loopback => s.loopback = None,
            }
        }
    }
}

impl FakeBackend {
    pub fn new(mode: FakeMode) -> Self {
        Self {
            mode,
            shared: Arc::new(Mutex::new(FakeShared::default())),
            realtime: false,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Drive the fake devices from a background thread every 20 ms.
    pub fn realtime(mut self) -> Self {
        self.realtime = true;
        let h = self.handle();
        let stop = self.stop.clone();
        std::thread::Builder::new()
            .name("vc-fake-audio".into())
            .spawn(move || {
                let mut next = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    h.drive(960);
                    // Do not keep an ever-growing played buffer in real-time mode.
                    h.clear_played();
                    next += Duration::from_millis(20);
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            })
            .expect("spawn fake audio");
        self
    }

    pub fn handle(&self) -> FakeHandle {
        FakeHandle {
            mode: self.mode,
            shared: self.shared.clone(),
        }
    }

    fn devices(prefix: &str) -> Vec<DeviceInfo> {
        (1..=2)
            .map(|i| DeviceInfo {
                id: format!("fake-{prefix}-{i}"),
                name: format!("Fake {prefix} {i}"),
            })
            .collect()
    }
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl AudioBackend for FakeBackend {
    fn inputs(&self) -> Vec<DeviceInfo> {
        Self::devices("input")
    }
    fn outputs(&self) -> Vec<DeviceInfo> {
        Self::devices("output")
    }
    fn start_capture(&mut self, id: Option<&str>, cb: CaptureCb) -> Result<Box<dyn Any>> {
        let mut s = self.shared.lock().expect("poisoned");
        s.capture = Some(cb);
        s.selected_in = id.map(str::to_string);
        Ok(Box::new(FakeStream {
            shared: self.shared.clone(),
            kind: FakeKind::Capture,
        }))
    }
    fn start_playback(&mut self, id: Option<&str>, cb: PlaybackCb) -> Result<Box<dyn Any>> {
        let mut s = self.shared.lock().expect("poisoned");
        s.playback = Some(cb);
        s.selected_out = id.map(str::to_string);
        Ok(Box::new(FakeStream {
            shared: self.shared.clone(),
            kind: FakeKind::Playback,
        }))
    }
    fn start_loopback(&mut self, cb: CaptureCb) -> Result<Box<dyn Any>> {
        self.shared.lock().expect("poisoned").loopback = Some(cb);
        Ok(Box::new(FakeStream {
            shared: self.shared.clone(),
            kind: FakeKind::Loopback,
        }))
    }
    fn name(&self) -> &'static str {
        if self.realtime {
            "fake (real time)"
        } else {
            "fake"
        }
    }
}

// ---------------------------------------------------------------- external

/// Audio driven from outside Rust (the iOS app's AVAudioEngine). Capture and
/// playback only store their callbacks; the host's real-time threads push mono
/// capture and pull mono playback through an [`ExternalAudio`] handle, at their
/// own sample rate.
pub struct ExternalBackend {
    shared: Arc<ExternalShared>,
}

/// The host's side of an [`ExternalBackend`]: cheap to clone, safe to use from any
/// thread. Calls never block: a busy or empty slot drops capture and plays zeros.
#[derive(Clone)]
pub struct ExternalAudio(Arc<ExternalShared>);

#[derive(Default)]
struct ExternalShared {
    /// The capture adapter and the host rate it was built for.
    capture: Mutex<Option<(u32, CaptureAdapter)>>,
    /// The playback adapter and the host rate it was built for.
    playback: Mutex<Option<(u32, PlaybackAdapter)>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExternalKind {
    Capture,
    Playback,
}

/// Keeps one external stream "open"; dropping it empties the slot.
struct ExternalStream {
    shared: Arc<ExternalShared>,
    kind: ExternalKind,
}

impl Drop for ExternalStream {
    fn drop(&mut self) {
        match self.kind {
            ExternalKind::Capture => {
                *self
                    .shared
                    .capture
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
            }
            ExternalKind::Playback => {
                *self
                    .shared
                    .playback
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
    }
}

impl ExternalBackend {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(ExternalShared::default()),
        }
    }

    /// The handle the host's audio threads use.
    pub fn handle(&self) -> ExternalAudio {
        ExternalAudio(self.shared.clone())
    }
}

impl Default for ExternalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for ExternalBackend {
    fn inputs(&self) -> Vec<DeviceInfo> {
        vec![]
    }
    fn outputs(&self) -> Vec<DeviceInfo> {
        vec![]
    }
    fn start_capture(&mut self, _: Option<&str>, cb: CaptureCb) -> Result<Box<dyn Any>> {
        let adapter = CaptureAdapter {
            cb,
            channels: 1,
            resampler: Resampler::new(SAMPLE_RATE, SAMPLE_RATE),
            buf: Vec::new(),
        };
        *self
            .shared
            .capture
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((SAMPLE_RATE, adapter));
        Ok(Box::new(ExternalStream {
            shared: self.shared.clone(),
            kind: ExternalKind::Capture,
        }))
    }
    fn start_playback(&mut self, _: Option<&str>, cb: PlaybackCb) -> Result<Box<dyn Any>> {
        let adapter = PlaybackAdapter {
            cb,
            channels: 1,
            resampler: Resampler::new(SAMPLE_RATE, SAMPLE_RATE),
            pending: VecDeque::new(),
            src: Vec::new(),
            tmp: Vec::new(),
        };
        *self
            .shared
            .playback
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((SAMPLE_RATE, adapter));
        Ok(Box::new(ExternalStream {
            shared: self.shared.clone(),
            kind: ExternalKind::Playback,
        }))
    }
    fn name(&self) -> &'static str {
        "external"
    }
}

impl ExternalAudio {
    /// Feed captured mono samples at `rate` Hz. Dropped when capture is not
    /// running or the slot is busy.
    pub fn push_capture(&self, mono: &[f32], rate: u32) {
        if rate == 0 {
            return;
        }
        let Ok(mut slot) = self.0.capture.try_lock() else {
            return;
        };
        let Some((r, adapter)) = slot.as_mut() else {
            return;
        };
        if *r != rate {
            *r = rate;
            adapter.resampler = Resampler::new(rate, SAMPLE_RATE);
        }
        adapter.push(mono);
    }

    /// Fill `out` with mono playback at `rate` Hz. Zeros when playback is not
    /// running or the slot is busy.
    pub fn pull_playback(&self, out: &mut [f32], rate: u32) {
        let filled = rate != 0
            && match self.0.playback.try_lock() {
                Ok(mut slot) => match slot.as_mut() {
                    Some((r, adapter)) => {
                        if *r != rate {
                            *r = rate;
                            adapter.resampler = Resampler::new(SAMPLE_RATE, rate);
                            adapter.pending.clear();
                        }
                        let mono = adapter.pull(out.len());
                        for (o, v) in out.iter_mut().zip(mono) {
                            *o = v.clamp(-1.0, 1.0);
                        }
                        true
                    }
                    None => false,
                },
                Err(_) => false,
            };
        if !filled {
            out.iter_mut().for_each(|o| *o = 0.0);
        }
    }

    /// Is a capture stream open (tests)?
    pub fn capture_running(&self) -> bool {
        self.0
            .capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Is a playback stream open (tests)?
    pub fn playback_running(&self) -> bool {
        self.0
            .playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

// ---------------------------------------------------------------- cpal

#[cfg(not(target_os = "ios"))]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Real devices via cpal's default host (WASAPI on Windows).
#[cfg(not(target_os = "ios"))]
#[derive(Debug, Default)]
pub struct CpalBackend;

#[cfg(not(target_os = "ios"))]
fn device_info(d: &cpal::Device) -> Option<DeviceInfo> {
    let id = d.id().ok()?.to_string();
    let name = d
        .description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| id.clone());
    Some(DeviceInfo { id, name })
}

#[cfg(not(target_os = "ios"))]
impl CpalBackend {
    pub fn new() -> Self {
        Self
    }

    fn find(&self, id: Option<&str>, input: bool) -> Result<cpal::Device> {
        let host = cpal::default_host();
        if let Some(id) = id
            && let Ok(did) = id.parse::<cpal::DeviceId>()
            && let Some(d) = host.device_by_id(&did)
        {
            return Ok(d);
        }
        let d = if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        };
        d.ok_or_else(|| {
            anyhow!(
                "no default {} device",
                if input { "input" } else { "output" }
            )
        })
    }

    /// Default device ids (for tests).
    pub fn default_ids(&self) -> (Option<String>, Option<String>) {
        let host = cpal::default_host();
        (
            host.default_input_device()
                .and_then(|d| device_info(&d))
                .map(|i| i.id),
            host.default_output_device()
                .and_then(|d| device_info(&d))
                .map(|i| i.id),
        )
    }
}

struct CaptureAdapter {
    cb: CaptureCb,
    channels: usize,
    resampler: Resampler,
    buf: Vec<f32>,
}

impl CaptureAdapter {
    fn push(&mut self, interleaved: &[f32]) {
        let mono = downmix_to_mono(interleaved, self.channels);
        self.buf.clear();
        self.resampler.process(&mono, &mut self.buf);
        (self.cb)(&self.buf);
    }
}

struct PlaybackAdapter {
    cb: PlaybackCb,
    /// Device channels (cpal interleaves; external playback is always mono).
    #[cfg_attr(target_os = "ios", allow(dead_code))]
    channels: usize,
    resampler: Resampler,
    pending: VecDeque<f32>,
    src: Vec<f32>,
    tmp: Vec<f32>,
}

impl PlaybackAdapter {
    /// Fill `frames` device-rate mono samples, then the caller interleaves.
    fn pull(&mut self, frames: usize) -> Vec<f32> {
        while self.pending.len() < frames {
            self.src.resize(480, 0.0);
            self.src.iter_mut().for_each(|s| *s = 0.0);
            (self.cb)(&mut self.src);
            self.tmp.clear();
            self.resampler.process(&self.src, &mut self.tmp);
            self.pending.extend(self.tmp.iter().copied());
        }
        self.pending.drain(..frames).collect()
    }
}

#[cfg(not(target_os = "ios"))]
fn build_input<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    mut adapter: CaptureAdapter,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let mut conv: Vec<f32> = Vec::new();
    let s = dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], _| {
            conv.clear();
            conv.extend(
                data.iter()
                    .map(|s| <f32 as cpal::FromSample<T>>::from_sample_(*s)),
            );
            adapter.push(&conv);
        },
        |e| tracing::warn!("input stream error: {e}"),
        Some(Duration::from_secs(3)),
    )?;
    Ok(s)
}

#[cfg(not(target_os = "ios"))]
fn build_output<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    mut adapter: PlaybackAdapter,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static,
{
    let s = dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            let ch = adapter.channels.max(1);
            let frames = data.len() / ch;
            let mono = adapter.pull(frames);
            for (frame, m) in data.chunks_exact_mut(ch).zip(mono) {
                let v = <T as cpal::FromSample<f32>>::from_sample_(m.clamp(-1.0, 1.0));
                frame.iter_mut().for_each(|o| *o = v);
            }
        },
        |e| tracing::warn!("output stream error: {e}"),
        Some(Duration::from_secs(3)),
    )?;
    Ok(s)
}

#[cfg(not(target_os = "ios"))]
impl AudioBackend for CpalBackend {
    fn inputs(&self) -> Vec<DeviceInfo> {
        cpal::default_host()
            .input_devices()
            .map(|it| it.filter_map(|d| device_info(&d)).collect())
            .unwrap_or_default()
    }

    fn outputs(&self) -> Vec<DeviceInfo> {
        cpal::default_host()
            .output_devices()
            .map(|it| it.filter_map(|d| device_info(&d)).collect())
            .unwrap_or_default()
    }

    fn start_capture(&mut self, id: Option<&str>, cb: CaptureCb) -> Result<Box<dyn Any>> {
        let dev = self.find(id, true)?;
        let sup = dev.default_input_config()?;
        let cfg = sup.config();
        let adapter = CaptureAdapter {
            cb,
            channels: cfg.channels as usize,
            resampler: Resampler::new(cfg.sample_rate, SAMPLE_RATE),
            buf: Vec::new(),
        };
        let stream = match sup.sample_format() {
            cpal::SampleFormat::F32 => build_input::<f32>(&dev, cfg, adapter)?,
            cpal::SampleFormat::I16 => build_input::<i16>(&dev, cfg, adapter)?,
            cpal::SampleFormat::U16 => build_input::<u16>(&dev, cfg, adapter)?,
            cpal::SampleFormat::I32 => build_input::<i32>(&dev, cfg, adapter)?,
            other => return Err(anyhow!("unsupported input sample format {other:?}")),
        };
        stream.play()?;
        Ok(Box::new(stream))
    }

    fn start_playback(&mut self, id: Option<&str>, cb: PlaybackCb) -> Result<Box<dyn Any>> {
        let dev = self.find(id, false)?;
        let sup = dev.default_output_config()?;
        let cfg = sup.config();
        let adapter = PlaybackAdapter {
            cb,
            channels: cfg.channels as usize,
            resampler: Resampler::new(SAMPLE_RATE, cfg.sample_rate),
            pending: VecDeque::new(),
            src: Vec::new(),
            tmp: Vec::new(),
        };
        let stream = match sup.sample_format() {
            cpal::SampleFormat::F32 => build_output::<f32>(&dev, cfg, adapter)?,
            cpal::SampleFormat::I16 => build_output::<i16>(&dev, cfg, adapter)?,
            cpal::SampleFormat::U16 => build_output::<u16>(&dev, cfg, adapter)?,
            cpal::SampleFormat::I32 => build_output::<i32>(&dev, cfg, adapter)?,
            other => return Err(anyhow!("unsupported output sample format {other:?}")),
        };
        stream.play()?;
        Ok(Box::new(stream))
    }

    #[cfg(windows)]
    fn start_loopback(&mut self, cb: CaptureCb) -> Result<Box<dyn Any>> {
        Ok(Box::new(crate::loopback::start(cb)?))
    }

    fn name(&self) -> &'static str {
        "cpal"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_backend_drives_callbacks() {
        let mut b = FakeBackend::new(FakeMode::Sine(440.0));
        let h = b.handle();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let _c = b
            .start_capture(
                Some("fake-input-1"),
                Box::new(move |x| g.lock().unwrap().extend_from_slice(x)),
            )
            .unwrap();
        let _p = b
            .start_playback(None, Box::new(|out| out.iter_mut().for_each(|o| *o = 0.25)))
            .unwrap();
        h.drive(960);
        assert_eq!(got.lock().unwrap().len(), 960);
        assert!(got.lock().unwrap().iter().any(|x| x.abs() > 0.4));
        assert_eq!(h.played(), vec![0.25; 960]);
        assert_eq!(h.selected_devices().0.as_deref(), Some("fake-input-1"));
        drop(_c);
        assert!(!h.capture_running());
        assert!(h.playback_running());
    }

    fn external_pair() -> (ExternalBackend, ExternalAudio) {
        let b = ExternalBackend::new();
        let h = b.handle();
        (b, h)
    }

    #[test]
    fn external_capture_passes_48k_through() {
        let (mut b, h) = external_pair();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let _c = b
            .start_capture(
                None,
                Box::new(move |x| g.lock().unwrap().extend_from_slice(x)),
            )
            .unwrap();
        let input: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        h.push_capture(&input, 48_000);
        assert_eq!(*got.lock().unwrap(), input);
        assert!(b.inputs().is_empty() && b.outputs().is_empty());
        assert_eq!(b.name(), "external");
    }

    #[test]
    fn external_capture_resamples_other_rates() {
        let (mut b, h) = external_pair();
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let _c = b
            .start_capture(
                None,
                Box::new(move |x| g.lock().unwrap().extend_from_slice(x)),
            )
            .unwrap();
        // 1 s at 44.1 kHz, in 441-sample chunks, gives about 1 s at 48 kHz.
        for _ in 0..100 {
            h.push_capture(&[0.1; 441], 44_100);
        }
        let n = got.lock().unwrap().len();
        assert!((47_990..=48_010).contains(&n), "44.1 kHz -> {n}");
        got.lock().unwrap().clear();
        for _ in 0..100 {
            h.push_capture(&[0.1; 240], 24_000);
        }
        let n = got.lock().unwrap().len();
        assert!((47_990..=48_010).contains(&n), "24 kHz -> {n}");
    }

    #[test]
    fn external_playback_resamples_and_keeps_callback() {
        let (mut b, h) = external_pair();
        let _p = b
            .start_playback(None, Box::new(|out| out.iter_mut().for_each(|o| *o = 0.5)))
            .unwrap();
        let mut out = vec![9.0; 480];
        h.pull_playback(&mut out, 48_000);
        assert_eq!(out, vec![0.5; 480]);
        // A rate change rebuilds the resampler but keeps the callback.
        let mut out = vec![9.0; 441];
        h.pull_playback(&mut out, 44_100);
        assert!(out[1..].iter().all(|v| (*v - 0.5).abs() < 1e-6), "{out:?}");
        let mut out = vec![9.0; 240];
        h.pull_playback(&mut out, 24_000);
        assert!(out[1..].iter().all(|v| (*v - 0.5).abs() < 1e-6));
    }

    #[test]
    fn external_without_streams_is_silent() {
        let (_b, h) = external_pair();
        let mut out = vec![9.0; 256];
        h.pull_playback(&mut out, 48_000);
        assert_eq!(out, vec![0.0; 256]);
        h.push_capture(&[0.5; 256], 48_000); // dropped, no panic
        assert!(!h.capture_running() && !h.playback_running());
    }

    #[test]
    fn external_guard_drop_clears_slot() {
        let (mut b, h) = external_pair();
        let c = b.start_capture(None, Box::new(|_| {})).unwrap();
        let p = b.start_playback(None, Box::new(|_| {})).unwrap();
        assert!(h.capture_running() && h.playback_running());
        drop(c);
        assert!(!h.capture_running() && h.playback_running());
        drop(p);
        assert!(!h.playback_running());
        let mut out = vec![9.0; 16];
        h.pull_playback(&mut out, 48_000);
        assert_eq!(out, vec![0.0; 16]);
    }

    #[test]
    fn null_backend_has_no_devices() {
        let mut n = NullBackend;
        assert!(n.inputs().is_empty() && n.outputs().is_empty());
        assert!(n.start_capture(None, Box::new(|_| {})).is_err());
        assert!(n.start_playback(None, Box::new(|_| {})).is_err());
    }
}
