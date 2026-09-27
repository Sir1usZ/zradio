use std::fs;
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::Accessor;

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub lyrics: Vec<LyricLine>,
    pub cover_path: Option<PathBuf>,
    pub cover: Option<crate::cover::CoverArt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    pub time: f32,
    pub text: String,
}

pub fn peek_tags(path: &Path, fallback_title: &str) -> TrackMeta {
    let mut meta = TrackMeta {
        title: fallback_title.to_string(),
        ..TrackMeta::default()
    };
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    if let Ok(tagged) = Probe::open(path)
        .map(|p| p.options(options))
        .and_then(|p| p.read())
    {
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(title) = tag.title() {
                if !title.trim().is_empty() {
                    meta.title = title.trim().to_string();
                }
            }
            if let Some(artist) = tag.artist() {
                meta.artist = artist.trim().to_string();
            }
            if let Some(album) = tag.album() {
                meta.album = album.trim().to_string();
            }
        }
    }
    meta
}

pub fn apply_peek(slot: &mut Option<TrackMeta>, peeked: TrackMeta) -> bool {
    let Some(existing) = slot.as_mut() else {
        *slot = Some(peeked);
        return true;
    };

    let mut changed = false;
    changed |= fill_empty_text(&mut existing.title, peeked.title);
    changed |= fill_empty_text(&mut existing.artist, peeked.artist);
    changed |= fill_empty_text(&mut existing.album, peeked.album);
    if existing.lyrics.is_empty() && !peeked.lyrics.is_empty() {
        existing.lyrics = peeked.lyrics;
        changed = true;
    }
    if existing.cover_path.is_none() && peeked.cover_path.is_some() {
        existing.cover_path = peeked.cover_path;
        changed = true;
    }
    if existing.cover.is_none() && peeked.cover.is_some() {
        existing.cover = peeked.cover;
        changed = true;
    }
    changed
}

fn fill_empty_text(dst: &mut String, src: String) -> bool {
    if dst.trim().is_empty() && !src.trim().is_empty() {
        *dst = src;
        true
    } else {
        false
    }
}

pub fn needs_remote(meta: &TrackMeta) -> bool {
    meta.artist.trim().is_empty()
        || meta.album.trim().is_empty()
        || meta.lyrics.is_empty()
        || (meta.cover.is_none() && meta.cover_path.is_none())
}

pub fn needs_fetch(meta: &TrackMeta, lyrics: bool, cover: bool) -> bool {
    meta.artist.trim().is_empty()
        || meta.album.trim().is_empty()
        || (meta.lyrics.is_empty() && lyrics)
        || (meta.cover.is_none() && meta.cover_path.is_none() && cover)
}

pub fn load_meta(path: &Path, fallback_title: &str) -> TrackMeta {
    let mut meta = TrackMeta {
        title: fallback_title.to_string(),
        ..TrackMeta::default()
    };
    if let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) {
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(title) = tag.title() {
                if !title.trim().is_empty() {
                    meta.title = title.trim().to_string();
                }
            }
            if let Some(artist) = tag.artist() {
                meta.artist = artist.trim().to_string();
            }
            if let Some(album) = tag.album() {
                meta.album = album.trim().to_string();
            }
            if let Some(lyrics) = tag.get_string(&lofty::tag::ItemKey::Lyrics) {
                meta.lyrics = parse_lyrics(lyrics);
            }
            let pictures = tag.pictures();
            let pic = pictures
                .iter()
                .find(|p| p.pic_type() == PictureType::CoverFront)
                .or_else(|| pictures.first());
            if let Some(pic) = pic {
                if let Some((cover_path, cover)) =
                    store_cover(path, pic.data(), pic.mime_type().map(|m| m.as_str()))
                {
                    meta.cover_path = Some(cover_path);
                    meta.cover = cover;
                }
            }
        }
    }
    if meta.lyrics.is_empty() {
        if let Some(lrc) = sidecar_lrc(path) {
            meta.lyrics = parse_lyrics(&lrc);
        }
    }
    if meta.cover_path.is_none() {
        if let Some(cover) = sidecar_cover(path) {
            if let Ok(bytes) = fs::read(&cover) {
                if let Some((cover_path, art)) = store_cover(&cover, &bytes, None) {
                    meta.cover_path = Some(cover_path);
                    meta.cover = art;
                }
            }
        }
    }
    meta
}

pub fn format_lrc(lines: &[LyricLine]) -> String {
    let mut out = String::new();
    for line in lines {
        let total_cs = (line.time * 100.0).round() as i64;
        let total_cs = total_cs.max(0);
        let minutes = total_cs / 6000;
        let seconds = (total_cs % 6000) / 100;
        let centis = total_cs % 100;
        out.push_str(&format!(
            "[{minutes:02}:{seconds:02}.{centis:02}]{}\n",
            line.text
        ));
    }
    out
}

pub fn parse_lyrics(raw: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((times, text)) = split_lrc(line) {
            for time in times {
                lines.push(LyricLine {
                    time,
                    text: text.to_string(),
                });
            }
        } else if !line.starts_with('[') {
            lines.push(LyricLine {
                time: lines.len() as f32 * 4.0,
                text: line.to_string(),
            });
        }
    }
    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    lines
}

pub fn current_lyric(lines: &[LyricLine], secs: f32) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, line)| secs + 0.05 >= line.time)
        .map(|(i, _)| i)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricFill {
    pub done: String,
    pub rest: String,
}

pub fn lyric_fill(text: &str, progress: f32) -> LyricFill {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return LyricFill {
            done: String::new(),
            rest: String::new(),
        };
    }
    let n = ((progress.clamp(0.0, 1.0) * chars.len() as f32).round() as usize).min(chars.len());
    LyricFill {
        done: chars[..n].iter().collect(),
        rest: chars[n..].iter().collect(),
    }
}

pub fn lyric_progress(lines: &[LyricLine], secs: f32) -> f32 {
    let Some(i) = current_lyric(lines, secs) else {
        return 0.0;
    };
    let start = lines[i].time;
    let end = lines
        .get(i + 1)
        .map(|l| l.time)
        .unwrap_or(start + 4.0)
        .max(start + 0.2);
    ((secs - start) / (end - start)).clamp(0.0, 1.0)
}

#[derive(Debug, Clone, PartialEq)]
pub struct LyricViewLine {
    pub text: String,
    pub current: bool,
    pub distance: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LyricWindow {
    pub current: Option<usize>,
    pub lines: Vec<LyricViewLine>,
}

pub fn lyric_window(lines: &[LyricLine], secs: f32, rows: usize) -> LyricWindow {
    if lines.is_empty() || rows == 0 {
        return LyricWindow {
            current: None,
            lines: Vec::new(),
        };
    }
    let current = current_lyric(lines, secs).unwrap_or(0);
    let half = rows / 2;
    let start = current.saturating_sub(half);
    let end = (start + rows).min(lines.len());
    let start = end.saturating_sub(rows);
    let view = lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let index = start + offset;
            let distance = index.abs_diff(current);
            LyricViewLine {
                text: line.text.clone(),
                current: index == current,
                distance,
            }
        })
        .collect();
    LyricWindow {
        current: Some(current),
        lines: view,
    }
}

fn split_lrc(line: &str) -> Option<(Vec<f32>, &str)> {
    let mut rest = line;
    let mut times = Vec::new();
    while rest.starts_with('[') {
        let end = rest.find(']')?;
        let inner = &rest[1..end];
        rest = rest[end + 1..].trim_start();
        if let Some(time) = parse_lrc_time(inner) {
            times.push(time);
        } else if inner.contains(':') && inner.chars().next()?.is_ascii_alphabetic() {
            return None;
        }
    }
    if times.is_empty() {
        None
    } else {
        Some((times, rest.trim()))
    }
}

fn parse_lrc_time(inner: &str) -> Option<f32> {
    let (mm, ss) = inner.split_once(':')?;
    let minutes: f32 = mm.parse().ok()?;
    let seconds: f32 = ss.parse().ok()?;
    Some(minutes * 60.0 + seconds)
}

fn sidecar_lrc(path: &Path) -> Option<String> {
    let direct = path.with_extension("lrc");
    if let Ok(text) = fs::read_to_string(&direct) {
        return Some(text);
    }
    let stem = path.file_stem()?.to_string_lossy();
    let dir = path.parent()?;
    for folder in ["lrc", "LRC"] {
        let candidate = dir.join(folder).join(format!("{stem}.lrc"));
        if let Ok(text) = fs::read_to_string(candidate) {
            return Some(text);
        }
    }
    None
}

fn sidecar_cover(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let stem = path.file_stem()?.to_string_lossy();
    let mut names = vec![
        format!("{stem}.jpg"),
        format!("{stem}.png"),
        "cover.jpg".into(),
        "cover.png".into(),
        "folder.jpg".into(),
        "Folder.jpg".into(),
    ];
    for folder in ["cover", "covers"] {
        names.push(format!("{folder}/{stem}.jpg"));
        names.push(format!("{folder}/{stem}.png"));
        names.push(format!("{folder}/cover.jpg"));
        names.push(format!("{folder}/cover.png"));
    }
    for name in names {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn cache_cover(src: &Path, data: &[u8]) -> Option<(PathBuf, Option<crate::cover::CoverArt>)> {
    store_cover(src, data, None)
}

fn store_cover(
    src: &Path,
    data: &[u8],
    mime: Option<&str>,
) -> Option<(PathBuf, Option<crate::cover::CoverArt>)> {
    let cache = cover_cache_dir()?;
    let ext = cover_extension(data, mime);
    let stem = src.file_stem()?.to_string_lossy();
    let out = cache.join(cover_cache_name(src, ext));
    fs::write(&out, data).ok()?;
    if let Some(dir) = src.parent() {
        let folder = dir.join("cover");
        let _ = fs::create_dir_all(&folder);
        let _ = fs::write(folder.join(format!("{stem}.{ext}")), data);
    }
    Some((out, crate::cover::CoverArt::from_bytes(data)))
}

fn cover_extension(data: &[u8], mime: Option<&str>) -> &'static str {
    match mime.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("image/png") => return "png",
        Some("image/gif") => return "gif",
        Some("image/webp") => return "webp",
        Some("image/jpeg" | "image/jpg") => return "jpg",
        _ => {}
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        "gif"
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        "webp"
    } else {
        "jpg"
    }
}

fn cover_cache_name(src: &Path, ext: &str) -> String {
    let stem: String = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("cover")
        .chars()
        .take(64)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in src.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{stem}-{hash:016x}.{ext}")
}

fn cover_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let dir = base.join("zradio");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

pub fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_cache_names_distinguish_equal_stems_in_different_folders() {
        let first = cover_cache_name(Path::new("/music/album-a/song.mp3"), "jpg");
        let second = cover_cache_name(Path::new("/music/album-b/song.mp3"), "jpg");

        assert_ne!(first, second);
        assert!(first.starts_with("song-"));
        assert!(first.ends_with(".jpg"));
    }

    #[test]
    fn cover_cache_type_uses_signature_when_mime_is_missing() {
        let png = b"\x89PNG\r\n\x1a\nrest";
        let gif = b"GIF89arest";
        let webp = b"RIFF\x08\x00\x00\x00WEBPrest";

        assert_eq!(cover_extension(png, None), "png");
        assert_eq!(cover_extension(gif, None), "gif");
        assert_eq!(cover_extension(webp, None), "webp");
        assert_eq!(cover_extension(png, Some("image/jpeg")), "jpg");
    }

    #[test]
    fn parse_lrc_times_and_text() {
        let lyrics = parse_lyrics("[00:12.50]hello\n[01:03]world\n[ti:skip]");
        assert_eq!(lyrics.len(), 2);
        assert!((lyrics[0].time - 12.5).abs() < 1e-3);
        assert_eq!(lyrics[0].text, "hello");
        assert!((lyrics[1].time - 63.0).abs() < 1e-3);
    }

    #[test]
    fn format_lrc_writes_complete_file() {
        let lyrics = parse_lyrics("[00:12.50]hello\n[01:03]world");
        assert_eq!(format_lrc(&lyrics), "[00:12.50]hello\n[01:03.00]world\n");
        assert_eq!(format_lrc(&[]), "");
    }

    #[test]
    fn current_lyric_picks_latest_line() {
        let lines = parse_lyrics("[00:00]a\n[00:10]b\n[00:20]c");
        assert_eq!(current_lyric(&lines, 0.0), Some(0));
        assert_eq!(current_lyric(&lines, 10.2), Some(1));
        assert_eq!(current_lyric(&lines, 40.0), Some(2));
    }

    #[test]
    fn lyric_window_centers_current_line() {
        let lines = parse_lyrics("[00:00]one\n[00:10]two\n[00:20]three\n[00:30]four\n[00:40]five");
        let view = lyric_window(&lines, 20.0, 5);
        assert_eq!(view.current, Some(2));
        assert_eq!(
            view.lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three", "four", "five"]
        );
        assert!(view.lines[2].current);
        assert!(!view.lines[1].current);
        assert_eq!(view.lines[2].distance, 0);
        assert_eq!(view.lines[1].distance, 1);
        assert_eq!(view.lines[3].distance, 1);
    }

    #[test]
    fn lyric_window_empty_when_no_lyrics() {
        let view = lyric_window(&[], 12.0, 5);
        assert!(view.lines.is_empty());
        assert_eq!(view.current, None);
    }

    #[test]
    fn lyric_fill_splits_current_line() {
        let fill = lyric_fill("waiting for love", 0.5);
        assert_eq!(fill.done, "waiting ");
        assert_eq!(fill.rest, "for love");
        let none = lyric_fill("waiting for love", 0.0);
        assert!(none.done.is_empty());
        assert_eq!(none.rest, "waiting for love");
    }

    #[test]
    fn lyric_progress_between_timestamps() {
        let lines = parse_lyrics("[00:00]a\n[00:10]b\n[00:20]c");
        let p = lyric_progress(&lines, 15.0);
        assert!((p - 0.5).abs() < 0.02);
    }

    fn write_silent_wav(path: &Path) {
        let samples = [0u8; 160];
        let data_len = samples.len() as u32;
        let riff_len = 36 + data_len;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_len.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.extend_from_slice(&samples);
        fs::write(path, wav).unwrap();
    }

    fn tagged_wav(dir: &Path, name: &str, artist: &str, title: &str, album: &str) -> PathBuf {
        use lofty::config::{ParseOptions, WriteOptions};
        use lofty::file::TaggedFileExt;
        use lofty::tag::{Tag, TagExt, TagType};
        let path = dir.join(name);
        write_silent_wav(&path);
        let mut tagged = Probe::open(&path)
            .unwrap()
            .options(ParseOptions::new().read_properties(false))
            .read()
            .unwrap();
        let tag = match tagged.primary_tag_mut() {
            Some(t) => t,
            None => {
                tagged.insert_tag(Tag::new(TagType::Id3v2));
                tagged.primary_tag_mut().unwrap()
            }
        };
        tag.set_artist(artist.into());
        tag.set_title(title.into());
        tag.set_album(album.into());
        tag.save_to_path(&path, WriteOptions::default()).unwrap();
        path
    }

    #[test]
    fn peek_tags_reads_artist_without_cover() {
        let dir = std::env::temp_dir().join(format!("zradio-peek-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = tagged_wav(&dir, "song.wav", "Avicii", "Levels", "True");

        let meta = peek_tags(&path, "fallback");
        assert_eq!(meta.artist, "Avicii");
        assert_eq!(meta.title, "Levels");
        assert_eq!(meta.album, "True");
        assert!(meta.cover.is_none());
        assert!(meta.lyrics.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn incomplete_meta_needs_fetch() {
        let empty = TrackMeta::default();
        assert!(needs_remote(&empty));
        let full = TrackMeta {
            artist: "Avicii".into(),
            title: "Levels".into(),
            album: "True".into(),
            lyrics: vec![LyricLine {
                time: 0.0,
                text: "oh".into(),
            }],
            cover_path: Some(std::path::PathBuf::from("/tmp/cover.jpg")),
            ..TrackMeta::default()
        };
        assert!(!needs_remote(&full));
    }

    #[test]
    fn apply_peek_fills_empty_slot() {
        let mut slot = None;
        assert!(apply_peek(
            &mut slot,
            TrackMeta {
                artist: "Avicii".into(),
                title: "Levels".into(),
                ..TrackMeta::default()
            }
        ));
        assert_eq!(slot.as_ref().unwrap().artist, "Avicii");
    }

    #[test]
    fn apply_peek_keeps_loaded_lyrics() {
        let mut slot = Some(TrackMeta {
            artist: "Avicii".into(),
            lyrics: vec![LyricLine {
                time: 0.0,
                text: "oh".into(),
            }],
            ..TrackMeta::default()
        });
        assert!(!apply_peek(
            &mut slot,
            TrackMeta {
                artist: "Other".into(),
                ..TrackMeta::default()
            }
        ));
        assert_eq!(slot.as_ref().unwrap().artist, "Avicii");
        assert_eq!(slot.as_ref().unwrap().lyrics.len(), 1);
    }

    #[test]
    fn apply_peek_fills_only_missing_fields() {
        let mut slot = Some(TrackMeta {
            title: "Remote title".into(),
            artist: "Remote artist".into(),
            ..TrackMeta::default()
        });

        assert!(apply_peek(
            &mut slot,
            TrackMeta {
                title: "Filename fallback".into(),
                artist: "Local artist".into(),
                album: "Local album".into(),
                ..TrackMeta::default()
            }
        ));

        let merged = slot.as_ref().unwrap();
        assert_eq!(merged.title, "Remote title");
        assert_eq!(merged.artist, "Remote artist");
        assert_eq!(merged.album, "Local album");
    }

    #[test]
    fn peek_tags_falls_back_to_filename_title() {
        let dir = std::env::temp_dir().join(format!("zradio-peek-fb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Waiting For Love.wav");
        write_silent_wav(&path);

        let meta = peek_tags(&path, "Waiting For Love");
        assert_eq!(meta.title, "Waiting For Love");
        assert!(meta.artist.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
