use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::library::Track;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub path: String,
    pub position_secs: f32,
}

impl SessionState {
    pub fn load() -> Self {
        fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn track_index(&self, tracks: &[Track]) -> Option<usize> {
        if self.path.is_empty() {
            return None;
        }
        let saved = Path::new(&self.path);
        tracks.iter().position(|t| t.path == saved)
    }

    pub fn save_for(track: &Path, secs: f32) {
        let state = Self {
            path: track.display().to_string(),
            position_secs: secs.max(0.0),
        };
        if let Some(parent) = path().parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = fs::write(path(), json);
        }
    }
}

pub fn resume_seek(position_secs: f32, duration_secs: f32) -> f32 {
    if duration_secs > 0.0 && position_secs + 3.0 >= duration_secs {
        0.0
    } else {
        position_secs.max(0.0)
    }
}

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("session.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_session_is_empty() {
        let s = SessionState::default();
        assert!(s.path.is_empty());
        assert_eq!(s.position_secs, 0.0);
    }

    #[test]
    fn track_index_matches_saved_path() {
        let tracks = vec![
            Track {
                path: PathBuf::from("/music/a.flac"),
                title: "A".into(),
            },
            Track {
                path: PathBuf::from("/music/b.flac"),
                title: "B".into(),
            },
        ];
        let miss = SessionState {
            path: "/music/missing.flac".into(),
            position_secs: 10.0,
        };
        assert_eq!(miss.track_index(&tracks), None);
        let hit = SessionState {
            path: "/music/b.flac".into(),
            position_secs: 82.9,
        };
        assert_eq!(hit.track_index(&tracks), Some(1));
    }

    #[test]
    fn resume_near_end_starts_over() {
        assert_eq!(resume_seek(10.0, 180.0), 10.0);
        assert_eq!(resume_seek(178.0, 180.0), 0.0);
        assert_eq!(resume_seek(0.0, 180.0), 0.0);
    }
}
