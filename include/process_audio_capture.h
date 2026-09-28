/*
 * ProcessAudioCapture - capture the audio a Windows process renders.
 *
 * Build the DLL with `cargo build --release`, which produces
 * `target/release/ProcessAudioCapture.dll`. Requires Windows 10 version 2004
 * (build 19041) or newer on the machine that runs the capture.
 */

#ifndef PROCESS_AUDIO_CAPTURE_H
#define PROCESS_AUDIO_CAPTURE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Success. */
#define PAC_OK 0
/* A null handle, a null callback, a zero pid or a null out parameter. */
#define PAC_E_INVALID_ARGUMENT (-1)
/* The running OS rejected process loopback activation, typically because it
 * predates Windows 10 version 2004 (build 19041). */
#define PAC_E_UNSUPPORTED_PLATFORM (-2)
/* Activation failed: the target process may be gone, silent or protected. */
#define PAC_E_ACTIVATION_FAILED (-3)
/* The loopback stream is not 32-bit IEEE float PCM. */
#define PAC_E_UNSUPPORTED_FORMAT (-4)
/* The audio client did not activate within the activation timeout. */
#define PAC_E_TIMEOUT (-5)
/* A Win32/COM resource could not be created. */
#define PAC_E_INTERNAL (-6)

/* Value returned by pac_version(). */
#define PAC_VERSION 2

/*
 * Called on the capture thread once per audio packet.
 *
 * `samples` points at `frames * channels` interleaved 32-bit IEEE float
 * samples and is only valid for the duration of the call. Silent packets are
 * delivered as zeroed samples, so downstream code always sees `frames`
 * samples while the session is alive. Copy anything you need to keep.
 *
 * The callback must return quickly and must not call pac_start_capture or
 * pac_stop_capture.
 */
typedef void (*pac_audio_callback)(const float *samples,
                                   uint32_t frames,
                                   uint16_t channels,
                                   uint32_t sample_rate,
                                   void *user_data);

/* Opaque session handle. */
typedef struct PacCaptureHandle pac_capture_handle;

/*
 * Starts capturing the audio tree of `pid`.
 *
 * Blocks until the stream is activated or the activation timeout (10 seconds)
 * elapses. On PAC_OK a handle is written to `out_handle`; on failure
 * `*out_handle` stays NULL and the returned code describes the reason.
 *
 * Every session must be released with pac_stop_capture().
 */
int32_t pac_start_capture(uint32_t pid,
                          pac_audio_callback callback,
                          void *user_data,
                          pac_capture_handle **out_handle);

/*
 * Stops the session and waits for the capture thread to exit. The handle is
 * consumed and must not be used again.
 */
int32_t pac_stop_capture(pac_capture_handle *handle);

/* Returns PAC_VERSION. */
uint32_t pac_version(void);

/* Returns a static, never NULL description of an error code. */
const char *pac_strerror(int32_t code);

#ifdef __cplusplus
}
#endif

#endif /* PROCESS_AUDIO_CAPTURE_H */
