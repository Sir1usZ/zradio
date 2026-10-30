use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};

use crate::analysis::TrackAnalysis;
use crate::automix::{
    at_tempo, cue, fade_secs_for, place, plan_for, plan_pair, Cue, MixStyle, TailBend,
    TransitionPlan, PREFETCH_LEAD_SECS, RAMP_MAX_SECS,
};
use crate::decode::{decode_file, AudioBuf};
use crate::dsp::{
    fade_frames, lp_alpha, mix_bass_swap, mix_echo_out, mix_filter_blend, mix_stereo_frame,
    MixMode, OnePole,
};
use crate::eq::Equalizer;
use crate::library::Track;
use crate::prefs::ShuffleMode;
use crate::remix::RemixMode;
use crate::scope::{PcmRing, PcmSnapshot};
use crate::spectrum::Spectrum;
use crate::stretch::{render_tail, TimeMap};

const FADE_SECS: f32 = 6.0;
const STREAM_REBUILD_GAP: Duration = Duration::from_millis(500);

struct StreamHealth {
    failed: AtomicBool,
}

impl StreamHealth {
    fn new() -> Self {
        Self {
            failed: AtomicBool::new(false),
        }
    }

    fn mark_failed(&self) {
        self.failed.store(true, Ordering::SeqCst);
    }

    fn take_failed(&self) -> bool {
        self.failed.swap(false, Ordering::SeqCst)
    }
}

struct RebuildGate {
    last: Option<Instant>,
}

impl RebuildGate {
    fn new() -> Self {
        Self { last: None }
    }

    fn allow(&mut self, now: Instant, gap: Duration) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < gap)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

fn stream_error_is_fatal(err: &str) -> bool {
    err.contains("POLLERR")
}

fn note_stream_error(health: &StreamHealth, err: impl ToString) {
    let msg = err.to_string();
    if stream_error_is_fatal(&msg) {
        health.mark_failed();
    }
}

struct StreamErrorLog {
    last: Option<String>,
    repeats: u64,
}

impl StreamErrorLog {
    fn new() -> Self {
        Self {
            last: None,
            repeats: 0,
        }
    }

    fn push(&mut self, err: impl ToString) -> Vec<String> {
        let msg = err.to_string();
        match self.last.as_deref() {
            Some(last) if last == msg => {
                self.repeats = self.repeats.saturating_add(1);
                Vec::new()
            }
            Some(last) => {
                let mut out = Vec::new();
                if self.repeats > 1 {
                    out.push(format!("audio stream: {last} (×{})", self.repeats));
                }
                out.push(format!("audio stream: {msg}"));
                self.last = Some(msg);
                self.repeats = 1;
                out
            }
            None => {
                self.last = Some(msg.clone());
                self.repeats = 1;
                vec![format!("audio stream: {msg}")]
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub current: Option<usize>,
    pub selected: usize,
    pub paused: bool,
    pub mix: MixMode,
    pub status: String,
    pub position_frames: usize,
    pub duration_frames: usize,
    pub fading: bool,
    pub volume: f32,
    pub sample_rate: u32,
    pub remix: RemixMode,
    pub current_bpm: Option<f32>,
    pub current_key: Option<String>,
    pub next_hint: Option<String>,
    pub spectrum_levels: [f32; 24],
    pub shuffle: bool,
    pub loop_mode: LoopMode,
    pub pcm: crate::scope::PcmSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::One,
            Self::One => Self::All,
            Self::All => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::One => "ONE",
            Self::All => "ALL",
        }
    }
}

struct ArmedMix {
    index: usize,
    deck: Deck,
    fade: usize,
    start_at: usize,
    plan: TransitionPlan,
}

struct Deck {
    samples: Arc<Vec<f32>>,
    /// Position in deck frames: source frames, or bent-time frames once inside `tail`.
    pos: usize,
    channels: usize,
    tail: Option<BentTail>,
    /// Playback speed relative to the analysed audio; a remix resamples the buffer.
    tempo: f32,
}

/// From `start` on the deck plays `samples`, its source rendered through `map`.
struct BentTail {
    start: usize,
    map: TimeMap,
    samples: Vec<f32>,
}

impl Deck {
    fn from_buf(buf: AudioBuf) -> Self {
        Self {
            samples: Arc::new(buf.samples),
            pos: 0,
            channels: buf.channels.max(1),
            tail: None,
            tempo: 1.0,
        }
    }

    fn source_frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    fn frames(&self) -> usize {
        match &self.tail {
            Some(t) => t.start + t.samples.len() / self.channels,
            None => self.source_frames(),
        }
    }

    fn remaining(&self) -> usize {
        self.frames().saturating_sub(self.pos)
    }

    /// Playhead in source frames, which is what analysis times and the UI clock mean.
    fn source_pos(&self) -> usize {
        match &self.tail {
            Some(t) if self.pos > t.start => {
                let src = t.map.src_of((self.pos - t.start) as f64).round() as usize;
                (t.start + src).min(self.source_frames())
            }
            _ => self.pos,
        }
    }

    fn unbend(&mut self) {
        if self.tail.is_some() {
            self.pos = self.source_pos();
            self.tail = None;
        }
    }

    fn rewind(&mut self) {
        self.tail = None;
        self.pos = 0;
    }

    fn seek_seconds(&mut self, seconds: f32, sample_rate: u32) {
        self.unbend();
        if sample_rate == 0 || self.frames() == 0 {
            return;
        }
        let frame = (seconds.max(0.0) * sample_rate as f32).round() as usize;
        self.pos = frame.min(self.frames().saturating_sub(1));
    }

    fn frame(&self, at: usize) -> [f32; 2] {
        if at >= self.frames() {
            return [0.0, 0.0];
        }
        let (samples, i) = match &self.tail {
            Some(t) if at >= t.start => (&t.samples[..], (at - t.start) * self.channels),
            _ => (&self.samples[..], at * self.channels),
        };
        if self.channels == 1 {
            let s = samples[i];
            [s, s]
        } else {
            [samples[i], samples.get(i + 1).copied().unwrap_or(0.0)]
        }
    }
}

/// Everything the worker needs to render a bend off the mixer lock.
pub struct BendJob {
    samples: Arc<Vec<f32>>,
    channels: usize,
    sample_rate: u32,
    index: usize,
    current_idx: usize,
    next_tempo: f32,
    plan: TransitionPlan,
    bend: TailBend,
    mix_at: usize,
}

impl BendJob {
    pub fn render(self) -> PreparedBend {
        let tail = render_tail(
            &self.samples,
            self.channels,
            self.bend.start,
            self.bend.map,
            self.bend.out_frames,
            self.sample_rate,
        );
        PreparedBend { job: self, tail }
    }
}

/// A rendered outgoing tail, valid only for the deck and plan it was cut for.
pub struct PreparedBend {
    job: BendJob,
    tail: Vec<f32>,
}

pub struct Mixer {
    tracks: Vec<Track>,
    current_idx: Option<usize>,
    incoming_idx: Option<usize>,
    current: Option<Deck>,
    incoming: Option<Deck>,
    fade_total: usize,
    fade_pos: usize,
    paused: bool,
    mix: MixMode,
    remix: RemixMode,
    analyses: Vec<Option<TrackAnalysis>>,
    last_plan: Option<TransitionPlan>,
    armed: Option<ArmedMix>,
    current_lp: OnePole,
    incoming_lp: OnePole,
    volume: f32,
    sample_rate: u32,
    channels: usize,
    status: String,
    done: bool,
    spectrum: Spectrum,
    shuffle: bool,
    shuffle_mode: ShuffleMode,
    recent: Vec<usize>,
    shuffle_state: u64,
    loop_mode: LoopMode,
    taste_boosts: Vec<f32>,
    eq: Equalizer,
    playable: Option<Vec<usize>>,
    pcm: PcmRing,
}

impl Mixer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            tracks: Vec::new(),
            current_idx: None,
            incoming_idx: None,
            current: None,
            incoming: None,
            fade_total: 0,
            fade_pos: 0,
            paused: true,
            mix: MixMode::Crossfade,
            remix: RemixMode::Off,
            analyses: Vec::new(),
            last_plan: None,
            armed: None,
            current_lp: OnePole::default(),
            incoming_lp: OnePole::default(),
            volume: 0.9,
            sample_rate,
            channels: channels.max(1),
            status: "idle".into(),
            done: false,
            spectrum: Spectrum::default(),
            shuffle: false,
            shuffle_mode: ShuffleMode::Off,
            recent: Vec::new(),
            shuffle_state: shuffle_entropy(),
            loop_mode: LoopMode::Off,
            taste_boosts: Vec::new(),
            eq: Equalizer::new(sample_rate, [0.0; 5]),
            playable: None,
            pcm: PcmRing::new(),
        }
    }

    pub fn set_eq_db(&mut self, db: [f32; 5]) {
        self.eq.set_db(self.sample_rate, db);
    }

    pub fn bump_eq(&mut self, band: usize, delta: f32) {
        self.eq.bump(self.sample_rate, band, delta);
        self.status = format!(
            "eq {} {:+.0}",
            Equalizer::labels()[band.min(4)],
            self.eq.db()[band.min(4)]
        );
    }

    pub fn set_eq_band(&mut self, band: usize, db: f32) {
        self.eq.set_band(self.sample_rate, band, db);
        self.status = format!(
            "eq {} {:+.0}",
            Equalizer::labels()[band.min(4)],
            self.eq.db()[band.min(4)]
        );
    }

    pub fn reset_eq(&mut self) {
        self.eq.reset(self.sample_rate);
        self.status = "eq reset".into();
    }

    pub fn eq_db(&self) -> [f32; 5] {
        self.eq.db()
    }

    pub fn set_playable(&mut self, playable: Option<Vec<usize>>) {
        self.playable = playable;
    }

    pub fn playable(&self) -> Option<&[usize]> {
        self.playable.as_deref()
    }

    pub fn set_tracks(&mut self, tracks: Vec<Track>) {
        let new_index: HashMap<&Path, usize> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.path.as_path(), i))
            .collect();
        let remap = |old: usize| {
            self.tracks
                .get(old)
                .and_then(|t| new_index.get(t.path.as_path()).copied())
        };
        let current_idx = self.current_idx.and_then(remap);
        let incoming_idx = self.incoming_idx.and_then(remap);
        let armed_idx = self.armed.as_ref().map(|a| remap(a.index));
        let recent: Vec<usize> = self.recent.iter().filter_map(|&i| remap(i)).collect();
        let mut analyses = vec![None; tracks.len()];
        let mut taste_boosts = vec![0.0; tracks.len()];
        for (old, track) in self.tracks.iter().enumerate() {
            if let Some(&new) = new_index.get(track.path.as_path()) {
                analyses[new] = self.analyses.get(old).cloned().flatten();
                taste_boosts[new] = self.taste_boosts.get(old).copied().unwrap_or(0.0);
            }
        }
        self.current_idx = current_idx;
        self.incoming_idx = incoming_idx;
        match armed_idx {
            Some(Some(index)) => {
                if let Some(armed) = self.armed.as_mut() {
                    armed.index = index;
                }
            }
            Some(None) => self.drop_armed(),
            None => {}
        }
        self.recent = recent;
        self.analyses = analyses;
        self.taste_boosts = taste_boosts;
        self.tracks = tracks;
        if self.tracks.is_empty() {
            self.status = "empty library".into();
        } else {
            self.status = format!("{} tracks", self.tracks.len());
        }
    }

    pub fn set_analysis(&mut self, index: usize, analysis: TrackAnalysis) {
        if index >= self.analyses.len() {
            self.analyses.resize(index + 1, None);
        }
        self.analyses[index] = Some(analysis);
    }

    pub fn analysis_at(&self, index: usize) -> Option<&TrackAnalysis> {
        self.analyses.get(index).and_then(|a| a.as_ref())
    }

    pub fn set_taste_boosts(&mut self, boosts: Vec<f32>) {
        self.taste_boosts = boosts;
        if self.taste_boosts.len() < self.tracks.len() {
            self.taste_boosts.resize(self.tracks.len(), 0.0);
        }
    }

    pub fn listen_ratio(&self) -> f32 {
        let Some(deck) = self.current.as_ref() else {
            return 0.0;
        };
        let frames = deck.source_frames();
        if frames == 0 {
            0.0
        } else {
            (deck.source_pos() as f32 / frames as f32).clamp(0.0, 1.0)
        }
    }

    pub fn remix(&self) -> RemixMode {
        self.remix
    }

    pub fn cycle_remix(&mut self) {
        self.remix = self.remix.next();
        self.status = format!("remix {}", self.remix.label());
    }

    pub fn bump_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.0);
        self.status = format!("vol {:.0}%", self.volume * 100.0);
    }

    pub fn set_track_title(&mut self, index: usize, title: String) {
        if let Some(track) = self.tracks.get_mut(index) {
            track.title = title;
        }
    }

    pub fn spectrum_bars(&self, height: u16) -> Vec<String> {
        self.spectrum.bars(height)
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn mix(&self) -> MixMode {
        self.mix
    }

    pub fn cycle_mix(&mut self) {
        self.mix = self.mix.next();
        self.status = format!("mix {}", self.mix.label());
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn shuffle_mode(&self) -> ShuffleMode {
        self.shuffle_mode
    }

    pub fn set_shuffle_mode(&mut self, mode: ShuffleMode) {
        self.shuffle_mode = mode;
        self.shuffle = mode != ShuffleMode::Off;
        self.status = format!("shuffle {}", mode.label());
    }

    pub fn toggle_shuffle(&mut self) {
        if self.shuffle_mode == ShuffleMode::Off {
            self.set_shuffle_mode(ShuffleMode::Random);
        } else {
            self.set_shuffle_mode(ShuffleMode::Off);
        }
    }

    pub fn remember_played(&mut self, index: usize) {
        if self.tracks.is_empty() {
            return;
        }
        self.shuffle_state = splitmix64(self.shuffle_state ^ index as u64);
        self.recent.retain(|&i| i != index);
        self.recent.push(index);
        let cap = self.tracks.len().saturating_sub(1).min(12);
        if self.recent.len() > cap {
            let extra = self.recent.len() - cap;
            self.recent.drain(0..extra);
        }
    }

    pub fn cycle_loop(&mut self) {
        self.loop_mode = self.loop_mode.next();
        self.status = format!("loop {}", self.loop_mode.label());
    }

    pub fn seek_by(&mut self, seconds: f32) {
        let sr = self.sample_rate;
        if let Some(deck) = self.current.as_mut() {
            let now = if sr == 0 {
                0.0
            } else {
                deck.source_pos() as f32 / sr as f32
            };
            deck.seek_seconds(now + seconds, sr);
            self.status = format!("seek {seconds:+.0}s");
        }
        self.pcm.reset();
        self.clear_ahead();
    }

    pub fn seek_to(&mut self, seconds: f32) {
        let sr = self.sample_rate;
        if let Some(deck) = self.current.as_mut() {
            deck.seek_seconds(seconds, sr);
            self.status = format!("seek {seconds:.0}s");
        }
        self.pcm.reset();
        self.clear_ahead();
    }

    fn clear_ahead(&mut self) {
        self.drop_armed();
        self.incoming = None;
        self.incoming_idx = None;
        self.fade_total = 0;
        self.done = false;
    }

    pub fn toggle_pause(&mut self) {
        if self.current.is_none() {
            return;
        }
        self.paused = !self.paused;
        self.status = if self.paused { "paused" } else { "playing" }.into();
    }

    pub fn play_decoded(&mut self, index: usize, buf: AudioBuf) {
        self.play_remixed(index, buf, 1.0);
    }

    /// `tempo` is the speed the remix resampled `buf` to.
    pub fn play_remixed(&mut self, index: usize, buf: AudioBuf, tempo: f32) {
        let mut deck = Deck::from_buf(buf);
        deck.tempo = tempo;
        self.play_deck(index, deck);
    }

    fn play_deck(&mut self, index: usize, mut deck: Deck) {
        deck.pos = 0;
        self.remember_played(index);
        self.current_idx = Some(index);
        self.incoming_idx = None;
        self.incoming = None;
        self.fade_total = 0;
        self.fade_pos = 0;
        self.last_plan = None;
        self.armed = None;
        self.current_lp = OnePole::default();
        self.incoming_lp = OnePole::default();
        self.current = Some(deck);
        self.paused = false;
        self.done = false;
        self.pcm.reset();
        self.status = self.play_status(index);
    }

    fn plan_transition(&self, index: usize, skip: bool) -> TransitionPlan {
        plan_for(
            self.current_idx,
            index,
            &self.analyses,
            self.mix,
            skip && self.mix != MixMode::AutoMix,
        )
    }

    /// The current track's analysis in its deck's time.
    fn deck_view(&self) -> Option<TrackAnalysis> {
        let tempo = self.current.as_ref().map_or(1.0, |d| d.tempo);
        self.analysis_at_current().map(|a| at_tempo(a, tempo))
    }

    /// Hands the outgoing deck to a worker when AutoMix wants to bend its tail into `next`.
    /// `next` is passed in because the worker has it before the mixer does.
    pub fn bend_job(&self, index: usize, next: &TrackAnalysis, skip: bool) -> Option<BendJob> {
        if self.mix != MixMode::AutoMix || self.incoming.is_some() {
            return None;
        }
        let current_idx = self.current_idx?;
        let deck = self.current.as_ref()?;
        let current = self.deck_view();
        let next_tempo = self.remix.rate();
        let next = at_tempo(next, next_tempo);
        let plan = plan_pair(current.as_ref(), Some(&next), index, self.mix, false);
        let pos = deck.source_pos();
        let cued = cue(
            current.as_ref(),
            &plan,
            pos,
            deck.source_frames(),
            self.sample_rate,
            skip,
        );
        Some(BendJob {
            samples: Arc::clone(&deck.samples),
            channels: deck.channels,
            sample_rate: self.sample_rate,
            index,
            current_idx,
            next_tempo,
            plan,
            bend: cued.bend?,
            mix_at: cued.mix_at,
        })
    }

    /// Drops an armed mix and gives the outgoing deck its own tempo back.
    fn drop_armed(&mut self) {
        self.armed = None;
        if let Some(deck) = self.current.as_mut() {
            deck.unbend();
        }
    }

    pub fn start_transition(&mut self, index: usize, buf: AudioBuf, skip: bool) {
        self.start_prepared_transition(index, buf, 1.0, skip, None);
    }

    /// `tempo` is the speed the remix resampled `buf` to. `prepared` is a tail the worker
    /// rendered; it is used only if it still fits the deck.
    pub fn start_prepared_transition(
        &mut self,
        index: usize,
        buf: AudioBuf,
        tempo: f32,
        skip: bool,
        prepared: Option<PreparedBend>,
    ) {
        self.drop_armed();
        let mut incoming = Deck::from_buf(buf);
        incoming.tempo = tempo;
        if !self.mix.uses_fade() || self.current.as_ref().is_none_or(|d| d.remaining() < 2) {
            self.play_deck(index, incoming);
            return;
        }
        if self.mix == MixMode::AutoMix {
            self.start_automix(index, incoming, skip, prepared);
            return;
        }
        let plan = self.plan_transition(index, skip);
        let remain = self
            .current
            .as_ref()
            .map(Deck::remaining)
            .unwrap_or(0)
            .min(incoming.remaining())
            .max(2);
        let fade = fade_frames(self.sample_rate, remain, plan.fade_secs);
        if fade < 2 {
            self.play_deck(index, incoming);
            return;
        }
        self.begin_mix(index, incoming, fade, plan);
    }

    fn start_automix(
        &mut self,
        index: usize,
        mut incoming: Deck,
        skip: bool,
        prepared: Option<PreparedBend>,
    ) {
        let sr = self.sample_rate;
        let Some(deck) = self.current.as_ref() else {
            return;
        };
        let (pos, frames) = (deck.pos, deck.source_frames());
        // Analysis times are in the original audio; remixed decks run at their own tempo.
        let current = self.deck_view();
        let next = self.analysis_at(index).map(|a| at_tempo(a, incoming.tempo));
        let plan = plan_pair(current.as_ref(), next.as_ref(), index, self.mix, false);
        let fits = |p: &PreparedBend| {
            Some(p.job.current_idx) == self.current_idx
                && p.job.index == index
                && p.job.next_tempo == incoming.tempo
                && p.job.plan == plan
                && pos <= p.job.bend.start
                && Arc::ptr_eq(&p.job.samples, &deck.samples)
        };
        let (cued, plan, tail) = match prepared.filter(|p| fits(p)) {
            Some(p) => {
                let cued = Cue {
                    mix_at: p.job.mix_at,
                    bend: Some(p.job.bend),
                };
                (cued, plan, Some(p.tail))
            }
            None => {
                // Without a rendered tail the outgoing deck keeps its own tempo.
                let plan = TransitionPlan { speed: 1.0, ..plan };
                let cued = cue(current.as_ref(), &plan, pos, frames, sr, skip);
                (cued, plan, None)
            }
        };
        let next_frames = incoming.source_frames();
        let placed = place(
            &cued,
            current.as_ref(),
            next.as_ref(),
            &plan,
            frames,
            next_frames,
            sr,
        );
        if placed.fade < 2 {
            self.play_deck(index, incoming);
            return;
        }
        incoming.pos = placed.incoming_pos;
        if let (Some(bend), Some(samples), Some(deck)) = (cued.bend, tail, self.current.as_mut()) {
            deck.tail = Some(BentTail {
                start: bend.start,
                map: bend.map,
                samples,
            });
        }
        if skip && placed.start_at <= pos {
            self.begin_mix(index, incoming, placed.fade, plan);
            return;
        }
        self.armed = Some(ArmedMix {
            index,
            deck: incoming,
            fade: placed.fade,
            start_at: placed.start_at,
            plan,
        });
        self.status = format!("armed {}", plan.style.label());
    }
    fn begin_mix(&mut self, index: usize, incoming: Deck, fade: usize, plan: TransitionPlan) {
        self.incoming_idx = Some(index);
        self.incoming = Some(incoming);
        self.fade_total = fade;
        self.fade_pos = 0;
        self.current_lp = OnePole::default();
        self.incoming_lp = OnePole::default();
        self.paused = false;
        self.done = false;
        self.armed = None;
        self.last_plan = Some(plan);
        self.status = self.mix_status(index);
    }

    #[cfg(test)]
    fn set_playhead(&mut self, pos: usize) {
        if let Some(deck) = self.current.as_mut() {
            deck.pos = pos.min(deck.frames().saturating_sub(1));
        }
    }

    pub fn remaining_frames(&self) -> usize {
        self.current.as_ref().map(Deck::remaining).unwrap_or(0)
    }

    pub fn fading(&self) -> bool {
        self.incoming.is_some() && self.fade_total > 0
    }

    pub fn should_prefetch(&self) -> bool {
        if self.loop_mode == LoopMode::One {
            return false;
        }
        if self.paused || self.incoming.is_some() || self.armed.is_some() || self.tracks.len() < 2 {
            return false;
        }
        let Some(deck) = self.current.as_ref() else {
            return false;
        };
        if !self.mix.uses_fade() {
            return deck.remaining() < (self.sample_rate as usize / 2).max(1);
        }
        if self.mix == MixMode::AutoMix {
            if let Some(current) = self.deck_view() {
                let mix_out = current.mix_out;
                let pos_secs = deck.pos as f32 / self.sample_rate.max(1) as f32;
                let fade = fade_secs_for(self.mix, Some(&current), false);
                // The tail starts bending up to RAMP_MAX_SECS before the overlap.
                return pos_secs + fade + RAMP_MAX_SECS + PREFETCH_LEAD_SECS >= mix_out;
            }
        }
        let need = fade_frames(self.sample_rate, deck.frames(), FADE_SECS)
            .saturating_add(self.sample_rate as usize);
        deck.remaining() <= need
    }

    fn pool(&self) -> Vec<usize> {
        match &self.playable {
            None => (0..self.tracks.len()).collect(),
            Some(p) => p
                .iter()
                .copied()
                .filter(|&i| i < self.tracks.len())
                .collect(),
        }
    }

    fn step_in_pool(&self, pool: &[usize], delta: i32) -> usize {
        match self
            .current_idx
            .and_then(|cur| pool.iter().position(|&i| i == cur))
        {
            Some(pos) => {
                let len = pool.len() as i32;
                pool[(pos as i32 + delta).rem_euclid(len) as usize]
            }
            None => match self.current_idx {
                None => pool[0],
                Some(cur) if delta >= 0 => {
                    pool.iter().copied().find(|&i| i > cur).unwrap_or(pool[0])
                }
                Some(cur) => pool
                    .iter()
                    .copied()
                    .rev()
                    .find(|&i| i < cur)
                    .unwrap_or(*pool.last().unwrap_or(&0)),
            },
        }
    }

    fn pick_next_in_pool(&self, pool: &[usize]) -> Option<usize> {
        if pool.is_empty() {
            return None;
        }
        if self.mix != MixMode::AutoMix || pool.len() == 1 {
            return Some(self.step_in_pool(pool, 1));
        }
        let cur = self.current_idx;
        let mut best: Option<(usize, f32)> = None;
        for &i in pool {
            if Some(i) == cur {
                continue;
            }
            let mut candidate = plan_for(cur, i, &self.analyses, self.mix, false);
            candidate.score += self.taste_boosts.get(i).copied().unwrap_or(0.0);
            if best.is_none_or(|(_, s)| candidate.score > s) {
                best = Some((i, candidate.score));
            }
        }
        best.map(|(i, _)| i)
            .or_else(|| Some(self.step_in_pool(pool, 1)))
    }

    pub fn next_index(&self) -> Option<usize> {
        if self.loop_mode == LoopMode::One {
            return self.current_idx;
        }
        let pool = self.pool();
        if pool.is_empty() {
            return None;
        }
        if self.shuffle_mode != ShuffleMode::Off && pool.len() > 1 {
            return Some(self.shuffled_next(&pool));
        }
        if self.loop_mode == LoopMode::All {
            return Some(self.step_in_pool(&pool, 1));
        }
        self.pick_next_in_pool(&pool)
    }

    fn shuffled_next(&self, pool: &[usize]) -> usize {
        match self.shuffle_mode {
            ShuffleMode::Off => self.current_idx.unwrap_or(pool[0]),
            ShuffleMode::Taste => self.taste_next(pool),
            ShuffleMode::NoRepeat => self.norepeat_next(pool),
            ShuffleMode::Random => self.random_next(pool),
        }
    }

    fn shuffle_seed(&self) -> usize {
        (self.shuffle_state >> 11) as usize
    }

    fn random_next(&self, pool: &[usize]) -> usize {
        let cur = self.current_idx;
        let candidates: Vec<usize> = pool.iter().copied().filter(|&i| Some(i) != cur).collect();
        if candidates.is_empty() {
            return cur.unwrap_or(pool[0]);
        }
        candidates[self.shuffle_seed() % candidates.len()]
    }

    fn norepeat_next(&self, pool: &[usize]) -> usize {
        let cur = self.current_idx;
        let mut candidates: Vec<usize> = pool
            .iter()
            .copied()
            .filter(|i| Some(*i) != cur && !self.recent.contains(i))
            .collect();
        if candidates.is_empty() {
            candidates = pool.iter().copied().filter(|i| Some(*i) != cur).collect();
        }
        if candidates.is_empty() {
            return cur.unwrap_or(pool[0]);
        }
        candidates[self.shuffle_seed() % candidates.len()]
    }

    fn taste_next(&self, pool: &[usize]) -> usize {
        let cur = self.current_idx;
        let weighted: Vec<(usize, f32)> = pool
            .iter()
            .copied()
            .filter(|i| Some(*i) != cur)
            .map(|i| {
                let boost = self.taste_boosts.get(i).copied().unwrap_or(0.0).max(0.0);
                (i, 1.0 + boost)
            })
            .collect();
        if weighted.is_empty() {
            return cur.unwrap_or(pool[0]);
        }
        let total: f32 = weighted.iter().map(|(_, w)| w).sum::<f32>().max(0.001);
        let mut pick = (self.shuffle_seed() % 10_007) as f32 / 10_007.0 * total;
        for (i, w) in weighted {
            if pick <= w {
                return i;
            }
            pick -= w;
        }
        cur.unwrap_or(pool[0])
    }

    pub fn prev_index(&self) -> Option<usize> {
        let pool = self.pool();
        if pool.is_empty() {
            return None;
        }
        Some(self.step_in_pool(&pool, -1))
    }

    pub fn take_auto_advance(&mut self) -> Option<usize> {
        if !std::mem::take(&mut self.done) {
            return None;
        }
        if self.loop_mode == LoopMode::One || self.tracks.len() < 2 {
            return None;
        }
        self.next_index()
    }

    pub fn snapshot(&self, selected: usize) -> Snapshot {
        self.snapshot_inner(selected, true)
    }

    pub fn playback_view(&self, selected: usize) -> Snapshot {
        self.snapshot_inner(selected, false)
    }

    fn snapshot_inner(&self, selected: usize, include_pcm: bool) -> Snapshot {
        let duration = self.current.as_ref().map(Deck::source_frames).unwrap_or(0);
        let pos = self.current.as_ref().map(Deck::source_pos).unwrap_or(0);
        Snapshot {
            current: self.current_idx,
            selected,
            paused: self.paused,
            mix: self.mix,
            status: self.status.clone(),
            position_frames: pos,
            duration_frames: duration,
            fading: self.fading(),
            volume: self.volume,
            sample_rate: self.sample_rate,
            remix: self.remix,
            current_bpm: self.analysis_at_current().and_then(|a| a.bpm),
            current_key: self
                .analysis_at_current()
                .and_then(|a| a.camelot.map(|c| c.label())),
            next_hint: self.next_hint(),
            spectrum_levels: self.spectrum.levels(),
            shuffle: self.shuffle,
            loop_mode: self.loop_mode,
            pcm: if include_pcm {
                self.pcm.snapshot()
            } else {
                crate::scope::PcmSnapshot::default()
            },
        }
    }

    fn analysis_at_current(&self) -> Option<&TrackAnalysis> {
        self.current_idx.and_then(|i| self.analysis_at(i))
    }

    fn play_status(&self, index: usize) -> String {
        let meta = self.meta_label(index);
        if meta.is_empty() {
            format!("playing {}", self.title_at(index))
        } else {
            format!("playing {} · {}", self.title_at(index), meta)
        }
    }

    fn mix_status(&self, index: usize) -> String {
        let extra = self.last_plan.map(|p| {
            format!(
                "{}{}{:.0}%",
                if p.key_ok { "key " } else { "" },
                if p.bpm_ok { "bpm " } else { "" },
                p.score * 100.0
            )
        });
        let kind = if self.mix == MixMode::AutoMix {
            self.last_plan
                .map(|p| p.style.label())
                .unwrap_or("bass-swap")
        } else {
            "fade"
        };
        match extra {
            Some(tag) if !tag.trim().is_empty() => {
                format!("{kind} → {} · {}", self.title_at(index), tag.trim())
            }
            _ => format!("{kind} → {}", self.title_at(index)),
        }
    }

    fn meta_label(&self, index: usize) -> String {
        let Some(a) = self.analysis_at(index) else {
            return String::new();
        };
        let bpm = a
            .bpm
            .map(|b| format!("{b:.0}"))
            .unwrap_or_else(|| "--".into());
        let key = a.camelot.map(|c| c.label()).unwrap_or_else(|| "--".into());
        format!("{bpm} {key}")
    }

    fn next_hint(&self) -> Option<String> {
        let next = self.next_index()?;
        if Some(next) == self.current_idx {
            return None;
        }
        Some(format!(
            "next {} {}",
            self.title_at(next),
            self.meta_label(next)
        ))
    }

    pub fn fill(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        let hop = 32;
        let mut hop_i = 0usize;
        let mut mono = [0.0f32; 512];
        let mut mono_n = 0usize;
        for frame in out.chunks_mut(ch) {
            let stereo = self.next_stereo();
            let stereo = self.eq.process_stereo(stereo);
            let left = stereo[0] * self.volume;
            let right = stereo[1] * self.volume;
            match ch {
                1 => frame[0] = (left + right) * 0.5,
                _ => {
                    frame[0] = left;
                    if frame.len() > 1 {
                        frame[1] = right;
                    }
                    for s in frame.iter_mut().skip(2) {
                        *s = 0.0;
                    }
                }
            }
            if hop_i == 0 {
                self.spectrum.push_fft_frame(left);
            }
            hop_i += 1;
            if hop_i == hop {
                hop_i = 0;
            }
            if mono_n == mono.len() {
                self.pcm.push(&mono, self.sample_rate);
                mono_n = 0;
            }
            mono[mono_n] = (left + right) * 0.5;
            mono_n += 1;
        }
        if mono_n > 0 {
            self.pcm.push(&mono[..mono_n], self.sample_rate);
        }
    }

    pub fn pcm_snapshot(&self) -> PcmSnapshot {
        self.pcm.snapshot()
    }

    pub fn refresh_spectrum(&mut self) {
        self.spectrum.analyze_pending();
    }

    pub fn ingest_cava(&mut self, bars: &[f32]) {
        if !bars.is_empty() {
            self.spectrum.ingest_cava_bars(bars);
        }
    }

    fn next_stereo(&mut self) -> [f32; 2] {
        if self.paused {
            return [0.0, 0.0];
        }
        if self.incoming.is_some() && self.fade_total > 0 {
            return self.mix_frame();
        }
        if let Some(armed) = self.armed.as_ref() {
            let pos = self.current.as_ref().map(|d| d.pos).unwrap_or(0);
            if pos >= armed.start_at {
                let armed = self.armed.take().unwrap();
                self.begin_mix(armed.index, armed.deck, armed.fade, armed.plan);
                return self.mix_frame();
            }
        }
        let Some(deck) = self.current.as_mut() else {
            return [0.0, 0.0];
        };
        if deck.pos >= deck.frames() {
            if self.loop_mode == LoopMode::One {
                deck.rewind();
            } else {
                self.done = true;
                self.paused = true;
                self.status = "ended".into();
                return [0.0, 0.0];
            }
        }
        let frame = deck.frame(deck.pos);
        deck.pos += 1;
        if deck.pos >= deck.frames() {
            if self.loop_mode == LoopMode::One {
                deck.rewind();
            } else {
                self.done = true;
            }
        }
        frame
    }

    fn mix_frame(&mut self) -> [f32; 2] {
        let t = if self.fade_total == 0 {
            1.0
        } else {
            self.fade_pos as f32 / self.fade_total as f32
        };
        let a = self
            .current
            .as_ref()
            .map(|d| d.frame(d.pos))
            .unwrap_or([0.0, 0.0]);
        let b = self
            .incoming
            .as_ref()
            .map(|d| d.frame(d.pos))
            .unwrap_or([0.0, 0.0]);
        if let Some(deck) = self.current.as_mut() {
            if deck.pos < deck.frames() {
                deck.pos += 1;
            }
        }
        if let Some(deck) = self.incoming.as_mut() {
            if deck.pos < deck.frames() {
                deck.pos += 1;
            }
        }
        self.fade_pos += 1;
        if self.fade_pos >= self.fade_total {
            self.finish_fade();
        }
        if self.mix == MixMode::AutoMix {
            let alpha = lp_alpha(self.sample_rate, 160.0);
            match self
                .last_plan
                .map(|p| p.style)
                .unwrap_or(MixStyle::BassSwap)
            {
                MixStyle::BassSwap => {
                    mix_bass_swap(a, b, &mut self.current_lp, &mut self.incoming_lp, t, alpha)
                }
                MixStyle::Filter => {
                    mix_filter_blend(a, b, &mut self.current_lp, &mut self.incoming_lp, t, alpha)
                }
                MixStyle::Echo | MixStyle::Quick => mix_echo_out(a, b, t),
            }
        } else {
            mix_stereo_frame(a, b, t)
        }
    }

    fn finish_fade(&mut self) {
        if let Some(incoming) = self.incoming.take() {
            self.current = Some(incoming);
            self.current_idx = self.incoming_idx.take();
            if let Some(idx) = self.current_idx {
                self.remember_played(idx);
            }
            self.status = format!(
                "playing {}",
                self.current_idx
                    .map(|i| self.title_at(i))
                    .unwrap_or_default()
            );
        }
        self.fade_total = 0;
        self.fade_pos = 0;
    }

    fn title_at(&self, index: usize) -> String {
        self.tracks
            .get(index)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    }
}

fn shuffle_entropy() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    splitmix64(nanos ^ (u64::from(std::process::id()) << 32))
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct Player {
    pub mixer: Arc<Mutex<Mixer>>,
    stream: Option<Stream>,
    device: cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    health: Arc<StreamHealth>,
    rebuild_gate: RebuildGate,
    pub sample_rate: u32,
    pub channels: usize,
}

impl Player {
    pub fn start() -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no output device"))?;
        let supported = device.default_output_config()?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let mixer = Arc::new(Mutex::new(Mixer::new(sample_rate, channels)));
        let health = Arc::new(StreamHealth::new());
        let stream = open_stream(
            &device,
            &config,
            sample_format,
            Arc::clone(&mixer),
            Arc::clone(&health),
        )?;
        stream.play()?;
        Ok(Self {
            mixer,
            stream: Some(stream),
            device,
            config,
            sample_format,
            health,
            rebuild_gate: RebuildGate::new(),
            sample_rate,
            channels,
        })
    }

    pub fn recover_if_needed(&mut self) {
        if !self.health.take_failed() {
            return;
        }
        if !self.rebuild_gate.allow(Instant::now(), STREAM_REBUILD_GAP) {
            self.health.mark_failed();
            return;
        }
        // Drop first so ALSA can reopen the same device.
        self.stream = None;
        match open_stream(
            &self.device,
            &self.config,
            self.sample_format,
            Arc::clone(&self.mixer),
            Arc::clone(&self.health),
        ) {
            Ok(stream) => {
                if let Err(err) = stream.play() {
                    eprintln!("audio stream: rebuild play failed: {err}");
                    self.health.mark_failed();
                    return;
                }
                self.stream = Some(stream);
            }
            Err(err) => {
                eprintln!("audio stream: rebuild failed: {err}");
                self.health.mark_failed();
            }
        }
    }
}

fn open_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    mixer: Arc<Mutex<Mixer>>,
    health: Arc<StreamHealth>,
) -> anyhow::Result<Stream> {
    match sample_format {
        SampleFormat::F32 => build_stream::<f32>(device, config, mixer, health),
        SampleFormat::I16 => build_stream::<i16>(device, config, mixer, health),
        SampleFormat::U16 => build_stream::<u16>(device, config, mixer, health),
        other => Err(anyhow!("unsupported sample format {other}")),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    health: Arc<StreamHealth>,
) -> anyhow::Result<Stream>
where
    T: Sample + cpal::FromSample<f32> + cpal::SizedSample,
{
    let log = Mutex::new(StreamErrorLog::new());
    let err_fn = move |err: cpal::StreamError| {
        let msg = err.to_string();
        note_stream_error(&health, &msg);
        if let Ok(mut log) = log.lock() {
            for line in log.push(msg) {
                eprintln!("{line}");
            }
        }
    };
    let mut tmp = Vec::new();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            tmp.resize(data.len(), 0.0);
            if let Ok(mut mix) = mixer.lock() {
                mix.fill(&mut tmp);
            } else {
                tmp.fill(0.0);
            }
            for (out, sample) in data.iter_mut().zip(tmp.iter()) {
                *out = T::from_sample(*sample);
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

pub fn load_track(path: &Path, sample_rate: u32, channels: usize) -> anyhow::Result<AudioBuf> {
    decode_file(path, sample_rate, channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Track;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn const_deck(frames: usize, amp: f32) -> AudioBuf {
        AudioBuf {
            samples: vec![amp; frames * 2],
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn two_tracks() -> Vec<Track> {
        vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "B".into(),
            },
        ]
    }

    #[test]
    fn pause_writes_silence() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.toggle_pause();
        let mut out = vec![1.0; 8];
        mixer.fill(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn cut_replaces_buffer_immediately() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::Cut;
        mixer.play_decoded(0, const_deck(200, 0.8));
        mixer.start_transition(1, const_deck(200, 0.1), true);
        assert_eq!(mixer.current_idx, Some(1));
        assert!(!mixer.fading());
    }

    #[test]
    fn cut_end_hands_next_track_to_ui_once() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::Cut;
        mixer.play_decoded(0, const_deck(64, 0.5));
        assert_eq!(mixer.take_auto_advance(), None);
        let mut out = vec![0.0; 256];
        mixer.fill(&mut out);
        assert!(!mixer.should_prefetch());
        assert_eq!(mixer.take_auto_advance(), Some(1));
        assert_eq!(mixer.take_auto_advance(), None);
    }

    #[test]
    fn fade_mode_still_advances_when_prefetch_never_landed() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::Crossfade;
        mixer.play_decoded(0, const_deck(64, 0.5));
        let mut out = vec![0.0; 256];
        mixer.fill(&mut out);
        assert_eq!(mixer.take_auto_advance(), Some(1));
    }

    #[test]
    fn auto_advance_respects_loop_one_and_single_track() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::Cut;
        mixer.loop_mode = LoopMode::One;
        mixer.play_decoded(0, const_deck(64, 0.5));
        let mut out = vec![0.0; 256];
        mixer.fill(&mut out);
        assert_eq!(mixer.take_auto_advance(), None);

        let mut single = Mixer::new(48_000, 2);
        single.set_tracks(two_tracks()[..1].to_vec());
        single.mix = MixMode::Cut;
        single.play_decoded(0, const_deck(64, 0.5));
        single.fill(&mut out);
        assert_eq!(single.take_auto_advance(), None);
    }

    #[test]
    fn rescan_keeps_playing_and_fading_tracks_by_path() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.mix = MixMode::Crossfade;
        mixer.set_analysis(1, analysis_at(1.0, 9.0, 10.0));
        mixer.play_decoded(1, const_deck(48_000, 0.5));
        mixer.start_transition(2, const_deck(48_000, 0.2), true);
        assert!(mixer.fading());

        let mut reordered = four_tracks();
        reordered.reverse();
        mixer.set_tracks(reordered);

        assert_eq!(mixer.current_idx, Some(2));
        assert_eq!(mixer.incoming_idx, Some(1));
        assert_eq!(mixer.recent, vec![2]);
        assert!(mixer.analysis_at(2).is_some());
        assert!(mixer.analysis_at(1).is_none());
    }

    #[test]
    fn rescan_forgets_a_playing_track_that_vanished() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.play_decoded(3, const_deck(100, 0.5));
        mixer.set_tracks(four_tracks()[..2].to_vec());
        assert_eq!(mixer.current_idx, None);
        assert!(mixer.recent.is_empty());
    }

    #[test]
    fn crossfade_mixes_then_lands_on_next() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::Crossfade;
        mixer.play_decoded(0, const_deck(48_000, 0.8));
        mixer.start_transition(1, const_deck(48_000, 0.2), true);
        assert!(mixer.fading());
        let fade = mixer.fade_total;
        assert!(fade > 10);
        let mut out = vec![0.0; fade * 2 + 4];
        mixer.fill(&mut out);
        assert!(!mixer.fading());
        assert_eq!(mixer.current_idx, Some(1));
        let first = out[0].abs();
        let last = out[out.len() - 2].abs();
        assert!(first > last);
    }

    fn tone(frames: usize) -> AudioBuf {
        AudioBuf {
            samples: (0..frames * 2)
                .map(|i| ((i / 2) as f32 * 0.01).sin() * 0.3)
                .collect(),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn run_until_current(mixer: &mut Mixer, index: usize, max_secs: usize) {
        let mut out = vec![0.0; 48_000 * 2];
        for _ in 0..max_secs {
            mixer.fill(&mut out);
            if mixer.current_idx == Some(index) && !mixer.fading() {
                return;
            }
        }
        panic!("no handover to {index} within {max_secs}s");
    }

    #[test]
    fn automix_hands_over_the_incoming_track_unresampled() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, const_deck(48_000 * 20, 0.5));
        let mut a = analysis_at(0.0, 16.0, 20.0);
        a.bpm = Some(128.0);
        mixer.set_analysis(0, a);
        mixer.set_analysis(1, analysis_at(0.0, 16.0, 20.0));
        let next = tone(48_000 * 20);
        mixer.start_transition(1, next.clone(), false);
        run_until_current(&mut mixer, 1, 20);
        let deck = mixer.current.as_ref().unwrap();
        assert_eq!(
            deck.samples.len(),
            next.samples.len(),
            "incoming track was resampled"
        );
        assert!(deck.samples[..] == next.samples[..]);
    }

    #[test]
    fn deck_past_its_end_is_silent_not_its_last_sample() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(100, 0.8));
        let plan = mixer.plan_transition(1, true);
        mixer.begin_mix(1, Deck::from_buf(const_deck(48_000, 0.0)), 400, plan);
        for _ in 0..100 {
            mixer.next_stereo();
        }
        for i in 0..200 {
            assert_eq!(mixer.next_stereo(), [0.0, 0.0], "frame {i} past the end");
        }
    }

    fn analysis_at(mix_in: f32, mix_out: f32, duration: f32) -> crate::analysis::TrackAnalysis {
        crate::analysis::TrackAnalysis {
            version: 1,
            mtime: 0,
            size: 0,
            bpm: Some(120.0),
            bpm_confidence: 0.9,
            key_root: Some(0),
            key_minor: false,
            key_confidence: 0.9,
            camelot: None,
            energy: 0.2,
            mix_in,
            mix_out,
            first_beat: mix_in,
            duration,
        }
    }

    #[test]
    fn automix_jumps_to_phrase_not_from_zero() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, const_deck(48_000 * 20, 0.8));
        mixer.set_analysis(0, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(1, analysis_at(4.0, 18.0, 20.0));
        if let Some(deck) = mixer.current.as_mut() {
            deck.pos = 48_000 * 10;
        }
        mixer.start_transition(1, const_deck(48_000 * 20, 0.2), false);
        assert!(mixer.armed.is_some(), "auto mix should wait for phrase");
        assert!(!mixer.fading());
        let incoming_pos = mixer.armed.as_ref().unwrap().deck.pos;
        assert!(
            incoming_pos > 48_000 * 3,
            "incoming should start at mix_in, pos={incoming_pos}"
        );
        let start_at = mixer.armed.as_ref().unwrap().start_at;
        let pos = mixer.current.as_ref().unwrap().pos;
        assert!(
            start_at >= pos,
            "must not rewind current track, start={start_at} pos={pos}"
        );
    }

    fn clicks(frames: usize, bpm: f64) -> AudioBuf {
        let beat = 48_000.0 * 60.0 / bpm;
        let samples = (0..frames)
            .flat_map(|f| {
                let into = f as f64 % beat;
                let v = if into < 48.0 { 0.9 } else { 0.0 };
                [v, v]
            })
            .collect();
        AudioBuf {
            samples,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    /// Mixer playing a 128 BPM click track with a bent mix into a 120 BPM track armed.
    fn armed_bend() -> (Mixer, AudioBuf) {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, clicks(48_000 * 20, 128.0));
        let mut a = analysis_at(0.0, 16.0, 20.0);
        a.bpm = Some(128.0);
        mixer.set_analysis(0, a);
        let b = analysis_at(0.0, 16.0, 20.0);
        mixer.set_analysis(1, b.clone());
        let job = mixer.bend_job(1, &b, false).expect("128 → 120 locks");
        let next = tone(48_000 * 20);
        mixer.start_prepared_transition(1, next.clone(), 1.0, false, Some(job.render()));
        assert!(mixer.armed.is_some());
        assert!(mixer.current.as_ref().unwrap().tail.is_some());
        (mixer, next)
    }

    fn onsets(deck: &Deck, from: usize, to: usize) -> Vec<usize> {
        let mut found = Vec::new();
        let mut last_loud: Option<usize> = None;
        for at in from..to {
            if deck.frame(at)[0].abs() > 0.3 {
                if last_loud.is_none_or(|l| at - l > 4_800) {
                    found.push(at);
                }
                last_loud = Some(at);
            }
        }
        found
    }
    #[test]
    fn automix_bends_the_outgoing_tail_to_the_incoming_tempo() {
        let (mut mixer, next) = armed_bend();
        let armed = mixer.armed.as_ref().unwrap();
        let (start_at, fade) = (armed.start_at, armed.fade);
        let deck = mixer.current.as_ref().unwrap();
        let found = onsets(deck, start_at - 2_000, start_at + fade);
        assert!(
            found.len() >= 10,
            "only {} beats in the overlap",
            found.len()
        );
        assert!(
            found[0].abs_diff(start_at) < 960,
            "overlap starts on a beat"
        );
        let beat_120 = 24_000;
        for pair in found.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap.abs_diff(beat_120) < 480,
                "beat gap {gap} is not 120 BPM"
            );
        }

        run_until_current(&mut mixer, 1, 20);
        let deck = mixer.current.as_ref().unwrap();
        assert!(deck.tail.is_none());
        assert!(
            deck.samples[..] == next.samples[..],
            "incoming track was touched"
        );
    }

    #[test]
    fn seeking_cancels_the_bend_and_keeps_the_source_position() {
        let (mut mixer, _) = armed_bend();
        let deck = mixer.current.as_ref().unwrap();
        let tail = deck.tail.as_ref().unwrap();
        let (start, map) = (tail.start, tail.map);
        let into = 48_000;
        let want = start + map.src_of(into as f64).round() as usize;
        mixer.set_playhead(start + into);
        assert_eq!(mixer.current.as_ref().unwrap().source_pos(), want);
        assert_eq!(mixer.snapshot(0).position_frames, want);
        mixer.seek_by(0.0);
        let deck = mixer.current.as_ref().unwrap();
        assert!(mixer.armed.is_none());
        assert!(deck.tail.is_none());
        assert!(deck.pos.abs_diff(want) <= 2, "pos {} want {want}", deck.pos);
    }

    #[test]
    fn remixed_decks_are_cued_in_their_own_time() {
        let tempo = RemixMode::Nightcore.rate();
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_remixed(0, const_deck(48_000 * 18, 0.5), tempo);
        mixer.set_analysis(0, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(1, analysis_at(4.0, 16.0, 20.0));
        mixer.set_playhead(48_000 * 7);
        mixer.start_prepared_transition(1, const_deck(48_000 * 18, 0.2), tempo, false, None);
        let armed = mixer.armed.as_ref().expect("armed");
        let bar = 2.0;
        let start_bars = armed.start_at as f64 * tempo as f64 / 48_000.0 / bar;
        assert!(
            (start_bars - start_bars.round()).abs() < 1e-3,
            "overlap starts {start_bars} bars into the source, off the grid"
        );
        let mix_in = armed.deck.pos as f64 * tempo as f64 / 48_000.0;
        assert!(
            (mix_in - 4.0).abs() < 1e-3,
            "next starts at {mix_in}s, not mix_in"
        );
    }

    #[test]
    fn stale_bend_falls_back_to_an_unbent_mix() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, clicks(48_000 * 20, 128.0));
        let mut a = analysis_at(0.0, 16.0, 20.0);
        a.bpm = Some(128.0);
        mixer.set_analysis(0, a);
        let b = analysis_at(0.0, 16.0, 20.0);
        mixer.set_analysis(1, b.clone());
        let job = mixer.bend_job(1, &b, false).unwrap();
        let prepared = job.render();
        mixer.set_playhead(prepared.job.bend.start + 1);
        mixer.start_prepared_transition(1, tone(48_000 * 20), 1.0, false, Some(prepared));
        let armed = mixer.armed.as_ref().expect("still mixes");
        assert_eq!(armed.plan.speed, 1.0);
        assert!(mixer.current.as_ref().unwrap().tail.is_none());
    }

    #[test]
    fn seek_moves_playhead() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(48_000 * 10, 0.5));
        mixer.seek_by(2.0);
        assert_eq!(mixer.current.as_ref().unwrap().pos, 48_000 * 2);
        mixer.seek_by(-1.0);
        assert_eq!(mixer.current.as_ref().unwrap().pos, 48_000);
    }

    #[test]
    fn seek_to_jumps_absolute() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(48_000 * 10, 0.5));
        mixer.seek_by(4.0);
        mixer.seek_to(2.0);
        assert_eq!(mixer.current.as_ref().unwrap().pos, 48_000 * 2);
    }

    #[test]
    fn fill_pushes_mixed_pcm_into_the_scope_ring() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(100, 0.5));
        let mut out = vec![0.0; 8];
        mixer.fill(&mut out);
        let snap = mixer.pcm_snapshot();
        assert_eq!(snap.len(), 4);
        assert_eq!(snap.sample_rate, 48_000);
        for &v in &snap.samples {
            assert!((v - 0.45).abs() < 1e-4, "got {v}");
        }
    }

    #[test]
    fn seek_resets_the_scope_ring() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(48_000 * 10, 0.5));
        let mut out = vec![0.0; 8];
        mixer.fill(&mut out);
        assert!(!mixer.pcm_snapshot().samples.is_empty());
        mixer.seek_to(1.0);
        assert_eq!(mixer.pcm_snapshot().len(), 0);
    }

    #[test]
    fn play_decoded_resets_the_scope_ring() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(48_000 * 10, 0.5));
        let mut out = vec![0.0; 8];
        mixer.fill(&mut out);
        assert!(!mixer.pcm_snapshot().is_empty());
        mixer.play_decoded(1, const_deck(48_000 * 10, 0.2));
        assert_eq!(mixer.pcm_snapshot().len(), 0);
    }

    #[test]
    fn snapshot_carries_pcm_so_the_ui_does_not_relock() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(100, 0.5));
        let mut out = vec![0.0; 8];
        mixer.fill(&mut out);
        let snap = mixer.snapshot(0);
        assert_eq!(snap.pcm.len(), mixer.pcm_snapshot().len());
        assert_eq!(snap.pcm.samples, mixer.pcm_snapshot().samples);
        assert_eq!(snap.pcm.sample_rate, 48_000);
    }

    #[test]
    fn playback_view_skips_pcm_copy() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(100, 0.5));
        let mut out = vec![0.0; 8];
        mixer.fill(&mut out);
        let view = mixer.playback_view(0);
        assert!(view.pcm.samples.is_empty());
        assert_eq!(view.current, Some(0));
        assert_eq!(view.spectrum_levels, mixer.snapshot(0).spectrum_levels);
    }

    #[test]
    fn snapshot_copies_a_short_triggered_window() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(48_000 * 10, 0.5));
        let mut out = vec![0.0; 48_000];
        mixer.fill(&mut out);
        let snap = mixer.snapshot(0);
        assert!(
            snap.pcm.len() <= 2_000,
            "UI snapshot must not copy the whole ring, got {}",
            snap.pcm.len()
        );
        assert!(
            snap.pcm.len() >= 1_800,
            "40ms window at 48kHz should be ~1920, got {}",
            snap.pcm.len()
        );
    }

    #[test]
    fn loop_one_rewinds_instead_of_ending() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.play_decoded(0, const_deck(8, 0.5));
        mixer.cycle_loop();
        assert_eq!(mixer.loop_mode, LoopMode::One);
        let mut out = vec![0.0; 40];
        mixer.fill(&mut out);
        assert!(!mixer.paused);
        assert!(!mixer.done);
        assert_eq!(mixer.current_idx, Some(0));
        assert!(mixer.current.as_ref().unwrap().pos < 8);
    }

    #[test]
    fn no_repeat_avoids_recent_tracks() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "B".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "C".into(),
            },
            Track {
                path: PathBuf::from("d.wav"),
                title: "D".into(),
            },
        ]);
        mixer.set_shuffle_mode(crate::prefs::ShuffleMode::NoRepeat);
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.remember_played(0);
        mixer.remember_played(1);
        mixer.remember_played(2);
        let next = mixer.next_index().unwrap();
        assert_eq!(next, 3);
    }

    #[test]
    fn shuffle_next_stays_stable_as_playhead_moves() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "Alpha".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "Beta".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "Gamma".into(),
            },
            Track {
                path: PathBuf::from("d.wav"),
                title: "Delta".into(),
            },
        ]);
        mixer.play_decoded(0, const_deck(10_000, 0.5));
        mixer.set_shuffle_mode(crate::prefs::ShuffleMode::Random);
        let first = mixer.next_index().unwrap();
        let hint = mixer.snapshot(0).next_hint.clone();
        mixer.set_playhead(1);
        assert_eq!(mixer.next_index(), Some(first));
        mixer.set_playhead(77);
        assert_eq!(mixer.next_index(), Some(first));
        mixer.set_playhead(9_000);
        assert_eq!(mixer.next_index(), Some(first));
        assert_eq!(mixer.snapshot(0).next_hint, hint);
    }

    #[test]
    fn random_shuffle_reaches_the_whole_library() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(
            (0..16)
                .map(|i| Track {
                    path: PathBuf::from(format!("{i}.wav")),
                    title: i.to_string(),
                })
                .collect(),
        );
        mixer.set_shuffle_mode(crate::prefs::ShuffleMode::Random);
        mixer.shuffle_state = 7;
        mixer.play_decoded(0, const_deck(100, 0.5));
        let mut seen = std::collections::HashSet::from([0]);
        for _ in 0..200 {
            let next = mixer.next_index().unwrap();
            mixer.play_decoded(next, const_deck(100, 0.5));
            seen.insert(next);
        }
        assert_eq!(seen.len(), 16, "visited {seen:?}");
    }

    #[test]
    fn shuffle_next_hint_matches_next_index_not_seq() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "Alpha".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "Beta".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "Gamma".into(),
            },
        ]);
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.set_shuffle_mode(crate::prefs::ShuffleMode::Random);
        let next = mixer.next_index().unwrap();
        assert_ne!(next, 0);
        let title = mixer.tracks()[next].title.clone();
        let hint = mixer.snapshot(0).next_hint.unwrap();
        assert!(
            hint.contains(&title),
            "hint must follow next_index, hint={hint} title={title}"
        );
    }

    #[test]
    fn shuffle_skips_current_track() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "B".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "C".into(),
            },
        ]);
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.toggle_shuffle();
        let next = mixer.next_index().unwrap();
        assert_ne!(next, 0);
        assert!(next < 3);
    }

    #[test]
    fn automix_taste_boost_changes_next() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "B".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "C".into(),
            },
        ]);
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.set_analysis(0, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(1, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(2, analysis_at(0.0, 16.0, 20.0));
        mixer.set_taste_boosts(vec![0.0, 0.0, 0.3]);
        assert_eq!(mixer.next_index(), Some(2));
    }

    fn four_tracks() -> Vec<Track> {
        vec![
            Track {
                path: PathBuf::from("a.wav"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("b.wav"),
                title: "B".into(),
            },
            Track {
                path: PathBuf::from("c.wav"),
                title: "C".into(),
            },
            Track {
                path: PathBuf::from("d.wav"),
                title: "D".into(),
            },
        ]
    }

    #[test]
    fn playable_next_stays_inside_filter() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.set_playable(Some(vec![1, 3]));
        mixer.play_decoded(1, const_deck(100, 0.5));
        mixer.cycle_loop();
        mixer.cycle_loop();
        assert_eq!(mixer.loop_mode, LoopMode::All);
        assert_eq!(mixer.next_index(), Some(3));
        mixer.play_decoded(3, const_deck(100, 0.5));
        assert_eq!(mixer.next_index(), Some(1));
        assert_eq!(mixer.prev_index(), Some(1));
    }

    #[test]
    fn playable_empty_returns_none() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.set_playable(Some(vec![]));
        mixer.play_decoded(0, const_deck(100, 0.5));
        assert_eq!(mixer.next_index(), None);
        assert_eq!(mixer.prev_index(), None);
    }

    #[test]
    fn playable_none_matches_whole_library() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.cycle_loop();
        mixer.cycle_loop();
        let unrestricted = mixer.next_index();
        mixer.set_playable(None);
        assert_eq!(mixer.next_index(), unrestricted);
        assert_eq!(unrestricted, Some(1));
    }

    #[test]
    fn shuffle_playable_stays_inside() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.set_playable(Some(vec![0, 2]));
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.set_shuffle_mode(crate::prefs::ShuffleMode::Random);
        let next = mixer.next_index().unwrap();
        assert_eq!(next, 2);
    }

    #[test]
    fn current_outside_playable_picks_from_range() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(four_tracks());
        mixer.set_playable(Some(vec![2, 3]));
        mixer.play_decoded(0, const_deck(100, 0.5));
        mixer.cycle_loop();
        mixer.cycle_loop();
        assert_eq!(mixer.next_index(), Some(2));
        assert_eq!(mixer.prev_index(), Some(3));
    }

    #[test]
    fn stream_error_log_prints_the_first_pollerr_only() {
        let mut log = StreamErrorLog::new();
        assert_eq!(
            log.push("A backend-specific error has occurred: `alsa::poll()` returned POLLERR"),
            vec![
                "audio stream: A backend-specific error has occurred: `alsa::poll()` returned POLLERR"
                    .to_string()
            ]
        );
        assert!(log
            .push("A backend-specific error has occurred: `alsa::poll()` returned POLLERR")
            .is_empty());
        assert!(log
            .push("A backend-specific error has occurred: `alsa::poll()` returned POLLERR")
            .is_empty());
    }

    #[test]
    fn stream_error_log_summarizes_repeats_when_the_error_changes() {
        let mut log = StreamErrorLog::new();
        log.push("POLLERR");
        log.push("POLLERR");
        log.push("POLLERR");
        assert_eq!(
            log.push("underrun occurred"),
            vec![
                "audio stream: POLLERR (×3)".to_string(),
                "audio stream: underrun occurred".to_string(),
            ]
        );
    }

    #[test]
    fn pollerr_is_a_fatal_stream_error() {
        assert!(stream_error_is_fatal(
            "A backend-specific error has occurred: `alsa::poll()` returned POLLERR"
        ));
        assert!(!stream_error_is_fatal("underrun occurred"));
        assert!(!stream_error_is_fatal("Device is no longer available"));
    }

    #[test]
    fn stream_health_latches_pollerr_until_taken() {
        let health = StreamHealth::new();
        assert!(!health.take_failed());
        note_stream_error(&health, "underrun occurred");
        assert!(!health.take_failed());
        note_stream_error(
            &health,
            "A backend-specific error has occurred: `alsa::poll()` returned POLLERR",
        );
        assert!(health.take_failed());
        assert!(!health.take_failed());
    }

    #[test]
    fn rebuild_gate_blocks_until_backoff_elapses() {
        let mut gate = RebuildGate::new();
        let t0 = Instant::now();
        let gap = Duration::from_millis(500);
        assert!(gate.allow(t0, gap));
        assert!(!gate.allow(t0 + Duration::from_millis(100), gap));
        assert!(gate.allow(t0 + Duration::from_millis(500), gap));
    }
}
