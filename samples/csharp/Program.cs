// ProcessAudioCapture consumer sample.
//
// Copy ProcessAudioCapture.dll next to the built executable, then run:
//     dotnet run --project samples/csharp -- <pid> [output.wav]
//
// The callback runs on a native capture thread, so the sample marshals the
// interleaved 32-bit float samples to 16-bit PCM and writes them under a lock.

using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;

namespace ProcessAudioCapture.Sample;

internal static class NativeMethods
{
    internal const string Dll = "ProcessAudioCapture";

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void AudioCallback(IntPtr samples, uint frames, ushort channels, uint sampleRate, IntPtr userData);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int pac_start_capture(uint pid, AudioCallback callback, IntPtr userData, out IntPtr handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern int pac_stop_capture(IntPtr handle);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint pac_version();

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    internal static extern IntPtr pac_strerror(int code);

    internal static string Describe(int code)
    {
        IntPtr text = pac_strerror(code);
        return text == IntPtr.Zero ? $"error {code}" : Marshal.PtrToStringAnsi(text) ?? $"error {code}";
    }
}

internal sealed class WavWriter : IDisposable
{
    private const int HeaderSize = 44;

    private readonly FileStream _stream;
    private readonly object _gate = new();
    private float[] _samples = new float[4096];
    private byte[] _pcm = new byte[4096 * sizeof(short)];
    private uint _sampleRate;
    private ushort _channels;
    private uint _frameCount;

    internal WavWriter(string path)
    {
        _stream = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.Read);
        _stream.Write(new byte[HeaderSize], 0, HeaderSize);
    }

    internal void Append(IntPtr samples, uint frames, ushort channels, uint sampleRate)
    {
        lock (_gate)
        {
            _sampleRate = sampleRate;
            _channels = channels;
            _frameCount += frames;

            int count = checked((int)((long)frames * channels));
            if (_samples.Length < count)
            {
                _samples = new float[count];
                _pcm = new byte[count * sizeof(short)];
            }

            Marshal.Copy(samples, _samples, 0, count);
            for (int index = 0; index < count; index++)
            {
                float value = Math.Clamp(_samples[index], -1f, 1f);
                short pcm = (short)(value * 32767f);
                _pcm[index * 2] = (byte)(pcm & 0xFF);
                _pcm[index * 2 + 1] = (byte)((pcm >> 8) & 0xFF);
            }

            _stream.Write(_pcm, 0, count * sizeof(short));
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            _stream.Position = 0;
            using var header = new BinaryWriter(_stream, Encoding.ASCII, leaveOpen: true);
            uint dataBytes = _frameCount * _channels * sizeof(short);
            uint byteRate = _sampleRate * _channels * sizeof(short);

            header.Write(Encoding.ASCII.GetBytes("RIFF"));
            header.Write(36 + dataBytes);
            header.Write(Encoding.ASCII.GetBytes("WAVE"));
            header.Write(Encoding.ASCII.GetBytes("fmt "));
            header.Write(16);
            header.Write((ushort)1);
            header.Write(_channels);
            header.Write(_sampleRate);
            header.Write(byteRate);
            header.Write((ushort)(_channels * sizeof(short)));
            header.Write((ushort)16);
            header.Write(Encoding.ASCII.GetBytes("data"));
            header.Write(dataBytes);
            header.Flush();
        }

        _stream.Dispose();
    }
}

internal static class Program
{
    private static int Main(string[] args)
    {
        if (args.Length < 1 || !uint.TryParse(args[0], out uint pid) || pid == 0)
        {
            Console.Error.WriteLine("usage: ProcessAudioCapture.Sample <pid> [output.wav]");
            return 2;
        }

        WavWriter? writer = args.Length > 1 ? new WavWriter(args[1]) : null;

        // Keep the delegate alive for the whole session, the DLL stores the
        // raw function pointer.
        NativeMethods.AudioCallback callback = (samples, frames, channels, sampleRate, _) =>
        {
            if (writer is not null && frames > 0)
            {
                writer.Append(samples, frames, channels, sampleRate);
            }
        };

        Console.WriteLine($"ProcessAudioCapture ABI version {NativeMethods.pac_version()}");

        int result = NativeMethods.pac_start_capture(pid, callback, IntPtr.Zero, out IntPtr handle);
        if (result != 0)
        {
            Console.Error.WriteLine($"pac_start_capture failed: {result} ({NativeMethods.Describe(result)})");
            writer?.Dispose();
            return 1;
        }

        Console.WriteLine($"capturing pid {pid}, press Enter to stop...");
        Console.ReadLine();

        result = NativeMethods.pac_stop_capture(handle);
        writer?.Dispose();

        Console.WriteLine($"pac_stop_capture returned {result}");
        return result == 0 ? 0 : 1;
    }
}
