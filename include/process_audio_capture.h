/*
 * ProcessAudioCapture - capture the audio a Windows process renders, and find
 * out which processes are worth capturing in the first place.
 *
 * Build the DLL with `cargo build --release`, which produces
 * `target/release/ProcessAudioCapture.dll`. Requires Windows 10 version 2004
 * (build 19041) or newer on the machine that runs the capture.
 *
 * The library covers everything a host needs to build an audio monitor:
 *
 *   * pac_enum_targets() - which processes can be captured, how loud they are
 *     right now, and what they are playing,
 *   * pac_start_capture() / pac_stop_capture() - the capture session itself,
 *   * pac_capture_format(), pac_is_capturing(), pac_capture_error() - session
 *     metadata,
 *   * pac_analyzer_*() - turn raw PCM into an envelope, a spectrum and level
 *     meters, so hosts do not need their own FFT.
 */

#ifndef PROCESS_AUDIO_CAPTURE_H
#define PROCESS_AUDIO_CAPTURE_H

#include <stddef.h>
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
/* A Win32/COM resource could not be created, or a running session failed. */
#define PAC_E_INTERNAL (-6)

/* Value returned by pac_version(). */
#define PAC_VERSION 3

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

/* ---------------------------------------------------------- target listing */

/* Session states reported in pac_target_t.session_state. */
#define PAC_SESSION_NONE 0     /* The process owns no audio session. */
#define PAC_SESSION_ACTIVE 1   /* Rendering audio right now. */
#define PAC_SESSION_INACTIVE 2 /* Has a session but is not rendering. */
#define PAC_SESSION_EXPIRED 3  /* The session is being torn down. */

/* Playback states reported in pac_target_t.media_status. */
#define PAC_MEDIA_UNKNOWN 0
#define PAC_MEDIA_PLAYING 1
#define PAC_MEDIA_PAUSED 2
#define PAC_MEDIA_STOPPED 3
#define PAC_MEDIA_CLOSED 4
#define PAC_MEDIA_CHANGING 5
#define PAC_MEDIA_OPENED 6

/*
 * One process that can be captured.
 *
 * Every string is owned by the list this target came from: it stays valid
 * until pac_free_target_list() and must not be freed individually. Empty
 * strings are used where a value is unknown - a target without a media session
 * reports "" for the media fields and PAC_MEDIA_UNKNOWN.
 */
typedef struct {
    /* Process id to pass to pac_start_capture(). */
    uint32_t pid;
    /* Top level window handle, or 0 when the process owns no visible window. */
    uint64_t hwnd;
    /* Display title: the window title, or the media metadata when the process
     * has no window (a player in the notification area). */
    const char *title;
    /* Executable file name, e.g. "chrome.exe". */
    const char *process_name;
    /* Full executable path, empty when it could not be queried. */
    const char *process_path;
    /* Non-zero when the process owns an audio session. */
    int32_t has_session;
    /* One of the PAC_SESSION_* values. */
    int32_t session_state;
    /* Live peak of the audio session, 0.0 - 1.0+. */
    float session_peak;
    /* Non-zero when the process owns a visible top level window. */
    int32_t has_window;
    /* Track title published over the System Media Transport Controls. */
    const char *media_title;
    const char *media_artist;
    const char *media_album;
    /* One of the PAC_MEDIA_* values. */
    int32_t media_status;
} pac_target_t;

/* Opaque snapshot of the capturable targets. */
typedef struct pac_target_list pac_target_list_t;

/*
 * Enumerates every process that can be captured, best candidates first.
 *
 * Visible top level windows, WASAPI audio sessions and SMTC media metadata are
 * merged by process id. A process that owns an audio session but no window is
 * included as well (hwnd = 0, has_window = 0), so a player minimised to the
 * notification area can still be captured.
 *
 * On PAC_OK the caller owns a list that must be released with
 * pac_free_target_list(). Reading the audio session list can fail on machines
 * without a rendering endpoint; that is reported through
 * pac_target_list_sessions_error(), and the window list is still returned.
 */
int32_t pac_enum_targets(pac_target_list_t **out_list);

/* Number of targets in a list. Returns 0 for NULL. */
size_t pac_target_count(const pac_target_list_t *list);

/* Borrows the target at `index`, or NULL when the index is out of range. */
const pac_target_t *pac_target_at(const pac_target_list_t *list, size_t index);

/*
 * Why the audio session list is empty, or "" when it was read successfully.
 * Never NULL for a valid list.
 */
const char *pac_target_list_sessions_error(const pac_target_list_t *list);

/* Releases a list returned by pac_enum_targets(). NULL is ignored. */
void pac_free_target_list(pac_target_list_t *list);

/* --------------------------------------------------------- capture metadata */

/* Format of a capture stream. */
typedef struct {
    uint32_t sample_rate;
    uint16_t channels;
    uint16_t bits_per_sample;
    /* Non-zero when samples are IEEE floats. */
    int32_t is_float;
} pac_format_t;

/*
 * Reports the format the stream is delivered in, without waiting for the first
 * callback. Process loopback always delivers 32-bit IEEE float PCM at 48 kHz
 * in stereo; the library constructs that format because the loopback client
 * implements neither IAudioClient2 nor GetMixFormat.
 */
int32_t pac_capture_format(const pac_capture_handle *handle, pac_format_t *out_format);

/*
 * Non-zero while the capture thread runs. 0 means the session was stopped, or
 * ended on its own - see pac_capture_error() for the reason.
 */
int32_t pac_is_capturing(const pac_capture_handle *handle);

/*
 * The failure that ended the capture thread on its own.
 *
 * Returns PAC_OK while the session runs and after a normal stop, so a host can
 * poll it next to pac_is_capturing() to tell "stopped" from "died".
 */
int32_t pac_capture_error(const pac_capture_handle *handle);

/* ------------------------------------------------------------------- DSP */

/* (min, max) pairs produced per analysis. */
#define PAC_WAVE_BUCKETS 256
/* Spectrum bars produced per analysis. */
#define PAC_SPECTRUM_BINS 128

/* Shape of one analysed frame. */
typedef struct {
    /* Root mean square level, 0.0 - 1.0. */
    float rms;
    /* Peak level, 0.0 - 1.0. */
    float peak;
    /* Floats written to the waveform buffer: 2 * PAC_WAVE_BUCKETS. */
    size_t waveform_len;
    /* Floats written to the spectrum buffer: PAC_SPECTRUM_BINS. */
    size_t spectrum_len;
} pac_frame_t;

/* Opaque analyser holding the FFT plan and its scratch buffers. */
typedef struct pac_analyzer pac_analyzer_t;

/*
 * Creates an analyser for a stream with `channels` interleaved channels.
 *
 * Create one per capture session and reuse it for every frame: recreating it
 * per frame would rebuild the FFT plan each time. On PAC_OK the caller owns an
 * analyser that must be released with pac_analyzer_destroy().
 */
int32_t pac_analyzer_create(uint32_t sample_rate,
                            uint16_t channels,
                            pac_analyzer_t **out_analyzer);

/* Releases an analyser. NULL is ignored. */
void pac_analyzer_destroy(pac_analyzer_t *analyzer);

/*
 * Analyses `frames` interleaved frames: downmixes to mono, then writes a
 * min/max envelope and a logarithmic spectrum into the caller's buffers and
 * reports the level meters.
 *
 * Either buffer may be NULL to skip that part. `waveform_capacity` and
 * `spectrum_capacity` are float counts; `out_frame` reports how many floats
 * were actually written. A chunk with `frames == 0` is analysed as silence.
 */
int32_t pac_analyzer_process(pac_analyzer_t *analyzer,
                             const float *samples,
                             uint32_t frames,
                             float *waveform,
                             size_t waveform_capacity,
                             float *spectrum,
                             size_t spectrum_capacity,
                             pac_frame_t *out_frame);

#ifdef __cplusplus
}
#endif

#endif /* PROCESS_AUDIO_CAPTURE_H */
