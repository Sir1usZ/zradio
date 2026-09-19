use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::library::Track;

const LISTEN_RATIO: f32 = 0.8;
const SKIP_RATIO: f32 = 0.4;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrackTaste {
    title: String,
    artist: String,
    listens: u32,
    replays: u32,
    skips: u32,
    listen_secs: u64,
    last_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TasteCache {
    tracks: HashMap<String, TrackTaste>,
}

pub struct TasteStore {
    path: PathBuf,
    cache: TasteCache,
    dirty: bool,
    last_complete: Option<String>,
}

impl TasteStore {
    pub fn memory() -> Self {
        Self {
            path: PathBuf::new(),
            cache: TasteCache::default(),
            dirty: false,
            last_complete: None,
        }
    }

    pub fn load() -> Self {
        let path = data_path();
        let cache = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            cache,
            dirty: false,
            last_complete: None,
        }
    }

    pub fn record_leave(
        &mut self,
        path: &Path,
        title: &str,
        artist: &str,
        ratio: f32,
        skipped: bool,
        duration_secs: f32,
    ) {
        let ratio = ratio.clamp(0.0, 1.0);
        let played = (ratio * duration_secs.max(0.0)).round() as u64;
        if skipped && ratio < SKIP_RATIO {
            self.bump(path, title, artist, TasteKind::Skip, played);
            self.last_complete = None;
            return;
        }
        if ratio >= LISTEN_RATIO {
            let key = path_key(path);
            let kind = if self.last_complete.as_deref() == Some(key.as_str()) {
                TasteKind::Replay
            } else {
                TasteKind::Listen
            };
            self.bump(path, title, artist, kind, played);
            self.last_complete = Some(key);
        } else if played > 0 {
            self.bump(path, title, artist, TasteKind::Partial, played);
        }
    }

    pub fn record_replay(&mut self, path: &Path, title: &str, artist: &str) {
        self.bump(path, title, artist, TasteKind::Replay, 0);
        self.last_complete = Some(path_key(path));
    }

    pub fn total_listen_secs(&self) -> u64 {
        self.cache.tracks.values().map(|e| e.listen_secs).sum()
    }

    pub fn track_score(&self, path: &Path) -> f32 {
        self.cache
            .tracks
            .get(&path_key(path))
            .map(score_of)
            .unwrap_or(0.0)
    }

    pub fn artist_score(&self, artist: &str) -> f32 {
        let artist = norm_artist(artist);
        if artist.is_empty() {
            return 0.0;
        }
        self.cache
            .tracks
            .values()
            .filter(|e| norm_artist(&e.artist) == artist)
            .map(score_of)
            .sum()
    }

    pub fn boost_for(&self, path: &Path, artist: &str) -> f32 {
        boost(self.track_score(path), self.artist_score(artist))
    }

    pub fn boosts_for(&self, tracks: &[Track], artists: &[String]) -> Vec<f32> {
        tracks
            .iter()
            .enumerate()
            .map(|(i, track)| {
                let artist = artists.get(i).map(String::as_str).unwrap_or("");
                self.boost_for(&track.path, artist)
            })
            .collect()
    }

    pub fn top_tracks(&self, n: usize) -> Vec<(String, String, f32)> {
        let mut rows: Vec<_> = self
            .cache
            .tracks
            .values()
            .map(|e| (e.title.clone(), e.artist.clone(), score_of(e)))
            .filter(|(_, _, s)| *s > 0.0)
            .collect();
        rows.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(n);
        rows
    }

    pub fn top_artists(&self, n: usize) -> Vec<(String, f32)> {
        let mut map: HashMap<String, f32> = HashMap::new();
        for e in self.cache.tracks.values() {
            let artist = norm_artist(&e.artist);
            if artist.is_empty() {
                continue;
            }
            *map.entry(artist).or_default() += score_of(e);
        }
        let mut rows: Vec<_> = map.into_iter().filter(|(_, s)| *s > 0.0).collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(n);
        rows
    }

    pub fn flush(&mut self) {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&self.cache) {
            let _ = std::fs::write(&self.path, json);
            self.dirty = false;
        }
    }

    fn bump(&mut self, path: &Path, title: &str, artist: &str, kind: TasteKind, played: u64) {
        let key = path_key(path);
        let entry = self.cache.tracks.entry(key).or_default();
        if !title.trim().is_empty() {
            entry.title = title.trim().to_string();
        }
        if !artist.trim().is_empty() {
            entry.artist = artist.trim().to_string();
        }
        match kind {
            TasteKind::Listen => entry.listens = entry.listens.saturating_add(1),
            TasteKind::Replay => entry.replays = entry.replays.saturating_add(1),
            TasteKind::Skip => entry.skips = entry.skips.saturating_add(1),
            TasteKind::Partial => {}
        }
        entry.listen_secs = entry.listen_secs.saturating_add(played);
        entry.last_unix = now_unix();
        self.dirty = true;
    }
}

impl Drop for TasteStore {
    fn drop(&mut self) {
        self.flush();
    }
}

#[derive(Clone, Copy)]
enum TasteKind {
    Listen,
    Replay,
    Skip,
    Partial,
}

fn score_of(e: &TrackTaste) -> f32 {
    e.listens as f32 + e.replays as f32 * 1.6 - e.skips as f32 * 0.4
}

fn boost(track: f32, artist: f32) -> f32 {
    0.14 * (track / 6.0).tanh() + 0.08 * (artist / 16.0).tanh()
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn norm_artist(artist: &str) -> String {
    artist.trim().to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn data_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("taste.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_outranks_single_listen() {
        let mut taste = TasteStore::memory();
        let a = Path::new("/m/a.mp3");
        let b = Path::new("/m/b.mp3");
        taste.record_leave(a, "A", "Avicii", 1.0, false, 180.0);
        taste.record_leave(a, "A", "Avicii", 1.0, false, 180.0);
        taste.record_leave(b, "B", "Other", 1.0, false, 120.0);
        assert!(taste.track_score(a) > taste.track_score(b));
        assert!(taste.artist_score("Avicii") > taste.artist_score("Other"));
    }

    #[test]
    fn early_skip_does_not_beat_complete_listen() {
        let mut taste = TasteStore::memory();
        let liked = Path::new("/m/liked.mp3");
        let skipped = Path::new("/m/skip.mp3");
        taste.record_leave(liked, "Liked", "A", 1.0, false, 200.0);
        taste.record_leave(skipped, "Skip", "B", 0.1, true, 200.0);
        taste.record_leave(skipped, "Skip", "B", 0.2, true, 200.0);
        assert!(taste.track_score(liked) > taste.track_score(skipped));
        assert!(taste.boost_for(liked, "A") > taste.boost_for(skipped, "B"));
    }

    #[test]
    fn mid_listen_is_ignored() {
        let mut taste = TasteStore::memory();
        let p = Path::new("/m/mid.mp3");
        taste.record_leave(p, "Mid", "X", 0.55, true, 100.0);
        assert_eq!(taste.track_score(p), 0.0);
    }

    #[test]
    fn top_lists_prefer_high_score() {
        let mut taste = TasteStore::memory();
        taste.record_leave(Path::new("/m/one.mp3"), "One", "Alpha", 1.0, false, 90.0);
        taste.record_replay(Path::new("/m/one.mp3"), "One", "Alpha");
        taste.record_leave(Path::new("/m/two.mp3"), "Two", "Beta", 1.0, false, 80.0);
        let tracks = taste.top_tracks(2);
        assert_eq!(tracks[0].0, "One");
        let artists = taste.top_artists(1);
        assert_eq!(artists[0].0, "Alpha");
    }

    #[test]
    fn listen_secs_accumulate_played_time() {
        let mut taste = TasteStore::memory();
        taste.record_leave(Path::new("/m/a.mp3"), "A", "X", 1.0, false, 120.0);
        taste.record_leave(Path::new("/m/b.mp3"), "B", "Y", 0.2, true, 100.0);
        assert_eq!(taste.total_listen_secs(), 140);
    }
}
