//! The log: every note, burst and host event, kept for the Log view and
//! written as it happens to `logs/ionhf.log` in the settings folder
//! ([`Config::dir`]), which rolls over to `ionhf.1.log` .. `ionhf.3.log`
//! by size.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{LineWriter, Write};
use std::path::PathBuf;

use eframe::egui::{self, Color32, RichText, text::LayoutJob};

use crate::config::Config;
use crate::theme;

/// Lines kept for the view (the file keeps everything).
const KEEP: usize = 20_000;
/// The file rolls over past this many bytes; this many old ones stay.
const ROLL_BYTES: u64 = 2_000_000;
const OLD_FILES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Link,
    Tx,
    Rx,
    Host,
    Error,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Link => "LINK",
            Kind::Tx => "TX",
            Kind::Rx => "RX",
            Kind::Host => "HOST",
            Kind::Error => "ERR",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Kind::Link => theme::OK,
            Kind::Tx => theme::US,
            Kind::Rx => theme::PEER,
            Kind::Host => theme::NET,
            Kind::Error => theme::BAD,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    Link,
    Bursts,
    Host,
    Errors,
}

impl Filter {
    const ALL: [Filter; 5] = [Filter::All, Filter::Link, Filter::Bursts, Filter::Host, Filter::Errors];

    fn name(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Link => "Link",
            Filter::Bursts => "Bursts",
            Filter::Host => "Host",
            Filter::Errors => "Errors",
        }
    }

    fn passes(self, k: Kind) -> bool {
        match self {
            Filter::All => true,
            Filter::Link => k == Kind::Link,
            Filter::Bursts => matches!(k, Kind::Tx | Kind::Rx),
            Filter::Host => k == Kind::Host,
            Filter::Errors => k == Kind::Error,
        }
    }
}

pub struct Line {
    /// UTC, "HH:MM:SS".
    pub at: String,
    pub kind: Kind,
    pub text: String,
}

impl Line {
    fn plain(&self) -> String {
        format!("{} {:<4} {}", self.at, self.kind.tag(), self.text)
    }
}

/// The log file, rolled over by size.
struct LogFile {
    dir: PathBuf,
    out: LineWriter<File>,
    bytes: u64,
}

impl LogFile {
    fn path(dir: &std::path::Path, n: usize) -> PathBuf {
        if n == 0 {
            dir.join("ionhf.log")
        } else {
            dir.join(format!("ionhf.{n}.log"))
        }
    }

    fn open(dir: PathBuf) -> std::io::Result<LogFile> {
        std::fs::create_dir_all(&dir)?;
        let f = OpenOptions::new().create(true).append(true).open(Self::path(&dir, 0))?;
        let bytes = f.metadata().map_or(0, |m| m.len());
        Ok(LogFile {
            dir,
            out: LineWriter::new(f),
            bytes,
        })
    }

    fn roll(&mut self) -> std::io::Result<()> {
        let _ = self.out.flush();
        for n in (0..OLD_FILES).rev() {
            let from = Self::path(&self.dir, n);
            if from.exists() {
                std::fs::rename(&from, Self::path(&self.dir, n + 1))?;
            }
        }
        *self = LogFile::open(self.dir.clone())?;
        Ok(())
    }

    fn write(&mut self, line: &str) {
        if self.bytes > ROLL_BYTES
            && let Err(e) = self.roll()
        {
            log::warn!("log roll-over: {e}");
        }
        if self.out.write_all(line.as_bytes()).is_ok() {
            self.bytes += line.len() as u64;
        }
    }
}

pub struct Logbook {
    lines: VecDeque<Line>,
    file: Option<LogFile>,
    filter: Filter,
    search: String,
    /// The view was at its newest line when last drawn.
    live: bool,
    jump_live: bool,
}

impl Logbook {
    pub fn new() -> Self {
        let file = match LogFile::open(Self::dir()) {
            Ok(f) => Some(f),
            Err(e) => {
                log::warn!("{}: {e}", Self::dir().display());
                None
            }
        };
        Logbook {
            lines: VecDeque::new(),
            file,
            filter: Filter::All,
            search: String::new(),
            live: true,
            jump_live: false,
        }
    }

    pub fn dir() -> PathBuf {
        Config::dir().join("logs")
    }

    pub fn push(&mut self, kind: Kind, text: impl Into<String>) {
        let now = chrono::Utc::now();
        let line = Line {
            at: now.format("%H:%M:%S").to_string(),
            kind,
            text: text.into(),
        };
        if let Some(f) = &mut self.file {
            f.write(&format!(
                "{} {:<4} {}\n",
                now.format("%Y-%m-%d %H:%M:%SZ"),
                kind.tag(),
                line.text
            ));
        }
        if self.lines.len() >= KEEP {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Show only errors, from the newest.
    pub fn show_errors(&mut self) {
        self.filter = Filter::Errors;
        self.jump_live = true;
    }

    fn shown(&self) -> Vec<&Line> {
        let needle = self.search.trim().to_lowercase();
        self.lines
            .iter()
            .filter(|l| self.filter.passes(l.kind))
            .filter(|l| needle.is_empty() || l.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// The lines shown, as text.
    fn text(&self) -> String {
        self.shown().iter().map(|l| l.plain() + "\n").collect()
    }

    /// Writes the lines shown to a new file beside the log; says where.
    fn save(&mut self) {
        let path = Self::dir().join(format!("saved-{}.txt", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
        let text = self.text();
        match std::fs::create_dir_all(Self::dir()).and_then(|()| std::fs::write(&path, text)) {
            Ok(()) => self.push(Kind::Link, format!("Log saved: {}", path.display())),
            Err(e) => self.push(Kind::Error, format!("{}: {e}", path.display())),
        }
    }

    fn open_folder(&mut self) {
        let dir = Self::dir();
        let _ = std::fs::create_dir_all(&dir);
        let program = if cfg!(windows) {
            "explorer"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if let Err(e) = std::process::Command::new(program).arg(&dir).spawn() {
            self.push(Kind::Error, format!("{program} {}: {e}", dir.display()));
        }
    }

    fn search_box(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search")
                .desired_width(150.0),
        );
        if !self.search.is_empty() && ui.small_button("×").clicked() {
            self.search.clear();
        }
    }

    /// Folder, Save and Copy, right to left; whether Copy was clicked.
    /// Android has no file manager to open a folder in.
    fn file_buttons(&mut self, ui: &mut egui::Ui) -> bool {
        if !cfg!(target_os = "android") && ui.button("Folder").clicked() {
            self.open_folder();
        }
        if ui.button("Save").clicked() {
            self.save();
        }
        ui.button("Copy").clicked()
    }

    /// Toolbar, then the lines: they follow the newest until scrolled
    /// back; LIVE returns to it.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let mut copy = false;
        // A phone: search and the file buttons on a row of their own.
        let narrow = ui.available_width() < 520.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(RichText::new("LOG").monospace().strong().color(theme::MUTED));
            ui.add_space(6.0);
            for f in Filter::ALL {
                if ui.selectable_label(self.filter == f, f.name()).clicked() {
                    self.filter = f;
                }
            }
            if !narrow {
                ui.add_space(8.0);
                self.search_box(ui);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !narrow {
                    copy = self.file_buttons(ui);
                }
                let live = self.live && !self.jump_live;
                let text =
                    RichText::new("LIVE")
                        .monospace()
                        .strong()
                        .color(if live { theme::OK } else { theme::MUTED });
                if ui
                    .add(egui::Button::new(text).selected(live))
                    .on_hover_text("Follow the newest")
                    .clicked()
                {
                    self.jump_live = true;
                }
            });
        });
        if narrow {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.search_box(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    copy = self.file_buttons(ui);
                });
            });
        }
        if copy {
            ui.ctx().copy_text(self.text());
        }
        ui.add_space(2.0);

        let shown = self.shown();
        let font = egui::FontId::monospace(12.0);
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let mut area = egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(true);
        if self.jump_live {
            area = area.vertical_scroll_offset(f32::MAX);
        }
        let style = ui.style().clone();
        let out = egui::Frame::new()
            .fill(theme::BG)
            .stroke(egui::Stroke::new(1.0, theme::LINE))
            .inner_margin(egui::Margin::symmetric(6, 4))
            .show(ui, |ui| {
                area.show_rows(ui, row_h, shown.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for l in &shown[range] {
                        let mut job = LayoutJob::default();
                        let ink = if l.kind == Kind::Error { theme::BAD } else { theme::TEXT };
                        for part in [
                            RichText::new(format!("{} ", l.at))
                                .font(font.clone())
                                .color(theme::MUTED),
                            RichText::new(format!("{:<5}", l.kind.tag()))
                                .font(font.clone())
                                .color(l.kind.color()),
                            RichText::new(&l.text).font(font.clone()).color(ink),
                        ] {
                            part.append_to(&mut job, &style, egui::FontSelection::Default, egui::Align::Center);
                        }
                        ui.add(egui::Label::new(job).extend());
                    }
                })
            })
            .inner;
        let bottom = out.state.clamped_offset().y + out.inner_rect.height() >= out.content_size.y - row_h;
        self.live = bottom;
        self.jump_live = false;
    }
}
