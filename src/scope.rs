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
        let len = self.filled;
        if len == 0 {
            return PcmSnapshot {
                samples: Vec::new(),
                sample_rate: self.sample_rate,
            };
        }
        let mut samples = vec![0.0; len];
        let start = (self.pos + CAPACITY - len) & MASK;
        let head = (CAPACITY - start).min(len);
        samples[..head].copy_from_slice(&self.samples[start..start + head]);
        samples[head..].copy_from_slice(&self.samples[..len - head]);
        PcmSnapshot {
            samples,
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

fn window_frames(snap: &PcmSnapshot) -> usize {
    if snap.sample_rate == 0 {
        return 0;
    }
    let by_time = (snap.sample_rate as f32 * WINDOW_MS / 1000.0) as usize;
    by_time.min(snap.len())
}

pub(crate) fn trigger_offset(snap: &PcmSnapshot, window: usize) -> usize {
    if snap.len() < window {
        return 0;
    }
    let latest = snap.len() - window;
    let search = ((snap.sample_rate as f32 * TRIGGER_SEARCH_MS / 1000.0) as usize).min(latest);
    let begin = latest - search;

    let mut peak = 0.0f32;
    for &v in &snap.samples[begin..latest] {
        peak = peak.max(v.abs());
    }
    let hysteresis = (peak * TRIGGER_HYSTERESIS).max(TRIGGER_FLOOR);

    let mut armed = false;
    let mut found = None;
    for i in begin..latest {
        let v = snap.samples[i];
        if v <= -hysteresis {
            armed = true;
        } else if armed && v >= 0.0 {
            armed = false;
            found = Some(i);
        }
    }
    found.unwrap_or(latest)
}

fn sample_row(v: f32, h_px: i32) -> i32 {
    let span = (h_px - 1) as f32;
    ((1.0 - v) * 0.5 * span).round().clamp(0.0, span) as i32
}

fn braille_bit(dx: usize, dy: usize) -> u8 {
    match (dx, dy) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (1, 3) => 0x80,
        _ => 0,
    }
}

fn braille_from_bits(bits: u8) -> char {
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

fn set_pixel(grid: &mut [u8], w_cells: usize, h_cells: usize, x: i32, y: i32) {
    if x < 0 || y < 0 {
        return;
    }
    let w_px = (w_cells * 2) as i32;
    let h_px = (h_cells * 4) as i32;
    if x >= w_px || y >= h_px {
        return;
    }
    let cell_x = (x / 2) as usize;
    let cell_y = (y / 4) as usize;
    if cell_x >= w_cells || cell_y >= h_cells {
        return;
    }
    let dx = (x % 2) as usize;
    let dy = (y % 4) as usize;
    grid[cell_y * w_cells + cell_x] |= braille_bit(dx, dy);
}

fn draw_channel(grid: &mut [u8], w_cells: usize, h_cells: usize, samples: &[f32]) {
    let w_px = w_cells * 2;
    let h_px = (h_cells * 4) as i32;
    let n = samples.len();
    if n == 0 || w_px == 0 {
        return;
    }
    let mut prev: Option<(i32, i32)> = None;
    for col in 0..w_px {
        let begin = col * n / w_px;
        let end = ((col + 1) * n / w_px).clamp(begin + 1, n);
        let (mut lo, mut hi) = (samples[begin], samples[begin]);
        for &v in &samples[begin..end] {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        let (top, bottom) = (sample_row(hi, h_px), sample_row(lo, h_px));
        let (mut from, mut to) = (top, bottom);
        if let Some((prev_top, prev_bottom)) = prev {
            from = from.min(prev_bottom);
            to = to.max(prev_top);
        }
        for y in from..=to {
            set_pixel(grid, w_cells, h_cells, col as i32, y);
        }
        prev = Some((top, bottom));
    }
}

fn draw_flat_line(grid: &mut [u8], w_cells: usize, h_cells: usize) {
    let zero = sample_row(0.0, (h_cells * 4) as i32);
    for col in 0..(w_cells * 2) as i32 {
        set_pixel(grid, w_cells, h_cells, col, zero);
    }
}

pub fn rasterize(snap: &PcmSnapshot, width: u16, height: u16) -> Vec<String> {
    let w_cells = width.max(1) as usize;
    let h_cells = height.max(1) as usize;
    let mut grid = vec![0u8; w_cells * h_cells];
    let window = window_frames(snap);
    if window < 2 {
        draw_flat_line(&mut grid, w_cells, h_cells);
    } else {
        let start = trigger_offset(snap, window);
        draw_channel(
            &mut grid,
            w_cells,
            h_cells,
            &snap.samples[start..start + window],
        );
    }
    let mut rows = Vec::with_capacity(h_cells);
    for row in 0..h_cells {
        let mut line = String::with_capacity(w_cells);
        for col in 0..w_cells {
            let bits = grid[row * w_cells + col];
            if bits == 0 {
                line.push(' ');
            } else {
                line.push(braille_from_bits(bits));
            }
        }
        rows.push(line);
    }
    rows
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

    #[test]
    fn ring_keeps_newest_samples_across_wrap() {
        let mut ring = PcmRing::new();
        let total = CAPACITY + 777;
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        for chunk in data.chunks(512) {
            ring.push(chunk, RATE);
        }
        let snap = ring.snapshot();
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
        assert!(rows[lit[0]].chars().all(|c| c != ' '));
    }

    #[test]
    fn full_scale_sine_spans_top_and_bottom() {
        let window = (RATE as f32 * WINDOW_MS / 1000.0) as usize;
        let snap = snapshot_from(sine(440.0, 0.0, window * 4));
        let rows = rasterize(&snap, 60, 8);
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
