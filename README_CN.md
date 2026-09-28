# ProcessAudioCapture

[English](README.md) | 简体中文

Windows 动态链接库（DLL）：**找出哪些进程正在出声，并捕获其中一个进程输出到其默认播放设备的音频**，全部通过一组精简的 C ABI 对外暴露。

提供三块能力：

* **目标枚举** —— 把可见顶层窗口、WASAPI 音频会话与系统媒体信息（SMTC）按进程合并，回答「哪些进程能采集、此刻有多响、在放什么」。播放器缩进托盘、一个窗口都没有时也能识别（`has_window = 0`，标题取自媒体信息）。
* **采集** —— 基于 Windows 10 2004（build 19041）及以上版本提供的 *process loopback* 激活路径：`ActivateAudioInterfaceAsync` 配合 `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`，以带事件回调的共享 loopback 模式打开 `IAudioClient`，再通过 `IAudioCaptureClient` 循环取包。音频以交错的 32 位 IEEE float PCM 形式交给调用方。
* **可视化 DSP** —— 降混单声道、峰谷包络、Hann 窗 FFT 对数频谱、RMS / 峰值电平。宿主不必自带 FFT，也不必把几万个原始采样搬进自己的 UI 层。

## 构建

需要在 Windows 上安装 Rust 的 MSVC 工具链（Visual Studio C++ 生成工具）：

```powershell
cargo build --release
```

产物为 `target/release/ProcessAudioCapture.dll`。

## 接口

公开 ABI 定义在 `include/process_audio_capture.h`。

### 目标枚举

| 函数 | 用途 |
| --- | --- |
| `pac_enum_targets(&list)` | 枚举当前可采集的目标，按「正在出声的优先」排好序。成功时返回的列表需释放。 |
| `pac_target_count(list)` / `pac_target_at(list, i)` | 遍历列表；条目指针在列表释放前一直有效。 |
| `pac_target_list_sessions_error(list)` | 音频会话列表读取失败的原因（例如机器上没有播放设备）；此时窗口列表依然有效。 |
| `pac_free_target_list(list)` | 释放列表，其中的字符串随之失效。 |

每个 `pac_target_t` 给出：`pid`、`hwnd`（没有窗口时为 0）、`title`、`process_name` / `process_path`、
`has_session` / `session_state`（`PAC_SESSION_*`）/ `session_peak`、`has_window`，
以及 SMTC 媒体信息 `media_title` / `media_artist` / `media_album` / `media_status`（`PAC_MEDIA_*`）。
媒体信息按会话 AUMID 与进程名匹配，匹配不上时留空。

### 采集

| 函数 | 用途 |
| --- | --- |
| `pac_start_capture(pid, callback, user_data, &handle)` | 激活 loopback 流并启动采集线程。会阻塞直到激活完成或 10 秒超时。 |
| `pac_stop_capture(handle)` | 通知采集线程退出、等待其结束并释放会话。该句柄被消费，不可再次使用。 |
| `pac_capture_format(handle, &format)` | 采集流的格式。进程回环固定为 48 kHz / 立体声 / 32-bit float，所以起流后立刻就能拿到，不必等首帧回调。 |
| `pac_is_capturing(handle)` | 采集线程是否仍在运行。 |
| `pac_capture_error(handle)` | 采集线程**自行结束**时的错误码；仍在运行或正常停止时为 `PAC_OK`。与上一个配合即可区分「用户停的」和「流断了」。 |

### 可视化 DSP

| 函数 | 用途 |
| --- | --- |
| `pac_analyzer_create(sample_rate, channels, &analyzer)` | 创建分析器。内部持有 FFT 计划与临时缓冲，**整场采集复用同一个**，不要每帧重建。 |
| `pac_analyzer_process(analyzer, samples, frames, wave, wave_cap, spec, spec_cap, &frame)` | 一次完成「降混 → 峰谷包络（`PAC_WAVE_BUCKETS` 组 min/max）→ 对数频谱（`PAC_SPECTRUM_BINS` 根柱子）→ 电平表」。输出缓冲由调用方提供，传 NULL 即可跳过对应部分。 |
| `pac_analyzer_destroy(analyzer)` | 释放分析器。 |

### 其他

| 函数 | 用途 |
| --- | --- |
| `pac_version()` | 返回 `PAC_VERSION`（当前为 `3`）。 |
| `pac_strerror(code)` | 返回错误码的静态描述文本。 |

错误码：`PAC_OK`、`PAC_E_INVALID_ARGUMENT`、`PAC_E_UNSUPPORTED_PLATFORM`、`PAC_E_ACTIVATION_FAILED`、`PAC_E_UNSUPPORTED_FORMAT`、`PAC_E_TIMEOUT`、`PAC_E_INTERNAL`。

回调签名：

```c
void callback(const float *samples, uint32_t frames, uint16_t channels,
              uint32_t sample_rate, void *user_data);
```

`samples` 仅在本次调用期间有效，内容为 `frames * channels` 个交错采样。静音包会以全零采样上报。回调运行在采集线程上：必须尽快返回，且不得调用 `pac_start_capture` / `pac_stop_capture`。没有专门错误码的失败会通过 `OutputDebugStringA` 输出，因此附加到宿主进程的调试器可以看到对应的 HRESULT。

## 示例

* `samples/c/main.c` —— 用 `LoadLibraryW` 动态加载 DLL，把捕获到的音频写成 16 位 PCM 的 WAV 文件。

  ```powershell
  cl /W4 /Iinclude samples\c\main.c
  .\main.exe target\release\ProcessAudioCapture.dll <pid> out.wav
  ```

* `samples/csharp/Program.cs` —— P/Invoke 调用示例（`dotnet run --project
  samples/csharp -- <pid> out.wav`）。请把 `ProcessAudioCapture.dll` 复制到生成的可执行文件旁边，或将其所在目录加入 `PATH` 以便解析到该 DLL。

## 注意事项与限制

* 需要 Windows 10 2004（build 19041）或更高版本，更早的系统激活会失败。
* 只能捕获经正常 Windows 音频引擎播放的音频；受保护内容和独占模式音频流无法捕获。
* Process loopback 始终以该 pid 的进程树为目标（`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`），其子进程播放的音频也会被包含。
* 流格式固定为 48 kHz / 立体声 / 32 位 IEEE float：进程回环客户端既不实现 `IAudioClient2`，`GetMixFormat` 也返回 `E_NOTIMPL`，所以格式由本库自行构造并以 `pac_capture_format` 如实报出。
* 不得在采集回调里调用 `pac_start_capture`；一个句柄必须且只能停止一次。
* 目标枚举里的媒体信息（曲名 / 歌手）依赖播放器实现了 SMTC；没有实现的播放器只会给出窗口标题。
* 「有声无窗」的进程只能靠音频会话发现，因此它必须**正在出声**（或能被 SMTC 认出）才会出现在列表里。
