//! Net: one screen a role. A member's: the check-in form, the net's page,
//! the messages. Net control's: the profile and its page before the net,
//! then the roster, the traffic and the bulletins. Above both, the net's
//! controls and a timeline that fills as the net goes. History: past nets.

use eframe::egui::{self, Color32, FontId, Margin, RichText, Stroke, pos2, vec2};
use ion::link::net::{MAX_LINE, Precedence, Traffic};

use crate::app::{App, NetLine};
use crate::ion_engine::{IonCmd, NetStatus, traffic_words};
use crate::markdown::{self, Block};
use crate::net_app::{Page, floor_estimate_s};
use crate::net_doc::{self, Announcement, Kind, MAX_FILE, Mode, PRECEDENCES, Purpose, State, To, precedence_word};
use crate::net_log::{self, NetLog, utc};
use crate::theme;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let role = app.net().map(|n| n.ncs);
    match (role, app.nets.page) {
        (Some(true), Page::Member) => app.nets.page = Page::Ncs,
        (Some(false), Page::Ncs) => app.nets.page = Page::Member,
        _ => {}
    }
    egui::Panel::top("net_pages")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .inner_margin(Margin::symmetric(14, 6)),
        )
        .show(ui, |ui| pages(app, ui));
    let page = app.nets.page;
    if page == Page::History {
        log_page(app, ui);
        return;
    }
    egui::Panel::top("net_bar")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .stroke(Stroke::new(1.0, theme::LINE))
                .inner_margin(Margin::symmetric(14, 8)),
        )
        .show(ui, |ui| {
            if page == Page::Ncs {
                ncs_bar(app, ui);
            } else {
                member_bar(app, ui);
            }
        });
    if app.net().is_some() && app.nets.log.is_some() {
        egui::Panel::top("net_timeline")
            .frame(
                egui::Frame::new()
                    .fill(theme::INSET)
                    .inner_margin(Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| timeline(app, ui));
    }
    match page {
        Page::Ncs => ncs(app, ui),
        _ => member(app, ui),
    }
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(theme::MUTED));
}

fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).strong());
}

/// A caption and its value, side by side in a bar.
fn kv(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.add_space(6.0);
    caption(ui, k);
    ui.label(RichText::new(v).monospace());
}

fn dot(ui: &mut egui::Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(r.center(), 4.0, c);
}

/// Drawn: the fonts have no check mark.
fn tick(ui: &mut egui::Ui, color: Color32) {
    let h = ui.text_style_height(&egui::TextStyle::Body);
    let (r, _) = ui.allocate_exact_size(vec2(h, h), egui::Sense::hover());
    let p = |x: f32, y: f32| r.left_top() + vec2(x, y) * h;
    let stroke = Stroke::new(2.0, color);
    ui.painter().line_segment([p(0.15, 0.55), p(0.4, 0.8)], stroke);
    ui.painter().line_segment([p(0.4, 0.8), p(0.85, 0.25)], stroke);
}

fn abort(app: &mut App, ui: &mut egui::Ui) {
    let b = egui::Button::new(RichText::new("Abort").color(theme::BAD)).min_size(vec2(72.0, 26.0));
    if ui.add(b).on_hover_text("Stop now; keys nothing (Esc)").clicked() {
        app.abort();
    }
}

fn db(v: Option<f32>) -> String {
    v.map_or("—".into(), |v| format!("{v:+.0}"))
}

fn dash(s: &str) -> String {
    if s.is_empty() { "—".into() } else { s.to_string() }
}

/// "01:02" or "01:02:03" of a UTC time.
fn clock_of(t: i64, secs: bool) -> String {
    utc(t).get(11..if secs { 19 } else { 16 }).unwrap_or("").to_string()
}

/// "01:00" of UTC minutes.
fn hm(m: u32) -> String {
    clock_of(m as i64 * 60, false)
}

/// A net is up and not closed.
fn active(app: &App) -> bool {
    app.net().is_some_and(|n| !n.closed)
}

fn prec_color(t: Traffic) -> Color32 {
    match t.precedence {
        _ if t.items == 0 => theme::MUTED,
        p => prec_tone(p),
    }
}

fn prec_tone(p: Precedence) -> Color32 {
    match p {
        Precedence::Emergency => theme::BAD,
        Precedence::Priority => theme::WARN,
        Precedence::Welfare => theme::PEER,
        Precedence::Routine => theme::TEXT,
    }
}

fn open_folder(app: &mut App, dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(dir).spawn() {
        app.note(format!("! {program} {}: {e}", dir.display()));
    }
}

fn pages(app: &mut App, ui: &mut egui::Ui) {
    let role = app.net().map(|n| n.ncs);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mut tab = |ui: &mut egui::Ui, page: Page, name: &str, on: bool| {
            let b = ui.add_enabled(on, egui::Button::selectable(app.nets.page == page, name));
            if b.clicked() {
                app.nets.page = page;
                if page == Page::History {
                    app.nets.refresh_past();
                }
            }
        };
        tab(ui, Page::Member, "Member", role != Some(true));
        tab(ui, Page::Ncs, "NCS", role != Some(false));
        ui.add_space(14.0);
        tab(ui, Page::History, "History", true);
    });
}

// -- the bars -----------------------------------------------------------------------

fn member_bar(app: &mut App, ui: &mut egui::Ui) {
    let can = !app.cfg.call.trim().is_empty() && !app.linked();
    ui.horizontal(|ui| match app.net().cloned() {
        None => {
            caption(ui, "Net");
            let edit = ui.add(
                egui::TextEdit::singleline(&mut app.cfg.net)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(140.0)
                    .hint_text("any"),
            );
            if edit.changed() {
                app.cfg.net = app.cfg.net.to_ascii_uppercase();
                app.mark_dirty();
            }
            let join = egui::Button::new(RichText::new("Join").strong()).min_size(vec2(84.0, 26.0));
            if ui
                .add_enabled(can, join)
                .on_hover_text("Listen for the net and check in; transmits")
                .clicked()
            {
                app.net_join();
            }
        }
        Some(n) => {
            let (c, word) = match (n.closed, n.slot) {
                (true, _) => (theme::MUTED, "Closed"),
                (false, Some(_)) => (theme::OK, "In"),
                (false, None) => (theme::WARN, "Joining"),
            };
            dot(ui, c);
            ui.label(RichText::new(word).strong().color(c));
            kv(ui, "Net", &dash(&n.net));
            kv(ui, "NCS", n.ncs_call.as_deref().unwrap_or("—"));
            kv(ui, "Slot", &n.slot.map_or("—".into(), |s| s.to_string()));
            kv(ui, "Via", n.relay.as_deref().unwrap_or("—"));
            kv(ui, "Stations", &n.members.len().to_string());
            kv(ui, "Traffic", &traffic_words(n.traffic));
            if !n.leaves.is_empty() {
                kv(ui, "Leaves", &n.leaves.join(" "));
            }
            if n.pending > 0 {
                kv(ui, "Queue", &theme::bytes(n.pending as u64));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                abort(app, ui);
                if ui
                    .add_enabled(!n.closed, egui::Button::new("Leave").min_size(vec2(72.0, 26.0)))
                    .clicked()
                {
                    app.cmd(IonCmd::NetClose);
                }
            });
        }
    });
}

fn ncs_bar(app: &mut App, ui: &mut egui::Ui) {
    let can = !app.cfg.call.trim().is_empty() && !app.linked();
    ui.horizontal(|ui| match app.net().cloned() {
        None => {
            caption(ui, "Net");
            let edit = ui.add(
                egui::TextEdit::singleline(&mut app.cfg.net_profile.name)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(140.0)
                    .hint_text("net"),
            );
            if edit.changed() {
                app.cfg.net_profile.name = app.cfg.net_profile.name.to_ascii_uppercase();
                app.mark_dirty();
            }
            let start = egui::Button::new(RichText::new("Start").strong()).min_size(vec2(84.0, 26.0));
            let start = ui
                .add_enabled(can && !app.cfg.net_profile.name.trim().is_empty(), start)
                .on_hover_text("Open the net as NCS; transmits");
            if start.clicked() {
                app.net_open();
            }
        }
        Some(n) => {
            let closing = app.nets.closing.map(|c| c.until - app.now());
            let (c, word) = match (n.closed, closing) {
                (true, _) => (theme::MUTED, "Closed".to_string()),
                (false, Some(left)) => (theme::WARN, format!("Closing {}", theme::clock(left))),
                (false, None) if n.open => (theme::OK, "Open".to_string()),
                (false, None) => (theme::NET, "Running".to_string()),
            };
            dot(ui, c);
            ui.label(RichText::new(word).strong().color(c));
            kv(ui, "Net", &dash(&n.net));
            kv(ui, "Round", &n.rounds.to_string());
            kv(ui, "Stations", &n.roster.len().to_string());
            if let Some(b) = n.beacon_in {
                kv(ui, "Beacon", &theme::clock(b));
            }
            ui.add_space(10.0);
            let mut open = n.open;
            let busy = n.closed || closing.is_some();
            if ui
                .add_enabled(!busy, egui::Checkbox::new(&mut open, "Check-ins"))
                .changed()
            {
                app.cmd(IonCmd::NetCheckins(open));
            }
            let beacon = ui
                .add_enabled(n.open && !busy, egui::Button::new("Beacon"))
                .on_hover_text("Send one now; transmits");
            if beacon.clicked() {
                app.cmd(IonCmd::NetBeacon);
            }
            if ui.selectable_label(app.nets.editing, "Page").clicked() {
                app.nets.editing = !app.nets.editing;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                abort(app, ui);
                if closing.is_some() {
                    if ui
                        .add_enabled(!n.closed, egui::Button::new("Now").min_size(vec2(72.0, 26.0)))
                        .on_hover_text("Close without waiting")
                        .clicked()
                    {
                        let cmds = app.nets.close_now();
                        app.cmds(cmds);
                    }
                } else {
                    let close = ui
                        .add_enabled(!n.closed, egui::Button::new("Close").min_size(vec2(72.0, 26.0)))
                        .on_hover_text("Summary, then close");
                    if close.clicked() {
                        app.net_close();
                    }
                }
            });
        }
    });
}

// -- the timeline -------------------------------------------------------------------

/// The net from its start to its end, filled to now, with a tick where
/// each station checked in and where priority or emergency traffic came.
fn timeline(app: &App, ui: &mut egui::Ui) {
    let (Some(log), Some(n)) = (app.nets.log.as_ref(), app.net()) else {
        return;
    };
    let r = &log.rec;
    let now = net_log::now();
    let shown_to = r.closed.unwrap_or(now);
    let (mut t0, mut t1) = (r.opened, r.opened + 3600);
    if let Some(a) = &app.nets.announcement
        && let (Some(s), Some(e)) = (a.start, a.end)
    {
        let (s, e) = (s as i64 * 60, e as i64 * 60);
        // This day's net, not one a day off.
        if e > s && r.opened >= s - 3600 && r.opened < e {
            (t0, t1) = (s.min(r.opened), e);
        }
    }
    t1 = t1.max(shown_to);
    let me = app.cfg.call.as_str();
    let mut marks: Vec<(i64, String, Color32, bool)> = r
        .checkins
        .iter()
        .map(|c| {
            let col = if c.call == me { theme::US } else { theme::TEXT };
            (c.at, c.call.clone(), col, false)
        })
        .chain(
            r.traffic
                .iter()
                .filter(|i| i.precedence >= Precedence::Priority && !i.from.is_empty())
                .map(|i| {
                    let w = precedence_word(i.precedence);
                    (i.at, format!("{} {}", &w[..1], i.from), prec_tone(i.precedence), true)
                }),
        )
        .collect();
    marks.sort_by_key(|m| m.0);

    let font = FontId::monospace(11.0);
    let side_w = 130.0;
    let edge_w = 50.0;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 56.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let rail = egui::Rect::from_min_max(
        pos2(rect.left() + edge_w, rect.top() + 4.0),
        pos2(rect.right() - edge_w - side_w, rect.top() + 10.0),
    );
    let frac = |t: i64| ((t - t0) as f32 / (t1 - t0).max(1) as f32).clamp(0.0, 1.0);
    let x_of = |t: i64| rail.left() + rail.width() * frac(t);
    p.text(
        pos2(rect.left(), rail.center().y),
        egui::Align2::LEFT_CENTER,
        clock_of(t0, false) + "Z",
        font.clone(),
        theme::MUTED,
    );
    p.text(
        pos2(rail.right() + 8.0, rail.center().y),
        egui::Align2::LEFT_CENTER,
        clock_of(t1, false) + "Z",
        font.clone(),
        theme::MUTED,
    );
    p.rect_filled(rail, 3.0, theme::LINE);
    let filled = egui::Rect::from_min_max(rail.min, pos2(x_of(shown_to), rail.bottom()));
    p.rect_filled(filled, 3.0, theme::NET);
    // Check-ins bunch up: each label takes the first of three rows it
    // fits on, else moves right of the row that ends first.
    let mut ends = [f32::MIN; 3];
    for (t, text, col, traffic) in &marks {
        let x = x_of(*t);
        let g = p.layout_no_wrap(text.clone(), font.clone(), *col);
        let row = (0..3)
            .find(|&r| ends[r] + 6.0 <= x + 3.0)
            .unwrap_or_else(|| (0..3).min_by(|&a, &b| ends[a].total_cmp(&ends[b])).expect("three rows"));
        let lx = (x + 3.0).max(ends[row] + 6.0);
        let y = rail.bottom() + 4.0 + row as f32 * 14.0;
        let stroke = Stroke::new(1.0, if *traffic { *col } else { theme::MUTED });
        p.line_segment([pos2(x, rail.bottom()), pos2(x, y + 11.0)], stroke);
        if lx > x + 3.0 {
            p.line_segment([pos2(x, y + 11.0), pos2(lx - 1.0, y + 11.0)], stroke);
        }
        ends[row] = lx + g.size().x;
        p.galley(pos2(lx, y), g, *col);
    }
    let sx = rect.right() - side_w + 14.0;
    if n.ncs {
        p.text(
            pos2(sx, rail.center().y),
            egui::Align2::LEFT_CENTER,
            format!("Round {}", n.rounds),
            font.clone(),
            theme::TEXT,
        );
    }
    if let Some(b) = n.beacon_in {
        p.text(
            pos2(sx, rail.center().y + 16.0),
            egui::Align2::LEFT_CENTER,
            format!("Beacon {}", theme::clock(b)),
            font,
            theme::TEXT,
        );
    }
    if let Some(pos) = resp.hover_pos() {
        let near = marks
            .iter()
            .filter(|m| (x_of(m.0) - pos.x).abs() < 8.0)
            .map(|m| format!("{} {}", clock_of(m.0, true), m.1))
            .collect::<Vec<_>>();
        if !near.is_empty() {
            resp.on_hover_text_at_pointer(near.join("\n"));
        } else if rail.x_range().contains(pos.x) {
            let t = t0 + ((pos.x - rail.left()) / rail.width() * (t1 - t0) as f32) as i64;
            resp.on_hover_text_at_pointer(clock_of(t, false) + "Z");
        }
    }
}

// -- the net's page -----------------------------------------------------------------

/// The page as someone joining sees it: its name, the essentials large,
/// then the body. A first-level heading at the top names the net.
fn net_page(ui: &mut egui::Ui, a: &Announcement, md: &mut markdown::Cached) {
    let blocks = md.get(&a.body);
    let (name, body) = match blocks.first() {
        Some(Block::Heading(1, spans)) => (spans.iter().map(|s| s.text.as_str()).collect(), &blocks[1..]),
        _ => (dash(&a.net), blocks),
    };
    ui.label(RichText::new(name).size(28.0).strong().color(theme::NET));
    let sub: Vec<&str> = [a.purpose.map(Purpose::name), a.mode.map(Mode::name)]
        .into_iter()
        .flatten()
        .collect();
    if !sub.is_empty() {
        ui.label(RichText::new(sub.join(" · ")).size(15.0).color(theme::MUTED));
    }
    ui.add_space(10.0);
    let time = match (a.start, a.end) {
        (Some(s), Some(e)) => format!("{}–{}Z", hm(s), hm(e)),
        (Some(s), None) => format!("{}Z", hm(s)),
        _ => String::new(),
    };
    let fields = [
        ("Frequency", a.freq_hz.map(net_doc::freq_text).unwrap_or_default()),
        ("Backup", a.backup_hz.map(net_doc::freq_text).unwrap_or_default()),
        ("Time", time),
        ("NCS", a.ncs.clone()),
        ("Alternate", a.alt.clone()),
        ("Next", a.next.map(net_doc::minutes_text).unwrap_or_default()),
    ];
    // Laid out by hand: wrapped rows of vertical groups squeeze the last
    // one to a letter wide.
    let value_font = FontId::monospace(19.0);
    let cells: Vec<(&str, String, f32)> = fields
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| {
            let w = ui.fonts_mut(|f| {
                let vw = f.layout_no_wrap(v.clone(), value_font.clone(), theme::TEXT).size().x;
                let kw = f
                    .layout_no_wrap(k.to_string(), FontId::proportional(11.0), theme::MUTED)
                    .size()
                    .x;
                vw.max(kw)
            });
            (k, v, w)
        })
        .collect();
    let gap = 30.0;
    let room = ui.available_width();
    let mut rows: Vec<Vec<(&str, String, f32)>> = vec![Vec::new()];
    let mut used = 0.0;
    for c in cells {
        let row = rows.last_mut().expect("a row");
        if !row.is_empty() && used + gap + c.2 > room {
            used = 0.0;
            rows.push(Vec::new());
        }
        let row = rows.last_mut().expect("a row");
        used += if row.is_empty() { c.2 } else { gap + c.2 };
        row.push(c);
    }
    for row in rows {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (k, v, w) in row {
                ui.allocate_ui_with_layout(vec2(w, 44.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    caption(ui, k);
                    ui.label(RichText::new(v).font(value_font.clone()));
                });
            }
        });
    }
    ui.add_space(14.0);
    markdown::show(ui, body);
}

// -- member -------------------------------------------------------------------------

fn member(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("net_checkin")
        .resizable(false)
        .exact_size(340.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("net_checkin_scroll")
                .show(ui, |ui| checkin(app, ui))
        });
    egui::Panel::right("net_messages")
        .resizable(true)
        .default_size(300.0)
        .min_size(240.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| messages(app, ui));
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(theme::INSET)
                .inner_margin(Margin::symmetric(30, 16)),
        )
        .show(ui, |ui| {
            let ann = app.nets.announcement.clone().filter(|_| app.nets.log.is_some());
            egui::ScrollArea::vertical()
                .id_salt("net_page")
                .auto_shrink([false, false])
                .show(ui, |ui| match &ann {
                    Some(a) => net_page(ui, a, &mut app.nets.md_inbox),
                    None => {
                        ui.label(RichText::new("No page").color(theme::MUTED));
                    }
                });
        });
}

fn checkin(app: &mut App, ui: &mut egui::Ui) {
    let in_net = app.net().is_some();
    let joined = app.nets.is_joined();
    title(ui, "Check-in");
    ui.add_space(6.0);
    let mut line_done = false;
    let mut line_changed = false;
    egui::Grid::new("checkin_form")
        .num_columns(2)
        .spacing([10.0, 8.0])
        .show(ui, |ui| {
            caption(ui, "Call");
            ui.label(RichText::new(dash(&app.cfg.call)).monospace().strong());
            ui.end_row();
            caption(ui, "Line");
            ui.vertical(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut app.cfg.net_line)
                        .desired_width(f32::INFINITY)
                        .hint_text("Trousdale County ARES"),
                );
                if r.changed() {
                    app.cfg.net_line = net_doc::clip(&app.cfg.net_line, MAX_LINE).to_string();
                    line_changed = true;
                }
                line_done = r.lost_focus();
            });
            ui.end_row();
        });
    ui.horizontal(|ui| {
        ui.checkbox(&mut app.nets.form.short, "Short");
        let mut c = app.net().map_or(app.net_caps, |n| n.caps);
        let mut changed = false;
        ui.add_enabled_ui(!in_net, |ui| {
            changed |= ui.checkbox(&mut c.relay, "Relay").changed();
            changed |= ui.checkbox(&mut c.attended, "Attended").changed();
        });
        if changed {
            app.net_caps = c;
            app.cmd(IonCmd::NetCaps(c));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            caption(ui, &format!("{}/{MAX_LINE}", app.cfg.net_line.len()));
        });
    });
    if line_changed {
        app.mark_dirty();
    }
    if line_done && active(app) {
        let line = app.cfg.net_line.trim().to_string();
        app.cmd(IonCmd::NetLine(line));
    }

    ui.add_space(12.0);
    title(ui, "Traffic");
    ui.add_space(4.0);
    let f = &mut app.nets.form;
    ui.add(
        egui::TextEdit::multiline(&mut f.text)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("None"),
    );
    ui.horizontal(|ui| {
        caption(ui, "File");
        ui.add(
            egui::TextEdit::singleline(&mut f.file)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .hint_text("path"),
        );
    });
    let len = Some(f.file.trim())
        .filter(|p| !p.is_empty())
        .and_then(|p| std::fs::metadata(p).ok())
        .filter(|m| m.is_file())
        .map(|m| m.len());
    if let Some(n) = len {
        let c = if n as usize > MAX_FILE {
            theme::BAD
        } else {
            theme::MUTED
        };
        ui.label(
            RichText::new(format!(
                "{} / {} · ≥ {}",
                theme::bytes(n),
                theme::bytes(MAX_FILE as u64),
                theme::clock(floor_estimate_s(n as usize))
            ))
            .monospace()
            .size(11.0)
            .color(c),
        )
        .on_hover_text("Uplink time at the floor, one frame a burst");
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for p in PRECEDENCES {
            ui.selectable_value(
                &mut f.precedence,
                p,
                RichText::new(precedence_word(p)).color(prec_tone(p)),
            );
        }
    });
    let ready = !f.text.trim().is_empty() || !f.file.trim().is_empty();
    let mut send = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.selectable_value(&mut f.to, To::Ncs, "NCS");
        ui.selectable_value(&mut f.to, To::Net, "Net")
            .on_hover_text("For the net, through net control");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if joined { "Send" } else { "Queue" };
            let b = egui::Button::new(RichText::new(label).strong()).min_size(vec2(84.0, 26.0));
            send = ui.add_enabled(ready, b).clicked();
        });
    });
    if send {
        app.net_send_form();
    }

    ui.add_space(12.0);
    title(ui, "Outbox");
    ui.add_space(4.0);
    let me = app.cfg.call.clone();
    let log = app.nets.log.as_ref().filter(|l| l.rec.closed.is_none());
    let ours: Vec<net_log::Item> = log
        .map(|l| l.rec.traffic.iter().filter(|i| i.from == me).cloned().collect())
        .unwrap_or_default();
    let files: Vec<net_log::Item> = app
        .nets
        .log
        .as_ref()
        .map(|l| {
            let mine = l.rec.traffic.iter().filter(|i| i.kind == Kind::File && i.from != me);
            mine.cloned().collect()
        })
        .unwrap_or_default();
    let queued = app.nets.queued();
    if ours.is_empty() && queued == 0 {
        ui.label(RichText::new("None").color(theme::MUTED));
    }
    if queued > 0 {
        ui.label(RichText::new(format!("{queued} queued")).color(theme::WARN));
    }
    egui::Grid::new("outbox_rows")
        .num_columns(3)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for it in ours {
                let p = it.precedence;
                ui.label(RichText::new(&precedence_word(p)[..1]).monospace().color(prec_tone(p)));
                ui.label(&it.subject);
                let c = if it.state == State::Queued {
                    theme::WARN
                } else {
                    theme::OK
                };
                ui.label(RichText::new(it.state.word()).color(c));
                ui.end_row();
            }
        });
    if !files.is_empty() {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            title(ui, "Files");
            if ui.small_button("Folder").clicked()
                && let Some(d) = app.nets.log.as_ref().map(NetLog::files_dir)
            {
                open_folder(app, &d);
            }
        });
        egui::Grid::new("inbox_files")
            .num_columns(3)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                for f in &files {
                    ui.label(RichText::new(&f.from).monospace().strong());
                    ui.label(&f.name);
                    ui.label(
                        RichText::new(theme::bytes(f.size as u64))
                            .monospace()
                            .color(theme::MUTED),
                    );
                    ui.end_row();
                }
            });
    }
}

// -- net control --------------------------------------------------------------------

fn ncs(app: &mut App, ui: &mut egui::Ui) {
    let running = app.net().is_some_and(|n| n.ncs);
    if !running || app.nets.editing {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::INSET).inner_margin(Margin::same(14)))
            .show(ui, |ui| profile(app, ui));
        return;
    }
    egui::Panel::right("net_ncs_side")
        .resizable(true)
        .default_size(380.0)
        .min_size(300.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            ncs_traffic(app, ui);
            ui.add_space(8.0);
            ui.separator();
            messages(app, ui);
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::INSET).inner_margin(Margin::same(14)))
        .show(ui, |ui| roster(app, ui));
}

fn profile(app: &mut App, ui: &mut egui::Ui) {
    let ncs_up = app.net().is_some_and(|n| n.ncs && !n.closed);
    let offered = app.nets.offered().filter(|_| ncs_up);
    let mut changed = false;
    let mut committed = false;
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        ui.horizontal(|ui| {
            title(ui, "Profile");
            if let Some(id) = offered {
                let s = id.to_string();
                ui.label(
                    RichText::new(format!("Offered {}", &s[..8]))
                        .monospace()
                        .color(theme::MUTED),
                );
            }
        });
        ui.add_space(6.0);
        let p = &mut app.cfg.net_profile;
        egui::Grid::new("profile_fields")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let fields = [
                    ("Frequency", &mut p.freq, "3.975", false),
                    ("Backup", &mut p.backup, "7.262", false),
                    ("Start", &mut p.start, "01:00", false),
                    ("End", &mut p.end, "02:00", false),
                    ("Next", &mut p.next, "2026-11-10 01:00", false),
                    ("Alternate", &mut p.alt, "call", true),
                    ("Regulars", &mut p.regulars, "calls", true),
                ];
                for (k, v, hint, upper) in fields {
                    caption(ui, k);
                    let r = ui.add(
                        egui::TextEdit::singleline(v)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .hint_text(hint),
                    );
                    if r.changed() {
                        if upper {
                            *v = v.to_ascii_uppercase();
                        }
                        changed = true;
                    }
                    committed |= r.lost_focus();
                    ui.end_row();
                }
                caption(ui, "Mode");
                egui::ComboBox::from_id_salt("profile_mode")
                    .selected_text(p.mode.name())
                    .show_ui(ui, |ui| {
                        for m in Mode::ALL {
                            if ui.selectable_value(&mut p.mode, m, m.name()).changed() {
                                changed = true;
                                committed = true;
                            }
                        }
                    });
                ui.end_row();
                caption(ui, "Purpose");
                egui::ComboBox::from_id_salt("profile_purpose")
                    .selected_text(p.purpose.name())
                    .show_ui(ui, |ui| {
                        for x in Purpose::ALL {
                            if ui.selectable_value(&mut p.purpose, x, x.name()).changed() {
                                changed = true;
                                committed = true;
                            }
                        }
                    });
                ui.end_row();
            });
        ui.add_space(8.0);
        caption(ui, "Page");
        let body = egui::ScrollArea::vertical()
            .id_salt("profile_body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_sized(
                    ui.available_size(),
                    egui::TextEdit::multiline(&mut p.body)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Markdown; ::: steps, ::: agenda, ::: note"),
                )
            })
            .inner;
        changed |= body.changed();
        committed |= body.lost_focus();

        let ui = &mut cols[1];
        title(ui, "Preview");
        ui.add_space(6.0);
        let a = app.cfg.net_profile.announcement(&app.cfg.call, net_log::now());
        egui::Frame::new()
            .fill(theme::BG)
            .corner_radius(4)
            .inner_margin(Margin::symmetric(22, 14))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("profile_preview")
                    .auto_shrink([false, false])
                    .show(ui, |ui| net_page(ui, &a, &mut app.nets.md_preview));
            });
    });
    if changed {
        app.mark_dirty();
        app.nets.edited = true;
    }
    if committed && app.nets.edited && ncs_up {
        app.nets.edited = false;
        app.nets.reoffer = true;
    }
}

fn roster(app: &mut App, ui: &mut egui::Ui) {
    let Some(n) = app.net().cloned().filter(|n| n.ncs) else {
        return;
    };
    title(ui, "Roster");
    ui.add_space(6.0);
    if n.roster.is_empty() {
        ui.label(RichText::new("None").color(theme::MUTED));
        return;
    }
    let page = app
        .nets
        .offered()
        .and_then(|id| n.offers.iter().position(|o| *o == Some(id)));
    let mut to: Option<String> = None;
    egui::ScrollArea::both().id_salt("roster").show(ui, |ui| {
        egui::Grid::new("roster_rows")
            .num_columns(11)
            .spacing([12.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for h in [
                    "Slot", "Call", "Line", "Traffic", "Up", "Down", "Route", "Level", "Missed", "Page", "",
                ] {
                    caption(ui, h);
                }
                ui.end_row();
                for r in &n.roster {
                    ui.label(RichText::new(r.slot.to_string()).monospace().color(theme::MUTED));
                    ui.label(RichText::new(&r.call).monospace().strong());
                    ui.label(&r.line);
                    ui.label(
                        RichText::new(traffic_words(r.traffic))
                            .monospace()
                            .color(prec_color(r.traffic)),
                    );
                    ui.label(RichText::new(db(r.snr_up)).monospace());
                    ui.label(RichText::new(db(r.snr_down)).monospace());
                    let route = r.via.as_ref().map_or("direct".to_string(), |v| format!("via {v}"));
                    ui.label(RichText::new(route).monospace().color(theme::MUTED));
                    ui.label(RichText::new(format!("L{}", r.cap)).monospace().color(theme::MUTED));
                    let mc = if r.missed > 0 { theme::BAD } else { theme::MUTED };
                    ui.label(RichText::new(r.missed.to_string()).monospace().color(mc));
                    if page.is_some_and(|i| r.holds & 1 << i != 0) {
                        tick(ui, theme::OK);
                    } else {
                        ui.label(RichText::new("—").color(theme::MUTED));
                    }
                    if ui.small_button("To").clicked() {
                        to = Some(r.call.clone());
                    }
                    ui.end_row();
                }
            });
    });
    if let Some(c) = to {
        app.net_to = c;
    }
}

/// What came in for net control, open items first by precedence.
fn ncs_traffic(app: &mut App, ui: &mut egui::Ui) {
    let me = app.cfg.call.clone();
    title(ui, "Traffic");
    ui.add_space(4.0);
    let Some(log) = app.nets.log.as_ref() else {
        return;
    };
    let mut rows: Vec<(usize, net_log::Item)> = log
        .rec
        .traffic
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, i)| i.from != me)
        .collect();
    if rows.is_empty() {
        ui.label(RichText::new("None").color(theme::MUTED));
        return;
    }
    rows.sort_by_key(|(_, i)| (i.state == State::Handled, std::cmp::Reverse(i.precedence), i.at));
    let files = log.files_dir();
    let picked = app.nets.picked.and_then(|i| log.rec.traffic.get(i)).cloned();
    let can_pass = active(app);
    let mut pick: Option<usize> = None;
    let mut handled: Option<(usize, bool)> = None;
    let mut pass: Option<usize> = None;
    let mut folder = false;
    egui::ScrollArea::vertical()
        .id_salt("ncs_traffic")
        .max_height((ui.available_height() * 0.4).max(120.0))
        .show(ui, |ui| {
            egui::Grid::new("ncs_traffic_rows")
                .num_columns(4)
                .spacing([10.0, 5.0])
                .show(ui, |ui| {
                    for (i, it) in &rows {
                        let p = it.precedence;
                        ui.label(RichText::new(&precedence_word(p)[..1]).monospace().color(prec_tone(p)));
                        ui.label(RichText::new(&it.from).monospace().strong());
                        let subj = match it.kind {
                            Kind::Message => it.subject.clone(),
                            Kind::File => format!("{} ({})", it.name, theme::bytes(it.size as u64)),
                        };
                        let subj = if it.for_net { format!("{subj} › Net") } else { subj };
                        if ui.selectable_label(app.nets.picked == Some(*i), subj).clicked() {
                            pick = Some(*i);
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let done = it.state == State::Handled;
                            if it.for_net
                                && !done
                                && ui.add_enabled(can_pass, egui::Button::new("Pass").small()).clicked()
                            {
                                pass = Some(*i);
                            }
                            if it.kind == Kind::File && ui.small_button("Folder").clicked() {
                                folder = true;
                            }
                            let word = RichText::new("Handled").color(if done { theme::OK } else { theme::TEXT });
                            if ui.selectable_label(done, word).clicked() {
                                handled = Some((*i, !done));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    if let Some(it) = picked {
        ui.add_space(6.0);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!("{} › {}  {}", it.from, dash(&it.to), utc(it.at)))
                    .monospace()
                    .color(theme::MUTED),
            );
            match it.kind {
                Kind::Message => {
                    ui.add(egui::Label::new(&it.text).wrap().selectable(true));
                }
                Kind::File => {
                    ui.label(RichText::new(dash(&it.saved)).monospace());
                }
            }
        });
    }
    if pick.is_some() {
        app.nets.picked = pick;
    }
    if let Some((i, h)) = handled {
        app.nets.set_handled(i, h);
    }
    if let Some(i) = pass {
        let cmds = app.nets.pass(&me, i);
        app.cmds(cmds);
    }
    if folder {
        open_folder(app, &files);
    }
}

// -- messages -----------------------------------------------------------------------

fn line(ui: &mut egui::Ui, l: &NetLine) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(&l.time).monospace().color(theme::MUTED));
        if !l.from.is_empty() {
            ui.label(RichText::new(&l.from).monospace().strong().color(theme::PEER));
        }
        if !l.to.is_empty() {
            ui.label(RichText::new(format!("› {}", l.to)).monospace().color(theme::MUTED));
        }
        let c = if l.note { theme::MUTED } else { theme::TEXT };
        ui.label(RichText::new(&l.text).color(c));
    });
}

fn messages(app: &mut App, ui: &mut egui::Ui) {
    let ncs = app.net().is_some_and(|n: &NetStatus| n.ncs);
    title(ui, if ncs { "Bulletins" } else { "Messages" });
    ui.add_space(6.0);
    // A member may write before joining: it is queued.
    let can = if ncs {
        active(app)
    } else {
        !app.net().is_some_and(|n| n.closed)
    };
    let input_h = if ncs { 92.0 } else { 40.0 };
    egui::ScrollArea::vertical()
        .id_salt("net_log")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .max_height((ui.available_height() - input_h).max(60.0))
        .show(ui, |ui| {
            for l in &app.net_log {
                line(ui, l);
            }
        });
    ui.add_space(6.0);
    if ncs {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            caption(ui, "To");
            ui.add(
                egui::TextEdit::singleline(&mut app.net_to)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(90.0)
                    .hint_text("all"),
            );
            for p in PRECEDENCES {
                let word = net_doc::precedence_word(p);
                let letter = RichText::new(&word[..1]).color(prec_tone(p));
                ui.selectable_value(&mut app.nets.form.precedence, p, letter)
                    .on_hover_text(word);
            }
        });
        ui.horizontal(|ui| {
            caption(ui, "File");
            ui.add(
                egui::TextEdit::singleline(&mut app.nets.form.file)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .hint_text("path"),
            );
        });
    }
    ui.horizontal(|ui| {
        let edit = ui.add_enabled(
            can,
            egui::TextEdit::singleline(&mut app.net_text).desired_width(ui.available_width() - 64.0),
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.add_enabled(can, egui::Button::new("Send")).clicked() || enter) && can {
            app.net_send();
        }
    });
}

// -- history ------------------------------------------------------------------------

fn log_page(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("net_past")
        .resizable(true)
        .default_size(300.0)
        .min_size(220.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                title(ui, "Nets");
                if ui.small_button("Refresh").clicked() {
                    app.nets.refresh_past();
                }
            });
            ui.add_space(6.0);
            egui::ScrollArea::vertical().id_salt("net_past_list").show(ui, |ui| {
                if app.nets.past.is_empty() {
                    ui.label(RichText::new("None").color(theme::MUTED));
                }
                let mut pick = None;
                for (i, l) in app.nets.past.iter().enumerate() {
                    let r = &l.rec;
                    let role = match r.role {
                        net_log::Role::Ncs => "NCS",
                        net_log::Role::Member => "Member",
                    };
                    let text = format!(
                        "{}  {}\n{role} · {} in",
                        utc(r.opened).get(..16).unwrap_or(""),
                        dash(&r.net),
                        r.checkins.len()
                    );
                    let b = egui::Button::selectable(app.nets.shown == Some(i), RichText::new(text).monospace());
                    if ui.add_sized([ui.available_width(), 36.0], b).clicked() {
                        pick = Some(i);
                    }
                }
                if pick.is_some() {
                    app.nets.shown = pick;
                }
            });
        });
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::INSET).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            let Some(i) = app.nets.shown.filter(|&i| i < app.nets.past.len()) else {
                ui.label(RichText::new("No net").color(theme::MUTED));
                return;
            };
            let mut export = false;
            let mut folder = false;
            ui.horizontal(|ui| {
                title(ui, "Net");
                export = ui.button("Export").clicked();
                folder = ui.button("Folder").clicked();
            });
            ui.add_space(6.0);
            let log = &app.nets.past[i];
            let r = log.rec.clone();
            let dir = log.path.parent().map(std::path::Path::to_path_buf);
            egui::ScrollArea::vertical()
                .id_salt("net_past_view")
                .auto_shrink([false, false])
                .show(ui, |ui| past(ui, &r, &mut app.nets.md_log));
            if export {
                match app.nets.past[i].export() {
                    Ok(files) => {
                        let names: Vec<String> = files
                            .iter()
                            .filter_map(|f| f.file_name()?.to_str().map(String::from))
                            .collect();
                        app.note(format!("Exported {}", names.join(", ")));
                    }
                    Err(e) => app.note(format!("! export: {e}")),
                }
            }
            if folder && let Some(d) = dir {
                open_folder(app, &d);
            }
        });
}

/// One past net.
fn past(ui: &mut egui::Ui, r: &net_log::NetRecord, md: &mut markdown::Cached) {
    egui::Grid::new("past_head")
        .num_columns(2)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            let rows = [
                ("Net", dash(&r.net)),
                ("NCS", dash(&r.ncs)),
                ("Operator", dash(&r.operator)),
                (
                    "Frequency",
                    r.freq_hz.map(net_doc::freq_text).unwrap_or_else(|| "—".into()),
                ),
                ("Opened", utc(r.opened)),
                ("Closed", r.closed.map(utc).unwrap_or_else(|| "—".into())),
                ("Summary", if r.summary { "Yes".into() } else { "—".into() }),
            ];
            for (k, v) in rows {
                caption(ui, k);
                ui.label(RichText::new(v).monospace());
                ui.end_row();
            }
        });
    ui.add_space(10.0);
    title(ui, &format!("Check-ins {}", r.checkins.len()));
    egui::Grid::new("past_in")
        .num_columns(5)
        .spacing([14.0, 3.0])
        .striped(true)
        .show(ui, |ui| {
            for c in &r.checkins {
                ui.label(RichText::new(clock_of(c.at, true)).monospace().color(theme::MUTED));
                ui.label(RichText::new(&c.call).monospace().strong());
                let t = Traffic::from_byte(c.traffic);
                ui.label(RichText::new(traffic_words(t)).monospace().color(prec_color(t)));
                let route = if c.via.is_empty() {
                    "direct".to_string()
                } else {
                    format!("via {}", c.via)
                };
                ui.label(RichText::new(route).color(theme::MUTED));
                ui.label(&c.line);
                ui.end_row();
            }
        });
    ui.add_space(10.0);
    title(ui, &format!("Traffic {}", r.traffic.len()));
    egui::Grid::new("past_traffic")
        .num_columns(6)
        .spacing([14.0, 3.0])
        .striped(true)
        .show(ui, |ui| {
            for it in &r.traffic {
                ui.label(RichText::new(clock_of(it.at, true)).monospace().color(theme::MUTED));
                ui.label(RichText::new(precedence_word(it.precedence)).color(prec_tone(it.precedence)));
                ui.label(RichText::new(&it.from).monospace().strong());
                let to = if it.to.is_empty() { "ALL" } else { &it.to };
                ui.label(RichText::new(format!("› {to}")).monospace().color(theme::MUTED));
                let subj = match it.kind {
                    Kind::Message => it.subject.clone(),
                    Kind::File => format!("{} ({})", it.name, theme::bytes(it.size as u64)),
                };
                ui.label(subj);
                ui.label(RichText::new(it.state.word()).color(theme::MUTED));
                ui.end_row();
            }
        });
    ui.add_space(10.0);
    title(ui, &format!("Bulletins {}", r.bulletins.len()));
    for b in &r.bulletins {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(clock_of(b.at, true)).monospace().color(theme::MUTED));
            ui.label(RichText::new(&b.from).monospace().strong());
            ui.label(&b.text);
        });
    }
    if !r.missed.is_empty() {
        ui.add_space(10.0);
        title(ui, &format!("Missed {}", r.missed.len()));
        let calls: Vec<&str> = r.missed.iter().map(|m| m.call.as_str()).collect();
        ui.label(RichText::new(calls.join(" ")).monospace());
    }
    if !r.announcement.trim().is_empty() {
        ui.add_space(10.0);
        egui::CollapsingHeader::new("Page")
            .id_salt("past_ann")
            .show(ui, |ui| markdown::show(ui, md.get(&r.announcement)));
    }
}
