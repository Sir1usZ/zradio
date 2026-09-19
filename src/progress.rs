use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::prefs::Palette;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransferBar<'a> {
    pub ratio: f64,
    pub label: &'a str,
    pub active: bool,
    pub pal: Palette,
}

impl Widget for TransferBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let ratio = self.ratio.clamp(0.0, 1.0);
        let filled = ((area.width as f64) * ratio).round() as u16;
        let fill_color = if self.active {
            Palette::rgb(self.pal.peak)
        } else if ratio >= 1.0 {
            Palette::rgb(self.pal.accent)
        } else {
            Palette::rgb(self.pal.dim)
        };
        let empty = if self.pal.is_light() {
            Color::Rgb(220, 220, 224)
        } else {
            Color::Rgb(24, 24, 27)
        };
        let y = area.y + area.height.saturating_sub(1) / 2;
        for x in 0..area.width {
            let color = if x < filled { fill_color } else { empty };
            buf[(area.x + x, y)]
                .set_char('▀')
                .set_fg(color)
                .set_bg(Color::Reset);
        }
        if area.height >= 2 {
            let label = if self.label.chars().count() > area.width as usize {
                self.label
                    .chars()
                    .take(area.width as usize)
                    .collect::<String>()
            } else {
                self.label.to_string()
            };
            let start = area.x + area.width.saturating_sub(label.chars().count() as u16) / 2;
            buf.set_stringn(
                start,
                area.y,
                &label,
                area.width as usize,
                self.pal.text_style(),
            );
        }
    }
}

pub fn bar_label(percent: f64, text: &str) -> String {
    format!("{:>3.0}%  {text}", (percent * 100.0).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_includes_percent() {
        assert!(bar_label(0.123, "dl").starts_with(" 12%"));
        assert!(bar_label(1.0, "done").starts_with("100%"));
    }
}
