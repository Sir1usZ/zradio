use ratatui::layout::Rect;

use crate::drawer::ease_out_quad;

pub const FADE_IN_MS: u64 = 400;
pub const HOLD_MS: u64 = 5000;
pub const FADE_OUT_MS: u64 = 600;
pub const GREET_HEIGHT: u16 = 5;

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
}
