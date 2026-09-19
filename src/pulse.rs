use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseKind {
    Idle,
    Think,
    Read,
    Edit,
    Bash,
    Search,
}

impl PulseKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Think => "thinking",
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Bash => "bash",
            Self::Search => "search",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Pulse {
    pub kind: PulseKind,
    pub tool: String,
    pub target: String,
    pub title: String,
    pub beats: Vec<(String, String)>,
}

impl Default for Pulse {
    fn default() -> Self {
        Self {
            kind: PulseKind::Idle,
            tool: "opencode".into(),
            target: String::new(),
            title: "opencode".into(),
            beats: Vec::new(),
        }
    }
}

pub fn poll() -> Pulse {
    let db = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/share/opencode/opencode.db");
    if !db.exists() {
        return Pulse::default();
    }
    let sql = r#"
select
  (select title from session where directory like '%ZRadio%' order by rowid desc limit 1),
  (select data from part where json_extract(data,'$.type')='tool' order by time_updated desc limit 1),
  (select group_concat(json_extract(data,'$.tool') || '|' || coalesce(json_extract(data,'$.state.input.command'), json_extract(data,'$.state.input.filePath'), json_extract(data,'$.state.input.path'), ''), char(10))
     from (select data from part where json_extract(data,'$.type')='tool' order by time_updated desc limit 6));
"#;
    let out = Command::new("sqlite3")
        .args([
            "-readonly",
            db.to_str().unwrap_or(""),
            "-separator",
            "\t",
            sql,
        ])
        .output();
    let Ok(out) = out else {
        return Pulse::default();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cols = text.trim().split('\t');
    let title = cols.next().unwrap_or("opencode").trim().to_string();
    let latest = cols.next().unwrap_or("").to_string();
    let rest = cols.next().unwrap_or("").to_string();
    let (kind, tool, target) = parse_latest(&latest);
    let mut beats = Vec::new();
    for line in rest.lines() {
        if let Some((t, tgt)) = line.split_once('|') {
            if !t.is_empty() {
                beats.push((t.to_string(), short_target(tgt)));
            }
        }
    }
    Pulse {
        kind,
        tool,
        target,
        title: if title.is_empty() {
            "opencode".into()
        } else {
            title
        },
        beats,
    }
}

fn parse_latest(raw: &str) -> (PulseKind, String, String) {
    if raw.is_empty() {
        return (PulseKind::Idle, "opencode".into(), String::new());
    }
    let tool = extract(raw, "\"tool\":\"").unwrap_or_default();
    let status = extract(raw, "\"status\":\"").unwrap_or_default();
    let target = extract(raw, "\"filePath\":\"")
        .or_else(|| extract(raw, "\"path\":\""))
        .or_else(|| extract(raw, "\"command\":\""))
        .or_else(|| extract(raw, "\"pattern\":\""))
        .unwrap_or_default();
    let kind = match (status.as_str(), tool.as_str()) {
        ("pending" | "running", "read" | "Read") => PulseKind::Read,
        ("pending" | "running", "edit" | "Edit" | "write" | "Write") => PulseKind::Edit,
        ("pending" | "running", "bash" | "Bash") => PulseKind::Bash,
        ("pending" | "running", "grep" | "Grep" | "glob" | "Glob") => PulseKind::Search,
        ("pending" | "running", _) => PulseKind::Think,
        _ => {
            if stale_idle() {
                PulseKind::Idle
            } else {
                match tool.as_str() {
                    "read" | "Read" => PulseKind::Read,
                    "edit" | "Edit" | "write" | "Write" => PulseKind::Edit,
                    "bash" | "Bash" => PulseKind::Bash,
                    "grep" | "Grep" | "glob" | "Glob" => PulseKind::Search,
                    _ => PulseKind::Think,
                }
            }
        }
    };
    (
        kind,
        if tool.is_empty() {
            "opencode".into()
        } else {
            tool
        },
        short_target(&target),
    )
}

fn extract(raw: &str, key: &str) -> Option<String> {
    let i = raw.find(key)? + key.len();
    let rest = &raw[i..];
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\n", " ").replace('\\', ""))
}

fn short_target(s: &str) -> String {
    let s = s.trim();
    if s.len() <= 42 {
        return s.to_string();
    }
    s.chars().take(42).collect::<String>() + "…"
}

fn stale_idle() -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.is_multiple_of(9)
}

pub fn lines(pulse: &Pulse, tick: u64, rows: u16) -> Vec<Line<'static>> {
    let spin = ["✶", "✻", "✽", "✶"][((tick / 4) % 4) as usize];
    let color = match pulse.kind {
        PulseKind::Idle => Color::Rgb(120, 120, 120),
        PulseKind::Think => Color::Rgb(215, 119, 87),
        PulseKind::Read => Color::Rgb(125, 196, 228),
        PulseKind::Edit => Color::Rgb(166, 227, 161),
        PulseKind::Bash => Color::Rgb(249, 226, 175),
        PulseKind::Search => Color::Rgb(203, 166, 247),
    };
    let mut out = vec![
        Line::from(vec![
            Span::styled(format!(" {spin} "), Style::default().fg(color)),
            Span::styled("opencode", Style::default().fg(color)),
            Span::raw("  "),
            Span::styled(
                pulse.kind.label(),
                Style::default().fg(Color::Rgb(180, 180, 180)),
            ),
        ]),
        Line::from(Span::styled(
            format!("   {} {}", pulse.tool, pulse.target),
            Style::default().fg(Color::Rgb(160, 160, 160)),
        )),
        Line::from(""),
    ];
    let n = (rows.saturating_sub(5) as usize).clamp(2, 6);
    for (tool, target) in pulse.beats.iter().take(n) {
        out.push(Line::from(vec![
            Span::styled(" ⏺ ", Style::default().fg(color)),
            Span::styled(tool.clone(), Style::default().fg(Color::Rgb(230, 230, 230))),
            Span::raw(" "),
            Span::styled(
                target.clone(),
                Style::default().fg(Color::Rgb(125, 196, 228)),
            ),
        ]));
    }
    if out.len() == 3 {
        out.push(Line::from(Span::styled(
            "   waiting for opencode",
            Style::default().fg(Color::Rgb(90, 90, 90)),
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_returns_quickly() {
        let started = std::time::Instant::now();
        let _ = poll();
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 200,
            "pulse::poll must stay off the UI hot path, took {elapsed:?}"
        );
    }

    #[test]
    fn parse_running_bash() {
        let raw = r#"{"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"cargo test"}}}"#;
        let (kind, tool, target) = parse_latest(raw);
        assert_eq!(kind, PulseKind::Bash);
        assert_eq!(tool, "bash");
        assert!(target.contains("cargo"));
    }
}
