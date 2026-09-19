use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde::Deserialize;

const BASE: &str = "http://127.0.0.1:25884";
const NETSTART: &str = "https://apis.netstart.cn/music";

#[derive(Debug, Clone)]
pub struct Hit {
    pub source: &'static str,
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
}

#[derive(Deserialize)]
struct NeteaseSearch {
    result: Option<NeteaseResult>,
}

#[derive(Deserialize)]
struct NeteaseResult {
    songs: Option<Vec<NeteaseSong>>,
}

#[derive(Deserialize)]
struct NeteaseSong {
    id: u64,
    name: Option<String>,
    ar: Option<Vec<Named>>,
    al: Option<Named>,
    artists: Option<Vec<Named>>,
    album: Option<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

#[derive(Deserialize)]
struct QqSearch {
    songs: Option<Vec<QqSong>>,
}

#[derive(Deserialize)]
struct QqSong {
    id: Option<serde_json::Value>,
    name: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

pub fn search(keyword: &str, limit: usize) -> anyhow::Result<Vec<Hit>> {
    let q = urlencoding(keyword);
    let mut hits = Vec::new();
    if let Ok(body) = get(&format!("{NETSTART}/search?keywords={q}&limit={limit}")) {
        if let Ok(parsed) = serde_json::from_str::<NeteaseSearch>(&body) {
            for song in parsed.result.and_then(|r| r.songs).unwrap_or_default() {
                hits.push(hit_from_netease("netstart", song));
            }
        }
    }
    if hits.len() < limit {
        if let Ok(body) = get(&format!(
            "{BASE}/api/netease/cloudsearch?keywords={q}&limit={limit}&type=1"
        )) {
            if let Ok(parsed) = serde_json::from_str::<NeteaseSearch>(&body) {
                for song in parsed.result.and_then(|r| r.songs).unwrap_or_default() {
                    hits.push(hit_from_netease("netease", song));
                }
            }
        }
    }
    if hits.len() < limit {
        if let Ok(body) = get(&format!("{BASE}/api/qqmusic/search?keyword={q}")) {
            if let Ok(parsed) = serde_json::from_str::<QqSearch>(&body) {
                for song in parsed.songs.unwrap_or_default() {
                    hits.push(Hit {
                        source: "qq",
                        id: song
                            .id
                            .map(|v| match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            })
                            .unwrap_or_default(),
                        title: song.name.unwrap_or_default(),
                        artist: song.artist.unwrap_or_default(),
                        album: song.album.unwrap_or_default(),
                    });
                }
            }
        }
    }
    hits.truncate(limit.max(1));
    if hits.is_empty() {
        anyhow::bail!("search empty");
    }
    Ok(hits)
}

fn hit_from_netease(source: &'static str, song: NeteaseSong) -> Hit {
    let artist = song
        .ar
        .or(song.artists)
        .unwrap_or_default()
        .iter()
        .filter_map(|a| a.name.clone())
        .collect::<Vec<_>>()
        .join(" / ");
    Hit {
        source,
        id: song.id.to_string(),
        title: song.name.unwrap_or_default(),
        artist,
        album: song
            .al
            .or(song.album)
            .and_then(|a| a.name)
            .unwrap_or_default(),
    }
}

#[derive(Debug, Clone)]
pub struct Login {
    pub logged_in: bool,
    pub vip: bool,
    pub name: String,
}

fn cookie_header() -> Option<String> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let db = home.join(".config/SPlayer/Cookies");
    if !db.is_file() {
        return None;
    }
    let out = Command::new("sqlite3")
        .args([
            "-readonly",
            db.to_str()?,
            "select name || '=' || value from cookies where host_key='localhost' and name in ('MUSIC_U','MUSIC_A_T','MUSIC_R_T','__csrf','NMTID');",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let cookie = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains('=') && !l.ends_with('='))
        .collect::<Vec<_>>()
        .join("; ");
    if cookie.contains("MUSIC_U=") {
        Some(cookie)
    } else {
        None
    }
}

static LOGIN_CACHE: OnceLock<Mutex<Option<(Instant, Login)>>> = OnceLock::new();
const LOGIN_TTL: Duration = Duration::from_secs(30);

pub fn login_status() -> Login {
    let cache = LOGIN_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, login)) = guard.as_ref() {
            if at.elapsed() < LOGIN_TTL {
                return login.clone();
            }
        }
    }
    let login = fetch_login_status();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), login.clone()));
    }
    login
}

fn fetch_login_status() -> Login {
    if cookie_header().is_some() {
        let name = profile_name().unwrap_or_else(|| "splayer".into());
        return Login {
            logged_in: true,
            vip: vip_from_status(),
            name,
        };
    }
    let Ok(body) = get(&format!("{BASE}/api/netease/login/status")) else {
        return Login {
            logged_in: false,
            vip: false,
            name: "splayer offline".into(),
        };
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Login {
            logged_in: false,
            vip: false,
            name: "splayer".into(),
        };
    };
    let data = v.get("data").unwrap_or(&v);
    let account = data.get("account").or_else(|| v.get("account"));
    let profile = data.get("profile").or_else(|| v.get("profile"));
    let anon = account
        .and_then(|a| a.get("anonimousUser"))
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    let vip = account
        .and_then(|a| a.get("vipType"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0)
        > 0;
    let name = profile
        .and_then(|p| p.get("nickname"))
        .and_then(|n| n.as_str())
        .unwrap_or(if anon { "anonymous" } else { "netease" })
        .to_string();
    Login {
        logged_in: !anon && account.is_some() || cookie_header().is_some(),
        vip,
        name,
    }
}

fn profile_name() -> Option<String> {
    let body = get(&format!("{BASE}/api/netease/user/account")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("profile")
        .and_then(|p| p.get("nickname"))
        .and_then(|n| n.as_str())
        .map(ToString::to_string)
}

fn vip_from_status() -> bool {
    get(&format!("{BASE}/api/netease/login/status"))
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .and_then(|v| {
            v.pointer("/data/account/vipType")
                .or_else(|| v.pointer("/account/vipType"))
                .and_then(|x| x.as_i64())
        })
        .unwrap_or(0)
        > 0
}

pub fn download(
    hit: &Hit,
    dest: &PathBuf,
    progress: Option<Sender<String>>,
) -> anyhow::Result<PathBuf> {
    let url = song_url(hit)?;
    std::fs::create_dir_all(dest).ok();
    let ext = url
        .split('?')
        .next()
        .and_then(|u| {
            PathBuf::from(u)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
        })
        .filter(|e| ["mp3", "flac", "m4a", "ogg", "aac"].contains(&e.as_str()))
        .unwrap_or_else(|| "mp3".into());
    let name = sanitize(&format!("{} - {}.{}", hit.artist, hit.title, ext));
    let path = dest.join(name);
    let args = curl_download_args(&url, path.to_str().unwrap_or("song.mp3"));
    let mut child = Command::new("curl")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("curl")?;
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || pump_curl_progress(stderr, progress));
    }
    let status = child.wait().context("curl wait")?;
    if !status.success() {
        anyhow::bail!("download failed");
    }
    Ok(path)
}

fn pump_curl_progress(stderr: impl Read, progress: Option<Sender<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) => {
                    if !buf.is_empty() {
                        emit_curl_progress(&buf, progress.as_ref());
                    }
                    return;
                }
                Ok(_) => {
                    if byte[0] == b'\n' || byte[0] == b'\r' {
                        emit_curl_progress(&buf, progress.as_ref());
                        buf.clear();
                        break;
                    }
                    buf.push(byte[0]);
                    if buf.len() > 256 {
                        emit_curl_progress(&buf, progress.as_ref());
                        buf.clear();
                        break;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

fn emit_curl_progress(buf: &[u8], progress: Option<&Sender<String>>) {
    let line = String::from_utf8_lossy(buf);
    if let Some(pct) = parse_curl_progress(&line) {
        if let Some(tx) = progress {
            let _ = tx.send(format!("{:.0}%", pct * 100.0));
        }
    }
}

fn curl_download_args(url: &str, out: &str) -> Vec<String> {
    vec![
        "-sS".into(),
        "-L".into(),
        "--fail".into(),
        "--progress-bar".into(),
        "-A".into(),
        "Mozilla/5.0".into(),
        "-H".into(),
        "Referer: https://music.163.com/".into(),
        "-o".into(),
        out.into(),
        url.into(),
    ]
}

pub fn parse_curl_progress(line: &str) -> Option<f64> {
    let line = line.trim();
    if !line.contains('%') {
        return None;
    }
    let pct = line.split('%').next()?;
    let num = pct
        .rsplit(|c: char| !(c.is_ascii_digit() || c == '.'))
        .next()?;
    num.parse::<f64>().ok().map(|n| (n / 100.0).clamp(0.0, 1.0))
}

fn song_url(hit: &Hit) -> anyhow::Result<String> {
    let mut errors = Vec::new();
    if hit.source == "netease" || hit.source == "netstart" {
        match get_json_url(&format!("{NETSTART}/song/download/url?id={}", hit.id)) {
            Ok(url) => return Ok(url),
            Err(err) => errors.push(err.to_string()),
        }
        match song_url_netease(&hit.id) {
            Ok(url) => return Ok(url),
            Err(err) => errors.push(err.to_string()),
        }
        match unblock_url(&format!("{BASE}/api/unblock/netease?id={}", hit.id)) {
            Ok(url) => return Ok(url),
            Err(err) => errors.push(err.to_string()),
        }
    }
    let keyword = format!("{}-{}", hit.title, hit.artist);
    let q = urlencoding(&keyword);
    for path in [
        format!(
            "{BASE}/api/unblock/kuwo?keyword={q}&songName={}&artist={}",
            urlencoding(&hit.title),
            urlencoding(&hit.artist)
        ),
        format!(
            "{BASE}/api/unblock/bodian?keyword={q}&songName={}&artist={}",
            urlencoding(&hit.title),
            urlencoding(&hit.artist)
        ),
    ] {
        match unblock_url(&path) {
            Ok(url) => return Ok(url),
            Err(err) => errors.push(err.to_string()),
        }
    }
    anyhow::bail!(
        "{}",
        errors
            .last()
            .cloned()
            .unwrap_or_else(|| "no playable url (copyright)".into())
    )
}

fn song_url_netease(id: &str) -> anyhow::Result<String> {
    for url in [
        format!("{BASE}/api/netease/song/url/v1?id={id}&level=exhigh"),
        format!("{BASE}/api/netease/song/url?id={id}"),
        format!("{BASE}/api/netease/song/download/url?id={id}"),
    ] {
        if let Ok(body) = get(&url) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(found) = extract_url(&v) {
                    return Ok(found);
                }
            }
        }
    }
    anyhow::bail!("netease has no url (copyright)")
}

fn unblock_url(url: &str) -> anyhow::Result<String> {
    let body = get(url)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    extract_url(&v).ok_or_else(|| anyhow!("unblock empty"))
}

fn extract_url(v: &serde_json::Value) -> Option<String> {
    if let Some(url) = v
        .get("url")
        .and_then(|u| u.as_str())
        .filter(|u| u.starts_with("http"))
    {
        return Some(url.to_string());
    }
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        if let Some(url) = arr
            .first()
            .and_then(|s| s.get("url"))
            .and_then(|u| u.as_str())
            .filter(|u| u.starts_with("http"))
        {
            return Some(url.to_string());
        }
    }
    if let Some(url) = v
        .get("data")
        .and_then(|d| d.get("url"))
        .and_then(|u| u.as_str())
        .filter(|u| u.starts_with("http"))
    {
        return Some(url.to_string());
    }
    None
}

fn get_json_url(url: &str) -> anyhow::Result<String> {
    let body = get(url)?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    extract_url(&v).ok_or_else(|| anyhow!("no url from {url}"))
}

fn get(url: &str) -> anyhow::Result<String> {
    let mut args = vec![
        "-sS".into(),
        "--max-time".into(),
        "8".into(),
        "-A".into(),
        "Mozilla/5.0".into(),
        "-H".into(),
        "Accept: application/json".into(),
        "-H".into(),
        "Referer: https://apis.netstart.cn/music/".into(),
    ];
    if url.contains("127.0.0.1:25884") {
        if let Some(cookie) = cookie_header() {
            args.push("-H".into());
            args.push(format!("Cookie: {cookie}"));
        }
    }
    args.push(url.into());
    let out = Command::new("curl")
        .args(args)
        .output()
        .context("curl search")?;
    if !out.status.success() {
        anyhow::bail!("splayer http failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_reads_music_u() {
        if std::path::Path::new("/home/xender/.config/SPlayer/Cookies").is_file() {
            let cookie = cookie_header();
            assert!(cookie.is_some(), "MUSIC_U should exist after SPlayer login");
            assert!(cookie.unwrap().contains("MUSIC_U="));
        }
    }

    #[test]
    fn encode_spaces() {
        assert_eq!(urlencoding("SOS Avicii"), "SOS%20Avicii");
    }

    #[test]
    fn login_status_cached_skips_http_when_cookie_exists() {
        if cookie_header().is_none() {
            return;
        }
        let first = login_status();
        let started = std::time::Instant::now();
        let second = login_status();
        let elapsed = started.elapsed();
        assert_eq!(first.logged_in, second.logged_in);
        assert!(
            elapsed.as_millis() < 50,
            "cached login_status must not block UI, took {elapsed:?}"
        );
    }

    #[test]
    fn login_detects_anonymous() {
        let v = serde_json::json!({
            "data": { "account": { "anonimousUser": true, "vipType": 0 }, "profile": null }
        });
        let account = v["data"]["account"].as_object();
        assert!(account.is_some());
        assert_eq!(v["data"]["account"]["anonimousUser"], true);
    }

    #[test]
    fn extract_url_from_download_payload() {
        let v = serde_json::json!({
            "data": { "url": "http://m701.music.126.net/file.flac" }
        });
        assert_eq!(
            extract_url(&v).as_deref(),
            Some("http://m701.music.126.net/file.flac")
        );
    }

    #[test]
    fn curl_download_is_silent_with_progress_bar() {
        let args = curl_download_args("http://example.com/a.mp3", "/tmp/a.mp3");
        let joined = args.join(" ");
        assert!(
            joined.contains("-sS") || joined.contains("-s"),
            "must not dump meter to TTY"
        );
        assert!(joined.contains("--progress-bar") || joined.contains("-#"));
        assert!(joined.contains("--fail"));
        assert!(joined.contains("-o"));
        assert!(
            !joined.split_whitespace().any(|a| a == "-#")
                || joined.contains("--progress-bar")
                || joined.contains("-sS")
        );
    }

    #[test]
    fn curl_progress_bar_parses_percent() {
        assert!(
            (parse_curl_progress("################################  45.2%").unwrap() - 0.452).abs()
                < 0.001
        );
        assert!(
            (parse_curl_progress(
                "######################################################################## 100.0%"
            )
            .unwrap()
                - 1.0)
                .abs()
                < 0.001
        );
        assert_eq!(parse_curl_progress("resolving netease 123"), None);
    }
}
