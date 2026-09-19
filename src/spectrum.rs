use rustfft::{num_complex::Complex32, FftPlanner};

pub const BANDS: usize = 24;
const HISTORY: usize = 96;
const FFT: usize = 512;

#[derive(Debug, Clone)]
pub struct Spectrum {
    bands: [f32; BANDS],
    history: Vec<f32>,
    fft_buf: Vec<f32>,
}

impl Default for Spectrum {
    fn default() -> Self {
        Self {
            bands: [0.0; BANDS],
            history: Vec::with_capacity(HISTORY),
            fft_buf: Vec::with_capacity(FFT),
        }
    }
}

impl Spectrum {
    pub fn push_frame(&mut self, sample: f32) {
        self.history.push(sample.abs().min(1.0));
        if self.history.len() > HISTORY {
            let extra = self.history.len() - HISTORY;
            self.history.drain(0..extra);
        }
        if self.history.len() < BANDS {
            return;
        }
        let chunk = self.history.len() / BANDS;
        for band in 0..BANDS {
            let start = band * chunk;
            let end = if band + 1 == BANDS {
                self.history.len()
            } else {
                start + chunk
            };
            let energy = if start >= end {
                0.0
            } else {
                let sum: f32 = self.history[start..end].iter().sum();
                (sum / (end - start) as f32).sqrt()
            };
            self.bands[band] = self.bands[band] * 0.62 + energy * 0.38;
        }
    }

    pub fn push_fft_frame(&mut self, sample: f32) {
        self.fft_buf.push(sample);
        if self.fft_buf.len() > FFT * 2 {
            let extra = self.fft_buf.len() - FFT;
            self.fft_buf.drain(0..extra);
        }
    }

    pub fn analyze_pending(&mut self) {
        if self.fft_buf.len() < FFT {
            return;
        }
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT);
        let window: Vec<f32> = (0..FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT - 1) as f32).cos())
            .collect();
        let mut spec: Vec<Complex32> = self
            .fft_buf
            .iter()
            .take(FFT)
            .zip(window)
            .map(|(s, w)| Complex32::new(s * w, 0.0))
            .collect();
        fft.process(&mut spec);
        let half = FFT / 2;
        let sr = 48_000.0;
        let nyquist = sr / 2.0;
        for band in 0..BANDS {
            let lo = 40.0 * (nyquist / 40.0_f32).powf(band as f32 / BANDS as f32);
            let hi = 40.0 * (nyquist / 40.0_f32).powf((band + 1) as f32 / BANDS as f32);
            let start = ((lo / nyquist) * half as f32).floor() as usize;
            let end = ((hi / nyquist) * half as f32).ceil() as usize;
            let start = start.clamp(1, half.saturating_sub(1));
            let end = end.clamp(start + 1, half);
            let sum: f32 = spec[start..end].iter().map(|c| c.norm()).sum();
            let energy = ((sum / (end - start) as f32).sqrt() * 0.35).clamp(0.0, 1.0);
            self.bands[band] = self.bands[band] * 0.55 + energy * 0.45;
        }
        let extra = self.fft_buf.len().saturating_sub(FFT / 4);
        if extra > 0 {
            self.fft_buf.drain(0..extra);
        }
    }

    pub fn ingest_cava_bars(&mut self, bars: &[f32]) {
        if bars.is_empty() {
            return;
        }
        for (i, band) in self.bands.iter_mut().enumerate() {
            let t = i as f32 * (bars.len().saturating_sub(1) as f32)
                / (BANDS.saturating_sub(1) as f32).max(1.0);
            let idx = t.floor() as usize;
            let frac = t - idx as f32;
            let a = bars[idx.min(bars.len() - 1)];
            let b = bars[(idx + 1).min(bars.len() - 1)];
            *band = *band * 0.4 + (a + (b - a) * frac).clamp(0.0, 1.0) * 0.6;
        }
    }

    pub fn bars(&self, height: u16) -> Vec<String> {
        self.bars_sized(height, BANDS as u16)
    }

    pub fn bars_sized(&self, height: u16, width: u16) -> Vec<String> {
        let h = height.max(1) as usize;
        let w = width.max(1) as usize;
        let glyphs = [' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let mut rows = vec![String::new(); h];
        for col in 0..w {
            let band = sample_band(&self.bands, col, w);
            let level = (band * h as f32 * 1.8).clamp(0.0, h as f32);
            for (row, line) in rows.iter_mut().enumerate() {
                let from_bottom = h - 1 - row;
                let filled = level - from_bottom as f32;
                let ch = if filled >= 1.0 {
                    glyphs[7]
                } else if filled > 0.0 {
                    glyphs[((filled * 6.0).round() as usize).min(6)]
                } else {
                    glyphs[0]
                };
                line.push(ch);
            }
        }
        rows
    }

    pub fn cnm_sized(&self, height: u16, width: u16) -> Vec<String> {
        let h = height.max(1) as usize;
        let w = width.max(1) as usize;
        if h == 1 {
            return vec!["─".repeat(w)];
        }
        let bars_h = h - 1;
        let mut grid = vec![vec![' '; w]; bars_h];
        let (bar_w, gap_w, count, offset) = cnm_layout(w, BANDS);
        for i in 0..count {
            let t = if count == 1 {
                0.0
            } else {
                i as f32 * (BANDS.saturating_sub(1) as f32) / (count.saturating_sub(1) as f32)
            };
            let idx = t.round() as usize;
            let val = self.bands[idx.min(BANDS - 1)].clamp(0.0, 1.0).powf(0.72);
            let bar_h = (val * bars_h as f32).round() as usize;
            let x0 = offset + i * (bar_w + gap_w);
            for y in 0..bar_h.min(bars_h) {
                let row = bars_h - 1 - y;
                let ch = cnm_density(y, bar_h.max(1));
                for cell in grid[row].iter_mut().take((x0 + bar_w).min(w)).skip(x0) {
                    *cell = ch;
                }
            }
        }
        let mut rows: Vec<String> = grid.into_iter().map(|r| r.into_iter().collect()).collect();
        rows.push("─".repeat(w));
        rows
    }

    #[allow(clippy::needless_range_loop)]
    pub fn scope_sized(&self, height: u16, width: u16) -> Vec<String> {
        let h = height.max(1) as usize;
        let w = width.max(1) as usize;
        let mid = (h.saturating_sub(1)) / 2;
        let mut rows = vec![" ".repeat(w).chars().collect::<Vec<char>>(); h];
        let mut prev_row = mid;
        for col in 0..w {
            let band = sample_band(&self.bands, col, w);
            let signed = (band - 0.35) * 2.2;
            let offset = (signed * mid as f32).round() as i32;
            let row = (mid as i32 - offset).clamp(0, h.saturating_sub(1) as i32) as usize;
            let ch = if row == prev_row {
                '━'
            } else if row < prev_row {
                '╱'
            } else {
                '╲'
            };
            rows[row][col] = ch;
            if row != prev_row {
                let (lo, hi) = if row < prev_row {
                    (row + 1, prev_row)
                } else {
                    (prev_row + 1, row)
                };
                for cells in rows.iter_mut().take(hi).skip(lo) {
                    if cells[col] == ' ' {
                        cells[col] = '│';
                    }
                }
            }
            prev_row = row;
        }
        for cell in &mut rows[mid] {
            if *cell == ' ' {
                *cell = '─';
            }
        }
        rows.into_iter().map(|r| r.into_iter().collect()).collect()
    }

    pub fn levels(&self) -> [f32; BANDS] {
        self.bands
    }

    pub fn from_levels(bands: [f32; BANDS]) -> Self {
        Self {
            bands,
            history: Vec::new(),
            fft_buf: Vec::new(),
        }
    }

    pub fn peak(&self) -> f32 {
        self.bands.iter().copied().fold(0.0, f32::max)
    }
}

fn cnm_layout(width: usize, wanted: usize) -> (usize, usize, usize, usize) {
    if width == 0 {
        return (1, 0, 0, 0);
    }
    let mut bars = wanted.min(width.div_ceil(2)).max(1);
    loop {
        let mut bar_w = (width / bars).max(1);
        while bar_w >= 1 {
            let gap_w = bar_w.div_ceil(2).max(1);
            let needed = bars * bar_w + bars.saturating_sub(1) * gap_w;
            if needed <= width {
                let offset = (width - needed) / 2;
                return (bar_w, gap_w, bars, offset);
            }
            if bar_w == 1 {
                break;
            }
            bar_w -= 1;
        }
        if bars <= 1 {
            return (width.max(1), 0, 1, 0);
        }
        bars -= 1;
    }
}

fn cnm_density(level: usize, height: usize) -> char {
    if height <= 1 {
        return '░';
    }
    let ratio = level as f32 / height as f32;
    if ratio < 0.25 {
        '█'
    } else if ratio < 0.50 {
        '▓'
    } else if ratio < 0.75 {
        '▒'
    } else {
        '░'
    }
}

fn sample_band(bands: &[f32; BANDS], col: usize, width: usize) -> f32 {
    if width <= 1 {
        return bands[0];
    }
    let t = col as f32 * (BANDS.saturating_sub(1) as f32) / (width.saturating_sub(1) as f32);
    let i = t.floor() as usize;
    let frac = t - i as f32;
    let a = bands[i.min(BANDS - 1)];
    let b = bands[(i + 1).min(BANDS - 1)];
    a + (b - a) * frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_spectrum_stays_low() {
        let mut spec = Spectrum::default();
        for _ in 0..200 {
            spec.push_frame(0.0);
        }
        assert!(spec.peak() < 0.05);
    }

    #[test]
    fn loud_frames_raise_bars() {
        let mut spec = Spectrum::default();
        for _ in 0..200 {
            spec.push_frame(0.9);
        }
        let bars = spec.bars(4);
        assert_eq!(bars.len(), 4);
        assert!(spec.peak() > 0.4);
        assert!(bars.last().unwrap().contains('█') || bars.last().unwrap().contains('▇'));
    }

    #[test]
    fn bars_stretch_to_requested_width() {
        let mut spec = Spectrum::default();
        for _ in 0..200 {
            spec.push_frame(0.6);
        }
        let rows = spec.bars_sized(5, 40);
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| row.chars().count() == 40));
        let bottom = rows.last().unwrap();
        assert!(bottom.contains('█') || bottom.contains('▇') || bottom.contains('▆'));
        let top_fill = rows[0].chars().filter(|c| *c != ' ').count();
        let bottom_fill = bottom.chars().filter(|c| *c != ' ').count();
        assert!(bottom_fill >= top_fill);
    }

    #[test]
    fn fft_loud_tone_raises_bands() {
        let mut spec = Spectrum::default();
        for i in 0..FFT * 2 {
            let s = ((i as f32) * 0.2).sin();
            spec.push_fft_frame(s);
        }
        spec.analyze_pending();
        assert!(spec.peak() > 0.05);
    }

    #[test]
    fn cnm_bars_have_gaps_density_and_baseline() {
        let mut bands = [0.0; BANDS];
        bands[0] = 1.0;
        bands[11] = 0.5;
        bands[23] = 0.8;
        let spec = Spectrum::from_levels(bands);
        let rows = spec.cnm_sized(8, 40);
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|r| r.chars().count() == 40));
        let baseline = rows.last().unwrap();
        assert!(
            baseline.chars().all(|c| c == '─' || c == ' '),
            "cnm last row is a baseline, got {baseline:?}"
        );
        assert!(baseline.contains('─'));
        let body: String = rows[..rows.len() - 1].join("");
        assert!(
            body.contains('█') || body.contains('▓') || body.contains('▒') || body.contains('░'),
            "cnm uses density blocks, got {body:?}"
        );
        let bars = spec.bars_sized(8, 40);
        assert_ne!(rows, bars, "cnm must not reuse packed bars");
        let mid = &rows[rows.len() / 2];
        assert!(mid.contains(' '), "cnm bars should leave gaps, got {mid:?}");
    }

    #[test]
    fn scope_is_a_waveform_not_columns() {
        let mut bands = [0.1; BANDS];
        bands[0] = 0.9;
        bands[11] = 0.05;
        bands[23] = 0.8;
        let spec = Spectrum::from_levels(bands);
        let scope = spec.scope_sized(5, 24);
        let bars = spec.bars_sized(5, 24);
        assert_eq!(scope.len(), 5);
        assert_ne!(scope, bars, "scope must not reuse bar columns");
        let mid = &scope[scope.len() / 2];
        assert!(
            mid.contains('━') || mid.contains('─') || mid.contains('╱') || mid.contains('╲'),
            "scope should draw a midline waveform, got {mid:?}"
        );
    }

    #[test]
    fn low_tone_lights_left_bands_more_than_right() {
        let mut spec = Spectrum::default();
        let freq = 4.0;
        for i in 0..FFT * 4 {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / FFT as f32).sin();
            spec.push_fft_frame(s);
        }
        spec.analyze_pending();
        let left: f32 = spec.levels()[..6].iter().sum();
        let right: f32 = spec.levels()[18..].iter().sum();
        assert!(
            left > right,
            "low tone should light left bands, left={left} right={right}"
        );
    }
}
