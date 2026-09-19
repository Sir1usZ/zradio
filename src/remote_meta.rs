use std::path::Path;
use std::process::Command;

use crate::meta::parse_lyrics;

pub fn fetch_lyrics(title: &str, artist: &str) -> Option<String> {
    let (artist, title) = query_artist_title(artist, title);
    if title.trim().is_empty() {
        return None;
    }
    let q = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        url_encode(&title),
        url_encode(&artist)
    );
    let body = curl(&q)?;
    pick_lyrics(&body)
}

fn query_artist_title(artist: &str, title: &str) -> (String, String) {
    let artist = artist.trim();
    let title = title.trim();
    if artist.is_empty() {
        return split_artist_title(title);
    }
    if title.contains(" - ") && title.to_lowercase().starts_with(&artist.to_lowercase()) {
        return split_artist_title(title);
    }
    (artist.to_string(), title.to_string())
}

fn split_artist_title(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some((a, t)) = raw.split_once(" - ") {
        let a = a.trim();
        let t = t.trim();
        if !a.is_empty() && !t.is_empty() {
            return (a.to_string(), t.to_string());
        }
    }
    (String::new(), raw.to_string())
}

fn pick_lyrics(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let items = v.as_array()?;
    for item in items {
        if let Some(s) = item.get("syncedLyrics").and_then(|s| s.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    for item in items {
        if let Some(s) = item.get("plainLyrics").and_then(|s| s.as_str()) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

pub fn lyrics_or_empty(title: &str, artist: &str) -> Vec<crate::meta::LyricLine> {
    fetch_lyrics(title, artist)
        .map(|raw| parse_lyrics(&raw))
        .unwrap_or_default()
}

pub fn fetch_cover(title: &str, artist: &str, album: &str) -> Option<Vec<u8>> {
    let (artist, title) = query_artist_title(artist, title);
    let query = [&artist, album, &title]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if query.trim().is_empty() {
        return None;
    }
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=song&media=music&limit=1",
        url_encode(&query)
    );
    let body = curl(&url)?;
    let art = parse_itunes_artwork(&body)?;
    curl_bytes(&art)
}

fn parse_itunes_artwork(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let url = v
        .get("results")?
        .as_array()?
        .first()?
        .get("artworkUrl100")?
        .as_str()?;
    Some(url.replace("100x100bb", "600x600bb"))
}

fn curl_bytes(url: &str) -> Option<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "8", "-A", "zradio/0.1", "-L", url])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

fn curl(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "6", "-A", "zradio/0.1", url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn save_sidecar_lrc(track: &Path, lyrics: &str) {
    let Some(dir) = track.parent() else {
        return;
    };
    let Some(stem) = track.file_stem() else {
        return;
    };
    let folder = dir.join("lrc");
    let _ = std::fs::create_dir_all(&folder);
    let _ = std::fs::write(
        folder.join(format!("{}.lrc", stem.to_string_lossy())),
        lyrics,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_spaces() {
        assert_eq!(url_encode("SOS Avicii"), "SOS%20Avicii");
    }

    #[test]
    fn itunes_artwork_upgrades_100_to_600() {
        let body = r#"{"results":[{"artworkUrl100":"https://is1-ssl.mzstatic.com/image/thumb/Music/aa/bb/cc/100x100bb.jpg"}]}"#;
        assert_eq!(
            parse_itunes_artwork(body).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Music/aa/bb/cc/600x600bb.jpg")
        );
    }

    #[test]
    fn itunes_artwork_empty_when_no_results() {
        assert_eq!(parse_itunes_artwork(r#"{"results":[]}"#), None);
    }

    #[test]
    fn lyrics_skip_null_synced_and_use_plain() {
        let body = r#"[{"name":"bad","syncedLyrics":null,"plainLyrics":"hello world"},{"name":"good","syncedLyrics":"[00:01]hi","plainLyrics":"hi"}]"#;
        assert_eq!(pick_lyrics(body).as_deref(), Some("[00:01]hi"));
        let only_plain = r#"[{"syncedLyrics":null,"plainLyrics":"plain only"}]"#;
        assert_eq!(pick_lyrics(only_plain).as_deref(), Some("plain only"));
        assert_eq!(pick_lyrics(r#"[]"#), None);
    }

    #[test]
    fn split_filename_artist_title() {
        assert_eq!(
            split_artist_title("Michael Jackson - We Are the World (Demo)"),
            ("Michael Jackson".into(), "We Are the World (Demo)".into())
        );
        assert_eq!(
            split_artist_title("We Are the World (Demo)"),
            ("".into(), "We Are the World (Demo)".into())
        );
    }
}
