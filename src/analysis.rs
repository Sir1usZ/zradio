use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;
use serde::{Deserialize, Serialize};

use crate::decode::AudioBuf;

const ANALYSIS_VERSION: u32 = 2;
const ENV_HZ: f32 = 50.0;
const BPM_MIN: f32 = 70.0;
const BPM_MAX: f32 = 180.0;

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
    let (bpm, bpm_confidence, first_beat) = detect_bpm(&envelope, ENV_HZ);
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

fn detect_bpm(envelope: &[f32], env_hz: f32) -> (Option<f32>, f32, Option<f32>) {
    if envelope.len() < 80 {
        return (None, 0.0, None);
    }
    let flux: Vec<f32> = envelope
        .windows(2)
        .map(|w| (w[1] - w[0]).max(0.0))
        .collect();
    let min_lag = (60.0 * env_hz / BPM_MAX).round() as usize;
    let max_lag = (60.0 * env_hz / BPM_MIN).round() as usize;
    let max_lag = max_lag.min(flux.len() / 2).max(min_lag + 1);
    let mut best_lag = 0;
    let mut best = 0.0;
    let mut second = 0.0;
    for lag in min_lag..max_lag {
        let mut sum = 0.0;
        let mut n = 0usize;
        let mut i = 0;
        while i + lag < flux.len() {
            sum += flux[i] * flux[i + lag];
            n += 1;
            i += 1;
        }
        if n == 0 {
            continue;
        }
        let score = sum / n as f32;
        if score > best {
            second = best;
            best = score;
            best_lag = lag;
        } else if score > second {
            second = score;
        }
    }
    if best_lag == 0 || best <= 1e-8 {
        return (None, 0.0, None);
    }
    let bpm = 60.0 * env_hz / best_lag as f32;
    let conf = if best == 0.0 {
        0.0
    } else {
        ((best - second) / best).clamp(0.0, 1.0)
    };
    let mut best_phase = 0;
    let mut best_energy = -1.0;
    for phase in 0..best_lag {
        let mut e = 0.0;
        let mut idx = phase;
        while idx < flux.len() {
            e += flux[idx];
            idx += best_lag;
        }
        if e > best_energy {
            best_energy = e;
            best_phase = phase;
        }
    }
    (Some(bpm), conf, Some(best_phase as f32 / env_hz))
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
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.cache) {
            let _ = fs::write(&self.path, json);
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

    #[test]
    fn bpm_detects_synthetic_120() {
        let buf = pulse_track(120.0, 12.0, 22_050);
        let analysis = analyze_buffer(&buf);
        let bpm = analysis.bpm.expect("bpm");
        assert!((bpm - 120.0).abs() < 4.0, "bpm={bpm}");
    }
}
