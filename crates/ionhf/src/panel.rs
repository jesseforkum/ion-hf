//! The lamp panel and the speed level readout, painted: lamps that say
//! what the modem is doing at a glance, and the level on the ladder.

use std::sync::OnceLock;

use eframe::egui::{self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use ion::level::{Guard, LADDER, Wave};
use ion::phy_mfsk::MfskMode;

use crate::theme;

#[derive(Clone, Copy, PartialEq)]
pub enum Lit {
    Off,
    /// Ready but idle: the colour shows faintly (a host port with no
    /// client, groups waiting for the link to end).
    Armed(Color32),
    On(Color32),
}

pub struct Lamp {
    pub legend: &'static str,
    pub lit: Lit,
    /// The lamp does something when clicked.
    pub click: bool,
}

impl Lamp {
    pub fn new(legend: &'static str, lit: Lit) -> Self {
        Lamp {
            legend,
            lit,
            click: false,
        }
    }

    pub fn on(legend: &'static str, on: bool, color: Color32) -> Self {
        Lamp::new(legend, if on { Lit::On(color) } else { Lit::Off })
    }
}

/// Unlit glass: the panel's colour with a trace of the lamp's.
fn dark(c: Color32) -> Color32 {
    theme::INSET.lerp_to_gamma(c, 0.07)
}

/// Legend of an unlit lamp: dim but readable.
fn dim_ink() -> Color32 {
    theme::MUTED.lerp_to_gamma(theme::BG, 0.38)
}

fn glow(p: &egui::Painter, r: Rect, c: Color32, round: f32, s: f32) {
    let shadow = egui::Shadow {
        offset: [0, 0],
        blur: (12.0 * s) as u8,
        spread: 0,
        color: c.gamma_multiply(0.33),
    };
    p.add(shadow.as_shape(r, CornerRadius::same(round as u8)));
}

/// One annunciator in `r`: a lens with its legend, lit from behind.
fn paint(p: &egui::Painter, r: Rect, lamp: &Lamp, s: f32, hovered: bool) {
    let round = 2.0;
    let (fill, edge, ink) = match lamp.lit {
        Lit::Off => (dark(theme::MUTED), theme::LINE, dim_ink()),
        Lit::Armed(c) => (
            dark(c),
            c.lerp_to_gamma(theme::BG, 0.45),
            c.lerp_to_gamma(theme::BG, 0.2),
        ),
        Lit::On(c) => {
            glow(p, r, c, round, s);
            (c.lerp_to_gamma(theme::BG, 0.12), c, theme::BG)
        }
    };
    let edge = if hovered { theme::TEXT } else { edge };
    p.rect(r, round, fill, Stroke::new(1.0, edge), StrokeKind::Inside);
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        lamp.legend,
        FontId::monospace(12.0 * s),
        ink,
    );
}

/// Lamp rows, each row sharing `width` evenly; `gap_before` puts extra
/// space above that row (a group of its own). Returns the clicked lamp's
/// legend.
pub fn block(
    ui: &mut egui::Ui,
    rows: &[&[Lamp]],
    width: f32,
    s: f32,
    gap_before: Option<usize>,
) -> Option<&'static str> {
    let (h, gap) = (26.0 * s, 6.0 * s);
    let extra = if gap_before.is_some() { 6.0 * s } else { 0.0 };
    let height = rows.len() as f32 * (h + gap) - gap + extra;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let mut clicked = None;
    let mut y = rect.min.y;
    for (i, row) in rows.iter().enumerate() {
        if gap_before == Some(i) {
            y += extra;
        }
        let n = row.len().max(1) as f32;
        let w = (width - gap * (n - 1.0)) / n;
        for (j, lamp) in row.iter().enumerate() {
            let r = Rect::from_min_size(pos2(rect.min.x + j as f32 * (w + gap), y), vec2(w, h));
            let mut hovered = false;
            if lamp.click {
                let resp = ui.interact(r, ui.scope_id().with(("lamp", lamp.legend)), Sense::click());
                hovered = resp.hovered();
                if resp.clicked() {
                    clicked = Some(lamp.legend);
                }
                resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
            paint(ui.painter(), r, lamp, s, hovered);
        }
        y += h + gap;
    }
    clicked
}

/// The waveform family of a level, as its lamp says.
pub fn family(l: usize) -> &'static str {
    match LADDER.get(l).map(|x| x.wave) {
        Some(Wave::Mfsk(MfskMode::R0)) => "MFSK R0",
        Some(Wave::Mfsk(MfskMode::R1)) => "MFSK R1",
        Some(Wave::Ofdm(Guard::Slice)) => "SLICE",
        Some(Wave::Ofdm(Guard::Long)) => "LONG",
        Some(Wave::Ofdm(Guard::Normal)) => "OFDM",
        Some(Wave::Lifeline) | None => "",
    }
}

pub const FAMILIES: [&str; 5] = ["MFSK R0", "MFSK R1", "SLICE", "LONG", "OFDM"];

/// Payload bit/s of each level in full bursts (frames as sent, before
/// acknowledgements and turnarounds).
pub fn level_bps(l: usize) -> f64 {
    use ion::link::Air;
    static RATES: OnceLock<Vec<f64>> = OnceLock::new();
    let rates = RATES.get_or_init(|| {
        let air = ion::link::audio::PhyAir;
        LADDER
            .iter()
            .enumerate()
            .map(|(code, lv)| {
                let frames = air.max_bits(lv.wave, code) / lv.coded_bits();
                let secs = air.duration(lv.wave, code, frames * lv.coded_bits());
                (frames * lv.frame_bytes() * 8) as f64 / secs.max(1e-3)
            })
            .collect()
    });
    rates.get(l).copied().unwrap_or(0.0)
}

/// "214 bit/s", "3.67 kbit/s".
pub fn bps_text(b: f64) -> String {
    if b < 1000.0 {
        format!("{b:.0} bit/s")
    } else {
        format!("{:.2} kbit/s", b / 1000.0)
    }
}

/// What the readout shows.
pub struct Readout {
    /// The level on the air now (ours while we send, the peer's while it
    /// does), and who sends it.
    pub level: Option<usize>,
    pub sending: bool,
    pub ours: Option<usize>,
    pub peer: Option<usize>,
    /// The highest level the peer's reports allow us.
    pub ceiling: Option<usize>,
    /// The link holds below the floor (`ion` docs/link.md, Lifeline).
    pub hold: bool,
}

/// The current level, large, with its name and rate, over the ladder:
/// the levels up to it lit, the ceiling marked above, our and the peer's
/// levels below.
pub fn readout(ui: &mut egui::Ui, r: &Readout, width: f32, s: f32) {
    let height = 96.0 * s;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let p = ui.painter_at(rect.expand(2.0));
    let color = if r.sending { theme::US } else { theme::PEER };
    let (big, name, sub) = match r.level {
        _ if r.hold => ("Hold".into(), String::new(), String::new()),
        Some(l) => {
            let short = theme::level_short(l);
            let name = short
                .split_once(' ')
                .map_or(String::new(), |(_, n)| n.replace("MFSK ", ""));
            (
                format!("L{l}"),
                name,
                format!("{}  {}", family(l), bps_text(level_bps(l))),
            )
        }
        None => ("L—".into(), String::new(), String::new()),
    };
    let g = p.layout_no_wrap(
        big,
        FontId::monospace(34.0 * s),
        if r.level.is_some() || r.hold {
            color
        } else {
            theme::MUTED.lerp_to_gamma(theme::BG, 0.4)
        },
    );
    let big_w = g.size().x;
    p.galley(rect.min, g, theme::TEXT);
    let x = rect.min.x + big_w + 14.0 * s;
    p.text(
        pos2(x, rect.min.y + 3.0 * s),
        Align2::LEFT_TOP,
        name,
        FontId::proportional(18.0 * s),
        theme::TEXT,
    );
    p.text(
        pos2(x, rect.min.y + 26.0 * s),
        Align2::LEFT_TOP,
        sub,
        FontId::monospace(12.0 * s),
        theme::MUTED,
    );
    ladder(
        &p,
        Rect::from_min_max(pos2(rect.min.x, rect.min.y + 48.0 * s), rect.max),
        r,
        color,
        s,
    );
}

fn ladder(p: &egui::Painter, rect: Rect, r: &Readout, color: Color32, s: f32) {
    let n = LADDER.len();
    let group_gap = 5.0 * s;
    let groups = (1..n).filter(|&l| family(l) != family(l - 1)).count();
    let gap = 2.0 * s;
    let w = (rect.width() - groups as f32 * group_gap - (n - 1) as f32 * gap) / n as f32;
    let top = rect.min.y + 9.0 * s;
    let cell_h = 16.0 * s;
    let mut x = rect.min.x;
    let mut centers = Vec::with_capacity(n);
    for l in 0..n {
        if l > 0 && family(l) != family(l - 1) {
            x += group_gap;
        }
        let cell = Rect::from_min_size(pos2(x, top), vec2(w, cell_h));
        centers.push(cell.center().x);
        let fill = match r.level {
            Some(cur) if l == cur => color,
            Some(cur) if l < cur => color.lerp_to_gamma(theme::BG, 0.62),
            _ => theme::INSET,
        };
        let stroke = if r.level.is_some_and(|cur| l <= cur) {
            Stroke::NONE
        } else {
            Stroke::new(1.0, theme::LINE)
        };
        p.rect(cell, 1.0, fill, stroke, StrokeKind::Inside);
        if l % 4 == 0 || l == n - 1 {
            p.text(
                pos2(cell.center().x, cell.max.y + 9.0 * s),
                Align2::CENTER_TOP,
                l.to_string(),
                FontId::monospace(10.0 * s),
                theme::MUTED,
            );
        }
        x += w + gap;
    }
    let tri = |c: Pos2, down: bool, col: Color32| {
        let k = 4.5 * s;
        let pts = if down {
            vec![pos2(c.x - k, c.y - k), pos2(c.x + k, c.y - k), pos2(c.x, c.y)]
        } else {
            vec![pos2(c.x - k, c.y + k), pos2(c.x + k, c.y + k), pos2(c.x, c.y)]
        };
        p.add(egui::Shape::convex_polygon(pts, col, Stroke::NONE));
    };
    if let Some(c) = r.ceiling.and_then(|l| centers.get(l)) {
        tri(pos2(*c, top - 2.0 * s), true, theme::TEXT);
    }
    let below = top + cell_h + 2.0 * s;
    if let Some(c) = r.ours.and_then(|l| centers.get(l)) {
        tri(pos2(*c, below), false, theme::US);
    }
    if let Some(c) = r.peer.and_then(|l| centers.get(l)) {
        tri(pos2(*c, below), false, theme::PEER);
    }
}

/// What has gone of what is queued: the figures, then a slim bar.
pub fn progress(ui: &mut egui::Ui, frac: f32, text: &str, width: f32, s: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0 * s), Sense::hover());
    let p = ui.painter_at(rect);
    let g = p.layout_no_wrap(text.to_string(), FontId::monospace(12.0 * s), theme::TEXT);
    let x = rect.min.x + g.size().x + 10.0 * s;
    p.galley(pos2(rect.min.x, rect.center().y - g.size().y / 2.0), g, theme::TEXT);
    let track = Rect::from_min_max(
        pos2(x, rect.center().y - 3.0 * s),
        pos2(rect.max.x, rect.center().y + 3.0 * s),
    );
    p.rect_filled(track, 1.0, theme::INSET);
    p.rect_stroke(track, 1.0, Stroke::new(1.0, theme::LINE), StrokeKind::Inside);
    let fill = Rect::from_min_max(
        track.min,
        pos2(track.min.x + track.width() * frac.clamp(0.0, 1.0), track.max.y),
    );
    p.rect_filled(fill, 1.0, theme::US);
}
