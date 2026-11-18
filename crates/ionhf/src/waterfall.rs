//! Waterfall: one texture, a new row per spectrum snapshot.

use eframe::egui::{self, Color32, ColorImage, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2};

use crate::engine::{SPECTRUM_BINS, SPECTRUM_HZ};

const ROWS: usize = 256;
const RANGE_DB: f32 = 45.0;

pub struct Waterfall {
    pixels: Vec<Color32>,
    texture: Option<TextureHandle>,
    floor_db: Option<f32>,
    scratch: Vec<f32>,
    dirty: bool,
}

/// A stretch of the passband to shade.
pub struct Band {
    pub lo_hz: f32,
    pub hi_hz: f32,
    pub color: Color32,
    pub label: String,
}

pub struct Cursor {
    pub center_hz: f32,
    /// Offsets from `center_hz` to mark with lines, Hz.
    pub marks: [f32; 2],
    pub keyed: bool,
}

fn palette(v: f32) -> Color32 {
    const STOPS: [(f32, [u8; 3]); 7] = [
        (0.0, [0, 0, 0]),
        (0.15, [0, 0, 90]),
        (0.35, [0, 70, 200]),
        (0.55, [0, 200, 220]),
        (0.75, [240, 240, 0]),
        (0.9, [255, 70, 0]),
        (1.0, [255, 255, 255]),
    ];
    let v = v.clamp(0.0, 1.0);
    for w in STOPS.windows(2) {
        let (a, ca) = w[0];
        let (b, cb) = w[1];
        if v <= b {
            let t = (v - a) / (b - a);
            let mix = |i: usize| (ca[i] as f32 + t * (cb[i] as f32 - ca[i] as f32)) as u8;
            return Color32::from_rgb(mix(0), mix(1), mix(2));
        }
    }
    Color32::WHITE
}

impl Waterfall {
    pub fn new() -> Self {
        Self {
            pixels: vec![Color32::BLACK; SPECTRUM_BINS * ROWS],
            texture: None,
            floor_db: None,
            scratch: Vec::with_capacity(SPECTRUM_BINS),
            dirty: true,
        }
    }

    pub fn push_row(&mut self, row_db: &[f32]) {
        // Noise floor: 30th percentile of the row, smoothed.
        self.scratch.clear();
        self.scratch.extend_from_slice(row_db);
        let k = self.scratch.len() * 3 / 10;
        let (_, p, _) = self.scratch.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
        let p = *p;
        let floor = self.floor_db.get_or_insert(p);
        *floor += 0.05 * (p - *floor);
        let floor = *floor;

        let w = SPECTRUM_BINS;
        self.pixels.copy_within(0..(ROWS - 1) * w, w);
        for (px, &db) in self.pixels[..w].iter_mut().zip(row_db) {
            *px = palette((db - floor + 3.0) / RANGE_DB);
        }
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.pixels.fill(Color32::BLACK);
        self.floor_db = None;
        self.dirty = true;
    }

    /// Draws the waterfall with its frequency scale, `bands` shaded over it
    /// and the frequency under the pointer.
    pub fn show(&mut self, ui: &mut egui::Ui, height: f32, cursor: &Cursor, bands: &[Band]) {
        let scale_h = 14.0;
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height + scale_h), Sense::hover());
        let wf_rect = Rect::from_min_max(Pos2::new(rect.min.x, rect.min.y + scale_h), rect.max);

        let image = || ColorImage::new([SPECTRUM_BINS, ROWS], self.pixels.clone());
        match &mut self.texture {
            Some(t) if self.dirty => t.set(image(), TextureOptions::LINEAR),
            Some(_) => {}
            None => {
                self.texture = Some(ui.ctx().load_texture("waterfall", image(), TextureOptions::LINEAR));
            }
        }
        self.dirty = false;

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
        if let Some(t) = &self.texture {
            painter.image(
                t.id(),
                wf_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        let x_of = |hz: f32| wf_rect.min.x + hz / SPECTRUM_HZ * wf_rect.width();
        let text_color = ui.visuals().text_color();
        for hz in (0..=3000).step_by(100) {
            let x = x_of(hz as f32);
            let major = hz % 500 == 0;
            let len = if major { 6.0 } else { 3.0 };
            painter.line_segment(
                [Pos2::new(x, wf_rect.min.y - len), Pos2::new(x, wf_rect.min.y)],
                Stroke::new(1.0, text_color),
            );
            if major && hz > 0 && hz < 3000 {
                painter.text(
                    Pos2::new(x, rect.min.y),
                    egui::Align2::CENTER_TOP,
                    format!("{hz}"),
                    egui::FontId::proportional(10.0),
                    text_color,
                );
            }
        }

        for b in bands {
            let r = Rect::from_min_max(
                Pos2::new(x_of(b.lo_hz), wf_rect.min.y),
                Pos2::new(x_of(b.hi_hz), wf_rect.max.y),
            );
            painter.rect_filled(r, 0.0, b.color.gamma_multiply(0.12));
            painter.rect_filled(
                Rect::from_min_max(r.min, Pos2::new(r.max.x, r.min.y + 3.0)),
                0.0,
                b.color,
            );
            painter.text(
                Pos2::new(r.center().x, r.min.y + 5.0),
                egui::Align2::CENTER_TOP,
                &b.label,
                egui::FontId::proportional(11.0),
                b.color,
            );
        }

        let color = if cursor.keyed {
            Color32::RED
        } else {
            Color32::from_rgb(255, 200, 0)
        };
        for off in cursor.marks {
            let x = x_of(cursor.center_hz + off);
            painter.line_segment(
                [Pos2::new(x, wf_rect.min.y), Pos2::new(x, wf_rect.max.y)],
                Stroke::new(1.0, color.gamma_multiply(0.8)),
            );
        }
        // The centre: a tick and a triangle at the top.
        let xc = x_of(cursor.center_hz);
        painter.line_segment(
            [Pos2::new(xc, wf_rect.min.y), Pos2::new(xc, wf_rect.min.y + 10.0)],
            Stroke::new(2.0, Color32::WHITE),
        );
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(xc - 5.0, wf_rect.min.y - 6.0),
                Pos2::new(xc + 5.0, wf_rect.min.y - 6.0),
                Pos2::new(xc, wf_rect.min.y),
            ],
            color,
            Stroke::NONE,
        ));

        let hz_at = |x: f32| ((x - wf_rect.min.x) / wf_rect.width() * SPECTRUM_HZ).clamp(0.0, SPECTRUM_HZ);
        if let Some(pos) = response.hover_pos() {
            painter.text(
                pos + Vec2::new(8.0, -14.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{:.0} Hz", hz_at(pos.x)),
                egui::FontId::monospace(11.0),
                Color32::WHITE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_scroll_down() {
        let mut w = Waterfall::new();
        let hot: Vec<f32> = (0..SPECTRUM_BINS).map(|i| if i == 10 { 0.0 } else { -100.0 }).collect();
        w.push_row(&hot);
        let cold = vec![-100.0; SPECTRUM_BINS];
        w.push_row(&cold);
        // Newest row on top; the hot bin is now in row 1.
        assert_ne!(w.pixels[SPECTRUM_BINS + 10], w.pixels[SPECTRUM_BINS + 11]);
        assert_eq!(w.pixels[10], w.pixels[11]);
    }

    #[test]
    fn palette_is_monotone_in_brightness() {
        let lum = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert_eq!(palette(0.0), Color32::BLACK);
        assert_eq!(palette(1.0), Color32::WHITE);
        assert!(lum(palette(0.3)) < lum(palette(0.7)));
    }
}
