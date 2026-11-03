//! The net log: each net we run or join, one TOML file in the nets folder
//! (`Config::dir()/nets`), rewritten as the net goes so a crash keeps it;
//! files received beside it in a folder of the same name. Exports as
//! text, CSV and an ICS-309 Communications Log go beside it too. Times
//! are UTC seconds since 1970.

use std::io;
use std::path::{Path, PathBuf};

use ion::link::net::{Precedence, Traffic};
use serde::{Deserialize, Serialize};

use crate::ion_engine::traffic_words;
use crate::net_doc::{self, Kind, State, SumBulletin, SumCheckIn, SumItem, Summary};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[default]
    Ncs,
    Member,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckIn {
    pub call: String,
    pub at: i64,
    pub slot: u8,
    pub line: String,
    /// `Traffic::to_byte`.
    pub traffic: u8,
    /// Empty: direct.
    pub via: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    pub at: i64,
    pub from: String,
    /// Empty: the whole net.
    pub to: String,
    #[serde(with = "net_doc::prec_letter")]
    pub precedence: Precedence,
    pub kind: Kind,
    /// For the net: net control passes it on.
    pub for_net: bool,
    pub subject: String,
    /// A message's text.
    pub text: String,
    /// A file's name, and where it was saved (empty: not saved).
    pub name: String,
    pub saved: String,
    pub size: u32,
    pub state: State,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bulletin {
    pub at: i64,
    pub from: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Missed {
    pub at: i64,
    pub call: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetRecord {
    pub net: String,
    pub ncs: String,
    /// Our call: the radio operator.
    pub operator: String,
    pub role: Role,
    pub freq_hz: Option<u32>,
    pub opened: i64,
    pub closed: Option<i64>,
    /// The announcement as Markdown.
    pub announcement: String,
    /// Member: net control's summary is merged in.
    pub summary: bool,
    pub checkins: Vec<CheckIn>,
    pub traffic: Vec<Item>,
    pub bulletins: Vec<Bulletin>,
    pub missed: Vec<Missed>,
}

pub struct NetLog {
    pub path: PathBuf,
    pub rec: NetRecord,
}

/// "2026-11-03 01:02:03Z".
pub fn utc(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn hhmm(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_default()
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// A file name made safe: no folders, nothing a file system refuses.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.() ".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() { "file".into() } else { s }
}

impl NetLog {
    /// A new log in `dir`, named by its opening time and net, saved at once.
    pub fn create(dir: &Path, rec: NetRecord) -> NetLog {
        let stamp = chrono::DateTime::from_timestamp(rec.opened, 0)
            .map(|d| d.format("%Y%m%d-%H%M%SZ").to_string())
            .unwrap_or_default();
        let net = safe_name(if rec.net.is_empty() { "NET" } else { &rec.net }).replace(' ', "_");
        let mut path = dir.join(format!("{stamp}-{net}.toml"));
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("{stamp}-{net}-{n}.toml"));
            n += 1;
        }
        let log = NetLog { path, rec };
        log.save();
        log
    }

    pub fn load(path: &Path) -> io::Result<NetLog> {
        let text = std::fs::read_to_string(path)?;
        let rec = toml::from_str(&text).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(NetLog {
            path: path.to_path_buf(),
            rec,
        })
    }

    /// Every log in `dir`, newest first.
    pub fn list(dir: &Path) -> Vec<NetLog> {
        let mut logs: Vec<NetLog> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .filter_map(|p| NetLog::load(&p).ok())
            .collect();
        logs.sort_by(|a, b| b.rec.opened.cmp(&a.rec.opened).then(b.path.cmp(&a.path)));
        logs
    }

    /// Written through a temporary file, so a crash leaves the last whole one.
    pub fn save(&self) {
        let result = (|| {
            if let Some(d) = self.path.parent() {
                std::fs::create_dir_all(d)?;
            }
            let text = toml::to_string_pretty(&self.rec).map_err(|e| io::Error::other(e.to_string()))?;
            let tmp = self.path.with_extension("toml.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            log::warn!("{}: {e}", self.path.display());
        }
    }

    /// Where files received in this net go.
    pub fn files_dir(&self) -> PathBuf {
        self.path.with_extension("")
    }

    /// Saves a received file beside the log; returns where.
    pub fn save_file(&self, name: &str, data: &[u8]) -> io::Result<PathBuf> {
        let dir = self.files_dir();
        std::fs::create_dir_all(&dir)?;
        let name = safe_name(name);
        let mut path = dir.join(&name);
        let mut n = 2;
        while path.exists() {
            let p = Path::new(&name);
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
            path = match p.extension().and_then(|e| e.to_str()) {
                Some(ext) => dir.join(format!("{stem}-{n}.{ext}")),
                None => dir.join(format!("{stem}-{n}")),
            };
            n += 1;
        }
        std::fs::write(&path, data)?;
        Ok(path)
    }

    fn checkin_mut(&mut self, call: &str) -> Option<&mut CheckIn> {
        self.rec.checkins.iter_mut().find(|c| c.call == call)
    }

    pub fn checked_in(&mut self, call: &str, slot: u8, via: Option<&str>, at: i64) {
        match self.checkin_mut(call) {
            Some(c) => {
                c.slot = slot;
                c.via = via.unwrap_or_default().to_string();
            }
            None => self.rec.checkins.push(CheckIn {
                call: call.to_string(),
                at,
                slot,
                via: via.unwrap_or_default().to_string(),
                ..CheckIn::default()
            }),
        }
        self.save();
    }

    fn with_checkin(&mut self, call: &str, at: i64, f: impl FnOnce(&mut CheckIn)) {
        if self.checkin_mut(call).is_none() {
            self.rec.checkins.push(CheckIn {
                call: call.to_string(),
                at,
                ..CheckIn::default()
            });
        }
        if let Some(c) = self.checkin_mut(call) {
            f(c);
        }
        self.save();
    }

    pub fn line(&mut self, call: &str, line: &str, at: i64) {
        self.with_checkin(call, at, |c| c.line = line.to_string());
    }

    pub fn traffic_listed(&mut self, call: &str, t: Traffic, at: i64) {
        self.with_checkin(call, at, |c| c.traffic = t.to_byte());
    }

    pub fn routed(&mut self, call: &str, via: Option<&str>, at: i64) {
        self.with_checkin(call, at, |c| c.via = via.unwrap_or_default().to_string());
    }

    pub fn missed(&mut self, call: &str, at: i64) {
        self.rec.missed.push(Missed {
            at,
            call: call.to_string(),
        });
        self.save();
    }

    /// Returns its index.
    pub fn item(&mut self, item: Item) -> usize {
        self.rec.traffic.push(item);
        self.save();
        self.rec.traffic.len() - 1
    }

    pub fn set_state(&mut self, i: usize, state: State) {
        if let Some(it) = self.rec.traffic.get_mut(i)
            && it.state != state
        {
            it.state = state;
            self.save();
        }
    }

    pub fn bulletin(&mut self, from: &str, text: &str, at: i64) {
        self.rec.bulletins.push(Bulletin {
            at,
            from: from.to_string(),
            text: text.to_string(),
        });
        self.save();
    }

    pub fn close(&mut self, at: i64) {
        if self.rec.closed.is_none() {
            self.rec.closed = Some(at);
            self.save();
        }
    }

    /// What net control sends all stations at the close.
    pub fn summary(&self, closed: i64) -> Summary {
        let r = &self.rec;
        let off = |t: i64| (t - r.opened).clamp(0, u16::MAX as i64) as u16;
        let mut missed: Vec<(String, u8)> = Vec::new();
        for m in &r.missed {
            match missed.iter_mut().find(|(c, _)| *c == m.call) {
                Some((_, n)) => *n = n.saturating_add(1),
                None => missed.push((m.call.clone(), 1)),
            }
        }
        Summary {
            net: r.net.clone(),
            ncs: r.ncs.clone(),
            opened: r.opened.max(0) as u32,
            closed: closed.max(0) as u32,
            freq_hz: r.freq_hz,
            checkins: r
                .checkins
                .iter()
                .map(|c| SumCheckIn {
                    call: c.call.clone(),
                    at: off(c.at),
                    traffic: Traffic::from_byte(c.traffic),
                    via: c.via.clone(),
                    line: c.line.clone(),
                })
                .collect(),
            items: r
                .traffic
                .iter()
                .map(|i| SumItem {
                    from: i.from.clone(),
                    to: i.to.clone(),
                    at: off(i.at),
                    precedence: i.precedence,
                    kind: i.kind,
                    state: i.state,
                    subject: i.subject.clone(),
                })
                .collect(),
            bulletins: r
                .bulletins
                .iter()
                .map(|b| SumBulletin {
                    at: off(b.at),
                    subject: net_doc::subject(&b.text),
                })
                .collect(),
            missed,
        }
    }

    /// Member: net control's summary into our copy. Its check-ins and
    /// missed polls stand; traffic and bulletins we lack are added, and
    /// ours take net control's state.
    pub fn merge(&mut self, s: &Summary) {
        let at = |o: u16| s.opened as i64 + o as i64;
        let r = &mut self.rec;
        if r.ncs.is_empty() {
            r.ncs = s.ncs.clone();
        }
        if r.net.is_empty() {
            r.net = s.net.clone();
        }
        r.freq_hz = r.freq_hz.or(s.freq_hz);
        r.closed = Some(s.closed as i64);
        r.checkins = s
            .checkins
            .iter()
            .map(|c| CheckIn {
                call: c.call.clone(),
                at: at(c.at),
                slot: r.checkins.iter().find(|o| o.call == c.call).map_or(0, |o| o.slot),
                line: c.line.clone(),
                traffic: c.traffic.to_byte(),
                via: c.via.clone(),
            })
            .collect();
        for i in &s.items {
            match r
                .traffic
                .iter_mut()
                .find(|o| o.from == i.from && o.subject == i.subject && o.kind == i.kind)
            {
                Some(o) if i.state == State::Handled => o.state = State::Handled,
                Some(_) => {}
                None => r.traffic.push(Item {
                    at: at(i.at),
                    from: i.from.clone(),
                    to: i.to.clone(),
                    precedence: i.precedence,
                    kind: i.kind,
                    subject: i.subject.clone(),
                    state: i.state,
                    ..Item::default()
                }),
            }
        }
        for b in &s.bulletins {
            if !r.bulletins.iter().any(|o| net_doc::subject(&o.text) == b.subject) {
                r.bulletins.push(Bulletin {
                    at: at(b.at),
                    from: s.ncs.clone(),
                    text: b.subject.clone(),
                });
            }
        }
        r.missed = s
            .missed
            .iter()
            .flat_map(|(c, n)| {
                (0..*n).map(|_| Missed {
                    at: s.closed as i64,
                    call: c.clone(),
                })
            })
            .collect();
        r.summary = true;
        self.save();
    }

    /// The text, CSV and ICS-309 exports, beside the log.
    pub fn export(&self) -> io::Result<Vec<PathBuf>> {
        let base = self.path.with_extension("");
        let name = base.file_name().and_then(|n| n.to_str()).unwrap_or("net").to_string();
        let out = [
            (format!("{name}.txt"), to_text(&self.rec)),
            (format!("{name}.csv"), to_csv(&self.rec)),
            (format!("{name}-ics309.txt"), to_ics309(&self.rec, now())),
        ];
        let dir = self.path.parent().unwrap_or(Path::new("."));
        out.into_iter()
            .map(|(f, text)| {
                let p = dir.join(f);
                std::fs::write(&p, text).map(|_| p)
            })
            .collect()
    }
}

fn freq(r: &NetRecord) -> String {
    r.freq_hz.map(net_doc::freq_text).unwrap_or_default()
}

fn to_or_all(to: &str) -> &str {
    if to.is_empty() { "ALL" } else { to }
}

/// A readable report.
pub fn to_text(r: &NetRecord) -> String {
    let mut s = String::new();
    let dash = |v: String| if v.is_empty() { "-".to_string() } else { v };
    s += &format!("Net        {}\n", dash(r.net.clone()));
    s += &format!("NCS        {}\n", dash(r.ncs.clone()));
    s += &format!("Frequency  {}\n", dash(freq(r)));
    s += &format!("Opened     {}\n", utc(r.opened));
    s += &format!("Closed     {}\n", r.closed.map(utc).unwrap_or_else(|| "-".into()));
    s += &format!("Operator   {}\n", dash(r.operator.clone()));
    s += &format!("\nCheck-ins ({})\n", r.checkins.len());
    for c in &r.checkins {
        let via = if c.via.is_empty() {
            String::new()
        } else {
            format!("  via {}", c.via)
        };
        s += &format!(
            "  {}  {:<8} {:<10} {}{via}\n",
            utc(c.at),
            c.call,
            traffic_words(Traffic::from_byte(c.traffic)),
            c.line
        );
    }
    s += &format!("\nTraffic ({})\n", r.traffic.len());
    for i in &r.traffic {
        let what = match i.kind {
            Kind::Message => i.subject.clone(),
            Kind::File => format!("{} ({} B)", i.name, i.size),
        };
        s += &format!(
            "  {}  {:<9} {:<8} > {:<8} {:<9} {what}\n",
            utc(i.at),
            net_doc::precedence_word(i.precedence),
            i.from,
            to_or_all(&i.to),
            i.state.word(),
        );
    }
    s += &format!("\nBulletins ({})\n", r.bulletins.len());
    for b in &r.bulletins {
        s += &format!("  {}  {}\n", utc(b.at), b.from);
        for l in b.text.lines() {
            s += &format!("    {l}\n");
        }
    }
    s += &format!("\nMissed polls ({})\n", r.missed.len());
    for m in &r.missed {
        s += &format!("  {}  {}\n", utc(m.at), m.call);
    }
    s
}

fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn csv_row(out: &mut String, fields: &[&str]) {
    let row: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
    out.push_str(&row.join(","));
    out.push_str("\r\n");
}

pub const CSV_HEAD: [&str; 9] = [
    "time_utc",
    "event",
    "from",
    "to",
    "precedence",
    "state",
    "route",
    "traffic",
    "text",
];

/// One row per check-in, item, bulletin and missed poll, in time order.
pub fn to_csv(r: &NetRecord) -> String {
    let mut rows: Vec<(i64, Vec<String>)> = Vec::new();
    for c in &r.checkins {
        rows.push((
            c.at,
            vec![
                "checkin".into(),
                c.call.clone(),
                r.ncs.clone(),
                String::new(),
                String::new(),
                c.via.clone(),
                traffic_words(Traffic::from_byte(c.traffic)),
                c.line.clone(),
            ],
        ));
    }
    for i in &r.traffic {
        let text = match i.kind {
            Kind::Message => i.text.clone(),
            Kind::File => format!("{} ({} B)", i.name, i.size),
        };
        rows.push((
            i.at,
            vec![
                match i.kind {
                    Kind::Message => "message".into(),
                    Kind::File => "file".into(),
                },
                i.from.clone(),
                to_or_all(&i.to).into(),
                net_doc::precedence_word(i.precedence).into(),
                i.state.word().into(),
                String::new(),
                String::new(),
                text,
            ],
        ));
    }
    for b in &r.bulletins {
        rows.push((
            b.at,
            vec![
                "bulletin".into(),
                b.from.clone(),
                "ALL".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                b.text.clone(),
            ],
        ));
    }
    for m in &r.missed {
        rows.push((
            m.at,
            vec![
                "missed".into(),
                r.ncs.clone(),
                m.call.clone(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ],
        ));
    }
    rows.sort_by_key(|r| r.0);
    let mut out = String::new();
    csv_row(&mut out, &CSV_HEAD);
    for (t, f) in rows {
        let mut all = vec![utc(t)];
        all.extend(f);
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        csv_row(&mut out, &refs);
    }
    out
}

/// Width of the ICS-309 columns: time, from, to, subject.
const W309: [usize; 4] = [6, 10, 10, 50];

/// The ICS-309 Communications Log as a printable text form: incident
/// (the net), operational period, radio operator, then time, from, to and
/// subject for each check-in, item of traffic and bulletin, and who
/// prepared it, when.
pub fn to_ics309(r: &NetRecord, prepared: i64) -> String {
    let mut s = String::new();
    let rule = "-".repeat(W309.iter().sum::<usize>() + 3 * 3);
    s += "COMMUNICATIONS LOG (ICS 309)\n";
    s += &format!("1. Incident Name: {}\n", r.net);
    s += &format!(
        "2. Operational Period: From {} To {}\n",
        utc(r.opened),
        r.closed.map(utc).unwrap_or_default()
    );
    let station = match freq(r) {
        f if f.is_empty() => r.operator.clone(),
        f => format!("{} on {f}", r.operator),
    };
    s += &format!("3. Radio Operator (Name, Station ID): {station}\n");
    s += "4. Log\n";
    s += &format!("{rule}\n");
    let row = |a: &str, b: &str, c: &str, d: &str| {
        format!(
            "{:<w0$} | {:<w1$} | {:<w2$} | {}\n",
            a,
            b,
            c,
            d,
            w0 = W309[0],
            w1 = W309[1],
            w2 = W309[2]
        )
    };
    s += &row("Time", "From", "To", "Subject");
    s += &format!("{rule}\n");
    let mut rows: Vec<(i64, String, String, String)> = Vec::new();
    rows.push((r.opened, r.ncs.clone(), "ALL".into(), format!("Net {} opened", r.net)));
    for c in &r.checkins {
        let mut subj = "Check-in".to_string();
        if !c.line.is_empty() {
            subj += &format!(": {}", c.line);
        }
        let t = Traffic::from_byte(c.traffic);
        if t.items > 0 {
            subj += &format!(" (traffic {})", traffic_words(t));
        }
        if !c.via.is_empty() {
            subj += &format!(" via {}", c.via);
        }
        rows.push((c.at, c.call.clone(), r.ncs.clone(), subj));
    }
    for i in &r.traffic {
        let what = match i.kind {
            Kind::Message => i.subject.clone(),
            Kind::File => format!("File {}", i.name),
        };
        let subj = match i.precedence {
            Precedence::Routine => what,
            p => format!("{}: {what}", net_doc::precedence_word(p)),
        };
        rows.push((i.at, i.from.clone(), to_or_all(&i.to).into(), subj));
    }
    for b in &r.bulletins {
        rows.push((
            b.at,
            b.from.clone(),
            "ALL".into(),
            format!("Bulletin: {}", net_doc::subject(&b.text)),
        ));
    }
    if let Some(c) = r.closed {
        rows.push((c, r.ncs.clone(), "ALL".into(), format!("Net {} closed", r.net)));
    }
    rows.sort_by_key(|r| r.0);
    for (t, from, to, subj) in rows {
        s += &row(&hhmm(t), &from, &to, net_doc::clip(&subj, 120));
    }
    s += &format!("{rule}\n");
    s += &format!("5. Prepared By (Name, Station ID): {}\n", r.operator);
    s += &format!("6. Date & Time Prepared: {}\n", utc(prepared));
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh folder in the system's temporary one, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(name: &str) -> TempDir {
            let d = std::env::temp_dir().join(format!(
                "ionhf-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const T0: i64 = 1_793_667_600; // 2026-11-03 01:00:00Z

    fn sample(dir: &Path) -> NetLog {
        let mut log = NetLog::create(
            dir,
            NetRecord {
                net: "TNARES".into(),
                ncs: "N0JLF".into(),
                operator: "N0JLF".into(),
                freq_hz: Some(3_975_000),
                opened: T0,
                ..NetRecord::default()
            },
        );
        log.checked_in("KJ5ET", 1, None, T0 + 40);
        log.line("KJ5ET", "Trousdale County ARES, no traffic", T0 + 41);
        log.checked_in("AG5B", 2, Some("KJ5ET"), T0 + 95);
        log.traffic_listed(
            "AG5B",
            Traffic {
                items: 1,
                precedence: Precedence::Priority,
                short: false,
            },
            T0 + 95,
        );
        log.routed("AG5B", Some("KJ5ET"), T0 + 96);
        let i = log.item(Item {
            at: T0 + 300,
            from: "AG5B".into(),
            to: "N0JLF".into(),
            precedence: Precedence::Priority,
            subject: "Shelter open, 40 cots".into(),
            text: "Shelter open, 40 cots\nNeed water, \"bottled\"".into(),
            state: State::Received,
            ..Item::default()
        });
        log.set_state(i, State::Handled);
        log.bulletin("N0JLF", "Net closes at 2130\nde N0JLF", T0 + 120);
        log.missed("W5XYZ", T0 + 200);
        log.close(T0 + 3600);
        log
    }

    #[test]
    fn a_log_is_saved_as_it_goes_and_listed() {
        let d = TempDir::new("netlog");
        let log = sample(&d.0);
        assert!(log.path.starts_with(&d.0));
        assert_eq!(
            log.path.file_name().unwrap().to_str().unwrap(),
            "20261103-010000Z-TNARES.toml"
        );
        let back = NetLog::load(&log.path).unwrap();
        assert_eq!(back.rec, log.rec);
        assert_eq!(back.rec.checkins[1].via, "KJ5ET");
        assert_eq!(back.rec.traffic[0].state, State::Handled);
        assert_eq!(back.rec.traffic[0].precedence, Precedence::Priority);
        let text = std::fs::read_to_string(&log.path).unwrap();
        assert!(text.contains("precedence = \"P\""), "{text}");
        // A second net the same second gets its own file; the newest lists first.
        let later = NetLog::create(
            &d.0,
            NetRecord {
                net: "TNARES".into(),
                opened: T0 + 86_400,
                ..NetRecord::default()
            },
        );
        let again = NetLog::create(
            &d.0,
            NetRecord {
                net: "TNARES".into(),
                opened: T0,
                ..NetRecord::default()
            },
        );
        assert_ne!(again.path, log.path);
        let list = NetLog::list(&d.0);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].path, later.path);
        let f = log.save_file("../../evil:name.txt", b"x").unwrap();
        assert!(f.starts_with(log.files_dir()), "{f:?}");
        assert_eq!(f.file_name().unwrap(), "evil_name.txt");
        let f2 = log.save_file("evil:name.txt", b"y").unwrap();
        assert_eq!(f2.file_name().unwrap(), "evil_name-2.txt");
    }

    #[test]
    fn exports_text_csv_and_ics309() {
        let d = TempDir::new("netexport");
        let log = sample(&d.0);
        let files = log.export().unwrap();
        assert_eq!(files.len(), 3);
        for f in &files {
            assert_eq!(f.parent(), log.path.parent());
            assert!(f.is_file());
        }
        let text = to_text(&log.rec);
        assert!(text.contains("Frequency  3.975 MHz"), "{text}");
        assert!(text.contains("KJ5ET"), "{text}");
        assert!(text.contains("Priority  AG5B     > N0JLF    Handled"), "{text}");

        let csv = to_csv(&log.rec);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], CSV_HEAD.join(","));
        // The head, 2 check-ins, an item, a bulletin and a missed poll; the
        // item's and the bulletin's text hold a line break each.
        assert_eq!(lines.len(), 1 + 2 + 1 + 1 + 1 + 2);
        assert!(
            lines[1].starts_with("2026-11-03 01:00:40Z,checkin,KJ5ET,N0JLF,,,,0 R,"),
            "{csv}"
        );
        assert!(
            csv.contains(
                "message,AG5B,N0JLF,Priority,Handled,,,\"Shelter open, 40 cots\nNeed water, \"\"bottled\"\"\""
            ),
            "{csv}"
        );
        assert!(csv.contains("missed,N0JLF,W5XYZ"), "{csv}");

        let ics = to_ics309(&log.rec, T0 + 4000);
        let l: Vec<&str> = ics.lines().collect();
        assert_eq!(l[0], "COMMUNICATIONS LOG (ICS 309)");
        assert_eq!(l[1], "1. Incident Name: TNARES");
        assert_eq!(
            l[2],
            "2. Operational Period: From 2026-11-03 01:00:00Z To 2026-11-03 02:00:00Z"
        );
        assert_eq!(l[3], "3. Radio Operator (Name, Station ID): N0JLF on 3.975 MHz");
        let head = l.iter().position(|x| x.starts_with("Time ")).unwrap();
        let cols: Vec<&str> = l[head].split('|').map(str::trim).collect();
        assert_eq!(cols, ["Time", "From", "To", "Subject"]);
        let rows: Vec<Vec<&str>> = l[head + 2..]
            .iter()
            .take_while(|x| !x.starts_with("---"))
            .map(|x| x.split(" | ").map(str::trim).collect())
            .collect();
        assert_eq!(rows[0], ["01:00", "N0JLF", "ALL", "Net TNARES opened"]);
        assert_eq!(
            rows[1],
            ["01:00", "KJ5ET", "N0JLF", "Check-in: Trousdale County ARES, no traffic"]
        );
        assert_eq!(rows[2], ["01:01", "AG5B", "N0JLF", "Check-in (traffic 1 P) via KJ5ET"]);
        assert_eq!(rows[3], ["01:02", "N0JLF", "ALL", "Bulletin: Net closes at 2130"]);
        assert_eq!(rows[4], ["01:05", "AG5B", "N0JLF", "Priority: Shelter open, 40 cots"]);
        assert_eq!(rows[5], ["02:00", "N0JLF", "ALL", "Net TNARES closed"]);
        assert_eq!(rows.len(), 6);
        assert!(ics.contains("5. Prepared By (Name, Station ID): N0JLF"));
        assert!(ics.contains("6. Date & Time Prepared: 2026-11-03 02:06:40Z"));
    }

    #[test]
    fn a_member_merges_the_summary() {
        let d = TempDir::new("netmerge");
        let ncs = sample(&d.0);
        let s = ncs.summary(T0 + 3600);
        let bytes = net_doc::Record::Summary(s.clone()).encode();
        let net_doc::Record::Summary(s) = net_doc::Record::decode(&bytes) else {
            panic!()
        };
        let mut member = NetLog::create(
            &d.0,
            NetRecord {
                net: "TNARES".into(),
                operator: "AG5B".into(),
                role: Role::Member,
                opened: T0 + 90,
                ..NetRecord::default()
            },
        );
        member.item(Item {
            at: T0 + 290,
            from: "AG5B".into(),
            to: "N0JLF".into(),
            precedence: Precedence::Priority,
            subject: "Shelter open, 40 cots".into(),
            state: State::Delivered,
            ..Item::default()
        });
        member.merge(&s);
        let r = &NetLog::load(&member.path).unwrap().rec;
        assert!(r.summary);
        assert_eq!(r.ncs, "N0JLF");
        assert_eq!(r.closed, Some(T0 + 3600));
        assert_eq!(r.checkins.len(), 2);
        assert_eq!(r.checkins[0].line, "Trousdale County ARES, no traffic");
        assert_eq!(r.checkins[1].at, T0 + 95);
        assert_eq!(r.traffic.len(), 1, "ours is matched, not doubled");
        assert_eq!(r.traffic[0].state, State::Handled);
        assert_eq!(r.bulletins[0].text, "Net closes at 2130");
        assert_eq!(r.missed.len(), 1);
        assert_eq!(r.freq_hz, Some(3_975_000));
    }
}
