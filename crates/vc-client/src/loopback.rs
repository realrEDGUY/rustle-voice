//! Stream sound on Windows: WASAPI process loopback of everything this machine
//! plays *except* this process tree (Windows 10 2004+), so viewers never hear
//! their own voices echoed back. There is no whole-system fallback: on older
//! Windows starting it fails and the toggle reports "unavailable".

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{BLOB, IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use crate::audio::CaptureCb;

/// Capture format asked of the loopback client (it converts): 48 kHz stereo f32.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Shared-mode buffer: 200 ms.
const BUFFER_HNS: i64 = 2_000_000;

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion(mpsc::SyncSender<()>);

impl IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// A running loopback capture; dropping it stops and joins the thread.
pub struct Loopback {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Opened {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
}

fn activate() -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: unsafe { GetCurrentProcessId() },
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The blob points at `params` on this stack frame; never let PropVariantClear free it.
    let pv = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: &params as *const _ as *mut u8,
                    },
                },
            }),
        },
    });
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Completion(tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv as *const PROPVARIANT),
            &handler,
        )
    }
    .map_err(|e| anyhow!("process loopback needs Windows 10 2004 or later ({e})"))?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow!("audio loopback activation timed out"))?;
    let mut hr = HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }?;
    hr.ok()?;
    Ok(unk.ok_or_else(|| anyhow!("no audio client"))?.cast()?)
}

fn open() -> Result<Opened> {
    crate::video::ensure_com_thread().map_err(|e| anyhow!(e))?;
    let client = activate()?;
    let block = CHANNELS * 4;
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            BUFFER_HNS,
            0,
            &fmt,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        if let Err(e) = client.SetEventHandle(event) {
            let _ = CloseHandle(event);
            return Err(e.into());
        }
        let capture: IAudioCaptureClient = match client.GetService() {
            Ok(c) => c,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(e.into());
            }
        };
        client.Start()?;
        Ok(Opened {
            client,
            capture,
            event,
        })
    }
}

/// Drain every ready packet, downmixed to mono, into `cb`.
fn drain(o: &Opened, cb: &mut CaptureCb, mono: &mut Vec<f32>) -> Result<()> {
    unsafe {
        while o.capture.GetNextPacketSize()? > 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            o.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            mono.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                mono.resize(frames as usize, 0.0);
            } else {
                let s = std::slice::from_raw_parts(
                    data as *const f32,
                    frames as usize * CHANNELS as usize,
                );
                mono.extend(
                    s.chunks_exact(CHANNELS as usize)
                        .map(|p| (p[0] + p[1]) * 0.5),
                );
            }
            o.capture.ReleaseBuffer(frames)?;
            if !mono.is_empty() {
                cb(mono);
            }
        }
    }
    Ok(())
}

/// Start capturing system audio minus this process into `cb` (48 kHz mono f32).
pub fn start(mut cb: CaptureCb) -> Result<Loopback> {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let thread = std::thread::Builder::new()
        .name("vc-loopback".into())
        .spawn(move || {
            let o = match open() {
                Ok(o) => {
                    let _ = ready_tx.send(Ok(()));
                    o
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            let mut mono = Vec::new();
            while !s.load(Ordering::Acquire) {
                unsafe { WaitForSingleObject(o.event, 100) };
                if let Err(e) = drain(&o, &mut cb, &mut mono) {
                    tracing::warn!("stream sound capture stopped: {e}");
                    break;
                }
            }
            unsafe {
                let _ = o.client.Stop();
                let _ = CloseHandle(o.event);
            }
        })?;
    let lb = Loopback {
        stop,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(())) => Ok(lb),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => Err(anyhow!("audio loopback did not start")),
    }
}
