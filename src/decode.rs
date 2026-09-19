use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context};
use symphonia::core::audio::{AudioBufferRef, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::dsp::resample_linear;

#[derive(Debug, Clone)]
pub struct AudioBuf {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: usize,
}

impl AudioBuf {
    pub fn frames(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }

    pub fn stereo_at(&self, frame: usize) -> [f32; 2] {
        if self.channels == 0 || self.samples.is_empty() {
            return [0.0, 0.0];
        }
        let i = frame.min(self.frames().saturating_sub(1)) * self.channels;
        if self.channels == 1 {
            let s = self.samples[i];
            [s, s]
        } else {
            [self.samples[i], self.samples[i + 1]]
        }
    }
}

pub fn decode_file(
    path: &Path,
    target_rate: u32,
    target_channels: usize,
) -> anyhow::Result<AudioBuf> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probe {}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?
        .clone();
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .with_context(|| format!("decoder for {}", path.display()))?;

    let mut raw = Vec::new();
    let mut spec: Option<SignalSpec> = None;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(_)) => break,
            Err(Error::ResetRequired) => break,
            Err(err) => return Err(err).with_context(|| format!("demux {}", path.display())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                collect_decoded(decoded, &mut spec, &mut sample_buf, &mut raw);
            }
            Err(Error::IoError(_)) | Err(Error::DecodeError(_)) => continue,
            Err(err) => return Err(err).with_context(|| format!("decode {}", path.display())),
        }
    }

    let spec = spec.ok_or_else(|| anyhow!("empty decode {}", path.display()))?;
    let in_rate = spec.rate;
    let in_channels = spec.channels.count();
    let samples = resample_linear(&raw, in_channels, in_rate, target_channels, target_rate);
    Ok(AudioBuf {
        samples,
        sample_rate: target_rate,
        channels: target_channels,
    })
}

fn collect_decoded(
    decoded: AudioBufferRef<'_>,
    spec: &mut Option<SignalSpec>,
    sample_buf: &mut Option<SampleBuffer<f32>>,
    raw: &mut Vec<f32>,
) {
    let decoded_spec = *decoded.spec();
    let frames = decoded.frames();
    if frames == 0 {
        return;
    }
    let buf = sample_buf
        .get_or_insert_with(|| SampleBuffer::<f32>::new(decoded.capacity() as u64, decoded_spec));
    if buf.capacity() < frames {
        *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, decoded_spec);
    }
    buf.copy_interleaved_ref(decoded);
    raw.extend_from_slice(buf.samples());
    *spec = Some(decoded_spec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_sine_wav(path: &Path, rate: u32, frames: u32) {
        let mut data = Vec::with_capacity(frames as usize * 4);
        for n in 0..frames {
            let t = n as f32 / rate as f32;
            let s = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
            let i = (s * 30000.0) as i16;
            data.extend_from_slice(&i.to_le_bytes());
            data.extend_from_slice(&i.to_le_bytes());
        }
        let mut f = File::create(path).unwrap();
        let data_len = data.len() as u32;
        let riff_len = 36 + data_len;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_len.to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&rate.to_le_bytes()).unwrap();
        let byte_rate = rate * 2 * 2;
        f.write_all(&byte_rate.to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        f.write_all(&data).unwrap();
    }

    #[test]
    fn decode_wav_to_target_rate() {
        let path = std::env::temp_dir().join(format!("zradio-sine-{}.wav", std::process::id()));
        write_sine_wav(&path, 44_100, 4410);
        let buf = decode_file(&path, 48_000, 2).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(buf.channels, 2);
        assert_eq!(buf.sample_rate, 48_000);
        assert!((buf.frames() as i32 - 4800).abs() < 8);
        let energy: f32 = buf.samples.iter().map(|s| s * s).sum();
        assert!(energy > 1.0);
    }
}
