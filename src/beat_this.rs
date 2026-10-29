//! Beats and downbeats from Beat This! (Foscarin, Schlüter and Widmer, ISMIR 2024; MIT).
//! The model reads a log-mel spectrogram at 50 frames a second and scores every frame for a
//! beat and a downbeat. This is the frontend it was trained on.

use std::f64::consts::{PI, TAU};

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

/// Rate the model's spectrogram is taken at.
pub const MODEL_RATE: u32 = 22_050;
const N_FFT: usize = 1024;
const HOP: usize = 441;
/// Spectrogram frames a second: one every 20 ms.
pub const FPS: f64 = MODEL_RATE as f64 / HOP as f64;
pub const N_MELS: usize = 128;
const F_MIN: f64 = 30.0;
const F_MAX: f64 = 11_000.0;
const LOG_MULTIPLIER: f32 = 1000.0;

/// Zero crossings the resampling low-pass reaches on either side of its centre.
const SINC_ZEROS: usize = 32;
/// Table steps between two zero crossings; taps in between are interpolated.
const SINC_STEPS: usize = 256;
/// Where the low-pass cuts, as a fraction of the lower of the two Nyquist frequencies.
const PASS_BAND: f64 = 0.92;

/// Mono at `MODEL_RATE`, the channels averaged.
pub fn to_model_rate(samples: &[f32], channels: usize, rate: u32) -> Vec<f32> {
    if channels == 0 || rate == 0 {
        return Vec::new();
    }
    let mono: Vec<f32> = samples
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    if rate == MODEL_RATE || mono.is_empty() {
        return mono;
    }
    resample(&mono, rate, MODEL_RATE)
}

/// Blackman-windowed sinc, each output's taps scaled to sum to one so a constant passes as
/// it is. Without the low-pass, a 44.1 kHz master's hi-hats would fold into the top bands.
fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    let cutoff = PASS_BAND * f64::from(from.min(to)) / 2.0;
    // Input samples from one zero crossing of the sinc to the next.
    let zero_step = f64::from(from) / (2.0 * cutoff);
    let half = SINC_ZEROS as f64 * zero_step;
    let table: Vec<f32> = (0..=SINC_ZEROS * SINC_STEPS + 1)
        .map(|i| {
            let u = i as f64 / SINC_STEPS as f64;
            let r = (u / SINC_ZEROS as f64).min(1.0);
            let window = 0.42 + 0.5 * (PI * r).cos() + 0.08 * (TAU * r).cos();
            let sinc = if i == 0 {
                1.0
            } else {
                (PI * u).sin() / (PI * u)
            };
            (sinc * window) as f32
        })
        .collect();
    let tap = |zeros: f64| -> f32 {
        let p = zeros * SINC_STEPS as f64;
        let i = p as usize;
        if i >= SINC_ZEROS * SINC_STEPS {
            return 0.0;
        }
        let frac = (p - i as f64) as f32;
        table[i] + (table[i + 1] - table[i]) * frac
    };
    let len = (x.len() as u64 * u64::from(to)).div_ceil(u64::from(from)) as usize;
    let (step, per_zero) = (f64::from(from) / f64::from(to), 1.0 / zero_step);
    (0..len)
        .map(|j| {
            let centre = j as f64 * step;
            let lo = (centre - half).ceil().max(0.0) as usize;
            let hi = ((centre + half).floor() as usize).min(x.len() - 1);
            let (mut acc, mut norm) = (0.0f32, 0.0f32);
            for (k, &v) in x.iter().enumerate().take(hi + 1).skip(lo) {
                let w = tap((k as f64 - centre).abs() * per_zero);
                acc += w * v;
                norm += w;
            }
            if norm > 0.0 {
                acc / norm
            } else {
                0.0
            }
        })
        .collect()
}

/// log(1 + 1000 x) of the 128-band Slaney mel magnitude, frame by frame, as torchaudio's
/// MelSpectrogram takes it for the model: centred, reflect-padded, periodic Hann, the
/// spectrum divided by sqrt(N_FFT). Frames are N_MELS values each, end to end.
pub fn log_mel(mono: &[f32]) -> Vec<f32> {
    let pad = N_FFT / 2;
    // Reflect padding needs more audio than it pads.
    if mono.len() <= pad {
        return Vec::new();
    }
    let n = mono.len() as isize;
    let at = |i: isize| {
        let i = i - pad as isize;
        let i = if i < 0 {
            -i
        } else if i >= n {
            2 * (n - 1) - i
        } else {
            i
        };
        mono[i as usize]
    };
    let frames = mono.len() / HOP + 1;
    let window: Vec<f32> = (0..N_FFT)
        .map(|i| (0.5 - 0.5 * (TAU * i as f64 / N_FFT as f64).cos()) as f32)
        .collect();
    let bands = mel_bands();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(N_FFT);
    let mut spectrum = vec![Complex32::new(0.0, 0.0); N_FFT];
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut mag = vec![0.0f32; N_FFT / 2 + 1];
    let scale = 1.0 / (N_FFT as f32).sqrt();
    let mut out = Vec::with_capacity(frames * N_MELS);
    for f in 0..frames {
        let start = (f * HOP) as isize;
        for (k, s) in spectrum.iter_mut().enumerate() {
            *s = Complex32::new(at(start + k as isize) * window[k], 0.0);
        }
        fft.process_with_scratch(&mut spectrum, &mut scratch);
        for (m, s) in mag.iter_mut().zip(&spectrum) {
            *m = s.norm() * scale;
        }
        out.extend(bands.iter().map(|(first, weights)| {
            let energy: f32 = weights.iter().zip(&mag[*first..]).map(|(w, m)| w * m).sum();
            (LOG_MULTIPLIER * energy).ln_1p()
        }));
    }
    out
}

fn hz_to_mel(hz: f64) -> f64 {
    let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
    if hz < min_log_hz {
        hz / f_sp
    } else {
        min_log_hz / f_sp + (hz / min_log_hz).ln() * 27.0 / 6.4f64.ln()
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
    let min_log_mel = min_log_hz / f_sp;
    if mel < min_log_mel {
        mel * f_sp
    } else {
        min_log_hz * ((mel - min_log_mel) * 6.4f64.ln() / 27.0).exp()
    }
}

/// Slaney-scale triangles from F_MIN to F_MAX, unnormalised, each as its first FFT bin and
/// the weights from there on.
fn mel_bands() -> Vec<(usize, Vec<f32>)> {
    let bins = N_FFT / 2 + 1;
    let nyquist = f64::from(MODEL_RATE / 2);
    let (lo, hi) = (hz_to_mel(F_MIN), hz_to_mel(F_MAX));
    let edges: Vec<f64> = (0..N_MELS + 2)
        .map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (N_MELS + 1) as f64))
        .collect();
    edges
        .windows(3)
        .map(|e| {
            let weight = |k: usize| {
                let hz = nyquist * k as f64 / (bins - 1) as f64;
                ((hz - e[0]) / (e[1] - e[0]))
                    .min((e[2] - hz) / (e[2] - e[1]))
                    .max(0.0)
            };
            let first = (0..bins).find(|&k| weight(k) > 0.0).unwrap_or(bins);
            let weights = (first..bins)
                .map(weight)
                .take_while(|&w| w > 0.0)
                .map(|w| w as f32)
                .collect();
            (first.min(bins - 1), weights)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f64, rate: u32, secs: f64) -> Vec<f32> {
        let n = (secs * f64::from(rate)) as usize;
        (0..n)
            .map(|i| (std::f64::consts::TAU * hz * i as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        let x = &x[x.len() / 4..x.len() * 3 / 4];
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn resampling_keeps_the_music_and_drops_what_would_alias() {
        for rate in [44_100, 48_000, 96_000] {
            let kept = to_model_rate(&sine(1_000.0, rate, 2.0), 1, rate);
            let expected = (2.0 * f64::from(MODEL_RATE)) as usize;
            assert!(
                kept.len().abs_diff(expected) <= 1,
                "{rate}: {} frames",
                kept.len()
            );
            let level = rms(&kept) * std::f32::consts::SQRT_2;
            assert!((level - 1.0).abs() < 0.01, "{rate}: 1 kHz at {level}");
            // 13 kHz would fold down to 9 kHz without a low-pass.
            let folded = rms(&to_model_rate(&sine(13_000.0, rate, 2.0), 1, rate));
            assert!(folded < 1e-3, "{rate}: 13 kHz leaks at {folded}");
        }
    }

    #[test]
    fn resampling_averages_the_channels() {
        let left = sine(440.0, 44_100, 1.0);
        let stereo: Vec<f32> = left.iter().flat_map(|&s| [s, -s]).collect();
        let mono = to_model_rate(&stereo, 2, 44_100);
        assert!(!mono.is_empty());
        assert!(mono.iter().all(|s| s.abs() < 1e-6));
        let same = to_model_rate(&sine(440.0, 22_050, 1.0), 1, 22_050);
        assert_eq!(same, sine(440.0, 22_050, 1.0));
    }

    /// Values from an independent numpy implementation of torchaudio's documented formulas.
    #[test]
    fn log_mel_matches_the_reference() {
        let n = 22_050;
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / 22_050.0;
                let mut s = 0.5 * (std::f64::consts::TAU * 220.0 * t).sin()
                    + 0.25 * (std::f64::consts::TAU * 3_000.0 * t).sin();
                if i % 11_025 < 32 {
                    s += 0.8;
                }
                s as f32
            })
            .collect();
        let mel = log_mel(&x);
        assert_eq!(mel.len(), 51 * N_MELS);
        for (frame, band, want) in [
            (0, 0, 7.775872),
            (10, 7, 8.128954),
            (10, 80, 3.982663),
            (25, 100, 5.334292),
            (26, 3, 4.265858),
            (40, 127, 0.000008),
            (50, 64, 3.667808),
            (12, 30, 0.036045),
        ] {
            let got = mel[frame * N_MELS + band];
            assert!(
                (got - want).abs() < 2e-3,
                "[{frame}, {band}] {got} != {want}"
            );
        }
    }

    #[test]
    fn log_mel_of_too_little_audio_is_empty() {
        assert!(log_mel(&[]).is_empty());
        assert!(log_mel(&[0.1; N_FFT / 2]).is_empty());
        assert_eq!(log_mel(&[0.0; N_FFT / 2 + 1]).len(), 2 * N_MELS);
    }
}
