use image::imageops::FilterType;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone)]
pub struct CoverArt {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl CoverArt {
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let img = image::load_from_memory(data).ok()?.to_rgb8();
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return None;
        }
        let max_edge = 512u32;
        let (nw, nh) = if w.max(h) <= max_edge {
            (w, h)
        } else if w >= h {
            (
                max_edge,
                ((h as u64 * max_edge as u64) / w as u64).max(1) as u32,
            )
        } else {
            (
                ((w as u64 * max_edge as u64) / h as u64).max(1) as u32,
                max_edge,
            )
        };
        let resized = if nw == w && nh == h {
            img
        } else {
            image::imageops::resize(&img, nw, nh, FilterType::CatmullRom)
        };
        let (width, height) = resized.dimensions();
        Some(Self {
            width,
            height,
            rgb: resized.into_raw(),
        })
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        if x >= self.width || y >= self.height {
            return [18, 18, 22];
        }
        let i = ((y * self.width + x) * 3) as usize;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    pub fn fit(&self, avail_cols: u16, avail_rows: u16) -> (u16, u16) {
        fit_cells(self.width, self.height, avail_cols, avail_rows)
    }

    pub fn lines(&self, cols: u16, rows: u16) -> Vec<Line<'static>> {
        let (cols, rows) = self.fit(cols, rows);
        let cols_u = cols.max(1) as u32;
        let rows_u = rows.max(1) as u32;
        let mut lines = Vec::with_capacity(rows_u as usize);
        for row in 0..rows_u {
            let mut spans = Vec::with_capacity(cols_u as usize);
            for col in 0..cols_u {
                let top = self.sample(col, row * 2, cols_u, rows_u * 2);
                let bot = self.sample(col, row * 2 + 1, cols_u, rows_u * 2);
                spans.push(Span::styled(
                    "▄",
                    Style::default()
                        .fg(Color::Rgb(bot[0], bot[1], bot[2]))
                        .bg(Color::Rgb(top[0], top[1], top[2])),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    fn sample(&self, x: u32, y: u32, cols: u32, px_rows: u32) -> [u8; 3] {
        let sx = ((x as f32 + 0.5) * self.width as f32 / cols.max(1) as f32).floor() as u32;
        let sy = ((y as f32 + 0.5) * self.height as f32 / px_rows.max(1) as f32).floor() as u32;
        self.pixel(
            sx.min(self.width.saturating_sub(1)),
            sy.min(self.height.saturating_sub(1)),
        )
    }
}

pub fn fit_cells(img_w: u32, img_h: u32, avail_cols: u16, avail_rows: u16) -> (u16, u16) {
    let avail_cols = avail_cols.max(2) as f32;
    let avail_rows = avail_rows.max(2) as f32;
    let aspect = img_w.max(1) as f32 / img_h.max(1) as f32;
    let cols_for_full_height = (avail_rows * 2.0 * aspect).round().max(1.0);
    if cols_for_full_height <= avail_cols {
        (cols_for_full_height as u16, avail_rows as u16)
    } else {
        let rows = (avail_cols / (2.0 * aspect)).round().max(1.0);
        (avail_cols as u16, rows.min(avail_rows) as u16)
    }
}

pub fn placeholder(cols: u16, rows: u16) -> Vec<Line<'static>> {
    let (cols, rows) = fit_cells(1, 1, cols, rows);
    let mut lines = Vec::new();
    for y in 0..rows {
        let mut spans = Vec::new();
        for x in 0..cols {
            let v = 22 + ((x * 5 + y * 9) % 28) as u8;
            spans.push(Span::styled(
                "▄",
                Style::default()
                    .fg(Color::Rgb(v, v + 6, v + 12))
                    .bg(Color::Rgb(v.saturating_sub(6), v, v + 4)),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_cover_uses_two_cols_per_row() {
        let (cols, rows) = fit_cells(100, 100, 40, 12);
        assert_eq!(cols, 24);
        assert_eq!(rows, 12);
    }

    #[test]
    fn wide_cover_caps_width() {
        let (cols, rows) = fit_cells(200, 100, 20, 20);
        assert_eq!(cols, 20);
        assert_eq!(rows, 5);
    }

    #[test]
    fn half_block_uses_fitted_size() {
        let mut rgb = vec![0u8; 40 * 40 * 3];
        rgb[0] = 255;
        let cover = CoverArt {
            width: 40,
            height: 40,
            rgb,
        };
        let lines = cover.lines(40, 12);
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[0].spans.len(), 24);
    }
}
