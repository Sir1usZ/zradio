use std::collections::HashSet;
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
    let mut visited_dirs = HashSet::new();
    let mut seen_files = HashSet::new();
    scan_dir(root, &mut tracks, &mut visited_dirs, &mut seen_files)?;
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(tracks)
}

fn scan_dir(
    dir: &Path,
    out: &mut Vec<Track>,
    visited_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let canonical = match std::fs::canonicalize(dir) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if !visited_dirs.insert(canonical) {
        return Ok(());
    }
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
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            scan_dir(&path, out, visited_dirs, seen_files)?;
            continue;
        }
        let is_file = if file_type.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {
                    scan_dir(&path, out, visited_dirs, seen_files)?;
                    continue;
                }
                Ok(metadata) => metadata.is_file(),
                Err(_) => false,
            }
        } else {
            file_type.is_file()
        };
        if !is_file {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !ext.as_deref().is_some_and(|e| EXTENSIONS.contains(&e)) {
            continue;
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(_) => continue,
        };
        if !seen_files.insert(canonical) {
            continue;
        }
        out.push(Track {
            title: title_from_path(&path),
            path,
        });
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
    match mode {
        SortMode::Path => idx.sort_by(|&a, &b| tracks[a].path.cmp(&tracks[b].path)),
        mode => {
            let keys: Vec<(String, String, String)> = (0..tracks.len())
                .map(|i| {
                    (
                        album_key(metas, i),
                        artist_key(metas, i),
                        title_key(tracks, metas, i),
                    )
                })
                .collect();
            idx.sort_by(|&a, &b| match mode {
                SortMode::Title => keys[a]
                    .2
                    .cmp(&keys[b].2)
                    .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
                SortMode::Artist => keys[a]
                    .1
                    .cmp(&keys[b].1)
                    .then_with(|| keys[a].2.cmp(&keys[b].2))
                    .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
                SortMode::Album => keys[a]
                    .0
                    .cmp(&keys[b].0)
                    .then_with(|| keys[a].1.cmp(&keys[b].1))
                    .then_with(|| keys[a].2.cmp(&keys[b].2))
                    .then_with(|| tracks[a].path.cmp(&tracks[b].path)),
                SortMode::Path => unreachable!(),
            });
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn scan_library_follows_symlinked_audio_without_looping() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("zradio-scan-symlink-{}", std::process::id()));
        let root = base.join("library");
        let source = base.join("source");
        let album = source.join("album");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&album).unwrap();
        fs::write(source.join("linked.flac"), b"x").unwrap();
        fs::write(album.join("inside.mp3"), b"x").unwrap();
        symlink(source.join("linked.flac"), root.join("linked.flac")).unwrap();
        symlink(&album, root.join("linked-album")).unwrap();
        symlink(&root, album.join("back-to-library")).unwrap();

        let tracks = scan_library(&root).unwrap();
        let titles: Vec<_> = tracks.iter().map(|track| track.title.as_str()).collect();
        assert_eq!(titles, ["inside", "linked"]);

        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_library_dedupes_file_symlinks_to_the_same_inode() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!("zradio-scan-file-{}", std::process::id()));
        let root = base.join("library");
        let source = base.join("source");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("song.flac"), b"x").unwrap();
        symlink(source.join("song.flac"), root.join("a.flac")).unwrap();
        symlink(source.join("song.flac"), root.join("b.flac")).unwrap();
        symlink(source.join("song.flac"), root.join("again.flac")).unwrap();

        let tracks = scan_library(&root).unwrap();
        assert_eq!(tracks.len(), 1, "{tracks:?}");

        fs::remove_dir_all(base).unwrap();
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
