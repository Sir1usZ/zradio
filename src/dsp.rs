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

#[derive(Debug, Clone, Copy, Default)]
pub struct OnePole {
    prev: [f32; 2],
}

impl OnePole {
    pub fn lowpass(&mut self, x: [f32; 2], alpha: f32) -> [f32; 2] {
        self.prev[0] += alpha * (x[0] - self.prev[0]);
        self.prev[1] += alpha * (x[1] - self.prev[1]);
        self.prev
    }
}

pub fn bass_swap_gains(t: f32) -> (f32, f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let (a_high, b_high) = equal_power_gains(t);
    let bass_t = ((t - 0.42) / 0.18).clamp(0.0, 1.0);
    let bass_t = bass_t * bass_t * (3.0 - 2.0 * bass_t);
    (1.0 - bass_t, a_high, bass_t, b_high)
}

pub fn mix_bass_swap(
    a: [f32; 2],
    b: [f32; 2],
    a_lp: &mut OnePole,
    b_lp: &mut OnePole,
    t: f32,
    alpha: f32,
) -> [f32; 2] {
    let a_low = a_lp.lowpass(a, alpha);
    let b_low = b_lp.lowpass(b, alpha);
    let a_high = [a[0] - a_low[0], a[1] - a_low[1]];
    let b_high = [b[0] - b_low[0], b[1] - b_low[1]];
    let (al, ah, bl, bh) = bass_swap_gains(t);
    [
        a_low[0] * al + a_high[0] * ah + b_low[0] * bl + b_high[0] * bh,
        a_low[1] * al + a_high[1] * ah + b_low[1] * bl + b_high[1] * bh,
    ]
}

pub fn mix_filter_blend(
    a: [f32; 2],
    b: [f32; 2],
    a_lp: &mut OnePole,
    b_lp: &mut OnePole,
    t: f32,
    alpha: f32,
) -> [f32; 2] {
    let a_low = a_lp.lowpass(a, alpha);
    let b_low = b_lp.lowpass(b, alpha);
    let open = t.clamp(0.0, 1.0);
    let close = 1.0 - open;
    let a_out = [
        a_low[0] * close + (a[0] - a_low[0]) * close * close,
        a_low[1] * close + (a[1] - a_low[1]) * close * close,
    ];
    let b_out = [
        b_low[0] + (b[0] - b_low[0]) * open,
        b_low[1] + (b[1] - b_low[1]) * open,
    ];
    let (ga, gb) = equal_power_gains(t * 0.85 + 0.08);
    [a_out[0] * ga + b_out[0] * gb, a_out[1] * ga + b_out[1] * gb]
}

pub fn mix_echo_out(a: [f32; 2], b: [f32; 2], t: f32) -> [f32; 2] {
    let duck = (1.0 - t.clamp(0.0, 1.0)).powf(1.6);
    let rise = t.clamp(0.0, 1.0).powf(0.7);
    [a[0] * duck + b[0] * rise, a[1] * duck + b[1] * rise]
}

pub fn lp_alpha(sample_rate: u32, cutoff_hz: f32) -> f32 {
    let sr = sample_rate.max(1) as f32;
    (2.0 * std::f32::consts::PI * cutoff_hz / sr).clamp(0.01, 0.45)
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
}
