//! Real PCM oscilloscope. Samples come from the mixer output, not FFT bands.

const CAPACITY: usize = 8192;
const MASK: usize = CAPACITY - 1;
const WINDOW_MS: f32 = 40.0;
const TRIGGER_SEARCH_MS: f32 = 25.0;
const TRIGGER_HYSTERESIS: f32 = 0.05;
const TRIGGER_FLOOR: f32 = 1.0e-4;

#[derive(Debug)]
pub struct PcmRing {
    samples: Box<[f32]>,
    pos: usize,
    filled: usize,
    sample_rate: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PcmSnapshot {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Default for PcmRing {
    fn default() -> Self {
        Self::new()
    }
}

impl PcmRing {
    pub fn new() -> Self {
        Self {
            samples: vec![0.0; CAPACITY].into_boxed_slice(),
            pos: 0,
            filled: 0,
            sample_rate: 0,
        }
    }

    pub fn push(&mut self, frames: &[f32], sample_rate: u32) {
        if frames.is_empty() {
            return;
        }
        self.sample_rate = sample_rate;
        let skip = frames.len().saturating_sub(CAPACITY);
        let src = &frames[skip..];
        let head = (CAPACITY - self.pos).min(src.len());
        self.samples[self.pos..self.pos + head].copy_from_slice(&src[..head]);
        self.samples[..src.len() - head].copy_from_slice(&src[head..]);
        self.pos = (self.pos + src.len()) & MASK;
        self.filled = (self.filled + src.len()).min(CAPACITY);
    }

    pub fn push_interleaved(&mut self, interleaved: &[f32], channels: usize, sample_rate: u32) {
        if interleaved.is_empty() || channels == 0 {
            return;
        }
        self.sample_rate = sample_rate;
        let frames = interleaved.len() / channels;
        let skip = frames.saturating_sub(CAPACITY);
        for i in skip..frames {
            let base = i * channels;
            let mono = if channels == 1 {
                interleaved[base]
            } else {
                (interleaved[base] + interleaved.get(base + 1).copied().unwrap_or(0.0)) * 0.5
            };
            self.samples[self.pos] = mono;
            self.pos = (self.pos + 1) & MASK;
            self.filled = (self.filled + 1).min(CAPACITY);
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0;
        self.filled = 0;
    }

    pub fn snapshot(&self) -> PcmSnapshot {
        if self.filled == 0 {
            return PcmSnapshot {
                samples: Vec::new(),
                sample_rate: self.sample_rate,
            };
        }
        let window = window_len(self.sample_rate, self.filled);
        if window < 2 {
            return PcmSnapshot {
                samples: self.copy_newest(self.filled),
                sample_rate: self.sample_rate,
            };
        }
        let search = ((self.sample_rate as f32 * TRIGGER_SEARCH_MS / 1000.0) as usize)
            .min(self.filled.saturating_sub(window));
        let mut buf = self.copy_newest(window + search);
        let start = trigger_offset_slice(&buf, self.sample_rate, window);
        if start > 0 {
            buf.copy_within(start..start + window, 0);
        }
        buf.truncate(window);
        PcmSnapshot {
            samples: buf,
            sample_rate: self.sample_rate,
        }
    }

    fn copy_newest(&self, n: usize) -> Vec<f32> {
        let n = n.min(self.filled);
        let mut samples = vec![0.0; n];
        let start = (self.pos + CAPACITY - n) & MASK;
        let head = (CAPACITY - start).min(n);
        samples[..head].copy_from_slice(&self.samples[start..start + head]);
        samples[head..].copy_from_slice(&self.samples[..n - head]);
        samples
    }

    #[cfg(test)]
    pub(crate) fn linearize(&self) -> PcmSnapshot {
        PcmSnapshot {
            samples: self.copy_newest(self.filled),
            sample_rate: self.sample_rate,
        }
    }
}

impl PcmSnapshot {
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

fn window_len(sample_rate: u32, available: usize) -> usize {
    if sample_rate == 0 {
        return 0;
    }
    ((sample_rate as f32 * WINDOW_MS / 1000.0) as usize).min(available)
}

fn window_frames(snap: &PcmSnapshot) -> usize {
    window_len(snap.sample_rate, snap.len())
}

pub(crate) fn trigger_offset(snap: &PcmSnapshot, window: usize) -> usize {
    trigger_offset_slice(&snap.samples, snap.sample_rate, window)
}

fn trigger_offset_slice(samples: &[f32], sample_rate: u32, window: usize) -> usize {
    if samples.len() < window {
        return 0;
    }
    let latest = samples.len() - window;
    let search = ((sample_rate as f32 * TRIGGER_SEARCH_MS / 1000.0) as usize).min(latest);
    let begin = latest - search;

    let mut peak = 0.0f32;
    for &v in &samples[begin..latest] {
        peak = peak.max(v.abs());
    }
    let hysteresis = (peak * TRIGGER_HYSTERESIS).max(TRIGGER_FLOOR);

    let mut armed = false;
    let mut found = None;
    for (i, &v) in samples.iter().enumerate().take(latest).skip(begin) {
        if v <= -hysteresis {
            armed = true;
        } else if armed && v >= 0.0 {
            armed = false;
            found = Some(i);
        }
    }
    found.unwrap_or(latest)
}

fn sample_row(v: f32, height: usize) -> usize {
    let span = height.saturating_sub(1) as f32;
    ((1.0 - v) * 0.5 * span).round().clamp(0.0, span) as usize
}

fn stroke(prev: usize, row: usize) -> char {
    if row == prev {
        '━'
    } else if row < prev {
        '╱'
    } else {
        '╲'
    }
}

pub fn rasterize(snap: &PcmSnapshot, width: u16, height: u16) -> Vec<String> {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    let mut grid = vec![vec![' '; w]; h];
    let mid = sample_row(0.0, h);
    let window = window_frames(snap);
    if window < 2 {
        grid[mid].fill('─');
        return grid.into_iter().map(|r| r.into_iter().collect()).collect();
    }
    let start = trigger_offset(snap, window);
    let samples = &snap.samples[start..start + window];
    let n = samples.len();
    let mut prev = mid;
    for col in 0..w {
        let begin = col * n / w;
        let end = ((col + 1) * n / w).clamp(begin + 1, n);
        let v = samples[begin..end].iter().copied().sum::<f32>() / (end - begin) as f32;
        let row = sample_row(v, h);
        grid[row][col] = stroke(prev, row);
        if row != prev {
            let (lo, hi) = if row < prev {
                (row + 1, prev)
            } else {
                (prev + 1, row)
            };
            for cells in grid.iter_mut().take(hi).skip(lo) {
                if cells[col] == ' ' {
                    cells[col] = '│';
                }
            }
        }
        prev = row;
    }
    grid.into_iter().map(|r| r.into_iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(freq: f32, phase: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                (std::f32::consts::TAU * freq * t + phase).sin()
            })
            .collect()
    }

    fn snapshot_from(samples: Vec<f32>) -> PcmSnapshot {
        PcmSnapshot {
            sample_rate: RATE,
            samples,
        }
    }

    fn lit_rows(rows: &[String]) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter(|(_, row)| row.chars().any(|c| c != ' '))
            .map(|(i, _)| i)
            .collect()
    }

    fn is_braille(c: char) -> bool {
        ('\u{2800}'..='\u{28FF}').contains(&c)
    }

    #[test]
    fn ring_keeps_newest_samples_across_wrap() {
        let mut ring = PcmRing::new();
        let total = CAPACITY + 777;
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        for chunk in data.chunks(512) {
            ring.push(chunk, RATE);
        }
        let snap = ring.linearize();
        assert_eq!(snap.len(), CAPACITY);
        assert_eq!(snap.sample_rate, RATE);
        assert_eq!(snap.samples[0], (total - CAPACITY) as f32);
        assert_eq!(snap.samples[CAPACITY - 1], (total - 1) as f32);
    }

    #[test]
    fn ring_reset_drops_old_audio() {
        let mut ring = PcmRing::new();
        ring.push(&[0.5; 64], RATE);
        ring.reset();
        assert_eq!(ring.snapshot().len(), 0);
    }

    #[test]
    fn trigger_locks_onto_a_rising_zero_crossing() {
        let window = (RATE as f32 * WINDOW_MS / 1000.0) as usize;
        for step in 0..8 {
            let phase = step as f32 * std::f32::consts::TAU / 8.0;
            let snap = snapshot_from(sine(440.0, phase, window * 4));
            let start = trigger_offset(&snap, window);
            assert!(snap.samples[start] >= 0.0 && snap.samples[start] < 0.07);
            assert!(snap.samples[start - 1] < 0.0);
            assert!(snap.samples[start + 1] > 0.0);
        }
    }

    #[test]
    fn silence_draws_a_centered_flat_line() {
        let rows = rasterize(&PcmSnapshot::default(), 60, 8);
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|r| r.chars().count() == 60));
        let joined: String = rows.concat();
        assert!(
            !joined.chars().any(is_braille),
            "scope must be a single line, not braille"
        );
        let lit = lit_rows(&rows);
        assert_eq!(
            lit.len(),
            1,
            "silence should light exactly one row, got {lit:?}"
        );
        assert!(
            lit[0] == 3 || lit[0] == 4,
            "centered line should sit in the middle two rows, got {}",
            lit[0]
        );
        assert!(rows[lit[0]]
            .chars()
            .all(|c| c == '─' || c == '━' || c == '-'));
    }

    #[test]
    fn full_scale_sine_spans_top_and_bottom() {
        let window = (RATE as f32 * WINDOW_MS / 1000.0) as usize;
        let snap = snapshot_from(sine(440.0, 0.0, window * 4));
        let rows = rasterize(&snap, 60, 8);
        let joined: String = rows.concat();
        assert!(
            !joined
                .chars()
                .any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)),
            "scope must be a single line, not braille"
        );
        let lit = lit_rows(&rows);
        assert_eq!(lit.first().copied(), Some(0), "top row untouched: {lit:?}");
        assert_eq!(
            lit.last().copied(),
            Some(7),
            "bottom row untouched: {lit:?}"
        );
        for col in 0..60 {
            let any = rows
                .iter()
                .any(|row| row.chars().nth(col).is_some_and(|c| c != ' '));
            assert!(any, "column {col} is empty");
        }
    }

    #[test]
    fn quiet_signal_stays_near_center() {
        let window = (RATE as f32 * WINDOW_MS / 1000.0) as usize;
        let mut samples = sine(440.0, 0.0, window * 4);
        for v in &mut samples {
            *v *= 0.02;
        }
        let rows = rasterize(&snapshot_from(samples), 60, 8);
        assert!(
            rows[0].chars().all(|c| c == ' '),
            "quiet signal reached the top"
        );
        assert!(
            rows[7].chars().all(|c| c == ' '),
            "quiet signal reached the bottom"
        );
        assert!(!lit_rows(&rows).is_empty());
    }
}
