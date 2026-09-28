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
                ActivateAudioInterfaceAsync, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
                AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
                IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
                IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
            },
            Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        },
        System::{
            Com::{
                CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED,
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

/// Sample rate every process loopback stream is delivered in.
pub(crate) const STREAM_SAMPLE_RATE: u32 = 48_000;
/// Channel count every process loopback stream is delivered in.
pub(crate) const STREAM_CHANNELS: u16 = 2;
/// Bits per sample every process loopback stream is delivered in.
pub(crate) const STREAM_BITS: u16 = 32;

/// Upper bound for a single wait, so a missed event cannot stall the thread.
const WAIT_SLICE_MS: u32 = 200;

/// `WAVEFORMATEXTENSIBLE` format tag.
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;

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
    /// Terminal error of the capture thread: [`PAC_OK`] while it runs and after
    /// a normal stop, a failure code when streaming died on its own.
    outcome: Mutex<i32>,
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
        Self {
            stop,
            ready,
            callback,
            user_data,
            activation: Mutex::new(PAC_OK),
            outcome: Mutex::new(PAC_OK),
        }
    }

    /// Outcome of the activation phase, written before `ready` is signalled.
    pub(crate) fn activation_code(&self) -> i32 {
        *self.activation.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// How the capture thread ended: [`PAC_OK`] while it runs and after a
    /// normal stop, otherwise the failure it ran into.
    pub(crate) fn outcome(&self) -> i32 {
        *self.outcome.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn set_outcome(&self, code: i32) {
        *self.outcome.lock().unwrap_or_else(|error| error.into_inner()) = code;
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
                // The stream ended without anyone asking for it, so remember
                // it: that is how a host tells "stopped" from "died".
                shared.set_outcome(PAC_E_INTERNAL);
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
    //
    // `PROPVARIANT` 在 windows crate 里实现了 `Drop`，会调用 `PropVariantClear`。
    // 对 `VT_BLOB` 而言它会用 `CoTaskMemFree` 释放 `pBlobData`，而这里的
    // `pBlobData` 指向栈上的 `parameters` —— 释放一个栈地址会直接摧毁堆，
    // 宿主进程随即以 STATUS_HEAP_CORRUPTION (0xC0000374) 崩溃。
    // 这个 blob 是调用方自有的栈内存，没有任何东西需要释放，用 ManuallyDrop
    // 跳过一次根本不该发生的释放。
    let mut property = std::mem::ManuallyDrop::new(PROPVARIANT::default());
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

    let property_ptr: *const PROPVARIANT = &*property;
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

/// 构造进程回环需要的捕获格式。
///
/// 进程回环客户端**不实现** `IAudioClient2`（`SetClientProperties` 会返回
/// `E_NOINTERFACE`），`GetMixFormat` 也会返回 `E_NOTIMPL`，所以格式必须自己
/// 构造 —— 微软的 ApplicationLoopback 示例同样是硬编码这份格式。
/// 进程回环固定交付 32-bit IEEE float PCM。
fn capture_format() -> WAVEFORMATEXTENSIBLE {
    let block_align = STREAM_CHANNELS * STREAM_BITS / 8;

    let mut format: WAVEFORMATEXTENSIBLE = unsafe { std::mem::zeroed() };
    format.Format.wFormatTag = WAVE_FORMAT_EXTENSIBLE_TAG;
    format.Format.nChannels = STREAM_CHANNELS;
    format.Format.nSamplesPerSec = STREAM_SAMPLE_RATE;
    format.Format.nAvgBytesPerSec = STREAM_SAMPLE_RATE * block_align as u32;
    format.Format.nBlockAlign = block_align;
    format.Format.wBitsPerSample = STREAM_BITS;
    format.Format.cbSize = (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16;
    format.Samples.wValidBitsPerSample = STREAM_BITS;
    format.dwChannelMask = 0x3;
    format.SubFormat = KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    format
}

/// Configures the activated client and runs the capture loop.
fn stream(client: &IAudioClient, shared: &Shared) -> Result<()> {
    let mut format = capture_format();
    let channels = format.Format.nChannels;
    let sample_rate = format.Format.nSamplesPerSec;
    let format_ptr: *mut WAVEFORMATEX = &mut format.Format;

    // Auto reset: the audio engine signals this handle for every packet but
    // never resets it, so the capture thread owns the reset.
    let sample_event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };
    let outcome = stream_packets(client, format_ptr, sample_event, channels, sample_rate, shared);
    let _ = unsafe { CloseHandle(sample_event) };
    outcome
}

fn stream_packets(
    client: &IAudioClient,
    format_ptr: *mut WAVEFORMATEX,
    sample_event: HANDLE,
    channels: u16,
    sample_rate: u32,
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
        if let Err(error) = drain(&capture, channels, sample_rate, shared, &mut silence) {
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
    channels: u16,
    sample_rate: u32,
    shared: &Shared,
    silence: &mut Vec<f32>,
) -> Result<()> {
    loop {
        if unsafe { capture.GetNextPacketSize()? } == 0 {
            break;
        }

        let mut data: *mut u8 = ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };

        if frames > 0 {
            let count = frames as usize * channels as usize;
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
                        channels,
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
                        channels,
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
