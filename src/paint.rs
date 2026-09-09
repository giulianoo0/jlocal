//! Software canvas for the status window: flat fills, rounded pills, dots
//! and Inter text, blended into a `0x00RRGGBB` buffer that softbuffer
//! presents. Everything takes logical points and scales itself.

use fontdue::{Font, FontSettings};
use std::sync::OnceLock;

pub const REGULAR_TTF: &[u8] = include_bytes!("../assets/Inter-Regular.ttf");
pub const SEMIBOLD_TTF: &[u8] = include_bytes!("../assets/Inter-SemiBold.ttf");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Semibold,
}

fn font(weight: Weight) -> &'static Font {
    static REGULAR: OnceLock<Font> = OnceLock::new();
    static SEMIBOLD: OnceLock<Font> = OnceLock::new();
    let (cell, bytes) = match weight {
        Weight::Regular => (&REGULAR, REGULAR_TTF),
        Weight::Semibold => (&SEMIBOLD, SEMIBOLD_TTF),
    };
    cell.get_or_init(|| {
        Font::from_bytes(bytes, FontSettings::default()).expect("bundled Inter parses")
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub scale: f32,
    pub pixels: Vec<u32>,
}

fn blend(dst: u32, src: u32, alpha: f32) -> u32 {
    if alpha >= 1.0 {
        return src;
    }
    let mix = |shift: u32| {
        let d = ((dst >> shift) & 0xff) as f32;
        let s = ((src >> shift) & 0xff) as f32;
        ((d + (s - d) * alpha).round() as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}

impl Canvas {
    pub fn new(width: usize, height: usize, scale: f32, background: u32) -> Self {
        Self {
            width,
            height,
            scale,
            pixels: vec![background; width * height],
        }
    }

    fn put(&mut self, x: i64, y: i64, color: u32, alpha: f32) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 || alpha <= 0.0 {
            return;
        }
        let i = y as usize * self.width + x as usize;
        self.pixels[i] = blend(self.pixels[i], color, alpha);
    }

    /// Rounded rectangle with antialiased corners; `radius` in points.
    pub fn round_rect(&mut self, rect: Rect, radius: f32, color: u32) {
        let s = self.scale;
        let (x0, y0, w, h) = (rect.x * s, rect.y * s, rect.w * s, rect.h * s);
        let r = (radius * s).min(w / 2.0).min(h / 2.0);
        let (px0, py0) = (x0.floor() as i64, y0.floor() as i64);
        let (px1, py1) = ((x0 + w).ceil() as i64, (y0 + h).ceil() as i64);
        for py in py0..py1 {
            for px in px0..px1 {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                let dx = (x0 + r - cx).max(cx - (x0 + w - r)).max(0.0);
                let dy = (y0 + r - cy).max(cy - (y0 + h - r)).max(0.0);
                let dist = (dx * dx + dy * dy).sqrt();
                let inside = if r > 0.0 { r - dist } else { 1.0 };
                let edge_x = (cx - x0).min(x0 + w - cx);
                let edge_y = (cy - y0).min(y0 + h - cy);
                let alpha = inside.min(edge_x + 0.5).min(edge_y + 0.5).clamp(0.0, 1.0);
                self.put(px, py, color, alpha);
            }
        }
    }

    pub fn dot(&mut self, cx: f32, cy: f32, radius: f32, color: u32) {
        self.round_rect(
            Rect {
                x: cx - radius,
                y: cy - radius,
                w: radius * 2.0,
                h: radius * 2.0,
            },
            radius,
            color,
        );
    }

    pub fn text_width(&self, text: &str, size: f32, weight: Weight) -> f32 {
        let f = font(weight);
        let px = size * self.scale;
        text.chars()
            .map(|c| f.metrics(c, px).advance_width)
            .sum::<f32>()
            / self.scale
    }

    /// Draws `text` with its baseline sitting where a line of `size` points
    /// starting at `y` would put it. Returns the advance in points.
    pub fn text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        weight: Weight,
        color: u32,
    ) -> f32 {
        let f = font(weight);
        let px = size * self.scale;
        let ascent = f
            .horizontal_line_metrics(px)
            .map(|m| m.ascent)
            .unwrap_or(px * 0.8);
        let baseline = y * self.scale + ascent;
        let mut pen = x * self.scale;
        for c in text.chars() {
            let (metrics, bitmap) = f.rasterize(c, px);
            let left = pen + metrics.xmin as f32;
            let top = baseline - metrics.ymin as f32 - metrics.height as f32;
            for (row, line) in bitmap.chunks(metrics.width.max(1)).enumerate() {
                for (col, &coverage) in line.iter().enumerate() {
                    self.put(
                        left.round() as i64 + col as i64,
                        top.round() as i64 + row as i64,
                        color,
                        coverage as f32 / 255.0,
                    );
                }
            }
            pen += metrics.advance_width;
        }
        (pen - x * self.scale) / self.scale
    }
}
