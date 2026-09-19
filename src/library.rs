use std::path::{Path, PathBuf};

use crate::meta::TrackMeta;
use crate::prefs::SortMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
}

const EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "opus", "wav", "m4a", "aac", "aiff", "aif",
];

pub fn scan_library(root: &Path) -> anyhow::Result<Vec<Track>> {
    let mut tracks = Vec::new();
    scan_dir(root, &mut tracks)?;
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(tracks)
}

fn scan_dir(dir: &Path, out: &mut Vec<Track>) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, out)?;
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.as_deref().is_some_and(|e| EXTENSIONS.contains(&e)) {
            out.push(Track {
                title: title_from_path(&path),
                path,
            });
        }
    }
    Ok(())
}

pub fn track_from_path(path: PathBuf) -> Option<Track> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;
    if !EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    Some(Track {
        title: title_from_path(&path),
        path,
    })
}

pub fn title_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub fn sort_indices(tracks: &[Track], metas: &[Option<TrackMeta>], mode: SortMode) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..tracks.len()).collect();
    idx.sort_by(|&a, &b| match mode {
        SortMode::Path => tracks[a].path.cmp(&tracks[b].path),
        SortMode::Title => title_key(tracks, metas, a)
            .cmp(&title_key(tracks, metas, b))
            .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
        SortMode::Artist => artist_key(metas, a)
            .cmp(&artist_key(metas, b))
            .then_with(|| title_key(tracks, metas, a).cmp(&title_key(tracks, metas, b)))
            .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
        SortMode::Album => album_key(metas, a)
            .cmp(&album_key(metas, b))
            .then_with(|| artist_key(metas, a).cmp(&artist_key(metas, b)))
            .then_with(|| title_key(tracks, metas, a).cmp(&title_key(tracks, metas, b)))
            .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
    });
    idx
}

fn title_key(tracks: &[Track], metas: &[Option<TrackMeta>], i: usize) -> String {
    metas
        .get(i)
        .and_then(|m| m.as_ref())
        .map(|m| m.title.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| tracks.get(i).map(|t| t.title.as_str()).unwrap_or(""))
        .trim()
        .to_lowercase()
}

fn artist_key(metas: &[Option<TrackMeta>], i: usize) -> String {
    metas
        .get(i)
        .and_then(|m| m.as_ref())
        .map(|m| m.artist.trim().to_lowercase())
        .unwrap_or_default()
}

fn album_key(metas: &[Option<TrackMeta>], i: usize) -> String {
    metas
        .get(i)
        .and_then(|m| m.as_ref())
        .map(|m| m.album.trim().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_library_finds_audio_and_skips_hidden() {
        let root = std::env::temp_dir().join(format!("zradio-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("album")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join("album/Song One.mp3"), b"x").unwrap();
        fs::write(root.join("album/notes.txt"), b"x").unwrap();
        fs::write(root.join(".hidden/secret.flac"), b"x").unwrap();
        fs::write(root.join("cover.jpg"), b"x").unwrap();
        fs::write(root.join("outro.flac"), b"x").unwrap();

        let tracks = scan_library(&root).unwrap();
        let titles: Vec<_> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["Song One", "outro"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sort_library_by_artist_then_title() {
        let tracks = vec![
            Track {
                path: PathBuf::from("/m/z.flac"),
                title: "Zoo".into(),
            },
            Track {
                path: PathBuf::from("/m/a.flac"),
                title: "Alpha".into(),
            },
            Track {
                path: PathBuf::from("/m/b.flac"),
                title: "Beta".into(),
            },
        ];
        let metas = vec![
            Some(crate::meta::TrackMeta {
                artist: "Zedd".into(),
                album: "Clarity".into(),
                title: "Zoo".into(),
                ..crate::meta::TrackMeta::default()
            }),
            Some(crate::meta::TrackMeta {
                artist: "Avicii".into(),
                album: "True".into(),
                title: "Alpha".into(),
                ..crate::meta::TrackMeta::default()
            }),
            Some(crate::meta::TrackMeta {
                artist: "Avicii".into(),
                album: "Stories".into(),
                title: "Beta".into(),
                ..crate::meta::TrackMeta::default()
            }),
        ];
        assert_eq!(sort_indices(&tracks, &metas, SortMode::Path), vec![1, 2, 0]);
        assert_eq!(
            sort_indices(&tracks, &metas, SortMode::Title),
            vec![1, 2, 0]
        );
        assert_eq!(
            sort_indices(&tracks, &metas, SortMode::Artist),
            vec![1, 2, 0]
        );
        assert_eq!(
            sort_indices(&tracks, &metas, SortMode::Album),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn title_strips_extension() {
        assert_eq!(
            title_from_path(Path::new("/m/Avicii - Waiting For Love.mp3")),
            "Avicii - Waiting For Love"
        );
        assert!(track_from_path(PathBuf::from("/m/a.flac")).is_some());
        assert!(track_from_path(PathBuf::from("/m/a.txt")).is_none());
    }
}
