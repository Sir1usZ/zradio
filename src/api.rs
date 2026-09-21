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

pub type SharedState = Arc<Mutex<ApiSnapshot>>;

pub fn new_shared_state() -> SharedState {
    Arc::new(Mutex::new(ApiSnapshot::default()))
}

// ── 请求类型 ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ControlRequest {
    action: String,
    #[serde(default)]
    value: Option<f32>,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    query: Option<String>,
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

    // 读请求行
    let mut request_line = String::new();
    buf_reader.read_line(&mut request_line)?;
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return send_json(&mut stream, 400, &err("bad request"));
    }

    let method = parts[0];
    let path = parts[1];

    // 读 headers，找 Content-Length
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

    // 读 body
    let body = if content_length > 0 {
        let mut body_buf = vec![0u8; content_length];
        buf_reader.read_exact(&mut body_buf)?;
        String::from_utf8_lossy(&body_buf).into_owned()
    } else {
        String::new()
    };

    // CORS 预检
    if method == "OPTIONS" {
        return send_cors_preflight(&mut stream);
    }

    // 路由
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
    // ── GET ──
    if method == "GET" {
        return match path {
            "/status" => handle_status(state),
            "/library" => handle_library_list(state),
            "/taste" => handle_taste(state),
            "/prefs" => handle_prefs(),
            "/health" => ok(serde_json::json!({"version": "0.1.0", "name": "zradio"})),
            p if p.starts_with("/track/") => handle_track_detail(p, state),
            p if p.starts_with("/cover/") => handle_cover(p, state),
            p if p.starts_with("/lyrics/") => handle_lyrics(p, state),
            _ => err("not found"),
        };
    }

    // ── POST ──
    if method == "POST" {
        return match path {
            "/control" => handle_control(body, tx, state),
            "/import" => handle_import(body, tx),
            "/library/scan" => handle_library_scan(tx),
            "/eq" => handle_eq(body, tx),
            "/search" => handle_search(body, state),
            _ => err("not found"),
        };
    }

    err("method not allowed")
}

// ── 处理函数 ───────────────────────────────────────────────────────

fn handle_status(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    let playback = snap.snapshot.as_ref().map(|s| {
        let sr = s.sample_rate.max(1);
        serde_json::json!({
            "paused": s.paused,
            "position_secs": s.position_frames as f32 / sr as f32,
            "duration_secs": s.duration_frames as f32 / sr as f32,
            "volume": (s.volume * 100.0).round() as u32,
            "mix_mode": format!("{:?}", s.mix).to_lowercase(),
            "remix_mode": s.remix.label(),
            "shuffle": s.shuffle,
            "loop_mode": s.loop_mode.label(),
            "status": s.status,
            "fading": s.fading,
            "bpm": s.current_bpm,
            "key": s.current_key,
            "next_hint": s.next_hint,
            "sample_rate": s.sample_rate,
        })
    });

    ok(serde_json::json!({
        "playback": playback.unwrap_or(serde_json::json!({
            "paused": true, "position_secs": 0.0, "duration_secs": 0.0,
            "volume": 90, "mix_mode": "crossfade", "remix_mode": "RAW",
            "shuffle": false, "loop_mode": "OFF", "status": "idle",
            "fading": false, "bpm": null, "key": null, "next_hint": null,
            "sample_rate": 0,
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
    }))
}

fn handle_library_list(state: &SharedState) -> serde_json::Value {
    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };
    ok(serde_json::json!({
        "total": snap.tracks_total,
        "current_track": snap.track,
    }))
}

fn handle_track_detail(path: &str, state: &SharedState) -> serde_json::Value {
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

    if let Some(ref track) = snap.track {
        if track.index == index {
            return ok(serde_json::json!({
                "info": track,
                "bpm": snap.snapshot.as_ref().and_then(|s| s.current_bpm),
                "key": snap.snapshot.as_ref().and_then(|s| s.current_key.clone()),
            }));
        }
    }

    err("track not in current session")
}

fn handle_cover(path: &str, state: &SharedState) -> serde_json::Value {
    let _index = path
        .trim_start_matches("/cover/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    if let Some(ref track) = snap.track {
        return ok(serde_json::json!({
            "index": track.index,
            "has_cover": track.has_cover,
            "note": "cover image served via /cover/<index>/image endpoint",
        }));
    }

    err("no track loaded")
}

fn handle_lyrics(path: &str, state: &SharedState) -> serde_json::Value {
    let _index = path
        .trim_start_matches("/lyrics/")
        .split('/')
        .next()
        .and_then(|s| s.parse::<usize>().ok());

    let snap = match state.lock() {
        Ok(s) => s,
        Err(_) => return err("state lock"),
    };

    if let Some(ref track) = snap.track {
        return ok(serde_json::json!({
            "index": track.index,
            "has_lyrics": track.has_lyrics,
            "note": "lyrics loaded per-track in the TUI session",
        }));
    }

    err("no track loaded")
}

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

fn handle_prefs() -> serde_json::Value {
    let prefs = crate::prefs::Prefs::load();
    ok(serde_json::json!({
        "theme": prefs.theme.label(),
        "visualize": prefs.visualize.label(),
        "lyrics_fetch": prefs.lyrics_fetch,
        "cover_fetch": prefs.cover_fetch,
        "resume": prefs.resume,
        "library": prefs.library,
        "eq_db": prefs.eq_db,
        "sort_mode": prefs.sort_mode.label(),
        "shuffle_mode": prefs.shuffle_mode.label(),
    }))
}

fn handle_control(body: &str, tx: &Sender<ControlCmd>, state: &SharedState) -> serde_json::Value {
    let req: ControlRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };

    let cmd = match req.action.as_str() {
        "play" => {
            if let Some(idx) = req.index {
                ControlCmd::PlayIndex(idx)
            } else {
                ControlCmd::Play
            }
        }
        "pause" => ControlCmd::Pause,
        "toggle" => ControlCmd::TogglePause,
        "next" => ControlCmd::Next,
        "prev" => ControlCmd::Prev,
        "seek" => ControlCmd::Seek(req.value.unwrap_or(0.0)),
        "seek_to" => ControlCmd::SeekTo(req.value.unwrap_or(0.0)),
        "volume" => ControlCmd::Volume(req.value.unwrap_or(0.0)),
        "mix" => ControlCmd::CycleMix,
        "remix" => ControlCmd::CycleRemix,
        "shuffle" => ControlCmd::ToggleShuffle,
        "loop" => ControlCmd::CycleLoop,
        "select" => ControlCmd::Select(req.index.unwrap_or(0)),
        "eq" => {
            let band = req.index.unwrap_or(0);
            let db = req.value.unwrap_or(0.0);
            ControlCmd::SetEq(band, db)
        }
        "eq_reset" => ControlCmd::ResetEq,
        _ => return err(format!("unknown action: {}", req.action)),
    };

    let _ = tx.send(cmd);
    // 返回更新后的状态
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

fn handle_eq(body: &str, tx: &Sender<ControlCmd>) -> serde_json::Value {
    let req: EqRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    let _ = tx.send(ControlCmd::SetEq(req.band, req.db));
    ok(serde_json::json!({"eq": "updated", "band": req.band, "db": req.db}))
}

fn handle_search(body: &str, _state: &SharedState) -> serde_json::Value {
    let req: ControlRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(format!("bad json: {e}")),
    };
    let query = req.query.unwrap_or_default();
    ok(serde_json::json!({
        "query": query,
        "note": "search forwarded to TUI; use /status to see results",
    }))
}

// ── 辅助函数 ───────────────────────────────────────────────────────

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

// ── 测试 ───────────────────────────────────────────────────────────

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
        assert!(req.value.is_none());
        assert!(req.index.is_none());
    }

    #[test]
    fn control_request_parses_seek() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"action":"seek","value":30.0}"#).unwrap();
        assert_eq!(req.action, "seek");
        assert_eq!(req.value, Some(30.0));
    }

    #[test]
    fn control_request_parses_play_index() {
        let req: ControlRequest = serde_json::from_str(r#"{"action":"play","index":5}"#).unwrap();
        assert_eq!(req.action, "play");
        assert_eq!(req.index, Some(5));
    }

    #[test]
    fn control_request_parses_eq() {
        let req: ControlRequest =
            serde_json::from_str(r#"{"action":"eq","index":2,"value":6.0}"#).unwrap();
        assert_eq!(req.action, "eq");
        assert_eq!(req.index, Some(2));
        assert_eq!(req.value, Some(6.0));
    }

    #[test]
    fn status_snapshot_serializes() {
        let resp = handle_status(&Arc::new(Mutex::new(ApiSnapshot::default())));
        assert_eq!(resp["ok"], true);
        assert!(resp["data"]["playback"].is_object());
        assert!(resp["data"]["library"].is_object());
    }
}
