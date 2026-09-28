//! Process loopback capture implemented on top of `ActivateAudioInterfaceAsync`.
//!
//! The activation request is wrapped in a `PROPVARIANT` blob carrying an
//! `AUDIOCLIENT_ACTIVATION_PARAMS` with
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`. The activated
//! `IAudioClient` is then initialized in shared mode with the loopback and
//! event callback stream flags and drained through `IAudioCaptureClient`.

use std::{
    ffi::{c_void, CString},
    mem::size_of,
    ptr,
    sync::{Arc, Mutex},
};

use windows::{
    core::{implement, Error, HRESULT, Interface, IUnknown, PCSTR, PCWSTR, Ref, Result},
    Win32::{
        Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::{
            Audio::{
                ActivateAudioInterfaceAsync, AudioCategory_Other, AudioClientProperties,
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
                AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
                IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
                IActivateAudioInterfaceCompletionHandler_Impl,
                IAudioCaptureClient, IAudioClient, IAudioClient2,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
            },
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{
                CoInitializeEx, CoTaskMemFree, CoUninitialize, BLOB, COINIT_MULTITHREADED,
                StructuredStorage::PROPVARIANT,
            },
            Diagnostics::Debug::OutputDebugStringA,
            Threading::{
                CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
            },
            Variant::VT_BLOB,
        },
    },
};

use crate::{
    PacAudioCallback, ACTIVATION_TIMEOUT_MS, PAC_E_ACTIVATION_FAILED, PAC_E_INTERNAL,
    PAC_E_UNSUPPORTED_PLATFORM, PAC_OK,
};

/// Requested loopback buffer duration, in 100 ns units (20 ms).
const BUFFER_DURATION_HNS: i64 = 200_000;

/// Upper bound for a single wait, so a missed event cannot stall the thread.
const WAIT_SLICE_MS: u32 = 200;

/// `WAVEFORMATEXTENSIBLE` format tag.
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

/// `AUDCLNT_E_UNSUPPORTED_FORMAT`.
const AUDCLNT_E_UNSUPPORTED_FORMAT: HRESULT = HRESULT(0x88890008u32 as i32);

/// `E_FAIL`, used when COM succeeds but hands back nothing usable.
const E_FAIL: HRESULT = HRESULT(0x80004005u32 as i32);

/// `E_INVALIDARG`: builds without the process loopback activation path reject
/// the activation params with this code.
const E_INVALIDARG: HRESULT = HRESULT(0x80070057u32 as i32);

/// Session state shared by the caller thread and the capture thread.
pub(crate) struct Shared {
    /// Manual reset event set by `pac_stop_capture`.
    pub(crate) stop: HANDLE,
    /// Manual reset event set once activation has been resolved.
    pub(crate) ready: HANDLE,
    callback: PacAudioCallback,
    user_data: *mut c_void,
    activation: Mutex<i32>,
}

// The raw pointers are only handed back to the callback supplied by the caller.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    pub(crate) fn new(
        stop: HANDLE,
        ready: HANDLE,
        callback: PacAudioCallback,
        user_data: *mut c_void,
    ) -> Self {
        Self { stop, ready, callback, user_data, activation: Mutex::new(PAC_OK) }
    }

    /// Outcome of the activation phase, written before `ready` is signalled.
    pub(crate) fn activation_code(&self) -> i32 {
        *self.activation.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn report(&self, code: i32) {
        *self.activation.lock().unwrap_or_else(|error| error.into_inner()) = code;
        let _ = unsafe { SetEvent(self.ready) };
    }
}

/// Entry point of the capture thread.
pub(crate) fn run(pid: u32, shared: &Shared) {
    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let com_owned = initialized.is_ok();
    if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
        log_hresult("CoInitializeEx", initialized);
        shared.report(PAC_E_INTERNAL);
        return;
    }

    match activate(pid) {
        Ok(client) => {
            shared.report(PAC_OK);
            if let Err(error) = stream(&client, shared) {
                log_error("streaming", &error);
            }
        }
        Err(error) => {
            log_error("process loopback activation", &error);
            // Systems without the activation path reject the params outright.
            let code = if error.code() == E_INVALIDARG {
                PAC_E_UNSUPPORTED_PLATFORM
            } else {
                PAC_E_ACTIVATION_FAILED
            };
            shared.report(code);
        }
    }

    if com_owned {
        unsafe { CoUninitialize() };
    }
}

/// Completion handler that hands the activated client back to the caller.
struct ActivationState {
    client: Mutex<Option<IAudioClient>>,
    result: Mutex<HRESULT>,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    event: HANDLE,
    state: Arc<ActivationState>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> Result<()> {
        let mut status = HRESULT(0);
        let mut activated: Option<IUnknown> = None;

        if let Ok(operation) = operation.ok() {
            if let Err(error) = unsafe { operation.GetActivateResult(&mut status, &mut activated) } {
                status = error.code();
            }
        } else {
            status = E_FAIL;
        }

        if status.is_ok() {
            match activated.map(|unknown| unknown.cast::<IAudioClient>()) {
                Some(Ok(client)) => {
                    *self.state.client.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(client);
                }
                Some(Err(error)) => {
                    log_error("IAudioClient activation result", &error);
                    status = error.code();
                }
                None => status = E_FAIL,
            }
        }

        *self.state.result.lock().unwrap_or_else(|error| error.into_inner()) = status;
        // Always signal, even on failure, otherwise the caller waits for the
        // full activation timeout.
        let _ = unsafe { SetEvent(self.event) };
        Ok(())
    }
}

/// Activates a process loopback `IAudioClient` for `pid`.
fn activate(pid: u32) -> Result<IAudioClient> {
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
    let outcome = activate_with_event(pid, event);
    let _ = unsafe { CloseHandle(event) };
    outcome
}

fn activate_with_event(pid: u32, event: HANDLE) -> Result<IAudioClient> {
    let state =
        Arc::new(ActivationState { client: Mutex::new(None), result: Mutex::new(HRESULT(0)) });

    let mut parameters = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    // ActivateAudioInterfaceAsync takes the parameters as a PROPVARIANT blob.
    let mut property = PROPVARIANT::default();
    unsafe {
        let value = &mut *property.Anonymous.Anonymous;
        value.vt = VT_BLOB;
        value.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut parameters as *mut AUDIOCLIENT_ACTIVATION_PARAMS as *mut u8,
        };
    }

    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { event, state: Arc::clone(&state) }.into();

    let property_ptr: *const PROPVARIANT = &property;
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(property_ptr),
            &handler,
        )?
    };

    let wait = unsafe { WaitForSingleObject(event, ACTIVATION_TIMEOUT_MS) };
    // Keep the activation operation alive until the handler has run.
    drop(operation);
    if wait != WAIT_OBJECT_0 {
        return Err(Error::from_hresult(if wait == WAIT_TIMEOUT {
            HRESULT(0x800705B4u32 as i32) // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
        } else {
            E_FAIL
        }));
    }

    let status = *state.result.lock().unwrap_or_else(|error| error.into_inner());
    status.ok()?;

    // Bind the guard's result before returning, so the temporary does not
    // outlive `state`.
    let client = state.client.lock().unwrap_or_else(|error| error.into_inner()).take();
    client.ok_or(Error::from_hresult(E_FAIL))
}

/// Configures the activated client and runs the capture loop.
fn stream(client: &IAudioClient, shared: &Shared) -> Result<()> {
    let client2: IAudioClient2 = client.cast()?;
    let properties = AudioClientProperties {
        cbSize: size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Other,
        ..Default::default()
    };
    unsafe { client2.SetClientProperties(&properties)? };

    let format_ptr = unsafe { client.GetMixFormat()? };
    let outcome = unsafe { stream_with_format(client, format_ptr, shared) };
    unsafe { CoTaskMemFree(Some(format_ptr as *const c_void)) };
    outcome
}

unsafe fn stream_with_format(
    client: &IAudioClient,
    format_ptr: *mut WAVEFORMATEX,
    shared: &Shared,
) -> Result<()> {
    let format = unsafe { *format_ptr };
    if !is_float_format(&format) {
        return Err(Error::from_hresult(AUDCLNT_E_UNSUPPORTED_FORMAT));
    }

    // Auto reset: the audio engine signals this handle for every packet but
    // never resets it, so the capture thread owns the reset.
    let sample_event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    let outcome = stream_packets(client, &format, format_ptr, sample_event, shared);
    let _ = unsafe { CloseHandle(sample_event) };
    outcome
}

fn stream_packets(
    client: &IAudioClient,
    format: &WAVEFORMATEX,
    format_ptr: *mut WAVEFORMATEX,
    sample_event: HANDLE,
    shared: &Shared,
) -> Result<()> {
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION_HNS,
            0,
            format_ptr,
            None,
        )?;
        client.SetEventHandle(sample_event)?;
    }

    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    unsafe { client.Start()? };

    let mut silence: Vec<f32> = Vec::new();
    let handles = [shared.stop, sample_event];
    loop {
        // Index 0 is the stop event, so a pending stop always wins.
        if unsafe { WaitForMultipleObjects(&handles, false, WAIT_SLICE_MS) } == WAIT_OBJECT_0 {
            break;
        }
        if let Err(error) = drain(&capture, format, shared, &mut silence) {
            let _ = unsafe { client.Stop() };
            return Err(error);
        }
    }

    let _ = unsafe { client.Stop() };
    Ok(())
}

/// Delivers every pending packet to the user callback.
fn drain(
    capture: &IAudioCaptureClient,
    format: &WAVEFORMATEX,
    shared: &Shared,
    silence: &mut Vec<f32>,
) -> Result<()> {
    let channels = format.nChannels as usize;
    let sample_rate = format.nSamplesPerSec;

    loop {
        if unsafe { capture.GetNextPacketSize()? } == 0 {
            break;
        }

        let mut data: *mut u8 = ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };

        if frames > 0 {
            let count = frames as usize * channels;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;

            if silent || data.is_null() {
                // A silent packet carries no usable samples, report silence.
                if silence.len() < count {
                    silence.resize(count, 0.0);
                }
                unsafe {
                    (shared.callback)(
                        silence.as_ptr(),
                        frames,
                        format.nChannels,
                        sample_rate,
                        shared.user_data,
                    )
                };
            } else {
                let samples = unsafe { std::slice::from_raw_parts(data as *const f32, count) };
                unsafe {
                    (shared.callback)(
                        samples.as_ptr(),
                        frames,
                        format.nChannels,
                        sample_rate,
                        shared.user_data,
                    )
                };
            }
        }

        unsafe { capture.ReleaseBuffer(frames)? };
    }

    Ok(())
}

/// Process loopback only supports 32-bit IEEE float PCM.
fn is_float_format(format: &WAVEFORMATEX) -> bool {
    if format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16 {
        return true;
    }
    if format.wFormatTag == WAVE_FORMAT_EXTENSIBLE_TAG {
        // Both structures are packed, so read the subformat through the raw
        // pointer instead of forming a reference to a possibly unaligned field.
        let extensible = format as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE;
        let sub_format = unsafe { ptr::addr_of!((*extensible).SubFormat).read_unaligned() };
        return sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    }
    false
}

fn log_error(what: &str, error: &Error) {
    log_message(&format!("ProcessAudioCapture: {what} failed with HRESULT 0x{:08X}: {error}", error.code().0 as u32));
}

fn log_hresult(what: &str, result: HRESULT) {
    log_message(&format!("ProcessAudioCapture: {what} failed with HRESULT 0x{:08X}", result.0 as u32));
}

/// Detailed failures only surface through the debugger, the ABI stays simple.
fn log_message(message: &str) {
    if let Ok(text) = CString::new(message) {
        unsafe { OutputDebugStringA(PCSTR::from_raw(text.as_ptr() as *const u8)) };
    }
}
