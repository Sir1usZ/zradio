//! Renders AutoMix transitions between audio files to WAV, through the player's mixer.
//!
//!     cargo run --release --example automix_render -- [options] a.flac b.mp3 [c.ogg ...]
//!
//! Each neighbouring pair becomes NN.wav in the output directory: `before` seconds of the
//! outgoing track, the overlap, then `after` seconds of the incoming one.
//!
//! `--beat-this beat_this.onnx` moves every track onto the grid Beat This! hears in it first,
//! as the player does with Beat This! on.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use zradio::analysis::{analyze_buffer, TrackAnalysis};
use zradio::beat_this::Grid;
use zradio::decode::{decode_file, AudioBuf};
use zradio::mixdown::{render, write_wav, Options, Side};
use zradio::MixMode;

const USAGE: &str = "usage: automix_render [--mode auto|fade|cut] [--skip SECS] \
[--before SECS] [--after SECS] [--rate HZ] [-o DIR] [--analyze] [--beat-this MODEL] \
FILE FILE [FILE ...]";

/// Refits a track's analysis on Beat This!'s grid, giving the grid too.
type Regrid = dyn Fn(&AudioBuf, TrackAnalysis) -> anyhow::Result<(TrackAnalysis, Option<Grid>)>;

struct Args {
    opts: Options,
    rate: u32,
    out: PathBuf,
    analyze_only: bool,
    model: Option<PathBuf>,
    files: Vec<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        opts: Options::default(),
        rate: 48_000,
        out: PathBuf::from("automix-out"),
        analyze_only: false,
        model: None,
        files: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--mode" => {
                args.opts.mode = match value()?.as_str() {
                    "auto" => MixMode::AutoMix,
                    "fade" => MixMode::Crossfade,
                    "cut" => MixMode::Cut,
                    other => bail!("unknown mode {other}"),
                }
            }
            "--skip" => args.opts.skip_at = Some(value()?.parse().context("--skip")?),
            "--before" => args.opts.before_secs = value()?.parse().context("--before")?,
            "--after" => args.opts.after_secs = value()?.parse().context("--after")?,
            "--rate" => args.rate = value()?.parse().context("--rate")?,
            "-o" | "--out" => args.out = PathBuf::from(value()?),
            "--analyze" => args.analyze_only = true,
            "--beat-this" => args.model = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => bail!("unknown option {arg}\n{USAGE}"),
            _ => args.files.push(PathBuf::from(arg)),
        }
    }
    let need = if args.analyze_only { 1 } else { 2 };
    if args.files.len() < need {
        bail!("{USAGE}");
    }
    Ok(args)
}

#[cfg(feature = "beat-this")]
fn regrid(model: &Path) -> anyhow::Result<Box<Regrid>> {
    use zradio::beat_this::{to_model_rate, Model};
    let model = Model::load(model).with_context(|| format!("load {}", model.display()))?;
    Ok(Box::new(move |buf, analysis| {
        let mono = to_model_rate(&buf.samples, buf.channels, buf.sample_rate);
        let grid = model.beats(&mono)?.grid();
        Ok((analysis.with_grid(grid), grid))
    }))
}

#[cfg(not(feature = "beat-this"))]
fn regrid(_: &Path) -> anyhow::Result<Box<Regrid>> {
    bail!("--beat-this needs the beat-this feature")
}

fn load(path: &Path, rate: u32, regrid: Option<&Regrid>) -> anyhow::Result<Side> {
    let buf = decode_file(path, rate, 2)?;
    let mut analysis = analyze_buffer(&buf);
    println!("{}", describe(path, &analysis));
    if let Some(regrid) = regrid {
        let start = std::time::Instant::now();
        let grid;
        (analysis, grid) = regrid(&buf, analysis)?;
        let secs = start.elapsed().as_secs_f32();
        match grid {
            Some(g) => println!(
                "  Beat This! in {secs:.1}s: bpm {:.2}  first beat {:.3}s ({})  \
                 mix in {:.2}s  mix out {:.2}s",
                g.bpm,
                g.first,
                if g.bar_known {
                    "on the bar"
                } else {
                    "bar unknown"
                },
                analysis.mix_in,
                analysis.mix_out
            ),
            None => println!("  Beat This! in {secs:.1}s: no steady grid, kept"),
        }
    }
    Ok(Side {
        path: path.to_path_buf(),
        buf,
        analysis,
    })
}

fn describe(path: &Path, a: &TrackAnalysis) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let bpm = a
        .bpm
        .map(|b| format!("{b:.2}"))
        .unwrap_or_else(|| "--".into());
    let key = a.camelot.map(|c| c.label()).unwrap_or_else(|| "--".into());
    let lufs = a
        .loudness
        .map(|l| format!("{l:.1}"))
        .unwrap_or_else(|| "--".into());
    format!(
        "{name}\n  {:.1}s  bpm {bpm} ({:.2})  key {key}  energy {:.3}  \
         {lufs} LUFS  peak {:+.1} dBFS  first beat {:.3}s  mix in {:.2}s  mix out {:.2}s",
        a.duration,
        a.bpm_confidence,
        a.energy,
        20.0 * a.peak.max(1e-6).log10(),
        a.first_beat,
        a.mix_in,
        a.mix_out
    )
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    let regrid = args.model.as_deref().map(regrid).transpose()?;
    let regrid = regrid.as_deref();
    if args.analyze_only {
        for path in &args.files {
            if let Err(err) = load(path, args.rate, regrid) {
                eprintln!("{}: {err:#}", path.display());
            }
        }
        return Ok(());
    }
    std::fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;
    let mut prev = load(&args.files[0], args.rate, regrid)?;
    for (n, path) in args.files.iter().enumerate().skip(1) {
        let next = load(path, args.rate, regrid)?;
        let mix = render(prev, next.clone(), &args.opts)?;
        let file = args.out.join(format!("{n:02}.wav"));
        let clipped = write_wav(&file, &mix.samples, mix.sample_rate, mix.channels)?;
        let span = |f: usize| mix.secs(f);
        match mix.plan {
            Some(p) => println!(
                "  → {}: {} speed {:.4} fade {:.1}s key {} bpm {} score {:.2}",
                file.display(),
                p.style.label(),
                p.speed,
                p.fade_secs,
                p.key_ok,
                p.bpm_ok,
                p.score
            ),
            None => println!("  → {}: cut", file.display()),
        }
        println!(
            "    overlap {:.2}–{:.2}s of the file, from {:.2}s of the outgoing track; \
             peak {:.3}, {clipped} samples clipped",
            span(mix.overlap.start),
            span(mix.overlap.end),
            mix.mix_at,
            mix.peak()
        );
        prev = next;
    }
    Ok(())
}
