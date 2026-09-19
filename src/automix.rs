use crate::analysis::TrackAnalysis;
use crate::dsp::MixMode;
use crate::library::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixStyle {
    BassSwap,
    Filter,
    Echo,
    Quick,
}

impl MixStyle {
    pub fn label(self) -> &'static str {
        match self {
            Self::BassSwap => "bass-swap",
            Self::Filter => "filter",
            Self::Echo => "echo-out",
            Self::Quick => "quick",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransitionPlan {
    pub next_index: usize,
    pub fade_secs: f32,
    pub rate: f32,
    pub key_ok: bool,
    pub bpm_ok: bool,
    pub score: f32,
    pub style: MixStyle,
    pub current_mix_out: Option<f32>,
    pub next_mix_in: Option<f32>,
}

pub fn select_style(
    key_ok: bool,
    bpm_ok: bool,
    current: Option<&TrackAnalysis>,
    next: Option<&TrackAnalysis>,
) -> MixStyle {
    let energy_jump = match (current, next) {
        (Some(a), Some(b)) => (b.energy - a.energy).abs(),
        _ => 0.0,
    };
    if !bpm_ok {
        return MixStyle::Echo;
    }
    if !key_ok {
        return if energy_jump > 0.18 {
            MixStyle::Quick
        } else {
            MixStyle::Filter
        };
    }
    if energy_jump > 0.22 {
        MixStyle::Filter
    } else {
        MixStyle::BassSwap
    }
}

pub fn beat_seconds(bpm: f32) -> f32 {
    if bpm <= 1.0 {
        0.5
    } else {
        60.0 / bpm
    }
}

pub fn aligned_mix_in(
    current_pos_secs: f32,
    current: Option<&TrackAnalysis>,
    next_mix_in: f32,
    rate: f32,
) -> f32 {
    let Some(cur) = current else {
        return next_mix_in.max(0.0);
    };
    let bpm = cur.bpm.unwrap_or(120.0);
    let beat = beat_seconds(bpm);
    if beat <= 0.001 {
        return next_mix_in.max(0.0);
    }
    let phase = (current_pos_secs - cur.first_beat).rem_euclid(beat);
    (next_mix_in + phase / rate.max(0.01)).max(0.0)
}

pub fn mix_start_frame(
    current_pos: usize,
    remaining: usize,
    mix_out_secs: Option<f32>,
    fade_secs: f32,
    sample_rate: u32,
) -> usize {
    let sr = sample_rate.max(1) as f32;
    let fade = (fade_secs.max(0.5) * sr).round() as usize;
    let by_phrase = mix_out_secs
        .map(|secs| ((secs.max(0.0) * sr) as usize).saturating_sub(fade))
        .unwrap_or(current_pos);
    let start = by_phrase.max(current_pos);
    let last = current_pos.saturating_add(remaining.saturating_sub(1));
    start.min(last)
}

pub fn bpm_compatible(a: f32, b: f32) -> bool {
    if a <= 1.0 || b <= 1.0 {
        return false;
    }
    let ratio = (a / b).max(b / a);
    ratio <= 1.08
}

pub fn match_rate(current_bpm: Option<f32>, next_bpm: Option<f32>) -> f32 {
    match (current_bpm, next_bpm) {
        (Some(a), Some(b)) if b > 1.0 => (a / b).clamp(0.92, 1.08),
        _ => 1.0,
    }
}

pub fn pair_score(current: &TrackAnalysis, next: &TrackAnalysis) -> (f32, bool, bool) {
    let mut score = 0.35;
    let key_ok = match (current.camelot, next.camelot) {
        (Some(a), Some(b)) => a.compatible(b),
        _ => false,
    };
    if key_ok {
        score += 0.35;
    } else if current.camelot.is_none() || next.camelot.is_none() {
        score += 0.08;
    }
    let bpm_ok = match (current.bpm, next.bpm) {
        (Some(a), Some(b)) => bpm_compatible(a, b),
        _ => false,
    };
    if bpm_ok {
        score += 0.25;
        if let (Some(a), Some(b)) = (current.bpm, next.bpm) {
            score += (1.0 - ((a - b).abs() / a).min(0.12)) * 0.08;
        }
    }
    let energy_delta = (current.energy - next.energy).abs();
    score += (0.12 - energy_delta.min(0.12)).max(0.0);
    (score.clamp(0.0, 1.0), key_ok, bpm_ok)
}

pub fn fade_secs_for(mode: MixMode, current: Option<&TrackAnalysis>, skip: bool) -> f32 {
    if skip {
        return 3.5;
    }
    match mode {
        MixMode::Cut => 0.0,
        MixMode::Crossfade => 6.0,
        MixMode::AutoMix => current
            .and_then(|a| a.bpm)
            .map(|bpm| (240.0 / bpm * 4.0).clamp(6.0, 10.0))
            .unwrap_or(8.0),
    }
}

pub fn plan_for(
    current_idx: Option<usize>,
    next_index: usize,
    analyses: &[Option<TrackAnalysis>],
    mode: MixMode,
    skip: bool,
) -> TransitionPlan {
    let current = current_idx.and_then(|i| analyses.get(i).and_then(|a| a.as_ref()));
    let next = analyses.get(next_index).and_then(|a| a.as_ref());
    let (score, key_ok, bpm_ok) = match (current, next) {
        (Some(a), Some(b)) => pair_score(a, b),
        _ => (0.0, false, false),
    };
    let rate = if mode == MixMode::AutoMix {
        match (current, next) {
            (Some(a), Some(b)) => match_rate(a.bpm, b.bpm),
            _ => 1.0,
        }
    } else {
        1.0
    };
    let style = if mode == MixMode::AutoMix {
        select_style(key_ok, bpm_ok, current, next)
    } else {
        MixStyle::Quick
    };
    let fade_secs = match (mode, style, skip) {
        (MixMode::AutoMix, MixStyle::Echo | MixStyle::Quick, _) => 4.0,
        (MixMode::AutoMix, MixStyle::Filter, _) => 6.5,
        _ => fade_secs_for(mode, current, skip),
    };
    TransitionPlan {
        next_index,
        fade_secs,
        rate,
        key_ok,
        bpm_ok,
        score,
        style,
        current_mix_out: current.map(|a| a.mix_out),
        next_mix_in: next.map(|a| a.mix_in / rate.max(0.01)),
    }
}

pub fn pick_next(
    tracks: &[Track],
    current_idx: Option<usize>,
    analyses: &[Option<TrackAnalysis>],
    mode: MixMode,
    taste_boosts: &[f32],
) -> Option<TransitionPlan> {
    let len = tracks.len();
    if len == 0 {
        return None;
    }
    let sequential = current_idx.map(|i| (i + 1) % len).unwrap_or(0);
    if mode != MixMode::AutoMix || len == 1 {
        return Some(plan_for(current_idx, sequential, analyses, mode, false));
    }
    let Some(cur_i) = current_idx else {
        return Some(plan_for(None, sequential, analyses, mode, false));
    };
    let mut best: Option<TransitionPlan> = None;
    for i in 0..len {
        if i == cur_i {
            continue;
        }
        let mut candidate = plan_for(current_idx, i, analyses, mode, false);
        candidate.score += taste_boosts.get(i).copied().unwrap_or(0.0);
        if best.is_none_or(|b| candidate.score > b.score) {
            best = Some(candidate);
        }
    }
    best.or_else(|| Some(plan_for(current_idx, sequential, analyses, mode, false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Camelot;
    use std::path::PathBuf;

    fn analysis(bpm: f32, camelot: Camelot, energy: f32) -> TrackAnalysis {
        TrackAnalysis {
            version: 1,
            mtime: 0,
            size: 0,
            bpm: Some(bpm),
            bpm_confidence: 0.8,
            key_root: Some(0),
            key_minor: camelot.minor,
            key_confidence: 0.8,
            camelot: Some(camelot),
            energy,
            mix_in: 0.0,
            mix_out: 180.0,
            first_beat: 0.0,
            duration: 200.0,
        }
    }

    fn track(name: &str) -> Track {
        Track {
            path: PathBuf::from(name),
            title: name.into(),
        }
    }

    #[test]
    fn prefers_harmonic_neighbor_over_clash() {
        let tracks = vec![track("a"), track("clash"), track("fit")];
        let current = analysis(
            128.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        let clash = analysis(
            90.0,
            Camelot {
                number: 3,
                minor: true,
            },
            0.9,
        );
        let fit = analysis(
            126.0,
            Camelot {
                number: 9,
                minor: false,
            },
            0.22,
        );
        let analyses = vec![Some(current), Some(clash), Some(fit)];
        let plan = pick_next(&tracks, Some(0), &analyses, MixMode::AutoMix, &[]).unwrap();
        assert_eq!(plan.next_index, 2);
        assert!(plan.key_ok);
        assert!(plan.bpm_ok);
        assert!((plan.rate - 128.0 / 126.0).abs() < 0.01);
    }

    #[test]
    fn cut_mode_stays_sequential() {
        let tracks = vec![track("a"), track("b"), track("c")];
        let plan = pick_next(&tracks, Some(0), &[None, None, None], MixMode::Cut, &[]).unwrap();
        assert_eq!(plan.next_index, 1);
        assert_eq!(plan.fade_secs, 0.0);
    }

    #[test]
    fn automix_uses_phrase_anchors() {
        let tracks = vec![track("a"), track("b")];
        let mut a = analysis(
            128.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        a.mix_in = 8.0;
        a.mix_out = 176.0;
        let mut b = analysis(
            128.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        b.mix_in = 16.0;
        b.mix_out = 180.0;
        let plan = pick_next(&tracks, Some(0), &[Some(a), Some(b)], MixMode::AutoMix, &[]).unwrap();
        assert_eq!(plan.current_mix_out, Some(176.0));
        assert_eq!(plan.next_mix_in, Some(16.0));
        assert_eq!(plan.style, MixStyle::BassSwap);
        assert!(plan.fade_secs > 5.0 && plan.fade_secs < 11.0);
    }

    #[test]
    fn clash_picks_echo_or_quick() {
        let a = analysis(
            128.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        let clash = analysis(
            90.0,
            Camelot {
                number: 3,
                minor: true,
            },
            0.9,
        );
        let plan = plan_for(Some(0), 1, &[Some(a), Some(clash)], MixMode::AutoMix, false);
        assert_eq!(plan.style, MixStyle::Echo);
    }

    #[test]
    fn aligned_mix_in_keeps_beat_phase() {
        let cur = analysis(
            120.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        let mix_in = aligned_mix_in(1.25, Some(&cur), 4.0, 1.0);
        let beat = 0.5;
        let phase = (mix_in - 4.0).rem_euclid(beat);
        assert!((phase - 0.25).abs() < 1e-4);
    }

    #[test]
    fn taste_boost_can_beat_close_harmonic_match() {
        let tracks = vec![track("a"), track("fit"), track("loved")];
        let current = analysis(
            128.0,
            Camelot {
                number: 8,
                minor: false,
            },
            0.2,
        );
        let fit = analysis(
            126.0,
            Camelot {
                number: 9,
                minor: false,
            },
            0.22,
        );
        let loved = analysis(
            124.0,
            Camelot {
                number: 9,
                minor: false,
            },
            0.25,
        );
        let analyses = vec![Some(current), Some(fit), Some(loved)];
        let plain = pick_next(&tracks, Some(0), &analyses, MixMode::AutoMix, &[]).unwrap();
        assert_eq!(plain.next_index, 1);
        let boosted = pick_next(
            &tracks,
            Some(0),
            &analyses,
            MixMode::AutoMix,
            &[0.0, 0.0, 0.2],
        )
        .unwrap();
        assert_eq!(boosted.next_index, 2);
    }

    #[test]
    fn mix_start_waits_for_phrase_not_now() {
        let start = mix_start_frame(48_000 * 6, 48_000 * 14, Some(16.0), 8.0, 48_000);
        assert_eq!(start, 48_000 * 8);
        let later = mix_start_frame(48_000 * 10, 48_000 * 10, Some(18.0), 8.0, 48_000);
        assert_eq!(later, 48_000 * 10);
    }
}
