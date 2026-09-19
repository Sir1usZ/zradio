use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};

use crate::engine::Snapshot;
use crate::meta::{file_url, TrackMeta};

pub enum MediaCmd {
    PlayPause,
    Next,
    Previous,
    Stop,
    Seek(f32),
}

pub struct Mpris {
    controls: Option<MediaControls>,
    events: Option<Receiver<MediaControlEvent>>,
    last_title: String,
    last_playing: Option<bool>,
    last_progress_secs: u64,
}

impl Mpris {
    pub fn start() -> Self {
        let config = PlatformConfig {
            dbus_name: "zradio",
            display_name: "ZRadio",
            hwnd: None,
        };
        match MediaControls::new(config) {
            Ok(mut controls) => {
                let (tx, rx) = mpsc::sync_channel(32);
                if controls
                    .attach(move |event| {
                        let _ = tx.send(event);
                    })
                    .is_ok()
                {
                    Self {
                        controls: Some(controls),
                        events: Some(rx),
                        last_title: String::new(),
                        last_playing: None,
                        last_progress_secs: u64::MAX,
                    }
                } else {
                    Self::disabled()
                }
            }
            Err(_) => Self::disabled(),
        }
    }

    fn disabled() -> Self {
        Self {
            controls: None,
            events: None,
            last_title: String::new(),
            last_playing: None,
            last_progress_secs: u64::MAX,
        }
    }

    pub fn poll(&mut self) -> Vec<MediaCmd> {
        let Some(rx) = self.events.as_mut() else {
            return Vec::new();
        };
        let mut cmds = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(
                    MediaControlEvent::Toggle | MediaControlEvent::Play | MediaControlEvent::Pause,
                ) => {
                    cmds.push(MediaCmd::PlayPause);
                }
                Ok(MediaControlEvent::Next) => cmds.push(MediaCmd::Next),
                Ok(MediaControlEvent::Previous) => cmds.push(MediaCmd::Previous),
                Ok(MediaControlEvent::Stop) => cmds.push(MediaCmd::Stop),
                Ok(MediaControlEvent::Seek(souvlaki::SeekDirection::Forward)) => {
                    cmds.push(MediaCmd::Seek(5.0));
                }
                Ok(MediaControlEvent::Seek(souvlaki::SeekDirection::Backward)) => {
                    cmds.push(MediaCmd::Seek(-5.0));
                }
                Ok(MediaControlEvent::SeekBy(dir, dur)) => {
                    let secs = dur.as_secs_f32();
                    let signed = match dir {
                        souvlaki::SeekDirection::Forward => secs,
                        souvlaki::SeekDirection::Backward => -secs,
                    };
                    cmds.push(MediaCmd::Seek(signed));
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        cmds
    }

    pub fn publish(&mut self, snap: &Snapshot, meta: Option<&TrackMeta>, cover: Option<&Path>) {
        let Some(controls) = self.controls.as_mut() else {
            return;
        };
        let title = meta.map(|m| m.title.as_str()).unwrap_or("ZRadio");
        let artist = meta.map(|m| m.artist.as_str()).filter(|s| !s.is_empty());
        let album = meta.map(|m| m.album.as_str()).filter(|s| !s.is_empty());
        let cover_url = cover.map(file_url);
        if title != self.last_title {
            let duration = if snap.sample_rate == 0 {
                None
            } else {
                Some(Duration::from_secs_f32(
                    snap.duration_frames as f32 / snap.sample_rate as f32,
                ))
            };
            let _ = controls.set_metadata(MediaMetadata {
                title: Some(title),
                artist,
                album,
                cover_url: cover_url.as_deref(),
                duration,
            });
            self.last_title = title.to_string();
        }
        let playing = !snap.paused && snap.current.is_some();
        let secs = if snap.sample_rate == 0 {
            0
        } else {
            (snap.position_frames as u64) / u64::from(snap.sample_rate.max(1))
        };
        if self.last_playing != Some(playing) || self.last_progress_secs != secs {
            let progress = if snap.sample_rate == 0 {
                None
            } else {
                Some(MediaPosition(Duration::from_secs_f32(
                    snap.position_frames as f32 / snap.sample_rate as f32,
                )))
            };
            let playback = if snap.current.is_none() {
                MediaPlayback::Stopped
            } else if snap.paused {
                MediaPlayback::Paused { progress }
            } else {
                MediaPlayback::Playing { progress }
            };
            let _ = controls.set_playback(playback);
            self.last_playing = Some(playing);
            self.last_progress_secs = secs;
        }
    }
}
