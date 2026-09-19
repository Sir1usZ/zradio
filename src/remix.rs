use crate::decode::AudioBuf;
use crate::dsp::resample_linear;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemixMode {
    Off,
    Chill,
    Club,
    Nightcore,
}

impl RemixMode {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Chill,
            Self::Chill => Self::Club,
            Self::Club => Self::Nightcore,
            Self::Nightcore => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "RAW",
            Self::Chill => "CHILL",
            Self::Club => "CLUB",
            Self::Nightcore => "NCORE",
        }
    }

    pub fn rate(self) -> f32 {
        match self {
            Self::Off => 1.0,
            Self::Chill => 0.92,
            Self::Club => 1.0,
            Self::Nightcore => 1.12,
        }
    }
}

pub fn apply_remix(buf: &AudioBuf, mode: RemixMode) -> AudioBuf {
    if mode == RemixMode::Off || buf.samples.is_empty() {
        return buf.clone();
    }
    let mut samples = if (mode.rate() - 1.0).abs() > 0.001 {
        let in_rate = ((buf.sample_rate as f32) * mode.rate()).round() as u32;
        resample_linear(
            &buf.samples,
            buf.channels,
            in_rate.max(1),
            buf.channels,
            buf.sample_rate,
        )
    } else {
        buf.samples.clone()
    };
    match mode {
        RemixMode::Chill => {
            one_pole_lowpass(&mut samples, buf.channels, 0.12);
            gain(&mut samples, 0.92);
        }
        RemixMode::Club => {
            highpass_tilt(&mut samples, buf.channels, 0.04);
            saturate(&mut samples, 1.35);
            gain(&mut samples, 0.86);
        }
        RemixMode::Nightcore => {
            highpass_tilt(&mut samples, buf.channels, 0.02);
            gain(&mut samples, 0.9);
        }
        RemixMode::Off => {}
    }
    AudioBuf {
        samples,
        sample_rate: buf.sample_rate,
        channels: buf.channels,
    }
}

fn gain(samples: &mut [f32], g: f32) {
    for s in samples {
        *s *= g;
    }
}

fn saturate(samples: &mut [f32], drive: f32) {
    for s in samples {
        *s = (*s * drive).tanh();
    }
}

fn one_pole_lowpass(samples: &mut [f32], channels: usize, alpha: f32) {
    if channels == 0 {
        return;
    }
    let mut prev = vec![0.0; channels];
    for frame in samples.chunks_mut(channels) {
        for (ch, sample) in frame.iter_mut().enumerate() {
            prev[ch] += alpha * (*sample - prev[ch]);
            *sample = prev[ch];
        }
    }
}

fn highpass_tilt(samples: &mut [f32], channels: usize, alpha: f32) {
    if channels == 0 {
        return;
    }
    let mut prev_x = vec![0.0; channels];
    let mut prev_y = vec![0.0; channels];
    for frame in samples.chunks_mut(channels) {
        for (ch, sample) in frame.iter_mut().enumerate() {
            let x = *sample;
            let y = (1.0 - alpha) * (prev_y[ch] + x - prev_x[ch]);
            prev_x[ch] = x;
            prev_y[ch] = y;
            *sample = y;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remix_cycles_and_nightcore_shortens() {
        assert_eq!(RemixMode::Off.next(), RemixMode::Chill);
        let buf = AudioBuf {
            samples: vec![0.2; 4800],
            sample_rate: 48_000,
            channels: 2,
        };
        let night = apply_remix(&buf, RemixMode::Nightcore);
        assert!(night.frames() < buf.frames());
        let off = apply_remix(&buf, RemixMode::Off);
        assert_eq!(off.frames(), buf.frames());
    }
}
