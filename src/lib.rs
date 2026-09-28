//! ProcessAudioCapture - a C ABI DLL that captures the audio a Windows process
//! renders to its default rendering endpoint.
//!
//! Capture uses the *process loopback* activation path
//! (`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`), which requires Windows 10
//! version 2004 (build 19041) or newer. The target process must render audio
//! through the normal Windows audio engine: protected content and exclusive
//! mode streams cannot be captured.
//!
//! Audio is delivered to a user supplied callback as interleaved 32-bit IEEE
//! float PCM. The callback runs on a capture thread owned by this DLL; it must
//! return quickly, must not block, and must not call back into any `pac_*`
//! function.
//!
//! Every exported function uses the `C` ABI. Release builds abort on panic, so
//! an unwinding panic can never cross the FFI boundary.

#![cfg(windows)]
// The crate name is intentionally PascalCase so the produced artifact keeps
// the ProcessAudioCapture.dll file name.
#![allow(non_snake_case)]

mod capture;
mod com;
mod dsp;
mod media;
mod targets;

use std::{
    ffi::{c_char, c_void, CString},
    ptr,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{CreateEventW, SetEvent, WaitForSingleObject},
    },
};

use capture::Shared;

/// Returned when the call succeeded.
pub const PAC_OK: i32 = 0;
/// A null handle, a null callback, a zero pid or a null out parameter was passed.
pub const PAC_E_INVALID_ARGUMENT: i32 = -1;
/// The operating system does not expose the process loopback activation path.
pub const PAC_E_UNSUPPORTED_PLATFORM: i32 = -2;
/// `ActivateAudioInterfaceAsync` failed; the target process may be gone, silent
/// or protected. Details are reported through the debugger output.
pub const PAC_E_ACTIVATION_FAILED: i32 = -3;
/// The process loopback format is not 32-bit IEEE float PCM.
pub const PAC_E_UNSUPPORTED_FORMAT: i32 = -4;
/// The audio client did not become ready within the activation timeout.
pub const PAC_E_TIMEOUT: i32 = -5;
/// A Win32/COM resource could not be created.
pub const PAC_E_INTERNAL: i32 = -6;

/// Version of the exported ABI, returned by [`pac_version`].
///
/// * 2 - capture only.
/// * 3 - adds target enumeration, capture format/state queries and the DSP
///   analyser.
pub const PAC_VERSION: u32 = 3;

/// How long [`pac_start_capture`] waits for the loopback stream to activate.
pub(crate) const ACTIVATION_TIMEOUT_MS: u32 = 10_000;

/// Called from the capture thread once per audio packet.
///
/// `samples` points to `frames * channels` interleaved 32-bit IEEE float
/// samples and is only valid for the duration of the call. Silent packets are
/// reported as zeroed samples.
pub type PacAudioCallback = unsafe extern "C" fn(
    samples: *const f32,
    frames: u32,
    channels: u16,
    sample_rate: u32,
    user_data: *mut c_void,
);

/// Opaque session handle owned by the caller.
pub struct PacCaptureHandle {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Starts capturing the audio tree of `pid`.
///
/// The call blocks until the loopback stream is activated or
/// [`ACTIVATION_TIMEOUT_MS`] elapses. On success a handle is written to
/// `out_handle` and the callback starts receiving audio packets from the
/// capture thread. On failure no handle is written and the returned code
/// describes the reason.
///
/// # Safety
///
/// `out_handle` must point to a writable pointer. `callback` must stay valid
/// until [`pac_stop_capture`] returns and must be thread safe. `user_data` is
/// passed through untouched and must be valid for the same lifetime.
#[no_mangle]
pub unsafe extern "C" fn pac_start_capture(
    pid: u32,
    callback: Option<PacAudioCallback>,
    user_data: *mut c_void,
    out_handle: *mut *mut PacCaptureHandle,
) -> i32 {
    if out_handle.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }
    unsafe { *out_handle = ptr::null_mut() };

    let Some(callback) = callback else {
        return PAC_E_INVALID_ARGUMENT;
    };
    if pid == 0 {
        return PAC_E_INVALID_ARGUMENT;
    }

    // Manual reset: the capture thread waits on these handles from several
    // places, the state must survive until the handle is closed.
    let stop = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(_) => return PAC_E_INTERNAL,
    };
    let ready = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(_) => {
            let _ = unsafe { CloseHandle(stop) };
            return PAC_E_INTERNAL;
        }
    };

    let shared = Arc::new(Shared::new(stop, ready, callback, user_data));
    let worker = thread::spawn({
        let shared = Arc::clone(&shared);
        move || capture::run(pid, &shared)
    });

    // The worker signals `ready` as soon as activation has been resolved, so a
    // failure is reported here instead of silently degrading into silence.
    let wait = unsafe { WaitForSingleObject(ready, ACTIVATION_TIMEOUT_MS) };
    if wait != WAIT_OBJECT_0 {
        let code = if wait == WAIT_TIMEOUT { PAC_E_TIMEOUT } else { PAC_E_INTERNAL };
        let _ = unsafe { SetEvent(stop) };
        let _ = worker.join();
        release(&shared);
        return code;
    }

    let code = shared.activation_code();
    if code != PAC_OK {
        let _ = unsafe { SetEvent(stop) };
        let _ = worker.join();
        release(&shared);
        return code;
    }

    let handle = Box::new(PacCaptureHandle { shared, worker: Mutex::new(Some(worker)) });
    unsafe { *out_handle = Box::into_raw(handle) };
    PAC_OK
}

/// Stops the session and waits for the capture thread to exit.
///
/// The handle is consumed and must not be used again. Passing null returns
/// [`PAC_E_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `handle` must be a pointer returned by [`pac_start_capture`] that has not
/// been passed to this function before.
#[no_mangle]
pub unsafe extern "C" fn pac_stop_capture(handle: *mut PacCaptureHandle) -> i32 {
    if handle.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }

    let handle = unsafe { Box::from_raw(handle) };
    let _ = unsafe { SetEvent(handle.shared.stop) };
    if let Some(worker) = handle.worker.lock().unwrap_or_else(|error| error.into_inner()).take() {
        let _ = worker.join();
    }
    release(&handle.shared);
    PAC_OK
}

/// Returns [`PAC_VERSION`].
#[no_mangle]
pub extern "C" fn pac_version() -> u32 {
    PAC_VERSION
}

/// Returns a static, never null description of an error code.
#[no_mangle]
pub extern "C" fn pac_strerror(code: i32) -> *const c_char {
    let text: &[u8] = match code {
        PAC_OK => b"ok\0",
        PAC_E_INVALID_ARGUMENT => b"invalid argument\0",
        PAC_E_UNSUPPORTED_PLATFORM => {
            b"process loopback requires Windows 10 version 2004 (build 19041) or newer\0"
        }
        PAC_E_ACTIVATION_FAILED => {
            b"process loopback activation failed, the target process may not be rendering audio\0"
        }
        PAC_E_UNSUPPORTED_FORMAT => b"the loopback stream is not 32-bit IEEE float PCM\0",
        PAC_E_TIMEOUT => b"the audio client did not activate in time\0",
        PAC_E_INTERNAL => b"internal error\0",
        _ => b"unknown error\0",
    };
    text.as_ptr() as *const c_char
}

fn release(shared: &Shared) {
    let _ = unsafe { CloseHandle(shared.stop) };
    let _ = unsafe { CloseHandle(shared.ready) };
}

/* ------------------------------------------------------------- targets */

/// One capturable target, as handed out by [`pac_enum_targets`].
///
/// Every `*const c_char` points into storage owned by the list it came from,
/// so it stays valid until that list is released.
#[repr(C)]
pub struct PacTarget {
    /// Process id to pass to [`pac_start_capture`].
    pub pid: u32,
    /// Window handle, or 0 when the process owns no visible window.
    pub hwnd: u64,
    /// Display title: the window title, or the media metadata when there is no
    /// window to read one from.
    pub title: *const c_char,
    /// Executable file name, e.g. `chrome.exe`.
    pub process_name: *const c_char,
    /// Full executable path, empty when it could not be queried.
    pub process_path: *const c_char,
    /// Whether the process owns an audio session.
    pub has_session: i32,
    /// One of the `PAC_SESSION_*` values.
    pub session_state: i32,
    /// Live peak of the audio session, 0.0 - 1.0+.
    pub session_peak: f32,
    /// Whether the process owns a visible top level window.
    pub has_window: i32,
    /// Track title published over SMTC, empty when there is no media session.
    pub media_title: *const c_char,
    pub media_artist: *const c_char,
    pub media_album: *const c_char,
    /// One of the `PAC_MEDIA_*` values.
    pub media_status: i32,
}

/// A snapshot of every process that can be captured, best candidates first.
///
/// Opaque by design: walk it with [`pac_target_count`] and [`pac_target_at`],
/// then release it with [`pac_free_target_list`].
pub struct PacTargetList {
    items: Vec<PacTarget>,
    /// Backing storage of every C string referenced by `items`; only kept alive
    /// here. The buffers of a `CString` do not move when the vector grows, so
    /// the pointers stay valid as long as the list lives.
    #[allow(dead_code)]
    strings: Vec<CString>,
    sessions_error: CString,
}

/// Enumerates the processes that can be captured right now.
///
/// Windows, WASAPI audio sessions and SMTC media metadata are merged by process
/// id; a process with an audio session but no window (a player in the
/// notification area) is included as well.
///
/// On [`PAC_OK`] the caller owns a list that must be released with
/// [`pac_free_target_list`]. Reading the audio session list can fail on
/// machines without a rendering endpoint - that is reported through
/// [`pac_target_list_sessions_error`], and the window list is still returned.
///
/// # Safety
///
/// `out_list` must point to a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn pac_enum_targets(out_list: *mut *mut PacTargetList) -> i32 {
    if out_list.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }
    unsafe { *out_list = ptr::null_mut() };

    let enumeration = targets::enumerate();
    let mut strings: Vec<CString> = Vec::new();
    let mut items = Vec::with_capacity(enumeration.targets.len());

    for target in &enumeration.targets {
        let media = target.media.as_ref();
        items.push(PacTarget {
            pid: target.pid,
            hwnd: target.hwnd,
            title: intern(&mut strings, &target.title),
            process_name: intern(&mut strings, &target.process_name),
            process_path: intern(&mut strings, &target.process_path),
            has_session: i32::from(target.has_session),
            session_state: target.session_state as i32,
            session_peak: target.session_peak,
            has_window: i32::from(target.has_window),
            media_title: intern(&mut strings, media.map(|m| m.title.as_str()).unwrap_or("")),
            media_artist: intern(&mut strings, media.map(|m| m.artist.as_str()).unwrap_or("")),
            media_album: intern(&mut strings, media.map(|m| m.album.as_str()).unwrap_or("")),
            // 0 is PAC_MEDIA_UNKNOWN.
            media_status: media.map(|m| m.status as i32).unwrap_or(0),
        });
    }

    let sessions_error = sanitised_cstring(enumeration.sessions_error.as_deref().unwrap_or_default());
    let list = Box::new(PacTargetList { items, strings, sessions_error });
    unsafe { *out_list = Box::into_raw(list) };
    PAC_OK
}

/// Number of targets in a list. Returns 0 for a null list.
///
/// # Safety
///
/// `list` must be a pointer returned by [`pac_enum_targets`] that has not been
/// released yet, or null.
#[no_mangle]
pub unsafe extern "C" fn pac_target_count(list: *const PacTargetList) -> usize {
    if list.is_null() {
        return 0;
    }
    unsafe { (*list).items.len() }
}

/// Borrows the target at `index`, or null when the index is out of range.
///
/// The pointer stays valid until the list is released.
///
/// # Safety
///
/// Same requirements as [`pac_target_count`].
#[no_mangle]
pub unsafe extern "C" fn pac_target_at(
    list: *const PacTargetList,
    index: usize,
) -> *const PacTarget {
    if list.is_null() {
        return ptr::null();
    }
    let list = unsafe { &*list };
    list.items.get(index).map_or(ptr::null(), |item| item as *const PacTarget)
}

/// Why the audio session list is empty, or an empty string when it was read
/// successfully.
///
/// # Safety
///
/// Same requirements as [`pac_target_count`].
#[no_mangle]
pub unsafe extern "C" fn pac_target_list_sessions_error(
    list: *const PacTargetList,
) -> *const c_char {
    if list.is_null() {
        return ptr::null();
    }
    unsafe { (*list).sessions_error.as_ptr() }
}

/// Releases a list returned by [`pac_enum_targets`]. Null is ignored.
///
/// # Safety
///
/// `list` must be a pointer returned by [`pac_enum_targets`] that has not been
/// released yet, or null.
#[no_mangle]
pub unsafe extern "C" fn pac_free_target_list(list: *mut PacTargetList) {
    if !list.is_null() {
        drop(unsafe { Box::from_raw(list) });
    }
}

/// Copies `value` into the list's string pool and returns a pointer to it.
fn intern(pool: &mut Vec<CString>, value: &str) -> *const c_char {
    let owned = sanitised_cstring(value);
    let pointer = owned.as_ptr();
    pool.push(owned);
    pointer
}

/// Builds a C string that is safe to hand to C.
fn sanitised_cstring(value: &str) -> CString {
    // A C string cannot contain a NUL. Enumeration never produces one, but a
    // lossy process name from a weird executable could.
    CString::new(value.replace('\0', "")).unwrap_or_default()
}

/* ------------------------------------------------------- capture metadata */

/// Format of a capture stream.
///
/// Process loopback always delivers 32-bit IEEE float PCM at 48 kHz in stereo;
/// the kernel constructs that format because the loopback client implements
/// neither `IAudioClient2` nor `GetMixFormat`.
#[repr(C)]
pub struct PacFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    /// 1 when samples are IEEE floats.
    pub is_float: i32,
}

/// Reports the format the stream of `handle` is delivered in, without having to
/// wait for the first callback.
///
/// # Safety
///
/// `handle` must come from [`pac_start_capture`]; `out_format` must be writable.
#[no_mangle]
pub unsafe extern "C" fn pac_capture_format(
    handle: *const PacCaptureHandle,
    out_format: *mut PacFormat,
) -> i32 {
    if handle.is_null() || out_format.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }

    unsafe {
        *out_format = PacFormat {
            sample_rate: capture::STREAM_SAMPLE_RATE,
            channels: capture::STREAM_CHANNELS,
            bits_per_sample: capture::STREAM_BITS,
            is_float: 1,
        };
    }
    PAC_OK
}

/// Whether the capture thread is still running. `0` means the session was
/// stopped, or ended on its own - see [`pac_capture_error`] for the reason.
///
/// # Safety
///
/// `handle` must come from [`pac_start_capture`] and not been stopped yet. Null
/// returns 0.
#[no_mangle]
pub unsafe extern "C" fn pac_is_capturing(handle: *const PacCaptureHandle) -> i32 {
    if handle.is_null() {
        return 0;
    }

    let handle = unsafe { &*handle };
    let worker = handle.worker.lock().unwrap_or_else(|error| error.into_inner());
    match worker.as_ref() {
        Some(worker) => i32::from(!worker.is_finished()),
        None => 0,
    }
}

/// The failure that ended the capture thread on its own.
///
/// Returns [`PAC_OK`] while the session runs and after a normal stop, so a host
/// can poll it next to [`pac_is_capturing`] to tell "stopped" from "died".
///
/// # Safety
///
/// `handle` must come from [`pac_start_capture`] and not been stopped yet.
#[no_mangle]
pub unsafe extern "C" fn pac_capture_error(handle: *const PacCaptureHandle) -> i32 {
    if handle.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }
    unsafe { (*handle).shared.outcome() }
}

/* ------------------------------------------------------------------- DSP */

/// Shape of one analysed frame.
#[repr(C)]
pub struct PacFrame {
    /// Root mean square level, 0.0 - 1.0.
    pub rms: f32,
    /// Peak level, 0.0 - 1.0.
    pub peak: f32,
    /// Floats written to the waveform buffer: `2 * PAC_WAVE_BUCKETS`.
    pub waveform_len: usize,
    /// Floats written to the spectrum buffer: `PAC_SPECTRUM_BINS`.
    pub spectrum_len: usize,
}

/// Holds the FFT plan and the scratch buffers of one analysis stream.
///
/// Create one per capture session and reuse it for every frame: recreating it
/// per frame would rebuild the FFT plan each time.
pub struct PacAnalyzer {
    inner: dsp::Analyzer,
}

/// Creates an analyser for a stream with `channels` interleaved channels.
///
/// On [`PAC_OK`] the caller owns an analyser that must be released with
/// [`pac_analyzer_destroy`].
///
/// # Safety
///
/// `out_analyzer` must point to a writable pointer.
#[no_mangle]
pub unsafe extern "C" fn pac_analyzer_create(
    sample_rate: u32,
    channels: u16,
    out_analyzer: *mut *mut PacAnalyzer,
) -> i32 {
    if out_analyzer.is_null() || channels == 0 {
        return PAC_E_INVALID_ARGUMENT;
    }
    unsafe { *out_analyzer = ptr::null_mut() };

    let analyzer = Box::new(PacAnalyzer { inner: dsp::Analyzer::new(sample_rate, channels) });
    unsafe { *out_analyzer = Box::into_raw(analyzer) };
    PAC_OK
}

/// Releases an analyser. Null is ignored.
///
/// # Safety
///
/// `analyzer` must come from [`pac_analyzer_create`].
#[no_mangle]
pub unsafe extern "C" fn pac_analyzer_destroy(analyzer: *mut PacAnalyzer) {
    if !analyzer.is_null() {
        drop(unsafe { Box::from_raw(analyzer) });
    }
}

/// Analyses `frames` interleaved frames: downmixes to mono, then writes a
/// min/max envelope and a logarithmic spectrum into the caller's buffers and
/// reports the level meters.
///
/// Buffers may be null to skip that part; `waveform_capacity` and
/// `spectrum_capacity` are float counts, and the frame reports how many floats
/// were actually written. A chunk with `frames == 0` is analysed as silence.
///
/// # Safety
///
/// `analyzer` must come from [`pac_analyzer_create`]. `samples` must point to
/// `frames * channels` readable floats unless `frames` is 0. `waveform` and
/// `spectrum` must be writable for the capacities given, or null.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn pac_analyzer_process(
    analyzer: *mut PacAnalyzer,
    samples: *const f32,
    frames: u32,
    waveform: *mut f32,
    waveform_capacity: usize,
    spectrum: *mut f32,
    spectrum_capacity: usize,
    out_frame: *mut PacFrame,
) -> i32 {
    if analyzer.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }
    let frame_count = frames as usize;
    if frame_count > 0 && samples.is_null() {
        return PAC_E_INVALID_ARGUMENT;
    }

    let analyzer = unsafe { &mut *analyzer };
    let channels = analyzer.inner.channels();

    let input: &[f32] = if frame_count == 0 {
        &[]
    } else {
        // SAFETY: the caller guarantees `frames * channels` valid samples.
        unsafe { std::slice::from_raw_parts(samples, frame_count * channels) }
    };

    let waveform_slice: &mut [f32] = if waveform.is_null() || waveform_capacity == 0 {
        &mut []
    } else {
        // SAFETY: the caller guarantees the buffer behind this pointer.
        unsafe { std::slice::from_raw_parts_mut(waveform, waveform_capacity) }
    };
    let spectrum_slice: &mut [f32] = if spectrum.is_null() || spectrum_capacity == 0 {
        &mut []
    } else {
        // SAFETY: the caller guarantees the buffer behind this pointer.
        unsafe { std::slice::from_raw_parts_mut(spectrum, spectrum_capacity) }
    };

    let levels = analyzer.inner.process(input, frame_count, waveform_slice, spectrum_slice);

    if !out_frame.is_null() {
        unsafe {
            *out_frame = PacFrame {
                rms: levels.rms,
                peak: levels.peak,
                waveform_len: waveform_slice.len().min(dsp::WAVE_BUCKETS * 2),
                spectrum_len: spectrum_slice.len().min(dsp::SPECTRUM_BINS),
            };
        }
    }

    PAC_OK
}
