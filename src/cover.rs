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
        self.raster(cols, rows, 0.0, 0.0, self.width as f32, self.height as f32)
    }

    pub fn fill_lines(&self, cols: u16, rows: u16) -> Vec<Line<'static>> {
        let (cols, rows) = fill_cells(self.width, self.height, cols, rows);
        let (x, y, w, h) = crop_rect(self.width, self.height, cols, rows);
        self.raster(cols, rows, x, y, w, h)
    }

    fn raster(&self, cols: u16, rows: u16, x: f32, y: f32, w: f32, h: f32) -> Vec<Line<'static>> {
        let cols_u = cols.max(1) as u32;
        let rows_u = rows.max(1) as u32;
        let mut lines = Vec::with_capacity(rows_u as usize);
        for row in 0..rows_u {
            let mut spans = Vec::with_capacity(cols_u as usize);
            for col in 0..cols_u {
                let top = self.sample_rect([col, row * 2, cols_u, rows_u * 2], [x, y, w, h]);
                let bot = self.sample_rect([col, row * 2 + 1, cols_u, rows_u * 2], [x, y, w, h]);
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

    fn sample_rect(&self, cell: [u32; 4], src: [f32; 4]) -> [u8; 3] {
        let [x, y, cols, px_rows] = cell;
        let [ox, oy, w, h] = src;
        let sx = ox + (x as f32 + 0.5) * w / cols.max(1) as f32;
        let sy = oy + (y as f32 + 0.5) * h / px_rows.max(1) as f32;
        self.pixel(
            sx.floor().clamp(0.0, self.width.saturating_sub(1) as f32) as u32,
            sy.floor().clamp(0.0, self.height.saturating_sub(1) as f32) as u32,
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

pub fn fill_cells(_img_w: u32, _img_h: u32, avail_cols: u16, avail_rows: u16) -> (u16, u16) {
    (avail_cols.max(1), avail_rows.max(1))
}

pub fn crop_rect(img_w: u32, img_h: u32, avail_cols: u16, avail_rows: u16) -> (f32, f32, f32, f32) {
    let img_w = img_w.max(1) as f32;
    let img_h = img_h.max(1) as f32;
    let cell_aspect = avail_cols.max(1) as f32 / (avail_rows.max(1) as f32 * 2.0);
    let img_aspect = img_w / img_h;
    if img_aspect > cell_aspect {
        let w = img_h * cell_aspect;
        let x = (img_w - w) / 2.0;
        (x, 0.0, w, img_h)
    } else {
        let h = img_w / cell_aspect;
        let y = (img_h - h) / 2.0;
        (0.0, y, img_w, h)
    }
}

pub fn placeholder(cols: u16, rows: u16) -> Vec<Line<'static>> {
    raster_placeholder(fit_cells(1, 1, cols, rows))
}

pub fn fill_placeholder(cols: u16, rows: u16) -> Vec<Line<'static>> {
    raster_placeholder(fill_cells(1, 1, cols, rows))
}

fn raster_placeholder((cols, rows): (u16, u16)) -> Vec<Line<'static>> {
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

    #[test]
    fn fill_cover_uses_every_cell() {
        let (cols, rows) = fill_cells(100, 100, 40, 12);
        assert_eq!((cols, rows), (40, 12));
        let (cols, rows) = fill_cells(200, 100, 20, 20);
        assert_eq!((cols, rows), (20, 20));
    }

    #[test]
    fn crop_rect_center_crops_taller_image() {
        let (x, y, w, h) = crop_rect(100, 100, 40, 12);
        assert_eq!(x, 0.0);
        assert!((w - 100.0).abs() < 0.01);
        assert!((h - 60.0).abs() < 0.01);
        assert!((y - 20.0).abs() < 0.01);
    }

    #[test]
    fn crop_rect_center_crops_wider_image() {
        let (x, y, w, h) = crop_rect(200, 100, 20, 20);
        assert_eq!(y, 0.0);
        assert!((h - 100.0).abs() < 0.01);
        assert!((w - 50.0).abs() < 0.01);
        assert!((x - 75.0).abs() < 0.01);
    }

    #[test]
    fn fill_lines_cover_the_cell_grid() {
        let mut rgb = vec![0u8; 40 * 40 * 3];
        rgb[0] = 255;
        let cover = CoverArt {
            width: 40,
            height: 40,
            rgb,
        };
        let lines = cover.fill_lines(40, 12);
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[0].spans.len(), 40);
    }
}
