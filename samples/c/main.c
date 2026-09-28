/*
 * Minimal ProcessAudioCapture consumer.
 *
 * Usage: pac_sample <path-to-ProcessAudioCapture.dll> <pid> [output.wav]
 *
 * The DLL is loaded at runtime through LoadLibraryW so the sample can be built
 * without an import library. Captured audio is written to a 16-bit PCM WAV
 * file; pass 0 to keep the audio in memory only.
 *
 * Build (MSVC):  cl /W4 /I..\..\include main.c
 * Build (MinGW): gcc -Wall -I../../include main.c -o pac_sample.exe
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "process_audio_capture.h"

typedef int32_t (*pac_start_capture_fn)(uint32_t, pac_audio_callback, void *, pac_capture_handle **);
typedef int32_t (*pac_stop_capture_fn)(pac_capture_handle *);
typedef uint32_t (*pac_version_fn)(void);
typedef const char *(*pac_strerror_fn)(int32_t);

static FILE *g_output = NULL;
static uint64_t g_frame_count = 0;
static uint16_t g_channels = 0;
static uint32_t g_sample_rate = 0;

static void write_wav_header(FILE *file, uint32_t sample_rate, uint16_t channels, uint32_t frames)
{
    uint32_t data_bytes = frames * channels * (uint32_t)sizeof(int16_t);
    uint32_t riff_bytes = 36 + data_bytes;
    uint32_t byte_rate = sample_rate * channels * (uint32_t)sizeof(int16_t);
    uint16_t block_align = (uint16_t)(channels * sizeof(int16_t));
    uint16_t bits_per_sample = 16;
    uint16_t format = 1; /* PCM */
    uint16_t channel_count = channels;

    fwrite("RIFF", 1, 4, file);
    fwrite(&riff_bytes, sizeof(riff_bytes), 1, file);
    fwrite("WAVEfmt ", 1, 8, file);
    {
        uint32_t fmt_size = 16;
        fwrite(&fmt_size, sizeof(fmt_size), 1, file);
    }
    fwrite(&format, sizeof(format), 1, file);
    fwrite(&channel_count, sizeof(channel_count), 1, file);
    fwrite(&sample_rate, sizeof(sample_rate), 1, file);
    fwrite(&byte_rate, sizeof(byte_rate), 1, file);
    fwrite(&block_align, sizeof(block_align), 1, file);
    fwrite(&bits_per_sample, sizeof(bits_per_sample), 1, file);
    fwrite("data", 1, 4, file);
    fwrite(&data_bytes, sizeof(data_bytes), 1, file);
}

/* Runs on the capture thread: convert to 16-bit PCM and append to the file. */
static void on_audio(const float *samples, uint32_t frames, uint16_t channels, uint32_t sample_rate, void *user_data)
{
    size_t count = (size_t)frames * channels;
    size_t index;
    int16_t *pcm;

    (void)user_data;
    g_frame_count += frames;

    if (g_output == NULL) {
        return;
    }

    if (g_channels == 0) {
        g_channels = channels;
        g_sample_rate = sample_rate;
    }

    pcm = (int16_t *)malloc(count * sizeof(int16_t));
    if (pcm == NULL) {
        return;
    }

    for (index = 0; index < count; ++index) {
        float clamped = samples[index];
        if (clamped > 1.0f) {
            clamped = 1.0f;
        } else if (clamped < -1.0f) {
            clamped = -1.0f;
        }
        pcm[index] = (int16_t)(clamped * 32767.0f);
    }

    fwrite(pcm, sizeof(int16_t), count, g_output);
    free(pcm);
}

static int finish_wav(FILE *file, uint32_t sample_rate, uint16_t channels)
{
    uint32_t frames = (uint32_t)(ftell(file) < 44 ? 0 : (ftell(file) - 44) / (channels * sizeof(int16_t)));

    if (fseek(file, 0, SEEK_SET) != 0) {
        return 0;
    }
    write_wav_header(file, sample_rate, channels, frames);
    return fflush(file) == 0;
}

int main(int argc, char **argv)
{
    HMODULE module;
    pac_start_capture_fn start_capture;
    pac_stop_capture_fn stop_capture;
    pac_strerror_fn strerror_fn;
    pac_capture_handle *handle = NULL;
    unsigned long pid;
    int32_t result;

    if (argc < 3) {
        fprintf(stderr, "usage: %s <path-to-ProcessAudioCapture.dll> <pid> [output.wav]\n", argv[0]);
        return 2;
    }

    pid = strtoul(argv[2], NULL, 10);
    if (pid == 0) {
        fprintf(stderr, "invalid pid: %s\n", argv[2]);
        return 2;
    }

    module = LoadLibraryW(L"ProcessAudioCapture.dll");
    if (module == NULL) {
        wchar_t wide_path[MAX_PATH];
        MultiByteToWideChar(CP_UTF8, 0, argv[1], -1, wide_path, MAX_PATH);
        module = LoadLibraryW(wide_path);
    }
    if (module == NULL) {
        fprintf(stderr, "LoadLibrary failed with error %lu\n", GetLastError());
        return 1;
    }

    start_capture = (pac_start_capture_fn)(void *)GetProcAddress(module, "pac_start_capture");
    stop_capture = (pac_stop_capture_fn)(void *)GetProcAddress(module, "pac_stop_capture");
    strerror_fn = (pac_strerror_fn)(void *)GetProcAddress(module, "pac_strerror");
    if (start_capture == NULL || stop_capture == NULL) {
        fprintf(stderr, "the DLL does not export the expected symbols\n");
        FreeLibrary(module);
        return 1;
    }

    if (argc > 3) {
        g_output = fopen(argv[3], "wb");
        if (g_output == NULL) {
            fprintf(stderr, "cannot open %s for writing\n", argv[3]);
            FreeLibrary(module);
            return 1;
        }
        /* Reserve room for the header, it is rewritten when capture stops. */
        write_wav_header(g_output, 48000, 2, 0);
    }

    result = start_capture((uint32_t)pid, on_audio, NULL, &handle);
    if (result != PAC_OK) {
        fprintf(stderr, "pac_start_capture failed: %d (%s)\n",
                result, strerror_fn != NULL ? strerror_fn(result) : "no message");
        if (g_output != NULL) {
            fclose(g_output);
        }
        FreeLibrary(module);
        return 1;
    }

    printf("capturing pid %lu, press Enter to stop...\n", pid);
    (void)getchar();

    result = stop_capture(handle);
    printf("captured %llu frames, pac_stop_capture returned %d\n",
           (unsigned long long)g_frame_count, result);

    if (g_output != NULL) {
        if (g_channels != 0 && !finish_wav(g_output, g_sample_rate, g_channels)) {
            fprintf(stderr, "failed to finalize the WAV file\n");
        }
        fclose(g_output);
    }

    FreeLibrary(module);
    return result == PAC_OK ? 0 : 1;
}
