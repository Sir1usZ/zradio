use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}
