use std::path::Path;
use std::process::Command;

use crate::meta::parse_lyrics;

pub fn fetch_lyrics(title: &str, artist: &str) -> Option<String> {
    let (artist, title) = query_artist_title(artist, title);
    if title.trim().is_empty() {
        return None;
    }
    lrclib_only(&title, &artist).or_else(|| fetch_cn_lyrics(&title, &artist))
}

fn lrclib_only(title: &str, artist: &str) -> Option<String> {
    let q = format!(
        "https://lrclib.net/api/search?track_name={}&artist_name={}",
        url_encode(title),
        url_encode(artist)
    );
    curl(&q).and_then(|body| pick_lyrics(&body))
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItunesHit {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub artwork: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteHit {
    pub id: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub artwork: Option<String>,
    pub duration_ms: u32,
    pub netease_id: String,
    pub qq_mid: String,
    pub kugou_hash: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteFill {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub lyrics: Option<String>,
    pub cover: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KugouLyricKey {
    id: String,
    accesskey: String,
}

const NETSTART: &str = "https://apis.netstart.cn/music";

impl From<ItunesHit> for RemoteHit {
    fn from(hit: ItunesHit) -> Self {
        RemoteHit {
            artist: hit.artist,
            title: hit.title,
            album: hit.album,
            artwork: hit.artwork,
            ..RemoteHit::default()
        }
    }
}

pub fn fetch_cover(title: &str, artist: &str, album: &str) -> Option<Vec<u8>> {
    gather_hit(title, artist, album)
        .artwork
        .as_deref()
        .and_then(fetch_bytes)
}

pub fn fetch_remote(
    title: &str,
    artist: &str,
    album: &str,
    want_lyrics: bool,
    want_cover: bool,
) -> RemoteFill {
    let need_tags = artist.trim().is_empty() || album.trim().is_empty();
    let hit = if need_tags || want_cover {
        gather_hit(title, artist, album)
    } else {
        RemoteHit {
            artist: artist.trim().to_string(),
            title: title.trim().to_string(),
            album: album.trim().to_string(),
            ..RemoteHit::default()
        }
    };
    let lyrics = if want_lyrics {
        lrclib_only(&hit.title, &hit.artist)
            .or_else(|| lyrics_from_ids(&hit))
            .or_else(|| fetch_cn_lyrics(&hit.title, &hit.artist))
    } else {
        None
    };
    let cover = if want_cover {
        hit.artwork.as_deref().and_then(fetch_bytes)
    } else {
        None
    };
    RemoteFill {
        artist: nonempty(&hit.artist),
        title: nonempty(&hit.title),
        album: nonempty(&hit.album),
        lyrics,
        cover,
    }
}

pub fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
    curl_bytes(url)
}

pub fn fetch_itunes(title: &str, artist: &str, album: &str) -> Option<ItunesHit> {
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
    parse_itunes_hit(&body)
}

fn parse_itunes_hit(body: &str) -> Option<ItunesHit> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let item = v.get("results")?.as_array()?.first()?;
    let artwork = item
        .get("artworkUrl100")
        .and_then(|s| s.as_str())
        .map(|url| url.replace("100x100bb", "600x600bb"));
    Some(ItunesHit {
        artist: json_str(item, "artistName"),
        title: json_str(item, "trackName"),
        album: json_str(item, "collectionName"),
        artwork,
    })
}

fn gather_hit(title: &str, artist: &str, album: &str) -> RemoteHit {
    let (artist, title) = query_artist_title(artist, title);
    let mut hit = RemoteHit {
        artist: artist.clone(),
        title: title.clone(),
        album: album.trim().to_string(),
        ..RemoteHit::default()
    };
    if let Some(itunes) = fetch_itunes(&title, &artist, album) {
        fill_missing(&mut hit, itunes.into());
    }
    if hit_incomplete(&hit) {
        if let Some(netease) = fetch_netease_hit(&title, &artist) {
            fill_missing(&mut hit, netease);
        }
    }
    if hit_incomplete(&hit) {
        if let Some(qq) = fetch_qq_hit(&title, &artist) {
            fill_missing(&mut hit, qq);
        }
    }
    if hit_incomplete(&hit) {
        if let Some(kugou) = fetch_kugou_hit(&title, &artist) {
            fill_missing(&mut hit, kugou);
        }
    }
    hit
}

fn hit_incomplete(hit: &RemoteHit) -> bool {
    hit.artist.trim().is_empty() || hit.album.trim().is_empty() || hit.artwork.is_none()
}

fn fill_missing(dst: &mut RemoteHit, src: RemoteHit) {
    if dst.artist.trim().is_empty() && !src.artist.trim().is_empty() {
        dst.artist = src.artist;
    }
    if dst.title.trim().is_empty() && !src.title.trim().is_empty() {
        dst.title = src.title;
    }
    if dst.album.trim().is_empty() && !src.album.trim().is_empty() {
        dst.album = src.album;
    }
    if dst.artwork.is_none() {
        dst.artwork = src.artwork;
    }
    if dst.netease_id.is_empty() {
        dst.netease_id = src.netease_id;
    }
    if dst.qq_mid.is_empty() {
        dst.qq_mid = src.qq_mid;
    }
    if dst.kugou_hash.is_empty() {
        dst.kugou_hash = src.kugou_hash;
    }
    if dst.id.is_empty() {
        dst.id = src.id;
    }
    if dst.duration_ms == 0 {
        dst.duration_ms = src.duration_ms;
    }
}

fn lyrics_from_ids(hit: &RemoteHit) -> Option<String> {
    if !hit.netease_id.is_empty() {
        if let Some(lrc) = fetch_netease_lyric(&hit.netease_id) {
            return Some(lrc);
        }
    }
    if !hit.qq_mid.is_empty() {
        if let Some(lrc) = fetch_qq_lyric(&hit.qq_mid) {
            return Some(lrc);
        }
    }
    if !hit.kugou_hash.is_empty() {
        if let Some(lrc) =
            fetch_kugou_lyric(&hit.title, &hit.artist, hit.duration_ms, &hit.kugou_hash)
        {
            return Some(lrc);
        }
    }
    None
}

fn fetch_cn_lyrics(title: &str, artist: &str) -> Option<String> {
    if let Some(hit) = fetch_netease_hit(title, artist) {
        if let Some(lrc) = fetch_netease_lyric(&hit.netease_id) {
            return Some(lrc);
        }
    }
    if let Some(hit) = fetch_qq_hit(title, artist) {
        if let Some(lrc) = fetch_qq_lyric(&hit.qq_mid) {
            return Some(lrc);
        }
    }
    if let Some(hit) = fetch_kugou_hit(title, artist) {
        return fetch_kugou_lyric(title, artist, hit.duration_ms, &hit.kugou_hash);
    }
    None
}

fn fetch_netease_hit(title: &str, artist: &str) -> Option<RemoteHit> {
    let q = search_query(title, artist);
    if q.is_empty() {
        return None;
    }
    let body = curl(&format!(
        "{NETSTART}/search?keywords={}&limit=1",
        url_encode(&q)
    ))?;
    let mut hit = parse_netease_search(&body)?;
    if let Some(detail) = curl(&format!("{NETSTART}/song/detail?ids={}", hit.netease_id))
        .and_then(|body| parse_netease_detail(&body))
    {
        fill_missing(&mut hit, detail);
    }
    Some(hit)
}

fn fetch_netease_lyric(id: &str) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    curl(&format!("{NETSTART}/lyric?id={id}")).and_then(|body| parse_netease_lyric(&body))
}

fn fetch_qq_hit(title: &str, artist: &str) -> Option<RemoteHit> {
    let q = search_query(title, artist);
    if q.is_empty() {
        return None;
    }
    let url = format!(
        "https://c.y.qq.com/splcloud/fcgi-bin/smartbox_new.fcg?key={}&format=json",
        url_encode(&q)
    );
    curl_referer(&url, "https://y.qq.com").and_then(|body| parse_qq_smartbox(&body))
}

fn fetch_qq_lyric(mid: &str) -> Option<String> {
    if mid.is_empty() {
        return None;
    }
    let url = format!(
        "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={mid}&format=json&nobase64=1"
    );
    curl_referer(&url, "https://y.qq.com").and_then(|body| parse_qq_lyric(&body))
}

fn fetch_kugou_hit(title: &str, artist: &str) -> Option<RemoteHit> {
    let q = search_query(title, artist);
    if q.is_empty() {
        return None;
    }
    let url = format!(
        "https://mobileservice.kugou.com/api/v3/search/song?keyword={}&page=1&pagesize=1",
        url_encode(&q)
    );
    curl(&url).and_then(|body| parse_kugou_song(&body))
}

fn fetch_kugou_lyric(title: &str, artist: &str, duration_ms: u32, hash: &str) -> Option<String> {
    let keyword = search_query(title, artist);
    if keyword.is_empty() {
        return None;
    }
    let mut url = format!(
        "http://lyrics.kugou.com/search?ver=1&man=yes&client=pc&keyword={}",
        url_encode(&keyword)
    );
    if duration_ms > 0 {
        url.push_str(&format!("&duration={duration_ms}"));
    }
    if !hash.is_empty() {
        url.push_str(&format!("&hash={hash}"));
    }
    let key = curl(&url).and_then(|body| parse_kugou_lyric_key(&body))?;
    let dl = format!(
        "http://lyrics.kugou.com/download?ver=1&client=pc&id={}&accesskey={}&fmt=lrc&charset=utf8",
        url_encode(&key.id),
        url_encode(&key.accesskey)
    );
    curl(&dl).and_then(|body| parse_kugou_lyric_content(&body))
}

fn parse_netease_search(body: &str) -> Option<RemoteHit> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let song = v.pointer("/result/songs")?.as_array()?.first()?;
    Some(hit_from_netease_song(song))
}

fn parse_netease_detail(body: &str) -> Option<RemoteHit> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let song = v.get("songs")?.as_array()?.first()?;
    Some(hit_from_netease_song(song))
}

fn hit_from_netease_song(song: &serde_json::Value) -> RemoteHit {
    let id = song
        .get("id")
        .and_then(|v| {
            v.as_u64()
                .map(|n| n.to_string())
                .or_else(|| v.as_str().map(ToString::to_string))
        })
        .unwrap_or_default();
    let artist = named_list(song.get("ar").or_else(|| song.get("artists")));
    let album = song
        .get("al")
        .or_else(|| song.get("album"))
        .and_then(|a| a.get("name"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let artwork = song
        .get("al")
        .or_else(|| song.get("album"))
        .and_then(|a| a.get("picUrl"))
        .and_then(|s| s.as_str())
        .map(ToString::to_string);
    RemoteHit {
        id: id.clone(),
        netease_id: id,
        artist,
        title: json_str(song, "name"),
        album,
        artwork,
        ..RemoteHit::default()
    }
}

fn parse_netease_lyric(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let lyric = v
        .pointer("/lrc/lyric")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(lyric.to_string())
}

fn parse_qq_smartbox(body: &str) -> Option<RemoteHit> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let song = v.pointer("/data/song/itemlist")?.as_array()?.first()?;
    let title = json_str(song, "name");
    let artist = json_str(song, "singer");
    let mid = json_str(song, "mid");
    if mid.is_empty() || title.is_empty() {
        return None;
    }
    let albums = v.pointer("/data/album/itemlist").and_then(|a| a.as_array());
    let album_item = albums.and_then(|items| {
        items
            .iter()
            .find(|item| json_str(item, "singer").eq_ignore_ascii_case(&artist))
    });
    let album = album_item
        .map(|item| json_str(item, "name"))
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let artwork = album_item
        .and_then(|item| item.get("pic"))
        .and_then(|s| s.as_str())
        .map(|url| url.replace("T002R180x180", "T002R800x800"));
    Some(RemoteHit {
        id: mid.clone(),
        qq_mid: mid,
        artist,
        title,
        album,
        artwork,
        ..RemoteHit::default()
    })
}

fn parse_qq_lyric(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let lyric = v
        .get("lyric")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(lyric.to_string())
}

fn parse_kugou_song(body: &str) -> Option<RemoteHit> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let song = v.pointer("/data/info")?.as_array()?.first()?;
    let hash = json_str(song, "hash");
    if hash.is_empty() {
        return None;
    }
    let duration_ms = song
        .get("duration")
        .and_then(|d| d.as_u64())
        .unwrap_or(0)
        .saturating_mul(1000) as u32;
    let artwork = song
        .pointer("/trans_param/union_cover")
        .and_then(|s| s.as_str())
        .map(|url| url.replace("{size}", "400"));
    Some(RemoteHit {
        id: hash.clone(),
        kugou_hash: hash,
        artist: json_str(song, "singername"),
        title: json_str(song, "songname"),
        album: json_str(song, "album_name"),
        artwork,
        duration_ms,
        ..RemoteHit::default()
    })
}

fn parse_kugou_lyric_key(body: &str) -> Option<KugouLyricKey> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let cand = v.get("candidates")?.as_array()?.first()?;
    let id = json_str(cand, "id");
    let accesskey = json_str(cand, "accesskey");
    if id.is_empty() || accesskey.is_empty() {
        return None;
    }
    Some(KugouLyricKey { id, accesskey })
}

fn parse_kugou_lyric_content(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let encoded = v
        .get("content")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let bytes = decode_base64(encoded)?;
    let text = String::from_utf8_lossy(&bytes);
    Some(text.trim_start_matches('\u{feff}').to_string())
}

fn named_list(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("name").and_then(|s| s.as_str()))
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default()
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn nonempty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn search_query(title: &str, artist: &str) -> String {
    [artist, title]
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [0xffu8; 256];
    for (i, b) in TABLE.iter().enumerate() {
        table[*b as usize] = i as u8;
    }
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let b0 = table[chunk[0] as usize];
        let b1 = table[chunk[1] as usize];
        if b0 == 0xff || b1 == 0xff {
            return None;
        }
        out.push((b0 << 2) | (b1 >> 4));
        if chunk.len() > 2 && chunk[2] != b'=' {
            let b2 = table[chunk[2] as usize];
            if b2 == 0xff {
                return None;
            }
            out.push((b1 << 4) | (b2 >> 2));
            if chunk.len() > 3 && chunk[3] != b'=' {
                let b3 = table[chunk[3] as usize];
                if b3 == 0xff {
                    return None;
                }
                out.push((b2 << 6) | b3);
            }
        }
    }
    Some(out)
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
    curl_referer(url, "")
}

fn curl_referer(url: &str, referer: &str) -> Option<String> {
    let mut args = vec![
        "-sS".into(),
        "--max-time".into(),
        "6".into(),
        "-A".into(),
        "zradio/0.1".into(),
        "-L".into(),
    ];
    if !referer.is_empty() {
        args.push("-H".into());
        args.push(format!("Referer: {referer}"));
    }
    args.push(url.into());
    let out = Command::new("curl").args(args).output().ok()?;
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
            parse_itunes_hit(body).and_then(|h| h.artwork).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Music/aa/bb/cc/600x600bb.jpg")
        );
    }

    #[test]
    fn itunes_artwork_empty_when_no_results() {
        assert_eq!(parse_itunes_hit(r#"{"results":[]}"#), None);
    }

    #[test]
    fn itunes_hit_reads_artist_album_title() {
        let body = r#"{"results":[{"artistName":"Avicii","trackName":"Levels","collectionName":"True","artworkUrl100":"https://x/100x100bb.jpg"}]}"#;
        let hit = parse_itunes_hit(body).unwrap();
        assert_eq!(hit.artist, "Avicii");
        assert_eq!(hit.title, "Levels");
        assert_eq!(hit.album, "True");
        assert_eq!(hit.artwork.as_deref(), Some("https://x/600x600bb.jpg"));
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

    #[test]
    fn netease_search_reads_id_and_tags() {
        let body = r#"{"result":{"songs":[{"id":1357375695,"name":"海阔天空","artists":[{"name":"Beyond"}],"album":{"name":"乐与怒"}}]}}"#;
        let hit = parse_netease_search(body).unwrap();
        assert_eq!(hit.id, "1357375695");
        assert_eq!(hit.artist, "Beyond");
        assert_eq!(hit.title, "海阔天空");
        assert_eq!(hit.album, "乐与怒");
        assert!(hit.artwork.is_none());
    }

    #[test]
    fn netease_search_empty_is_none() {
        assert_eq!(parse_netease_search(r#"{"result":{"songs":[]}}"#), None);
        assert_eq!(parse_netease_search(r#"{}"#), None);
    }

    #[test]
    fn netease_detail_reads_cover() {
        let body = r#"{"songs":[{"name":"海阔天空","ar":[{"name":"Beyond"}],"al":{"name":"乐与怒","picUrl":"https://p2.music.126.net/cover.jpg"}}]}"#;
        let hit = parse_netease_detail(body).unwrap();
        assert_eq!(hit.artist, "Beyond");
        assert_eq!(hit.album, "乐与怒");
        assert_eq!(
            hit.artwork.as_deref(),
            Some("https://p2.music.126.net/cover.jpg")
        );
    }

    #[test]
    fn netease_lyric_reads_lrc() {
        let body = r#"{"lrc":{"lyric":"[00:00.00] 作词 : 黄家驹\n[00:18.85]今天我"}}"#;
        assert_eq!(
            parse_netease_lyric(body).as_deref(),
            Some("[00:00.00] 作词 : 黄家驹\n[00:18.85]今天我")
        );
        assert_eq!(parse_netease_lyric(r#"{"lrc":{"lyric":""}}"#), None);
    }

    #[test]
    fn qq_smartbox_reads_song_and_matching_album_art() {
        let body = r#"{"code":0,"data":{"song":{"itemlist":[{"mid":"001yS0N33yPm1B","name":"海阔天空","singer":"BEYOND"}]},"album":{"itemlist":[{"name":"海阔天空","singer":"信乐团","pic":"http://y.gtimg.cn/music/photo_new/T002R180x180M000wrong.jpg"},{"name":"乐与怒","singer":"BEYOND","pic":"http://y.gtimg.cn/music/photo_new/T002R180x180M000004CLlFV0mj6fC_2.jpg"}]}}}"#;
        let hit = parse_qq_smartbox(body).unwrap();
        assert_eq!(hit.id, "001yS0N33yPm1B");
        assert_eq!(hit.artist, "BEYOND");
        assert_eq!(hit.title, "海阔天空");
        assert_eq!(hit.album, "乐与怒");
        assert_eq!(
            hit.artwork.as_deref(),
            Some("http://y.gtimg.cn/music/photo_new/T002R800x800M000004CLlFV0mj6fC_2.jpg")
        );
    }

    #[test]
    fn qq_lyric_reads_lrc() {
        let body = r#"{"retcode":0,"lyric":"[ti:海阔天空]\n[ar:BEYOND]\n[00:19.34]今天我"}"#;
        assert_eq!(
            parse_qq_lyric(body).as_deref(),
            Some("[ti:海阔天空]\n[ar:BEYOND]\n[00:19.34]今天我")
        );
        assert_eq!(parse_qq_lyric(r#"{"retcode":-1310}"#), None);
    }

    #[test]
    fn kugou_song_reads_tags_hash_and_cover() {
        let body = r#"{"status":1,"data":{"info":[{"hash":"c41e80a18d1448fa47086372999c7f43","songname":"海阔天空","singername":"BEYOND","album_name":"乐与怒","duration":324,"trans_param":{"union_cover":"http://imge.kugou.com/stdmusic/{size}/cover.jpg"}}]}}"#;
        let hit = parse_kugou_song(body).unwrap();
        assert_eq!(hit.id, "c41e80a18d1448fa47086372999c7f43");
        assert_eq!(hit.artist, "BEYOND");
        assert_eq!(hit.title, "海阔天空");
        assert_eq!(hit.album, "乐与怒");
        assert_eq!(hit.duration_ms, 324_000);
        assert_eq!(
            hit.artwork.as_deref(),
            Some("http://imge.kugou.com/stdmusic/400/cover.jpg")
        );
    }

    #[test]
    fn kugou_lyric_key_reads_id_and_accesskey() {
        let body = r#"{"status":200,"candidates":[{"id":"346467937","accesskey":"26E1C38D2620C85B154DB097AFDDCEC6","song":"海阔天空"}]}"#;
        let key = parse_kugou_lyric_key(body).unwrap();
        assert_eq!(key.id, "346467937");
        assert_eq!(key.accesskey, "26E1C38D2620C85B154DB097AFDDCEC6");
        assert_eq!(
            parse_kugou_lyric_key(r#"{"status":200,"candidates":[]}"#),
            None
        );
    }

    #[test]
    fn kugou_lyric_content_decodes_base64_and_strips_bom() {
        let body = r#"{"status":200,"content":"77u/WzAwOjAxLjAwXWhp"}"#;
        assert_eq!(
            parse_kugou_lyric_content(body).as_deref(),
            Some("[00:01.00]hi")
        );
        assert_eq!(
            parse_kugou_lyric_content(r#"{"status":200,"content":""}"#),
            None
        );
    }

    #[test]
    fn fill_missing_keeps_existing_and_fills_empty() {
        let mut hit = RemoteHit {
            artist: "Beyond".into(),
            title: "海阔天空".into(),
            album: String::new(),
            artwork: None,
            ..RemoteHit::default()
        };
        fill_missing(
            &mut hit,
            RemoteHit {
                artist: "信乐团".into(),
                title: "海阔天空".into(),
                album: "乐与怒".into(),
                artwork: Some("http://cover.jpg".into()),
                ..RemoteHit::default()
            },
        );
        assert_eq!(hit.artist, "Beyond");
        assert_eq!(hit.title, "海阔天空");
        assert_eq!(hit.album, "乐与怒");
        assert_eq!(hit.artwork.as_deref(), Some("http://cover.jpg"));
    }
}
