use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream};

use crate::analysis::TrackAnalysis;
use crate::automix::{
    aligned_mix_in, fade_secs_for, mix_start_frame, pick_next, plan_for, MixStyle, TransitionPlan,
};
use crate::decode::{decode_file, AudioBuf};
use crate::dsp::{
    fade_frames, lp_alpha, mix_bass_swap, mix_echo_out, mix_filter_blend, mix_stereo_frame,
    resample_linear, MixMode, OnePole,
};
use crate::eq::Equalizer;
use crate::library::Track;
use crate::remix::RemixMode;
use crate::spectrum::Spectrum;

const FADE_SECS: f32 = 6.0;

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
    pub spectrum: Vec<String>,
    pub spectrum_levels: [f32; 24],
    pub shuffle: bool,
    pub loop_mode: LoopMode,
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
    samples: Vec<f32>,
    pos: usize,
    channels: usize,
}

impl Deck {
    fn from_buf(buf: AudioBuf) -> Self {
        Self {
            samples: buf.samples,
            pos: 0,
            channels: buf.channels.max(1),
        }
    }

    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    fn remaining(&self) -> usize {
        self.frames().saturating_sub(self.pos)
    }

    fn seek_seconds(&mut self, seconds: f32, sample_rate: u32) {
        if sample_rate == 0 || self.frames() == 0 {
            return;
        }
        let frame = (seconds.max(0.0) * sample_rate as f32).round() as usize;
        self.pos = frame.min(self.frames().saturating_sub(1));
    }

    fn frame(&self, at: usize) -> [f32; 2] {
        if self.frames() == 0 {
            return [0.0, 0.0];
        }
        let i = at.min(self.frames() - 1) * self.channels;
        if self.channels == 1 {
            let s = self.samples[i];
            [s, s]
        } else {
            [
                self.samples[i],
                self.samples.get(i + 1).copied().unwrap_or(0.0),
            ]
        }
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
    current_lp: OnePole,
    incoming_lp: OnePole,
    volume: f32,
    sample_rate: u32,
    channels: usize,
    status: String,
    done: bool,
    spectrum: Spectrum,
    shuffle: bool,
    loop_mode: LoopMode,
    taste_boosts: Vec<f32>,
    eq: Equalizer,
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
            loop_mode: LoopMode::Off,
            taste_boosts: Vec::new(),
            eq: Equalizer::new(sample_rate, [0.0; 5]),
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

    pub fn reset_eq(&mut self) {
        self.eq.reset(self.sample_rate);
        self.status = "eq reset".into();
    }

    pub fn eq_db(&self) -> [f32; 5] {
        self.eq.db()
    }

    pub fn set_tracks(&mut self, tracks: Vec<Track>) {
        self.analyses = vec![None; tracks.len()];
        self.taste_boosts = vec![0.0; tracks.len()];
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
        let frames = deck.frames();
        if frames == 0 {
            0.0
        } else {
            (deck.pos as f32 / frames as f32).clamp(0.0, 1.0)
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

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.status = if self.shuffle {
            "shuffle on"
        } else {
            "shuffle off"
        }
        .into();
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
                deck.pos as f32 / sr as f32
            };
            deck.seek_seconds(now + seconds, sr);
            self.status = format!("seek {seconds:+.0}s");
        }
        self.clear_ahead();
    }

    pub fn seek_to(&mut self, seconds: f32) {
        let sr = self.sample_rate;
        if let Some(deck) = self.current.as_mut() {
            deck.seek_seconds(seconds, sr);
            self.status = format!("seek {seconds:.0}s");
        }
        self.clear_ahead();
    }

    fn clear_ahead(&mut self) {
        self.armed = None;
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
        self.current_idx = Some(index);
        self.incoming_idx = None;
        self.incoming = None;
        self.fade_total = 0;
        self.fade_pos = 0;
        self.last_plan = None;
        self.armed = None;
        self.current_lp = OnePole::default();
        self.incoming_lp = OnePole::default();
        self.current = Some(Deck::from_buf(buf));
        self.paused = false;
        self.done = false;
        self.status = self.play_status(index);
    }

    pub fn start_transition(&mut self, index: usize, buf: AudioBuf, skip: bool) {
        let plan = plan_for(
            self.current_idx,
            index,
            &self.analyses,
            self.mix,
            skip && self.mix != MixMode::AutoMix,
        );
        let fade_secs = plan.fade_secs;
        let buf = beatmatch(buf, plan.rate);
        if !self.mix.uses_fade() || self.current.as_ref().is_none_or(|d| d.remaining() < 2) {
            self.play_decoded(index, buf);
            return;
        }
        let mut incoming = Deck::from_buf(buf);
        if self.mix == MixMode::AutoMix {
            let pos_secs = self
                .current
                .as_ref()
                .map(|d| d.pos as f32 / self.sample_rate.max(1) as f32)
                .unwrap_or(0.0);
            let mix_in = aligned_mix_in(
                pos_secs,
                self.analysis_at_current(),
                plan.next_mix_in.unwrap_or(0.0),
                plan.rate,
            );
            incoming.seek_seconds(mix_in, self.sample_rate);
        }
        let remain = self
            .current
            .as_ref()
            .map(Deck::remaining)
            .unwrap_or(0)
            .min(incoming.remaining())
            .max(2);
        let fade = fade_frames(self.sample_rate, remain, fade_secs);
        if fade < 2 {
            self.play_decoded(
                index,
                AudioBuf {
                    samples: incoming.samples,
                    sample_rate: self.sample_rate,
                    channels: incoming.channels,
                },
            );
            return;
        }
        if self.mix == MixMode::AutoMix && !skip {
            let start_at = self.current.as_ref().map_or(0, |d| {
                mix_start_frame(
                    d.pos,
                    d.remaining(),
                    plan.current_mix_out,
                    fade_secs,
                    self.sample_rate,
                )
            });
            self.armed = Some(ArmedMix {
                index,
                deck: incoming,
                fade,
                start_at,
                plan,
            });
            self.status = format!("armed {}", plan.style.label());
            return;
        }
        self.begin_mix(index, incoming, fade, plan);
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
            if let Some(mix_out) = self.analysis_at_current().map(|a| a.mix_out) {
                let pos_secs = deck.pos as f32 / self.sample_rate.max(1) as f32;
                let fade = fade_secs_for(self.mix, self.analysis_at_current(), false);
                return pos_secs + fade + 4.0 >= mix_out;
            }
        }
        let need = fade_frames(self.sample_rate, deck.frames(), FADE_SECS)
            .saturating_add(self.sample_rate as usize);
        deck.remaining() <= need
    }

    pub fn next_index(&self) -> Option<usize> {
        if self.loop_mode == LoopMode::One {
            return self.current_idx;
        }
        if self.shuffle && self.tracks.len() > 1 {
            return Some(self.shuffled_next());
        }
        if self.loop_mode == LoopMode::All {
            let len = self.tracks.len();
            if len == 0 {
                return None;
            }
            return Some(self.current_idx.map(|i| (i + 1) % len).unwrap_or(0));
        }
        pick_next(
            &self.tracks,
            self.current_idx,
            &self.analyses,
            self.mix,
            &self.taste_boosts,
        )
        .map(|p| p.next_index)
    }

    fn shuffled_next(&self) -> usize {
        let len = self.tracks.len();
        let cur = self.current_idx.unwrap_or(0);
        let seed = (self.current.as_ref().map(|d| d.pos).unwrap_or(0) ^ (cur * 1_000_003) ^ len)
            % (len - 1);
        if seed >= cur {
            seed + 1
        } else {
            seed
        }
    }

    pub fn prev_index(&self) -> Option<usize> {
        let len = self.tracks.len();
        if len == 0 {
            return None;
        }
        Some(
            self.current_idx
                .map(|i| if i == 0 { len - 1 } else { i - 1 })
                .unwrap_or(0),
        )
    }

    pub fn take_finished(&mut self) -> bool {
        let done = self.done;
        self.done = false;
        done
    }

    pub fn snapshot(&self, selected: usize) -> Snapshot {
        let duration = self.current.as_ref().map(Deck::frames).unwrap_or(0);
        let pos = self.current.as_ref().map(|d| d.pos).unwrap_or(0);
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
            spectrum: self.spectrum.bars(if self.paused { 4 } else { 8 }),
            spectrum_levels: self.spectrum.levels(),
            shuffle: self.shuffle,
            loop_mode: self.loop_mode,
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
        let plan = pick_next(
            &self.tracks,
            self.current_idx,
            &self.analyses,
            self.mix,
            &self.taste_boosts,
        )?;
        if plan.next_index == self.current_idx.unwrap_or(usize::MAX) {
            return None;
        }
        Some(format!(
            "next {} {}",
            self.title_at(plan.next_index),
            self.meta_label(plan.next_index)
        ))
    }

    pub fn fill(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        for frame in out.chunks_mut(ch) {
            let stereo = self.next_stereo();
            let stereo = self.eq.process_stereo(stereo);
            match ch {
                1 => frame[0] = (stereo[0] + stereo[1]) * 0.5 * self.volume,
                _ => {
                    frame[0] = stereo[0] * self.volume;
                    if frame.len() > 1 {
                        frame[1] = stereo[1] * self.volume;
                    }
                    for s in frame.iter_mut().skip(2) {
                        *s = 0.0;
                    }
                }
            }
        }
        let hop = (ch.max(1) * 32).max(1);
        for sample in out.iter().step_by(hop) {
            self.spectrum.push_fft_frame(*sample);
        }
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
                deck.pos = 0;
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
                deck.pos = 0;
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

fn beatmatch(buf: AudioBuf, rate: f32) -> AudioBuf {
    if (rate - 1.0).abs() < 0.004 || buf.samples.is_empty() {
        return buf;
    }
    let in_rate = ((buf.sample_rate as f32) * rate).round() as u32;
    let samples = resample_linear(
        &buf.samples,
        buf.channels,
        in_rate.max(1),
        buf.channels,
        buf.sample_rate,
    );
    AudioBuf {
        samples,
        sample_rate: buf.sample_rate,
        channels: buf.channels,
    }
}

pub struct Player {
    pub mixer: Arc<Mutex<Mixer>>,
    _stream: Stream,
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
        let mixer = Arc::new(Mutex::new(Mixer::new(sample_rate, channels)));
        let stream_mixer = Arc::clone(&mixer);
        let stream = match supported.sample_format() {
            SampleFormat::F32 => build_stream::<f32>(&device, &supported.into(), stream_mixer)?,
            SampleFormat::I16 => build_stream::<i16>(&device, &supported.into(), stream_mixer)?,
            SampleFormat::U16 => build_stream::<u16>(&device, &supported.into(), stream_mixer)?,
            other => return Err(anyhow!("unsupported sample format {other}")),
        };
        stream.play()?;
        Ok(Self {
            mixer,
            _stream: stream,
            sample_rate,
            channels,
        })
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
) -> anyhow::Result<Stream>
where
    T: Sample + cpal::FromSample<f32> + cpal::SizedSample,
{
    let err_fn = |err| eprintln!("audio stream: {err}");
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
}
