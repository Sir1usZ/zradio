#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixMode {
    Cut,
    Crossfade,
    AutoMix,
}

impl MixMode {
    pub fn next(self) -> Self {
        match self {
            Self::Cut => Self::Crossfade,
            Self::Crossfade => Self::AutoMix,
            Self::AutoMix => Self::Cut,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cut => "CUT",
            Self::Crossfade => "FADE",
            Self::AutoMix => "AUTO",
        }
    }

    pub fn uses_fade(self) -> bool {
        !matches!(self, Self::Cut)
    }
}

pub fn equal_power_gains(t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let angle = t * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

pub fn fade_frames(sample_rate: u32, track_frames: usize, fade_secs: f32) -> usize {
    if track_frames < 2 {
        return 0;
    }
    let wanted = (sample_rate as f32 * fade_secs).round() as usize;
    let cap = (track_frames / 3).max(1);
    wanted.min(cap).min(track_frames.saturating_sub(1))
}

pub fn mix_stereo_frame(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    let (ga, gb) = equal_power_gains(t);
    [a[0] * ga + b[0] * gb, a[1] * ga + b[1] * gb]
}

/// Where AutoMix splits the bass a mix swaps from the rest.
pub const CROSSOVER_HZ: f32 = 160.0;

/// One second-order section, transposed direct form II.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Biquad {
    pub(crate) b0: f64,
    pub(crate) b1: f64,
    pub(crate) b2: f64,
    pub(crate) a1: f64,
    pub(crate) a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// From coefficients normalised to a0 = 1.
    pub(crate) fn new([b0, b1, b2]: [f64; 3], [a1, a2]: [f64; 2]) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Second-order Butterworth low- or high-pass at `hz`, by bilinear transform.
    fn butterworth(sample_rate: u32, hz: f64, high: bool) -> Self {
        let k = (std::f64::consts::PI * hz / sample_rate.max(1) as f64).tan();
        let q = std::f64::consts::SQRT_2;
        let a0 = 1.0 + q * k + k * k;
        let a = [2.0 * (k * k - 1.0) / a0, (1.0 - q * k + k * k) / a0];
        let b = if high {
            [1.0, -2.0, 1.0]
        } else {
            [k * k, 2.0 * k * k, k * k]
        };
        Self::new(b.map(|b| b / a0), a)
    }

    /// What a `Crossover` at `hz` sums its bands back to: the all-pass B(-s)/B(s) of its
    /// Butterworth B, as one section.
    pub(crate) fn crossover_sum(sample_rate: u32, hz: f32) -> Self {
        let b = Self::butterworth(sample_rate, f64::from(hz), false);
        Self::new([b.a2, b.a1, 1.0], [b.a1, b.a2])
    }

    pub(crate) fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// A stereo frame split at a crossover.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bands {
    pub low: [f32; 2],
    pub high: [f32; 2],
}

impl Bands {
    pub fn sum(self) -> [f32; 2] {
        [self.low[0] + self.high[0], self.low[1] + self.high[1]]
    }
}

/// Linkwitz-Riley crossover, 24 dB an octave: each band is two Butterworth sections, and
/// the bands sum back to the input with only its phase turned, so a deck can run through
/// it the whole time and a mix can split it without a jump.
#[derive(Debug, Clone, Copy)]
pub struct Crossover {
    /// Per channel, the two sections of each band.
    low: [[Biquad; 2]; 2],
    high: [[Biquad; 2]; 2],
}

impl Crossover {
    pub fn new(sample_rate: u32, hz: f32) -> Self {
        let low = Biquad::butterworth(sample_rate, f64::from(hz), false);
        let high = Biquad::butterworth(sample_rate, f64::from(hz), true);
        Self {
            low: [[low; 2]; 2],
            high: [[high; 2]; 2],
        }
    }

    pub fn split(&mut self, x: [f32; 2]) -> Bands {
        let mut bands = Bands::default();
        for (ch, s) in x.into_iter().enumerate() {
            let s = f64::from(s);
            bands.low[ch] = cascade(&mut self.low[ch], s) as f32;
            bands.high[ch] = cascade(&mut self.high[ch], s) as f32;
        }
        bands
    }

    /// Forgets the audio so far, for a deck that has gone unsplit for a while.
    pub fn reset(&mut self) {
        for f in self.low.iter_mut().chain(&mut self.high).flatten() {
            f.z1 = 0.0;
            f.z2 = 0.0;
        }
    }
}

fn cascade([first, second]: &mut [Biquad; 2], x: f64) -> f64 {
    second.process(first.process(x))
}

pub fn bass_swap_gains(t: f32) -> (f32, f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let (a_high, b_high) = equal_power_gains(t);
    let bass_t = ((t - 0.42) / 0.18).clamp(0.0, 1.0);
    let bass_t = bass_t * bass_t * (3.0 - 2.0 * bass_t);
    (1.0 - bass_t, a_high, bass_t, b_high)
}

pub fn mix_bass_swap(a: Bands, b: Bands, t: f32) -> [f32; 2] {
    let (al, ah, bl, bh) = bass_swap_gains(t);
    [
        a.low[0] * al + a.high[0] * ah + b.low[0] * bl + b.high[0] * bh,
        a.low[1] * al + a.high[1] * ah + b.low[1] * bl + b.high[1] * bh,
    ]
}

pub fn mix_filter_blend(a: Bands, b: Bands, t: f32) -> [f32; 2] {
    let open = t.clamp(0.0, 1.0);
    let close = 1.0 - open;
    let a_out = [
        a.low[0] * close + a.high[0] * close * close,
        a.low[1] * close + a.high[1] * close * close,
    ];
    let b_out = [b.low[0] + b.high[0] * open, b.low[1] + b.high[1] * open];
    let (ga, gb) = equal_power_gains(t * 0.85 + 0.08);
    [a_out[0] * ga + b_out[0] * gb, a_out[1] * ga + b_out[1] * gb]
}

pub fn mix_echo_out(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    let duck = (1.0 - t.clamp(0.0, 1.0)).powf(1.6);
    let rise = t.clamp(0.0, 1.0).powf(0.7);
    [a[0] * duck + b[0] * rise, a[1] * duck + b[1] * rise]
}

pub fn resample_linear(
    input: &[f32],
    in_channels: usize,
    in_rate: u32,
    out_channels: usize,
    out_rate: u32,
) -> Vec<f32> {
    if input.is_empty() || in_channels == 0 || out_channels == 0 || in_rate == 0 || out_rate == 0 {
        return Vec::new();
    }
    let in_frames = input.len() / in_channels;
    if in_frames == 0 {
        return Vec::new();
    }
    if in_rate == out_rate && in_channels == out_channels {
        return input.to_vec();
    }
    let out_frames =
        ((in_frames as u64 * u64::from(out_rate)) / u64::from(in_rate)).max(1) as usize;
    let mut out = vec![0.0; out_frames * out_channels];
    let ratio = in_rate as f64 / out_rate as f64;
    for of in 0..out_frames {
        let src = of as f64 * ratio;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(in_frames.saturating_sub(1));
        let frac = (src - i0 as f64) as f32;
        for ch in 0..out_channels {
            let s0 = sample_at(input, in_channels, i0, ch);
            let s1 = sample_at(input, in_channels, i1, ch);
            out[of * out_channels + ch] = s0 + (s1 - s0) * frac;
        }
    }
    out
}

fn sample_at(input: &[f32], in_channels: usize, frame: usize, out_ch: usize) -> f32 {
    if in_channels == 1 {
        return input[frame];
    }
    let ch = out_ch.min(in_channels - 1);
    input[frame * in_channels + ch]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_power_starts_on_a_and_ends_on_b() {
        let (a0, b0) = equal_power_gains(0.0);
        let (a1, b1) = equal_power_gains(1.0);
        assert!((a0 - 1.0).abs() < 1e-5 && b0.abs() < 1e-5);
        assert!(a1.abs() < 1e-5 && (b1 - 1.0).abs() < 1e-5);
        let (am, bm) = equal_power_gains(0.5);
        assert!((am - bm).abs() < 1e-5);
        assert!((am * am + bm * bm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn fade_frames_never_eats_the_whole_track() {
        assert_eq!(fade_frames(48_000, 48_000 * 30, 6.0), 48_000 * 6);
        assert_eq!(fade_frames(48_000, 48_000, 6.0), 48_000 / 3);
        assert_eq!(fade_frames(48_000, 1, 6.0), 0);
    }

    #[test]
    fn mix_mode_cycles() {
        assert_eq!(MixMode::Cut.next(), MixMode::Crossfade);
        assert_eq!(MixMode::Crossfade.next(), MixMode::AutoMix);
        assert_eq!(MixMode::AutoMix.next(), MixMode::Cut);
        assert!(MixMode::AutoMix.uses_fade());
        assert!(!MixMode::Cut.uses_fade());
    }

    #[test]
    fn resample_mono_to_stereo_keeps_length_ratio() {
        let input = vec![0.0, 1.0, 0.0, -1.0];
        let out = resample_linear(&input, 1, 44_100, 2, 44_100);
        assert_eq!(out.len(), 8);
        assert_eq!(out[2], 1.0);
        assert_eq!(out[3], 1.0);
    }

    #[test]
    fn mix_stereo_frame_is_a_at_start() {
        let mixed = mix_stereo_frame([0.8, -0.2], [0.1, 0.4], 0.0);
        assert!((mixed[0] - 0.8).abs() < 1e-5);
        assert!((mixed[1] + 0.2).abs() < 1e-5);
    }

    #[test]
    fn bass_swap_keeps_outgoing_kick_until_mid() {
        let (al0, ah0, bl0, bh0) = bass_swap_gains(0.0);
        assert!((al0 - 1.0).abs() < 1e-5);
        assert!(bl0.abs() < 1e-5);
        assert!((ah0 - 1.0).abs() < 1e-5);
        assert!(bh0.abs() < 1e-5);

        let (al_mid, _, bl_mid, _) = bass_swap_gains(0.35);
        assert!(al_mid > 0.85);
        assert!(bl_mid < 0.15);

        let (al1, _, bl1, bh1) = bass_swap_gains(1.0);
        assert!(al1.abs() < 1e-5);
        assert!((bl1 - 1.0).abs() < 1e-5);
        assert!((bh1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn crossover_bands_sum_flat_and_part_steeply() {
        let db = |x: f64| 20.0 * x.log10();
        // Level of the low band, the high band and their sum for a unit sine at `hz`, after
        // a second for the filters to settle.
        let levels = |hz: f64| {
            let mut split = Crossover::new(48_000, 160.0);
            let mut peak = [0.0f64; 3];
            for i in 0..96_000 {
                let x = (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32;
                let bands = split.split([x, x]);
                if i >= 48_000 {
                    let got = [bands.low[0], bands.high[0], bands.sum()[0]];
                    for (p, g) in peak.iter_mut().zip(got) {
                        *p = p.max(f64::from(g.abs()));
                    }
                }
            }
            peak.map(db)
        };
        for hz in [40.0, 80.0, 160.0, 320.0, 640.0, 2_560.0] {
            let [_, _, sum] = levels(hz);
            assert!(sum.abs() < 0.05, "{hz} Hz sums to {sum:.2} dB");
        }
        let [low, high, _] = levels(160.0);
        assert!((low + 6.02).abs() < 0.05 && (high + 6.02).abs() < 0.05);
        // Two octaves out each band is 48 dB down.
        let [_, high, _] = levels(40.0);
        let [low, _, _] = levels(640.0);
        assert!(high < -46.0 && low < -46.0, "{high:.1} and {low:.1} dB");
    }

    #[test]
    fn crossover_sums_to_its_all_pass() {
        let mut split = Crossover::new(44_100, CROSSOVER_HZ);
        let mut sum = Biquad::crossover_sum(44_100, CROSSOVER_HZ);
        let mut noise = 0x2545_f491_u32;
        for _ in 0..44_100 {
            noise ^= noise << 13;
            noise ^= noise >> 17;
            noise ^= noise << 5;
            let x = noise as f32 / u32::MAX as f32 - 0.5;
            let want = sum.process(f64::from(x));
            let got = split.split([x, x]).sum()[0];
            assert!((f64::from(got) - want).abs() < 1e-5, "{got} is not {want}");
        }
    }
}
