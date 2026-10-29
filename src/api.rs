use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::control::ControlCmd;
use crate::engine::Snapshot;

const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;

const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

fn request_body_size_allowed(content_length: usize) -> bool {
    content_length <= MAX_REQUEST_BODY_BYTES
}

fn read_head_line<R: BufRead>(
    reader: &mut R,
    budget: &mut usize,
) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let n = reader
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if n > *budget {
        return Ok(None);
    }
    *budget -= n;
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn request_read_timeout() -> Duration {
    Duration::from_secs(5)
}

// ── 共享状态：App 每个 tick 更新，API 线程读取 ────────────────────

#[derive(Debug, Clone, Default)]
pub struct ApiSnapshot {
    pub snapshot: Option<Snapshot>,
    pub track: Option<TrackInfo>,
    pub tracks_total: usize,
    pub eq: [f32; 5],
    pub taste_top: Vec<TasteEntry>,
    pub total_listen_secs: u64,
    /// 完整曲目列表（按序号）
    pub library: Vec<TrackInfo>,
    /// 偏好设置快照
    pub prefs: Option<PrefsSnapshot>,
    /// 当前歌词（时间戳行，播放器内部用）
    pub lyrics: Vec<LyricLine>,
    /// 全库完整 LRC 文本，下标对齐曲目序号；空字符串表示没有歌词
    pub lyrics_lrc: Vec<String>,
    /// 封面缓存路径，下标对齐曲目序号；按需读盘，不进 /library/full
    pub cover_paths: Vec<Option<PathBuf>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TrackInfo {
    pub index: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub has_cover: bool,
    pub has_lyrics: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TasteEntry {
    pub title: String,
    pub artist: String,
    pub score: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LyricLine {
    pub time: f32,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PrefsSnapshot {
    pub theme: String,
    pub transparent: bool,
    pub visualize: String,
    pub lyrics_fetch: bool,
    pub cover_fetch: bool,
    pub resume: bool,
    pub library: String,
    pub sort_mode: String,
    pub shuffle_mode: String,
}

pub type SharedState = Arc<Mutex<ApiSnapshot>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(ApiSnapshot::default()))
}

// ── 请求类型 ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ControlRequest {
    #[serde(default)]
    action: String,
    #[serde(default)]
    value: Option<f32>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ImportRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
struct EqRequest {
    band: usize,
    db: f32,
}

// ── HTTP 服务器 ────────────────────────────────────────────────────

pub const PORT: u16 = 18765;

/// 正在运行的 API 服务。默认只听本机，`set_lan(true)` 后改听局域网。
pub struct ApiServer {
    port: u16,
    lan: Arc<AtomicBool>,
    thread: thread::Thread,
}

impl ApiServer {
    pub fn set_lan(&self, lan: bool) {
        if self.lan.swap(lan, Ordering::SeqCst) == lan {
            return;
        }
        // 叫醒服务线程：绑定失败时它在 park，正常时它卡在 accept。
        self.thread.unpark();
        let local = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let _ = TcpStream::connect_timeout(&local, Duration::from_millis(200));
    }
}

pub fn start(lan: bool, tx: Sender<ControlCmd>, state: SharedState) -> ApiServer {
    start_on(PORT, lan, token_path(), tx, state)
}

fn start_on(
    port: u16,
    lan: bool,
    token_path: PathBuf,
    tx: Sender<ControlCmd>,
    state: SharedState,
) -> ApiServer {
    let lan = Arc::new(AtomicBool::new(lan));
    let flag = Arc::clone(&lan);
    let handle = thread::spawn(move || serve(port, &flag, &token_path, &tx, &state));
    ApiServer {
        port,
        lan,
        thread: handle.thread().clone(),
    }
}

fn listen_addr(lan: bool, port: u16) -> SocketAddr {
    let ip = if lan {
        Ipv4Addr::UNSPECIFIED
    } else {
        Ipv4Addr::LOCALHOST
    };
    SocketAddr::from((ip, port))
}

fn serve(
    port: u16,
    lan: &AtomicBool,
    token_path: &Path,
    tx: &Sender<ControlCmd>,
    state: &SharedState,
) {
    loop {
        let want = lan.load(Ordering::SeqCst);
        let addr = listen_addr(want, port);
        let listener = match TcpListener::bind(addr) {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("[api] 绑定 {addr} 失败: {e}");
                while lan.load(Ordering::SeqCst) == want {
                    thread::park();
                }
                continue;
            }
        };
        eprintln!("[api] 远程控制监听 {addr}");
        let token: Option<Arc<str>> = load_token(token_path, want).map(Arc::from);
        if want && token.is_none() {
            eprintln!("[api] 没有可用的 token，局域网请求会被拒绝");
        }
        for stream in listener.incoming().flatten() {
            if lan.load(Ordering::SeqCst) != want {
                break;
            }
            let Ok(peer) = stream.peer_addr() else {
                continue;
            };
            let tx = tx.clone();
            let state = Arc::clone(state);
            let token = token.clone();
            thread::spawn(move || {
                if let Err(e) = handle_connection(stream, peer.ip(), token.as_deref(), &tx, &state)
                {
                    eprintln!("[api] 连接错误: {e}");
                }
            });
        }
    }
}

/// 局域网客户端要带的 token 存在这里，权限 0600。
pub fn token_path() -> PathBuf {
    crate::prefs::config_dir().join("api-token")
}

/// 读出 token；文件不存在或为空且 `create` 时生成一个新的写进去。
fn load_token(path: &Path, create: bool) -> Option<String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let existing = existing.trim();
    if !existing.is_empty() {
        return Some(existing.to_string());
    }
    if !create {
        return None;
    }
    let token = random_token()?;
    match write_private(path, &format!("{token}\n")) {
        Ok(()) => Some(token),
        Err(e) => {
            eprintln!("[api] 写 {} 失败: {e}", path.display());
            None
        }
    }
}

fn random_token() -> Option<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // 文件已存在时 mode() 不生效，先收紧权限再写内容
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(contents.as_bytes())
}

/// 本机来的请求直接放行；别的机器必须带 `Authorization: Bearer <token>`。
fn authorized(peer: IpAddr, bearer: Option<&str>, token: Option<&str>) -> bool {
    if peer.to_canonical().is_loopback() {
        return true;
    }
    match (bearer, token) {
        (Some(bearer), Some(token)) if !token.is_empty() => {
            constant_time_eq(bearer.as_bytes(), token.as_bytes())
        }
        _ => false,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer_token(authorization: &str) -> Option<&str> {
    let (scheme, token) = authorization.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

fn handle_connection(
    mut stream: TcpStream,
    peer: IpAddr,
    token: Option<&str>,
    tx: &Sender<ControlCmd>,
    state: &SharedState,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    reader.set_read_timeout(Some(request_read_timeout()))?;
    let mut buf_reader = BufReader::new(reader);

    let mut budget = MAX_REQUEST_HEAD_BYTES;
    let Some(request_line) = read_head_line(&mut buf_reader, &mut budget)? else {
        return send_json(&mut stream, 431, &err("request head too large"));
    };
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return send_json(&mut stream, 400, &err("bad request"));
    }

    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    let mut host = None;
    let mut origin = None;
    let mut authorization = None;
    loop {
        let Some(line) = read_head_line(&mut buf_reader, &mut budget)? else {
            return send_json(&mut stream, 431, &err("request head too large"));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "host" => host = Some(value.to_string()),
            "origin" => origin = Some(value.to_string()),
            "authorization" => authorization = Some(value.to_string()),
            _ => {}
        }
    }

    if !request_body_size_allowed(content_length) {
        return send_json(&mut stream, 413, &err("request body too large"));
    }

    let body = if content_length > 0 {
        let mut body_buf = vec![0u8; content_length];
        buf_reader.read_exact(&mut body_buf)?;
        String::from_utf8_lossy(&body_buf).into_owned()
    } else {
        String::new()
    };

    let bearer = authorization.as_deref().and_then(bearer_token);
    if !authorized(peer, bearer, token) {
        return send_json(&mut stream, 401, &err("missing or wrong API token"));
    }

    if !origin_allowed(origin.as_deref(), host.as_deref()) {
        return send_json(
            &mut stream,
            403,
            &err("cross-origin requests are not allowed"),
        );
    }

    if method == "OPTIONS" {
        return send_options(&mut stream);
    }

    if method == "GET" && path.starts_with("/cover/") {
        return match cover_bytes_for(path, state) {
            Some((mime, bytes)) => send_bytes(&mut stream, 200, mime, &bytes),
            None => send_json(&mut stream, 200, &err("cover not found")),
        };
    }

    let response = route(method, path, &body, tx, state);
    send_json(&mut stream, 200, &response)
}

/// 浏览器给跨站请求带 `Origin`。只放行和 `Host` 同源的页面，
/// 这样别的网站既读不到资料库，也不能用表单 POST 控制播放。
/// curl、MCP 这类非浏览器客户端不带 `Origin`，照常放行。
fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    match (authority, host) {
        (Some(authority), Some(host)) => authority.eq_ignore_ascii_case(host),
        _ => false,
    }
}

fn route(
    method: &str,
    path: &str,
    body: &str,
    tx: &Sender<ControlCmd>,
    state: &SharedState,
) -> serde_json::Value {
    // ── GET ────────────────────────────────────────────────────
    if method == "GET" {
        return match path {
            // 状态
            "/status" => handle_status(state),
            "/health" => ok(serde_json::json!({"version": "0.1.0", "name": "zradio"})),
            // 资料库
            "/library" => handle_library(state),
            "/library/full" => handle_library_full(state),
            // 曲目
            p if p.starts_with("/track/") => handle_track(p, state),
            p if p.starts_with("/cover/") => handle_cover(p, state),
            p if p.starts_with("/lyrics/") => handle_lyrics(p, state),
            // 模式查询
            "/shuffle" => handle_get_shuffle(state),
            "/loop" => handle_get_loop(state),
            "/mix" => handle_get_mix(state),
            "/remix" => handle_get_remix(state),
            "/sort" => handle_get_sort(state),
            // 口味统计
            "/taste" => handle_taste(state),
            // 偏好设置
            "/prefs" => handle_prefs(state),
            "/eq" => handle_get_eq(state),
            _ => err("not found"),
        };
    }

    // ── POST ───────────────────────────────────────────────────
    if method == "POST" {
        return match path {
            // 播放控制
            "/control" => handle_control(body, tx, state),
            // 资料库操作
            "/import" => handle_import(body, tx),
            "/library/scan" => handle_library_scan(tx),
            "/meta/scan" => handle_meta_scan(tx),
            // EQ
            "/eq" => handle_eq(body, tx),
            "/eq/reset" => handle_eq_reset(tx),
            // 搜索
            "/search" => handle_search(body, state),
            _ => err("not found"),
        };
    }

    err("method not allowed")
}

// ══════════════════════════════════════════════════════════════════
//  GET 处理函数
// ══════════════════════════════════════════════════════════════════

fn handle_status(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    let playback = snap.snapshot.as_ref().map(|s| {
        let sr = s.sample_rate.max(1);
        serde_json::json!({
            "paused": s.paused,
            "position_secs": (s.position_frames as f32 / sr as f32 * 10.0).round() / 10.0,
            "duration_secs": (s.duration_frames as f32 / sr as f32 * 10.0).round() / 10.0,
            "volume": (s.volume * 100.0).round() as u32,
            "mix_mode": format!("{:?}", s.mix).to_lowercase(),
            "remix_mode": s.remix.label().to_string(),
            "shuffle": s.shuffle,
            "loop_mode": s.loop_mode.label().to_string(),
            "status": s.status.clone(),
            "fading": s.fading,
            "bpm": s.current_bpm,
            "key": s.current_key.clone(),
            "next_hint": s.next_hint.clone(),
            "sample_rate": s.sample_rate,
        })
    });

    let prefs = snap.prefs.as_ref().map(|p| {
        serde_json::json!({
            "theme": p.theme,
            "transparent": p.transparent,
            "visualize": p.visualize,
            "lyrics_fetch": p.lyrics_fetch,
            "cover_fetch": p.cover_fetch,
            "resume": p.resume,
            "sort_mode": p.sort_mode,
            "shuffle_mode": p.shuffle_mode,
        })
    });

    ok(serde_json::json!({
        "playback": playback.unwrap_or(serde_json::json!({
            "paused": true, "position_secs": 0.0, "duration_secs": 0.0,
            "volume": 90, "mix_mode": "crossfade", "remix_mode": "RAW",
            "shuffle": false, "shuffle_mode": "off", "loop_mode": "OFF",
            "status": "idle", "fading": false,
            "bpm": null, "key": null, "next_hint": null, "sample_rate": 0,
        })),
        "current_track": snap.track,
        "eq": {
            "bands": ["60", "250", "1k", "4k", "12k"],
            "db": snap.eq,
        },
        "library": {
            "total": snap.tracks_total,
        },
        "taste": {
            "total_listen_secs": snap.total_listen_secs,
            "top_tracks": snap.taste_top,
        },
        "prefs": prefs,
    }))
}

// ── 资料库 ─────────────────────────────────────────────────────

fn handle_library(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    ok(serde_json::json!({
        "total": snap.tracks_total,
        "current_track": snap.track,
    }))
}

fn handle_library_full(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    ok(serde_json::json!({
        "total": snap.library.len(),
        "tracks": snap.library,
    }))
}

// ── 曲目详情 ───────────────────────────────────────────────────

fn handle_track(path: &str, state: &SharedState) -> serde_json::Value {
    let index = path
        .trim_start_matches("/track/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());

    let index = match index {
        Some(i) => i,
        None => return err("invalid index"),
    };

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    // 优先返回当前播放曲目
    if let Some(ref track) = snap.track {
        if track.index == index {
            return ok(serde_json::json!({
                "info": track,
                "bpm": snap.snapshot.as_ref().and_then(|s| s.current_bpm),
                "key": snap.snapshot.as_ref().and_then(|s| s.current_key.clone()),
                "cover": cover_json_at(index, &snap),
            }));
        }
    }

    // 从完整资料库中查找
    if let Some(track) = snap.library.iter().find(|t| t.index == index) {
        return ok(serde_json::json!({
            "info": track,
            "cover": cover_json_at(index, &snap),
        }));
    }

    err("track not found")
}

fn handle_cover(path: &str, state: &SharedState) -> serde_json::Value {
    let index = path
        .trim_start_matches("/cover/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    let idx = index.or(snap.track.as_ref().map(|t| t.index));
    let Some(idx) = idx else {
        return err("track not found");
    };
    match cover_bytes_at(idx, &snap) {
        Some((mime, bytes)) => ok(serde_json::json!({
            "index": idx,
            "has_cover": true,
            "mime": mime,
            "data": encode_base64(&bytes),
        })),
        None => {
            if snap.library.iter().any(|t| t.index == idx) {
                ok(serde_json::json!({
                    "index": idx,
                    "has_cover": false,
                    "mime": serde_json::Value::Null,
                    "data": serde_json::Value::Null,
                }))
            } else {
                err("track not found")
            }
        }
    }
}

fn handle_lyrics(path: &str, state: &SharedState) -> serde_json::Value {
    let index = path
        .trim_start_matches("/lyrics/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    let idx = match index.or(snap.track.as_ref().map(|t| t.index)) {
        Some(i) => i,
        None => return err("track not found"),
    };
    if snap.library.iter().all(|t| t.index != idx) && snap.lyrics_lrc.get(idx).is_none() {
        return err("track not found");
    }
    let lrc = snap.lyrics_lrc.get(idx).cloned().unwrap_or_default();
    ok(serde_json::json!({
        "index": idx,
        "lrc": lrc,
        "has_lyrics": !lrc.is_empty(),
    }))
}

// ── 模式查询 ───────────────────────────────────────────────────

fn handle_get_shuffle(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    let shuffle = snap.snapshot.as_ref().map(|s| s.shuffle).unwrap_or(false);
    let mode = snap
        .prefs
        .as_ref()
        .map(|p| p.shuffle_mode.clone())
        .unwrap_or_else(|| "off".into());
    ok(serde_json::json!({
        "shuffle": shuffle,
        "mode": mode,
        "modes": ["off", "random", "norepeat", "taste"],
    }))
}

fn handle_get_loop(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    let mode = snap
        .snapshot
        .as_ref()
        .map(|s| s.loop_mode.label().to_lowercase())
        .unwrap_or_else(|| "off".into());
    ok(serde_json::json!({
        "mode": mode,
        "modes": ["off", "one", "all"],
    }))
}

fn handle_get_mix(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    let mode = snap
        .snapshot
        .as_ref()
        .map(|s| format!("{:?}", s.mix).to_lowercase())
        .unwrap_or_else(|| "crossfade".into());
    ok(serde_json::json!({
        "mode": mode,
        "modes": ["cut", "crossfade", "automix"],
    }))
}

fn handle_get_remix(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    let mode = snap
        .snapshot
        .as_ref()
        .map(|s| s.remix.label().to_string())
        .unwrap_or_else(|| "RAW".into());
    ok(serde_json::json!({
        "mode": mode,
        "modes": ["off", "chill", "club", "ncore"],
    }))
}

fn handle_get_sort(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    let mode = snap
        .prefs
        .as_ref()
        .map(|p| p.sort_mode.clone())
        .unwrap_or_else(|| "path".into());
    ok(serde_json::json!({
        "mode": mode,
        "modes": ["path", "title", "artist", "album"],
    }))
}

// ── 口味统计 ───────────────────────────────────────────────────

fn handle_taste(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    ok(serde_json::json!({
        "total_listen_secs": snap.total_listen_secs,
        "total_listen_hours": (snap.total_listen_secs as f64 / 3600.0 * 10.0).round() / 10.0,
        "top_tracks": snap.taste_top,
    }))
}

// ── 偏好设置 ───────────────────────────────────────────────────

fn handle_prefs(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    if let Some(ref prefs) = snap.prefs {
        return ok(serde_json::json!({
            "theme": prefs.theme,
            "transparent": prefs.transparent,
            "visualize": prefs.visualize,
            "lyrics_fetch": prefs.lyrics_fetch,
            "cover_fetch": prefs.cover_fetch,
            "resume": prefs.resume,
            "library": prefs.library,
            "sort_mode": prefs.sort_mode,
            "shuffle_mode": prefs.shuffle_mode,
            "themes": ["system", "latte", "frappe", "macchiato", "mocha"],
            "visualizes": ["off", "bars", "scope", "cnm"],
            "sorts": ["path", "title", "artist", "album"],
            "shuffles": ["off", "random", "norepeat", "taste"],
        }));
    }
    err("prefs not loaded")
}

fn handle_get_eq(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    ok(serde_json::json!({
        "bands": ["60", "250", "1k", "4k", "12k"],
        "db": snap.eq,
    }))
}

// ══════════════════════════════════════════════════════════════════
//  POST 处理函数
// ══════════════════════════════════════════════════════════════════

fn handle_control(body: &str, tx: &Sender<ControlCmd>, state: &SharedState) -> serde_json::Value {
    let req: ControlRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };

    let cmd = match req.action.as_str() {
        // ── 播放控制 ──
        "play" => {
            if let Some(idx) = req.index {
                ControlCmd::PlayIndex(idx)
            } else {
                ControlCmd::Play
            }
        }
        "play_path" => {
            let path = match req.path {
                Some(p) => p,
                None => return err("missing path field"),
            };
            ControlCmd::PlayPath(path)
        }
        "pause" => ControlCmd::Pause,
        "toggle" => ControlCmd::TogglePause,
        "next" => ControlCmd::Next,
        "prev" => ControlCmd::Prev,

        // ── 跳转 ──
        "seek" => ControlCmd::Seek(req.value.unwrap_or(0.0)),
        "seek_to" => ControlCmd::SeekTo(req.value.unwrap_or(0.0)),

        // ── 音量 ──
        "volume" => ControlCmd::Volume(req.value.unwrap_or(0.0)),
        "set_volume" => {
            let v = req.value.unwrap_or(90.0).clamp(0.0, 100.0) / 100.0;
            ControlCmd::SetVolume(v)
        }

        // ── 混音模式 ──
        "mix" => ControlCmd::CycleMix,
        "set_mix" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetMix(mode)
        }

        // ── Remix 模式 ──
        "remix" => ControlCmd::CycleRemix,
        "set_remix" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetRemix(mode)
        }

        // ── 随机播放 ──
        "shuffle" => ControlCmd::ToggleShuffle,
        "set_shuffle" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetShuffle(mode)
        }

        // ── 循环模式 ──
        "loop" => ControlCmd::CycleLoop,
        "set_loop" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetLoop(mode)
        }

        // ── 选中 ──
        "select" => ControlCmd::Select(req.index.unwrap_or(0)),

        // ── EQ ──
        "eq" => {
            let band = req.index.unwrap_or(0);
            if band >= crate::eq::BANDS {
                return err("band must be 0-4");
            }
            let db = req.value.unwrap_or(0.0);
            ControlCmd::SetEq(band, db)
        }
        "eq_reset" => ControlCmd::ResetEq,

        // ── 排序 ──
        "set_sort" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetSort(mode)
        }

        // ── 主题/可视化 ──
        "set_theme" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetTheme(mode)
        }
        "set_visualize" => {
            let mode = req.query.unwrap_or_default();
            ControlCmd::SetVisualize(mode)
        }
        "toggle_transparent" => ControlCmd::ToggleTransparent,
        "toggle_lyrics_fetch" => ControlCmd::ToggleLyricsFetch,
        "toggle_cover_fetch" => ControlCmd::ToggleCoverFetch,
        "toggle_resume" => ControlCmd::ToggleResume,

        // ── 资料库 ──
        "meta_scan" => ControlCmd::MetaScan,

        _ => return err(format!("unknown action: {}", req.action)),
    };

    if let Err(response) = enqueue_control(tx, cmd) {
        return response;
    }
    handle_status(state)
}

fn handle_import(body: &str, tx: &Sender<ControlCmd>) -> serde_json::Value {
    let req: ImportRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    if !crate::fetch::looks_like_media_url(&req.url) {
        return err("need a youtube or bilibili url");
    }
    if let Err(response) = enqueue_control(tx, ControlCmd::Import(req.url.clone())) {
        return response;
    }
    ok(serde_json::json!({"import": "started", "url": req.url}))
}

fn handle_library_scan(tx: &Sender<ControlCmd>) -> serde_json::Value {
    if let Err(response) = enqueue_control(tx, ControlCmd::RescanLibrary) {
        return response;
    }
    ok(serde_json::json!({"scan": "started"}))
}

fn handle_meta_scan(tx: &Sender<ControlCmd>) -> serde_json::Value {
    if let Err(response) = enqueue_control(tx, ControlCmd::MetaScan) {
        return response;
    }
    ok(serde_json::json!({"meta_scan": "started"}))
}

fn handle_eq(body: &str, tx: &Sender<ControlCmd>) -> serde_json::Value {
    let req: EqRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    if req.band >= crate::eq::BANDS {
        return err("band must be 0-4");
    }
    if let Err(response) = enqueue_control(tx, ControlCmd::SetEq(req.band, req.db)) {
        return response;
    }
    ok(serde_json::json!({"eq": "updated", "band": req.band, "db": req.db}))
}

fn handle_eq_reset(tx: &Sender<ControlCmd>) -> serde_json::Value {
    if let Err(response) = enqueue_control(tx, ControlCmd::ResetEq) {
        return response;
    }
    ok(serde_json::json!({"eq": "reset"}))
}

fn handle_search(body: &str, state: &SharedState) -> serde_json::Value {
    let req: ControlRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    let query = req.query.unwrap_or_default();
    if query.is_empty() {
        return err("missing query field");
    }

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    let q = query.to_lowercase();
    let matches: Vec<&TrackInfo> = snap
        .library
        .iter()
        .filter(|t| {
            t.title.to_lowercase().contains(&q)
                || t.artist.to_lowercase().contains(&q)
                || t.album.to_lowercase().contains(&q)
                || t.path.to_lowercase().contains(&q)
        })
        .collect();

    ok(serde_json::json!({
        "query": query,
        "total": matches.len(),
        "matches": matches,
    }))
}

// ══════════════════════════════════════════════════════════════════
//  辅助函数
// ══════════════════════════════════════════════════════════════════

fn parse_index_suffix(path: &str, prefix: &str) -> Option<usize> {
    path.trim_start_matches(prefix)
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok())
}

fn cover_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/jpeg",
    }
}

fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn cover_bytes_at(index: usize, snap: &ApiSnapshot) -> Option<(&'static str, Vec<u8>)> {
    let path = snap.cover_paths.get(index).and_then(|p| p.as_ref())?;
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some((cover_mime(path), bytes))
}

fn cover_json_at(index: usize, snap: &ApiSnapshot) -> serde_json::Value {
    match cover_bytes_at(index, snap) {
        Some((mime, bytes)) => serde_json::json!({
            "mime": mime,
            "data": encode_base64(&bytes),
        }),
        None => serde_json::Value::Null,
    }
}

fn cover_bytes_for(path: &str, state: &SharedState) -> Option<(&'static str, Vec<u8>)> {
    let snap = state.lock().ok()?;
    let idx = parse_index_suffix(path, "/cover/").or(snap.track.as_ref().map(|t| t.index))?;
    cover_bytes_at(idx, &snap)
}

fn ok(data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"ok": true, "data": data})
}

fn err(msg: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": msg.into()})
}

fn enqueue_control(tx: &Sender<ControlCmd>, cmd: ControlCmd) -> Result<(), serde_json::Value> {
    tx.send(cmd).map_err(|_| err("control unavailable"))
}

fn http_status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

fn send_bytes(stream: &mut TcpStream, status: u16, mime: &str, body: &[u8]) -> std::io::Result<()> {
    let status_text = http_status_text(status);
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    Ok(())
}

fn send_json(stream: &mut TcpStream, status: u16, body: &serde_json::Value) -> std::io::Result<()> {
    let json = serde_json::to_vec(body).unwrap_or_default();
    let status_text = http_status_text(status);
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        json.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&json)?;
    Ok(())
}

fn send_options(stream: &mut TcpStream) -> std::io::Result<()> {
    let header = "HTTP/1.1 204 No Content\r\n\
                  Allow: GET, POST, OPTIONS\r\n\
                  Content-Length: 0\r\n\
                  Connection: close\r\n\r\n";
    stream.write_all(header.as_bytes())
}

// ══════════════════════════════════════════════════════════════════
//  测试
// ══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_limit_allows_64_kib_and_rejects_more() {
        assert!(request_body_size_allowed(65_536));
        assert!(!request_body_size_allowed(65_537));
        assert_eq!(http_status_text(413), "Payload Too Large");
    }

    #[test]
    fn request_head_reads_stop_at_the_byte_budget() {
        let mut budget = MAX_REQUEST_HEAD_BYTES;
        let mut endless = BufReader::new(std::io::repeat(b'a'));
        assert!(read_head_line(&mut endless, &mut budget).unwrap().is_none());

        let mut budget = MAX_REQUEST_HEAD_BYTES;
        let mut flood = std::io::Cursor::new(b"X-Pad: 1\r\n".repeat(4000));
        let mut lines = 0;
        while read_head_line(&mut flood, &mut budget).unwrap().is_some() {
            lines += 1;
        }
        assert!(lines < 4000, "header count must be bounded, read {lines}");

        let mut budget = MAX_REQUEST_HEAD_BYTES;
        let mut req = std::io::Cursor::new(b"GET /status HTTP/1.1\r\n\r\n".to_vec());
        let line = read_head_line(&mut req, &mut budget).unwrap().unwrap();
        assert_eq!(line, "GET /status HTTP/1.1\r\n");
        assert_eq!(budget, MAX_REQUEST_HEAD_BYTES - line.len());
        assert_eq!(http_status_text(431), "Request Header Fields Too Large");
    }

    #[test]
    fn oversized_header_line_gets_431_instead_of_buffering() {
        let mut req = b"GET /status HTTP/1.1\r\nX-Pad: ".to_vec();
        req.resize(MAX_REQUEST_HEAD_BYTES + 1, b'a');
        let resp = serve_once(&req);
        assert!(resp.starts_with("HTTP/1.1 431 "), "got {resp:?}");

        let body = r#"{"url":"https://youtu.be/tRsQsTMvPNg"}"#;
        let req = format!(
            "POST /import HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let resp = serve_once(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200 "), "got {resp:?}");
        assert!(resp.contains(r#""ok":true"#), "got {resp:?}");
    }

    fn serve_once(req: &[u8]) -> String {
        serve_queued(req).0
    }

    fn serve_queued(req: &[u8]) -> (String, Vec<ControlCmd>) {
        serve_from(IpAddr::from(Ipv4Addr::LOCALHOST), None, req)
    }

    /// 假装请求来自 `peer`、服务端 token 是 `token`，返回响应和排进控制队列的命令。
    fn serve_from(peer: IpAddr, token: Option<&str>, req: &[u8]) -> (String, Vec<ControlCmd>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let token = token.map(str::to_string);
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = handle_connection(stream, peer, token.as_deref(), &tx, &new_shared_state());
            rx.try_iter().collect::<Vec<_>>()
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        client.write_all(req).unwrap();
        let mut resp = String::new();
        let _ = client.read_to_string(&mut resp);
        let queued = server.join().unwrap();
        (resp, queued)
    }

    fn post_next(headers: &str) -> String {
        let body = r#"{"action":"next"}"#;
        format!(
            "POST /control HTTP/1.1\r\n{headers}Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn cross_origin_pages_can_neither_read_nor_control() {
        let (resp, queued) =
            serve_queued(b"GET /library/full HTTP/1.1\r\nHost: localhost:18765\r\nOrigin: https://evil.example\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 403 "), "got {resp:?}");
        assert!(!resp.contains("tracks"), "got {resp:?}");
        assert!(queued.is_empty());

        let req = post_next("Host: 127.0.0.1:18765\r\nOrigin: http://evil.example\r\n");
        let (resp, queued) = serve_queued(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 403 "), "got {resp:?}");
        assert!(
            queued.is_empty(),
            "a cross-site form post must not queue {queued:?}"
        );

        let req = post_next("Host: 127.0.0.1:18765\r\nOrigin: null\r\n");
        let (resp, queued) = serve_queued(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 403 "), "got {resp:?}");
        assert!(queued.is_empty());

        let resp = serve_once(
            b"OPTIONS /control HTTP/1.1\r\nHost: localhost:18765\r\nOrigin: https://evil.example\r\nAccess-Control-Request-Method: POST\r\n\r\n",
        );
        assert!(
            !resp
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "got {resp:?}"
        );
    }

    #[test]
    fn lan_clients_need_the_token() {
        let phone = IpAddr::from([192, 168, 1, 23]);
        let (resp, _) = serve_from(
            phone,
            Some("s3cret"),
            b"GET /library/full HTTP/1.1\r\nHost: 192.168.1.5:18765\r\n\r\n",
        );
        assert!(resp.starts_with("HTTP/1.1 401 "), "got {resp:?}");
        assert!(!resp.contains("tracks"), "got {resp:?}");

        for auth in [
            "",
            "Authorization: Bearer wrong\r\n",
            "Authorization: s3cret\r\n",
        ] {
            let req = post_next(&format!("Host: 192.168.1.5:18765\r\n{auth}"));
            let (resp, queued) = serve_from(phone, Some("s3cret"), req.as_bytes());
            assert!(resp.starts_with("HTTP/1.1 401 "), "{auth:?} got {resp:?}");
            assert!(queued.is_empty(), "{auth:?} queued {queued:?}");
        }

        for auth in [
            "Authorization: Bearer s3cret\r\n",
            "authorization: bearer s3cret\r\n",
        ] {
            let req = post_next(&format!("Host: 192.168.1.5:18765\r\n{auth}"));
            let (resp, queued) = serve_from(phone, Some("s3cret"), req.as_bytes());
            assert!(resp.starts_with("HTTP/1.1 200 "), "{auth:?} got {resp:?}");
            assert!(matches!(queued[..], [ControlCmd::Next]), "got {queued:?}");
        }

        // 没有 token（文件写不出来）时局域网一律拒绝，空 Bearer 也不算数
        let req = post_next("Host: 192.168.1.5:18765\r\nAuthorization: Bearer \r\n");
        let (resp, queued) = serve_from(phone, None, req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 401 "), "got {resp:?}");
        assert!(queued.is_empty());
    }

    #[test]
    fn loopback_clients_need_no_token() {
        for peer in [
            IpAddr::from(Ipv4Addr::LOCALHOST),
            IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
        ] {
            let req = post_next("Host: localhost:18765\r\n");
            let (resp, queued) = serve_from(peer, Some("s3cret"), req.as_bytes());
            assert!(resp.starts_with("HTTP/1.1 200 "), "{peer} got {resp:?}");
            assert!(matches!(queued[..], [ControlCmd::Next]), "got {queued:?}");
        }
    }

    #[test]
    fn api_token_is_created_private_and_kept() {
        let dir = std::env::temp_dir().join(format!("zradio-api-token-{}", std::process::id()));
        let path = dir.join("api-token");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(load_token(&path, false), None);
        assert!(!path.exists(), "only switching LAN on may create the token");

        let token = load_token(&path, true).unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(load_token(&path, true).as_deref(), Some(token.as_str()));
        assert_eq!(load_token(&path, false).as_deref(), Some(token.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file mode {mode:o}");
        }

        std::fs::write(&path, "  my-phone-key \n").unwrap();
        assert_eq!(load_token(&path, true).as_deref(), Some("my-phone-key"));
        std::fs::write(&path, "\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let fresh = load_token(&path, true).unwrap();
        assert_ne!(fresh, token);
        assert_eq!(fresh.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "rewritten token file mode {mode:o}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_origin_and_non_browser_clients_still_work() {
        let req = post_next("Host: localhost:18765\r\nOrigin: http://localhost:18765\r\n");
        let (resp, queued) = serve_queued(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200 "), "got {resp:?}");
        assert!(matches!(queued[..], [ControlCmd::Next]), "got {queued:?}");

        let req = post_next("Host: 127.0.0.1:18765\r\n");
        let (resp, queued) = serve_queued(req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200 "), "got {resp:?}");
        assert!(matches!(queued[..], [ControlCmd::Next]), "got {queued:?}");
        assert!(
            !resp
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "got {resp:?}"
        );
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn answers(addr: SocketAddr) -> bool {
        let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
            return false;
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        if s.write_all(b"GET /health HTTP/1.1\r\n\r\n").is_err() {
            return false;
        }
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        resp.contains(r#""ok":true"#)
    }

    fn eventually(mut check: impl FnMut() -> bool) -> bool {
        (0..100).any(|_| {
            check() || {
                thread::sleep(Duration::from_millis(20));
                false
            }
        })
    }

    // Linux 上整个 127.0.0.0/8 都是本机：绑 0.0.0.0 时 127.0.0.2 连得上，
    // 只绑 127.0.0.1 时连不上，借此看出服务有没有对外开放。
    #[cfg(target_os = "linux")]
    #[test]
    fn api_listens_on_loopback_until_lan_is_switched_on() {
        let port = free_port();
        let dir = std::env::temp_dir().join(format!("zradio-api-bind-{}", std::process::id()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let server = start_on(port, false, dir.join("api-token"), tx, new_shared_state());
        let local = SocketAddr::from(([127, 0, 0, 1], port));
        let other = SocketAddr::from(([127, 0, 0, 2], port));
        assert!(eventually(|| answers(local)));
        assert!(
            !answers(other),
            "the API must not listen beyond loopback by default"
        );

        server.set_lan(true);
        assert!(
            eventually(|| answers(other)),
            "switching LAN on must rebind"
        );
        assert!(answers(local));

        server.set_lan(false);
        assert!(
            eventually(|| !answers(other)),
            "switching LAN off must rebind"
        );
        assert!(answers(local));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_reads_have_a_five_second_inactivity_timeout() {
        assert_eq!(request_read_timeout(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn ok_response_structure() {
        let resp = ok(serde_json::json!({"hello": "world"}));
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["hello"], "world");
    }

    #[test]
    fn err_response_structure() {
        let resp = err("something broke");
        assert_eq!(resp["ok"], false);
        assert_eq!(resp["error"], "something broke");
    }

    #[test]
    fn eq_rejects_unknown_bands_before_queueing() {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = new_shared_state();
        assert_eq!(handle_eq(r#"{"band":5,"db":3.0}"#, &tx)["ok"], false);
        let resp = handle_control(r#"{"action":"eq","index":7,"value":3.0}"#, &tx, &state);
        assert_eq!(resp["ok"], false);
        assert!(rx.try_recv().is_err());

        assert_eq!(handle_eq(r#"{"band":4,"db":3.0}"#, &tx)["ok"], true);
        assert!(matches!(rx.try_recv(), Ok(ControlCmd::SetEq(4, db)) if db == 3.0));
    }

    #[test]
    fn import_rejects_non_media_urls_before_queueing() {
        let (tx, rx) = std::sync::mpsc::channel();
        let resp = handle_import(r#"{"url":"https://evil.example/youtube.com/x"}"#, &tx);
        assert_eq!(resp["ok"], false);
        assert!(rx.try_recv().is_err());

        let resp = handle_import(r#"{"url":"https://youtu.be/tRsQsTMvPNg"}"#, &tx);
        assert_eq!(resp["ok"], true);
        assert!(matches!(rx.try_recv(), Ok(ControlCmd::Import(_))));
    }

    #[test]
    fn disconnected_control_handlers_report_unavailable() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let state = new_shared_state();
        let responses = [
            handle_control(r#"{"action":"play"}"#, &tx, &state),
            handle_import(r#"{"url":"https://youtu.be/tRsQsTMvPNg"}"#, &tx),
            handle_library_scan(&tx),
            handle_meta_scan(&tx),
            handle_eq(r#"{"band":2,"db":4.5}"#, &tx),
            handle_eq_reset(&tx),
        ];

        for response in responses {
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"], "control unavailable");
        }
    }

    #[test]
    fn control_request_parses_play() {
        let req: ControlRequest = serde_json::from_str(r#"{"action":"play"}"#).unwrap();
        assert_eq!(req.action, "play");
    }

    #[test]
    fn control_request_parses_set_shuffle() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"action":"set_shuffle","query":"taste"}"#).unwrap();
        assert_eq!(req.action, "set_shuffle");
        assert_eq!(req.query.as_deref(), Some("taste"));
    }

    #[test]
    fn control_request_parses_set_volume() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"action":"set_volume","value":75.0}"#).unwrap();
        assert_eq!(req.action, "set_volume");
        assert_eq!(req.value, Some(75.0));
    }

    #[test]
    fn control_request_parses_play_path() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"action":"play_path","path":"/music/a.mp3"}"#).unwrap();
        assert_eq!(req.action, "play_path");
        assert_eq!(req.path.as_deref(), Some("/music/a.mp3"));
    }

    #[test]
    fn status_snapshot_serializes() {
        let resp = handle_status(&Arc::new(Mutex::new(ApiSnapshot::default())));
        assert_eq!(resp["ok"], true);
        assert!(resp["data"]["playback"].is_object());
        assert!(resp["data"]["library"].is_object());
        // prefs 为 None 时序列化为 null，这是正常行为
    }

    #[test]
    fn search_filters_by_title() {
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![
                TrackInfo {
                    index: 0,
                    title: "Levels".into(),
                    artist: "Avicii".into(),
                    ..Default::default()
                },
                TrackInfo {
                    index: 1,
                    title: "Clarity".into(),
                    artist: "Zedd".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }));
        let resp = handle_search(r#"{"query":"levels"}"#, &state);
        assert_eq!(resp["data"]["total"], 1);
        assert_eq!(resp["data"]["matches"][0]["title"], "Levels");
    }

    #[test]
    fn shuffle_endpoint_returns_modes() {
        let resp = handle_get_shuffle(&Arc::new(Mutex::new(ApiSnapshot::default())));
        assert_eq!(resp["ok"], true);
        assert!(resp["data"]["modes"].is_array());
    }

    #[test]
    fn library_full_returns_track_list() {
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![
                TrackInfo {
                    index: 0,
                    title: "A".into(),
                    ..Default::default()
                },
                TrackInfo {
                    index: 1,
                    title: "B".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }));
        let resp = handle_library_full(&state);
        assert_eq!(resp["data"]["total"], 2);
    }

    fn sample_track(index: usize, title: &str) -> TrackInfo {
        TrackInfo {
            index,
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn lyrics_returns_complete_lrc_for_any_index() {
        let lrc = "[00:12.50]hello\n[01:03.00]world\n";
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![sample_track(0, "A"), sample_track(1, "B")],
            lyrics_lrc: vec![String::new(), lrc.into()],
            lyrics: vec![LyricLine {
                time: 0.0,
                text: "cached current only".into(),
            }],
            track: Some(sample_track(0, "A")),
            ..Default::default()
        }));
        let resp = handle_lyrics("/lyrics/1", &state);
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["index"], 1);
        assert_eq!(resp["data"]["lrc"], lrc);
        assert_eq!(resp["data"]["has_lyrics"], true);
        assert!(resp["data"].get("lyrics").is_none() || resp["data"]["lyrics"].is_null());
    }

    #[test]
    fn lyrics_empty_when_track_has_none() {
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![sample_track(0, "A")],
            lyrics_lrc: vec![String::new()],
            ..Default::default()
        }));
        let resp = handle_lyrics("/lyrics/0", &state);
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["lrc"], "");
        assert_eq!(resp["data"]["has_lyrics"], false);
    }

    fn temp_cover(bytes: &[u8], ext: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "zradio-cover-{}-{}-{}.{}",
            std::process::id(),
            nanos,
            bytes.len(),
            ext
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn track_info_returns_album_cover() {
        let jpeg = b"\xff\xd8\xfffakejpeg";
        let path = temp_cover(jpeg, "jpg");
        let mut info = sample_track(1, "B");
        info.has_cover = true;
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![sample_track(0, "A"), info],
            cover_paths: vec![None, Some(path.clone())],
            ..Default::default()
        }));
        let resp = handle_track("/track/1", &state);
        let _ = std::fs::remove_file(&path);
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["data"]["info"]["title"], "B");
        assert_eq!(resp["data"]["cover"]["mime"], "image/jpeg");
        assert_eq!(
            resp["data"]["cover"]["data"].as_str().unwrap(),
            "/9j/ZmFrZWpwZWc="
        );
    }

    #[test]
    fn encode_base64_pads() {
        assert_eq!(encode_base64(b"\xff\xd8\xfffakejpeg"), "/9j/ZmFrZWpwZWc=");
        assert_eq!(encode_base64(b"abc"), "YWJj");
    }

    #[test]
    fn library_full_omits_cover_bytes() {
        let path = temp_cover(b"\xff\xd8\xffsecret", "jpg");
        let mut info = sample_track(0, "A");
        info.has_cover = true;
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![info],
            cover_paths: vec![Some(path.clone())],
            ..Default::default()
        }));
        let resp = handle_library_full(&state);
        let _ = std::fs::remove_file(&path);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("image/jpeg"));
        assert_eq!(resp["data"]["tracks"][0]["has_cover"], true);
    }

    #[test]
    fn cover_endpoint_reads_file_bytes() {
        let jpeg = b"\xff\xd8\xffcover";
        let path = temp_cover(jpeg, "jpg");
        let state = Arc::new(Mutex::new(ApiSnapshot {
            library: vec![sample_track(2, "C")],
            cover_paths: vec![None, None, Some(path.clone())],
            ..Default::default()
        }));
        let found = cover_bytes_for("/cover/2", &state);
        let _ = std::fs::remove_file(&path);
        let (mime, bytes) = found.expect("cover");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, jpeg);
        assert!(cover_bytes_for("/cover/0", &state).is_none());
    }
}
