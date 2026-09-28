# ProcessAudioCapture

English | [简体中文](README_CN.md)

Windows DLL that captures the audio a single process renders to its default
rendering endpoint, exposed through a small C ABI.

Capture is built on the Windows 10 2004 (build 19041)+ *process loopback*
activation path: `ActivateAudioInterfaceAsync` with
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, an `IAudioClient` opened in
shared loopback mode with event callbacks, and an `IAudioCaptureClient` drain
loop. Audio is handed to the caller as interleaved 32-bit IEEE float PCM.

## Build

Requires a Rust MSVC toolchain (Visual Studio C++ build tools) on Windows:

```powershell
cargo build --release
```

The artifact is `target/release/ProcessAudioCapture.dll`.

## API

The public ABI lives in `include/process_audio_capture.h`:

| Function | Purpose |
| --- | --- |
| `pac_start_capture(pid, callback, user_data, &handle)` | Activates the loopback stream and starts the capture thread. Blocks until activation resolves or the 10 s timeout elapses. |
| `pac_stop_capture(handle)` | Signals the capture thread, joins it, releases the session. The handle is consumed. |
| `pac_version()` | Returns `PAC_VERSION` (currently `2`). |
| `pac_strerror(code)` | Static description of an error code. |

Error codes: `PAC_OK`, `PAC_E_INVALID_ARGUMENT`, `PAC_E_UNSUPPORTED_PLATFORM`,
`PAC_E_ACTIVATION_FAILED`, `PAC_E_UNSUPPORTED_FORMAT`, `PAC_E_TIMEOUT`,
`PAC_E_INTERNAL`.

The callback signature is

```c
void callback(const float *samples, uint32_t frames, uint16_t channels,
              uint32_t sample_rate, void *user_data);
```

`samples` is valid only for the duration of the call and holds
`frames * channels` interleaved samples. Silent packets are delivered as
zeroed samples. The callback runs on the capture thread: it must return
quickly and must not call `pac_start_capture` / `pac_stop_capture`. Failures
that have no dedicated error code are reported through
`OutputDebugStringA`, so a debugger attached to the host sees the HRESULT.

## Samples

* `samples/c/main.c` — loads the DLL with `LoadLibraryW`, writes captured audio
  to a 16-bit PCM WAV file.

  ```powershell
  cl /W4 /Iinclude samples\c\main.c
  .\main.exe target\release\ProcessAudioCapture.dll <pid> out.wav
  ```

* `samples/csharp/Program.cs` — P/Invoke consumer (`dotnet run --project
  samples/csharp -- <pid> out.wav`). Copy `ProcessAudioCapture.dll` next to the
  built executable, or set `PATH` so it can be resolved.

## Notes and limitations

* Windows 10 version 2004 (build 19041) or newer is required. Older systems
  fail activation.
* Only audio rendered through the normal Windows audio engine is captured.
  Protected content and exclusive-mode streams are not capturable.
* Process loopback always targets the process tree of the pid:
  `PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`. Child processes that
  render audio are included.
* The stream format is the endpoint mix format, which process loopback
  guarantees to be 32-bit IEEE float. Anything else fails with
  `PAC_E_UNSUPPORTED_FORMAT`.
* `pac_start_capture` must not be called from the capture callback, and one
  handle must be stopped exactly once.
