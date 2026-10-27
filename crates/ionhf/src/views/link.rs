//! Link: the session as it happens. Two rows across the top, each
//! station's figures on its side (local left, remote right) and the link's
//! between; the text, or levels, SNRs and data over time, or every burst,
//! across the rest.

use crate::ion_engine::IonStatus;
use eframe::egui::{self, Margin, RichText};
use ion::level::LADDER;
use ion::link::Role;

use crate::app::App;
use crate::charts::{self, Series, Style};
use crate::theme;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let s = app.link.clone();
    egui::Panel::top("mirror")
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::ZERO))
        .show(ui, |ui| mirror(app, ui, s.as_ref()));
    egui::Panel::top("exchange")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .inner_margin(Margin::symmetric(14, 8)),
        )
        .show(ui, |ui| exchange(app, ui, s.as_ref()));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(10)))
        .show(ui, |ui| center(app, ui));
}

/// Bytes moved both ways.
fn moved(s: &IonStatus) -> u64 {
    s.bytes_acked + s.bytes_received
}

/// "L6 QPSK 1/3" → ("L6", "QPSK 1/3"); "5.6 kB" → ("5.6", "kB").
fn split(text: String) -> (String, String) {
    match text.split_once(' ') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (text, String::new()),
    }
}

/// The exchange, one row: our side justified left, the peer's right, and
/// between them the band, with what the link is using on it.
fn exchange(app: &App, ui: &mut egui::Ui, s: Option<&IonStatus>) {
    let h = 40.0;
    let row = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), h));
    ui.allocate_rect(row, egui::Sense::hover());
    let side = |ui: &mut egui::Ui, layout: egui::Layout, f: &mut dyn FnMut(&mut egui::Ui)| {
        ui.scope_builder(egui::UiBuilder::new().max_rect(row).layout(layout), |ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            f(ui);
        });
    };
    let left = egui::Layout::left_to_right(egui::Align::Center);
    let right = egui::Layout::right_to_left(egui::Align::Center);
    let Some(s) = s else {
        side(ui, left, &mut |ui| {
            station(ui, &app.cfg.call, theme::US, "Local", egui::Align::Min);
            ui.label(RichText::new("No link").color(theme::MUTED));
        });
        return;
    };
    // Ours, from the left edge.
    side(ui, left, &mut |ui| {
        let total = s.bytes_acked + s.pending as u64;
        let frac = if total == 0 {
            0.0
        } else {
            s.bytes_acked as f32 / total as f32
        };
        ui.scope(|ui| {
            ui.set_width(170.0);
            theme::bar(ui, frac, theme::US, &format!("ACK {} / {}", s.bytes_acked, total));
        });
        item(ui, "Bursts", &s.data_bursts.to_string(), egui::Align::Min);
        item(ui, "Control", &s.control_bursts.to_string(), egui::Align::Min);
        item(
            ui,
            "Frames",
            &format!("{} / {}", s.frames_sent, s.frames_resent),
            egui::Align::Min,
        );
        item(ui, "HARQ", &s.increments_sent.to_string(), egui::Align::Min);
        item(ui, "Polls", &s.polls.to_string(), egui::Align::Min);
        item(ui, "Air", &format!("{:.1} s", s.airtime_s), egui::Align::Min);
    });
    // The peer's, from the right edge: its call outermost.
    side(ui, right, &mut |ui| {
        item(ui, "Saved", &s.harq_recovered.to_string(), egui::Align::Max);
        item(ui, "Lost", &s.codewords_failed.to_string(), egui::Align::Max);
        item(ui, "Heard", &app.hist.bursts_heard.to_string(), egui::Align::Max);
    });
    // The band in the middle.
    let w = 132.0;
    let mid = egui::Rect::from_center_size(egui::pos2(row.center().x, row.center().y), egui::vec2(w, h));
    ui.scope_builder(egui::UiBuilder::new().max_rect(mid), |ui| band(app, ui, s));
}

/// A call in its colour, "Local" or "Remote" under it, aligned to its side.
fn station(ui: &mut egui::Ui, call: &str, color: egui::Color32, side: &str, align: egui::Align) {
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(call).monospace().size(16.0).strong().color(color));
        ui.label(RichText::new(side).size(11.0).color(theme::MUTED));
    });
}

fn peer_call(s: &IonStatus) -> &str {
    if s.peer.is_empty() { "—" } else { &s.peer }
}

/// Each station's figures: (local, remote), each (label, value, unit). A
/// level is a station's while it sends.
#[allow(clippy::type_complexity)]
fn sides(
    app: &App,
    s: Option<&IonStatus>,
) -> (Vec<(&'static str, String, String)>, Vec<(&'static str, String, String)>) {
    let sending = s.is_some_and(|s| s.role == Role::Sender);
    let dash = || ("—".to_string(), String::new());
    let hold = || ("Hold".to_string(), String::new());
    let (llv, llu) = match s {
        Some(s) if s.holding => hold(),
        Some(s) if sending => split(theme::level(s.level)),
        _ => dash(),
    };
    let (av, au) = match s {
        Some(s) if sending => split(theme::level(s.target_level)),
        _ => dash(),
    };
    let (qv, qu) = s.map_or_else(dash, |s| split(theme::bytes(s.pending as u64)));
    let (rlv, rlu) = match s {
        Some(s) if s.holding => hold(),
        Some(_) if !sending => app.hist.peer_level.map_or_else(dash, |l| split(theme::level(l))),
        _ => dash(),
    };
    let (rxv, rxu) = s.map_or_else(dash, |s| split(theme::bytes(s.bytes_received)));
    (
        vec![
            ("Level", llv, llu),
            ("Aim", av, au),
            ("Rpt Rcvd", theme::db(app.hist.last_there), "dB".into()),
            ("Queue", qv, qu),
        ],
        vec![
            ("Level", rlv, rlu),
            ("Rpt Sent", theme::db(app.hist.last_here), "dB".into()),
            ("RX", rxv, rxu),
        ],
    )
}

/// The link's own figures: rate, data both ways, time.
fn link_figures(s: Option<&IonStatus>) -> Vec<(&'static str, String, String)> {
    let dash = || ("—".to_string(), String::new());
    let rate = s.filter(|s| s.connected_s > 5.0).map_or("—".into(), |s| {
        format!("{:.0}", moved(s) as f64 * 8.0 / s.connected_s)
    });
    let (dv, du) = s.map_or_else(dash, |s| split(theme::bytes(moved(s))));
    vec![
        ("Rate", rate, "bit/s".into()),
        ("Data", dv, du),
        (
            "Time",
            s.map_or("—".into(), |s| theme::clock(s.connected_s)),
            String::new(),
        ),
    ]
}

/// A tile sized to its content: caption, figure in `color`.
fn cell(ui: &mut egui::Ui, label: &str, value: &str, unit: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(theme::INSET)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                theme::caption(ui, label);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(RichText::new(value).monospace().size(18.0).strong().color(color));
                    if !unit.is_empty() {
                        ui.label(RichText::new(unit).monospace().size(12.0).color(theme::MUTED));
                    }
                });
            });
        });
}

/// How wide [`cell`] will draw.
fn cell_w(ui: &egui::Ui, label: &str, value: &str, unit: &str) -> f32 {
    let p = ui.painter();
    let w = |t: &str, f: egui::FontId| p.layout_no_wrap(t.to_string(), f, theme::TEXT).size().x;
    let cap = w(label, egui::FontId::proportional(11.0));
    let fig = w(value, egui::FontId::monospace(18.0))
        + if unit.is_empty() {
            0.0
        } else {
            4.0 + w(unit, egui::FontId::monospace(12.0))
        };
    24.0 + cap.max(fig)
}

/// The top row: the local station's tiles from the left (its
/// call outermost), the link's in the middle, the remote's at the right
/// (its call outermost). One left-to-right row, spaced by measure.
fn mirror(app: &App, ui: &mut egui::Ui, s: Option<&IonStatus>) {
    let (local, remote) = sides(app, s);
    // To fit one row: levels without "OFDM", and no Aim.
    let short = |v: Vec<(&'static str, String, String)>| -> Vec<(&'static str, String, String)> {
        v.into_iter()
            .filter(|f| f.0 != "Aim")
            .map(|(l, v, u)| {
                if l == "Level" {
                    (l, v, u.replace("OFDM ", ""))
                } else {
                    (l, v, u)
                }
            })
            .collect()
    };
    let me = app.cfg.call.clone();
    let mut left = vec![("Local", me, String::new(), theme::US)];
    left.extend(short(local).into_iter().map(|(l, v, u)| (l, v, u, theme::TEXT)));
    let mid: Vec<_> = link_figures(s)
        .into_iter()
        .map(|(l, v, u)| (l, v, u, theme::MUTED))
        .collect();
    let mut right: Vec<_> = short(remote)
        .into_iter()
        .rev()
        .map(|(l, v, u)| (l, v, u, theme::TEXT))
        .collect();
    right.push((
        "Remote",
        s.map_or("—", peer_call).to_string(),
        String::new(),
        theme::PEER,
    ));
    let gap = 1.0;
    let width = |v: &[(&str, String, String, egui::Color32)]| -> f32 {
        v.iter().map(|c| cell_w(ui, c.0, &c.1, &c.2) + gap).sum::<f32>()
    };
    let (wl, wm, wr) = (width(&left), width(&mid), width(&right));
    let avail = ui.available_width();
    let mid_at = ((avail - wm) / 2.0).max(wl + 8.0);
    let pad1 = mid_at - wl;
    let pad2 = (avail - mid_at - wm - wr).max(8.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        for (l, v, u, c) in &left {
            cell(ui, l, v, u, *c);
        }
        ui.add_space(pad1);
        for (l, v, u, c) in &mid {
            cell(ui, l, v, u, *c);
        }
        ui.add_space(pad2);
        for (l, v, u, c) in &right {
            cell(ui, l, v, u, *c);
        }
    });
}

/// A caption over a mono value, aligned to its side.
fn item(ui: &mut egui::Ui, label: &str, value: &str, align: egui::Align) {
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        theme::caption(ui, label);
        ui.label(RichText::new(value).monospace().strong());
    });
}

/// What the link is using of the passband now: one third (a slice level,
/// at its slot; the MFSK levels, the middle) or all of it (the full-band
/// OFDM levels). The level in use is ours as ISS, the peer's as IRS.
fn in_use(app: &App, s: &IonStatus) -> (bool, u8) {
    use ion::level::{Guard, Wave};
    let sending = s.role == Role::Sender;
    let level = if sending { Some(s.level) } else { app.hist.peer_level };
    match level.and_then(|l| LADDER.get(l)).map(|l| l.wave) {
        Some(Wave::Ofdm(Guard::Slice)) => (false, if sending { s.slot_tx } else { s.slot_rx }),
        Some(Wave::Ofdm(_)) => (true, 0),
        _ => (false, 0),
    }
}

/// The band: how clean each third is here, and over it, in our colour,
/// what the link is using. Going from a slice to the full band, the mark
/// spreads from its third across all three (and draws back going down).
fn band(app: &App, ui: &mut egui::Ui, s: &IonStatus) {
    let (full, slot) = in_use(app, s);
    let rect = ui.max_rect();
    let col = rect.width() / 3.0;
    // Thirds by frequency: low, mid, high (slot numbers 1, 0, 2).
    let x_of = |slot: u8| match slot {
        1 => 0.0,
        2 => 2.0,
        _ => 1.0,
    };
    let spread = ui
        .ctx()
        .animate_value_with_time(egui::Id::unique("band_spread"), if full { 1.0 } else { 0.0 }, 0.45);
    let at = ui
        .ctx()
        .animate_value_with_time(egui::Id::unique("band_slot"), x_of(slot), 0.25);
    let p = ui.painter();
    let (top, bot) = (rect.min.y + 2.0, rect.min.y + 22.0);
    for (i, (k, name)) in [(1usize, "Low"), (0, "Mid"), (2, "High")].into_iter().enumerate() {
        let clean = ((100.0 - s.band[k]) / 100.0).clamp(0.0, 1.0);
        let x0 = rect.min.x + i as f32 * col + 3.0;
        let x1 = x0 + col - 6.0;
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, bot)),
            2.0,
            theme::INSET,
        );
        let y = bot - clean * (bot - top);
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1, bot)),
            2.0,
            theme::MUTED.gamma_multiply(0.5),
        );
        let used = if spread > 0.5 {
            true
        } else {
            (x_of(k as u8) - at).abs() < 0.5
        };
        p.text(
            egui::pos2((x0 + x1) / 2.0, bot + 3.0),
            egui::Align2::CENTER_TOP,
            name,
            egui::FontId::proportional(11.0),
            if used { theme::TEXT } else { theme::MUTED },
        );
    }
    // The mark: from its third (spread 0) to the whole band (spread 1).
    let lerp = |a: f32, b: f32| a + (b - a) * spread;
    let x0 = lerp(rect.min.x + at * col + 1.0, rect.min.x + 1.0);
    let x1 = lerp(rect.min.x + (at + 1.0) * col - 1.0, rect.max.x - 1.0);
    let mark = egui::Rect::from_min_max(egui::pos2(x0, top - 2.0), egui::pos2(x1, bot + 2.0));
    p.rect(
        mark,
        3.0,
        theme::US.gamma_multiply(0.18),
        egui::Stroke::new(1.5, theme::US),
        egui::StrokeKind::Inside,
    );
}

/// The centre: the chat (the main thing), or the charts, or every burst.
fn center(app: &mut App, ui: &mut egui::Ui) {
    use crate::app::Center;
    ui.horizontal(|ui| {
        for (v, name) in [(Center::Chat, "Chat"), (Center::Charts, "Charts"), (Center::Log, "Log")] {
            if ui.selectable_label(app.center == v, name).clicked() {
                app.center = v;
            }
        }
    });
    ui.add_space(6.0);
    let h = ui.available_height();
    match app.center {
        Center::Chat => text(app, ui, h),
        Center::Charts => charts(app, ui, h),
        Center::Log => log(app, ui, h),
    }
}

/// Levels, SNRs and data over time, sharing the height.
fn charts(app: &mut App, ui: &mut egui::Ui, h: f32) {
    let k = ((h - 12.0) / 390.0).max(0.6);
    let t = app.hist.t();
    let top = (LADDER.len() - 1) as f32;
    // Every 4th level, so that the top one (16) has its tick.
    let lticks: Vec<(f32, String)> = (0..LADDER.len())
        .step_by(4)
        .map(|l| (l as f32, format!("L{l}")))
        .collect();
    // Latest level and SNR, for the charts' right edge.
    let lv = |p: &[(f64, f32)]| p.last().map(|x| format!("L{}", x.1 as usize));
    let db = |p: &[(f64, f32)]| p.last().map(|x| format!("{:+.0}", x.1));
    let me = app.cfg.call.clone();
    let them = app.link.as_ref().map_or(String::new(), |s| s.peer.clone());
    charts::chart(
        ui,
        "Level",
        &[(theme::US, me.as_str()), (theme::PEER, them.as_str())],
        150.0 * k,
        t,
        (0.0, top),
        &lticks,
        &[
            Series {
                color: theme::PEER,
                points: &app.hist.peer_levels,
                style: Style::Steps,
                last: lv(&app.hist.peer_levels),
            },
            Series {
                color: theme::US,
                points: &app.hist.us_levels,
                style: Style::Steps,
                last: lv(&app.hist.us_levels),
            },
        ],
        None,
    );
    ui.add_space(4.0);
    let sticks: Vec<(f32, String)> = (-10..=30).step_by(10).map(|v| (v as f32, format!("{v:+}"))).collect();
    charts::chart(
        ui,
        "SNR",
        &[(theme::PEER, "Rpt Sent"), (theme::US, "Rpt Rcvd")],
        130.0 * k,
        t,
        (-15.0, 30.0),
        &sticks,
        &[
            Series {
                color: theme::PEER,
                points: &app.hist.snr_here,
                style: Style::Dots,
                last: db(&app.hist.snr_here),
            },
            Series {
                color: theme::US,
                points: &app.hist.snr_there,
                style: Style::Dots,
                last: db(&app.hist.snr_there),
            },
        ],
        Some(0.0),
    );
    ui.add_space(4.0);
    let kb_top = app.hist.data.last().map_or(1.0, |p| p.1).max(1.0) * 1.15;
    let kticks = vec![(0.0, "0".to_string()), (kb_top / 1.15, format!("{:.1}", kb_top / 1.15))];
    charts::chart(
        ui,
        "kB",
        &[],
        110.0 * k,
        t,
        (0.0, kb_top),
        &kticks,
        &[Series {
            color: theme::OK,
            points: &app.hist.data,
            style: Style::Line,
            last: app.hist.data.last().map(|p| format!("{:.1}", p.1)),
        }],
        None,
    );
}

fn log(app: &App, ui: &mut egui::Ui, h: f32) {
    theme::card().fill(theme::INSET).show(ui, |ui| {
        ui.set_min_height(h - 26.0);
        ui.set_width(ui.available_width());
        let me = app.cfg.call.as_str();
        let them = app.link.as_ref().map_or("", |s| s.peer.as_str());
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("log_rows")
                    .num_columns(4)
                    .spacing([12.0, 3.0])
                    .show(ui, |ui| {
                        for r in &app.hist.rows {
                            ui.label(RichText::new(format!("{:.1}", r.t)).monospace().color(theme::MUTED));
                            let (who, c) = if r.tx { (me, theme::US) } else { (them, theme::PEER) };
                            ui.label(RichText::new(who).monospace().color(c));
                            let heard = if r.tx {
                                RichText::new(format!("{:.1} s", r.secs))
                                    .monospace()
                                    .color(theme::MUTED)
                            } else if r.lost > 0 {
                                RichText::new(format!("{} lost {}", theme::db(r.snr), r.lost))
                                    .monospace()
                                    .color(theme::BAD)
                            } else {
                                RichText::new(format!("{} dB", theme::db(r.snr)))
                                    .monospace()
                                    .color(theme::OK)
                            };
                            ui.label(heard);
                            ui.label(RichText::new(&r.what).monospace());
                            ui.end_row();
                        }
                    });
            });
    });
}

fn text(app: &mut App, ui: &mut egui::Ui, h: f32) {
    theme::card().fill(theme::INSET).show(ui, |ui| {
        ui.set_min_height(h - 26.0);
        ui.set_width(ui.available_width());
        let input_h = 72.0;
        // Pictures to the window once.
        for (k, x) in app.files.iter_mut().enumerate() {
            if x.image_new
                && let Some(img) = &x.image
            {
                x.texture = Some(
                    ui.ctx()
                        .load_texture(format!("xfer{k}"), img.clone(), egui::TextureOptions::LINEAR),
                );
                x.image_new = false;
            }
        }
        let me = app.cfg.call.clone();
        let them = app.link.as_ref().map_or(String::new(), |s| s.peer.clone());
        egui::ScrollArea::vertical()
            .id_salt("chat")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .max_height((h - 26.0 - input_h - 16.0).max(40.0))
            .show(ui, |ui| {
                if app.chat.is_empty() {
                    ui.label(RichText::new("—").color(theme::MUTED));
                }
                for c in &app.chat {
                    match c {
                        crate::app::Chat::Text { sent, text, ends_at } => {
                            let (who, c) = if *sent { (&me, theme::US) } else { (&them, theme::PEER) };
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(who.as_str()).monospace().strong().size(15.0).color(c));
                                ui.label(RichText::new(text.trim_end()).size(15.0).color(theme::TEXT));
                                if *sent {
                                    let start = ends_at - text.len() as u64;
                                    let (mark, color) = match &app.link {
                                        Some(s) if s.text_acked >= *ends_at => ("✓", theme::OK),
                                        Some(s) if s.text_started > start => ("sending", theme::WARN),
                                        _ => ("queued", theme::MUTED),
                                    };
                                    ui.label(RichText::new(mark).size(12.0).color(color));
                                }
                            });
                            ui.add_space(2.0);
                        }
                        crate::app::Chat::File(k) => {
                            if let Some(x) = app.files.get(*k) {
                                file_row(ui, x);
                            }
                        }
                    }
                }
            });
        ui.horizontal(|ui| {
            let linked = app.state() == ion::link::State::Connected;
            let edit = ui.add_enabled(
                linked,
                egui::TextEdit::singleline(&mut app.compose)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(ui.available_width() - 70.0),
            );
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(linked, egui::Button::new("Send")).clicked() || enter) && linked {
                app.send_text();
                edit.request_focus();
            }
        });
        ui.horizontal(|ui| {
            let linked = app.state() == ion::link::State::Connected;
            ui.add(
                egui::TextEdit::singleline(&mut app.file_path)
                    .hint_text("Drop a file")
                    .font(egui::TextStyle::Monospace)
                    .desired_width(ui.available_width() - 140.0),
            );
            if ui.button("Browse").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_file()
            {
                if linked {
                    app.send_file(&p);
                } else {
                    app.file_path = p.display().to_string();
                }
            }
            let path = std::path::PathBuf::from(app.file_path.trim().trim_matches('"'));
            let ok = linked && path.is_file();
            if ui.add_enabled(ok, egui::Button::new("File")).clicked() {
                app.send_file(&path);
                app.file_path.clear();
            }
        });
    });
}

/// A file in the chat: who, name, size and how far; a picture as it fills
/// in; where a received one was saved.
fn file_row(ui: &mut egui::Ui, x: &crate::app::Xfer) {
    let color = if x.sent { theme::US } else { theme::PEER };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(if x.sent { "→" } else { "←" }).monospace().color(color));
        ui.label(RichText::new(&x.name).monospace().strong().color(color));
        ui.label(
            RichText::new(theme::bytes(x.size as u64))
                .monospace()
                .color(theme::MUTED),
        );
    });
    let frac = if x.size == 0 {
        1.0
    } else {
        x.have as f32 / x.size as f32
    };
    let label = if x.done {
        if x.sent {
            "Sent".to_string()
        } else {
            "Received".to_string()
        }
    } else {
        format!("{:.0}%", frac * 100.0)
    };
    theme::bar(ui, frac, color, &label);
    if let Some(t) = &x.texture {
        let size = t.size_vec2();
        let w = size.x.min(ui.available_width()).min(320.0);
        ui.add(egui::Image::from_texture(t).fit_to_exact_size(egui::vec2(w, w * size.y / size.x.max(1.0))));
    }
    if let Some(p) = &x.saved {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(p.display().to_string())
                    .monospace()
                    .small()
                    .color(theme::MUTED),
            );
            if ui.small_button("Open").clicked() {
                let opener = if cfg!(windows) {
                    "explorer"
                } else if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                let _ = std::process::Command::new(opener).arg(p).spawn();
            }
        });
    }
    if let Some(e) = &x.error {
        ui.label(RichText::new(e).monospace().small().color(theme::BAD));
    }
    ui.add_space(4.0);
}
