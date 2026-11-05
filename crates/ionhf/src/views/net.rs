//! Net: run one as net control or join one. Pages: the profile and its
//! announcement, the roster, the traffic in precedence order, the
//! check-in form, what came in, and the log of past nets. Beside them the
//! messages as they come, and a box to send.

use eframe::egui::{self, Margin, RichText};
use ion::link::net::{MAX_LINE, Precedence, Traffic};

use crate::app::{App, NetLine};
use crate::ion_engine::{IonCmd, traffic_words};
use crate::markdown;
use crate::net_app::{Draft, Page, floor_estimate_s};
use crate::net_doc::{self, Announcement, Kind, MAX_FILE, Mode, PRECEDENCES, Purpose, State, To, precedence_word};
use crate::net_log::{self, NetLog, utc};
use crate::theme;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::top("net_pages")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .inner_margin(Margin::symmetric(14, 6)),
        )
        .show(ui, |ui| pages(app, ui));
    let page = app.nets.page;
    if page == Page::Log {
        log_page(app, ui);
        return;
    }
    egui::Panel::left("net_ctl")
        .resizable(false)
        .exact_size(250.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("net_ctl_scroll")
                .show(ui, |ui| controls(app, ui))
        });
    if page != Page::Profile {
        egui::Panel::right("net_traffic")
            .resizable(true)
            .default_size(360.0)
            .min_size(260.0)
            .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
            .show(ui, |ui| messages(app, ui));
    }
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::INSET).inner_margin(Margin::same(14)))
        .show(ui, |ui| match page {
            Page::Profile => profile(app, ui),
            Page::Roster => roster(app, ui),
            Page::Traffic => traffic(app, ui),
            Page::CheckIn => checkin(app, ui),
            Page::Inbox => inbox(app, ui),
            Page::Log => {}
        });
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(theme::MUTED));
}

fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).strong());
}

/// Drawn: the fonts have no check mark.
fn tick(ui: &mut egui::Ui, color: egui::Color32) {
    let h = ui.text_style_height(&egui::TextStyle::Body);
    let (r, _) = ui.allocate_exact_size(egui::vec2(h, h), egui::Sense::hover());
    let p = |x: f32, y: f32| r.left_top() + egui::vec2(x, y) * h;
    let stroke = egui::Stroke::new(2.0, color);
    ui.painter().line_segment([p(0.15, 0.55), p(0.4, 0.8)], stroke);
    ui.painter().line_segment([p(0.4, 0.8), p(0.85, 0.25)], stroke);
}

fn abort(app: &mut App, ui: &mut egui::Ui, size: egui::Vec2) {
    let b = egui::Button::new(RichText::new("Abort").color(theme::BAD)).min_size(size);
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

/// A net is up and not closed.
fn active(app: &App) -> bool {
    app.net().is_some_and(|n| !n.closed)
}

fn prec_color(t: Traffic) -> egui::Color32 {
    match t.precedence {
        _ if t.items == 0 => theme::MUTED,
        p => prec_tone(p),
    }
}

fn prec_tone(p: Precedence) -> egui::Color32 {
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
                if page == Page::Log {
                    app.nets.refresh_past();
                }
            }
        };
        let ncs = role != Some(false);
        let member = role != Some(true);
        tab(ui, Page::Profile, "Profile", ncs);
        tab(ui, Page::Roster, "Roster", ncs);
        tab(ui, Page::Traffic, "Traffic", ncs);
        ui.add_space(14.0);
        tab(ui, Page::CheckIn, "Check-in", member);
        tab(ui, Page::Inbox, "Inbox", member);
        ui.add_space(14.0);
        tab(ui, Page::Log, "Log", true);
    });
}

// -- the left panel -----------------------------------------------------------------

fn controls(app: &mut App, ui: &mut egui::Ui) {
    let in_net = app.net().is_some();
    let role = app.net().map(|n| n.ncs);
    let can = !app.cfg.call.trim().is_empty() && !app.linked();
    let w = ui.available_width();
    let b = egui::vec2((w - 8.0) / 2.0, 30.0);

    if role != Some(false) {
        caption(ui, "Control");
        let edit = ui.add_enabled(
            !in_net,
            egui::TextEdit::singleline(&mut app.cfg.net_profile.name)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .hint_text("net"),
        );
        if edit.changed() {
            app.cfg.net_profile.name = app.cfg.net_profile.name.to_ascii_uppercase();
            app.mark_dirty();
        }
        ui.add_space(4.0);
        match app.net().cloned() {
            None => {
                let start = ui
                    .add_enabled(
                        can && !app.cfg.net_profile.name.trim().is_empty(),
                        egui::Button::new(RichText::new("Start").strong()).min_size(b),
                    )
                    .on_hover_text("Open the net as NCS; transmits");
                if start.clicked() {
                    app.net_open();
                }
            }
            Some(n) if app.nets.closing.is_some() => {
                let left = app.nets.closing.map_or(0.0, |c| c.until - app.now());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Closing {}", theme::clock(left))).color(theme::WARN));
                    if ui.add_enabled(!n.closed, egui::Button::new("Now")).clicked() {
                        let cmds = app.nets.close_now();
                        app.cmds(cmds);
                    }
                    abort(app, ui, egui::Vec2::ZERO);
                });
            }
            Some(n) => {
                ui.horizontal(|ui| {
                    let close = egui::Button::new("Close").min_size(b);
                    let close = ui.add_enabled(!n.closed, close).on_hover_text("Summary, then close");
                    if close.clicked() {
                        app.net_close();
                    }
                    abort(app, ui, b);
                });
                let mut open = n.open;
                let check = ui.add_enabled(!n.closed, egui::Checkbox::new(&mut open, "Check-ins"));
                if check.changed() {
                    app.cmd(IonCmd::NetCheckins(open));
                }
            }
        }
        ui.add_space(14.0);
    }

    if role != Some(true) {
        caption(ui, "Member");
        let edit = ui.add_enabled(
            !in_net,
            egui::TextEdit::singleline(&mut app.cfg.net)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .hint_text("any"),
        );
        if edit.changed() {
            app.cfg.net = app.cfg.net.to_ascii_uppercase();
            app.mark_dirty();
        }
        ui.add_space(4.0);
        if !in_net {
            if ui.add_enabled(can, egui::Button::new("Join").min_size(b)).clicked() {
                app.net_join();
            }
        } else {
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new("Leave").min_size(b)).clicked() {
                    app.cmd(IonCmd::NetClose);
                }
                abort(app, ui, b);
            });
        }
        ui.add_space(6.0);
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
        ui.add_space(14.0);
    }

    if let Some(n) = app.net() {
        let d = || "—".to_string();
        let mut rows = vec![
            ("Role", if n.ncs { "NCS".into() } else { "Member".into() }),
            ("Net", dash(&n.net)),
            ("NCS", n.ncs_call.clone().unwrap_or_else(d)),
            ("Round", n.rounds.to_string()),
            ("Queue", theme::bytes(n.pending as u64)),
            ("Offers", n.offers.iter().flatten().count().to_string()),
        ];
        if n.ncs {
            rows.push(("Stations", n.roster.len().to_string()));
        } else {
            rows.push(("Slot", n.slot.map_or_else(d, |s| s.to_string())));
            rows.push(("Via", n.relay.clone().unwrap_or_else(d)));
            rows.push(("Leaves", if n.leaves.is_empty() { d() } else { n.leaves.join(" ") }));
            rows.push(("Line", dash(&n.line)));
            rows.push(("Traffic", traffic_words(n.traffic)));
            rows.push(("Stations", n.members.len().to_string()));
        }
        egui::Grid::new("net_kv")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for (k, v) in rows {
                    caption(ui, k);
                    ui.label(RichText::new(v).monospace());
                    ui.end_row();
                }
            });
    }
}

// -- the announcement ---------------------------------------------------------------

/// The fields as a header block, then the body.
fn announcement(ui: &mut egui::Ui, a: &Announcement, md: &mut markdown::Cached) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(dash(&a.net)).size(20.0).strong().color(theme::NET));
        ui.add_space(4.0);
        let mut rows: Vec<(&str, String)> = Vec::new();
        let mut put = |k, v: String| {
            if !v.is_empty() {
                rows.push((k, v));
            }
        };
        put("NCS", a.ncs.clone());
        put("Alternate", a.alt.clone());
        put("Frequency", a.freq_hz.map(net_doc::freq_text).unwrap_or_default());
        put("Backup", a.backup_hz.map(net_doc::freq_text).unwrap_or_default());
        let times = match (a.start, a.end) {
            (Some(s), Some(e)) => format!("{} – {}", net_doc::minutes_text(s), net_doc::minutes_text(e)),
            (Some(s), None) => net_doc::minutes_text(s),
            (None, Some(e)) => format!("– {}", net_doc::minutes_text(e)),
            (None, None) => String::new(),
        };
        put("Time", times);
        put("Next", a.next.map(net_doc::minutes_text).unwrap_or_default());
        put("Mode", a.mode.map(|m| m.name().to_string()).unwrap_or_default());
        put("Purpose", a.purpose.map(|p| p.name().to_string()).unwrap_or_default());
        egui::Grid::new("ann_fields")
            .num_columns(2)
            .spacing([14.0, 3.0])
            .show(ui, |ui| {
                for (k, v) in rows {
                    caption(ui, k);
                    ui.label(RichText::new(v).monospace());
                    ui.end_row();
                }
            });
    });
    ui.add_space(8.0);
    markdown::show(ui, md.get(&a.body));
}

// -- net control --------------------------------------------------------------------

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
        caption(ui, "Body");
        let body = egui::ScrollArea::vertical()
            .id_salt("profile_body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_sized(
                    ui.available_size(),
                    egui::TextEdit::multiline(&mut p.body)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Markdown"),
                )
            })
            .inner;
        changed |= body.changed();
        committed |= body.lost_focus();

        let ui = &mut cols[1];
        title(ui, "Preview");
        ui.add_space(6.0);
        let a = app.cfg.net_profile.announcement(&app.cfg.call, net_log::now());
        egui::ScrollArea::vertical()
            .id_salt("profile_preview")
            .auto_shrink([false, false])
            .show(ui, |ui| announcement(ui, &a, &mut app.nets.md_preview));
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
        ui.label(RichText::new("No net").color(theme::MUTED));
        return;
    };
    ui.horizontal(|ui| {
        title(ui, "Roster");
        ui.label(RichText::new(dash(&n.net)).monospace().color(theme::NET));
        if n.closed {
            ui.label(RichText::new("Closed").color(theme::MUTED));
        }
    });
    ui.add_space(6.0);
    let ann = app
        .nets
        .offered()
        .and_then(|id| n.offers.iter().position(|o| *o == Some(id)));
    let mut to: Option<String> = None;
    egui::ScrollArea::vertical().id_salt("roster").show(ui, |ui| {
        egui::Grid::new("roster_rows")
            .num_columns(11)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for h in [
                    "Slot", "Call", "Line", "Traffic", "Up", "Down", "Route", "Level", "Missed", "Ann", "",
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
                    let held = ann.is_some_and(|i| r.holds & 1 << i != 0);
                    if held {
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

fn traffic(app: &mut App, ui: &mut egui::Ui) {
    let me = app.cfg.call.clone();
    let roster = app
        .net()
        .filter(|n| n.ncs)
        .map(|n| n.roster.clone())
        .unwrap_or_default();
    title(ui, "Listed");
    ui.add_space(4.0);
    let mut listed: Vec<_> = roster.iter().filter(|r| r.traffic.items > 0).collect();
    listed.sort_by_key(|r| (std::cmp::Reverse(r.traffic.precedence), !r.traffic.short, r.slot));
    if listed.is_empty() {
        ui.label(RichText::new("None").color(theme::MUTED));
    } else {
        egui::Grid::new("listed_rows")
            .num_columns(4)
            .spacing([16.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                for h in ["Precedence", "Call", "Items", "Short"] {
                    caption(ui, h);
                }
                ui.end_row();
                for r in listed {
                    let p = r.traffic.precedence;
                    ui.label(RichText::new(precedence_word(p)).color(prec_tone(p)));
                    ui.label(RichText::new(&r.call).monospace().strong());
                    ui.label(RichText::new(r.traffic.items.to_string()).monospace());
                    ui.label(if r.traffic.short { "Yes" } else { "" });
                    ui.end_row();
                }
            });
    }
    ui.add_space(12.0);
    title(ui, "Received");
    ui.add_space(4.0);
    let Some(log) = app.nets.log.as_ref() else {
        ui.label(RichText::new("No net").color(theme::MUTED));
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
    rows.sort_by_key(|(_, i)| (i.state == State::Handled, std::cmp::Reverse(i.precedence), i.at));
    let files = log.files_dir();
    let picked = app.nets.picked.and_then(|i| log.rec.traffic.get(i)).cloned();
    let can_pass = active(app);
    let mut pick: Option<usize> = None;
    let mut handled: Option<(usize, bool)> = None;
    let mut pass: Option<usize> = None;
    let mut folder = false;
    egui::ScrollArea::vertical()
        .id_salt("traffic_rows")
        .max_height((ui.available_height() - 140.0).max(120.0))
        .show(ui, |ui| {
            egui::Grid::new("traffic_rows_g")
                .num_columns(7)
                .spacing([14.0, 5.0])
                .striped(true)
                .show(ui, |ui| {
                    for h in ["Precedence", "Time", "From", "To", "Subject", "Handled", ""] {
                        caption(ui, h);
                    }
                    ui.end_row();
                    for (i, it) in &rows {
                        let p = it.precedence;
                        ui.label(RichText::new(precedence_word(p)).color(prec_tone(p)));
                        ui.label(RichText::new(clock_of(it.at, false)).monospace().color(theme::MUTED));
                        ui.label(RichText::new(&it.from).monospace().strong());
                        ui.label(RichText::new(if it.for_net { "Net" } else { "NCS" }).color(theme::MUTED));
                        let subj = match it.kind {
                            Kind::Message => it.subject.clone(),
                            Kind::File => format!("{} ({})", it.name, theme::bytes(it.size as u64)),
                        };
                        if ui.selectable_label(app.nets.picked == Some(*i), subj).clicked() {
                            pick = Some(*i);
                        }
                        let mut h = it.state == State::Handled;
                        if ui.checkbox(&mut h, "").changed() {
                            handled = Some((*i, h));
                        }
                        ui.horizontal(|ui| {
                            if it.for_net
                                && it.state != State::Handled
                                && ui.add_enabled(can_pass, egui::Button::new("Pass").small()).clicked()
                            {
                                pass = Some(*i);
                            }
                            if it.kind == Kind::File && ui.small_button("Folder").clicked() {
                                folder = true;
                            }
                        });
                        ui.end_row();
                    }
                });
        });
    if let Some(it) = picked {
        ui.add_space(8.0);
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

// -- member -------------------------------------------------------------------------

fn checkin(app: &mut App, ui: &mut egui::Ui) {
    let joined = app.nets.is_joined();
    title(ui, "Check-in");
    ui.add_space(6.0);
    let mut line_done = false;
    let mut line_changed = false;
    egui::Grid::new("checkin_form")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            caption(ui, "Call");
            ui.label(RichText::new(dash(&app.cfg.call)).monospace().strong());
            ui.end_row();

            caption(ui, "Line");
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut app.cfg.net_line)
                        .desired_width(340.0)
                        .hint_text("Trousdale County ARES"),
                );
                if r.changed() {
                    app.cfg.net_line = net_doc::clip(&app.cfg.net_line, MAX_LINE).to_string();
                    line_changed = true;
                }
                line_done = r.lost_focus();
                caption(ui, &format!("{}/{MAX_LINE}", app.cfg.net_line.len()));
            });
            ui.end_row();

            let f = &mut app.nets.form;
            caption(ui, "Traffic");
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.kind, Draft::None, "None");
                ui.radio_value(&mut f.kind, Draft::Message, "Message");
                ui.radio_value(&mut f.kind, Draft::File, "File");
            });
            ui.end_row();

            caption(ui, "Precedence");
            ui.horizontal(|ui| {
                for p in PRECEDENCES {
                    ui.selectable_value(
                        &mut f.precedence,
                        p,
                        RichText::new(precedence_word(p)).color(prec_tone(p)),
                    );
                }
            });
            ui.end_row();

            caption(ui, "To");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut f.to, To::Ncs, "NCS");
                ui.selectable_value(&mut f.to, To::Net, "Net (via NCS)");
            });
            ui.end_row();

            caption(ui, "Short");
            ui.checkbox(&mut f.short, "");
            ui.end_row();

            match f.kind {
                Draft::None => {}
                Draft::Message => {
                    caption(ui, "Message");
                    ui.add(
                        egui::TextEdit::multiline(&mut f.text)
                            .desired_rows(5)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();
                }
                Draft::File => {
                    caption(ui, "File");
                    ui.add(
                        egui::TextEdit::singleline(&mut f.file)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .hint_text("path"),
                    );
                    ui.end_row();
                    caption(ui, "Size");
                    let len = std::fs::metadata(f.file.trim())
                        .ok()
                        .filter(|m| m.is_file())
                        .map(|m| m.len());
                    match len {
                        Some(n) => {
                            let c = if n as usize > MAX_FILE { theme::BAD } else { theme::TEXT };
                            ui.label(
                                RichText::new(format!(
                                    "{} / {} · ≥ {}",
                                    theme::bytes(n),
                                    theme::bytes(MAX_FILE as u64),
                                    theme::clock(floor_estimate_s(n as usize))
                                ))
                                .monospace()
                                .color(c),
                            )
                            .on_hover_text("Uplink time at the floor, one frame a burst");
                        }
                        None => {
                            ui.label(RichText::new("—").color(theme::MUTED));
                        }
                    }
                    ui.end_row();
                }
            }
        });
    if line_changed {
        app.mark_dirty();
    }
    ui.add_space(8.0);
    let ready = app.nets.form_ready();
    let label = if joined { "Send" } else { "Queue" };
    let send = egui::Button::new(RichText::new(label).strong()).min_size(egui::vec2(110.0, 30.0));
    if ui.add_enabled(ready, send).clicked() {
        app.net_send_form();
    }
    if line_done && active(app) {
        let line = app.cfg.net_line.trim().to_string();
        app.cmd(IonCmd::NetLine(line));
    }

    ui.add_space(14.0);
    title(ui, "Outbox");
    ui.add_space(4.0);
    let me = app.cfg.call.clone();
    let ours: Vec<net_log::Item> = app
        .nets
        .log
        .as_ref()
        .filter(|l| l.rec.closed.is_none())
        .map(|l| l.rec.traffic.iter().filter(|i| i.from == me).cloned().collect())
        .unwrap_or_default();
    let queued = app.nets.queued();
    if ours.is_empty() && queued == 0 {
        ui.label(RichText::new("None").color(theme::MUTED));
        return;
    }
    if queued > 0 {
        ui.label(RichText::new(format!("{queued} queued")).color(theme::WARN));
    }
    egui::Grid::new("outbox_rows")
        .num_columns(4)
        .spacing([14.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for it in ours {
                ui.label(RichText::new(clock_of(it.at, false)).monospace().color(theme::MUTED));
                ui.label(RichText::new(precedence_word(it.precedence)).color(prec_tone(it.precedence)));
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
}

fn inbox(app: &mut App, ui: &mut egui::Ui) {
    let me = app.cfg.call.clone();
    let members = app.net().map(|n| n.members.clone()).unwrap_or_default();
    let files: Vec<net_log::Item> = app
        .nets
        .log
        .as_ref()
        .map(|l| {
            l.rec
                .traffic
                .iter()
                .filter(|i| i.kind == Kind::File && i.from != me)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let dir = app.nets.log.as_ref().map(NetLog::files_dir);
    let ann = app.nets.announcement.clone();
    let mut folder = false;
    egui::ScrollArea::vertical()
        .id_salt("inbox")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !files.is_empty() {
                title(ui, "Files");
                egui::Grid::new("inbox_files")
                    .num_columns(4)
                    .spacing([14.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for f in &files {
                            ui.label(RichText::new(&f.from).monospace().strong());
                            ui.label(&f.name);
                            ui.label(RichText::new(theme::bytes(f.size as u64)).monospace());
                            if ui.small_button("Folder").clicked() {
                                folder = true;
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(12.0);
            }
            if !members.is_empty() {
                title(ui, "Stations");
                ui.horizontal_wrapped(|ui| {
                    for (slot, call) in &members {
                        ui.label(RichText::new(slot.to_string()).monospace().color(theme::MUTED));
                        ui.label(RichText::new(call).monospace().strong());
                        ui.add_space(8.0);
                    }
                });
                ui.add_space(12.0);
            }
            title(ui, "Announcement");
            ui.add_space(6.0);
            match &ann {
                Some(a) => announcement(ui, a, &mut app.nets.md_inbox),
                None => {
                    ui.label(RichText::new("None").color(theme::MUTED));
                }
            }
        });
    if folder && let Some(d) = dir {
        open_folder(app, &d);
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
    let ncs = app.net().is_some_and(|n| n.ncs);
    title(ui, if ncs { "Bulletins" } else { "Messages" });
    ui.add_space(6.0);
    // A member may write before joining: it is queued.
    let can = if ncs {
        active(app)
    } else {
        !app.net().is_some_and(|n| n.closed)
    };
    let input_h = if ncs { 74.0 } else { 40.0 };
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
            caption(ui, "To");
            ui.add(
                egui::TextEdit::singleline(&mut app.net_to)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(110.0)
                    .hint_text("all"),
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

// -- the log ------------------------------------------------------------------------

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
        egui::CollapsingHeader::new("Announcement")
            .id_salt("past_ann")
            .show(ui, |ui| markdown::show(ui, md.get(&r.announcement)));
    }
}
