//! The decode plot: what the last bursts heard looked like to the
//! receiver. OFDM: the equalised data symbols over the constellation's
//! points; MFSK: each symbol's likeliest tone share against 1 less the
//! runner-up's (Data2G's tone plot), good symbols top right. Older bursts
//! fade.

use std::collections::VecDeque;
use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use ion::link::audio::RxPlot;
use ion::link::node::BurstPlot;

use crate::theme;

/// Bursts drawn, newest first.
const KEEP: usize = 3;
const FADE: [f32; KEEP] = [0.85, 0.32, 0.14];

#[derive(Default)]
pub struct Plots {
    recent: VecDeque<Arc<BurstPlot>>,
}

impl Plots {
    pub fn push(&mut self, p: Arc<BurstPlot>) {
        self.recent.push_front(p);
        self.recent.truncate(KEEP);
    }

    pub fn clear(&mut self) {
        self.recent.clear();
    }

    /// The plot in a `side` square, its newest burst's line under it.
    pub fn show(&self, ui: &mut egui::Ui, side: f32, s: f32) {
        let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
        let cap = 18.0 * s;
        let area = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.max.y - cap));
        let p = ui.painter_at(rect);
        p.rect(area, 0.0, theme::BG, Stroke::new(1.0, theme::LINE), StrokeKind::Inside);
        let inner = area.shrink(1.0);
        let small = FontId::monospace(11.0 * s);
        let Some(newest) = self.recent.front() else {
            p.text(inner.center(), Align2::CENTER_CENTER, "—", small, theme::MUTED);
            return;
        };
        let tones = matches!(newest.plot, RxPlot::Tones(_));
        let same: Vec<_> = self
            .recent
            .iter()
            .zip(FADE)
            .filter(|(b, _)| matches!(b.plot, RxPlot::Tones(_)) == tones)
            .collect();
        if tones {
            tone_axes(&p, inner);
        } else {
            iq_axes(&p, inner);
        }
        let dot = (1.2 * s).max(1.1);
        // Dots stay in the frame: at low S:N they scatter far past it.
        let dots = p.with_clip_rect(inner);
        for (b, alpha) in same.iter().rev() {
            let c = theme::PEER.gamma_multiply(*alpha);
            match &b.plot {
                RxPlot::Iq { constellation, symbols } => {
                    let reach = reach(&constellation.unit_points());
                    for z in symbols {
                        dots.circle_filled(iq_pos(inner, reach, z.re, z.im), dot, c);
                    }
                }
                RxPlot::Tones(pts) => {
                    for [x, y] in pts {
                        dots.circle_filled(tone_pos(inner, *x, *y), dot, c);
                    }
                }
            }
        }
        // The reference points over the symbols, so they show through.
        let mark = Stroke::new(1.0, theme::TEXT.gamma_multiply(0.7));
        let rad = (inner.width() / 90.0).clamp(2.0, 4.0);
        match &newest.plot {
            RxPlot::Iq { constellation, .. } => {
                let pts = constellation.unit_points();
                let reach = reach(&pts);
                for z in &pts {
                    p.circle_stroke(iq_pos(inner, reach, z.re, z.im), rad, mark);
                }
            }
            RxPlot::Tones(_) => {
                p.circle_stroke(tone_pos(inner, 1.0, 1.0), rad, mark);
            }
        }
        // The newest burst: what it was, its S/N and how it fared.
        let y = rect.max.y - cap / 2.0;
        let what = newest.what.replacen("DATA ", "", 1).replacen(" OFDM", "", 1);
        let what = what.split(" ×").next().unwrap_or_default();
        let (verdict, color) = if newest.lost == 0 {
            ("OK".to_string(), theme::OK)
        } else {
            (format!("LOST {}", newest.lost), theme::BAD)
        };
        let v = p.text(pos2(rect.max.x, y), Align2::RIGHT_CENTER, verdict, small.clone(), color);
        // The S/N goes first when the line is short of room.
        let room = rect.width() - v.width() - 8.0 * s;
        let mut line = format!("{what}  {:+.0} dB", newest.snr_db);
        if p.layout_no_wrap(line.clone(), small.clone(), theme::MUTED).size().x > room {
            line = what.to_string();
        }
        let g = p.layout_no_wrap(line, small, theme::MUTED);
        let clip = Rect::from_min_max(rect.min, pos2(rect.min.x + room, rect.max.y));
        p.with_clip_rect(clip)
            .galley(pos2(rect.min.x, y - g.size().y / 2.0), g, theme::MUTED);
    }
}

/// Equal scales on both axes, the origin in the middle.
fn iq_pos(r: Rect, reach: f32, x: f32, y: f32) -> egui::Pos2 {
    let k = r.width().min(r.height()) / 2.0 / reach;
    pos2(r.center().x + x * k, r.center().y - y * k)
}

fn reach(points: &[ion_dsp::dsp::Complex]) -> f32 {
    let r = points.iter().fold(0f32, |m, z| m.max(z.re.abs()).max(z.im.abs()));
    1.5 * if r > 0.0 { r } else { 1.0 }
}

fn faint() -> Color32 {
    theme::LINE.lerp_to_gamma(theme::BG, 0.3)
}

fn iq_axes(p: &egui::Painter, r: Rect) {
    let c = r.center();
    let line = Stroke::new(1.0, faint());
    p.line_segment([pos2(r.min.x, c.y), pos2(r.max.x, c.y)], line);
    p.line_segment([pos2(c.x, r.min.y), pos2(c.x, r.max.y)], line);
}

/// Tone plot space: x from 0.5 (1 less the runner-up's share, which is
/// never above a half) and y from 0, both to 1.04, so the ideal (1, 1)
/// sits just inside the corner.
fn tone_pos(r: Rect, x: f32, y: f32) -> egui::Pos2 {
    let (x0, top) = (0.5, 1.04);
    pos2(
        r.min.x + (x - x0) / (top - x0) * r.width(),
        r.max.y - y / top * r.height(),
    )
}

fn tone_axes(p: &egui::Painter, r: Rect) {
    let line = Stroke::new(1.0, faint());
    for k in 1..=4 {
        let (x, y) = (0.5 + 0.125 * k as f32, 0.25 * k as f32);
        p.line_segment([tone_pos(r, x, 0.0), tone_pos(r, x, 1.04)], line);
        p.line_segment([tone_pos(r, 0.5, y), tone_pos(r, 1.04, y)], line);
    }
}
