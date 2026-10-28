use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};

use crate::decode::AudioBuf;
use crate::dsp::{Biquad, CROSSOVER_HZ};
use crate::storage::write_json_atomic;

const ANALYSIS_VERSION: u32 = 6;
const ENV_HZ: f32 = 50.0;
const BPM_MIN: f32 = 70.0;
const BPM_MAX: f32 = 180.0;
/// Onset frames per second; the hop is 10 ms at any rate.
const ONSET_HZ: f64 = 100.0;
/// Magnitude compression: log(1 + gamma * |X|).
const ONSET_GAMMA: f32 = 100.0;
/// Where in its window an onset frame's flux peaks, as a fraction of the window length.
const ONSET_LEAD: f64 = 0.55;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Camelot {
    pub number: u8,
    pub minor: bool,
}

impl Camelot {
    pub fn label(self) -> String {
        format!("{}{}", self.number, if self.minor { 'A' } else { 'B' })
    }

    pub fn from_root_mode(root: u8, minor: bool) -> Self {
        let root = root % 12;
        let number = if minor {
            [5, 12, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10][root as usize]
        } else {
            [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1][root as usize]
        };
        Self { number, minor }
    }

    pub fn compatible(self, other: Self) -> bool {
        if self == other {
            return true;
        }
        if self.number == other.number {
            return true;
        }
        if self.minor == other.minor {
            let d = (i16::from(self.number) - i16::from(other.number)).unsigned_abs() as u8;
            return d == 1 || d == 11;
        }
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackAnalysis {
    pub version: u32,
    pub mtime: u64,
    pub size: u64,
    pub bpm: Option<f32>,
    pub bpm_confidence: f32,
    pub key_root: Option<u8>,
    pub key_minor: bool,
    pub key_confidence: f32,
    pub camelot: Option<Camelot>,
    pub energy: f32,
    pub mix_in: f32,
    pub mix_out: f32,
    pub first_beat: f32,
    pub duration: f32,
    /// Integrated loudness in LUFS (BS.1770); none for silence.
    #[serde(default)]
    pub loudness: Option<f32>,
    /// Sample peak as AutoMix plays the track, through its crossover, 1 at full scale.
    #[serde(default)]
    pub peak: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AnalysisCache {
    tracks: HashMap<String, TrackAnalysis>,
}

pub fn analyze_buffer(buf: &AudioBuf) -> TrackAnalysis {
    let duration = if buf.sample_rate == 0 {
        0.0
    } else {
        buf.frames() as f32 / buf.sample_rate as f32
    };
    let envelope = rms_envelope(buf, ENV_HZ);
    let (bpm, bpm_confidence, first_beat) = detect_bpm(buf);
    let (key_root, key_minor, key_confidence) = detect_key(buf);
    let camelot = key_root.map(|root| Camelot::from_root_mode(root, key_minor));
    let energy = mean_energy(&envelope);
    let first = first_beat.unwrap_or(0.0).clamp(0.0, duration);
    let mix_in = if let Some(bpm) = bpm {
        snap_to_bar(first, first, bpm).min(duration)
    } else {
        first
    };
    let mix_out = if let Some(bpm) = bpm {
        let bar = 240.0 / bpm;
        snap_to_bar((duration - bar * 8.0).max(mix_in + bar), first, bpm).min(duration)
    } else {
        (duration * 0.82).max(0.0).min(duration)
    };
    TrackAnalysis {
        version: ANALYSIS_VERSION,
        mtime: 0,
        size: 0,
        bpm,
        bpm_confidence,
        key_root,
        key_minor,
        key_confidence,
        camelot,
        energy,
        mix_in,
        mix_out,
        first_beat: first,
        duration,
        loudness: loudness(buf),
        peak: peak(buf),
    }
}

pub fn analyze_file(path: &Path, buf: &AudioBuf, cache: &mut AnalysisStore) -> TrackAnalysis {
    if let Some(hit) = cache.get(path) {
        return hit;
    }
    let mut analysis = analyze_buffer(buf);
    if let Ok(meta) = fs::metadata(path) {
        analysis.mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        analysis.size = meta.len();
    }
    cache.insert(path, analysis.clone());
    analysis
}

fn rms_envelope(buf: &AudioBuf, env_hz: f32) -> Vec<f32> {
    if buf.frames() == 0 || buf.sample_rate == 0 {
        return Vec::new();
    }
    let window = ((buf.sample_rate as f32 / env_hz).round() as usize).max(1);
    let mut out = Vec::with_capacity(buf.frames() / window + 1);
    let mut i = 0;
    while i < buf.frames() {
        let end = (i + window).min(buf.frames());
        let mut sum = 0.0;
        let mut n = 0usize;
        for frame in i..end {
            let s = buf.stereo_at(frame);
            let m = (s[0] + s[1]) * 0.5;
            sum += m * m;
            n += 1;
        }
        out.push(if n == 0 { 0.0 } else { (sum / n as f32).sqrt() });
        i = end;
    }
    out
}

/// The BS.1770 K-weighting, a high shelf then a high-pass, designed from its analogue
/// prototype so it holds at any rate.
fn k_weighting(sample_rate: u32) -> [Biquad; 2] {
    let fs = sample_rate.max(1) as f64;
    let (f0, gain_db, q) = (
        1_681.974_450_955_533,
        3.999_843_853_973_347,
        0.707_175_236_955_419_6,
    );
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new(
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (std::f64::consts::PI * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad::new(
        [1.0, -2.0, 1.0],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    [shelf, high_pass]
}

/// Gated integrated loudness of the stereo mix (BS.1770-4): 400 ms blocks every 100 ms, an
/// absolute gate at -70 LUFS and a relative one 10 LU under what passes it.
fn loudness(buf: &AudioBuf) -> Option<f32> {
    let step = (buf.sample_rate as usize / 10).max(1);
    let mut filters = [k_weighting(buf.sample_rate); 2];
    // Summed mean square of both channels over each 100 ms step.
    let mut steps = Vec::with_capacity(buf.frames() / step + 1);
    let mut acc = 0.0f64;
    for i in 0..buf.frames() {
        let x = buf.stereo_at(i);
        for ([shelf, high_pass], s) in filters.iter_mut().zip(x) {
            let y = high_pass.process(shelf.process(f64::from(s)));
            acc += y * y;
        }
        if (i + 1) % step == 0 {
            steps.push(acc / step as f64);
            acc = 0.0;
        }
    }
    let blocks: Vec<f64> = steps
        .windows(4)
        .map(|w| w.iter().sum::<f64>() / 4.0)
        .collect();
    let lufs = |power: f64| -0.691 + 10.0 * power.log10();
    let mean_over = |floor: f64| {
        let kept: Vec<f64> = blocks
            .iter()
            .copied()
            .filter(|&p| lufs(p) > floor)
            .collect();
        (!kept.is_empty()).then(|| kept.iter().sum::<f64>() / kept.len() as f64)
    };
    let ungated = mean_over(-70.0)?;
    let gated = mean_over((lufs(ungated) - 10.0).max(-70.0))?;
    Some(lufs(gated) as f32)
}

/// The crossover only turns the phase of the bass, but on a limited master that alone lifts
/// the peaks by several dB.
fn peak(buf: &AudioBuf) -> f32 {
    let mut sums = [Biquad::crossover_sum(buf.sample_rate, CROSSOVER_HZ); 2];
    let mut peak = 0.0f64;
    for i in 0..buf.frames() {
        for (sum, s) in sums.iter_mut().zip(buf.stereo_at(i)) {
            peak = peak.max(sum.process(f64::from(s)).abs());
        }
    }
    peak as f32
}

/// Onset strength: spectral flux at ONSET_HZ, the low, mid and high bands weighted alike.
struct Onsets {
    fps: f64,
    /// Seconds from a frame's start to the onset it reports.
    lead: f64,
    strength: Vec<f32>,
}

fn onset_strength(buf: &AudioBuf) -> Option<Onsets> {
    let sr = buf.sample_rate as usize;
    let hop = (sr as f64 / ONSET_HZ).round() as usize;
    if hop < 16 {
        return None;
    }
    let n = (2 * hop).next_power_of_two();
    let frames = buf.frames().checked_sub(n)? / hop + 1;
    let bin = |hz: f64| ((hz * n as f64 / sr as f64).round() as usize).clamp(1, n / 2);
    let edges = [bin(30.0), bin(200.0), bin(2_500.0), bin(11_000.0)];
    let window: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
        .collect();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    let mut spectrum = vec![Complex32::new(0.0, 0.0); n];
    let mut scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    // Log-compressed magnitude, 1 for a full-scale sine.
    let gain = 4.0 / n as f32;
    let mut prev = vec![0.0f32; edges[3]];
    let mut mag = vec![0.0f32; edges[3]];
    let mut flux = vec![[0.0f32; 3]; frames];
    for (i, f) in flux.iter_mut().enumerate() {
        for (k, s) in spectrum.iter_mut().enumerate() {
            let x = buf.stereo_at(i * hop + k);
            *s = Complex32::new((x[0] + x[1]) * 0.5 * window[k], 0.0);
        }
        fft.process_with_scratch(&mut spectrum, &mut scratch);
        for (k, m) in mag.iter_mut().enumerate().skip(edges[0]) {
            *m = (1.0 + ONSET_GAMMA * gain * spectrum[k].norm()).ln();
        }
        if i > 0 {
            for (band, lo_hi) in edges.windows(2).enumerate() {
                f[band] = (lo_hi[0]..lo_hi[1])
                    .map(|k| (mag[k] - prev[k]).max(0.0))
                    .sum();
            }
        }
        std::mem::swap(&mut prev, &mut mag);
    }
    let mut mean = [0.0f32; 3];
    for f in &flux {
        for (m, v) in mean.iter_mut().zip(f) {
            *m += v / frames as f32;
        }
    }
    let raw: Vec<f32> = flux
        .iter()
        .map(|f| {
            (0..3)
                .filter(|&b| mean[b] > 1e-9)
                .map(|b| f[b] / mean[b])
                .sum()
        })
        .collect();
    // Only what stands out of its surroundings is an onset.
    let half = (ONSET_HZ * 0.25) as usize;
    let mut sums = vec![0.0f64; frames + 1];
    for (i, v) in raw.iter().enumerate() {
        sums[i + 1] = sums[i] + f64::from(*v);
    }
    let strength = (0..frames)
        .map(|i| {
            let (lo, hi) = (i.saturating_sub(half), (i + half + 1).min(frames));
            let local = (sums[hi] - sums[lo]) / (hi - lo) as f64;
            (raw[i] - local as f32).max(0.0)
        })
        .collect();
    Some(Onsets {
        fps: sr as f64 / hop as f64,
        lead: ONSET_LEAD * n as f64 / sr as f64,
        strength,
    })
}

/// Tempo and beat phase from the onset strength, or no tempo when the track has no steady
/// pulse to lock to.
fn detect_bpm(buf: &AudioBuf) -> (Option<f32>, f32, Option<f32>) {
    let Some(onsets) = onset_strength(buf) else {
        return (None, 0.0, None);
    };
    let Some((period, confidence)) = beat_period(&onsets.strength, onsets.fps) else {
        return (None, 0.0, None);
    };
    let (period, phase) = fit_grid(&onsets.strength, period);
    let beat = period / onsets.fps;
    let first = (phase / onsets.fps + onsets.lead).rem_euclid(beat);
    (Some((60.0 / beat) as f32), confidence, Some(first as f32))
}

/// Beat period in onset frames. Every autocorrelation peak in the tempo range is a
/// candidate, pinned down by its peaks at 2, 4, 8, ... beats, since a lag of k beats carries k
/// times the precision. The candidate whose bars and half bars repeat as well as its beats
/// wins, so a figure every 1.5 or 0.75 beats is not taken for the beat.
fn beat_period(strength: &[f32], fps: f64) -> Option<(f64, f32)> {
    let mean = strength.iter().map(|&v| f64::from(v)).sum::<f64>() / strength.len().max(1) as f64;
    let x: Vec<f64> = strength.iter().map(|&v| f64::from(v) - mean).collect();
    let acf = |lag: usize| -> f64 {
        let n = x.len().saturating_sub(lag);
        if n == 0 {
            return 0.0;
        }
        x[..n]
            .iter()
            .zip(&x[lag..])
            .map(|(a, b)| a * b)
            .sum::<f64>()
            / n as f64
    };
    let min_lag = ((60.0 * fps / BPM_MAX as f64).floor() as usize).max(2);
    let max_lag = (60.0 * fps / BPM_MIN as f64).ceil() as usize;
    if x.len() < max_lag * 4 {
        return None;
    }
    let zero = acf(0);
    if zero <= 1e-12 {
        return None;
    }
    let half = x.len() as f64 / 2.0;
    let refine = |lag: usize| {
        let base = acf(lag);
        let mut period = lag as f64 + vertex(acf(lag - 1), base, acf(lag + 1));
        let mut k = 2;
        while (k as f64 * period) < half {
            let center = (k as f64 * period).round() as usize;
            let near: Vec<f64> = (center - 3..=center + 3).map(acf).collect();
            let i = (0..near.len())
                .max_by(|&i, &j| near[i].total_cmp(&near[j]))
                .unwrap_or(3);
            if i == 0 || i == near.len() - 1 || near[i] < 0.3 * base {
                break;
            }
            let peak = (center - 3 + i) as f64 + vertex(near[i - 1], near[i], near[i + 1]);
            period = peak / k as f64;
            k *= 2;
        }
        period
    };
    // How well beats, half bars, bars and pairs of bars repeat.
    let metre = |period: f64| {
        let near = |lag: f64| {
            let c = lag.round() as usize;
            (c - 2..=c + 2).map(acf).fold(f64::MIN, f64::max)
        };
        let lags: Vec<f64> = [1.0, 2.0, 4.0, 8.0]
            .iter()
            .map(|k| k * period)
            .filter(|&lag| lag < half)
            .collect();
        lags.iter().map(|&lag| near(lag)).sum::<f64>() / lags.len() as f64
    };
    // A mild preference for tempos near 120 settles half and double time.
    let prior = |period: f64| {
        let octaves = (60.0 * fps / period / 120.0).log2();
        (-0.5 * octaves.powi(2)).exp()
    };
    let scores: Vec<f64> = (min_lag - 1..=max_lag + 1).map(acf).collect();
    let (lag, period) = (1..scores.len() - 1)
        .filter(|&i| scores[i] > 0.0 && scores[i] >= scores[i - 1] && scores[i] >= scores[i + 1])
        .map(|i| {
            let lag = min_lag - 1 + i;
            (lag, refine(lag))
        })
        .max_by(|a, b| (metre(a.1) * prior(a.1)).total_cmp(&(metre(b.1) * prior(b.1))))?;
    Some((period, (acf(lag) / zero).clamp(0.0, 1.0) as f32))
}

/// Offset of a parabola's vertex through three equally spaced points, from the middle one.
fn vertex(a: f64, b: f64, c: f64) -> f64 {
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-15 {
        0.0
    } else {
        (0.5 * (a - c) / d).clamp(-0.5, 0.5)
    }
}

/// Beat phase in onset frames, and the period refined by a weighted line through the beats:
/// each beat's onset centroid against its index.
fn fit_grid(strength: &[f32], period: f64) -> (f64, f64) {
    let len = strength.len() as f64;
    let comb = |phase: f64| {
        let (mut sum, mut n) = (0.0, 0usize);
        let mut t = phase;
        while t < len - 1.0 {
            sum += lerp(strength, t);
            n += 1;
            t = phase + n as f64 * period;
        }
        sum / n.max(1) as f64
    };
    let mut phase = (0..(period * 4.0) as usize)
        .map(|i| i as f64 * 0.25)
        .max_by(|&a, &b| comb(a).total_cmp(&comb(b)))
        .unwrap_or(0.0);
    let mut period = period;
    for _ in 0..3 {
        let reach = period * 0.1;
        let (mut sw, mut sn, mut sc, mut snn, mut snc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut beat = 0usize;
        loop {
            let at = phase + beat as f64 * period;
            let (lo, hi) = ((at - reach).ceil(), (at + reach).floor());
            if hi >= len {
                break;
            }
            if lo >= 0.0 {
                let (lo, hi) = (lo as usize, hi as usize);
                let (mut w, mut m) = (0.0, 0.0);
                for (i, &v) in strength[lo..=hi].iter().enumerate() {
                    let v = f64::from(v);
                    w += v;
                    m += v * (lo + i) as f64;
                }
                if w > 0.0 {
                    let (n, c) = (beat as f64, m / w);
                    sw += w;
                    sn += w * n;
                    sc += w * c;
                    snn += w * n * n;
                    snc += w * n * c;
                }
            }
            beat += 1;
        }
        let det = sw * snn - sn * sn;
        if det <= 1e-9 * sw * sw {
            break;
        }
        let slope = (sw * snc - sn * sc) / det;
        if (slope / period - 1.0).abs() > 0.01 {
            break;
        }
        phase = (sc - slope * sn) / sw;
        period = slope;
    }
    (period, phase.rem_euclid(period))
}

fn lerp(x: &[f32], t: f64) -> f64 {
    let i = t.floor() as usize;
    let f = t - i as f64;
    let a = x.get(i).copied().unwrap_or(0.0) as f64;
    let b = x.get(i + 1).copied().unwrap_or(0.0) as f64;
    a + (b - a) * f
}

fn detect_key(buf: &AudioBuf) -> (Option<u8>, bool, f32) {
    const FRAME: usize = 4096;
    const STEP: usize = 2048;
    if buf.frames() < FRAME {
        return (None, false, 0.0);
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME);
    let mut spectrum = vec![Complex32::new(0.0, 0.0); FRAME];
    let window: Vec<f32> = (0..FRAME)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FRAME - 1) as f32).cos())
        .collect();
    let mut chroma = [0.0f32; 12];
    let sr = buf.sample_rate as f32;
    let mut frame_i = 0;
    let limit = buf.frames().min(buf.sample_rate as usize * 45);
    while frame_i + FRAME <= limit {
        for n in 0..FRAME {
            let s = buf.stereo_at(frame_i + n);
            spectrum[n] = Complex32::new((s[0] + s[1]) * 0.5 * window[n], 0.0);
        }
        fft.process(&mut spectrum);
        for (bin, bin_val) in spectrum.iter().enumerate().take(FRAME / 2).skip(1) {
            let hz = bin as f32 * sr / FRAME as f32;
            if !(55.0..=5000.0).contains(&hz) {
                continue;
            }
            let midi = 69.0 + 12.0 * (hz / 440.0).log2();
            let pc = ((midi.round() as i32).rem_euclid(12)) as usize;
            chroma[pc] += bin_val.norm_sqr();
        }
        frame_i += STEP;
    }
    let norm = chroma.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= 1e-8 {
        return (None, false, 0.0);
    }
    for c in &mut chroma {
        *c /= norm;
    }
    let major = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];
    let minor = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];
    let mut best = f32::MIN;
    let mut root = 0u8;
    let mut is_minor = false;
    for r in 0..12 {
        let mut s_maj = 0.0;
        let mut s_min = 0.0;
        for (i, &c) in chroma.iter().enumerate() {
            let idx = (i + 12 - r) % 12;
            s_maj += c * major[idx];
            s_min += c * minor[idx];
        }
        if s_maj > best {
            best = s_maj;
            root = r as u8;
            is_minor = false;
        }
        if s_min > best {
            best = s_min;
            root = r as u8;
            is_minor = true;
        }
    }
    (Some(root), is_minor, best.clamp(0.0, 1.0))
}

pub fn snap_to_bar(time: f32, first_beat: f32, bpm: f32) -> f32 {
    if bpm <= 1.0 {
        return time.max(0.0);
    }
    let bar = 240.0 / bpm;
    if bar <= 0.001 {
        return time.max(0.0);
    }
    let units = ((time - first_beat) / bar).round();
    (units * bar + first_beat).max(0.0)
}

fn mean_energy(envelope: &[f32]) -> f32 {
    if envelope.is_empty() {
        return 0.0;
    }
    envelope.iter().sum::<f32>() / envelope.len() as f32
}

pub struct AnalysisStore {
    path: PathBuf,
    cache: AnalysisCache,
    dirty: bool,
}

impl AnalysisStore {
    pub fn load() -> Self {
        let path = cache_path();
        let cache = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            cache,
            dirty: false,
        }
    }

    pub fn get(&self, track: &Path) -> Option<TrackAnalysis> {
        let key = track.to_string_lossy().into_owned();
        let hit = self.cache.tracks.get(&key)?.clone();
        let meta = fs::metadata(track).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if hit.version == ANALYSIS_VERSION && hit.mtime == mtime && hit.size == meta.len() {
            Some(hit)
        } else {
            None
        }
    }

    pub fn insert(&mut self, track: &Path, analysis: TrackAnalysis) {
        self.cache
            .tracks
            .insert(track.to_string_lossy().into_owned(), analysis);
        self.dirty = true;
    }

    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        if write_json_atomic(&self.path, &self.cache).is_ok() {
            self.dirty = false;
        }
    }
}

impl Drop for AnalysisStore {
    fn drop(&mut self) {
        self.flush();
    }
}

fn cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("analysis.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_flush_keeps_analysis_dirty_for_retry() {
        let path = std::env::temp_dir().join(format!(
            "zradio-analysis-failed-flush-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        let mut store = AnalysisStore {
            path: path.clone(),
            cache: AnalysisCache::default(),
            dirty: true,
        };

        store.flush();

        assert!(store.dirty);
        drop(store);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn camelot_maps_c_major_and_a_minor() {
        assert_eq!(
            Camelot::from_root_mode(0, false),
            Camelot {
                number: 8,
                minor: false
            }
        );
        assert_eq!(
            Camelot::from_root_mode(9, true),
            Camelot {
                number: 8,
                minor: true
            }
        );
        assert!(Camelot {
            number: 8,
            minor: false
        }
        .compatible(Camelot {
            number: 8,
            minor: true
        }));
        assert!(Camelot {
            number: 8,
            minor: false
        }
        .compatible(Camelot {
            number: 9,
            minor: false
        }));
        assert!(!Camelot {
            number: 8,
            minor: false
        }
        .compatible(Camelot {
            number: 3,
            minor: true
        }));
    }

    fn pulse_track(bpm: f32, seconds: f32, rate: u32) -> AudioBuf {
        let frames = (seconds * rate as f32) as usize;
        let period = (60.0 / bpm * rate as f32).round() as usize;
        let mut samples = vec![0.0; frames * 2];
        let mut t = 0;
        while t < frames {
            for n in 0..200.min(frames - t) {
                let s = (1.0 - n as f32 / 200.0) * (n as f32 * 0.4).sin();
                samples[(t + n) * 2] = s;
                samples[(t + n) * 2 + 1] = s;
            }
            t += period;
        }
        AudioBuf {
            samples,
            sample_rate: rate,
            channels: 2,
        }
    }

    #[test]
    fn snap_to_bar_lands_on_grid() {
        let snapped = snap_to_bar(17.3, 0.2, 120.0);
        let bar = 2.0;
        let units = ((snapped - 0.2) / bar).round();
        assert!((snapped - (units * bar + 0.2)).abs() < 1e-4);
    }

    #[derive(Clone, Copy)]
    enum Hit {
        Kick,
        Snare,
        Hat,
        Stab,
    }

    /// Hits at offsets in beats.
    type Pattern = [(f64, Hit)];

    /// A kick on every beat from `first` seconds on and, with `hats`, a noise hat on every
    /// off-beat.
    fn beat_track(bpm: f64, first: f64, secs: f64, rate: u32, hats: bool) -> AudioBuf {
        let pattern: &Pattern = if hats {
            &[(0.0, Hit::Kick), (0.5, Hit::Hat)]
        } else {
            &[(0.0, Hit::Kick)]
        };
        drum_track(bpm, first, secs, rate, pattern, 1.0)
    }

    /// `pattern` repeated every `cycle` beats from `first` seconds on.
    fn drum_track(
        bpm: f64,
        first: f64,
        secs: f64,
        rate: u32,
        pattern: &Pattern,
        cycle: f64,
    ) -> AudioBuf {
        let sr = rate as f64;
        let frames = (secs * sr) as usize;
        let beat = 60.0 / bpm;
        let mut mono = vec![0.0f32; frames];
        let mut seed = 0x2545_f491_u32;
        let mut noise = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 * 2.0 - 1.0
        };
        let mut add = |at: f64, len: f64, f: &mut dyn FnMut(f64) -> f32| {
            let start = (at * sr).round() as usize;
            for i in 0..(len * sr) as usize {
                let Some(s) = mono.get_mut(start + i) else {
                    break;
                };
                *s += f(i as f64 / sr);
            }
        };
        let tau = std::f64::consts::TAU;
        for n in 0.. {
            let start = first + n as f64 * cycle * beat;
            if start >= secs {
                break;
            }
            for &(offset, hit) in pattern {
                let t = start + offset * beat;
                match hit {
                    Hit::Kick => {
                        let mut phase = 0.0;
                        add(t, 0.15, &mut |x| {
                            phase += tau * (50.0 + 90.0 * (-x / 0.02).exp()) / sr;
                            (0.8 * (-x / 0.06).exp() * phase.sin()) as f32
                                + 0.3 * (-x / 0.002).exp() as f32 * noise()
                        });
                    }
                    Hit::Snare => add(t, 0.2, &mut |x| {
                        (0.3 * (-x / 0.04).exp() * (tau * 190.0 * x).sin()) as f32
                            + 0.4 * (-x / 0.05).exp() as f32 * noise()
                    }),
                    Hit::Hat => add(t, 0.04, &mut |x| 0.2 * (-x / 0.01).exp() as f32 * noise()),
                    Hit::Stab => add(t, 0.15, &mut |x| {
                        let chord: f64 = [220.0, 277.2, 329.6, 440.0]
                            .iter()
                            .map(|f| (tau * f * x).sin())
                            .sum();
                        (0.15 * (-x / 0.05).exp() * chord) as f32
                    }),
                }
            }
        }
        AudioBuf {
            samples: mono.iter().flat_map(|&s| [s, s]).collect(),
            sample_rate: rate,
            channels: 2,
        }
    }

    /// Furthest any true beat lies from the analysed grid.
    fn grid_error(a: &TrackAnalysis, bpm: f64, first: f64, secs: f64) -> f64 {
        let beat = 60.0 / a.bpm.expect("bpm") as f64;
        let grid = a.first_beat as f64;
        (0..)
            .map(|n| first + n as f64 * 60.0 / bpm)
            .take_while(|&t| t < secs)
            .map(|t| {
                let near = grid + ((t - grid) / beat).round() * beat;
                (near - t).abs()
            })
            .fold(0.0, f64::max)
    }

    #[test]
    fn tempo_and_beat_grid_hold_to_the_end_of_the_track() {
        let secs = 90.0;
        for (bpm, first, hats) in [(128.0, 0.31, true), (123.7, 0.05, false), (87.0, 0.6, true)] {
            let a = analyze_buffer(&beat_track(bpm, first, secs, 22_050, hats));
            let got = a.bpm.expect("bpm") as f64;
            assert!((got - bpm).abs() < 0.01, "{bpm} BPM read as {got}");
            let err = grid_error(&a, bpm, first, secs);
            assert!(err < 0.008, "{bpm} BPM grid is {:.1} ms off", err * 1e3);
        }
    }

    #[test]
    fn dotted_figures_do_not_set_the_tempo() {
        use Hit::*;
        // Accents 1.5 or 0.75 beats apart correlate more strongly than the beat itself: a
        // dotted kick under a snare on 3, and a 3+3+2 figure in every band over the kick.
        let cases: [(f64, &Pattern, f64); 2] = [
            (
                160.0,
                &[
                    (0.0, Kick),
                    (0.0, Stab),
                    (1.5, Kick),
                    (1.5, Stab),
                    (2.0, Snare),
                    (3.0, Kick),
                    (3.0, Stab),
                ],
                4.0,
            ),
            (
                95.0,
                &[
                    (0.0, Kick),
                    (0.0, Stab),
                    (0.0, Snare),
                    (0.75, Stab),
                    (0.75, Snare),
                    (1.0, Kick),
                    (1.5, Stab),
                    (1.5, Snare),
                ],
                2.0,
            ),
        ];
        for (bpm, pattern, cycle) in cases {
            let a = analyze_buffer(&drum_track(bpm, 0.2, 90.0, 22_050, pattern, cycle));
            let got = a.bpm.expect("bpm") as f64;
            // Half or double time still puts the grid on beats; 2/3 or 4/3 does not.
            let octaves = (got / bpm).log2();
            assert!(
                (octaves - octaves.round()).abs() < 0.01,
                "{bpm} BPM read as {got}"
            );
            let beat = 60.0 / bpm;
            let off = ((a.first_beat as f64 - 0.2) / beat).rem_euclid(1.0);
            let off = off.min(1.0 - off) * beat;
            assert!(
                off < 0.01,
                "{bpm} BPM grid sits {:.0} ms off the beat",
                off * 1e3
            );
        }
    }

    /// A stereo 1 kHz sine in `(seconds, dBFS)` segments; it reads as its level in LUFS.
    fn tone(rate: u32, segments: &[(f64, f64)]) -> AudioBuf {
        let mut samples = Vec::new();
        let mut n = 0usize;
        for &(secs, db) in segments {
            let amp = 10f64.powf(db / 20.0);
            for _ in 0..(secs * rate as f64) as usize {
                let x = std::f64::consts::TAU * 1000.0 * n as f64 / rate as f64;
                let s = (amp * x.sin()) as f32;
                samples.extend([s, s]);
                n += 1;
            }
        }
        AudioBuf {
            samples,
            sample_rate: rate,
            channels: 2,
        }
    }

    #[test]
    fn k_weighting_matches_bs1770_at_48k() {
        let want = [
            [
                1.53512485958697,
                -2.69169618940638,
                1.19839281085285,
                -1.69065929318241,
                0.73248077421585,
            ],
            [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621],
        ];
        for (f, want) in k_weighting(48_000).iter().zip(want) {
            let got = [f.b0, f.b1, f.b2, f.a1, f.a2];
            for (g, w) in got.iter().zip(want) {
                assert!((g - w).abs() < 1e-9, "{got:?} is not {want:?}");
            }
        }
    }

    #[test]
    fn loudness_follows_bs1770() {
        // EBU Tech 3341 cases 1 to 4: steady tones, then quiet and silent stretches the
        // gates must leave out.
        let cases: [(&[(f64, f64)], f64); 4] = [
            (&[(20.0, -23.0)], -23.0),
            (&[(20.0, -33.0)], -33.0),
            (&[(10.0, -36.0), (60.0, -23.0), (10.0, -36.0)], -23.0),
            (
                &[
                    (10.0, -72.0),
                    (10.0, -36.0),
                    (60.0, -23.0),
                    (10.0, -36.0),
                    (10.0, -72.0),
                ],
                -23.0,
            ),
        ];
        for rate in [48_000, 44_100] {
            for (segments, want) in cases {
                let got = loudness(&tone(rate, segments)).expect("loudness") as f64;
                assert!(
                    (got - want).abs() < 0.1,
                    "{segments:?} at {rate} Hz read {got:.2} LUFS"
                );
            }
        }
        assert_eq!(loudness(&tone(48_000, &[(5.0, f64::NEG_INFINITY)])), None);
    }

    #[test]
    fn analysis_measures_loudness_and_peak() {
        let buf = tone(48_000, &[(3.0, -6.0)]);
        let a = analyze_buffer(&buf);
        let lufs = a.loudness.expect("loudness");
        assert!((lufs + 6.0).abs() < 0.1, "read {lufs} LUFS");
        // The peak AutoMix's crossover plays, the tone's onset ringing through it included.
        let mut split = crate::dsp::Crossover::new(48_000, CROSSOVER_HZ);
        let played = buf
            .samples
            .chunks(2)
            .flat_map(|f| split.split([f[0], f[1]]).sum())
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (a.peak - played).abs() < 1e-5 && played > 10f32.powf(-6.0 / 20.0),
            "peak {} where AutoMix plays {played}",
            a.peak
        );
    }

    #[test]
    fn bpm_detects_synthetic_120() {
        let buf = pulse_track(120.0, 12.0, 22_050);
        let analysis = analyze_buffer(&buf);
        let bpm = analysis.bpm.expect("bpm");
        assert!((bpm - 120.0).abs() < 4.0, "bpm={bpm}");
    }
}
