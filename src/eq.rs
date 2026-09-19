const BANDS: usize = 5;
const FREQS: [f32; BANDS] = [60.0, 250.0, 1000.0, 4000.0, 12000.0];

#[derive(Debug, Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    fn peaking(sr: f32, freq: f32, db: f32) -> Self {
        let a = 10f32.powf(db / 40.0);
        let w = 2.0 * std::f32::consts::PI * freq / sr.max(1.0);
        let q = 0.9;
        let alpha = w.sin() / (2.0 * q);
        let cosw = w.cos();
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cosw;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cosw;
        let a2 = 1.0 - alpha / a;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

#[derive(Debug, Clone)]
pub struct Equalizer {
    db: [f32; BANDS],
    left: Vec<Biquad>,
    right: Vec<Biquad>,
}

impl Equalizer {
    pub fn new(sample_rate: u32, db: [f32; BANDS]) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mk = |gain: [f32; BANDS]| {
            FREQS
                .iter()
                .zip(gain)
                .map(|(f, g)| Biquad::peaking(sr, *f, g))
                .collect()
        };
        Self {
            db,
            left: mk(db),
            right: mk(db),
        }
    }

    pub fn set_db(&mut self, sample_rate: u32, db: [f32; BANDS]) {
        *self = Self::new(sample_rate, db);
    }

    pub fn bump(&mut self, sample_rate: u32, band: usize, delta: f32) {
        if band >= BANDS {
            return;
        }
        self.db[band] = (self.db[band] + delta).clamp(-12.0, 12.0);
        self.set_db(sample_rate, self.db);
    }

    pub fn reset(&mut self, sample_rate: u32) {
        self.set_db(sample_rate, [0.0; BANDS]);
    }

    pub fn db(&self) -> [f32; BANDS] {
        self.db
    }

    pub fn labels() -> [&'static str; BANDS] {
        ["60", "250", "1k", "4k", "12k"]
    }

    pub fn process_stereo(&mut self, frame: [f32; 2]) -> [f32; 2] {
        let mut l = frame[0];
        let mut r = frame[1];
        for (left, right) in self.left.iter_mut().zip(self.right.iter_mut()) {
            l = left.process(l);
            r = right.process(r);
        }
        [l, r]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_eq_keeps_sample() {
        let mut eq = Equalizer::new(48_000, [0.0; 5]);
        let out = eq.process_stereo([0.2, -0.1]);
        assert!((out[0] - 0.2).abs() < 0.02);
        assert!((out[1] + 0.1).abs() < 0.02);
    }

    #[test]
    fn bump_then_reset_returns_flat() {
        let mut eq = Equalizer::new(48_000, [0.0; 5]);
        eq.bump(48_000, 2, 6.0);
        assert!((eq.db()[2] - 6.0).abs() < 1e-4);
        eq.reset(48_000);
        assert!(eq.db().iter().all(|g| *g == 0.0));
    }
}
