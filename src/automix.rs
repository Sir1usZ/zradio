use crate::analysis::TrackAnalysis;
use crate::dsp::{fade_frames, MixMode};
use crate::library::Track;
use crate::stretch::TimeMap;

/// Lead the bend needs before it starts: worker render plus the UI tick that installs it.
const BEND_LEAD_SECS: f64 = 1.0;
const SKIP_LEAD_SECS: f64 = 0.25;
const RAMP_BARS: f64 = 4.0;
pub const RAMP_MAX_SECS: f32 = 8.0;
/// How long before the bend AutoMix wants the next track decoded and analysed.
pub const PREFETCH_LEAD_SECS: f32 = 6.0;

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
    /// Speed the outgoing tail glides to so it plays at the incoming tempo; 1 means no bend.
    pub speed: f32,
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

pub fn bpm_compatible(a: f32, b: f32) -> bool {
    if a <= 1.0 || b <= 1.0 {
        return false;
    }
    let ratio = (a / b).max(b / a);
    ratio <= 1.08
}

/// Only bend when the tempos are close enough to lock; a clashing pair is mixed unbent.
pub fn tail_speed(current_bpm: Option<f32>, next_bpm: Option<f32>) -> f32 {
    match (current_bpm, next_bpm) {
        (Some(a), Some(b)) if bpm_compatible(a, b) => b / a,
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
    plan_pair(current, next, next_index, mode, skip)
}

pub fn plan_pair(
    current: Option<&TrackAnalysis>,
    next: Option<&TrackAnalysis>,
    next_index: usize,
    mode: MixMode,
    skip: bool,
) -> TransitionPlan {
    let (score, key_ok, bpm_ok) = match (current, next) {
        (Some(a), Some(b)) => pair_score(a, b),
        _ => (0.0, false, false),
    };
    let speed = if mode == MixMode::AutoMix {
        tail_speed(current.and_then(|a| a.bpm), next.and_then(|b| b.bpm))
    } else {
        1.0
    };
    let style = if mode == MixMode::AutoMix {
        select_style(key_ok, bpm_ok, current, next)
    } else {
        MixStyle::Quick
    };
    // A bent overlap runs at the incoming tempo, so its bars are the incoming track's.
    let tempo = if speed != 1.0 { next } else { current };
    let fade_secs = match (mode, style, skip) {
        (MixMode::AutoMix, MixStyle::Echo | MixStyle::Quick, _) => 4.0,
        (MixMode::AutoMix, MixStyle::Filter, _) => 6.5,
        _ => fade_secs_for(mode, tempo, skip),
    };
    TransitionPlan {
        next_index,
        fade_secs,
        speed,
        key_ok,
        bpm_ok,
        score,
        style,
        current_mix_out: current.map(|a| a.mix_out),
        next_mix_in: next.map(|a| a.mix_in),
    }
}

/// From `start` on, the outgoing deck plays its source through `map`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TailBend {
    pub start: usize,
    pub map: TimeMap,
    /// Output frames from `start` to the end of the source.
    pub out_frames: usize,
}

/// Outgoing side of a transition, in source frames of the current deck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cue {
    /// Where the overlap starts; a bend has finished gliding by here.
    pub mix_at: usize,
    pub bend: Option<TailBend>,
}

/// Mixer side of a transition, once the incoming buffer is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Outgoing deck frame (bent time) where the overlap starts.
    pub start_at: usize,
    /// First incoming frame, on the outgoing track's beat phase.
    pub incoming_pos: usize,
    pub fade: usize,
}

fn beat_grid(a: &TrackAnalysis) -> Option<(f64, f64)> {
    a.bpm
        .filter(|&bpm| bpm > 1.0)
        .map(|bpm| (a.first_beat as f64, 60.0 / bpm as f64))
}

/// Nearest grid line on one side of `t`; lines sit at `first + n * step`.
fn grid_line(t: f64, first: f64, step: f64, up: bool) -> f64 {
    let n = (t - first) / step;
    let n = if up {
        (n - 1e-6).ceil()
    } else {
        (n + 1e-6).floor()
    };
    first + n * step
}

/// Where the outgoing deck hands over and, when the tempos lock, how its tail glides to the
/// incoming tempo. A natural mix finishes the glide on the bar the overlap starts on; a skip
/// glides over one beat and starts on the next beat line.
pub fn cue(
    current: Option<&TrackAnalysis>,
    plan: &TransitionPlan,
    pos: usize,
    frames: usize,
    sample_rate: u32,
    skip: bool,
) -> Cue {
    let sr = sample_rate.max(1) as f64;
    let at = |secs: f64| ((secs.max(0.0) * sr).round() as usize).min(frames);
    let pos_s = pos as f64 / sr;
    let end_s = frames as f64 / sr;
    let grid = current.and_then(beat_grid);
    let speed = plan.speed as f64;
    let fade = plan.fade_secs.max(0.5) as f64;
    if let (Some((first, beat)), true) = (grid, speed != 1.0) {
        let overlap = fade * speed;
        let span = if skip {
            let mix_s = grid_line(pos_s + SKIP_LEAD_SECS + beat, first, beat, true);
            (mix_s - beat, mix_s)
        } else {
            let bar = beat * 4.0;
            let ramp = (RAMP_BARS * bar).clamp(2.0, RAMP_MAX_SECS as f64);
            let lead = pos_s + BEND_LEAD_SECS;
            let ideal = plan.current_mix_out.map_or(end_s, f64::from) - overlap;
            let mut mix_s = grid_line(ideal, first, bar, false);
            if mix_s < lead + beat {
                mix_s = grid_line(lead + beat, first, bar, true);
            }
            ((mix_s - ramp).max(lead), mix_s)
        };
        let (start_s, mix_s) = span;
        if mix_s + overlap <= end_s {
            let (start, mix_at) = (at(start_s), at(mix_s));
            let map = TimeMap::new((mix_at - start) as f64, speed);
            let out_frames = map.out_of((frames - start) as f64).ceil() as usize;
            return Cue {
                mix_at,
                bend: Some(TailBend {
                    start,
                    map,
                    out_frames,
                }),
            };
        }
    }
    let latest = frames.saturating_sub((fade * sr) as usize);
    let unbent = |mix_at: usize| Cue {
        mix_at: mix_at.min(latest).max(pos),
        bend: None,
    };
    let Some(mix_out) = plan.current_mix_out.filter(|_| !skip) else {
        return unbent(pos);
    };
    let ideal = mix_out as f64 - fade;
    let mix_s = match grid {
        Some((first, beat)) => {
            let down = grid_line(ideal, first, beat * 4.0, false);
            if down < pos_s {
                grid_line(pos_s, first, beat * 4.0, true)
            } else {
                down
            }
        }
        None => ideal,
    };
    unbent(at(mix_s))
}

/// Fixes the overlap on the outgoing deck's clock and starts the incoming track on the same
/// beat phase the outgoing one has at `cue.mix_at`.
pub fn place(
    cue: &Cue,
    current: Option<&TrackAnalysis>,
    next: Option<&TrackAnalysis>,
    plan: &TransitionPlan,
    frames: usize,
    next_frames: usize,
    sample_rate: u32,
) -> Placement {
    let sr = sample_rate.max(1) as f64;
    let (start_at, out_left) = match cue.bend {
        Some(b) => {
            let at = b.start + b.map.out_of((cue.mix_at - b.start) as f64).round() as usize;
            (at, (b.start + b.out_frames).saturating_sub(at))
        }
        None => (cue.mix_at, frames.saturating_sub(cue.mix_at)),
    };
    let phase = current.and_then(beat_grid).map_or(0.0, |(first, beat)| {
        let p = ((cue.mix_at as f64 / sr - first) / beat).rem_euclid(1.0);
        if p > 1.0 - 1e-6 {
            0.0
        } else {
            p
        }
    });
    let next_beat = next
        .and_then(beat_grid)
        .filter(|_| plan.bpm_ok)
        .or(current.and_then(beat_grid))
        .map_or(0.0, |(_, beat)| beat);
    let mix_in = plan.next_mix_in.map_or(0.0, |s| s.max(0.0) as f64);
    let incoming_pos =
        (((mix_in + phase * next_beat) * sr).round() as usize).min(next_frames.saturating_sub(1));
    let fade = fade_frames(sample_rate, frames.min(next_frames), plan.fade_secs)
        .min(out_left)
        .min(next_frames.saturating_sub(incoming_pos));
    Placement {
        start_at,
        incoming_pos,
        fade,
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
        assert!((plan.speed - 126.0 / 128.0).abs() < 1e-6);
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

    fn key8() -> Camelot {
        Camelot {
            number: 8,
            minor: false,
        }
    }

    #[test]
    fn natural_bend_glides_into_the_bar_the_overlap_starts_on() {
        let a = analysis(128.0, key8(), 0.2);
        let mut b = analysis(120.0, key8(), 0.2);
        b.mix_in = 16.0;
        let plan = plan_pair(Some(&a), Some(&b), 1, MixMode::AutoMix, false);
        assert!((plan.speed - 120.0 / 128.0).abs() < 1e-6);
        let sr = 48_000;
        let frames = 200 * sr as usize;
        let c = cue(Some(&a), &plan, 150 * sr as usize, frames, sr, false);
        let bend = c.bend.expect("compatible tempos should bend");
        let bar = 240.0 / 128.0;
        let overlap = plan.fade_secs as f64 * plan.speed as f64;
        let mix_s = c.mix_at as f64 / sr as f64;
        assert!(
            (mix_s / bar - (mix_s / bar).round()).abs() < 1e-6,
            "on a bar line"
        );
        assert!(mix_s + overlap <= 180.0 + 1e-9, "overlap ends by mix_out");
        assert!(mix_s + bar + overlap > 180.0, "latest bar that fits");
        assert_eq!(bend.map.ramp(), (c.mix_at - bend.start) as f64);
        assert!(
            (bend.map.ramp() / sr as f64 - 4.0 * bar).abs() < 1e-3,
            "4 bar glide"
        );

        let p = place(&c, Some(&a), Some(&b), &plan, frames, frames, sr);
        let glide_out = bend.start as f64 + bend.map.out_of(bend.map.ramp());
        assert!((p.start_at as f64 - glide_out).abs() <= 1.0);
        assert_eq!(p.incoming_pos, 16 * sr as usize, "downbeat meets downbeat");
        assert_eq!(p.fade, 8 * sr as usize);

        let late = cue(Some(&a), &plan, 178 * sr as usize, frames, sr, false);
        let late_bend = late.bend.expect("a late cue shortens the glide");
        assert!(
            late_bend.start >= 179 * sr as usize,
            "never glides in the past"
        );
        assert!(late.mix_at > late_bend.start);
    }

    #[test]
    fn skip_bend_glides_over_one_beat_to_the_next_beat_line() {
        let a = analysis(128.0, key8(), 0.2);
        let b = analysis(124.0, key8(), 0.2);
        let plan = plan_pair(Some(&a), Some(&b), 1, MixMode::AutoMix, false);
        let sr = 48_000;
        let pos = 60 * sr as usize;
        let c = cue(Some(&a), &plan, pos, 200 * sr as usize, sr, true);
        let bend = c.bend.expect("a skip still locks tempo");
        let beat = 60.0 / 128.0;
        let mix_s = c.mix_at as f64 / sr as f64;
        assert!(
            (mix_s / beat - (mix_s / beat).round()).abs() < 1e-6,
            "on a beat line"
        );
        let wait = mix_s - 60.0;
        assert!(wait > SKIP_LEAD_SECS + beat - 1e-9 && wait <= SKIP_LEAD_SECS + 2.0 * beat);
        assert!(bend.start >= pos);
        assert!((bend.map.ramp() / sr as f64 - beat).abs() < 1e-6);
    }

    #[test]
    fn clashing_tempos_mix_unbent() {
        let a = analysis(128.0, key8(), 0.2);
        let b = analysis(90.0, key8(), 0.2);
        let plan = plan_pair(Some(&a), Some(&b), 1, MixMode::AutoMix, false);
        assert_eq!(plan.speed, 1.0);
        let c = cue(Some(&a), &plan, 0, 200 * 48_000, 48_000, false);
        assert_eq!(c.bend, None);
        let fit = analysis(126.0, key8(), 0.2);
        let crossfade = plan_pair(Some(&a), Some(&fit), 1, MixMode::Crossfade, false);
        assert_eq!(crossfade.speed, 1.0);
    }

    #[test]
    fn unbent_mix_waits_for_a_bar_line() {
        let a = analysis(120.0, key8(), 0.2);
        let b = analysis(120.0, key8(), 0.2);
        let plan = plan_pair(Some(&a), Some(&b), 1, MixMode::AutoMix, false);
        assert_eq!(plan.speed, 1.0);
        let sr = 48_000;
        let frames = 200 * sr as usize;
        let c = cue(Some(&a), &plan, 150 * sr as usize, frames, sr, false);
        let want = Cue {
            mix_at: 172 * sr as usize,
            bend: None,
        };
        assert_eq!(c, want);
        let late = cue(Some(&a), &plan, 175 * sr as usize, frames, sr, false);
        assert_eq!(late.mix_at, 176 * sr as usize, "never rewinds, waits a bar");
    }

    #[test]
    fn unbent_skip_starts_now_on_the_outgoing_beat_phase() {
        let a = analysis(120.0, key8(), 0.2);
        let mut b = analysis(120.0, key8(), 0.2);
        b.mix_in = 4.0;
        let plan = plan_pair(Some(&a), Some(&b), 1, MixMode::AutoMix, false);
        let sr = 48_000;
        let frames = 20 * sr as usize;
        let pos = sr as usize * 5 / 4;
        let c = cue(Some(&a), &plan, pos, frames, sr, true);
        assert_eq!(
            c,
            Cue {
                mix_at: pos,
                bend: None
            }
        );
        let p = place(&c, Some(&a), Some(&b), &plan, frames, frames, sr);
        assert_eq!(p.start_at, pos);
        assert_eq!(
            p.incoming_pos,
            sr as usize * 17 / 4,
            "half a beat past mix_in"
        );
    }
}
