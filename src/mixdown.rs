//! Offline AutoMix render: plays a transition through the real mixer into a buffer, so a mix
//! can be listened to, measured or tested without an audio device.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};

use crate::analysis::TrackAnalysis;
use crate::automix::TransitionPlan;
use crate::decode::AudioBuf;
use crate::dsp::MixMode;
use crate::engine::Mixer;
use crate::library::Track;

/// Frames per `fill`; the overlap is placed to within one block.
const BLOCK: usize = 32;
/// How much earlier than the overlap the outgoing track starts, on top of `before_secs`, so
/// the mixer picks the moment to fetch the next track as it does live.
const LEAD_SECS: f32 = 40.0;

#[derive(Clone)]
pub struct Side {
    pub path: PathBuf,
    pub buf: AudioBuf,
    pub analysis: TrackAnalysis,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub mode: MixMode,
    /// Skip to the next track this many seconds into the outgoing one instead of mixing out.
    pub skip_at: Option<f32>,
    /// Audio kept before the overlap starts and after it ends.
    pub before_secs: f32,
    pub after_secs: f32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: MixMode::AutoMix,
            skip_at: None,
            before_secs: 12.0,
            after_secs: 6.0,
        }
    }
}

pub struct Mixdown {
    /// Interleaved, at the input buffers' rate and channel count.
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
    /// The overlap in frames of `samples`; empty for a cut.
    pub overlap: Range<usize>,
    /// Where in the outgoing track the overlap starts, in seconds of its source.
    pub mix_at: f32,
    pub plan: Option<TransitionPlan>,
}

impl Mixdown {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    pub fn secs(&self, frame: usize) -> f32 {
        frame as f32 / self.sample_rate.max(1) as f32
    }

    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }
}

/// Plays `a` into `b` the way the player does: the mixer decides when to ask for `b`, and `b`
/// arrives the way the decode worker hands it over.
pub fn render(a: Side, b: Side, opts: &Options) -> anyhow::Result<Mixdown> {
    let sr = a.buf.sample_rate;
    let ch = a.buf.channels;
    ensure!(
        sr > 0 && ch > 0 && a.buf.frames() > 0,
        "empty outgoing track"
    );
    ensure!(
        b.buf.sample_rate == sr && b.buf.channels == ch,
        "tracks differ in rate or channels"
    );
    let a_secs = a.buf.frames() as f32 / sr as f32;
    let from = match opts.skip_at {
        Some(at) => {
            ensure!(
                (0.0..a_secs).contains(&at),
                "skip at {at} s is past the end"
            );
            at - opts.before_secs
        }
        None => a.analysis.mix_out.min(a_secs) - LEAD_SECS - opts.before_secs,
    }
    .max(0.0);
    let limit = a.buf.frames() + b.buf.frames() + 60 * sr as usize;
    let skip_frame = opts
        .skip_at
        .map(|at| ((at - from) * sr as f32).round() as usize);

    let mut mixer = Mixer::new(sr, ch);
    mixer.set_tracks(vec![track(&a.path), track(&b.path)]);
    mixer.set_mix(opts.mode);
    mixer.bump_volume(1.0);
    mixer.set_analysis(0, a.analysis);
    mixer.play_decoded(0, a.buf);
    mixer.seek_to(from);

    let mut next = Some(b);
    let mut block = vec![0.0f32; BLOCK * ch];
    let mut out = Vec::new();
    let mut overlap: Option<Range<usize>> = None;
    let mut start = None;
    let mut mix_at = 0.0;
    let mut handed_at = 0.0;
    let source_secs = |m: &Mixer| m.playback_view(0).position_frames as f32 / sr as f32;
    loop {
        let frame = out.len() / ch;
        if next.is_some() {
            let ask = match skip_frame {
                Some(at) => (frame >= at).then_some((true, true)),
                None if mixer.take_auto_advance().is_some() => Some((false, false)),
                None if mixer.mix().uses_fade() && mixer.should_prefetch() => Some((true, false)),
                None => None,
            };
            if let Some((transition, skip)) = ask {
                handed_at = source_secs(&mixer);
                hand_over(&mut mixer, next.take().unwrap(), transition, skip);
            }
        }
        mixer.fill(&mut block);
        out.extend_from_slice(&block);
        let fading = mixer.fading();
        if fading && start.is_none() {
            start = Some(frame);
            mix_at = source_secs(&mixer);
        }
        if overlap.is_none() && next.is_none() && !fading && mixer.current_index() == Some(1) {
            // A cut never fades; it hands over where the outgoing deck stood.
            let begin = *start.get_or_insert_with(|| {
                mix_at = handed_at;
                frame
            });
            overlap = Some(begin..frame);
        }
        if let Some(o) = &overlap {
            if out.len() / ch >= o.end + (opts.after_secs.max(0.0) * sr as f32) as usize {
                break;
            }
        }
        if out.len() / ch > limit {
            bail!("the mixer never handed over to the next track");
        }
    }
    let overlap = overlap.unwrap_or_default();
    let keep = overlap
        .start
        .saturating_sub((opts.before_secs.max(0.0) * sr as f32) as usize);
    Ok(Mixdown {
        samples: out.split_off(keep * ch),
        sample_rate: sr,
        channels: ch,
        overlap: overlap.start - keep..overlap.end - keep,
        mix_at,
        plan: mixer.last_plan(),
    })
}

/// Hands `b` to the mixer in the order the decode worker and the UI thread do.
fn hand_over(mixer: &mut Mixer, b: Side, transition: bool, skip: bool) {
    let bend = if transition {
        mixer.bend_job(1, &b.analysis, skip).map(|job| job.render())
    } else {
        None
    };
    mixer.set_analysis(1, b.analysis);
    if transition && mixer.mix().uses_fade() {
        mixer.start_prepared_transition(1, b.buf, 1.0, skip, bend);
    } else {
        mixer.play_decoded(1, b.buf);
    }
}

fn track(path: &Path) -> Track {
    Track {
        path: path.to_path_buf(),
        title: path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// Writes 16-bit PCM and returns how many samples had to be clipped.
pub fn write_wav(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
) -> anyhow::Result<usize> {
    let ch = u16::try_from(channels.max(1)).context("channel count")?;
    let data_len = u32::try_from(samples.len() * 2)
        .ok()
        .filter(|n| n.checked_add(36).is_some())
        .context("too long for a wav")?;
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut w = BufWriter::new(file);
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&ch.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * u32::from(ch) * 2).to_le_bytes())?;
    w.write_all(&(ch * 2).to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    let mut clipped = 0;
    for &s in samples {
        if !(-1.0..=1.0).contains(&s) {
            clipped += 1;
        }
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        w.write_all(&v.to_le_bytes())?;
    }
    w.flush()?;
    Ok(clipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Camelot;
    use crate::automix::MixStyle;

    const SR: u32 = 48_000;

    /// One millisecond clicks on every beat from `first` seconds on.
    fn clicks(secs: f32, bpm: f64, first: f64) -> AudioBuf {
        let frames = (secs * SR as f32) as usize;
        let beat = SR as f64 * 60.0 / bpm;
        let first = first * SR as f64;
        let samples = (0..frames)
            .flat_map(|f| {
                let into = (f as f64 - first).rem_euclid(beat);
                let v = if f as f64 >= first && into < 48.0 {
                    0.9
                } else {
                    0.0
                };
                [v, v]
            })
            .collect();
        AudioBuf {
            samples,
            sample_rate: SR,
            channels: 2,
        }
    }

    fn analysis(bpm: f32, first_beat: f32, mix_in: f32, mix_out: f32, secs: f32) -> TrackAnalysis {
        TrackAnalysis {
            version: 1,
            mtime: 0,
            size: 0,
            bpm: Some(bpm),
            bpm_confidence: 0.9,
            key_root: Some(0),
            key_minor: false,
            key_confidence: 0.9,
            camelot: Some(Camelot {
                number: 8,
                minor: false,
            }),
            energy: 0.2,
            mix_in,
            mix_out,
            first_beat,
            duration: secs,
            loudness: None,
            peak: 0.0,
            music_start: 0.0,
            music_end: secs,
            beat_this: false,
        }
    }

    fn side(name: &str, buf: AudioBuf, analysis: TrackAnalysis) -> Side {
        Side {
            path: PathBuf::from(name),
            buf,
            analysis,
        }
    }

    /// 128 BPM into 120 BPM, both on the beat from their first frame.
    fn pair() -> (Side, Side) {
        let a = side(
            "a.wav",
            clicks(40.0, 128.0, 0.0),
            analysis(128.0, 0.0, 0.0, 30.0, 40.0),
        );
        let b = side(
            "b.wav",
            clicks(40.0, 120.0, 0.0),
            analysis(120.0, 0.0, 2.0, 30.0, 40.0),
        );
        (a, b)
    }

    /// Frames where the left channel turns loud after at least 3 ms of quiet, so two clicks
    /// more than 3 ms apart count twice.
    fn onsets(mix: &Mixdown) -> Vec<usize> {
        let quiet = SR as usize * 3 / 1000;
        let mut found = Vec::new();
        let mut last_loud: Option<usize> = None;
        for (at, frame) in mix.samples.chunks(mix.channels).enumerate() {
            if frame[0].abs() > 0.3 {
                if last_loud.is_none_or(|l| at - l > quiet) {
                    found.push(at);
                }
                last_loud = Some(at);
            }
        }
        found
    }

    /// Every gap between beats lies within `slack` frames of the 128 to 120 BPM glide.
    fn assert_one_beat_train(found: &[usize], slack: usize) {
        let (beat_a, beat_b) = (22_500, 24_000);
        for pair in found.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap + slack >= beat_a && gap <= beat_b + slack,
                "gap {gap} at frame {} breaks the beat",
                pair[0]
            );
        }
    }

    #[test]
    fn automix_hands_over_on_one_beat_train() {
        let (a, b) = pair();
        let mix = render(a, b, &Options::default()).unwrap();
        let plan = mix.plan.expect("a planned mix");
        assert_eq!(plan.style, MixStyle::BassSwap);
        assert!((plan.speed - 120.0 / 128.0).abs() < 1e-6);
        assert!(mix.overlap.len() as f32 > 0.95 * plan.fade_secs * SR as f32);

        // The overlap starts on one of the outgoing track's bars.
        let bars = mix.mix_at / (4.0 * 60.0 / 128.0);
        assert!(
            (bars - bars.round()).abs() < 0.01,
            "overlap starts {bars} bars in"
        );

        let found = onsets(&mix);
        let inside = found.iter().filter(|f| mix.overlap.contains(f)).count();
        assert!(inside >= 14, "only {inside} beats in the overlap");
        // WSOLA moves the outgoing clicks by up to half a grain; a second beat train would
        // show up as gaps far shorter than a beat.
        assert_one_beat_train(&found, 960);
        let after: Vec<_> = found.iter().filter(|&&f| f > mix.overlap.end).collect();
        assert!(after.len() >= 10, "{} beats after the overlap", after.len());
        for pair in after.windows(2) {
            assert_eq!(
                pair[1] - pair[0],
                24_000,
                "the incoming track is not 120 BPM"
            );
        }
    }

    /// A stereo `hz` sine at `amp`, mixing out 10 s before its end.
    fn tone(secs: f32, hz: f32, amp: f32, bpm: f32) -> Side {
        let samples = (0..(secs * SR as f32) as usize)
            .flat_map(|f| {
                let v = amp * (std::f32::consts::TAU * hz * f as f32 / SR as f32).sin();
                [v, v]
            })
            .collect();
        side(
            "tone.wav",
            AudioBuf {
                samples,
                sample_rate: SR,
                channels: 2,
            },
            analysis(bpm, 0.0, 0.0, secs - 10.0, secs),
        )
    }

    /// A 1 kHz sine at `amp` with the loudness and peak a stereo sine reads as.
    fn sine(secs: f32, amp: f32, bpm: f32) -> Side {
        let mut side = tone(secs, 1000.0, amp, bpm);
        side.analysis.loudness = Some(20.0 * amp.log10());
        side.analysis.peak = amp;
        side
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    #[test]
    fn automix_plays_both_tracks_at_the_same_loudness() {
        // About -2 and -26 LUFS: one has to come down and the other up.
        let (a, b) = (sine(40.0, 0.8, 120.0), sine(40.0, 0.05, 120.0));
        let mix = render(a, b, &Options::default()).unwrap();
        let window = 2 * SR as usize * mix.channels;
        let start = mix.overlap.start * mix.channels;
        let end = mix.overlap.end * mix.channels;
        let before = rms(&mix.samples[start - window..start]);
        let after = rms(&mix.samples[end..end + window]);
        let db = |x: f32| 20.0 * x.log10();
        // -14 LUFS is a 0.2 sine, 0.141 RMS.
        let target = 0.2 / std::f32::consts::SQRT_2;
        assert!(
            (db(before) - db(target)).abs() < 0.2 && (db(after) - db(target)).abs() < 0.2,
            "{:.1} dB before the mix and {:.1} dB after it, not {:.1}",
            db(before),
            db(after),
            db(target)
        );
    }

    #[test]
    fn bass_swap_never_plays_two_bass_lines() {
        // A 50 Hz bass line on one side and silence on the other, rendered with `t` running
        // over the overlap: returns the bass left over where `muted` says its low band is off.
        let leak = |outgoing: bool, muted: fn(f32) -> bool| {
            let (bass, silence) = (tone(40.0, 50.0, 0.5, 120.0), tone(40.0, 50.0, 0.0, 120.0));
            let (a, b) = if outgoing {
                (bass, silence)
            } else {
                (silence, bass)
            };
            let mix = render(a, b, &Options::default()).unwrap();
            assert_eq!(mix.plan.expect("a planned mix").style, MixStyle::BassSwap);
            let len = mix.overlap.len() as f32;
            let left: Vec<f32> = mix.samples[mix.overlap.start * 2..mix.overlap.end * 2]
                .chunks(2)
                .enumerate()
                .filter(|(at, _)| muted(*at as f32 / len))
                .map(|(_, f)| f[0])
                .collect();
            20.0 * (rms(&left) / (0.5 / std::f32::consts::SQRT_2)).log10()
        };
        // The incoming bass waits for the swap and the outgoing bass stops at it.
        let incoming = leak(false, |t| t < 0.42);
        let outgoing = leak(true, |t| t > 0.6);
        assert!(
            incoming < -30.0 && outgoing < -30.0,
            "bass leaks through the high band: incoming {incoming:.1} dB, outgoing {outgoing:.1} dB"
        );
    }

    #[test]
    fn skip_renders_from_the_press() {
        let (a, b) = pair();
        let opts = Options {
            skip_at: Some(10.0),
            ..Options::default()
        };
        let mix = render(a, b, &opts).unwrap();
        assert!(!mix.overlap.is_empty());
        assert!(
            (10.0..11.5).contains(&mix.mix_at),
            "skip mixed in at {} s",
            mix.mix_at
        );
        assert_one_beat_train(&onsets(&mix), 960);
    }

    #[test]
    fn cut_plays_the_outgoing_track_out_and_the_next_from_its_start() {
        let (mut a, mut b) = pair();
        a.buf = clicks(20.0, 128.0, 0.0);
        a.analysis.duration = 20.0;
        b.buf = clicks(20.0, 120.0, 0.25);
        let (a_buf, b_buf) = (a.buf.clone(), b.buf.clone());
        let opts = Options {
            mode: MixMode::Cut,
            before_secs: 30.0,
            after_secs: 2.0,
            ..Options::default()
        };
        let mix = render(a, b, &opts).unwrap();
        assert!(mix.plan.is_none());
        assert!(mix.overlap.is_empty());
        assert!((mix.mix_at - 20.0).abs() < 1e-3, "cut at {}", mix.mix_at);
        let cut = mix.overlap.start * 2;
        assert!(cut >= a_buf.samples.len() && cut - a_buf.samples.len() < BLOCK * 2);
        let close = |x: &[f32], y: &[f32]| x.iter().zip(y).all(|(p, q)| (p - q).abs() < 1e-4);
        assert!(close(&mix.samples[..a_buf.samples.len()], &a_buf.samples));
        assert!(mix.samples[a_buf.samples.len()..cut]
            .iter()
            .all(|s| s.abs() < 1e-4));
        assert!(close(&mix.samples[cut..], &b_buf.samples));
        assert_eq!(mix.frames() - mix.overlap.end, 2 * SR as usize);
    }

    #[test]
    fn wav_round_trips_and_counts_clipping() {
        let path = std::env::temp_dir().join(format!("zradio-mixdown-{}.wav", std::process::id()));
        let mut samples: Vec<f32> = (0..2_000).map(|i| (i as f32 * 0.01).sin() * 0.8).collect();
        samples.extend([1.5, -1.2]);
        let clipped = write_wav(&path, &samples, 44_100, 2).unwrap();
        let back = crate::decode::decode_file(&path, 44_100, 2);
        let _ = std::fs::remove_file(&path);
        let back = back.unwrap();
        assert_eq!(clipped, 2);
        assert_eq!(back.samples.len(), samples.len());
        for (x, y) in samples.iter().zip(&back.samples) {
            assert!(
                (x.clamp(-1.0, 1.0) - y).abs() < 1e-3,
                "{x} came back as {y}"
            );
        }
    }
}
