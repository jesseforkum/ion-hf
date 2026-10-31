//! Setup: station, radio, levels, keying, host. Radio link changes apply
//! on the next start; levels and the host apply at once.

use crate::config::{Backend, Clipper, PttMethod, TxAudio};
use crate::rig::{cat, civ};
use eframe::egui::{self, Margin, RichText};

use crate::app::App;
use crate::theme;

/// Narrowest card body; the cards share the width beyond it.
const CARD_MIN: f32 = 260.0;
/// A card's frame: margins and stroke.
const CARD_EDGE: f32 = 26.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(12)))
        .show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                // Cards in rows, as many across as fit.
                type Body = fn(&mut App, &mut egui::Ui);
                let cards: [(&str, Body); 5] = [
                    ("Station", station),
                    ("Radio", radio),
                    ("Levels", levels),
                    ("Keying", keying),
                    ("Host", host),
                ];
                let gap = 12.0;
                let avail = ui.available_width();
                let across = (((avail + gap) / (CARD_MIN + CARD_EDGE + gap)) as usize).max(1);
                let w = (avail - gap * (across - 1) as f32) / across as f32 - CARD_EDGE;
                ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
                for row in cards.chunks(across) {
                    ui.horizontal_top(|ui| {
                        for (title, body) in row {
                            card(ui, title, w, |ui| body(app, ui));
                        }
                    });
                }
            });
        });
}

fn card(ui: &mut egui::Ui, title: &str, width: f32, body: impl FnOnce(&mut egui::Ui)) {
    theme::card().show(ui, |ui| {
        ui.set_width(width);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).strong().size(14.0));
            ui.add_space(6.0);
            body(ui);
        });
    });
}

fn row(ui: &mut egui::Ui, grid: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(grid)
        .num_columns(2)
        .spacing([12.0, 6.0])
        .min_col_width(60.0)
        .show(ui, body);
}

fn station(app: &mut App, ui: &mut egui::Ui) {
    let mut call = app.call_draft.clone().unwrap_or_else(|| app.cfg.call.clone());
    let mut grid = app.cfg.grid.clone();
    row(ui, "station", |ui| {
        ui.label(RichText::new("Call").color(theme::MUTED));
        let c = ui.add(
            egui::TextEdit::singleline(&mut call)
                .font(egui::TextStyle::Monospace)
                .char_limit(10),
        );
        // A new call restarts the station, so it applies when the box is left.
        if c.lost_focus() {
            app.call_draft = None;
            if call.trim().to_ascii_uppercase() != app.cfg.call {
                app.set_call(call.clone());
            }
        } else if c.has_focus() {
            app.call_draft = Some(call.clone());
        }
        ui.end_row();
        ui.label(RichText::new("Grid").color(theme::MUTED));
        if ui
            .add(
                egui::TextEdit::singleline(&mut grid)
                    .font(egui::TextStyle::Monospace)
                    .char_limit(8),
            )
            .changed()
        {
            app.cfg.grid = grid.trim().to_string();
            app.mark_dirty();
        }
        ui.end_row();
    });
    if app.cfg.call.trim().is_empty() {
        ui.label(RichText::new("! call needed").color(theme::WARN));
    }
}

fn radio(app: &mut App, ui: &mut egui::Ui) {
    let before = app.cfg.radio.clone();
    let before_url = app.cfg.tci_url.clone();
    let lists = if app.cfg.radio.backend != Backend::Tci {
        app.audio_devices().cloned().unwrap_or_default()
    } else {
        Default::default()
    };
    let mut rescan = false;
    let r = &mut app.cfg.radio;
    row(ui, "radio", |ui| {
        ui.label(RichText::new("Link").color(theme::MUTED));
        egui::ComboBox::from_id_salt("backend")
            .selected_text(r.backend.short())
            .show_ui(ui, |ui| {
                for b in Backend::ALL {
                    ui.selectable_value(&mut r.backend, b, b.short());
                }
            });
        ui.end_row();
        match r.backend {
            Backend::Tci => {
                ui.label(RichText::new("TCI").color(theme::MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut app.cfg.tci_url)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(ui.available_width()),
                );
                ui.end_row();
            }
            Backend::Rigctld => {
                ui.label(RichText::new("rigctld").color(theme::MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut r.rigctld_addr)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(ui.available_width()),
                );
                ui.end_row();
            }
            Backend::Icom | Backend::Cat => {
                ui.label(RichText::new("Model").color(theme::MUTED));
                if r.backend == Backend::Icom {
                    egui::ComboBox::from_id_salt("icom")
                        .selected_text(r.icom_model.clone())
                        .show_ui(ui, |ui| {
                            for (name, addr) in civ::MODELS {
                                if ui.selectable_label(r.icom_model == *name, *name).clicked() {
                                    r.icom_model = name.to_string();
                                    r.civ_addr = *addr;
                                }
                            }
                        });
                } else {
                    egui::ComboBox::from_id_salt("cat")
                        .selected_text(r.cat_model.clone())
                        .show_ui(ui, |ui| {
                            for m in cat::MODELS {
                                if ui.selectable_label(r.cat_model == m.name, m.name).clicked() {
                                    r.cat_model = m.name.to_string();
                                    r.baud = m.baud;
                                }
                            }
                        });
                }
                ui.end_row();
                ui.label(RichText::new("Port").color(theme::MUTED));
                ui.add(
                    egui::TextEdit::singleline(&mut r.port)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("COM3"),
                );
                ui.end_row();
                ui.label(RichText::new("Baud").color(theme::MUTED));
                ui.add(egui::DragValue::new(&mut r.baud).range(1200..=115_200));
                ui.end_row();
            }
            Backend::None => {}
        }
        if r.backend != Backend::Tci {
            let (ins, outs) = &lists;
            ui.label(RichText::new("In").color(theme::MUTED));
            device(ui, "audio_in", &mut r.audio_in, ins);
            ui.end_row();
            ui.label(RichText::new("Out").color(theme::MUTED));
            device(ui, "audio_out", &mut r.audio_out, outs);
            ui.end_row();
            ui.label("");
            rescan = ui.button("Rescan").clicked();
            ui.end_row();
        }
    });
    if rescan {
        app.rescan_audio_devices();
    }
    if app.cfg.radio != before || app.cfg.tci_url != before_url {
        app.restart = true;
        app.mark_dirty();
    }
    ui.add_space(6.0);
    let (color, text) = if app.loopback {
        (theme::NET, "Loopback".to_string())
    } else if app.radio.connected {
        (
            theme::OK,
            app.radio.device.clone().unwrap_or_else(|| "Connected".into()),
        )
    } else {
        (theme::BAD, "Not connected".to_string())
    };
    ui.label(RichText::new(text).color(color));
    if let Some(p) = &app.radio.problem {
        ui.label(RichText::new(format!("! {p}")).color(theme::WARN));
    }
    if app.restart {
        ui.label(RichText::new("Restart to apply").color(theme::WARN));
    }
}

fn device(ui: &mut egui::Ui, id: &str, value: &mut String, list: &[String]) {
    let shown = if value.is_empty() {
        "Default".to_string()
    } else {
        value.clone()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown)
        .width(ui.available_width().min(230.0))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, String::new(), "Default");
            for d in list {
                ui.selectable_value(value, d.clone(), d);
            }
        });
}

fn levels(app: &mut App, ui: &mut egui::Ui) {
    let mut drive = app.cfg.drive;
    let mut ofdm = app.cfg.ofdm_db;
    row(ui, "levels", |ui| {
        ui.label(RichText::new("Drive").color(theme::MUTED));
        let d = ui.add(egui::Slider::new(&mut drive, 0..=100).suffix(" %"));
        // Arrow keys never change transmit power: a stray press on a
        // focused slider would key louder (the slider snaps back).
        let arrows = ui.input(|i| {
            [
                egui::Key::ArrowLeft,
                egui::Key::ArrowRight,
                egui::Key::ArrowUp,
                egui::Key::ArrowDown,
            ]
            .iter()
            .any(|k| i.key_pressed(*k))
        });
        if d.drag_stopped() || (d.changed() && !d.dragged() && !arrows) {
            app.set_drive(drive);
        }
        ui.end_row();
        ui.label(RichText::new("OFDM").color(theme::MUTED));
        let o = ui.add(egui::Slider::new(&mut ofdm, 0.0..=3.0).step_by(0.1).suffix(" dB"));
        if o.changed() {
            app.set_ofdm_db(ofdm);
        }
        ui.end_row();
        ui.label(RichText::new("Clipper").color(theme::MUTED));
        let mut clipper = app.cfg.clipper;
        ui.horizontal(|ui| {
            for c in Clipper::ALL {
                ui.selectable_value(&mut clipper, c, c.name());
            }
        });
        if clipper != app.cfg.clipper {
            app.set_clipper(clipper);
        }
        ui.end_row();
    });
    if app.radio.drive_unsupported {
        ui.label(RichText::new("Drive: set on the radio").color(theme::MUTED));
    }
}

fn keying(app: &mut App, ui: &mut egui::Ui) {
    let r = &mut app.cfg.radio;
    let before = r.clone();
    row(ui, "keying", |ui| {
        ui.label(RichText::new("PTT").color(theme::MUTED));
        ui.add_enabled_ui(r.backend != Backend::Tci, |ui| {
            egui::ComboBox::from_id_salt("ptt")
                .selected_text(r.ptt.name())
                .show_ui(ui, |ui| {
                    for p in [PttMethod::Cat, PttMethod::Rts, PttMethod::Dtr, PttMethod::Vox] {
                        ui.selectable_value(&mut r.ptt, p, p.name());
                    }
                });
        });
        ui.end_row();
        // Only Kenwood CAT and Hamlib tell the radio which input to key.
        let picks_input = r.ptt == PttMethod::Cat
            && (r.backend == Backend::Rigctld
                || (r.backend == Backend::Cat
                    && cat::model(&r.cat_model).is_some_and(|m| m.brand == cat::Brand::Kenwood)));
        ui.label(RichText::new("TX audio").color(theme::MUTED));
        ui.add_enabled_ui(picks_input, |ui| {
            egui::ComboBox::from_id_salt("tx_audio")
                .selected_text(r.tx_audio.name())
                .show_ui(ui, |ui| {
                    for a in TxAudio::ALL {
                        ui.selectable_value(&mut r.tx_audio, a, a.name());
                    }
                });
        });
        ui.end_row();
        ui.label(RichText::new("Lead").color(theme::MUTED));
        ui.add_enabled(
            r.backend != Backend::Tci,
            egui::DragValue::new(&mut r.ptt_lead_ms).range(0..=500).suffix(" ms"),
        );
        ui.end_row();
    });
    if *r != before {
        app.restart = true;
        app.mark_dirty();
    }
}

fn host(app: &mut App, ui: &mut egui::Ui) {
    let mut on = app.host_on();
    let mut port = app.cfg.host_port;
    row(ui, "host", |ui| {
        ui.label(RichText::new("VARA").color(theme::MUTED));
        if ui.checkbox(&mut on, "On").changed() {
            app.cfg.host_enabled = on;
            app.set_host(on);
            app.mark_dirty();
        }
        ui.end_row();
        ui.label(RichText::new("Port").color(theme::MUTED));
        let p = ui.add_enabled(!app.host_on(), egui::DragValue::new(&mut port).range(1024..=65_000));
        if p.changed() {
            app.cfg.host_port = port;
            app.mark_dirty();
        }
        ui.end_row();
        ui.label(RichText::new("Data").color(theme::MUTED));
        ui.label(RichText::new((port + 1).to_string()).monospace());
        ui.end_row();
    });
}
