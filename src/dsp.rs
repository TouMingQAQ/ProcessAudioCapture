//! Visualisation DSP: turns interleaved PCM into the three things a UI draws -
//! a min/max envelope, a logarithmic spectrum and level meters.
//!
//! Living here means a host does not have to ship its own FFT, and does not
//! have to push tens of thousands of raw samples through its UI layer just to
//! draw a curve.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Number of `(min, max)` pairs produced per analysis.
pub const WAVE_BUCKETS: usize = 256;
/// Number of logarithmic spectrum bars produced per analysis.
pub const SPECTRUM_BINS: usize = 128;
/// Length of the FFT window.
pub const FFT_SIZE: usize = 2048;

/// Lowest frequency a spectrum bar can represent.
const FFT_LOW_HZ: f32 = 20.0;
/// Amplitude mapped to the bottom of the spectrum scale.
const DB_FLOOR: f32 = -90.0;

/// Level meters of an analysed chunk.
#[derive(Debug, Clone, Copy, Default)]
pub struct Levels {
    /// Root mean square, 0.0 - 1.0.
    pub rms: f32,
    /// Absolute peak, 0.0 - 1.0.
    pub peak: f32,
}

/// Holds the FFT plan and the scratch buffers, so repeated calls do not
/// allocate. One instance belongs to one capture session.
pub struct Analyzer {
    channels: usize,
    sample_rate: u32,
    /// Downmixed samples of the chunk currently being analysed.
    mono: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    /// FFT bin range behind every spectrum bar.
    bin_ranges: Vec<(usize, usize)>,
}

impl Analyzer {
    /// Creates an analyser for a stream with `channels` interleaved channels.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);

        let window = (0..FFT_SIZE)
            .map(|index| {
                let x = std::f32::consts::PI * 2.0 * index as f32 / (FFT_SIZE as f32 - 1.0);
                0.5 - 0.5 * x.cos()
            })
            .collect();

        let mut analyzer = Self {
            channels: channels.max(1) as usize,
            sample_rate: 0,
            mono: Vec::new(),
            fft,
            window,
            scratch: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            bin_ranges: Vec::new(),
        };
        analyzer.set_sample_rate(sample_rate);
        analyzer
    }

    /// Interleaved channel count this analyser expects.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Recomputes the frequency to bin mapping. Call it when the stream format
    /// changes; the analyser keeps no other state that depends on it.
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;

        let rate = if sample_rate == 0 { 48_000.0 } else { sample_rate as f32 };
        let nyquist = (rate / 2.0).max(FFT_LOW_HZ * 2.0);
        let bin_hz = rate / FFT_SIZE as f32;

        // Bars are spaced logarithmically between 20 Hz and Nyquist, each one
        // covering the bins around its centre frequency.
        self.bin_ranges = (0..SPECTRUM_BINS)
            .map(|index| {
                let t = index as f32 / (SPECTRUM_BINS as f32 - 1.0);
                let frequency = FFT_LOW_HZ * (nyquist / FFT_LOW_HZ).powf(t);
                let centre = (frequency / bin_hz).round() as isize;
                let low = (centre - 1).clamp(1, (FFT_SIZE / 2 - 1) as isize) as usize;
                let high = (centre + 1).clamp(1, (FFT_SIZE / 2 - 1) as isize) as usize;
                (low.min(high), high.max(low))
            })
            .collect();
    }

    /// Analyses `frames` interleaved frames and fills the caller's buffers.
    ///
    /// `waveform` receives [`WAVE_BUCKETS`] `(min, max)` pairs and `spectrum`
    /// receives [`SPECTRUM_BINS`] normalised bars. Buffers that are too small
    /// are only partially filled - the caller can query the lengths up front
    /// through the C ABI.
    pub fn process(
        &mut self,
        interleaved: &[f32],
        frames: usize,
        waveform: &mut [f32],
        spectrum: &mut [f32],
    ) -> Levels {
        self.downmix(interleaved, frames);
        let levels = levels(&self.mono);
        envelope(&self.mono, waveform);
        self.spectrum(spectrum);
        levels
    }

    /// Averages interleaved frames into the mono buffer.
    fn downmix(&mut self, interleaved: &[f32], frames: usize) {
        let channels = self.channels;
        let usable = frames.min(interleaved.len() / channels);

        self.mono.clear();
        self.mono.reserve(usable);

        if channels == 1 {
            self.mono.extend_from_slice(&interleaved[..usable]);
            return;
        }

        let scale = 1.0 / channels as f32;
        for frame in 0..usable {
            let base = frame * channels;
            let mut sum = 0.0f32;
            for channel in 0..channels {
                sum += interleaved[base + channel];
            }
            self.mono.push(sum * scale);
        }
    }

    /// FFT of the tail of the chunk, grouped into logarithmic bars.
    fn spectrum(&mut self, out: &mut [f32]) {
        for slot in self.scratch.iter_mut() {
            *slot = Complex::new(0.0, 0.0);
        }

        let take = self.mono.len().min(FFT_SIZE);
        if take > 0 {
            let start = self.mono.len() - take;
            for index in 0..take {
                self.scratch[index] = Complex::new(self.mono[start + index] * self.window[index], 0.0);
            }
        }

        self.fft.process(&mut self.scratch);

        let norm = 2.0 / FFT_SIZE as f32;
        for (bar, &(low, high)) in self.bin_ranges.iter().enumerate() {
            if bar >= out.len() {
                break;
            }

            let mut magnitude = 0.0f32;
            for bin in low..=high {
                let value = self.scratch[bin].norm() * norm;
                if value > magnitude {
                    magnitude = value;
                }
            }

            let db = 20.0 * (magnitude + 1e-9).log10();
            out[bar] = ((db - DB_FLOOR) / -DB_FLOOR).clamp(0.0, 1.0);
        }
    }
}

/// Root mean square and peak of a mono chunk.
pub fn levels(mono: &[f32]) -> Levels {
    if mono.is_empty() {
        return Levels::default();
    }

    let mut sum_squares = 0.0f64;
    let mut peak = 0.0f32;
    for &sample in mono {
        let value = if sample.is_finite() { sample } else { 0.0 };
        sum_squares += (value as f64) * (value as f64);
        let magnitude = value.abs();
        if magnitude > peak {
            peak = magnitude;
        }
    }

    Levels {
        rms: ((sum_squares / mono.len() as f64).sqrt() as f32).min(1.0),
        peak: peak.min(1.0),
    }
}

/// Compresses a mono chunk into [`WAVE_BUCKETS`] `(min, max)` pairs.
///
/// `out` is cleared first; unfilled buckets stay at zero, which reads as a flat
/// line - the right thing to show for a chunk shorter than the bucket count.
pub fn envelope(mono: &[f32], out: &mut [f32]) {
    out.fill(0.0);
    let buckets = out.len() / 2;
    if mono.is_empty() || buckets == 0 {
        return;
    }

    let bucket = (mono.len() as f32 / buckets as f32).max(1.0);
    for index in 0..buckets {
        let start = ((index as f32) * bucket) as usize;
        if start >= mono.len() {
            break;
        }

        let end = if index == buckets - 1 {
            mono.len()
        } else {
            (((index + 1) as f32) * bucket) as usize
        };
        let end = end.min(mono.len()).max(start + 1);

        let mut min = f32::MAX;
        let mut max = f32::MIN;
        for &sample in &mono[start..end] {
            let value = if sample.is_finite() { sample } else { 0.0 };
            if value < min {
                min = value;
            }
            if value > max {
                max = value;
            }
        }
        if min == f32::MAX {
            min = 0.0;
            max = 0.0;
        }

        out[index * 2] = min;
        out[index * 2 + 1] = max;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sine_wave_lands_in_one_spectrum_bar() {
        let sample_rate = 48_000;
        let mut analyzer = Analyzer::new(sample_rate, 1);

        // 1 kHz at full scale.
        let frames = 960;
        let samples: Vec<f32> = (0..frames)
            .map(|index| {
                (std::f32::consts::TAU * 1_000.0 * index as f32 / sample_rate as f32).sin()
            })
            .collect();

        let mut waveform = vec![0.0f32; WAVE_BUCKETS * 2];
        let mut spectrum = vec![0.0f32; SPECTRUM_BINS];
        let levels = analyzer.process(&samples, frames, &mut waveform, &mut spectrum);

        assert!(levels.rms > 0.6 && levels.rms <= 1.0, "rms = {}", levels.rms);
        assert!(levels.peak > 0.99, "peak = {}", levels.peak);

        // 1 kHz sits well above the 20 Hz floor, so the loudest bar must be a
        // mid-range one, not the first.
        let loudest = spectrum
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
            .unwrap_or(0);
        assert!((20..SPECTRUM_BINS - 20).contains(&loudest), "loudest bar = {loudest}");

        // The envelope must actually contain the sine wave.
        let max = waveform.iter().copied().fold(f32::MIN, f32::max);
        assert!(max > 0.9, "envelope max = {max}");
    }

    #[test]
    fn downmixes_stereo() {
        let mut analyzer = Analyzer::new(48_000, 2);
        let frames = 4;
        // Left = 1.0, right = 0.0 → mono = 0.5.
        let mut interleaved = Vec::new();
        for _ in 0..frames {
            interleaved.push(1.0);
            interleaved.push(0.0);
        }

        let mut waveform = vec![0.0f32; WAVE_BUCKETS * 2];
        let mut spectrum = vec![0.0f32; SPECTRUM_BINS];
        let levels = analyzer.process(&interleaved, frames, &mut waveform, &mut spectrum);

        assert!((levels.rms - 0.5).abs() < 1e-6, "rms = {}", levels.rms);
        assert!((levels.peak - 0.5).abs() < 1e-6, "peak = {}", levels.peak);
    }

    #[test]
    fn silent_and_short_chunks_stay_sane() {
        let mut analyzer = Analyzer::new(48_000, 2);
        let mut waveform = vec![7.0f32; WAVE_BUCKETS * 2];
        let mut spectrum = vec![7.0f32; SPECTRUM_BINS];

        let levels = analyzer.process(&[], 0, &mut waveform, &mut spectrum);
        assert_eq!(levels.rms, 0.0);
        assert_eq!(levels.peak, 0.0);
        assert!(waveform.iter().all(|value| *value == 0.0));
        assert!(spectrum.iter().all(|value| *value >= 0.0 && *value <= 1.0));

        // A truncated final frame must not panic or read out of bounds.
        let interleaved = vec![0.25f32; 5];
        let levels = analyzer.process(&interleaved, 3, &mut waveform, &mut spectrum);
        assert!(levels.peak > 0.2);
    }
}
