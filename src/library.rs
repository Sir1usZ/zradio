use std::path::{Path, PathBuf};

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
    fn title_strips_extension() {
        assert_eq!(
            title_from_path(Path::new("/m/Avicii - Waiting For Love.mp3")),
            "Avicii - Waiting For Love"
        );
        assert!(track_from_path(PathBuf::from("/m/a.flac")).is_some());
        assert!(track_from_path(PathBuf::from("/m/a.txt")).is_none());
    }
}
