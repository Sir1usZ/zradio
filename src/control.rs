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
    Next,
    Prev,
    Vibe(Option<String>),
    Import(String),
    Search(String),
}

#[derive(Deserialize)]
struct Body {
    url: Option<String>,
    query: Option<String>,
    genre: Option<String>,
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
        genre: None,
    });
    match path {
        "/play" => Some(ControlCmd::Play),
        "/pause" => Some(ControlCmd::Pause),
        "/next" => Some(ControlCmd::Next),
        "/prev" => Some(ControlCmd::Prev),
        "/vibe" => Some(ControlCmd::Vibe(parsed.genre)),
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
}
