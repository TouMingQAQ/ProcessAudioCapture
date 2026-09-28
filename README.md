# ProcessAudioCapture

English | [简体中文](README_CN.md)

Windows DLL that **finds out which processes are playing audio, and captures the
audio one of them renders to its default rendering endpoint**, exposed through a
small C ABI.

It covers three things:

* **Target listing** — visible top level windows, WASAPI audio sessions and SMTC
  media metadata merged by process id, answering "which processes can be
  captured, how loud are they right now, and what are they playing". A player
  tucked away in the notification area is recognised too (`has_window = 0`, and
  its title comes from the media metadata).
* **Capture** — built on the Windows 10 2004 (build 19041)+ *process loopback*
  activation path: `ActivateAudioInterfaceAsync` with
  `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, an `IAudioClient` opened in
  shared loopback mode with event callbacks, and an `IAudioCaptureClient` drain
  loop. Audio is handed to the caller as interleaved 32-bit IEEE float PCM.
* **Visualisation DSP** — mono downmix, min/max envelope, Hann-windowed FFT
  spectrum and level meters, so a host does not need its own FFT and does not
  have to move tens of thousands of raw samples into its UI layer.

## Build

Requires a Rust MSVC toolchain (Visual Studio C++ build tools) on Windows:

```powershell
cargo build --release
```

The artifact is `target/release/ProcessAudioCapture.dll`.

## API

The public ABI lives in `include/process_audio_capture.h`.

### Target listing

| Function | Purpose |
| --- | --- |
| `pac_enum_targets(&list)` | Lists the processes that can be captured, best candidates first. The returned list must be released. |
| `pac_target_count(list)` / `pac_target_at(list, i)` | Walks the list; entries stay valid until the list is released. |
| `pac_target_list_sessions_error(list)` | Why the audio session list is empty (no rendering endpoint, for example). The window list is still valid. |
| `pac_free_target_list(list)` | Releases the list, invalidating its strings. |

Every `pac_target_t` carries `pid`, `hwnd` (0 when there is no window), `title`,
`process_name` / `process_path`, `has_session` / `session_state`
(`PAC_SESSION_*`) / `session_peak`, `has_window`, and the SMTC media fields
`media_title` / `media_artist` / `media_album` / `media_status` (`PAC_MEDIA_*`).
Media metadata is matched by session AUMID against the process name and is left
empty when there is no match.

### Capture

| Function | Purpose |
| --- | --- |
| `pac_start_capture(pid, callback, user_data, &handle)` | Activates the loopback stream and starts the capture thread. Blocks until activation resolves or the 10 s timeout elapses. |
| `pac_stop_capture(handle)` | Signals the capture thread, joins it, releases the session. The handle is consumed. |
| `pac_capture_format(handle, &format)` | Format of the stream. Process loopback is always 48 kHz / stereo / 32-bit float, so it is available right after the stream starts instead of after the first callback. |
| `pac_is_capturing(handle)` | Whether the capture thread still runs. |
| `pac_capture_error(handle)` | The failure that ended the thread **on its own**; `PAC_OK` while it runs and after a normal stop. Together with the previous call that distinguishes "stopped" from "died". |

### Visualisation DSP

| Function | Purpose |
| --- | --- |
| `pac_analyzer_create(sample_rate, channels, &analyzer)` | Creates an analyser. It owns the FFT plan and scratch buffers, so create **one per capture session** rather than one per frame. |
| `pac_analyzer_process(analyzer, samples, frames, wave, wave_cap, spec, spec_cap, &frame)` | Downmixes, then writes a min/max envelope (`PAC_WAVE_BUCKETS` pairs) and a logarithmic spectrum (`PAC_SPECTRUM_BINS` bars) into the caller's buffers and reports the level meters. Either buffer may be NULL to skip that part. |
| `pac_analyzer_destroy(analyzer)` | Releases the analyser. |

### Miscellaneous

| Function | Purpose |
| --- | --- |
| `pac_version()` | Returns `PAC_VERSION` (currently `3`). |
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
* The stream format is fixed to 48 kHz / stereo / 32-bit IEEE float: the
  loopback client implements neither `IAudioClient2` nor `GetMixFormat`
  (`E_NOTIMPL`), so the library constructs the format itself and reports it
  through `pac_capture_format`.
* `pac_start_capture` must not be called from the capture callback, and one
  handle must be stopped exactly once.
* Media metadata (track and artist) only exists for players that publish an SMTC
  session; the rest are identified by their window title alone.
* A process that owns an audio session but no window is only discovered through
  that session, so it must actually be rendering (or be recognised by SMTC) to
  show up in the list.
