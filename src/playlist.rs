use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::library::Track;

pub const MAX_LISTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistError {
    EmptyName,
    DuplicateName(String),
    Full,
    CannotDeleteAll,
    NotFound,
    AlreadyInList(String),
    EmptySlot(usize),
    WriteFailed,
}

impl PlaylistError {
    pub fn as_status(&self) -> String {
        match self {
            Self::EmptyName => "名字空".into(),
            Self::DuplicateName(name) => format!("已有 {name}"),
            Self::Full => "列表已满 8/8".into(),
            Self::CannotDeleteAll => "不能删全部库".into(),
            Self::NotFound => "列表不存在".into(),
            Self::AlreadyInList(name) => format!("已在 {name}"),
            Self::EmptySlot(slot) => format!("{slot} 空"),
            Self::WriteFailed => "playlist 写入失败".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Playlist {
    pub name: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaylistStore {
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub lists: Vec<Playlist>,
}

impl PlaylistStore {
    pub fn load() -> Self {
        Self::load_from(&store_path())
    }

    pub fn load_from(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Self>(&s).ok())
            .unwrap_or_default()
            .clamped()
    }

    pub fn save(&self) -> Result<(), PlaylistError> {
        self.save_to(&store_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), PlaylistError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| PlaylistError::WriteFailed)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|_| PlaylistError::WriteFailed)?;
        fs::write(path, json).map_err(|_| PlaylistError::WriteFailed)
    }

    fn clamped(mut self) -> Self {
        if self.lists.len() > MAX_LISTS {
            self.lists.truncate(MAX_LISTS);
        }
        if self.active > self.lists.len() {
            self.active = 0;
        }
        self
    }

    pub fn create(&mut self, name: &str) -> Result<usize, PlaylistError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(PlaylistError::EmptyName);
        }
        if self.lists.iter().any(|l| l.name == name) {
            return Err(PlaylistError::DuplicateName(name.into()));
        }
        if self.lists.len() >= MAX_LISTS {
            return Err(PlaylistError::Full);
        }
        self.lists.push(Playlist {
            name: name.into(),
            paths: Vec::new(),
        });
        self.active = self.lists.len();
        Ok(self.lists.len() - 1)
    }

    pub fn remove_current(&mut self) -> Result<(), PlaylistError> {
        if self.active == 0 {
            return Err(PlaylistError::CannotDeleteAll);
        }
        let idx = self.active - 1;
        if idx >= self.lists.len() {
            self.active = 0;
            return Err(PlaylistError::NotFound);
        }
        self.lists.remove(idx);
        self.active = 0;
        Ok(())
    }

    pub fn add_path(&mut self, list_idx: usize, path: PathBuf) -> Result<(), PlaylistError> {
        let list = self
            .lists
            .get_mut(list_idx)
            .ok_or(PlaylistError::NotFound)?;
        if list.paths.iter().any(|p| p == &path) {
            return Err(PlaylistError::AlreadyInList(list.name.clone()));
        }
        list.paths.push(path);
        Ok(())
    }

    pub fn remove_path(&mut self, list_idx: usize, path: &Path) -> Result<(), PlaylistError> {
        let list = self
            .lists
            .get_mut(list_idx)
            .ok_or(PlaylistError::NotFound)?;
        let before = list.paths.len();
        list.paths.retain(|p| p != path);
        if list.paths.len() == before {
            return Err(PlaylistError::NotFound);
        }
        Ok(())
    }

    pub fn jump_slot(&mut self, slot: usize) -> Result<usize, PlaylistError> {
        if slot == 1 {
            self.active = 0;
            return Ok(0);
        }
        if !(2..=9).contains(&slot) {
            return Err(PlaylistError::EmptySlot(slot));
        }
        let list_idx = slot - 2;
        if list_idx >= self.lists.len() {
            return Err(PlaylistError::EmptySlot(slot));
        }
        self.active = list_idx + 1;
        Ok(self.active)
    }

    pub fn cycle(&mut self, delta: i32) -> usize {
        let occupied = self.occupied_actives();
        let pos = occupied.iter().position(|&a| a == self.active).unwrap_or(0);
        let len = occupied.len() as i32;
        let next = occupied[(pos as i32 + delta).rem_euclid(len) as usize];
        self.active = next;
        next
    }

    fn occupied_actives(&self) -> Vec<usize> {
        let mut out = vec![0];
        out.extend(1..=self.lists.len());
        out
    }

    pub fn current_list(&self) -> Option<&Playlist> {
        if self.active == 0 {
            return None;
        }
        self.lists.get(self.active - 1)
    }

    pub fn current_list_idx(&self) -> Option<usize> {
        if self.active == 0 {
            None
        } else {
            Some(self.active - 1)
        }
    }

    pub fn slot_name(&self, slot: usize) -> String {
        if slot == 1 {
            return "全部".into();
        }
        self.lists
            .get(slot.saturating_sub(2))
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "—".into())
    }

    pub fn playable_indices(&self, tracks: &[Track]) -> Option<Vec<usize>> {
        let list = self.current_list()?;
        Some(
            list.paths
                .iter()
                .filter_map(|p| tracks.iter().position(|t| &t.path == p))
                .collect(),
        )
    }

    pub fn persist_or_rollback(&mut self, backup: Self, path: &Path) -> Result<(), PlaylistError> {
        if let Err(err) = self.save_to(path) {
            *self = backup;
            return Err(err);
        }
        Ok(())
    }
}

pub fn store_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("zradio").join("playlists.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "zradio-pl-{}-{}.json",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&p);
        p
    }

    fn track(path: &str) -> Track {
        Track {
            path: PathBuf::from(path),
            title: path.into(),
        }
    }

    #[test]
    fn rejects_empty_and_duplicate_names() {
        let mut s = PlaylistStore::default();
        assert_eq!(s.create("  "), Err(PlaylistError::EmptyName));
        assert_eq!(s.create("通勤"), Ok(0));
        assert_eq!(
            s.create("通勤"),
            Err(PlaylistError::DuplicateName("通勤".into()))
        );
        assert_eq!(s.active, 1);
    }

    #[test]
    fn ninth_list_is_rejected() {
        let mut s = PlaylistStore::default();
        for i in 0..MAX_LISTS {
            assert_eq!(s.create(&format!("l{i}")), Ok(i));
        }
        assert_eq!(s.create("overflow"), Err(PlaylistError::Full));
        assert_eq!(s.lists.len(), 8);
    }

    #[test]
    fn same_path_is_not_added_twice() {
        let mut s = PlaylistStore::default();
        s.create("通勤").unwrap();
        let p = PathBuf::from("/m/a.flac");
        assert!(s.add_path(0, p.clone()).is_ok());
        assert_eq!(
            s.add_path(0, p),
            Err(PlaylistError::AlreadyInList("通勤".into()))
        );
        assert_eq!(s.lists[0].paths.len(), 1);
    }

    #[test]
    fn playable_skips_missing_and_all_is_none() {
        let mut s = PlaylistStore::default();
        s.create("通勤").unwrap();
        s.add_path(0, PathBuf::from("/m/a.flac")).unwrap();
        s.add_path(0, PathBuf::from("/m/gone.flac")).unwrap();
        s.add_path(0, PathBuf::from("/m/c.flac")).unwrap();
        let tracks = vec![track("/m/a.flac"), track("/m/b.flac"), track("/m/c.flac")];
        assert_eq!(s.playable_indices(&tracks), Some(vec![0, 2]));
        s.active = 0;
        assert_eq!(s.playable_indices(&tracks), None);
    }

    #[test]
    fn active_out_of_range_clamps_on_load() {
        let p = tmp();
        fs::write(&p, r#"{"active":99,"lists":[{"name":"a","paths":[]}]}"#).unwrap();
        let s = PlaylistStore::load_from(&p);
        assert_eq!(s.active, 0);
        assert_eq!(s.lists.len(), 1);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn jump_empty_slot_fails_without_changing_active() {
        let mut s = PlaylistStore::default();
        s.create("通勤").unwrap();
        assert_eq!(s.jump_slot(3), Err(PlaylistError::EmptySlot(3)));
        assert_eq!(s.active, 1);
        assert_eq!(s.jump_slot(1), Ok(0));
        assert_eq!(s.active, 0);
        assert_eq!(s.jump_slot(2), Ok(1));
    }

    #[test]
    fn cycle_walks_all_then_user_lists() {
        let mut s = PlaylistStore::default();
        s.create("a").unwrap();
        s.create("b").unwrap();
        s.active = 0;
        assert_eq!(s.cycle(1), 1);
        assert_eq!(s.cycle(1), 2);
        assert_eq!(s.cycle(1), 0);
        assert_eq!(s.cycle(-1), 2);
    }

    #[test]
    fn cannot_delete_all_library() {
        let mut s = PlaylistStore::default();
        assert_eq!(s.remove_current(), Err(PlaylistError::CannotDeleteAll));
        s.create("通勤").unwrap();
        assert!(s.remove_current().is_ok());
        assert_eq!(s.active, 0);
        assert!(s.lists.is_empty());
    }

    #[test]
    fn write_failure_rolls_back() {
        let mut s = PlaylistStore::default();
        s.create("通勤").unwrap();
        let backup = PlaylistStore::default();
        let bad = PathBuf::from("/proc/zradio-nope/playlists.json");
        assert_eq!(
            s.persist_or_rollback(backup.clone(), &bad),
            Err(PlaylistError::WriteFailed)
        );
        assert_eq!(s, backup);
    }

    #[test]
    fn roundtrip_json() {
        let p = tmp();
        let mut s = PlaylistStore::default();
        s.create("通勤").unwrap();
        s.add_path(0, PathBuf::from("/m/a.flac")).unwrap();
        s.save_to(&p).unwrap();
        let loaded = PlaylistStore::load_from(&p);
        assert_eq!(loaded, s);
        let _ = fs::remove_file(&p);
    }
}
