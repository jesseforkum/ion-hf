//! Net: run one as net control or join one; the roster; the traffic and
//! bulletins, and a box to send.

use eframe::egui::{self, Margin, RichText};
use ion::link::net::Precedence;

use crate::app::{App, NetLine};
use crate::ion_engine::IonCmd;
use crate::theme;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("net_ctl")
        .resizable(false)
        .exact_size(250.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("net_ctl_scroll")
                .show(ui, |ui| controls(app, ui))
        });
    egui::Panel::right("net_traffic")
        .resizable(true)
        .default_size(400.0)
        .min_size(260.0)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(Margin::same(14)))
        .show(ui, |ui| traffic(app, ui));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::INSET).inner_margin(Margin::same(14)))
        .show(ui, |ui| roster(app, ui));
}

fn caption(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).color(theme::MUTED));
}

fn db(v: Option<f32>) -> String {
    v.map_or("—".into(), |v| format!("{v:+.0}"))
}

/// A net is up and not closed.
fn active(app: &App) -> bool {
    app.net().is_some_and(|n| !n.closed)
}

fn controls(app: &mut App, ui: &mut egui::Ui) {
    caption(ui, "Net");
    let in_net = app.net().is_some();
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
    ui.add_space(6.0);
    let w = ui.available_width();
    let b = egui::vec2((w - 8.0) / 2.0, 30.0);
    if !in_net {
        let can = !app.cfg.call.trim().is_empty() && !app.linked();
        ui.horizontal(|ui| {
            let open = ui.add_enabled(
                can && !app.cfg.net.trim().is_empty(),
                egui::Button::new(RichText::new("Open").strong()).min_size(b),
            );
            if open.clicked() {
                app.net_started(true);
            }
            if ui.add_enabled(can, egui::Button::new("Join").min_size(b)).clicked() {
                app.net_started(false);
            }
        });
    } else if let Some(n) = app.net().cloned() {
        ui.horizontal(|ui| {
            if n.ncs {
                let mut open = n.open;
                let check = ui.add_enabled(!n.closed, egui::Checkbox::new(&mut open, "Check-ins"));
                if check.changed() {
                    app.cmd(IonCmd::NetCheckins(open));
                }
                let close = egui::Button::new(RichText::new("Close").color(theme::BAD)).min_size(b);
                if ui.add_enabled(!n.closed, close).clicked() {
                    app.cmd(IonCmd::NetClose);
                }
            } else if ui.add(egui::Button::new("Leave").min_size(b)).clicked() {
                app.cmd(IonCmd::NetClose);
            }
        });
    }

    ui.add_space(14.0);
    if let Some(n) = app.net() {
        let dash = || "—".to_string();
        let rows = [
            ("Role", if n.ncs { "NCS".into() } else { "Member".into() }),
            ("NCS", n.ncs_call.clone().unwrap_or_else(dash)),
            ("Slot", n.slot.map_or_else(dash, |s| s.to_string())),
            ("Round", n.rounds.to_string()),
            ("Via", n.relay.clone().unwrap_or_else(dash)),
            ("Queue", theme::bytes(n.pending as u64)),
            (
                "Leaves",
                if n.leaves.is_empty() {
                    dash()
                } else {
                    n.leaves.join(" ")
                },
            ),
            ("Line", if n.line.is_empty() { dash() } else { n.line.clone() }),
            ("Traffic", crate::ion_engine::traffic_words(n.traffic)),
            ("Offers", n.offers.len().to_string()),
        ];
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
        ui.add_space(14.0);
    }

    caption(ui, "Caps");
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
}

fn roster(app: &mut App, ui: &mut egui::Ui) {
    let Some(n) = app.net().cloned() else {
        ui.label(RichText::new("No net").color(theme::MUTED));
        return;
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Roster").strong());
        let name = if n.net.is_empty() { "…" } else { n.net.as_str() };
        ui.label(RichText::new(name).monospace().color(theme::NET));
        if n.closed {
            ui.label(RichText::new("Closed").color(theme::MUTED));
        }
    });
    ui.add_space(6.0);
    if n.ncs {
        let mut to: Option<String> = None;
        egui::ScrollArea::vertical().id_salt("roster").show(ui, |ui| {
            egui::Grid::new("roster_rows")
                .num_columns(10)
                .spacing([18.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    for h in [
                        "Slot", "Call", "Line", "Traffic", "Up", "Down", "Via", "Level", "Missed", "",
                    ] {
                        caption(ui, h);
                    }
                    ui.end_row();
                    for r in &n.roster {
                        ui.label(RichText::new(r.slot.to_string()).monospace().color(theme::MUTED));
                        ui.label(RichText::new(&r.call).monospace().strong());
                        ui.label(&r.line);
                        let tc = match r.traffic.precedence {
                            _ if r.traffic.items == 0 => theme::MUTED,
                            Precedence::Emergency => theme::BAD,
                            Precedence::Priority => theme::WARN,
                            _ => theme::TEXT,
                        };
                        let words = crate::ion_engine::traffic_words(r.traffic);
                        ui.label(RichText::new(words).monospace().color(tc));
                        ui.label(RichText::new(db(r.snr_up)).monospace());
                        ui.label(RichText::new(db(r.snr_down)).monospace());
                        ui.label(
                            RichText::new(r.via.as_deref().unwrap_or("—"))
                                .monospace()
                                .color(theme::MUTED),
                        );
                        ui.label(RichText::new(format!("L{}", r.cap)).monospace().color(theme::MUTED));
                        let mc = if r.missed > 0 { theme::BAD } else { theme::MUTED };
                        ui.label(RichText::new(r.missed.to_string()).monospace().color(mc));
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
    } else {
        egui::Grid::new("members")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                caption(ui, "Slot");
                caption(ui, "Call");
                ui.end_row();
                for (slot, call) in &n.members {
                    ui.label(RichText::new(slot.to_string()).monospace().color(theme::MUTED));
                    ui.label(RichText::new(call).monospace().strong());
                    ui.end_row();
                }
            });
    }
}

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

fn traffic(app: &mut App, ui: &mut egui::Ui) {
    ui.label(RichText::new("Traffic").strong());
    ui.add_space(6.0);
    let in_net = active(app);
    let ncs = app.net().is_some_and(|n| n.ncs);
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
            in_net,
            egui::TextEdit::singleline(&mut app.net_text).desired_width(ui.available_width() - 64.0),
        );
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.add_enabled(in_net, egui::Button::new("Send")).clicked() || enter) && in_net {
            app.net_send();
        }
    });
}
