use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const SPIN: [&str; 4] = ["✶", "✻", "✽", "✶"];

#[derive(Debug, Clone, Copy)]
pub struct WorkBeat {
    pub tool: &'static str,
    pub target: &'static str,
    pub detail: &'static str,
}

const BEATS: &[WorkBeat] = &[
    WorkBeat {
        tool: "Read",
        target: "src/cover.rs",
        detail: "104 lines",
    },
    WorkBeat {
        tool: "Bash",
        target: "cargo test --quiet",
        detail: "30 passed",
    },
    WorkBeat {
        tool: "Grep",
        target: "MixMode",
        detail: "12 matches",
    },
    WorkBeat {
        tool: "Edit",
        target: "src/ui.rs",
        detail: "+24 −11",
    },
    WorkBeat {
        tool: "Read",
        target: "src/engine.rs",
        detail: "783 lines",
    },
    WorkBeat {
        tool: "Bash",
        target: "cargo clippy --all-targets",
        detail: "finished",
    },
    WorkBeat {
        tool: "Edit",
        target: "src/cover.rs",
        detail: "keep letterbox",
    },
    WorkBeat {
        tool: "Bash",
        target: "playerctl -p zradio metadata",
        detail: "title ok",
    },
];

const THINKING: [&str; 6] = [
    "Checking cover aspect against the panel.",
    "Keeping album art square in the terminal grid.",
    "Wiring :url import through yt-dlp.",
    "Matching Claude Code tool-call rhythm.",
    "Leaving AutoMix on while the stream plays.",
    "Not stretching pixels to fill empty space.",
];

pub fn spinner(tick: u64) -> &'static str {
    SPIN[((tick / 4) % SPIN.len() as u64) as usize]
}

pub fn lines(tick: u64, rows: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let think = THINKING[((tick / 18) % THINKING.len() as u64) as usize];
    out.push(Line::from(vec![
        Span::styled(
            format!(" {} ", spinner(tick)),
            Style::default().fg(Color::Rgb(215, 119, 87)),
        ),
        Span::styled("Thinking…", Style::default().fg(Color::Rgb(215, 119, 87))),
    ]));
    out.push(Line::from(Span::styled(
        format!("   {think}"),
        Style::default().fg(Color::Rgb(138, 138, 138)),
    )));
    out.push(Line::from(""));

    let n = (rows.saturating_sub(6) as usize / 3).clamp(2, BEATS.len());
    let start = ((tick / 10) as usize) % BEATS.len();
    for i in 0..n {
        let beat = BEATS[(start + i) % BEATS.len()];
        out.push(Line::from(vec![
            Span::styled(" ⏺ ", Style::default().fg(Color::Rgb(215, 119, 87))),
            Span::styled(beat.tool, Style::default().fg(Color::Rgb(232, 232, 232))),
            Span::raw(" "),
            Span::styled(beat.target, Style::default().fg(Color::Rgb(125, 196, 228))),
        ]));
        out.push(Line::from(Span::styled(
            format!("    {}", beat.detail),
            Style::default().fg(Color::Rgb(120, 120, 120)),
        )));
        out.push(Line::from(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_has_tool_calls() {
        let rendered = lines(12, 16);
        let joined: String = rendered.iter().map(|l| l.to_string()).collect();
        assert!(joined.contains("Thinking"));
        assert!(joined.contains("Read") || joined.contains("Bash") || joined.contains("Edit"));
    }
}
