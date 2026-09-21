use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::Sender;
use std::thread;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum ControlCmd {
    Status,
    Play,
    Pause,
    TogglePause,
    Next,
    Prev,
    Import(String),
    Search(String),
    /// 播放指定索引的曲目
    PlayIndex(usize),
    /// 按路径播放
    PlayPath(String),
    /// 相对跳转（秒）
    Seek(f32),
    /// 绝对跳转（秒）
    SeekTo(f32),
    /// 音量增减（-1.0 ~ 1.0）
    Volume(f32),
    /// 音量绝对值（0.0 ~ 1.0）
    SetVolume(f32),
    /// 循环切换混音模式
    CycleMix,
    /// 精确设置混音模式
    SetMix(String),
    /// 循环切换 Remix 模式
    CycleRemix,
    /// 精确设置 Remix 模式
    SetRemix(String),
    /// 切换随机播放
    ToggleShuffle,
    /// 精确设置随机模式
    SetShuffle(String),
    /// 循环切换循环模式
    CycleLoop,
    /// 精确设置循环模式
    SetLoop(String),
    /// 选中指定索引
    Select(usize),
    /// 重新扫描资料库
    RescanLibrary,
    /// 扫描缺失元数据
    MetaScan,
    /// 设置 EQ 某个频段
    SetEq(usize, f32),
    /// 重置 EQ
    ResetEq,
    /// 设置排序模式
    SetSort(String),
    /// 设置主题
    SetTheme(String),
    /// 设置可视化模式
    SetVisualize(String),
    /// 切换透明背景
    ToggleTransparent,
    /// 切换歌词获取
    ToggleLyricsFetch,
    /// 切换封面获取
    ToggleCoverFetch,
    /// 切换恢复播放
    ToggleResume,
}

#[derive(Deserialize)]
struct Body {
    url: Option<String>,
    query: Option<String>,
}

pub fn start(tx: Sender<ControlCmd>) {
    thread::spawn(move || {
        let Ok(listener) = TcpListener::bind("127.0.0.1:18765") else {
            return;
        };
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let n = match stream.read(&mut buf) {
                Ok(n) => n,
                Err(_) => continue,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let cmd = parse(&req);
            let ok = cmd
                .as_ref()
                .map(|c| tx.send(c.clone()).is_ok())
                .unwrap_or(true);
            let body = if cmd.is_some() && ok {
                "{\"ok\":true}"
            } else {
                "{\"ok\":false}"
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
}

fn parse(req: &str) -> Option<ControlCmd> {
    let first = req.lines().next().unwrap_or("");
    let path = first.split_whitespace().nth(1).unwrap_or("/");
    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed: Body = serde_json::from_str(body).unwrap_or(Body {
        url: None,
        query: None,
    });
    match path {
        "/play" => Some(ControlCmd::Play),
        "/pause" => Some(ControlCmd::Pause),
        "/toggle" => Some(ControlCmd::TogglePause),
        "/next" => Some(ControlCmd::Next),
        "/prev" => Some(ControlCmd::Prev),
        "/import" => parsed.url.map(ControlCmd::Import),
        "/search" => parsed.query.or(parsed.url).map(ControlCmd::Search),
        "/status" => Some(ControlCmd::Status),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_import() {
        let req = "POST /import HTTP/1.1\r\n\r\n{\"url\":\"https://youtu.be/abc\"}";
        match parse(req) {
            Some(ControlCmd::Import(url)) => assert!(url.contains("youtu")),
            _ => panic!("bad parse"),
        }
    }

    #[test]
    fn parse_vibe_is_gone() {
        let req = "POST /vibe HTTP/1.1\r\n\r\n{\"genre\":\"lofi\"}";
        assert!(
            parse(req).is_none(),
            "vibe control endpoint must be removed"
        );
    }
}
