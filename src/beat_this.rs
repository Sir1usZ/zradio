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

/// Frames the model scores at once: 15 s. It was trained on 30 s excerpts, but scoring those
/// takes 740 MB of memory against 280 MB, for the same tempo and bar line on nearly every track.
pub const CHUNK_FRAMES: usize = 750;
/// Frames at either edge of a chunk the model was not trained to score; neighbouring chunks
/// overlap by this much and each keeps only its inner frames.
const BORDER_FRAMES: usize = 6;

/// Beat and downbeat times in seconds, ascending. Every downbeat is also a beat, unless the
/// model heard no beats at all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Beats {
    pub beats: Vec<f64>,
    pub downbeats: Vec<f64>,
}

/// Beat and downbeat logits for every frame of `mel`, scored `chunk` frames at a time by
/// `run`, as Beat This! itself splits a track: zero-padded at both ends, neighbours
/// overlapping by twice the border, the last chunk ending on the last frame, and each frame
/// kept from the first chunk that scores it away from its border.
pub fn score_chunked<F>(mel: &[f32], chunk: usize, mut run: F) -> (Vec<f32>, Vec<f32>)
where
    F: FnMut(&[f32]) -> (Vec<f32>, Vec<f32>),
{
    let frames = mel.len() / N_MELS;
    let (border, inner) = (
        BORDER_FRAMES as isize,
        chunk as isize - 2 * BORDER_FRAMES as isize,
    );
    let len = frames as isize;
    let mut starts: Vec<isize> = (0..)
        .map(|i| i * inner - border)
        .take_while(|&s| s < len - border)
        .collect();
    if len > inner {
        if let Some(last) = starts.last_mut() {
            *last = len - (chunk as isize - border);
        }
    }
    let (mut beat, mut downbeat) = (vec![-1000.0; frames], vec![-1000.0; frames]);
    let mut input = Vec::with_capacity(chunk * N_MELS);
    // Frames before `done` were already scored by an earlier chunk.
    let mut done = 0;
    for &start in &starts {
        let (lo, hi) = (
            start.max(0) as usize,
            ((start + chunk as isize).min(len)) as usize,
        );
        let right = (start + chunk as isize - len).clamp(0, border) as usize;
        input.clear();
        input.resize((-start).max(0) as usize * N_MELS, 0.0);
        input.extend_from_slice(&mel[lo * N_MELS..hi * N_MELS]);
        input.resize(input.len() + right * N_MELS, 0.0);
        let (b, d) = run(&input);
        let keep = BORDER_FRAMES..b.len().min(d.len()).saturating_sub(BORDER_FRAMES);
        for i in keep {
            let f = start + i as isize;
            if (done..len).contains(&f) {
                beat[f as usize] = b[i];
                downbeat[f as usize] = d[i];
                done = f + 1;
            }
        }
    }
    (beat, downbeat)
}

/// Beats and downbeats from frame logits, as Beat This!'s minimal postprocessing finds them:
/// a frame whose logit is above zero and the highest within 70 ms, adjacent frames merged
/// into their mean, and each downbeat moved onto the nearest beat.
pub fn pick(beat: &[f32], downbeat: &[f32]) -> Beats {
    let peaks = |logits: &[f32]| -> Vec<f64> {
        let half = 3;
        let frames = (0..logits.len()).filter(|&i| {
            let around = &logits[i.saturating_sub(half)..(i + half + 1).min(logits.len())];
            logits[i] > 0.0 && around.iter().all(|&v| v <= logits[i])
        });
        // Adjacent frames, each within a frame of the running mean, are one peak.
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for f in frames {
            let f = f as f64;
            match merged.last_mut() {
                Some((mean, n)) if f - *mean <= 1.0 => {
                    *n += 1.0;
                    *mean += (f - *mean) / *n;
                }
                _ => merged.push((f, 1.0)),
            }
        }
        merged.into_iter().map(|(mean, _)| mean / FPS).collect()
    };
    let beats = peaks(beat);
    let mut downbeats: Vec<f64> = peaks(downbeat)
        .into_iter()
        .map(|d| {
            beats
                .iter()
                .copied()
                .fold(None, |best: Option<f64>, b| match best {
                    Some(x) if (x - d).abs() <= (b - d).abs() => Some(x),
                    _ => Some(b),
                })
                .unwrap_or(d)
        })
        .collect();
    downbeats.sort_by(f64::total_cmp);
    downbeats.dedup();
    Beats { beats, downbeats }
}

/// The Beat This! network, loaded from its ONNX export.
#[cfg(feature = "beat-this")]
pub struct Model {
    plan: std::sync::Arc<tract_onnx::prelude::TypedRunnableModel>,
}

#[cfg(feature = "beat-this")]
impl Model {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        use tract_onnx::prelude::*;
        let model = tract_onnx::onnx().model_for_path(path)?;
        let time = model.symbols.sym("T");
        let plan = model
            .with_input_fact(
                0,
                f32::fact(&[1.to_dim(), time.to_dim(), N_MELS.to_dim()]).into(),
            )?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { plan })
    }

    /// Frame logits for one chunk of spectrogram.
    fn score(&self, chunk: &[f32]) -> anyhow::Result<(Vec<f32>, Vec<f32>)> {
        use tract_onnx::prelude::*;
        let frames = chunk.len() / N_MELS;
        let input = Tensor::from_shape(&[1, frames, N_MELS], chunk)?;
        let out = self.plan.run(tvec!(input.into()))?;
        let logits = |i: usize| -> anyhow::Result<Vec<f32>> {
            let t = out
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("model gave no output {i}"))?;
            Ok(t.to_plain_array_view::<f32>()?.iter().copied().collect())
        };
        Ok((logits(0)?, logits(1)?))
    }

    /// Beats and downbeats of mono audio at `MODEL_RATE`.
    pub fn beats(&self, mono: &[f32]) -> anyhow::Result<Beats> {
        let mut failed = None;
        let (beat, downbeat) = score_chunked(&log_mel(mono), CHUNK_FRAMES, |chunk| {
            if failed.is_some() {
                return (Vec::new(), Vec::new());
            }
            self.score(chunk).unwrap_or_else(|e| {
                failed = Some(e);
                (Vec::new(), Vec::new())
            })
        });
        match failed {
            Some(e) => Err(e),
            None => Ok(pick(&beat, &downbeat)),
        }
    }
}

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

    /// A spectrogram whose frame `f` carries `f + 1` in its first band, so a chunk shows
    /// where it was cut from and zero where it was padded.
    fn numbered(frames: usize) -> Vec<f32> {
        (0..frames * N_MELS)
            .map(|i| {
                if i % N_MELS == 0 {
                    (i / N_MELS + 1) as f32
                } else {
                    0.5
                }
            })
            .collect()
    }

    /// Scores each chunk with the chunk's number times 1000 plus the position in it as the
    /// beat, and what the chunk holds there as the downbeat.
    fn tagging_run(sizes: &mut Vec<usize>) -> impl FnMut(&[f32]) -> (Vec<f32>, Vec<f32>) + '_ {
        move |chunk: &[f32]| {
            let call = sizes.len() as f32;
            sizes.push(chunk.len() / N_MELS);
            chunk
                .chunks(N_MELS)
                .enumerate()
                .map(|(i, f)| (call * 1000.0 + i as f32, f[0]))
                .unzip()
        }
    }

    #[test]
    fn chunks_overlap_and_keep_each_frame_from_the_first_that_scores_it() {
        let mut sizes = Vec::new();
        let (beat, downbeat) = score_chunked(&numbered(50), 30, tagging_run(&mut sizes));
        // Starts at -6, 12 and 26: the last one pulled back to end on the last frame.
        assert_eq!(sizes, [30, 30, 30]);
        let expected: Vec<f32> = (0..50)
            .map(|f| match f {
                0..18 => (f + 6) as f32,
                // The second chunk keeps frames 18 to 35, the overlap with the third included.
                18..36 => (1000 + f - 12) as f32,
                _ => (2000 + f - 26) as f32,
            })
            .collect();
        assert_eq!(beat, expected);
        assert_eq!(downbeat, (1..=50).map(|f| f as f32).collect::<Vec<_>>());

        let mut sizes = Vec::new();
        let (_, downbeat) = score_chunked(&numbered(100), 30, tagging_run(&mut sizes));
        assert_eq!(sizes, [30; 6]);
        assert_eq!(downbeat, (1..=100).map(|f| f as f32).collect::<Vec<_>>());
    }

    #[test]
    fn a_track_shorter_than_a_chunk_is_scored_once() {
        let mut sizes = Vec::new();
        let (beat, downbeat) = score_chunked(&numbered(10), 30, tagging_run(&mut sizes));
        assert_eq!(sizes, [22]);
        assert_eq!(beat, (6..16).map(|f| f as f32).collect::<Vec<_>>());
        assert_eq!(downbeat, (1..=10).map(|f| f as f32).collect::<Vec<_>>());

        let mut sizes = Vec::new();
        let (beat, _) = score_chunked(&[], 30, tagging_run(&mut sizes));
        assert!(beat.is_empty() && sizes.is_empty());
    }

    #[test]
    fn beats_are_the_confident_local_peaks_and_downbeats_land_on_them() {
        let mut beat = vec![-5.0f32; 40];
        beat[5] = 2.0;
        beat[7] = 1.0; // within 70 ms of a stronger peak
        beat[12] = -0.5; // a peak, but under even odds
        beat[20] = 3.0;
        beat[21] = 3.0; // two frames tied: one beat between them
        beat[30] = 1.5;
        beat[39] = 0.8;
        let mut downbeat = vec![-5.0f32; 40];
        downbeat[6] = 1.0;
        downbeat[29] = 2.0;
        downbeat[33] = 1.0; // nearest beat is the one frame 29 already took
        let picked = pick(&beat, &downbeat);
        assert_eq!(picked.beats, [0.1, 0.41, 0.6, 0.78]);
        assert_eq!(picked.downbeats, [0.1, 0.6]);

        let mut downbeat = vec![-1.0f32; 10];
        downbeat[4] = 1.0;
        let picked = pick(&[-1.0; 10], &downbeat);
        assert!(picked.beats.is_empty());
        assert_eq!(picked.downbeats, [0.08]);
    }

    /// 120 BPM kicks from 0.3 s, every fourth one louder and brighter.
    #[cfg(feature = "beat-this")]
    fn click_track(rate: u32, secs: f64) -> Vec<f32> {
        let n = (secs * f64::from(rate)) as usize;
        let mut x = vec![0.0f32; n];
        for k in 0.. {
            let at = 0.3 + 0.5 * k as f64;
            let start = (at * f64::from(rate)) as usize;
            if start >= n {
                break;
            }
            let (amp, hz) = if k % 4 == 0 {
                (0.9, 1_800.0)
            } else {
                (0.5, 900.0)
            };
            for (i, s) in x[start..].iter_mut().take(rate as usize / 5).enumerate() {
                let t = i as f64 / f64::from(rate);
                let kick = (std::f64::consts::TAU * 55.0 * t).sin() * (-t * 25.0).exp();
                let tick = (std::f64::consts::TAU * hz * t).sin() * (-t * 120.0).exp();
                *s += (amp * (kick + 0.5 * tick)) as f32;
            }
        }
        x
    }

    /// Needs the model: ZRADIO_BEAT_THIS_MODEL=path/to/beat_this.onnx cargo test -- --ignored
    #[cfg(feature = "beat-this")]
    #[test]
    #[ignore]
    fn the_model_finds_the_beats_of_a_click_track() {
        let Some(path) = std::env::var_os("ZRADIO_BEAT_THIS_MODEL") else {
            panic!("set ZRADIO_BEAT_THIS_MODEL to the model file");
        };
        let model = Model::load(std::path::Path::new(&path)).expect("load");
        let mono = to_model_rate(&click_track(44_100, 40.0), 1, 44_100);
        let found = model.beats(&mono).expect("beats");
        let expected: Vec<f64> = (0..80).map(|k| 0.3 + 0.5 * k as f64).collect();
        let hit = |t: &f64| found.beats.iter().any(|b| (b - t).abs() <= 0.07);
        let hits = expected.iter().filter(|t| hit(t)).count();
        assert!(hits >= 76, "{hits}/80 beats found: {:?}", found.beats);
        let extra = found.beats.len().saturating_sub(hits);
        assert!(extra <= 4, "{extra} beats off the grid: {:?}", found.beats);
    }
}
