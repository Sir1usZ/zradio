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
    at_tempo, cue, fade_secs_for, level_gain, place, plan_for, plan_pair, Cue, MixStyle, TailBend,
    TransitionPlan, PREFETCH_LEAD_SECS, RAMP_MAX_SECS,
};
use crate::decode::{decode_file, AudioBuf};
use crate::dsp::{fade_frames, mix_stereo_frame, Bands, Crossover, MixMode, CROSSOVER_HZ};
use crate::eq::Equalizer;
use crate::library::Track;
use crate::prefs::ShuffleMode;
use crate::remix::RemixMode;
use crate::scope::{PcmRing, PcmSnapshot};
use crate::spectrum::Spectrum;
use crate::stems::{plan_handoff, played_power, Handoff};
use crate::stretch::{render_tail, TimeMap};

const FADE_SECS: f32 = 6.0;
/// How long switching into or out of AutoMix blends the two ways of playing.
const MODE_BLEND_SECS: f32 = 0.1;
/// AutoMix moves a deck's level at most this many seconds per unit of gain.
const LEVEL_GLIDE_SECS: f32 = 0.1;
/// Audio separated on each side of an overlap, so its edges are not a segment's edges.
const VOCAL_MARGIN_SECS: f32 = 1.0;
/// Fetching the next track this much earlier leaves time to separate both decks' voices
/// on one core, about 30 s, before the overlap starts.
pub const STEMS_LEAD_SECS: f32 = 45.0;
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
    /// Tells this mix from those armed before it, so late voices go only to their own.
    id: u64,
    index: usize,
    deck: Deck,
    fade: usize,
    start_at: usize,
    plan: TransitionPlan,
    vocals: Option<VocalOverlay>,
    /// Whether its voices were handed to a worker to separate.
    vocals_asked: bool,
}

struct Deck {
    samples: Arc<Vec<f32>>,
    /// Position in deck frames: source frames, or bent-time frames once inside `tail`.
    pos: usize,
    channels: usize,
    tail: Option<BentTail>,
    /// Playback speed relative to the analysed audio; a remix resamples the buffer.
    tempo: f32,
    /// Level AutoMix plays the deck at.
    gain: f32,
    /// Level AutoMix played the last frame at, gliding to `gain` so it never jumps.
    level: f32,
    /// Most `level` moves in a frame.
    glide: f32,
    /// AutoMix plays the deck through this, so a mix can take its bass away.
    split: Crossover,
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
            split: Crossover::new(buf.sample_rate, CROSSOVER_HZ),
            samples: Arc::new(buf.samples),
            pos: 0,
            channels: buf.channels.max(1),
            tail: None,
            tempo: 1.0,
            gain: 1.0,
            level: 1.0,
            glide: 1.0 / (LEVEL_GLIDE_SECS * buf.sample_rate.max(1) as f32),
        }
    }

    /// Sets the level of a deck not heard yet, so it starts there instead of gliding.
    fn start_at(&mut self, gain: f32) {
        self.gain = gain;
        self.level = gain;
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

    /// The frame at `pos` split for AutoMix, at the deck's level, with `less` taken out of
    /// it first; call once a frame.
    fn split_frame(&mut self, less: [f32; 2]) -> Bands {
        self.level = approach(self.level, self.gain, self.glide);
        let [l, r] = self.frame(self.pos);
        self.split
            .split([(l - less[0]) * self.level, (r - less[1]) * self.level])
    }

    /// The frame at `pos`, `wet` of the way from as recorded to as AutoMix plays it;
    /// call once a frame.
    fn play_frame(&mut self, wet: f32) -> [f32; 2] {
        if wet <= 0.0 {
            return self.frame(self.pos);
        }
        let auto = self.split_frame([0.0; 2]).sum();
        blend(self.frame(self.pos), auto, wet)
    }
}

/// Frames into an overlap of `fade` frames that start at `pos` in a deck analysed as `a`
/// on which a beat falls.
fn beat_lines(a: &TrackAnalysis, pos: usize, fade: usize, sample_rate: u32) -> Vec<usize> {
    let Some(bpm) = a.bpm.filter(|&b| b > 0.0) else {
        return Vec::new();
    };
    let sr = f64::from(sample_rate);
    let beat = 60.0 / f64::from(bpm) * sr;
    if beat < 1.0 {
        return Vec::new();
    }
    let first = f64::from(a.first_beat) * sr;
    let from = ((pos as f64 - first) / beat).ceil();
    (0..)
        .map(|k| {
            (first + (from + f64::from(k)) * beat - pos as f64)
                .round()
                .max(0.0) as usize
        })
        .take_while(|&f| f < fade)
        .collect()
}

fn approach(from: f32, to: f32, step: f32) -> f32 {
    if to > from {
        (from + step).min(to)
    } else {
        (from - step).max(to)
    }
}

fn blend(raw: [f32; 2], auto: [f32; 2], wet: f32) -> [f32; 2] {
    if wet >= 1.0 {
        return auto;
    }
    let dry = 1.0 - wet;
    [raw[0] * dry + auto[0] * wet, raw[1] * dry + auto[1] * wet]
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

/// Each deck's separated voice through an armed AutoMix overlap, and where one gives way
/// to the other, so the two singers are not heard over each other.
pub struct VocalOverlay {
    arm: u64,
    /// Interleaved stereo, a frame per overlap frame, as each deck plays it.
    out: Vec<f32>,
    inn: Vec<f32>,
    handoff: Handoff,
}

/// Everything a worker needs to separate the voices of an armed mix off the mixer lock.
pub struct VocalJob {
    arm: u64,
    sample_rate: u32,
    fade: usize,
    style: MixStyle,
    /// Level each deck plays at.
    gains: (f32, f32),
    /// Overlap frames on the incoming deck's beat lines.
    beats: Vec<usize>,
    /// Interleaved stereo as each deck plays it around the overlap, which starts `lead`
    /// frames in.
    out: Vec<f32>,
    out_lead: usize,
    inn: Vec<f32>,
    inn_lead: usize,
}

impl VocalJob {
    /// The overlay for the mix, with `separate` taking the voice out of interleaved
    /// stereo at the mixer's rate; None when the two voices do not clash.
    pub fn run(
        self,
        separate: impl FnMut(&[f32], u32) -> anyhow::Result<Vec<f32>>,
    ) -> anyhow::Result<Option<VocalOverlay>> {
        let mut separate = separate;
        let fade = self.fade;
        let mut overlap = |audio: &[f32], lead: usize| -> anyhow::Result<Vec<f32>> {
            let voice = separate(audio, self.sample_rate)?;
            anyhow::ensure!(
                voice.len() == audio.len() && audio.len() >= 2 * (lead + fade),
                "{} voice samples for {} of audio",
                voice.len(),
                audio.len()
            );
            Ok(voice[2 * lead..2 * (lead + fade)].to_vec())
        };
        let out = overlap(&self.out, self.out_lead)?;
        let inn = overlap(&self.inn, self.inn_lead)?;
        let (style, (ga, gb)) = (self.style, self.gains);
        let t = |f: usize| f as f32 / fade as f32;
        let out_power = played_power(&out, self.sample_rate, |f| ga * style.voice_gains(t(f)).0);
        let in_power = played_power(&inn, self.sample_rate, |f| gb * style.voice_gains(t(f)).1);
        let handoff = plan_handoff(&out_power, &in_power, self.sample_rate, fade, &self.beats);
        Ok(handoff.map(|handoff| VocalOverlay {
            arm: self.arm,
            out,
            inn,
            handoff,
        }))
    }
}

impl VocalOverlay {
    fn fits(&self, arm: u64, fade: usize) -> bool {
        self.arm == arm && self.out.len() == 2 * fade && self.inn.len() == 2 * fade
    }

    /// How much of the outgoing and the incoming deck's voice to take out at overlap
    /// frame `f`.
    fn lose(&self, f: usize) -> ([f32; 2], [f32; 2]) {
        let (keep_out, keep_in) = self.handoff.keep(f);
        let voice = |v: &[f32], keep: f32| {
            let at = |c| v.get(2 * f + c).copied().unwrap_or(0.0) * (1.0 - keep);
            [at(0), at(1)]
        };
        (voice(&self.out, keep_out), voice(&self.inn, keep_in))
    }
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
    /// Ids handed to armed mixes so far.
    arm_seq: u64,
    /// Voices of the mix under way, if they were separated in time.
    vocals: Option<VocalOverlay>,
    stems: bool,
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
    /// How much of the output is AutoMix's: 1 in AutoMix, 0 otherwise, gliding between
    /// the two for `MODE_BLEND_SECS` after a switch so the level and bass phase don't jump.
    automix_wet: f32,
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
            arm_seq: 0,
            vocals: None,
            stems: false,
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
            automix_wet: 0.0,
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
        let gain = self.level_of(index);
        let decks = [
            (self.current_idx, self.current.as_mut()),
            (self.incoming_idx, self.incoming.as_mut()),
            (
                self.armed.as_ref().map(|a| a.index),
                self.armed.as_mut().map(|a| &mut a.deck),
            ),
        ];
        for (at, deck) in decks {
            if let (Some(deck), true) = (deck, at == Some(index)) {
                deck.gain = gain;
            }
        }
    }

    fn level_of(&self, index: usize) -> f32 {
        level_gain(self.analysis_at(index))
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
        if self.mix == MixMode::AutoMix && self.automix_wet <= 0.0 {
            // Left over from the last time AutoMix played these decks, the crossover would
            // ring out old bass. While a blend out is still running it is playing them.
            for deck in [&mut self.current, &mut self.incoming]
                .into_iter()
                .flatten()
            {
                deck.split.reset();
            }
        }
        self.status = format!("mix {}", self.mix.label());
    }

    pub fn set_mix(&mut self, mode: MixMode) {
        while self.mix != mode {
            self.cycle_mix();
        }
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current_idx
    }

    /// The plan of the transition that is running or last ran.
    pub fn last_plan(&self) -> Option<TransitionPlan> {
        self.last_plan
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
        self.vocals = None;
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
        deck.start_at(self.level_of(index));
        // A new track starts from silence, so there is nothing to blend from.
        self.automix_wet = self.automix_target();
        self.remember_played(index);
        self.current_idx = Some(index);
        self.incoming_idx = None;
        self.incoming = None;
        self.fade_total = 0;
        self.fade_pos = 0;
        self.last_plan = None;
        self.armed = None;
        self.vocals = None;
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
        incoming.start_at(self.level_of(index));
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
        self.arm_seq += 1;
        self.armed = Some(ArmedMix {
            id: self.arm_seq,
            index,
            deck: incoming,
            fade: placed.fade,
            start_at: placed.start_at,
            plan,
            vocals: None,
            vocals_asked: false,
        });
        self.status = format!("armed {}", plan.style.label());
    }

    /// Whether AutoMix separates the voices of its mixes, which needs the next track sooner.
    pub fn set_stems(&mut self, on: bool) {
        self.stems = on;
    }

    /// The armed mix's voices to separate, once per mix.
    pub fn take_vocal_job(&mut self) -> Option<VocalJob> {
        if !self.stems || self.mix != MixMode::AutoMix {
            return None;
        }
        let armed = self.armed.as_ref().filter(|a| !a.vocals_asked)?;
        let current = self.current.as_ref()?;
        let fade = armed.fade;
        let margin = (VOCAL_MARGIN_SECS * self.sample_rate as f32).round() as usize;
        // Each deck from `margin` before the overlap to `margin` after it, as it plays.
        let around = |deck: &Deck, pos: usize| {
            let from = pos.saturating_sub(margin);
            let audio: Vec<f32> = (from..pos + fade + margin)
                .flat_map(|f| deck.frame(f))
                .collect();
            (audio, pos - from)
        };
        let (out, out_lead) = around(current, armed.start_at);
        let (inn, inn_lead) = around(&armed.deck, armed.deck.pos);
        let beats = self
            .analysis_at(armed.index)
            .map(|a| at_tempo(a, armed.deck.tempo))
            .map(|a| beat_lines(&a, armed.deck.pos, fade, self.sample_rate))
            .unwrap_or_default();
        let job = VocalJob {
            arm: armed.id,
            sample_rate: self.sample_rate,
            fade,
            style: armed.plan.style,
            gains: (current.gain, armed.deck.gain),
            beats,
            out,
            out_lead,
            inn,
            inn_lead,
        };
        if let Some(armed) = self.armed.as_mut() {
            armed.vocals_asked = true;
        }
        Some(job)
    }

    /// Hands the armed mix the voices separated for it; false, and they are dropped, if
    /// that mix is no longer the one armed.
    pub fn attach_vocals(&mut self, overlay: VocalOverlay) -> bool {
        match self.armed.as_mut() {
            Some(armed) if overlay.fits(armed.id, armed.fade) => {
                armed.vocals = Some(overlay);
                true
            }
            _ => false,
        }
    }

    fn begin_mix(&mut self, index: usize, incoming: Deck, fade: usize, plan: TransitionPlan) {
        self.incoming_idx = Some(index);
        self.incoming = Some(incoming);
        self.fade_total = fade;
        self.fade_pos = 0;
        self.paused = false;
        self.done = false;
        self.armed = None;
        self.vocals = None;
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

    /// Where the mix under way hands one voice to the next, if its voices were separated.
    pub fn vocal_handoff(&self) -> Option<Handoff> {
        self.vocals.as_ref().map(|v| v.handoff)
    }

    /// Whether a mix is armed or under way; its plan was made from the analyses as they were.
    pub fn transition_pending(&self) -> bool {
        self.incoming.is_some() || self.armed.is_some()
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
                let stems = if self.stems { STEMS_LEAD_SECS } else { 0.0 };
                return pos_secs + fade + RAMP_MAX_SECS + PREFETCH_LEAD_SECS + stems >= mix_out;
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

    fn automix_target(&self) -> f32 {
        if self.mix == MixMode::AutoMix {
            1.0
        } else {
            0.0
        }
    }

    fn step_automix_wet(&mut self) {
        let step = 1.0 / (MODE_BLEND_SECS * self.sample_rate.max(1) as f32);
        self.automix_wet = approach(self.automix_wet, self.automix_target(), step);
    }

    fn next_stereo(&mut self) -> [f32; 2] {
        if self.paused {
            return [0.0, 0.0];
        }
        self.step_automix_wet();
        if self.incoming.is_some() && self.fade_total > 0 {
            return self.mix_frame();
        }
        if let Some(armed) = self.armed.as_ref() {
            let pos = self.current.as_ref().map(|d| d.pos).unwrap_or(0);
            if pos >= armed.start_at {
                let armed = self.armed.take().unwrap();
                self.begin_mix(armed.index, armed.deck, armed.fade, armed.plan);
                // The voices were separated from the overlap as armed, so only a mix that
                // starts where it was armed to can take them out.
                if pos == armed.start_at {
                    self.vocals = armed.vocals;
                }
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
        let frame = deck.play_frame(self.automix_wet);
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

    /// Both decks at `t` through the transition, as AutoMix mixes them.
    fn automix_frame(&mut self, t: f32) -> [f32; 2] {
        let (less_a, less_b) = self
            .vocals
            .as_ref()
            .map_or(([0.0; 2], [0.0; 2]), |v| v.lose(self.fade_pos));
        let a = self
            .current
            .as_mut()
            .map(|d| d.split_frame(less_a))
            .unwrap_or_default();
        let b = self
            .incoming
            .as_mut()
            .map(|d| d.split_frame(less_b))
            .unwrap_or_default();
        self.last_plan
            .map(|p| p.style)
            .unwrap_or(MixStyle::BassSwap)
            .mix(a, b, t)
    }

    /// Both decks at `t` through the transition, as Cut and Crossfade mix them.
    fn crossfade_frame(&self, t: f32) -> [f32; 2] {
        let a = self
            .current
            .as_ref()
            .map(|d| d.frame(d.pos))
            .unwrap_or_default();
        let b = self
            .incoming
            .as_ref()
            .map(|d| d.frame(d.pos))
            .unwrap_or_default();
        mix_stereo_frame(a, b, t)
    }

    fn mix_frame(&mut self) -> [f32; 2] {
        let t = if self.fade_total == 0 {
            1.0
        } else {
            self.fade_pos as f32 / self.fade_total as f32
        };
        let wet = self.automix_wet;
        let out = if wet <= 0.0 {
            self.crossfade_frame(t)
        } else {
            let auto = self.automix_frame(t);
            blend(self.crossfade_frame(t), auto, wet)
        };
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
        out
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
        self.vocals = None;
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
            loudness: None,
            peak: 0.0,
            music_start: 0.0,
            music_end: duration,
            beat_this: false,
        }
    }

    #[test]
    fn automix_levels_the_deck_even_when_its_analysis_lands_late() {
        // -6 LUFS with a full-scale peak comes down 8 dB to the -14 LUFS target.
        let loud = crate::analysis::TrackAnalysis {
            loudness: Some(-6.0),
            peak: 1.0,
            ..analysis_at(0.0, 16.0, 20.0)
        };
        let want = 0.5 * 10f32.powf(-8.0 / 20.0);
        for (mode, gain) in [(MixMode::AutoMix, want), (MixMode::Crossfade, 0.5)] {
            let mut mixer = Mixer::new(48_000, 2);
            mixer.set_tracks(two_tracks());
            mixer.mix = mode;
            mixer.play_decoded(0, const_deck(48_000, 0.5));
            mixer.set_analysis(0, loud.clone());
            // AutoMix's crossover turns the phase of the step the deck starts on.
            let got = (0..4_800).map(|_| mixer.next_stereo()[0]).last().unwrap();
            assert!(
                (got - gain).abs() < 1e-5,
                "{mode:?} played {got}, not {gain}"
            );
        }
    }

    /// 60 Hz at 0.5: sits under the crossover, so the allpass turns its phase.
    fn bass_tone(frames: usize) -> AudioBuf {
        AudioBuf {
            samples: (0..frames)
                .flat_map(|f| {
                    let v = 0.5 * (std::f32::consts::TAU * 60.0 * f as f32 / 48_000.0).sin();
                    [v, v]
                })
                .collect(),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    /// The most one frame can move the 60 Hz tone, with room for the blend itself.
    const BASS_STEP: f32 = 0.5 * std::f32::consts::TAU * 60.0 / 48_000.0 * 1.2;

    fn left(mixer: &mut Mixer, frames: usize) -> Vec<f32> {
        (0..frames).map(|_| mixer.next_stereo()[0]).collect()
    }

    fn largest_step(out: &[f32]) -> f32 {
        out.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    fn rms(out: &[f32]) -> f32 {
        (out.iter().map(|v| v * v).sum::<f32>() / out.len() as f32).sqrt()
    }

    fn loud_analysis() -> crate::analysis::TrackAnalysis {
        // -6 LUFS with a full-scale peak: AutoMix plays it 8 dB down.
        crate::analysis::TrackAnalysis {
            loudness: Some(-6.0),
            peak: 1.0,
            ..analysis_at(0.0, 16.0, 20.0)
        }
    }

    #[test]
    fn switching_into_and_out_of_automix_glides_instead_of_jumping() {
        let gain = 10f32.powf(-8.0 / 20.0);
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.set_analysis(0, loud_analysis());
        mixer.play_decoded(0, bass_tone(48_000 * 10));
        // Switch on a peak of the tone, where a jump would be largest.
        let before = left(&mut mixer, 24_200);
        mixer.cycle_mix();
        assert_eq!(mixer.mix(), MixMode::AutoMix);
        let into = left(&mut mixer, 24_000);
        let mut joined = before[before.len() - 100..].to_vec();
        joined.extend(&into);
        let step = largest_step(&joined);
        assert!(step <= BASS_STEP, "switching into AutoMix stepped {step}");
        let (was, now) = (rms(&before[..24_000]), rms(&into[12_000..]));
        assert!(
            (now / was - gain).abs() < 0.01,
            "AutoMix settled at {} of the level, not {gain}",
            now / was
        );

        mixer.cycle_mix();
        assert_eq!(mixer.mix(), MixMode::Cut);
        let out = left(&mut mixer, 24_000);
        let mut joined = into[into.len() - 100..].to_vec();
        joined.extend(&out);
        let step = largest_step(&joined);
        assert!(step <= BASS_STEP, "switching out of AutoMix stepped {step}");
        assert!((rms(&out[12_000..]) / was - 1.0).abs() < 0.01);

        // Pressing `m` fast: back into AutoMix halfway through blending into it, then out
        // and in again before the blend finishes.
        mixer.set_mix(MixMode::AutoMix);
        let mut fast = out[out.len() - 100..].to_vec();
        fast.extend(left(&mut mixer, 2_400));
        mixer.set_mix(MixMode::Cut);
        fast.extend(left(&mut mixer, 1_200));
        mixer.set_mix(MixMode::AutoMix);
        fast.extend(left(&mut mixer, 9_600));
        let step = largest_step(&fast);
        assert!(step <= BASS_STEP, "switching back mid-blend stepped {step}");
    }

    #[test]
    fn late_analysis_glides_the_level_instead_of_jumping() {
        let gain = 10f32.powf(-8.0 / 20.0);
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.set_mix(MixMode::AutoMix);
        mixer.play_decoded(0, bass_tone(48_000 * 10));
        // Land on a peak of the tone, where a jump would be largest.
        let before = left(&mut mixer, 24_200);
        mixer.set_analysis(0, loud_analysis());
        let after = left(&mut mixer, 24_000);
        let mut joined = before[before.len() - 100..].to_vec();
        joined.extend(&after);
        let step = largest_step(&joined);
        assert!(step <= BASS_STEP, "the analysis landing stepped {step}");
        let (was, now) = (rms(&before[12_200..]), rms(&after[12_000..]));
        assert!((now / was - gain).abs() < 0.01, "settled at {}", now / was);
    }

    #[test]
    fn a_deck_whose_analysis_is_known_starts_at_its_level() {
        let gain = 10f32.powf(-8.0 / 20.0);
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.set_mix(MixMode::AutoMix);
        mixer.set_analysis(0, loud_analysis());
        mixer.play_decoded(0, bass_tone(48_000));
        // The first 100 ms, six periods of the tone.
        let level = rms(&left(&mut mixer, 4_800)) / (0.5 / 2f32.sqrt());
        assert!(
            (level - gain).abs() < 0.02,
            "started at {level}, not {gain}"
        );
    }

    #[test]
    fn switching_mode_mid_transition_glides_instead_of_jumping() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.set_mix(MixMode::AutoMix);
        mixer.set_analysis(0, loud_analysis());
        mixer.play_decoded(0, bass_tone(48_000 * 10));
        let plan = mixer.plan_transition(1, true);
        let silent = Deck::from_buf(const_deck(48_000 * 10, 0.0));
        mixer.begin_mix(1, silent, 48_000 * 4, plan);
        let before = left(&mut mixer, 48_200);
        mixer.set_mix(MixMode::Cut);
        assert!(mixer.fading());
        let after = left(&mut mixer, 12_000);
        let mut joined = before[before.len() - 100..].to_vec();
        joined.extend(&after);
        let step = largest_step(&joined);
        assert!(step <= BASS_STEP, "switching mid-transition stepped {step}");
    }

    #[test]
    fn automix_never_plays_a_deck_past_the_ceiling() {
        // A clipped 808: 20 ms of 50 Hz square every half second. Quiet enough to be raised,
        // and its flat tops grow by 4 dB once the crossover turns the bass's phase.
        let samples = (0..48_000 * 6)
            .flat_map(|f| {
                let into = f as f32 / 48_000.0 % 0.5;
                let v = if into < 0.02 {
                    0.5 * (std::f32::consts::TAU * 50.0 * into).sin().signum()
                } else {
                    0.0
                };
                [v, v]
            })
            .collect();
        let buf = AudioBuf {
            samples,
            sample_rate: 48_000,
            channels: 2,
        };
        let analysis = crate::analysis::analyze_buffer(&buf);
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.set_analysis(0, analysis);
        mixer.play_decoded(0, buf);
        let peak = (0..48_000 * 6)
            .map(|_| mixer.next_stereo()[0].abs())
            .fold(0.0, f32::max);
        let db = 20.0 * peak.log10();
        assert!(db < -0.99, "AutoMix played it at {db:.2} dBFS");
    }

    #[test]
    fn automix_comes_back_without_old_bass() {
        // A fifth of a second of 50 Hz, then silence.
        let samples = (0..48_000)
            .flat_map(|f| {
                let v = if f < 9_600 {
                    0.5 * (std::f32::consts::TAU * 50.0 * f as f32 / 48_000.0).sin()
                } else {
                    0.0
                };
                [v, v]
            })
            .collect();
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(
            0,
            AudioBuf {
                samples,
                sample_rate: 48_000,
                channels: 2,
            },
        );
        let play = |mixer: &mut Mixer, frames: usize| {
            (0..frames)
                .map(|_| mixer.next_stereo()[0].abs())
                .fold(0.0, f32::max)
        };
        play(&mut mixer, 4_800);
        // Out of AutoMix in the middle of the bass, back once the deck has gone quiet.
        mixer.set_mix(MixMode::Crossfade);
        play(&mut mixer, 9_600);
        mixer.set_mix(MixMode::AutoMix);
        let ring = play(&mut mixer, 2_400);
        assert!(ring < 1e-3, "AutoMix came back ringing at {ring}");
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

    #[test]
    fn a_mix_is_pending_from_when_it_is_armed_until_it_is_done() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, const_deck(48_000 * 20, 0.8));
        mixer.set_analysis(0, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(1, analysis_at(4.0, 18.0, 20.0));
        assert!(!mixer.transition_pending());
        mixer.set_playhead(48_000 * 10);
        mixer.start_transition(1, const_deck(48_000 * 20, 0.2), false);
        assert!(mixer.armed.is_some() && mixer.transition_pending());
        let mut out = vec![0.0f32; 4_800 * 2];
        while mixer.armed.is_some() {
            mixer.fill(&mut out);
        }
        assert!(mixer.fading() && mixer.transition_pending());
        while mixer.fading() {
            mixer.fill(&mut out);
        }
        assert_eq!(mixer.current_idx, Some(1));
        assert!(!mixer.transition_pending());
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

    fn sine(frames: usize, hz: f32, amp: f32) -> AudioBuf {
        let w = std::f32::consts::TAU * hz / 48_000.0;
        AudioBuf {
            samples: (0..frames)
                .flat_map(|f| {
                    let v = (f as f32 * w).sin() * amp;
                    [v, v]
                })
                .collect(),
            sample_rate: 48_000,
            channels: 2,
        }
    }

    /// Mixer with an AutoMix from `a` into `b` armed and its playhead where the overlap
    /// starts.
    fn armed_voices(a: AudioBuf, b: AudioBuf) -> Mixer {
        armed_voices_into(a, b, 4.0)
    }

    /// As `armed_voices`, with `b` coming in at `mix_in` seconds.
    fn armed_voices_into(a: AudioBuf, b: AudioBuf, mix_in: f32) -> Mixer {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, a);
        mixer.set_analysis(0, analysis_at(0.0, 16.0, 20.0));
        mixer.set_analysis(1, analysis_at(mix_in, 18.0, 20.0));
        mixer.set_playhead(48_000 * 10);
        mixer.start_transition(1, b, false);
        let start_at = mixer.armed.as_ref().expect("armed").start_at;
        mixer.set_playhead(start_at);
        mixer
    }

    /// The `fade` frames of a deck from `pos`, as it plays them.
    fn deck_frames(deck: &Deck, pos: usize, fade: usize) -> Vec<f32> {
        (pos..pos + fade).flat_map(|f| deck.frame(f)).collect()
    }

    fn play_overlap(mixer: &mut Mixer, fade: usize) -> Vec<[f32; 2]> {
        (0..fade).map(|_| mixer.next_stereo()).collect()
    }

    #[test]
    fn the_handoff_takes_each_voice_out_of_the_mix_in_turn() {
        let voice_a = || sine(48_000 * 20, 1_000.0, 0.3);
        let voice_b = || sine(48_000 * 20, 1_500.0, 0.3);
        let silent = || const_deck(48_000 * 20, 0.0);
        let mut mixed = armed_voices(voice_a(), voice_b());
        let armed = mixed.armed.as_ref().unwrap();
        let (fade, start_at, arm) = (armed.fade, armed.start_at, armed.id);
        let handoff = Handoff {
            at: fade / 2,
            ramp: 14_400,
        };
        let overlay = VocalOverlay {
            arm,
            out: deck_frames(mixed.current.as_ref().unwrap(), start_at, fade),
            inn: deck_frames(&armed.deck, armed.deck.pos, fade),
            handoff,
        };
        assert!(mixed.attach_vocals(overlay));
        let plain = play_overlap(&mut armed_voices(voice_a(), voice_b()), fade);
        let only_out = play_overlap(&mut armed_voices(voice_a(), silent()), fade);
        let only_in = play_overlap(&mut armed_voices(silent(), voice_b()), fade);
        let got = play_overlap(&mut mixed, fade);
        let before = handoff.at - handoff.ramp;
        let settled = handoff.at + handoff.ramp + 4_800;
        let gap = |x: [f32; 2], y: [f32; 2]| (x[0] - y[0]).abs().max((x[1] - y[1]).abs());
        assert!(gap(plain[before - 1], only_out[before - 1]) > 1e-3);
        assert!(gap(plain[settled], only_in[settled]) > 1e-3);
        for f in 0..before {
            assert_eq!(
                got[f], only_out[f],
                "frame {f}: the incoming voice is heard"
            );
        }
        for f in settled..fade {
            let off = gap(got[f], only_in[f]);
            assert!(off < 1e-4, "frame {f}: the outgoing voice is heard, {off}");
        }
    }

    #[test]
    fn voices_go_only_to_the_mix_they_were_separated_for() {
        let mut mixer = armed_voices(sine(48_000 * 20, 1_000.0, 0.3), tone(48_000 * 20));
        let armed = mixer.armed.as_ref().unwrap();
        let (fade, arm) = (armed.fade, armed.id);
        let overlay = |arm, frames: usize| VocalOverlay {
            arm,
            out: vec![0.1; 2 * frames],
            inn: vec![0.1; 2 * frames],
            handoff: Handoff {
                at: fade / 2,
                ramp: 100,
            },
        };
        assert!(!mixer.attach_vocals(overlay(arm + 1, fade)));
        assert!(!mixer.attach_vocals(overlay(arm, fade - 1)));
        assert!(mixer.armed.as_ref().unwrap().vocals.is_none());
        assert!(mixer.attach_vocals(overlay(arm, fade)));
        mixer.next_stereo();
        assert!(mixer.fading() && mixer.vocals.is_some());
        run_until_current(&mut mixer, 1, 20);
        assert!(mixer.vocals.is_none(), "kept past the mix");

        // A mix that does not start where it was armed to starts without them.
        let mut late = armed_voices(sine(48_000 * 20, 1_000.0, 0.3), tone(48_000 * 20));
        let (arm, start_at) = {
            let armed = late.armed.as_ref().unwrap();
            (armed.id, armed.start_at)
        };
        assert!(late.attach_vocals(overlay(arm, fade)));
        late.set_playhead(start_at + 10);
        late.next_stereo();
        assert!(late.fading() && late.vocals.is_none());

        // Seeking drops the armed mix, and its voices with it; the next one gets a new id.
        let mut seeked = armed_voices(sine(48_000 * 20, 1_000.0, 0.3), tone(48_000 * 20));
        let arm = seeked.armed.as_ref().unwrap().id;
        assert!(seeked.attach_vocals(overlay(arm, fade)));
        seeked.seek_by(0.0);
        assert!(seeked.armed.is_none() && seeked.vocals.is_none());
        assert!(!seeked.attach_vocals(overlay(arm, fade)));
    }

    // Neither repeats on a whole second, so voices taken from the wrong frames differ.
    fn voice_a() -> AudioBuf {
        sine(48_000 * 20, 1_000.5, 0.3)
    }

    fn voice_b() -> AudioBuf {
        sine(48_000 * 20, 1_500.25, 0.3)
    }

    #[test]
    fn voices_are_separated_once_per_armed_mix_and_only_with_stems_on() {
        let mut mixer = armed_voices(voice_a(), voice_b());
        assert!(mixer.take_vocal_job().is_none(), "stems are off");
        mixer.set_stems(true);
        assert!(mixer.take_vocal_job().is_some());
        assert!(mixer.take_vocal_job().is_none(), "asked twice");
    }

    #[test]
    fn separated_voices_line_up_with_the_overlap() {
        // Coming in at 0.5 s leaves less than the margin before the overlap.
        for mix_in in [4.0, 0.5] {
            let mut mixer = armed_voices_into(voice_a(), voice_b(), mix_in);
            mixer.set_stems(true);
            let armed = mixer.armed.as_ref().unwrap();
            let (fade, start_at, in_pos) = (armed.fade, armed.start_at, armed.deck.pos);
            let want_out = deck_frames(mixer.current.as_ref().unwrap(), start_at, fade);
            let want_in = deck_frames(&armed.deck, in_pos, fade);
            let job = mixer.take_vocal_job().unwrap();
            let mut calls = 0;
            let overlay = job
                .run(|x, rate| {
                    assert_eq!(rate, 48_000);
                    calls += 1;
                    Ok(x.to_vec())
                })
                .unwrap()
                .expect("two voices all through the overlap clash");
            assert_eq!(calls, 2);
            assert!(overlay.out == want_out, "{mix_in}: outgoing voice is off");
            assert!(overlay.inn == want_in, "{mix_in}: incoming voice is off");
            let at = in_pos + overlay.handoff.at;
            assert_eq!(at % 24_000, 0, "{mix_in}: handoff at {at} is off the beat");
            assert!(mixer.attach_vocals(overlay));
        }
    }

    #[test]
    fn voices_that_never_clash_get_no_overlay() {
        let job = || {
            let mut mixer = armed_voices(voice_a(), voice_b());
            mixer.set_stems(true);
            mixer.take_vocal_job().unwrap()
        };
        let none = job().run(|x, _| Ok(vec![0.0; x.len()])).unwrap();
        assert!(none.is_none());
        assert!(job().run(|_, _| Err(anyhow::anyhow!("no model"))).is_err());
        let short = job().run(|x, _| Ok(x[..x.len() / 2].to_vec()));
        assert!(
            short.is_err(),
            "a short voice must not be read past its end"
        );
    }

    #[test]
    fn beat_lines_fall_on_the_incoming_grid() {
        let a = analysis_at(4.0, 18.0, 20.0);
        let lines = |pos, fade| beat_lines(&a, pos, fade, 48_000);
        assert_eq!(lines(192_000, 100_000), [0, 24_000, 48_000, 72_000, 96_000]);
        assert_eq!(lines(198_000, 100_000), [18_000, 42_000, 66_000, 90_000]);
        // The grid runs back before the first beat too.
        assert_eq!(lines(0, 50_000), [0, 24_000, 48_000]);
        let no_grid = TrackAnalysis {
            bpm: None,
            ..a.clone()
        };
        assert!(beat_lines(&no_grid, 192_000, 100_000, 48_000).is_empty());
    }

    #[test]
    fn separating_voices_fetches_the_next_track_sooner() {
        let mut mixer = Mixer::new(48_000, 2);
        mixer.set_tracks(two_tracks());
        mixer.mix = MixMode::AutoMix;
        mixer.play_decoded(0, const_deck(48_000 * 72, 0.5));
        mixer.set_analysis(0, analysis_at(0.0, 70.0, 72.0));
        let first = |mixer: &mut Mixer| {
            (0..72).find(|&s| {
                mixer.set_playhead(48_000 * s);
                mixer.should_prefetch()
            })
        };
        let plain = first(&mut mixer).expect("fetches");
        mixer.set_stems(true);
        let early = first(&mut mixer).expect("fetches");
        assert!(early > 0, "clamped at the start");
        assert_eq!(plain - early, STEMS_LEAD_SECS as usize);
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
