//! The window: Ion as a modem. A header (call, state, Log, Setup), the
//! modem view (lamps, level, decode plot, waterfall, log) or Setup;
//! events from the engine and the Ion worker; settings saved as they
//! change. Host programs (Ragtime, Pat, Winlink Express) do the talking.

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{self, Margin, RichText};
use ion::link::State;

use crate::config::{Clipper, Config};
use crate::engine::{Msg, Status, UiEvent};
use crate::history::History;
use crate::ion_engine::{IonCmd, IonEvent, IonHandle, IonStatus};
use crate::logbook::{Kind, Logbook};
use crate::plot::Plots;
use crate::radio::{LoopCmd, RadioInfo, RadioThread};
use crate::waterfall::{Band, Cursor, Waterfall};
use crate::{theme, views};

pub const SIZE: [f32; 2] = [1100.0, 640.0];
pub const MIN_SIZE: [f32; 2] = [640.0, 420.0];
/// How long an activity lamp stays lit after what lit it, seconds.
const FLASH_S: f64 = 3.0;
/// The WARN lamp after a warning or error, seconds.
const WARN_S: f64 = 60.0;

pub struct Threads {
    pub engine: Option<std::thread::JoinHandle<()>>,
    pub radio: RadioThread,
    pub ion: IonHandle,
    pub ui_tx: Sender<UiEvent>,
}

pub struct App {
    pub cfg: Config,
    pub loopback: bool,
    engine: Sender<Msg>,
    events: Receiver<UiEvent>,
    spectrum_return: Sender<Vec<f32>>,
    pub threads: Threads,
    pub status: Status,
    pub radio: RadioInfo,
    pub link: Option<IonStatus>,
    pub hist: History,
    pub logbook: Logbook,
    pub plots: Plots,
    /// Decode plots asked of the Ion worker.
    plot_on: bool,
    /// Until when (seconds since start) each activity lamp stays lit.
    pub lit_until: Flash,
    /// A host program is connected to the command port.
    pub host_client: bool,
    /// The level of our last broadcast burst.
    pub bcast_level: Option<usize>,
    /// The Log fills the window below the panel.
    pub log_view: bool,
    /// The Call box while it is being edited.
    pub call_draft: Option<String>,
    /// Sound cards for Setup: read in the background (slow on some systems), once and on Rescan.
    audio_devices: Option<(Vec<String>, Vec<String>)>,
    audio_scan: Option<std::sync::mpsc::Receiver<(Vec<String>, Vec<String>)>>,
    pub waterfall: Waterfall,
    /// Setup is showing.
    pub setup: bool,
    host: Option<crate::host::HostHandle>,
    last_state: State,
    dirty: bool,
    last_saved: Instant,
    /// Radio settings changed: they apply on the next start.
    pub restart: bool,
    /// Screenshots to this folder every 15 s; quit after this many seconds.
    pub shots: Option<std::path::PathBuf>,
    pub quit_after: Option<f64>,
    /// `--shot-set`: (name, seconds in).
    pub shot_set: Option<(String, f64)>,
    /// `--size`: the window in pixels, one point to a pixel.
    pub pixel_size: Option<[f32; 2]>,
    /// The zoom `--size` sets is not saved.
    sized: bool,
    /// A widget (a text box) had the keyboard at the end of the last frame.
    had_focus: bool,
    shot_steps: Vec<(f64, ShotStep)>,
    started: Instant,
    next_shot: f64,
}

/// Seconds since start until which each activity lamp stays lit.
#[derive(Default)]
pub struct Flash {
    pub harq: f64,
    pub retry: f64,
    pub lost: f64,
    pub warn: f64,
    pub cw_id: f64,
}

enum ShotStep {
    Size([f32; 2]),
    Shoot(String),
}

impl App {
    pub fn new(
        cfg: Config,
        loopback: bool,
        engine: Sender<Msg>,
        events: Receiver<UiEvent>,
        spectrum_return: Sender<Vec<f32>>,
        threads: Threads,
    ) -> Self {
        let _ = engine.send(Msg::Ion(Some(threads.ion.audio.clone())));
        let cmds = &threads.ion.cmds;
        let _ = cmds.send(IonCmd::SetCall(cfg.call.clone()));
        let _ = cmds.send(IonCmd::SetOfdmDb(cfg.ofdm_db));
        let _ = cmds.send(IonCmd::SetGear(cfg.gear.link()));
        let _ = cmds.send(IonCmd::SetBurstClip(cfg.clipper == Clipper::Burst));
        if loopback && let Some(lb) = threads.radio.ctl.loopback() {
            // The simulated station "LOOP" answers calls and echoes text.
            let _ = lb.send(LoopCmd::IonPeer(true));
        }
        let mut app = Self {
            loopback,
            engine,
            events,
            spectrum_return,
            threads,
            status: Status::default(),
            radio: RadioInfo::default(),
            link: None,
            hist: History::new(),
            logbook: Logbook::new(),
            plots: Plots::default(),
            plot_on: false,
            lit_until: Flash::default(),
            host_client: false,
            bcast_level: None,
            log_view: false,
            call_draft: None,
            audio_devices: None,
            audio_scan: None,
            waterfall: Waterfall::new(),
            setup: false,
            host: None,
            last_state: State::Idle,
            dirty: false,
            last_saved: Instant::now(),
            restart: false,
            shots: None,
            quit_after: None,
            shot_set: None,
            pixel_size: None,
            sized: false,
            had_focus: false,
            shot_steps: Vec::new(),
            started: Instant::now(),
            next_shot: 10.0,
            cfg,
        };
        app.note(format!(
            "Ion {} · {}",
            env!("CARGO_PKG_VERSION"),
            if loopback {
                "loopback (LOOP answers)"
            } else {
                app.cfg.radio.backend.short()
            }
        ));
        if app.cfg.call.trim().is_empty() {
            app.setup = true;
            app.note("! set your call");
        } else if app.cfg.answer {
            app.cmd(IonCmd::Listen);
        }
        if app.cfg.host_enabled {
            app.set_host(true);
        }
        app
    }

    pub fn cmd(&self, c: IonCmd) {
        let _ = self.threads.ion.cmds.send(c);
    }

    /// A line for the log; "! ..." is an error.
    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        match text.strip_prefix("! ") {
            Some(e) => self.error(e),
            None => self.logbook.push(Kind::Link, text),
        }
    }

    fn error(&mut self, text: &str) {
        self.lit_until.warn = self.now() + WARN_S;
        self.logbook.push(Kind::Error, text);
    }

    /// Seconds since start.
    pub fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn state(&self) -> State {
        self.link.as_ref().map_or(State::Idle, |s| s.state)
    }

    /// A link is up or being set up.
    pub fn linked(&self) -> bool {
        matches!(
            self.state(),
            State::Connecting | State::Connected | State::Disconnecting
        )
    }

    /// Calls a station (`--connect`, for tests: host programs do this).
    pub fn connect(&mut self, call: &str) {
        let call = call.trim().to_ascii_uppercase();
        if call.is_empty() || call.contains(char::is_whitespace) {
            return;
        }
        self.hist.new_link();
        self.cmd(IonCmd::Connect(call));
    }

    /// Drops the link at once, without transmitting.
    pub fn abort(&mut self) {
        self.cmd(IonCmd::Stop);
        if self.cfg.answer {
            self.cmd(IonCmd::Listen);
        }
    }

    /// Starts or stops the host interface.
    pub fn set_host(&mut self, on: bool) {
        if !on {
            if self.host.take().is_some() {
                self.cmd(IonCmd::AttachHost(None));
                self.note("Host off");
            }
            return;
        }
        if self.host.is_some() {
            return;
        }
        let port = self.cfg.host_port;
        match crate::host::spawn(port, self.threads.ion.cmds.clone(), self.threads.ui_tx.clone()) {
            Ok(h) => {
                self.note(format!("Host 127.0.0.1:{port} / {}", port + 1));
                self.host = Some(h);
            }
            Err(e) => {
                self.note(format!("! host port {port}: {e}"));
            }
        }
    }

    pub fn host_on(&self) -> bool {
        self.host.is_some()
    }

    pub fn set_drive(&mut self, pct: u32) {
        self.cfg.drive = pct;
        self.threads.radio.ctl.drive(pct);
        self.mark_dirty();
    }

    pub fn set_ofdm_db(&mut self, db: f32) {
        self.cfg.ofdm_db = db;
        self.cmd(IonCmd::SetOfdmDb(db));
        self.mark_dirty();
    }

    pub fn set_clipper(&mut self, c: Clipper) {
        self.cfg.clipper = c;
        self.cmd(IonCmd::SetBurstClip(c == Clipper::Burst));
        self.mark_dirty();
    }

    /// The sound card lists, starting a read the first time.
    pub fn audio_devices(&mut self) -> Option<&(Vec<String>, Vec<String>)> {
        if let Some(rx) = &self.audio_scan
            && let Ok(lists) = rx.try_recv()
        {
            self.audio_devices = Some(lists);
            self.audio_scan = None;
        }
        if self.audio_devices.is_none() && self.audio_scan.is_none() {
            self.rescan_audio_devices();
        }
        self.audio_devices.as_ref()
    }

    pub fn rescan_audio_devices(&mut self) {
        if self.audio_scan.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::soundcard::list_devices());
        });
        self.audio_scan = Some(rx);
    }

    pub fn set_call(&mut self, call: String) {
        self.cfg.call = call.trim().to_ascii_uppercase();
        self.cmd(IonCmd::SetCall(self.cfg.call.clone()));
        if self.cfg.answer && !self.cfg.call.is_empty() {
            self.cmd(IonCmd::Listen);
        }
        self.mark_dirty();
    }

    // -- events ------------------------------------------------------------

    fn pump(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                UiEvent::Spectrum(row) => {
                    self.waterfall.push_row(&row);
                    let _ = self.spectrum_return.try_send(row);
                }
                UiEvent::Status(s) => self.status = s,
                UiEvent::Warning(w) => self.error(&w),
                UiEvent::Radio(info) => {
                    if let Some(d) = info.drive {
                        self.cfg.drive = d;
                    }
                    self.radio = info;
                }
                UiEvent::Ion(ev) => self.ion_event(ev),
                UiEvent::Host(note) => {
                    if note.starts_with("Host connected") {
                        self.host_client = true;
                    } else if note.starts_with("Host disconnected") {
                        self.host_client = false;
                    }
                    self.logbook.push(Kind::Host, note);
                }
            }
        }
    }

    fn ion_event(&mut self, ev: IonEvent) {
        match ev {
            IonEvent::Status(s) => {
                if s.state != self.last_state {
                    self.state_changed(self.last_state, &s);
                    self.last_state = s.state;
                }
                self.activity(&s);
                self.hist.on_status(&s);
                // The bursts, as History keeps them, into the log.
                let rows = self.hist.rows.len().min(s.bursts.len());
                for r in self.hist.rows.iter().skip(self.hist.rows.len() - rows) {
                    let snr = r.snr.map_or(String::new(), |v| format!(" {v:+.0} dB"));
                    let lost = if r.lost > 0 {
                        format!(" lost {}", r.lost)
                    } else {
                        String::new()
                    };
                    let secs = if r.tx {
                        format!(" {:.1} s", r.secs)
                    } else {
                        String::new()
                    };
                    let kind = if r.tx { Kind::Tx } else { Kind::Rx };
                    self.logbook.push(kind, format!("{}{snr}{lost}{secs}", r.what));
                }
                if s.state == State::Connected {
                    self.hist.heard_peer(&s.peer.clone(), self.hist.last_here);
                }
                self.link = Some(s);
            }
            // With no host program attached, what a link brings has nowhere
            // to go: say how much.
            IonEvent::Received(bytes) => self.note(format!("{} received, no host", theme::bytes(bytes.len() as u64))),
            IonEvent::Plot(p) => self.plots.push(p),
            IonEvent::Note(text) => {
                let text = text.strip_prefix("Ion: ").unwrap_or(&text).to_string();
                // The state pill already says so.
                if text != "Listening" {
                    self.note(text);
                }
            }
        }
    }

    /// Lights the activity lamps for what changed since the last status.
    fn activity(&mut self, s: &IonStatus) {
        let now = self.now();
        if let Some(old) = &self.link {
            let rose = |a: u32, b: u32| b > a;
            if rose(old.increments_sent, s.increments_sent) || rose(old.harq_recovered, s.harq_recovered) {
                self.lit_until.harq = now + FLASH_S;
            }
            if rose(old.frames_resent, s.frames_resent) {
                self.lit_until.retry = now + FLASH_S;
            }
            if rose(old.codewords_failed, s.codewords_failed) {
                self.lit_until.lost = now + FLASH_S;
            }
        }
        for b in s.bursts.iter().filter(|b| b.tx) {
            // A mid-link CW ID goes out ahead of a burst.
            if b.what == "CW ID" {
                self.lit_until.cw_id = now + b.secs;
            }
            if b.what.starts_with("BCAST") {
                self.bcast_level = b.level;
            }
        }
    }

    fn state_changed(&mut self, from: State, s: &IonStatus) {
        match s.state {
            State::Connecting | State::Connected if from == State::Listening => {
                self.hist.new_link();
                self.plots.clear();
            }
            State::Disconnected | State::Failed if self.cfg.answer => self.cmd(IonCmd::Listen),
            _ => {}
        }
    }

    // -- drawing -----------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            ui.label(RichText::new("ION HF").strong().size(15.0).color(theme::TEXT));
            ui.label(RichText::new(&self.cfg.call).monospace().color(theme::MUTED));
            let (color, text, detail) = self.state_pill();
            theme::pill(ui, color, &text, &detail);
            if self.linked() {
                // Abort (Esc) drops the link without transmitting.
                if ui.button("Disconnect").clicked() {
                    self.cmd(IonCmd::Disconnect);
                }
                if ui.button(RichText::new("Abort").color(theme::BAD)).clicked() {
                    self.abort();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let label = if self.setup { "Done" } else { "Setup" };
                if ui.selectable_label(self.setup, label).clicked() {
                    self.setup = !self.setup;
                }
                if ui.selectable_label(self.log_view && !self.setup, "Log").clicked() {
                    self.log_view = !(self.log_view && !self.setup);
                    self.setup = false;
                }
            });
        });
    }

    /// Colour, word and detail for the state pill.
    pub fn state_pill(&self) -> (egui::Color32, String, String) {
        let s = self.link.as_ref();
        let peer = s.map_or(String::new(), |s| s.peer.clone());
        let groups = s.map_or(Vec::new(), |s| s.groups.clone());
        match self.state() {
            State::Idle | State::Listening | State::Disconnected | State::Failed if !groups.is_empty() => {
                (theme::NET, "Group".into(), groups.join(" "))
            }
            State::Idle => (theme::MUTED, "Idle".into(), String::new()),
            State::Listening => (theme::US, "Listening".into(), String::new()),
            State::Connecting => (theme::WARN, "Calling".into(), peer),
            State::Connected => (
                theme::OK,
                "Connected".into(),
                format!("{peer} {}", theme::clock(s.map_or(0.0, |s| s.connected_s))),
            ),
            State::Disconnecting => (theme::WARN, "Closing".into(), peer),
            State::Disconnected => (theme::MUTED, "Closed".into(), peer),
            State::Failed => (theme::BAD, "Failed".into(), peer),
        }
    }

    /// `--size`, `--shots`, `--shot-set` and `--quit`: a window of so many
    /// pixels, screenshots when they are due (saved as they arrive), and
    /// close when the time is up.
    fn scripted(&mut self, ctx: &egui::Context) {
        let t = self.now();
        if let Some(px) = self.pixel_size.take() {
            self.size_pixels(ctx, px);
        }
        if let Some((name, at)) = self.shot_set.take() {
            // Each size, a second and a half to settle.
            let mut when = at;
            for px in [[1280.0, 720.0], [1920.0, 1080.0]] {
                self.shot_steps.push((when, ShotStep::Size(px)));
                let file = format!("{name}_{}.png", px[0] as u32);
                self.shot_steps.push((when + 1.5, ShotStep::Shoot(file)));
                when += 2.0;
            }
            self.next_shot = f64::INFINITY;
        }
        while self.shot_steps.first().is_some_and(|(at, _)| t >= *at) {
            match self.shot_steps.remove(0).1 {
                ShotStep::Size(px) => self.size_pixels(ctx, px),
                ShotStep::Shoot(file) => {
                    if let Some(dir) = &self.shots {
                        shoot(ctx, dir.join(file));
                    }
                }
            }
        }
        if let Some(dir) = &self.shots
            && t >= self.next_shot
        {
            self.next_shot += 15.0;
            shoot(ctx, dir.join(format!("ion_{:03}s.png", t as u64)));
        }
        if self.quit_after.is_some_and(|q| t >= q) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The window at `px` pixels, one point to a pixel whatever the
    /// display's scaling.
    fn size_pixels(&mut self, ctx: &egui::Context, px: [f32; 2]) {
        self.sized = true;
        let native = ctx.native_pixels_per_point().unwrap_or(1.0);
        ctx.set_zoom_factor(1.0 / native);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(px[0], px[1]) / native));
    }

    /// What the link has on the air: on a slice level, the third the
    /// slice is in (ours as sender, the peer's pick; the peer's as
    /// receiver, ours, which its next burst follows); else the whole band.
    pub fn band_use(&self) -> theme::BandUse {
        use ion::level::{Guard, LADDER, Wave};
        let pick = self.link.as_ref().map_or(0, |s| s.slot_rx);
        let Some(s) = self.link.as_ref().filter(|s| s.state == State::Connected) else {
            return theme::BandUse::Idle(pick);
        };
        let sending = s.role == ion::link::Role::Sender;
        let level = if sending { Some(s.level) } else { self.hist.peer_level };
        let Some(l) = level.and_then(|l| LADDER.get(l)) else {
            return theme::BandUse::Idle(pick);
        };
        if l.wave == Wave::Ofdm(Guard::Slice) {
            theme::BandUse::Third(if sending { s.slot_tx } else { s.slot_rx }.min(2))
        } else {
            theme::BandUse::Full
        }
    }

    /// The passband the link is on (a slice's, or the whole band's), its
    /// name, and whether we send it.
    pub fn link_band(&self) -> Option<(f32, f32, &'static str, bool)> {
        let sending = self.link.as_ref()?.role == ion::link::Role::Sender;
        match self.band_use() {
            theme::BandUse::Third(slot) => {
                let c = ion::link::audio::CENTER_HZ + ion::slots::OFFSETS_HZ[slot as usize];
                Some((c - 325.0, c + 325.0, "Slice", sending))
            }
            theme::BandUse::Full => {
                let (lo, hi) = ion::link::audio::ofdm_band_hz();
                Some((lo, hi, "Full", sending))
            }
            theme::BandUse::Idle(_) => None,
        }
    }

    pub fn waterfall_panel(&mut self, ui: &mut egui::Ui, height: f32) {
        let (lo, hi) = ion::link::audio::ofdm_band_hz();
        let c = ion::link::audio::CENTER_HZ;
        let cursor = Cursor {
            center_hz: c,
            marks: [lo - c, hi - c],
            keyed: self.status.keyed,
        };
        let bands: Vec<Band> = self
            .link_band()
            .map(|(lo, hi, name, us)| Band {
                lo_hz: lo,
                hi_hz: hi,
                color: if us { theme::US } else { theme::PEER },
                label: name.into(),
            })
            .into_iter()
            .collect();
        self.waterfall.show(ui, height, &cursor, &bands);
    }
}

fn shoot(ctx: &egui::Context, path: std::path::PathBuf) {
    ctx.request_screenshot(move |img| {
        let [w, h] = img.size;
        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
        if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
            && let Err(e) = buf.save(&path)
        {
            log::warn!("{}: {e}", path.display());
        }
    });
}

impl eframe::App for App {
    fn persist_egui_memory(&self) -> bool {
        !self.sized
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        let ctx = ui.ctx().clone();
        // Esc in the log's search box only leaves the box (egui drops the
        // focus before this frame's widgets run, so last frame's counts).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.had_focus {
            self.abort();
        }
        self.scripted(&ctx);
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| self.header(ui));
        // Decode plots only while the modem screen shows.
        if self.plot_on == self.setup {
            self.plot_on = !self.setup;
            self.cmd(IonCmd::Plot(self.plot_on));
        }
        if self.setup {
            views::setup::show(self, ui);
        } else {
            if !self.log_view {
                egui::Panel::bottom("log")
                    .resizable(true)
                    .default_size(230.0)
                    .min_size(120.0)
                    .frame(
                        egui::Frame::new()
                            .fill(theme::BG)
                            .inner_margin(Margin::symmetric(12, 8)),
                    )
                    .show(ui, |ui| self.logbook.show(ui));
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme::BG).inner_margin(Margin::same(12)))
                .show(ui, |ui| views::modem::show(self, ui));
        }

        if self.dirty && self.last_saved.elapsed() > Duration::from_secs(2) {
            self.cfg.save();
            self.dirty = false;
            self.last_saved = Instant::now();
        }
        self.had_focus = ctx.memory(|m| m.focused().is_some());
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cmd(IonCmd::Stop);
        let _ = self.engine.send(Msg::Shutdown);
        if let Some(h) = self.threads.engine.take() {
            let _ = h.join();
        }
        self.threads.radio.stop();
        self.cfg.save();
    }
}
