//! The vocals of a stretch of audio, separated with htdemucs (Rouard, Massa and Défossez,
//! ICASSP 2023; Meta, MIT), so AutoMix can keep two singers from overlapping in a mix.

use crate::beat_this::resample;
use std::path::PathBuf;

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
#[cfg(feature = "stems")]
pub fn download_model(
    progress: Option<std::sync::mpsc::Sender<String>>,
) -> anyhow::Result<PathBuf> {
    let path = model_path();
    if !model_ready() {
        crate::beat_this::fetch_verified(MODEL_URL, MODEL_SHA256, &path, progress)?;
    }
    Ok(path)
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

    #[test]
    fn the_model_is_kept_beside_beat_this() {
        assert_eq!(
            model_path().parent(),
            crate::beat_this::model_path().parent()
        );
        assert_eq!(model_path().file_name().unwrap(), "htdemucs.onnx");
    }
}
