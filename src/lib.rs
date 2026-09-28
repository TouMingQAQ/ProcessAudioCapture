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

use std::{
    ffi::{c_char, c_void},
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
pub const PAC_VERSION: u32 = 2;

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
