//! Media Foundation H.264: hardware encoder MFT (all intra) and the Microsoft
//! H.264 decoder MFT. Every object here is created, used and released on one
//! thread, after `ensure_thread` has initialised COM (MTA) on it.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};

use super::{Decoded, HwEncoder, bitrate_for, normalize_au, nv12};

/// How long one encode may wait for the hardware encoder's output event.
const ENCODE_TIMEOUT: Duration = Duration::from_secs(2);
/// 100 ns units per second (Media Foundation time base).
const HNS: i64 = 10_000_000;

fn err(ctx: &str, e: windows::core::Error) -> String {
    format!("{ctx}: {e}")
}

thread_local! {
    static COM_READY: Cell<bool> = const { Cell::new(false) };
}

/// Initialise COM (multithreaded) on this thread once. A thread already in an STA
/// (`RPC_E_CHANGED_MODE`) is fine: COM is usable there too. COM stays initialised
/// for the thread's life, so no object can outlive it.
pub fn ensure_thread() -> Result<(), String> {
    COM_READY.with(|ready| {
        if ready.get() {
            return Ok(());
        }
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(format!(
                "CoInitializeEx: {}",
                windows::core::Error::from(hr)
            ));
        }
        ready.set(true);
        Ok(())
    })
}

/// COM on this thread plus a process-wide `MFStartup` (never shut down).
fn ensure_mf() -> Result<(), String> {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    ensure_thread()?;
    STARTED
        .get_or_init(|| {
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| err("MFStartup", e))
        })
        .clone()
}

/// Run `f` on a short-lived helper thread (keeps MF off the caller, e.g. the GUI).
fn off_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::Builder::new()
        .name("vc-mf-probe".into())
        .spawn(f)
        .ok()?
        .join()
        .ok()
}

/// Hardware NV12 → H.264 encoder activations, best first.
fn hw_encoders() -> Result<Vec<IMFActivate>, String> {
    ensure_mf()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut n,
        )
        .map_err(|e| err("MFTEnumEx", e))?;
        if list.is_null() {
            return Ok(vec![]);
        }
        let acts = std::slice::from_raw_parts_mut(list, n as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(list as *const _));
        Ok(acts)
    }
}

fn friendly_name(act: &IMFActivate) -> String {
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0u32;
        if act
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut len)
            .is_err()
            || s.is_null()
        {
            return "Hardware H.264 encoder".into();
        }
        let name = s.to_string().unwrap_or_default();
        CoTaskMemFree(Some(s.0 as *const _));
        name
    }
}

pub fn detect() -> Option<HwEncoder> {
    static FOUND: OnceLock<Option<HwEncoder>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            off_thread(|| match hw_encoders() {
                Ok(acts) => acts.first().map(|a| HwEncoder {
                    name: friendly_name(a),
                }),
                Err(e) => {
                    tracing::info!("no hardware video encoder: {e}");
                    None
                }
            })
            .flatten()
        })
        .clone()
}

pub fn decoder_available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| off_thread(|| Decoder::new().is_ok()).unwrap_or(false))
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn video_type(subtype: &GUID, w: u32, h: u32, fps: u32) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        if w > 0 && h > 0 {
            t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        }
        if fps > 0 {
            t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
        }
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        Ok(t)
    }
}

fn make_sample(data: &[u8], time: i64, duration: i64) -> windows::core::Result<IMFSample> {
    unsafe {
        let buf = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut p = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.Unlock()?;
        buf.SetCurrentLength(data.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        s.SetSampleTime(time)?;
        s.SetSampleDuration(duration)?;
        Ok(s)
    }
}

fn sample_bytes(s: &IMFSample) -> windows::core::Result<Vec<u8>> {
    unsafe {
        let buf = s.ConvertToContiguousBuffer()?;
        let mut p = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut p, None, Some(&mut len))?;
        let v = std::slice::from_raw_parts(p, len as usize).to_vec();
        buf.Unlock()?;
        Ok(v)
    }
}

fn stream_ids(t: &IMFTransform) -> (u32, u32) {
    let (mut i, mut o) = ([0u32; 1], [0u32; 1]);
    match unsafe { t.GetStreamIDs(&mut i, &mut o) } {
        Ok(()) => (i[0], o[0]),
        Err(_) => (0, 0), // E_NOTIMPL: ids are 0..n
    }
}

/// Pull one output sample. `Ok(None)` when the transform needs more input.
fn pull_output(t: &IMFTransform, out_id: u32) -> windows::core::Result<Option<IMFSample>> {
    unsafe {
        let info = t.GetOutputStreamInfo(out_id)?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let ours = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1))?)?;
            Some(s)
        };
        let mut buf = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: out_id,
            pSample: ManuallyDrop::new(ours),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let r = t.ProcessOutput(0, &mut buf, &mut status);
        let sample = ManuallyDrop::take(&mut buf[0].pSample);
        drop(ManuallyDrop::take(&mut buf[0].pEvents));
        match r {
            Ok(()) => Ok(sample),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// All-intra hardware H.264 encoder session.
pub struct Encoder {
    /// The activation the MFT came from; shut down on drop so no hardware encoder
    /// session leaks across rebuilds.
    act: IMFActivate,
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    in_id: u32,
    out_id: u32,
    /// `METransformNeedInput` events not yet answered with input (async MFTs).
    credits: u32,
    frame: i64,
    duration: i64,
    /// Cached `MF_MT_MPEG_SEQUENCE_HEADER` (SPS + PPS), prepended when missing.
    header: Vec<u8>,
}

impl Encoder {
    pub fn new(w: u32, h: u32, fps: u32, quality: u8) -> Result<Self, String> {
        let acts = hw_encoders()?;
        let mut last = "no hardware H.264 encoder".to_string();
        for act in acts {
            match Self::build(&act, w, h, fps.max(1), quality) {
                Ok(e) => {
                    tracing::info!(
                        "hardware encoder {} at {w}x{h} {fps} fps",
                        friendly_name(&act)
                    );
                    return Ok(e);
                }
                Err(e) => {
                    let _ = unsafe { act.ShutdownObject() };
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn build(act: &IMFActivate, w: u32, h: u32, fps: u32, quality: u8) -> Result<Self, String> {
        unsafe {
            let mft: IMFTransform = act.ActivateObject().map_err(|e| err("activate", e))?;
            let attrs = mft.GetAttributes().map_err(|e| err("attributes", e))?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| err("async unlock", e))?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            let codec: Option<ICodecAPI> = mft.cast().ok();
            let bitrate = bitrate_for(w, h, fps, quality);
            if let Some(c) = &codec {
                let set = |k: &GUID, v: VARIANT| c.SetValue(k, &v).is_ok();
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(1u32));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                let quality_mode =
                    set(
                        &CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32),
                    ) && set(&CODECAPI_AVEncCommonQuality, VARIANT::from(quality as u32));
                if !quality_mode {
                    set(
                        &CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    );
                    set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                }
            }
            let (in_id, out_id) = stream_ids(&mft);
            let out =
                video_type(&MFVideoFormat_H264, w, h, fps).map_err(|e| err("output type", e))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(|e| err("bitrate", e))?;
            let _ = out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32);
            mft.SetOutputType(out_id, &out, 0)
                .map_err(|e| err("set output type", e))?;
            let inp =
                video_type(&MFVideoFormat_NV12, w, h, fps).map_err(|e| err("input type", e))?;
            mft.SetInputType(in_id, &inp, 0)
                .map_err(|e| err("set input type (system-memory NV12)", e))?;
            let header = mft
                .GetOutputCurrentType(out_id)
                .ok()
                .and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER))
                .unwrap_or_default();
            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(|e| err("flush", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("begin streaming", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("start of stream", e))?;
            let events = if is_async {
                Some(
                    mft.cast::<IMFMediaEventGenerator>()
                        .map_err(|e| err("event generator", e))?,
                )
            } else {
                None
            };
            Ok(Self {
                act: act.clone(),
                mft,
                codec,
                events,
                in_id,
                out_id,
                credits: 0,
                frame: 0,
                duration: HNS / fps as i64,
                header,
            })
        }
    }

    fn feed(&mut self, nv12: &[u8]) -> Result<(), String> {
        let sample = make_sample(nv12, self.frame * self.duration, self.duration)
            .map_err(|e| err("input sample", e))?;
        self.frame += 1;
        if let Some(c) = &self.codec {
            // GOP size 1 alone does not guarantee an IDR with every input.
            let _ = unsafe { c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
        unsafe { self.mft.ProcessInput(self.in_id, &sample, 0) }.map_err(|e| err("ProcessInput", e))
    }

    fn take_output(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match pull_output(&self.mft, self.out_id) {
                Ok(Some(s)) => return sample_bytes(&s).map(Some).map_err(|e| err("output", e)),
                Ok(None) => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => unsafe {
                    let t = self
                        .mft
                        .GetOutputAvailableType(self.out_id, 0)
                        .map_err(|e| err("renegotiate", e))?;
                    self.mft
                        .SetOutputType(self.out_id, &t, 0)
                        .map_err(|e| err("renegotiate", e))?;
                    if let Some(h) = blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER) {
                        self.header = h;
                    }
                },
                Err(e) => return Err(err("ProcessOutput", e)),
            }
        }
    }

    fn encode_raw(&mut self, nv12: &[u8]) -> Result<Vec<u8>, String> {
        let Some(events) = self.events.clone() else {
            // Synchronous MFT: one input, then drain what it has.
            self.feed(nv12)?;
            return self
                .take_output()?
                .ok_or_else(|| "encoder produced no output".to_string());
        };
        let mut fed = false;
        if self.credits > 0 {
            self.credits -= 1;
            self.feed(nv12)?;
            fed = true;
        }
        let deadline = Instant::now() + ENCODE_TIMEOUT;
        loop {
            let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => ev,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Err("hardware encoder timed out".into());
                    }
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                Err(e) => return Err(err("GetEvent", e)),
            };
            let kind = unsafe { ev.GetType() }.map_err(|e| err("event type", e))?;
            if kind == METransformNeedInput.0 as u32 {
                if fed {
                    self.credits += 1;
                } else {
                    self.feed(nv12)?;
                    fed = true;
                }
            } else if kind == METransformHaveOutput.0 as u32
                && let Some(au) = self.take_output()?
            {
                return Ok(au);
            }
        }
    }

    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<u8>, String> {
        let raw = self.encode_raw(nv12)?;
        if self.header.is_empty()
            && let Ok(t) = unsafe { self.mft.GetOutputCurrentType(self.out_id) }
            && let Some(h) = blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER)
        {
            self.header = h;
        }
        normalize_au(&raw, &self.header)
            .ok_or_else(|| "encoder output is not a self-contained H.264 IDR frame".to_string())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(g) = &self.events {
                // Async MFTs hold a reference to their event queue until shut down.
                if let Ok(s) = g.cast::<IMFShutdown>() {
                    let _ = s.Shutdown();
                }
            }
            let _ = self.act.ShutdownObject();
        }
    }
}

fn blob(t: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    unsafe {
        let n = t.GetBlobSize(key).ok()?;
        let mut v = vec![0u8; n as usize];
        t.GetBlob(key, &mut v, None).ok()?;
        (!v.is_empty()).then_some(v)
    }
}

/// Microsoft H.264 decoder MFT (software or DXVA inside; system-memory NV12 out).
struct Decoder {
    mft: IMFTransform,
    in_id: u32,
    out_id: u32,
    /// Output geometry: (visible w, visible h, stride, padded height).
    geom: Option<(u32, u32, usize, usize)>,
}

impl Decoder {
    fn new() -> Result<Self, String> {
        ensure_mf()?;
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| err("create H.264 decoder", e))?;
            if let Ok(a) = mft.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let (in_id, out_id) = stream_ids(&mft);
            let inp =
                video_type(&MFVideoFormat_H264, 0, 0, 0).map_err(|e| err("decoder input", e))?;
            mft.SetInputType(in_id, &inp, 0)
                .map_err(|e| err("decoder input type", e))?;
            let mut d = Self {
                mft,
                in_id,
                out_id,
                geom: None,
            };
            d.set_nv12_output()?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("decoder begin", e))?;
            Ok(d)
        }
    }

    fn set_nv12_output(&mut self) -> Result<(), String> {
        unsafe {
            let mut i = 0;
            loop {
                let t = self
                    .mft
                    .GetOutputAvailableType(self.out_id, i)
                    .map_err(|e| err("no NV12 decoder output", e))?;
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    self.mft
                        .SetOutputType(self.out_id, &t, 0)
                        .map_err(|e| err("decoder output type", e))?;
                    self.geom = geometry(&t);
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<Decoded, String> {
        let sample = make_sample(au, 0, HNS / 30).map_err(|e| err("decoder sample", e))?;
        unsafe {
            self.mft
                .ProcessInput(self.in_id, &sample, 0)
                .map_err(|e| err("decoder ProcessInput", e))?;
            // Drain: an all-intra frame must come out now, not one frame late.
            self.mft
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(|e| err("decoder drain", e))?;
        }
        let mut last = None;
        let mut changes = 0;
        loop {
            match pull_output(&self.mft, self.out_id) {
                Ok(Some(s)) => last = Some(self.to_rgba(&s)?),
                Ok(None) => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    changes += 1;
                    if changes > 4 {
                        return Err("decoder keeps changing its output format".into());
                    }
                    self.set_nv12_output()?
                }
                Err(e) => return Err(err("decoder ProcessOutput", e)),
            }
        }
        last.ok_or_else(|| "decoder produced no frame".to_string())
    }

    fn to_rgba(&self, s: &IMFSample) -> Result<Decoded, String> {
        let (w, h, stride, padded_h) = self.geom.ok_or("decoder output size unknown")?;
        let bytes = sample_bytes(s).map_err(|e| err("decoder output", e))?;
        let uv_at = stride * padded_h;
        if bytes.len() < uv_at + stride * (h as usize).div_ceil(2) {
            return Err(format!("decoder frame too short ({} bytes)", bytes.len()));
        }
        let (y, uv) = bytes.split_at(uv_at);
        Ok((w, h, nv12::nv12_to_rgba(y, uv, stride, w, h)))
    }
}

/// (visible w, visible h, stride, padded height) of an NV12 output type.
fn geometry(t: &IMFMediaType) -> Option<(u32, u32, usize, usize)> {
    unsafe {
        let size = t.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
        let (fw, fh) = ((size >> 32) as u32, size as u32);
        if fw == 0 || fh == 0 {
            return None;
        }
        let stride = t
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .map(|s| (s as i32).unsigned_abs() as usize)
            .filter(|s| *s >= fw as usize)
            .unwrap_or(fw as usize);
        let (mut w, mut h) = (fw, fh);
        let mut area = MFVideoArea::default();
        let raw = std::slice::from_raw_parts_mut(
            &mut area as *mut MFVideoArea as *mut u8,
            std::mem::size_of::<MFVideoArea>(),
        );
        if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, raw, None)
            .is_ok()
            && area.Area.cx > 0
            && area.Area.cy > 0
        {
            w = (area.Area.cx as u32).min(fw);
            h = (area.Area.cy as u32).min(fh);
        }
        Some((w, h, stride, fh as usize))
    }
}

struct DecodeJob {
    au: Vec<u8>,
    reply: mpsc::SyncSender<Result<Decoded, String>>,
}

/// Long-lived decoder threads; a sharer always lands on the same one.
pub const DECODE_WORKERS: usize = 4;
/// Longest wait for one frame; a hung worker is then replaced (and leaked).
const DECODE_TIMEOUT: Duration = Duration::from_secs(2);

/// Start a decoder worker. It owns its `Decoder` and never exits: decoders are
/// never kept in thread-locals, which are dropped at thread exit under the loader
/// lock, where releasing the Microsoft decoder deadlocks.
fn spawn_worker() -> Option<mpsc::Sender<DecodeJob>> {
    let (tx, rx) = mpsc::channel::<DecodeJob>();
    std::thread::Builder::new()
        .name("vc-h264-decode".into())
        .spawn(move || {
            let mut dec: Option<Decoder> = None;
            for job in rx {
                let r = (|| {
                    if dec.is_none() {
                        dec = Some(Decoder::new()?);
                    }
                    dec.as_mut().expect("just built").decode(&job.au)
                })();
                if r.is_err() {
                    dec = None; // rebuilt for the next frame
                }
                let _ = job.reply.send(r);
            }
        })
        .ok()
        .map(|_| tx)
}

fn pool() -> &'static [std::sync::Mutex<Option<mpsc::Sender<DecodeJob>>>; DECODE_WORKERS] {
    static POOL: OnceLock<[std::sync::Mutex<Option<mpsc::Sender<DecodeJob>>>; DECODE_WORKERS]> =
        OnceLock::new();
    POOL.get_or_init(|| std::array::from_fn(|_| std::sync::Mutex::new(spawn_worker())))
}

/// Run `job` on worker `slot % DECODE_WORKERS`, waiting at most `timeout`. On a
/// timeout that worker is replaced by a fresh one (the hung thread is leaked).
fn run_on(
    slot: u64,
    job: impl FnOnce(mpsc::SyncSender<Result<Decoded, String>>) -> DecodeJob,
    timeout: Duration,
) -> Result<Decoded, String> {
    let entry = &pool()[(slot % DECODE_WORKERS as u64) as usize];
    let (reply, rx) = mpsc::sync_channel(1);
    {
        let mut tx = entry.lock().unwrap_or_else(|e| e.into_inner());
        if tx.is_none() {
            *tx = spawn_worker();
        }
        let sent = tx
            .as_ref()
            .is_some_and(|t| t.send(job(reply.clone())).is_ok());
        if !sent {
            *tx = None; // respawned on the next frame
            return Err("could not start the H.264 decoder thread".into());
        }
    }
    drop(reply);
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            *entry.lock().unwrap_or_else(|e| e.into_inner()) = spawn_worker();
            Err("H.264 decode timed out".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            *entry.lock().unwrap_or_else(|e| e.into_inner()) = None;
            Err("H.264 decoder thread is gone".into())
        }
    }
}

/// Decode on the pool worker for `slot` (a stable per-sharer value), so one
/// sharer's frames stay on one decoder and a hung decode stalls only that worker.
pub fn decode(slot: u64, au: &[u8]) -> Result<Decoded, String> {
    let au = au.to_vec();
    run_on(slot, move |reply| DecodeJob { au, reply }, DECODE_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Screen-like content: sharp grey (luma-only) edges that move with `t`, under
    /// smooth colour gradients (NV12 halves chroma resolution, so sharp colour
    /// edges would measure the format, not the encoder).
    fn frame(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = ((x + t * 4) / 16 + y / 16) % 2 * 90 + 50;
                rgba.extend_from_slice(&[
                    (v + x * 60 / w) as u8,
                    (v + y * 60 / h) as u8,
                    (v + 30) as u8,
                    255,
                ]);
            }
        }
        rgba
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mut se = 0f64;
        let mut n = 0f64;
        for (pa, pb) in a.chunks(4).zip(b.chunks(4)) {
            for c in 0..3 {
                let d = pa[c] as f64 - pb[c] as f64;
                se += d * d;
                n += 1.0;
            }
        }
        let mse = se / n;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0 * 255.0 / mse).log10()
        }
    }

    #[test]
    fn decode_timeout_replaces_the_worker() {
        // An empty access unit fails fast; a reply that never comes (the worker's
        // reply sender is kept here, never used) times out instead of hanging.
        let mut kept = None;
        let t = Instant::now();
        let r = run_on(
            3,
            |reply| {
                kept = Some(reply);
                DecodeJob {
                    au: vec![],
                    reply: mpsc::sync_channel(1).0,
                }
            },
            Duration::from_millis(200),
        );
        assert_eq!(r, Err("H.264 decode timed out".to_string()));
        assert!(t.elapsed() < Duration::from_secs(2));
        // The slot has a working worker again.
        assert!(pool()[3].lock().unwrap().is_some());
    }

    #[test]
    #[ignore = "needs a hardware H.264 encoder: cargo test -p vc-client --lib encoder_rebuild_20x -- --ignored"]
    fn encoder_rebuild_20x() {
        ensure_thread().unwrap();
        let (w, h) = (1920u32, 1080u32);
        for _ in 0..20 {
            drop(Encoder::new(w, h, 60, 70).expect("encoder"));
        }
        let mut enc = Encoder::new(w, h, 60, 70).expect("encoder after 20 rebuilds");
        let au = enc
            .encode(&nv12::rgba_to_nv12(&frame(w, h, 0), w, w, h))
            .expect("encode");
        assert_eq!(
            vc_proto::sniff_screen(&au),
            Some(vc_proto::ScreenKind::H264)
        );
    }

    #[test]
    #[ignore = "needs a hardware H.264 encoder: cargo test -p vc-client --lib detect_hw_encoder -- --ignored --nocapture"]
    fn detect_hw_encoder() {
        let hw = detect().expect("no hardware encoder found");
        println!("HW ENCODER: {}", hw.name);
        assert!(
            hw.name.to_lowercase().contains("nvidia"),
            "expected the NVIDIA MFT, got {}",
            hw.name
        );
        assert!(decoder_available());
    }

    #[test]
    #[ignore = "needs a hardware H.264 encoder: cargo test -p vc-client --lib mf_h264_roundtrip -- --ignored --nocapture"]
    fn mf_h264_roundtrip() {
        ensure_thread().unwrap();
        let (w, h) = (1920u32, 1080u32);
        let mut enc = Encoder::new(w, h, 30, 70).expect("encoder");
        let mut bodies = Vec::new();
        let t0 = Instant::now();
        for t in 0..10 {
            let rgba = frame(w, h, t);
            let nv = nv12::rgba_to_nv12(&rgba, w, w, h);
            let au = enc.encode(&nv).expect("encode");
            assert_eq!(
                vc_proto::sniff_screen(&au),
                Some(vc_proto::ScreenKind::H264),
                "frame {t}"
            );
            let types: Vec<u8> = super::super::split_nals(&au)
                .iter()
                .map(|n| n[0] & 0x1F)
                .collect();
            assert!(
                types.contains(&7) && types.contains(&8),
                "frame {t}: {types:?}"
            );
            assert!(types.contains(&5), "frame {t} is not an IDR: {types:?}");
            bodies.push(au);
        }
        let per_frame = t0.elapsed() / 10;
        drop(enc);
        // A fresh decoder joining at frame 5 decodes frame 5 alone.
        let mut dec = Decoder::new().expect("decoder");
        let (dw, dh, rgba) = dec.decode(&bodies[5]).expect("decode");
        assert_eq!((dw, dh), (w, h));
        let p = psnr(&frame(w, h, 5), &rgba);
        println!(
            "ROUNDTRIP encode/frame={per_frame:?} sizes={:?} psnr={p:.1}",
            bodies.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(p >= 35.0, "PSNR {p:.1} < 35");
        // And the next frame right after, without being one frame late.
        let (_, _, rgba6) = dec.decode(&bodies[6]).expect("decode 6");
        assert!(psnr(&frame(w, h, 6), &rgba6) >= 35.0);
    }

    #[test]
    #[ignore = "needs a hardware H.264 encoder: cargo test -p vc-client --lib mf_small_frame_via_worker -- --ignored"]
    fn mf_small_frame_via_worker() {
        ensure_thread().unwrap();
        let (w, h) = (320u32, 180u32);
        let mut enc = Encoder::new(w, h, 30, 60).expect("encoder");
        let rgba = frame(w, h, 1);
        let au = enc
            .encode(&nv12::rgba_to_nv12(&rgba, w, w, h))
            .expect("encode");
        // From a thread that exits afterwards (like a tokio blocking thread).
        let (dw, dh, out) = std::thread::spawn(move || decode(0, &au))
            .join()
            .unwrap()
            .unwrap();
        assert_eq!((dw, dh), (w, h));
        assert!(psnr(&rgba, &out) >= 35.0);
    }
}
