use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use serde::{Deserialize, Serialize};

use crate::control::ControlCmd;
use crate::engine::Snapshot;

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

pub fn start(tx: Sender<ControlCmd>, state: SharedState) {
    thread::spawn(move || {
        let Ok(listener) = TcpListener::bind("0.0.0.0:18765") else {
            eprintln!("[api] 绑定 18765 失败");
            return;
        };
        eprintln!("[api] 远程控制监听 0.0.0.0:18765");
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let state = Arc::clone(&state);
            thread::spawn(move || {
                if let Err(e) = handle_connection(stream, &tx, &state) {
                    eprintln!("[api] 连接错误: {e}");
                }
            });
        }
    });
}

fn handle_connection(
    mut stream: TcpStream,
    tx: &Sender<ControlCmd>,
    state: &SharedState,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    let mut buf_reader = BufReader::new(reader);

    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return send_json(&mut stream, 400, &err("bad request"));
    }

    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        buf_reader.read_line(&mut line)?;
        let trimmed = line.trim().to_lowercase();
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    let body = if content_length > 0 {
        let mut body_buf = vec![0u8; content_length];
        buf_reader.read_exact(&mut body_buf)?;
        String::from_utf8_lossy(&body_buf).into_owned()
    } else {
        String::new()
    };

    if method == "OPTIONS" {
        return send_cors_preflight(&mut stream);
    }

    let response = route(method, path, &body, tx, state);
    send_json(&mut stream, 200, &response)
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
            }));
        }
    }

    // 从完整资料库中查找
    if let Some(track) = snap.library.iter().find(|t| t.index == index) {
        return ok(serde_json::json!({"info": track}));
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

    if let Some(track) = snap
        .library
        .iter()
        .find(|t| t.index == index.or(snap.track.as_ref().map(|t| t.index)).unwrap_or(0))
    {
        return ok(serde_json::json!({
            "index": track.index,
            "has_cover": track.has_cover,
        }));
    }

    err("track not found")
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

    let _ = tx.send(cmd);
    handle_status(state)
}

fn handle_import(body: &str, tx: &Sender<ControlCmd>) -> serde_json::Value {
    let req: ImportRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    let _ = tx.send(ControlCmd::Import(req.url.clone()));
    ok(serde_json::json!({"import": "started", "url": req.url}))
}

fn handle_library_scan(tx: &Sender<ControlCmd>) -> serde_json::Value {
    let _ = tx.send(ControlCmd::RescanLibrary);
    ok(serde_json::json!({"scan": "started"}))
}

fn handle_meta_scan(tx: &Sender<ControlCmd>) -> serde_json::Value {
    let _ = tx.send(ControlCmd::MetaScan);
    ok(serde_json::json!({"meta_scan": "started"}))
}

fn handle_eq(body: &str, tx: &Sender<ControlCmd>) -> serde_json::Value {
    let req: EqRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    let _ = tx.send(ControlCmd::SetEq(req.band, req.db));
    ok(serde_json::json!({"eq": "updated", "band": req.band, "db": req.db}))
}

fn handle_eq_reset(tx: &Sender<ControlCmd>) -> serde_json::Value {
    let _ = tx.send(ControlCmd::ResetEq);
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

fn ok(data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"ok": true, "data": data})
}

fn err(msg: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": msg.into()})
}

fn send_json(stream: &mut TcpStream, status: u16, body: &serde_json::Value) -> std::io::Result<()> {
    let json = serde_json::to_vec(body).unwrap_or_default();
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\r\n",
        json.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&json)?;
    Ok(())
}

fn send_cors_preflight(stream: &mut TcpStream) -> std::io::Result<()> {
    let header = "HTTP/1.1 204 No Content\r\n\
                  Access-Control-Allow-Origin: *\r\n\
                  Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
                  Access-Control-Allow-Headers: Content-Type\r\n\
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
}
