//! The vocals of a stretch of audio, separated with htdemucs (Rouard, Massa and Défossez,
//! ICASSP 2023; Meta, MIT), so AutoMix can keep two singers from overlapping in a mix.

use crate::beat_this::resample;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

/// Rate the model runs at.
pub const MODEL_RATE: u32 = 44_100;
/// Frames the model takes at once: 7.8 s, the length it was exported for.
pub const SEGMENT: usize = 343_980;
/// The model's stems come out as drums, bass, other, vocals.
#[cfg(feature = "stems")]
const STEMS: usize = 4;
#[cfg(feature = "stems")]
const VOCALS: usize = 3;

/// Planar stereo: left, then right.
pub type Planar = [Vec<f32>; 2];

/// `mix` separated `segment` frames at a time by `run`, which takes and gives that many
/// frames of planar stereo. Neighbouring segments overlap by a quarter and cross-fade over
/// it, as the reference inference does, but the two ends of the audio are not faded: there
/// is nothing to fade them into. The last segment is padded with silence.
pub fn separate_chunked<F>(mix: &Planar, segment: usize, mut run: F) -> anyhow::Result<Planar>
where
    F: FnMut(&Planar) -> anyhow::Result<Planar>,
{
    let total = mix[0].len().min(mix[1].len());
    let overlap = segment / 4;
    let stride = segment - overlap;
    let mut out = [vec![0.0f32; total], vec![0.0f32; total]];
    let mut weight = vec![0.0f32; total];
    let mut start = 0;
    while start < total {
        let end = (start + segment).min(total);
        let (first, last) = (start == 0, end == total);
        let seg = [0, 1].map(|c| {
            let mut s = mix[c][start..end].to_vec();
            s.resize(segment, 0.0);
            s
        });
        let got = run(&seg)?;
        anyhow::ensure!(
            got.iter().all(|s| s.len() >= end - start),
            "a segment came back short"
        );
        for i in 0..end - start {
            // One step past the ends of the fade, so neither side is ever weighed at 0.
            let fade = |k: usize| (k + 1) as f32 / (overlap + 1) as f32;
            let mut w = 1.0f32;
            if !first && i < overlap {
                w = fade(i);
            }
            if !last && i >= segment - overlap {
                w = fade(segment - 1 - i);
            }
            for c in 0..2 {
                out[c][start + i] += got[c][i] * w;
            }
            weight[start + i] += w;
        }
        if last {
            break;
        }
        start += stride;
    }
    for c in &mut out {
        for (s, w) in c.iter_mut().zip(&weight) {
            *s /= w.max(1e-8);
        }
    }
    Ok(out)
}

/// The vocals of `samples`, interleaved with `channels` at `rate`, as interleaved stereo at
/// `rate`, frame for frame. `run` separates one `SEGMENT` of planar stereo at `MODEL_RATE`.
pub fn vocals_with<F>(
    samples: &[f32],
    channels: usize,
    rate: u32,
    run: F,
) -> anyhow::Result<Vec<f32>>
where
    F: FnMut(&Planar) -> anyhow::Result<Planar>,
{
    let frames = samples.len() / channels.max(1);
    if frames == 0 || rate == 0 {
        return Ok(Vec::new());
    }
    // A mono deck plays the same samples on both sides.
    let side = |c: usize| -> Vec<f32> {
        let x: Vec<f32> = samples
            .chunks_exact(channels)
            .map(|f| f[c.min(channels - 1)])
            .collect();
        if rate == MODEL_RATE {
            x
        } else {
            resample(&x, rate, MODEL_RATE)
        }
    };
    let [l, r] = separate_chunked(&[side(0), side(1)], SEGMENT, run)?;
    let back = |x: Vec<f32>| {
        let mut y = if rate == MODEL_RATE {
            x
        } else {
            resample(&x, MODEL_RATE, rate)
        };
        y.resize(frames, 0.0);
        y
    };
    Ok(back(l)
        .into_iter()
        .zip(back(r))
        .flat_map(|(a, b)| [a, b])
        .collect())
}

/// Blocks a voice's loudness is measured over.
const BLOCK_SECS: f32 = 0.05;
/// Quietest a played voice can be and still be heard over a mix leveled to -14 LUFS.
const VOICE_FLOOR_DB: f32 = -40.0;
/// A voice this far under the other is masked by it, so the two do not clash.
const MASKED_DB: f32 = 12.0;
/// Two voices heard together for less than this are left to overlap.
const CLASH_MIN_SECS: f32 = 1.0;
/// Each voice fades over this, the outgoing one out to the handoff, the incoming in after.
const RAMP_SECS: f32 = 0.3;
/// Handoffs that remove this much more voice than the least are as good as it.
const COST_SLACK: f64 = 0.01;

/// Where in an overlap the outgoing voice gives way to the incoming one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handoff {
    /// Overlap frame the outgoing voice is gone by and the incoming one starts at.
    pub at: usize,
    /// Frames each voice fades over.
    pub ramp: usize,
}

impl Handoff {
    /// How much of the outgoing and the incoming voice is kept at overlap frame `f`.
    pub fn keep(&self, f: usize) -> (f32, f32) {
        let ramp = self.ramp.max(1) as f64;
        let smooth = |x: f64| {
            let x = x.clamp(0.0, 1.0) as f32;
            x * x * (3.0 - 2.0 * x)
        };
        let to = self.at as f64 - f as f64;
        (smooth(to / ramp), smooth(-to / ramp))
    }
}

fn block_frames(rate: u32) -> usize {
    ((BLOCK_SECS * rate as f32).round() as usize).max(1)
}

/// Mean square of interleaved stereo `vocals`, block by block, each frame weighed by
/// `gain(frame)` as the deck plays it.
pub fn played_power(vocals: &[f32], rate: u32, gain: impl Fn(usize) -> f32) -> Vec<f64> {
    let block = block_frames(rate);
    vocals
        .chunks(2 * block)
        .enumerate()
        .map(|(b, chunk)| {
            let frames = chunk.as_chunks::<2>().0;
            let sum: f64 = frames
                .iter()
                .enumerate()
                .map(|(i, &[l, r])| {
                    let g = f64::from(gain(b * block + i));
                    g * g * (f64::from(l).powi(2) + f64::from(r).powi(2))
                })
                .sum();
            sum / (2 * frames.len().max(1)) as f64
        })
        .collect()
}

/// Where to hand over when the two voices, as played (`out` and `inn`, from
/// `played_power`), would otherwise be heard together for `CLASH_MIN_SECS` or more; None
/// when they would not. Of `candidates` (overlap frames, beat lines say) that leave room
/// for both fades in the `fade` frames of the overlap, it takes the one that removes the
/// least voice, counted as if the handoff were a cut, and of those as good, the one
/// nearest the middle.
pub fn plan_handoff(
    out: &[f64],
    inn: &[f64],
    rate: u32,
    fade: usize,
    candidates: &[usize],
) -> Option<Handoff> {
    let floor = 10f64.powf(f64::from(VOICE_FLOOR_DB) / 10.0);
    let masked = 10f64.powf(f64::from(MASKED_DB) / 10.0);
    let together = out
        .iter()
        .zip(inn)
        .filter(|&(&o, &i)| o.min(i) >= floor && o.max(i) <= masked * o.min(i))
        .count();
    if together < (CLASH_MIN_SECS / BLOCK_SECS).round() as usize {
        return None;
    }
    let ramp = (RAMP_SECS * rate as f32).round() as usize;
    let block = block_frames(rate);
    let sums = |p: &[f64]| {
        std::iter::once(0.0)
            .chain(p.iter().scan(0.0, |s, &x| {
                *s += x;
                Some(*s)
            }))
            .collect::<Vec<f64>>()
    };
    let (so, si) = (sums(out), sums(inn));
    // A cut at `at` would remove the outgoing voice from its block on and the incoming
    // one before it.
    let cost = |at: usize| {
        let b = (at + block / 2) / block;
        so[out.len()] - so[b.min(out.len())] + si[b.min(inn.len())]
    };
    let fits: Vec<(usize, f64)> = candidates
        .iter()
        .filter(|&&c| c >= ramp && c + ramp <= fade)
        .map(|&c| (c, cost(c)))
        .collect();
    let least = fits.iter().map(|&(_, c)| c).reduce(f64::min)?;
    let at = fits
        .iter()
        .filter(|&&(_, c)| c <= least * (1.0 + COST_SLACK))
        .map(|&(at, _)| at)
        .min_by_key(|&at| (at.abs_diff(fade / 2), at))?;
    Some(Handoff { at, ramp })
}

#[cfg(feature = "stems")]
pub struct Model {
    plan: std::sync::Arc<tract_onnx::prelude::TypedRunnableModel>,
}

#[cfg(feature = "stems")]
impl Model {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        use tract_onnx::prelude::*;
        let plan = tract_onnx::onnx()
            .model_for_path(path)?
            .with_input_fact(0, f32::fact([1, 2, SEGMENT]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Self { plan })
    }

    /// The vocals of one `SEGMENT` of planar stereo at `MODEL_RATE`.
    fn segment_vocals(&self, mix: &Planar) -> anyhow::Result<Planar> {
        use tract_onnx::prelude::*;
        let input: Vec<f32> = mix.iter().flatten().copied().collect();
        let input = Tensor::from_shape(&[1, 2, SEGMENT], &input)?;
        let out = self.plan.run(tvec!(input.into()))?;
        let stems = out
            .first()
            .ok_or_else(|| anyhow::anyhow!("model gave no output"))?
            .to_plain_array_view::<f32>()?;
        anyhow::ensure!(
            stems.shape() == [1, STEMS, 2, SEGMENT],
            "model gave stems shaped {:?}",
            stems.shape()
        );
        let flat = stems
            .as_slice()
            .ok_or_else(|| anyhow::anyhow!("model gave non-contiguous stems"))?;
        let side = |c: usize| {
            let at = (VOCALS * 2 + c) * SEGMENT;
            flat[at..at + SEGMENT].to_vec()
        };
        Ok([side(0), side(1)])
    }

    /// The vocals of `samples`, interleaved with `channels` at `rate`, as interleaved
    /// stereo at `rate`.
    pub fn vocals(&self, samples: &[f32], channels: usize, rate: u32) -> anyhow::Result<Vec<f32>> {
        vocals_with(samples, channels, rate, |mix| self.segment_vocals(mix))
    }
}

/// htdemucs as exported to ONNX by StemSplitio (MIT), with its STFT moved to ONNX `DFT` by
/// itamiArika (MIT): the "transparent" build, fp16 weights computed in fp32 and no
/// quantization. Pinned to a revision and checked against its hash.
pub const MODEL_URL: &str = "https://huggingface.co/itamiArika/htdemucs-int8-memory/resolve/\
86db1d28b1bdec60a082f4e745c490298847307c/htdemucs-dft-fp16-transparent-portable.onnx";
pub const MODEL_BYTES: u64 = 129_977_189;
pub const MODEL_SHA256: &str = "d0a9cc802ac3f0b9f20ad2f04d56aa8013b9fe3160831c2a3f3ebc4bea254339";

/// Where the model is kept, beside the Beat This! one.
pub fn model_path() -> PathBuf {
    crate::beat_this::models_dir_file("htdemucs.onnx")
}

/// Whether the model has been fetched; its hash was checked when it was.
pub fn model_ready() -> bool {
    std::fs::metadata(model_path()).is_ok_and(|m| m.len() == MODEL_BYTES)
}

/// Fetches the model, reporting progress as "42%", unless it is already there.
pub fn download_model(progress: Option<Sender<String>>) -> anyhow::Result<PathBuf> {
    #[cfg(feature = "stems")]
    {
        let path = model_path();
        if !model_ready() {
            crate::beat_this::fetch_verified(MODEL_URL, MODEL_SHA256, &path, progress)?;
        }
        Ok(path)
    }
    #[cfg(not(feature = "stems"))]
    {
        let _ = progress;
        anyhow::bail!("this build has no voice separation")
    }
}

/// Whether this build can separate voices.
pub const AVAILABLE: bool = cfg!(feature = "stems");

/// One thread running the jobs sent to it in turn; it ends when the worker is dropped.
/// Each job is for the mix armed when it was sent, so of those waiting only the newest is
/// run.
pub struct Worker<J, R> {
    jobs: Sender<J>,
    done: Receiver<anyhow::Result<R>>,
}

impl<J: Send + 'static, R: Send + 'static> Worker<J, R> {
    pub fn spawn(mut run: impl FnMut(J) -> anyhow::Result<R> + Send + 'static) -> Self {
        let (jobs, rx) = mpsc::channel::<J>();
        let (tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(mut job) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    job = newer;
                }
                // A model that panics on one mix must not take the worker down with it.
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(job)));
                let ran = ran.unwrap_or_else(|_| Err(anyhow::anyhow!("separation crashed")));
                if tx.send(ran).is_err() {
                    break;
                }
            }
        });
        Self { jobs, done }
    }

    pub fn send(&self, job: J) -> bool {
        self.jobs.send(job).is_ok()
    }

    pub fn try_recv(&self) -> Option<anyhow::Result<R>> {
        self.done.try_recv().ok()
    }
}

pub type Separate = dyn FnMut(crate::engine::VocalJob) -> anyhow::Result<Option<crate::engine::VocalOverlay>>
    + Send;

/// What the app separates the voices of a mix with, when this build can.
pub fn separator() -> Option<Box<Separate>> {
    #[cfg(feature = "stems")]
    {
        Some(Box::new(|job: crate::engine::VocalJob| {
            // Loaded for each mix, so the memory it runs in is given back between them.
            let model = Model::load(&model_path())?;
            job.run(|x, rate| model.vocals(x, 2, rate))
        }))
    }
    #[cfg(not(feature = "stems"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(total: usize) -> Planar {
        [
            (0..total).map(|i| i as f32).collect(),
            (0..total).map(|i| -(i as f32) - 0.5).collect(),
        ]
    }

    #[test]
    fn segments_cover_the_mix_end_to_end() {
        for total in [1, 7, 39, 40, 41, 69, 70, 71, 100, 129, 130, 131, 400] {
            let mix = ramp(total);
            let mut calls = 0;
            let out = separate_chunked(&mix, 40, |seg| {
                calls += 1;
                assert_eq!(seg[0].len(), 40);
                assert_eq!(seg[1].len(), 40);
                Ok(seg.clone())
            })
            .unwrap();
            assert!(calls > 0, "{total}: never ran");
            for c in 0..2 {
                assert_eq!(out[c].len(), total);
                for (i, (y, x)) in out[c].iter().zip(&mix[c]).enumerate() {
                    assert!(
                        (y - x).abs() < 1e-3,
                        "{total} ch {c} frame {i}: {y} for {x}"
                    );
                }
            }
        }
    }

    #[test]
    fn each_segment_fades_into_the_next() {
        // Segments of 40 overlap by 10, so they start 30 apart: 0, 30, 60.
        let mut k = 0.0;
        let out = separate_chunked(&ramp(100), 40, |_| {
            k += 1.0;
            Ok([vec![k; 40], vec![-k; 40]])
        })
        .unwrap();
        assert_eq!(k, 3.0);
        let want = |i: usize| -> f32 {
            match i {
                0..30 => 1.0,
                30..40 => 1.0 + (i - 30) as f32 / 10.0,
                40..60 => 2.0,
                60..70 => 2.0 + (i - 60) as f32 / 10.0,
                _ => 3.0,
            }
        };
        for (i, (l, r)) in out[0].iter().zip(&out[1]).enumerate() {
            // The fade is in its own segment's frames, so it never quite reaches 0 or 1.
            assert!((l - want(i)).abs() < 0.1, "frame {i}: {l}");
            assert!((r + want(i)).abs() < 0.1, "frame {i}: {r}");
        }
        assert_eq!(out[0][..30], [1.0; 30]);
        assert_eq!(out[0][70..], [3.0; 30]);
    }

    #[test]
    fn the_last_segment_is_padded_with_silence() {
        let mut calls = 0;
        let out = separate_chunked(&ramp(25), 40, |seg| {
            calls += 1;
            assert_eq!(seg[0][..25], ramp(25)[0][..]);
            assert!(seg[0][25..].iter().chain(&seg[1][25..]).all(|&s| s == 0.0));
            Ok(seg.clone())
        })
        .unwrap();
        assert_eq!((calls, out[0].len()), (1, 25));
    }

    fn tones(rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        let tau = std::f32::consts::TAU;
        (0..n)
            .flat_map(|i| {
                let t = i as f32 / rate as f32;
                [
                    0.5 * (tau * 440.0 * t).sin(),
                    0.3 * (tau * 1000.0 * t).sin(),
                ]
            })
            .collect()
    }

    fn snr_db(want: &[f32], got: &[f32]) -> f32 {
        let s: f64 = want.iter().map(|&x| f64::from(x).powi(2)).sum();
        let e: f64 = want
            .iter()
            .zip(got)
            .map(|(&x, &y)| f64::from(x - y).powi(2))
            .sum();
        (10.0 * (s / e.max(1e-30)).log10()) as f32
    }

    #[test]
    fn vocals_come_back_frame_for_frame_at_the_decks_rate() {
        for rate in [44_100, 48_000] {
            let mix = tones(rate, 1.0);
            let out = vocals_with(&mix, 2, rate, |seg| Ok(seg.clone())).unwrap();
            assert_eq!(out.len(), mix.len(), "{rate}");
            // The resampler's taps run out within a few hundred frames of either end.
            let inner = 2 * 400..mix.len() - 2 * 400;
            let snr = snr_db(&mix[inner.clone()], &out[inner]);
            assert!(snr > 40.0, "{rate}: {snr:.1} dB");
        }
    }

    #[test]
    fn a_mono_deck_is_separated_as_the_same_on_both_sides() {
        let mono: Vec<f32> = tones(48_000, 0.5).into_iter().step_by(2).collect();
        let mut calls = 0;
        let out = vocals_with(&mono, 1, 48_000, |seg| {
            calls += 1;
            assert_eq!(seg[0], seg[1]);
            Ok(seg.clone())
        })
        .unwrap();
        assert_eq!((calls, out.len()), (1, mono.len() * 2));
        assert!(out.as_chunks::<2>().0.iter().all(|[l, r]| l == r));
    }

    /// Kick, hi-hat and bass, no voice, at 48 kHz stereo.
    #[cfg(feature = "stems")]
    fn drums_and_bass(secs: f32) -> Vec<f32> {
        let rate = 48_000.0;
        let tau = std::f32::consts::TAU;
        let mut noise = 0x2545_f491u32;
        (0..(rate * secs) as usize)
            .flat_map(|i| {
                let t = i as f32 / rate;
                let (kt, ht) = (t % 0.5, (t + 0.25) % 0.5);
                let kick =
                    (tau * (50.0 + 90.0 * (-kt * 40.0).exp()) * kt).sin() * (-kt * 12.0).exp();
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                let hat = white * (-ht * 60.0).exp() * 0.15;
                let root = [41.2, 41.2, 55.0, 49.0][(t / 2.0) as usize % 4];
                let bass = 0.3 * (tau * root * t).sin();
                let s = 0.6 * kick + hat + bass;
                [s, s * 0.9]
            })
            .collect()
    }

    #[cfg(feature = "stems")]
    #[test]
    #[ignore = "needs the htdemucs model: ZRADIO_STEMS_MODEL=path"]
    fn drums_and_bass_have_next_to_no_vocals() {
        let path = std::env::var("ZRADIO_STEMS_MODEL").expect("ZRADIO_STEMS_MODEL");
        let model = Model::load(std::path::Path::new(&path)).expect("load");
        let mix = drums_and_bass(8.0);
        let vocals = model.vocals(&mix, 2, 48_000).expect("vocals");
        assert_eq!(vocals.len(), mix.len());
        let energy = |x: &[f32]| x.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        let db = 10.0 * (energy(&vocals) / energy(&mix)).log10();
        assert!(db < -25.0, "vocals at {db:.1} dB of the mix");
    }

    // At 48 kHz a block is 2400 frames, and a 10 s overlap is 200 of them.
    const FADE: usize = 480_000;
    const RAMP: usize = 14_400;

    /// A voice at `db` over `blocks` of the overlap, silence elsewhere.
    fn voice(db: f32, blocks: std::ops::Range<usize>) -> Vec<f64> {
        let p = 10f64.powf(f64::from(db) / 10.0);
        (0..200)
            .map(|b| if blocks.contains(&b) { p } else { 0.0 })
            .collect()
    }

    /// Beat lines at 120 bpm, every 10 blocks.
    fn beats() -> Vec<usize> {
        (0..=20).map(|k| k * 24_000).collect()
    }

    fn handoff(out: &[f64], inn: &[f64], candidates: &[usize]) -> Option<usize> {
        plan_handoff(out, inn, 48_000, FADE, candidates).map(|h| {
            assert_eq!(h.ramp, RAMP);
            h.at
        })
    }

    #[test]
    fn two_singers_hand_over_nearest_the_middle_of_their_overlap() {
        let (out, inn) = (voice(-20.0, 0..120), voice(-20.0, 80..200));
        assert_eq!(handoff(&out, &inn, &beats()), Some(240_000));
    }

    #[test]
    fn the_handoff_falls_in_a_gap_between_lines() {
        // The outgoing singer rests from block 60 to 100; the incoming one starts at 70.
        let mut out = voice(-20.0, 0..60);
        for (o, v) in out.iter_mut().zip(voice(-20.0, 100..140)) {
            *o += v;
        }
        let inn = voice(-20.0, 70..200);
        assert_eq!(handoff(&out, &inn, &beats()), Some(168_000));
    }

    #[test]
    fn voices_that_do_not_clash_are_left_alone() {
        let quiet = voice(-90.0, 0..0);
        let lone = voice(-20.0, 0..200);
        assert_eq!(handoff(&lone, &quiet, &beats()), None);
        assert_eq!(handoff(&quiet, &lone, &beats()), None);
        // Taking turns.
        assert_eq!(
            handoff(&voice(-20.0, 0..90), &voice(-20.0, 110..200), &beats()),
            None
        );
        // Together for 0.9 s, then for 1.1 s.
        let inn = voice(-20.0, 90..200);
        assert_eq!(handoff(&voice(-20.0, 0..108), &inn, &beats()), None);
        assert!(handoff(&voice(-20.0, 0..112), &inn, &beats()).is_some());
        // One masked by the other, or both too quiet to hear under the mix.
        assert_eq!(
            handoff(&voice(-20.0, 0..120), &voice(-35.0, 80..200), &beats()),
            None
        );
        assert_eq!(
            handoff(&voice(-45.0, 0..120), &voice(-45.0, 80..200), &beats()),
            None
        );
    }

    #[test]
    fn the_handoff_leaves_room_for_both_fades() {
        let (out, inn) = (voice(-20.0, 0..120), voice(-20.0, 80..200));
        assert_eq!(handoff(&out, &inn, &[0, FADE]), None);
        assert_eq!(handoff(&out, &inn, &[RAMP - 1, FADE - RAMP + 1]), None);
        assert_eq!(handoff(&out, &inn, &[]), None);
        assert_eq!(handoff(&out, &inn, &[0, RAMP, FADE]), Some(RAMP));
        assert_eq!(handoff(&out, &inn, &[FADE - RAMP]), Some(FADE - RAMP));
    }

    #[test]
    fn the_two_voices_are_never_kept_together() {
        let h = Handoff {
            at: 240_000,
            ramp: RAMP,
        };
        assert_eq!(h.keep(0), (1.0, 0.0));
        assert_eq!(h.keep(240_000 - RAMP), (1.0, 0.0));
        assert_eq!(h.keep(240_000), (0.0, 0.0));
        assert_eq!(h.keep(240_000 + RAMP), (0.0, 1.0));
        assert_eq!(h.keep(FADE), (0.0, 1.0));
        let (o, _) = h.keep(240_000 - RAMP / 2);
        let (_, i) = h.keep(240_000 + RAMP / 2);
        assert!((o - 0.5).abs() < 1e-3 && (i - 0.5).abs() < 1e-3, "{o} {i}");
        let mut last = (1.0, 0.0);
        for f in (0..FADE).step_by(100) {
            let (o, i) = h.keep(f);
            assert!(o * i == 0.0, "{f}: both kept");
            assert!(o <= last.0 && i >= last.1, "{f}: not monotonic");
            last = (o, i);
        }
    }

    #[test]
    fn played_power_weighs_each_frame_as_the_deck_plays_it() {
        let flat = vec![0.5f32; 2 * (3 * 2400 + 1200)];
        let p = played_power(&flat, 48_000, |_| 1.0);
        assert_eq!(p.len(), 4);
        assert!(p.iter().all(|&x| (x - 0.25).abs() < 1e-9), "{p:?}");
        let rising = played_power(&flat, 48_000, |f| (f as f32 / 4800.0).min(1.0));
        let want = [0.25 / 12.0, 0.25 * 7.0 / 12.0, 0.25, 0.25];
        for (got, want) in rising.iter().zip(want) {
            assert!((got - want).abs() < want * 1e-3, "{rising:?}");
        }
    }

    fn wait<R: Send + 'static>(worker: &Worker<u32, R>) -> anyhow::Result<R> {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < until {
            if let Some(r) = worker.try_recv() {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the worker did not answer");
    }

    #[test]
    fn the_worker_runs_only_the_newest_of_the_jobs_waiting() {
        let (started_tx, started) = mpsc::channel();
        let (gate, gate_rx) = mpsc::channel::<()>();
        let worker = Worker::spawn(move |j: u32| {
            started_tx.send(j).unwrap();
            gate_rx.recv().unwrap();
            Ok(j * 10)
        });
        assert!(worker.send(1));
        let first = started.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(first.unwrap(), 1);
        assert!(worker.send(2) && worker.send(3));
        gate.send(()).unwrap();
        assert_eq!(wait(&worker).unwrap(), 10);
        gate.send(()).unwrap();
        assert_eq!(wait(&worker).unwrap(), 30);
        assert_eq!(
            started.try_iter().collect::<Vec<_>>(),
            [3],
            "ran a stale job"
        );
    }

    #[test]
    fn a_job_that_panics_does_not_take_the_worker_down() {
        let worker = Worker::spawn(|j: u32| {
            assert!(j != 0, "no voice in job 0");
            Ok(j)
        });
        assert!(worker.send(0));
        assert!(wait(&worker).is_err());
        assert!(worker.send(7));
        assert_eq!(wait(&worker).unwrap(), 7);
    }

    #[test]
    fn the_model_is_kept_beside_beat_this() {
        assert_eq!(
            model_path().parent(),
            crate::beat_this::model_path().parent()
        );
        assert_eq!(model_path().file_name().unwrap(), "htdemucs.onnx");
    }
}
