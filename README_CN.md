# ProcessAudioCapture

[English](README.md) | 简体中文

Windows 动态链接库（DLL），用于捕获单个进程输出到其默认播放设备的音频，并通过一组精简的 C ABI 对外暴露。

捕获基于 Windows 10 2004（build 19041）及以上版本提供的 *process loopback* 激活路径实现：使用 `ActivateAudioInterfaceAsync` 配合 `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`，以带事件回调的共享 loopback 模式打开 `IAudioClient`，再通过 `IAudioCaptureClient` 循环取包。音频以交错的 32 位 IEEE float PCM 形式交给调用方。

## 构建

需要在 Windows 上安装 Rust 的 MSVC 工具链（Visual Studio C++ 生成工具）：

```powershell
cargo build --release
```

产物为 `target/release/ProcessAudioCapture.dll`。

## 接口

公开 ABI 定义在 `include/process_audio_capture.h`：

| 函数 | 用途 |
| --- | --- |
| `pac_start_capture(pid, callback, user_data, &handle)` | 激活 loopback 流并启动采集线程。会阻塞直到激活完成或 10 秒超时。 |
| `pac_stop_capture(handle)` | 通知采集线程退出、等待其结束并释放会话。该句柄被消费，不可再次使用。 |
| `pac_version()` | 返回 `PAC_VERSION`（当前为 `2`）。 |
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
* 流格式为设备混音格式（mix format），process loopback 保证其为 32 位 IEEE float；其他格式会以 `PAC_E_UNSUPPORTED_FORMAT` 失败。
* 不得在采集回调里调用 `pac_start_capture`；一个句柄必须且只能停止一次。
