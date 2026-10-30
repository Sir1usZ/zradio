//! Pitch-preserving tempo bend for the outgoing deck's tail (WSOLA).
//!
//! AutoMix only ever bends the track that is leaving. The incoming track plays at
//! its own speed, so its analysis (beats, mix points) stays valid once it is current.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeMap {
    ramp: f64,
    speed: f64,
}

impl TimeMap {
    /// Speed glides linearly from 1 to `speed` over the first `ramp` source frames, then holds.
    /// Speed is source frames per output frame, so above 1 plays faster.
    pub fn new(ramp: f64, speed: f64) -> Self {
        Self {
            ramp: ramp.max(0.0),
            speed: speed.clamp(0.5, 2.0),
        }
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn ramp(&self) -> f64 {
        self.ramp
    }

    fn slope(&self) -> Option<f64> {
        if self.ramp <= 0.0 || (self.speed - 1.0).abs() < 1e-12 {
            None
        } else {
            Some((self.speed - 1.0) / self.ramp)
        }
    }

    /// Output frames it takes to play `s` source frames.
    pub fn out_of(&self, s: f64) -> f64 {
        let s = s.max(0.0);
        match self.slope() {
            None => s / self.speed,
            Some(k) => {
                let head = s.min(self.ramp);
                (k * head).ln_1p() / k + (s - head) / self.speed
            }
        }
    }

    /// Source frames consumed after `t` output frames.
    pub fn src_of(&self, t: f64) -> f64 {
        let t = t.max(0.0);
        match self.slope() {
            None => t * self.speed,
            Some(k) => {
                let t_ramp = self.speed.ln() / k;
                if t <= t_ramp {
                    (k * t).exp_m1() / k
                } else {
                    self.ramp + (t - t_ramp) * self.speed
                }
            }
        }
    }
}

/// Renders `out_frames` of `src` from frame `start` on, played through `map`.
/// The first output frame is `src[start]`, so the tail can be spliced in without a seam.
pub fn render_tail(
    src: &[f32],
    channels: usize,
    start: usize,
    map: TimeMap,
    out_frames: usize,
    sample_rate: u32,
) -> Vec<f32> {
    let ch = channels.max(1);
    let mut out = vec![0.0f32; out_frames * ch];
    let src_frames = src.len() / ch;
    if out_frames == 0 || src_frames == 0 {
        return out;
    }
    let hop = ((sample_rate.max(8_000) as f32 * 0.021).round() as usize).max(64);
    let win = hop * 2;
    let tol = hop / 2;
    let dec = (sample_rate as usize / 11_025).max(2);
    let window: Vec<f32> = (0..win)
        .map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / win as f32).cos())
        .collect();
    let start = start as i64;
    let hop_i = hop as i64;
    let reach = map.src_of((out_frames + win) as f64).ceil() as i64;
    let mono = Mono::new(
        src,
        ch,
        start - hop_i - (tol + win) as i64,
        start + reach + (tol + 2 * win) as i64,
    );
    let mut prev = start - hop_i;
    let mut k: i64 = 0;
    loop {
        let o0 = (k - 1) * hop_i;
        if o0 >= out_frames as i64 {
            break;
        }
        let pos = match k {
            0 => start - hop_i,
            1 => start,
            _ => {
                let nominal = start + map.src_of((o0 + hop_i) as f64).round() as i64 - hop_i;
                best_match(&mono, prev + hop_i, nominal, hop, tol, dec)
            }
        };
        for (n, &w) in window.iter().enumerate() {
            let o = o0 + n as i64;
            if o < 0 {
                continue;
            }
            if o >= out_frames as i64 {
                break;
            }
            let s = pos + n as i64;
            if s < 0 || s >= src_frames as i64 {
                continue;
            }
            let si = s as usize * ch;
            let oi = o as usize * ch;
            for c in 0..ch {
                out[oi + c] += src[si + c] * w;
            }
        }
        prev = pos;
        k += 1;
    }
    out
}

struct Mono {
    lo: i64,
    data: Vec<f32>,
}

impl Mono {
    fn new(src: &[f32], ch: usize, lo: i64, hi: i64) -> Self {
        let frames = (src.len() / ch) as i64;
        let lo = lo.max(0);
        let hi = hi.min(frames).max(lo);
        let data = (lo..hi)
            .map(|f| {
                let i = f as usize * ch;
                src[i..i + ch].iter().sum::<f32>() / ch as f32
            })
            .collect();
        Self { lo, data }
    }

    fn at(&self, frame: i64) -> f32 {
        let i = frame - self.lo;
        if i < 0 || i as usize >= self.data.len() {
            0.0
        } else {
            self.data[i as usize]
        }
    }

    fn boxed(&self, frame: i64, dec: usize) -> f32 {
        (0..dec as i64).map(|n| self.at(frame + n)).sum::<f32>() / dec as f32
    }
}

// Coarse search on a decimated guide, then refine at full rate around the winner.
fn best_match(mono: &Mono, natural: i64, nominal: i64, len: usize, tol: usize, dec: usize) -> i64 {
    let d = dec as i64;
    let tol = tol as i64;
    let taps = (len / dec).max(1);
    let half = tol / d;
    let steps = (2 * half) as usize;
    let first = nominal - half * d;
    let target: Vec<f32> = (0..taps)
        .map(|n| mono.boxed(natural + n as i64 * d, dec))
        .collect();
    let lattice: Vec<f32> = (0..steps + taps)
        .map(|j| mono.boxed(first + j as i64 * d, dec))
        .collect();
    let mut energy: f64 = lattice[..taps].iter().map(|x| f64::from(x * x)).sum();
    let mut coarse = nominal;
    let mut best = f64::MIN;
    for j in 0..=steps {
        if j > 0 {
            energy += f64::from(lattice[j + taps - 1].powi(2)) - f64::from(lattice[j - 1].powi(2));
        }
        let dot: f64 = lattice[j..j + taps]
            .iter()
            .zip(&target)
            .map(|(a, b)| f64::from(a * b))
            .sum();
        let score = if energy > 1e-12 {
            dot / energy.sqrt()
        } else {
            0.0
        };
        let cand = first + j as i64 * d;
        if score > best + 1e-9
            || ((score - best).abs() <= 1e-9 && (cand - nominal).abs() < (coarse - nominal).abs())
        {
            best = score;
            coarse = cand;
        }
    }
    let mut pick = coarse;
    let mut best = f64::MIN;
    for cand in (coarse - d + 1).max(nominal - tol)..=(coarse + d - 1).min(nominal + tol) {
        let mut dot = 0.0f64;
        let mut energy = 0.0f64;
        for n in 0..len as i64 {
            let a = f64::from(mono.at(cand + n));
            dot += a * f64::from(mono.at(natural + n));
            energy += a * a;
        }
        let score = if energy > 1e-12 {
            dot / energy.sqrt()
        } else {
            0.0
        };
        if score > best + 1e-9
            || ((score - best).abs() <= 1e-9 && (cand - coarse).abs() < (pick - coarse).abs())
        {
            best = score;
            pick = cand;
        }
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 22_050;

    fn sine(freq: f32, secs: f32) -> Vec<f32> {
        let frames = (secs * SR as f32) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = (std::f32::consts::TAU * freq * i as f32 / SR as f32).sin() * 0.5;
            out.push(s);
            out.push(s);
        }
        out
    }

    fn clicks(bpm: f32, secs: f32) -> (Vec<f32>, Vec<usize>) {
        let frames = (secs * SR as f32) as usize;
        let period = (60.0 / bpm * SR as f32).round() as usize;
        let mut out = vec![0.0; frames * 2];
        let mut at = Vec::new();
        let mut t = period / 2;
        while t + 200 < frames {
            for n in 0..200 {
                let s = (1.0 - n as f32 / 200.0) * (n as f32 * 0.6).sin() * 0.8;
                out[(t + n) * 2] = s;
                out[(t + n) * 2 + 1] = s;
            }
            at.push(t);
            t += period;
        }
        (out, at)
    }

    fn crossings(x: &[f32]) -> usize {
        x.chunks(2)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    #[test]
    fn time_map_round_trips_through_the_ramp() {
        for speed in [0.93, 1.07] {
            let map = TimeMap::new(40_000.0, speed);
            for s in [0.0, 1.0, 10_000.0, 39_999.0, 40_000.0, 90_000.0] {
                let back = map.src_of(map.out_of(s));
                assert!((back - s).abs() < 1e-6 * s.max(1.0), "{speed} {s} {back}");
            }
            let t_ramp = map.out_of(40_000.0);
            let later = map.out_of(50_000.0);
            assert!((later - t_ramp - 10_000.0 / speed).abs() < 1e-6);
        }
        let flat = TimeMap::new(0.0, 1.05);
        assert!((flat.out_of(10_500.0) - 10_000.0).abs() < 1e-9);
        assert!((flat.src_of(10_000.0) - 10_500.0).abs() < 1e-9);
    }

    #[test]
    fn unit_speed_render_is_the_source() {
        let src = sine(330.0, 3.0);
        let start = 8_000;
        let out = render_tail(&src, 2, start, TimeMap::new(0.0, 1.0), 30_000, SR);
        assert_eq!(out.len(), 60_000);
        for (i, (a, b)) in out.iter().zip(&src[start * 2..]).enumerate() {
            assert!((a - b).abs() < 1e-5, "sample {i}: {a} vs {b}");
        }
    }

    #[test]
    fn tail_starts_on_the_source_even_with_an_instant_bend() {
        let src = sine(220.0, 3.0);
        let start = 10_000;
        let out = render_tail(&src, 2, start, TimeMap::new(0.0, 1.08), 20_000, SR);
        let hop = (SR as f32 * 0.021).round() as usize;
        for i in 0..hop * 2 {
            assert!(
                (out[i] - src[start * 2 + i]).abs() < 1e-5,
                "seam sample {i}"
            );
        }
    }

    #[test]
    fn bend_keeps_pitch_where_resampling_would_not() {
        let src = sine(440.0, 6.0);
        let map = TimeMap::new(0.0, 0.94);
        let frames = map.out_of(4.0 * SR as f64) as usize;
        let out = render_tail(&src, 2, SR as usize, map, frames, SR);
        let mid = &out[SR as usize..(frames - SR as usize / 2) * 2];
        let secs = mid.len() as f32 / 2.0 / SR as f32;
        let freq = crossings(mid) as f32 / 2.0 / secs;
        assert!((freq - 440.0).abs() < 440.0 * 0.015, "freq={freq}");
    }

    #[test]
    fn beats_land_where_the_time_map_says() {
        let (src, beats) = clicks(120.0, 14.0);
        let start = SR as usize;
        let map = TimeMap::new(2.0 * SR as f64, 1.06);
        let frames = map.out_of(10.0 * SR as f64) as usize;
        let out = render_tail(&src, 2, start, map, frames, SR);
        let slack = (0.02 * SR as f32) as i64;
        let mut checked = 0;
        for &b in beats.iter().filter(|&&b| b > start + 2_000) {
            let want = map.out_of((b - start) as f64).round() as i64;
            if want + slack >= frames as i64 {
                break;
            }
            let lo = (want - 2 * slack).max(0);
            let hi = (want + 2 * slack).min(frames as i64);
            let hit = (lo..hi)
                .find(|&o| out[o as usize * 2].abs() > 0.15)
                .unwrap_or_else(|| panic!("no click near {want}"));
            assert!(
                (hit - want).abs() <= slack,
                "beat {b}: want {want} got {hit}"
            );
            checked += 1;
        }
        assert!(checked > 15, "checked {checked}");
    }
}
