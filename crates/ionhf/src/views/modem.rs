//! The modem: what Ion is doing, at a glance. The lamp panel and the
//! link's counts, the level on the ladder with S/N, rate and data, the
//! decode plot; below, the passband (slice thirds and waterfall), or the
//! log when it has the window.

use eframe::egui::{self, Layout, RichText, text::LayoutJob, vec2};
use ion::link::{Role, State};

use crate::app::App;
use crate::panel::{self, Lamp, Lit, Readout};
use crate::theme;

const GAP: f32 = 14.0;

/// Caption / value pairs as one line, wrapping at `width`.
fn pairs(ui: &mut egui::Ui, pairs: &[(&str, RichText)], width: f32) {
    let style = ui.style().clone();
    let mut job = LayoutJob::default();
    for (i, (k, v)) in pairs.iter().enumerate() {
        let gap = if i == 0 { "" } else { "   " };
        for part in [
            RichText::new(format!("{gap}{k} ")).size(11.0).color(theme::MUTED),
            v.clone(),
        ] {
            part.append_to(&mut job, &style, egui::FontSelection::Default, egui::Align::Center);
        }
    }
    job.wrap.max_width = width;
    ui.label(job);
}

fn lamps(app: &mut App, ui: &mut egui::Ui, width: f32, s: f32) {
    let st = app.link.as_ref();
    let now = app.now();
    let keyed = app.status.keyed;
    let busy = st.is_some_and(|s| s.busy);
    let link = match app.state() {
        State::Connected => Lit::On(theme::OK),
        State::Connecting | State::Disconnecting => Lit::On(theme::WARN),
        State::Failed => Lit::On(theme::BAD),
        _ => Lit::Off,
    };
    let host = match (app.host_on(), app.host_client) {
        (false, _) => Lit::Off,
        (true, false) => Lit::Armed(theme::US),
        (true, true) => Lit::On(theme::US),
    };
    let rig = match (&app.radio.problem, app.radio.connected) {
        (Some(_), _) => Lit::On(theme::WARN),
        (None, true) => Lit::On(theme::OK),
        (None, false) => Lit::Off,
    };
    let arq = app.linked();
    let groups = st.is_some_and(|s| !s.groups.is_empty());
    let bcast = match (groups, arq) {
        (false, _) => Lit::Off,
        (true, false) => Lit::On(theme::NET),
        (true, true) => Lit::Armed(theme::NET),
    };
    let cw_id = st.is_some_and(|s| s.cw_id) || now < app.lit_until.cw_id;
    let mut warn = Lamp::on("WARN", now < app.lit_until.warn, theme::WARN);
    warn.click = true;
    let row1 = [
        Lamp::on("TX", keyed, theme::TX),
        Lamp::on("RX", busy && !keyed, theme::OK),
        Lamp::new("LINK", link),
        Lamp::on("CW ID", cw_id, theme::WARN),
        Lamp::new("HOST", host),
        Lamp::new("RIG", rig),
    ];
    let stream = st.is_some_and(|s| s.stream.on);
    let row2 = [
        Lamp::on("ARQ", arq, theme::OK),
        Lamp::on("STREAM", stream, theme::OK),
        Lamp::new("BCAST", bcast),
        Lamp::on("HARQ", now < app.lit_until.harq, theme::WARN),
        Lamp::on("RETRY", now < app.lit_until.retry, theme::WARN),
        Lamp::on("LOST", now < app.lit_until.lost, theme::BAD),
        warn,
    ];
    let (level, sending) = on_air(app);
    let family = level.map(panel::family);
    let row3 = panel::FAMILIES.map(|f| {
        let c = if sending { theme::US } else { theme::PEER };
        Lamp::on(f, family == Some(f), c)
    });
    let tone = |legend, i: usize| {
        let (until, ours) = app.lit_until.tone[i];
        Lamp::on(legend, now < until, if ours { theme::US } else { theme::PEER })
    };
    let row4 = [
        Lamp::on("CUE", now < app.lit_until.cue, theme::WARN),
        tone("DONE", 0),
        tone("SLOWER", 1),
        tone("FASTER", 2),
    ];
    if panel::block(ui, &[&row1, &row2, &row3, &row4], width, s, Some(2)) == Some("WARN") {
        app.logbook.show_errors();
        app.log_view = true;
    }
}

/// The level on the air now and whether we send it: ours as the sender,
/// the peer's last as the receiver; with no link, our last broadcast
/// burst's while groups are open.
fn on_air(app: &App) -> (Option<usize>, bool) {
    let Some(s) = app.link.as_ref().filter(|_| app.linked()) else {
        let groups = app.link.as_ref().is_some_and(|s| !s.groups.is_empty());
        return (app.bcast_level.filter(|_| groups), true);
    };
    if s.role == Role::Sender {
        (Some(s.level), true)
    } else {
        (app.hist.peer_level, false)
    }
}

fn counts(app: &App, ui: &mut egui::Ui, width: f32) {
    let Some(s) = &app.link else { return };
    let counts = [
        ("Bursts", s.data_bursts.to_string()),
        ("Frames", s.frames_sent.to_string()),
        ("Resent", s.frames_resent.to_string()),
        ("HARQ", format!("{}/{}", s.increments_sent, s.harq_recovered)),
        ("Lost", s.codewords_failed.to_string()),
        ("Polls", s.polls.to_string()),
        ("Ctrl", s.control_bursts.to_string()),
        ("Air", theme::clock(s.airtime_s)),
    ]
    .map(|(k, v)| (k, RichText::new(v).monospace()));
    pairs(ui, &counts, width);
    // Broadcast groups a host program has open, and who is heard in them.
    if !s.groups.is_empty() {
        let heard = if s.heard.is_empty() {
            RichText::new("—").monospace().color(theme::MUTED)
        } else {
            let calls: Vec<String> = s.heard.iter().map(|(c, snr)| format!("{c} {snr:+.0}")).collect();
            RichText::new(calls.join("  ")).monospace()
        };
        pairs(
            ui,
            &[
                (
                    "Groups",
                    RichText::new(s.groups.join("  ")).monospace().color(theme::NET),
                ),
                ("Heard", heard),
            ],
            width,
        );
    }
}

fn level(app: &App, ui: &mut egui::Ui, width: f32, s: f32) {
    let st = app.link.as_ref().filter(|_| app.linked());
    let (level, sending) = on_air(app);
    let readout = Readout {
        level,
        sending,
        ours: st.and(app.hist.us_levels.last()).map(|(_, l)| *l as usize),
        peer: st.and(app.hist.peer_level),
        ceiling: st.map(|s| s.target_level),
    };
    panel::readout(ui, &readout, width, s);
    ui.add_space(4.0 * s);

    let big = |v: String| RichText::new(v).monospace().size(15.0 * s).strong();
    let link = app.link.as_ref();
    let rate = link.filter(|s| s.connected_s > 5.0).map_or("—".into(), |s| {
        panel::bps_text((s.bytes_acked + s.bytes_received) as f64 * 8.0 / s.connected_s)
    });
    let data = link.map_or("—".into(), |s| theme::bytes(s.bytes_acked + s.bytes_received));
    let snr = app.hist.last_here.map_or("—".into(), |v| format!("{v:+.1} dB"));
    pairs(
        ui,
        &[("S/N", big(snr)), ("Rate", big(rate)), ("Data", big(data))],
        width,
    );
    // The stream: its inner option, the period, and (receiving) the
    // pieces held of those the block needs.
    if let Some(x) = link.map(|l| &l.stream).filter(|x| x.on) {
        let color = if x.sending { theme::US } else { theme::PEER };
        let opt = x.opt.map_or("—", |o| ion::link::stream::OPTS[o].name);
        let mut line = vec![
            ("Stream", big(opt.to_string()).color(color)),
            ("Period", big(x.period.to_string())),
        ];
        if !x.sending {
            line.push(("Have", big(format!("{}/{}", x.have, x.need))));
        }
        pairs(ui, &line, width);
    }

    let scale = s;
    if let Some(s) = st {
        let total = s.bytes_acked + s.pending as u64;
        let frac = if total == 0 {
            0.0
        } else {
            s.bytes_acked as f32 / total as f32
        };
        let text = format!("TX {} / {}", theme::bytes(s.bytes_acked), theme::bytes(total));
        panel::progress(ui, frac, &text, width, scale);
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let full = ui.available_size();
    // Everything grows a little on a big window.
    let s = (full.x / 1250.0).min(full.y / 640.0).clamp(1.0, 1.4);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let y0 = ui.cursor().top();
        let w = ui.available_width();
        let side = (full.y * 0.34).clamp(170.0, 330.0).min(w * 0.3);
        let lamps_w = (470.0 * s).min((w - side - GAP) * 0.55);
        let spare = w - side - lamps_w - 2.0 * GAP;
        let level_w = spare.min(640.0 * s);
        let top = Layout::top_down(egui::Align::Min);
        if level_w >= 300.0 {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                ui.allocate_ui_with_layout(vec2(lamps_w, side), top, |ui| {
                    lamps(app, ui, lamps_w, s);
                    ui.add_space(6.0 * s);
                    counts(app, ui, lamps_w);
                });
                ui.allocate_ui_with_layout(vec2(level_w, side), top, |ui| level(app, ui, level_w, s));
                // The plot keeps to the right edge.
                ui.add_space(spare - level_w);
                app.plots.show(ui, side, s);
            });
        } else {
            // Narrow: the lamps beside the plot, the level below.
            let lamps_w = w - side - GAP;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                ui.allocate_ui_with_layout(vec2(lamps_w, side), top, |ui| {
                    lamps(app, ui, lamps_w, s);
                    ui.add_space(6.0 * s);
                    counts(app, ui, lamps_w);
                });
                app.plots.show(ui, side, s);
            });
            ui.add_space(8.0);
            level(app, ui, w.min(560.0), s);
        }
        ui.add_space(10.0);
        let used = ui.cursor().top() - y0;
        let rest = (full.y - used).max(160.0);
        if app.log_view {
            ui.allocate_ui_with_layout(vec2(w, rest), top, |ui| {
                ui.set_min_height(rest);
                app.logbook.show(ui);
            });
        } else {
            let band = app.link.as_ref().map(|s| s.band);
            if let Some(b) = band {
                theme::band_meter(ui, b, app.band_use());
            }
            let fall = (rest - if band.is_some() { 58.0 } else { 0.0 }).max(100.0);
            app.waterfall_panel(ui, fall);
        }
    });
}
