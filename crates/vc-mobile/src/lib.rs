//! iOS core of the voicechat client: the desktop's headless app (network, audio
//! pipeline, model) behind a small C bridge that speaks JSON. SwiftUI only draws.
//!
//! The C header is `include/vc_mobile.h` (written by hand; `tests/golden.rs`
//! checks that every function exported here is declared there).
//!
//! Threads: every `vc_app_*` call is made on the app's main thread. The
//! `vc_audio_*` calls run on the host's real-time audio threads and only touch
//! their own [`VcAudio`] handle, never the [`VcApp`]. `vc_pixels_release` may run
//! on any thread.
//!
//! Every body runs under `catch_unwind`. After a panic the app is marked
//! poisoned and shut down (network, audio pipeline and audio streams stopped,
//! so the microphone no longer transmits), and every later call returns NULL or
//! the error `"core panicked"`.

pub mod app;
pub mod cmd;
pub mod view;

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;

use egui::ColorImage;
use vc_client::audio::ExternalAudio;

pub use app::MobileApp;

/// The error text after a caught panic.
pub const PANICKED: &str = "core panicked";

/// The opaque app handle behind `VcApp *`.
pub struct VcApp {
    /// `None` once a caught panic shut the app down.
    app: Option<MobileApp>,
    poisoned: bool,
}

/// The opaque audio handle behind `VcAudio *`.
pub struct VcAudio(ExternalAudio);

/// Pixels of a chat image or screen frame: premultiplied RGBA8, row-major,
/// `width * height * 4` bytes. `handle` keeps them alive until
/// `vc_pixels_release(handle)`.
#[repr(C)]
pub struct VcPixels {
    pub data: *const u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub handle: *mut c_void,
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn to_c(s: String) -> *mut c_char {
    let s = s.replace('\0', " ");
    CString::new(s).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// # Safety
/// `p` is NULL or a NUL-terminated string.
unsafe fn from_c<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller passes a valid NUL-terminated string.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Run `f` on the app unless it is poisoned; a panic poisons it and drops the
/// app, which stops its threads, closes its connections and ends its audio
/// streams (so `vc_audio_capture` no longer reaches the pipeline).
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
unsafe fn with_app<T>(app: *mut VcApp, dead: T, f: impl FnOnce(&mut MobileApp) -> T) -> T {
    if app.is_null() {
        return dead;
    }
    // SAFETY: the caller passes a live pointer from `vc_app_new`, on the main thread.
    let a = unsafe { &mut *app };
    if a.poisoned {
        return dead;
    }
    let Some(inner) = a.app.as_mut() else {
        return dead;
    };
    match catch_unwind(AssertUnwindSafe(|| f(inner))) {
        Ok(v) => v,
        Err(e) => {
            a.poisoned = true;
            let what = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!("{PANICKED}: {what}; shutting the core down");
            let _ = catch_unwind(AssertUnwindSafe(|| a.app = None));
            dead
        }
    }
}

fn pixels(img: Arc<ColorImage>, generation: u64, out: &mut VcPixels) {
    out.data = img.pixels.as_ptr().cast::<u8>();
    out.len = img.pixels.len() * 4;
    out.width = img.size[0] as u32;
    out.height = img.size[1] as u32;
    out.generation = generation;
    out.handle = Box::into_raw(Box::new(img)).cast::<c_void>();
}

/// Create the app. `audio` is `"external"` or `"fake-sine"`. NULL on error.
///
/// # Safety
/// Both arguments are NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_new(
    profile_dir: *const c_char,
    audio: *const c_char,
) -> *mut VcApp {
    init_logging();
    // SAFETY: the caller passes NUL-terminated strings.
    let (dir, audio) = unsafe { (from_c(profile_dir), from_c(audio)) };
    let (Some(dir), Some(audio)) = (dir, audio) else {
        return std::ptr::null_mut();
    };
    match catch_unwind(|| MobileApp::new(Path::new(dir), audio)) {
        Ok(Ok(app)) => Box::into_raw(Box::new(VcApp {
            app: Some(app),
            poisoned: false,
        })),
        Ok(Err(e)) => {
            tracing::error!("vc_app_new: {e}");
            std::ptr::null_mut()
        }
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free the app (stop the host's audio engine first).
///
/// # Safety
/// `app` is NULL or a pointer from `vc_app_new`, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_free(app: *mut VcApp) {
    if app.is_null() {
        return;
    }
    // SAFETY: the pointer came from `Box::into_raw` in `vc_app_new`.
    let b = unsafe { Box::from_raw(app) };
    let _ = catch_unwind(AssertUnwindSafe(move || drop(b)));
}

/// One tick: drain the network, ping, refresh the pipeline.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_step(app: *mut VcApp) {
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, (), MobileApp::step) }
}

/// Run a JSON command. NULL when it worked, else error text to free with
/// `vc_string_free`.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`; `json` is a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_command(app: *mut VcApp, json: *const c_char) -> *mut c_char {
    // SAFETY: the caller passes a NUL-terminated string.
    let Some(json) = (unsafe { from_c(json) }) else {
        return to_c("bad command: not UTF-8".into());
    };
    // SAFETY: forwarded from the caller.
    let res = unsafe { with_app(app, Err(PANICKED.to_string()), |a| a.command(json)) };
    match res {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => to_c(e),
    }
}

/// The snapshot JSON, or NULL when unchanged (or poisoned). Free with
/// `vc_string_free`.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_snapshot(app: *mut VcApp) -> *mut c_char {
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, None, MobileApp::snapshot) }.map_or(std::ptr::null_mut(), to_c)
}

/// The live JSON, or NULL when unchanged (or poisoned). Free with
/// `vc_string_free`.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_live(app: *mut VcApp) -> *mut c_char {
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, None, MobileApp::live) }.map_or(std::ptr::null_mut(), to_c)
}

/// Is the app poisoned by a caught panic (or NULL)?
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_poisoned(app: *const VcApp) -> bool {
    // SAFETY: the caller passes NULL or a live pointer.
    app.is_null() || unsafe { (*app).poisoned }
}

/// Push to talk held or released.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_set_ptt(app: *mut VcApp, down: bool) {
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, (), |a| a.set_ptt(down)) }
}

/// Show (`msg`) or clear (NULL) the audio warning banner.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`; `msg` is NULL or a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_set_audio_warning(app: *mut VcApp, msg: *const c_char) {
    // SAFETY: the caller passes NULL or a NUL-terminated string.
    let msg = unsafe { from_c(msg) };
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, (), |a| a.set_audio_warning(msg)) }
}

/// Pixels of chat image `image_id` on server `key`. `false` when it is gone.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`; `out` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_image(
    app: *mut VcApp,
    key: u64,
    image_id: u64,
    out: *mut VcPixels,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: forwarded from the caller.
    let img = unsafe { with_app(app, None, |a| a.image(key, image_id)) };
    match img {
        Some(img) => {
            // SAFETY: `out` is writable (checked non-NULL above).
            pixels(img, 0, unsafe { &mut *out });
            true
        }
        None => false,
    }
}

/// The latest screen frame of `user` on server `key`. `false` until one arrives.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`; `out` is NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_screen(
    app: *mut VcApp,
    key: u64,
    user: u32,
    out: *mut VcPixels,
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: forwarded from the caller.
    let frame = unsafe { with_app(app, None, |a| a.screen(key, user)) };
    match frame {
        Some((img, generation)) => {
            // SAFETY: `out` is writable (checked non-NULL above).
            pixels(img, generation, unsafe { &mut *out });
            true
        }
        None => false,
    }
}

/// Release pixels from `vc_app_image` or `vc_app_screen` (any thread).
///
/// # Safety
/// `handle` is NULL or a `VcPixels::handle` not released before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_pixels_release(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle came from `Box::into_raw` in `pixels`.
    let b = unsafe { Box::from_raw(handle.cast::<Arc<ColorImage>>()) };
    let _ = catch_unwind(AssertUnwindSafe(move || drop(b)));
}

/// The audio handle, independent of the app. NULL unless the app was created
/// with `"external"` audio. Free with `vc_audio_free`.
///
/// # Safety
/// `app` is NULL or a live pointer from `vc_app_new`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_app_audio(app: *mut VcApp) -> *mut VcAudio {
    // SAFETY: forwarded from the caller.
    unsafe { with_app(app, None, |a| a.audio()) }.map_or(std::ptr::null_mut(), |h| {
        Box::into_raw(Box::new(VcAudio(h)))
    })
}

/// Feed `n` captured mono samples at `rate` Hz (real-time thread).
///
/// # Safety
/// `a` is NULL or a live pointer from `vc_app_audio`; `mono` is NULL or points
/// at `n` floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_audio_capture(
    a: *const VcAudio,
    mono: *const f32,
    n: usize,
    rate: u32,
) {
    if a.is_null() || mono.is_null() || n == 0 {
        return;
    }
    // SAFETY: the caller passes a live handle and `n` readable floats.
    let (a, mono) = unsafe { (&*a, std::slice::from_raw_parts(mono, n)) };
    let _ = catch_unwind(AssertUnwindSafe(|| a.0.push_capture(mono, rate)));
}

/// Fill `n` mono playback samples at `rate` Hz (real-time thread). Zeros when
/// there is nothing to play.
///
/// # Safety
/// `a` is NULL or a live pointer from `vc_app_audio`; `mono` is NULL or points
/// at `n` writable floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_audio_render(a: *const VcAudio, mono: *mut f32, n: usize, rate: u32) {
    if mono.is_null() || n == 0 {
        return;
    }
    // SAFETY: the caller passes `n` writable floats.
    let out = unsafe { std::slice::from_raw_parts_mut(mono, n) };
    if a.is_null() {
        out.iter_mut().for_each(|o| *o = 0.0);
        return;
    }
    // SAFETY: the caller passes a live handle.
    let a = unsafe { &*a };
    if catch_unwind(AssertUnwindSafe(|| a.0.pull_playback(out, rate))).is_err() {
        out.iter_mut().for_each(|o| *o = 0.0);
    }
}

/// Free an audio handle (stop the engine that uses it first).
///
/// # Safety
/// `a` is NULL or a pointer from `vc_app_audio`, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_audio_free(a: *mut VcAudio) {
    if a.is_null() {
        return;
    }
    // SAFETY: the pointer came from `Box::into_raw` in `vc_app_audio`.
    drop(unsafe { Box::from_raw(a) });
}

/// Free a string returned by this library.
///
/// # Safety
/// `s` is NULL or a string from this library, not freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vc_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: the string came from `CString::into_raw` in `to_c`.
    drop(unsafe { CString::from_raw(s) });
}

/// The core's version (static; do not free).
#[unsafe(no_mangle)]
pub extern "C" fn vc_version() -> *const c_char {
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr().cast::<c_char>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caught_panic_poisons_and_shuts_the_app_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = CString::new(dir.path().to_str().unwrap()).unwrap();
        let mode = CString::new("fake-sine").unwrap();
        // SAFETY: valid NUL-terminated strings; the pointer is used on this thread
        // only and freed once at the end.
        unsafe {
            let p = vc_app_new(path.as_ptr(), mode.as_ptr());
            assert!(!p.is_null());
            assert!(!vc_app_poisoned(p));
            with_app(p, (), |_| panic!("test"));
            assert!(vc_app_poisoned(p));
            assert!((*p).app.is_none(), "the app is dropped after the panic");
            let json = CString::new(r#"{"t":"ToggleMute"}"#).unwrap();
            let err = vc_app_command(p, json.as_ptr());
            assert!(!err.is_null());
            assert_eq!(CStr::from_ptr(err).to_str().unwrap(), PANICKED);
            vc_string_free(err);
            assert!(vc_app_snapshot(p).is_null());
            assert!(vc_app_audio(p).is_null());
            vc_app_step(p);
            vc_app_free(p);
        }
    }
}
