//! Renders AutoMix transitions between audio files to WAV, through the player's mixer.
//!
//!     cargo run --release --example automix_render -- [options] a.flac b.mp3 [c.ogg ...]
//!
//! Each neighbouring pair becomes NN.wav in the output directory: `before` seconds of the
//! outgoing track, the overlap, then `after` seconds of the incoming one.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use zradio::analysis::{analyze_buffer, TrackAnalysis};
use zradio::decode::decode_file;
use zradio::mixdown::{render, write_wav, Options, Side};
use zradio::MixMode;

const USAGE: &str = "usage: automix_render [--mode auto|fade|cut] [--skip SECS] \
[--before SECS] [--after SECS] [--rate HZ] [-o DIR] [--analyze] FILE FILE [FILE ...]";

struct Args {
    opts: Options,
    rate: u32,
    out: PathBuf,
    analyze_only: bool,
    files: Vec<PathBuf>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        opts: Options::default(),
        rate: 48_000,
        out: PathBuf::from("automix-out"),
        analyze_only: false,
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

fn load(path: &Path, rate: u32) -> anyhow::Result<Side> {
    let buf = decode_file(path, rate, 2)?;
    let analysis = analyze_buffer(&buf);
    println!("{}", describe(path, &analysis));
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
    format!(
        "{name}\n  {:.1}s  bpm {bpm} ({:.2})  key {key}  energy {:.3}  \
         first beat {:.3}s  mix in {:.2}s  mix out {:.2}s",
        a.duration, a.bpm_confidence, a.energy, a.first_beat, a.mix_in, a.mix_out
    )
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    if args.analyze_only {
        for path in &args.files {
            if let Err(err) = load(path, args.rate) {
                eprintln!("{}: {err:#}", path.display());
            }
        }
        return Ok(());
    }
    std::fs::create_dir_all(&args.out).with_context(|| format!("create {}", args.out.display()))?;
    let mut prev = load(&args.files[0], args.rate)?;
    for (n, path) in args.files.iter().enumerate().skip(1) {
        let next = load(path, args.rate)?;
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
