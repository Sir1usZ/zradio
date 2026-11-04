use ratatui::layout::Rect;

use crate::drawer::ease_out_quad;

pub const FADE_IN_MS: u64 = 400;
pub const HOLD_MS: u64 = 5000;
pub const FADE_OUT_MS: u64 = 600;
pub const GREET_HEIGHT: u16 = 5;
pub const GREET_SCALE: u8 = 2;

pub const fn total_ms() -> u64 {
    FADE_IN_MS + HOLD_MS + FADE_OUT_MS
}

pub fn period_for_hour(hour: u32) -> &'static str {
    match hour {
        5..=10 => "早上好",
        11..=13 => "中午好",
        14..=17 => "下午好",
        _ => "晚上好",
    }
}

pub fn resolve_name(user_env: Option<&str>, whoami: Option<&str>) -> String {
    nonempty(user_env)
        .or_else(|| nonempty(whoami))
        .unwrap_or_else(|| "朋友".into())
}

pub fn greeting_line(hour: u32, name: &str) -> String {
    format!("{}，{name}", period_for_hour(hour))
}

pub fn system_name() -> String {
    let user = std::env::var("USER").ok();
    if nonempty(user.as_deref()).is_some() {
        return resolve_name(user.as_deref(), None);
    }
    resolve_name(None, whoami().as_deref())
}

pub fn local_hour() -> u32 {
    date_hour().unwrap_or(12)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GreetingAnim {
    elapsed_ms: u64,
}

impl Default for GreetingAnim {
    fn default() -> Self {
        Self::new()
    }
}

impl GreetingAnim {
    pub fn new() -> Self {
        Self { elapsed_ms: 0 }
    }

    pub fn tick(&mut self, dt_ms: u64) {
        self.elapsed_ms = self.elapsed_ms.saturating_add(dt_ms).min(total_ms());
    }

    pub fn is_visible(self) -> bool {
        self.elapsed_ms < total_ms()
    }

    pub fn visual(self) -> f32 {
        let fade_out_at = FADE_IN_MS + HOLD_MS;
        if self.elapsed_ms >= total_ms() {
            0.0
        } else if self.elapsed_ms < fade_out_at {
            1.0
        } else {
            let t = (self.elapsed_ms - fade_out_at) as f32 / FADE_OUT_MS as f32;
            1.0 - ease_out_quad(t)
        }
    }

    pub fn opacity(self) -> f32 {
        if self.elapsed_ms >= total_ms() {
            0.0
        } else if self.elapsed_ms < FADE_IN_MS {
            ease_out_quad(self.elapsed_ms as f32 / FADE_IN_MS as f32)
        } else if self.elapsed_ms < FADE_IN_MS + HOLD_MS {
            1.0
        } else {
            let t = (self.elapsed_ms - FADE_IN_MS - HOLD_MS) as f32 / FADE_OUT_MS as f32;
            1.0 - ease_out_quad(t)
        }
    }
}

pub fn music_split(area: Rect, visual: f32) -> (Rect, Rect) {
    let visual = visual.clamp(0.0, 1.0);
    let max_greet = GREET_HEIGHT.min(area.height.saturating_sub(4));
    let greet_h = ((max_greet as f32) * visual).round() as u16;
    let greet = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: greet_h,
    };
    let list = Rect {
        x: area.x,
        y: area.y + greet_h,
        width: area.width,
        height: area.height.saturating_sub(greet_h),
    };
    (greet, list)
}

pub fn scaled_span(text: &str, scale: u8) -> String {
    let scale = scale.clamp(1, 7);
    if scale <= 1 || text.is_empty() {
        return text.to_string();
    }
    format!("\x1b]66;s={scale};{text}\x07")
}

pub fn scaled_cell_width(text: &str, scale: u8) -> u16 {
    let scale = scale.clamp(1, 7) as u16;
    text.chars()
        .map(|c| if c.is_ascii() { 1 } else { 2 })
        .sum::<u16>()
        .saturating_mul(scale)
}

pub fn greeting_origin(area: Rect, scale: u8) -> (u16, u16) {
    let scale = scale.max(1) as u16;
    let x = area
        .x
        .saturating_add(2)
        .min(area.x.saturating_add(area.width.saturating_sub(1)));
    let y = if area.height <= scale {
        area.y
    } else {
        area.y + (area.height - scale) / 2
    };
    (x, y)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreetingOverlay {
    pub text: String,
    pub scale: u8,
    pub col: u16,
    pub row: u16,
}

pub fn greeting_overlay(area: Rect, text: &str, scale: u8) -> Option<GreetingOverlay> {
    if scale <= 1 || text.is_empty() || area.width == 0 || area.height < scale as u16 {
        return None;
    }
    let (x, y) = greeting_origin(area, scale);
    Some(GreetingOverlay {
        text: text.to_string(),
        scale,
        col: x.saturating_add(1),
        row: y.saturating_add(1),
    })
}

pub fn overlay_escape(overlay: &GreetingOverlay) -> String {
    format!(
        "\x1b[{};{}H{}",
        overlay.row,
        overlay.col,
        scaled_span(&overlay.text, overlay.scale)
    )
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn date_hour() -> Option<u32> {
    std::process::Command::new("date")
        .arg("+%H")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .filter(|h| *h <= 23)
}

fn whoami() -> Option<String> {
    std::process::Command::new("whoami")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| nonempty(Some(&s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0,
            y: 2,
            width: 80,
            height: 20,
        }
    }

    #[test]
    fn period_follows_local_hour_buckets() {
        assert_eq!(period_for_hour(5), "早上好");
        assert_eq!(period_for_hour(10), "早上好");
        assert_eq!(period_for_hour(11), "中午好");
        assert_eq!(period_for_hour(13), "中午好");
        assert_eq!(period_for_hour(14), "下午好");
        assert_eq!(period_for_hour(17), "下午好");
        assert_eq!(period_for_hour(18), "晚上好");
        assert_eq!(period_for_hour(23), "晚上好");
        assert_eq!(period_for_hour(0), "晚上好");
        assert_eq!(period_for_hour(4), "晚上好");
    }

    #[test]
    fn resolve_name_prefers_user_then_whoami_then_friend() {
        assert_eq!(resolve_name(Some("xender"), Some("root")), "xender");
        assert_eq!(resolve_name(Some("  "), Some("xender")), "xender");
        assert_eq!(resolve_name(None, Some("xender")), "xender");
        assert_eq!(resolve_name(None, Some("  ")), "朋友");
        assert_eq!(resolve_name(None, None), "朋友");
    }

    #[test]
    fn greeting_line_joins_period_and_name() {
        assert_eq!(greeting_line(9, "xender"), "早上好，xender");
        assert_eq!(greeting_line(15, "朋友"), "下午好，朋友");
    }

    #[test]
    fn starts_with_list_shrunk_and_text_invisible() {
        let anim = GreetingAnim::new();
        assert!(anim.is_visible());
        assert_eq!(anim.visual(), 1.0);
        assert_eq!(anim.opacity(), 0.0);
    }

    #[test]
    fn fade_in_reaches_full_opacity_without_growing_the_list() {
        let mut anim = GreetingAnim::new();
        anim.tick(FADE_IN_MS / 2);
        let mid = anim.opacity();
        assert!(mid > 0.7 && mid < 0.8, "ease-out quad mid, got {mid}");
        assert_eq!(anim.visual(), 1.0);
        anim.tick(FADE_IN_MS / 2);
        assert_eq!(anim.opacity(), 1.0);
        assert_eq!(anim.visual(), 1.0);
    }

    #[test]
    fn hold_keeps_greeting_up_for_five_seconds() {
        let mut anim = GreetingAnim::new();
        anim.tick(FADE_IN_MS + HOLD_MS - 1);
        assert!(anim.is_visible());
        assert_eq!(anim.visual(), 1.0);
        assert_eq!(anim.opacity(), 1.0);
    }

    #[test]
    fn fade_out_grows_the_list_and_hides_text() {
        let mut anim = GreetingAnim::new();
        anim.tick(FADE_IN_MS + HOLD_MS);
        assert_eq!(anim.visual(), 1.0);
        anim.tick(FADE_OUT_MS / 2);
        let vis = anim.visual();
        let op = anim.opacity();
        assert!(
            vis > 0.2 && vis < 0.5,
            "eased visual should drop past 0.5, got {vis}"
        );
        assert!((vis - op).abs() < 1e-6);
        anim.tick(FADE_OUT_MS / 2);
        assert!(!anim.is_visible());
        assert_eq!(anim.visual(), 0.0);
        assert_eq!(anim.opacity(), 0.0);
    }

    #[test]
    fn extra_ticks_after_done_stay_hidden() {
        let mut anim = GreetingAnim::new();
        anim.tick(total_ms());
        anim.tick(1_000);
        assert!(!anim.is_visible());
        assert_eq!(anim.visual(), 0.0);
    }

    #[test]
    fn music_split_reserves_top_rows_then_returns_them() {
        let full = area();
        let (greet, list) = music_split(full, 1.0);
        assert_eq!(greet.height, GREET_HEIGHT);
        assert_eq!(greet.y, full.y);
        assert_eq!(list.y, full.y + GREET_HEIGHT);
        assert_eq!(list.height, full.height - GREET_HEIGHT);
        assert_eq!(list.y + list.height, full.y + full.height);

        let (greet, list) = music_split(full, 0.0);
        assert_eq!(greet.height, 0);
        assert_eq!(list, full);
    }

    #[test]
    fn music_split_halfway_is_shorter_greeting_same_bottom() {
        let full = area();
        let (greet, list) = music_split(full, 0.5);
        assert!(greet.height < GREET_HEIGHT);
        assert_eq!(list.y + list.height, full.y + full.height);
        assert_eq!(greet.height + list.height, full.height);
    }

    #[test]
    fn music_split_tiny_terminal_keeps_a_usable_list() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 4,
        };
        let (greet, list) = music_split(tiny, 1.0);
        assert_eq!(greet.height, 0);
        assert_eq!(list, tiny);

        let five = Rect {
            x: 1,
            y: 2,
            width: 40,
            height: 5,
        };
        let (greet, list) = music_split(five, 1.0);
        assert_eq!(greet.height, 1);
        assert_eq!(list.height, 4);
        assert_eq!(list.y + list.height, five.y + five.height);

        let empty = Rect::default();
        let (greet, list) = music_split(empty, 1.0);
        assert_eq!(greet.height, 0);
        assert_eq!(list, empty);
    }

    #[test]
    fn scaled_span_wraps_text_in_kitty_osc66() {
        assert_eq!(scaled_span("hi", 1), "hi");
        assert_eq!(scaled_span("", 2), "");
        assert_eq!(scaled_span("早上好", 2), "\x1b]66;s=2;早上好\x07");
    }

    #[test]
    fn greeting_overlay_skips_plain_text_and_empty_area() {
        let area = Rect {
            x: 0,
            y: 6,
            width: 80,
            height: 5,
        };
        assert_eq!(greeting_overlay(area, "早上好，xender", 1), None);
        assert_eq!(greeting_overlay(area, "", 2), None);
        assert_eq!(greeting_overlay(Rect::default(), "早上好，xender", 2), None);
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        assert_eq!(greeting_overlay(tiny, "早上好，xender", 2), None);
    }

    #[test]
    fn greeting_overlay_keeps_text_free_of_escapes() {
        let area = Rect {
            x: 0,
            y: 6,
            width: 80,
            height: 5,
        };
        let overlay = greeting_overlay(area, "早上好，xender", 2).expect("overlay");
        assert!(!overlay.text.contains('\x1b'));
        assert_eq!(overlay.text, "早上好，xender");
        assert_eq!(overlay.scale, 2);
        assert_eq!(overlay.col, 3);
        assert_eq!(overlay.row, 8);
        let esc = overlay_escape(&overlay);
        assert!(esc.starts_with("\x1b[8;3H"));
        assert!(esc.contains("\x1b]66;s=2;早上好，xender\x07"));
    }

    #[test]
    fn overlay_escape_is_stable_for_the_same_overlay() {
        let area = Rect {
            x: 0,
            y: 6,
            width: 80,
            height: 5,
        };
        let a = greeting_overlay(area, "早上好，xender", 2).expect("overlay");
        let b = greeting_overlay(area, "早上好，xender", 2).expect("overlay");
        assert_eq!(overlay_escape(&a), overlay_escape(&b));
        let faded = greeting_overlay(area, "早上好，xender", 1);
        assert_eq!(faded, None);
    }

    #[test]
    fn scaled_cell_width_counts_cjk_double_width() {
        assert_eq!(scaled_cell_width("ab", 2), 4);
        assert_eq!(scaled_cell_width("早上好", 2), 12);
        assert_eq!(scaled_cell_width("早上好，xender", 2), 28);
    }

    #[test]
    fn greeting_origin_is_left_and_vertically_centered() {
        let area = Rect {
            x: 0,
            y: 6,
            width: 80,
            height: 5,
        };
        let (x, y) = greeting_origin(area, 2);
        assert_eq!(x, 2);
        assert_eq!(y, 7);
    }
}
