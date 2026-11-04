use ratatui::layout::Rect;

pub const DRAWER_MS: u64 = 320;
pub const DRAWER_SLOTS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerPhase {
    Closed,
    Opening,
    Open,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawerAnim {
    phase: DrawerPhase,
    progress: f32,
}

impl Default for DrawerAnim {
    fn default() -> Self {
        Self::closed()
    }
}

impl DrawerAnim {
    pub fn closed() -> Self {
        Self {
            phase: DrawerPhase::Closed,
            progress: 0.0,
        }
    }

    pub fn phase(self) -> DrawerPhase {
        self.phase
    }

    pub fn open(&mut self) {
        if self.phase != DrawerPhase::Open {
            self.phase = DrawerPhase::Opening;
        }
    }

    pub fn close(&mut self) {
        if self.phase != DrawerPhase::Closed {
            self.phase = DrawerPhase::Closing;
        }
    }

    pub fn toggle(&mut self) {
        match self.phase {
            DrawerPhase::Closed | DrawerPhase::Closing => self.open(),
            DrawerPhase::Opening | DrawerPhase::Open => self.close(),
        }
    }

    pub fn tick(&mut self, dt_ms: u64) {
        let step = dt_ms as f32 / DRAWER_MS as f32;
        match self.phase {
            DrawerPhase::Opening => {
                self.progress = (self.progress + step).min(1.0);
                if self.progress >= 1.0 {
                    self.phase = DrawerPhase::Open;
                }
            }
            DrawerPhase::Closing => {
                self.progress = (self.progress - step).max(0.0);
                if self.progress <= 0.0 {
                    self.phase = DrawerPhase::Closed;
                }
            }
            DrawerPhase::Closed | DrawerPhase::Open => {}
        }
    }

    pub fn visual(self) -> f32 {
        ease_out_quad(self.progress)
    }

    pub fn is_visible(self) -> bool {
        self.visual() > 0.0
    }

    pub fn is_interactive(self) -> bool {
        matches!(self.phase, DrawerPhase::Opening | DrawerPhase::Open)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawerRow {
    pub slot: usize,
    pub name: String,
    pub empty: bool,
    pub active: bool,
}

pub fn ease_out_quad(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(2)
}

pub fn drawer_open_height(frame_h: u16) -> u16 {
    12u16.min(frame_h.saturating_sub(2)).max(3)
}

pub fn drawer_rect(frame: Rect, progress: f32, open_height: u16) -> Rect {
    let progress = progress.clamp(0.0, 1.0);
    let height = ((open_height as f32) * progress).round() as u16;
    let height = height.min(frame.height);
    Rect {
        x: frame.x,
        y: frame.y + frame.height.saturating_sub(height),
        width: frame.width,
        height,
    }
}

pub fn drawer_rows(active: usize, lists: &[(String, bool)]) -> Vec<DrawerRow> {
    (1..=DRAWER_SLOTS)
        .map(|slot| {
            if slot == 1 {
                DrawerRow {
                    slot,
                    name: "全部".into(),
                    empty: false,
                    active: active == 0,
                }
            } else {
                let list_idx = slot - 2;
                match lists.get(list_idx) {
                    Some((name, empty)) => DrawerRow {
                        slot,
                        name: name.clone(),
                        empty: *empty,
                        active: active == list_idx + 1,
                    },
                    None => DrawerRow {
                        slot,
                        name: "—".into(),
                        empty: true,
                        active: false,
                    },
                }
            }
        })
        .collect()
}

pub fn wrap_idx(idx: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (idx as i32 + delta).rem_euclid(len as i32) as usize
}

pub fn list_title(slot: usize, name: &str, n: usize) -> String {
    format!(" {slot} · {name} · {n} tracks ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        }
    }

    #[test]
    fn ease_out_quad_anchors_and_eases() {
        assert_eq!(ease_out_quad(0.0), 0.0);
        assert_eq!(ease_out_quad(1.0), 1.0);
        let mid = ease_out_quad(0.5);
        assert!(mid > 0.7 && mid < 0.8, "quad mid should be 0.75, got {mid}");
        assert!(ease_out_quad(-1.0) == 0.0);
        assert!(ease_out_quad(2.0) == 1.0);
    }

    #[test]
    fn closed_drawer_has_zero_height_at_bottom() {
        let area = drawer_rect(frame(), 0.0, 12);
        assert_eq!(area.height, 0);
        assert_eq!(area.y, 24);
        assert_eq!(area.width, 80);
    }

    #[test]
    fn open_drawer_sits_on_the_bottom_edge() {
        let area = drawer_rect(frame(), 1.0, 12);
        assert_eq!(area.height, 12);
        assert_eq!(area.y, 12);
        assert_eq!(area.x, 0);
        assert_eq!(area.width, 80);
    }

    #[test]
    fn half_progress_is_shorter_than_full_and_still_bottom_anchored() {
        let half = drawer_rect(frame(), 0.5, 12);
        let full = drawer_rect(frame(), 1.0, 12);
        assert!(half.height < full.height);
        assert_eq!(half.y + half.height, 24);
        assert_eq!(full.y + full.height, 24);
    }

    #[test]
    fn tick_opens_in_about_320ms_with_ease() {
        let mut anim = DrawerAnim::closed();
        anim.open();
        assert_eq!(anim.phase(), DrawerPhase::Opening);
        anim.tick(DRAWER_MS / 2);
        let mid = anim.visual();
        assert!(
            mid > 0.5 && mid < 0.9,
            "ease-out quad at halfway should sit near 0.75, got {mid}"
        );
        anim.tick(DRAWER_MS / 2);
        assert_eq!(anim.phase(), DrawerPhase::Open);
        assert_eq!(anim.visual(), 1.0);
    }

    #[test]
    fn tick_closes_from_current_progress() {
        let mut anim = DrawerAnim::closed();
        anim.open();
        anim.tick(DRAWER_MS);
        anim.close();
        anim.tick(DRAWER_MS / 2);
        assert_eq!(anim.phase(), DrawerPhase::Closing);
        assert!(anim.visual() < 1.0);
        anim.tick(DRAWER_MS / 2);
        assert_eq!(anim.phase(), DrawerPhase::Closed);
        assert_eq!(anim.visual(), 0.0);
        assert!(!anim.is_visible());
    }

    #[test]
    fn toggle_reverses_in_flight() {
        let mut anim = DrawerAnim::closed();
        anim.toggle();
        anim.tick(60);
        anim.toggle();
        assert_eq!(anim.phase(), DrawerPhase::Closing);
        anim.tick(60);
        assert_eq!(anim.phase(), DrawerPhase::Closed);
    }

    #[test]
    fn rows_always_have_nine_slots_and_mark_empty() {
        let rows = drawer_rows(1, &[("通勤".into(), false)]);
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[0].name, "全部");
        assert!(!rows[0].empty);
        assert!(!rows[0].active);
        assert_eq!(rows[1].name, "通勤");
        assert!(rows[1].active);
        assert!(!rows[1].empty);
        assert!(rows[2].empty);
        assert_eq!(rows[2].name, "—");
        assert_eq!(rows[8].slot, 9);
    }

    #[test]
    fn named_list_with_no_songs_is_drawn_empty() {
        let rows = drawer_rows(0, &[("通勤".into(), true), ("夜车".into(), false)]);
        assert_eq!(rows[1].name, "通勤");
        assert!(rows[1].empty);
        assert!(!rows[1].active);
        assert_eq!(rows[2].name, "夜车");
        assert!(!rows[2].empty);
        assert!(rows[3].empty);
        assert_eq!(rows[3].name, "—");
    }

    #[test]
    fn wrap_idx_cycles() {
        assert_eq!(wrap_idx(0, -1, 9), 8);
        assert_eq!(wrap_idx(8, 1, 9), 0);
        assert_eq!(wrap_idx(3, 2, 9), 5);
        assert_eq!(wrap_idx(0, 1, 0), 0);
    }

    #[test]
    fn list_title_is_current_slot_only() {
        assert_eq!(list_title(1, "全部", 12), " 1 · 全部 · 12 tracks ");
        assert_eq!(list_title(2, "通勤", 3), " 2 · 通勤 · 3 tracks ");
    }
}
