//! What the Net section sends in the net's messages and objects: records
//! with a magic and a type byte, so bytes from other programs (Ragtime,
//! a host client) still show as text.
//!
//! A record is `[0xFF 'I'][type][body]`. 0xFF never occurs in UTF-8.
//!
//! | Type | Body |
//! |---|---|
//! | 1 message | `[flags]` then `compress::pack(text)` |
//! | 2 file | `[flags][name length][name]` then `compress::pack(bytes)` |
//! | 3 announcement | `[fields u16]`, the fields present in bit order, then `compress::pack(body)` |
//! | 4 summary | `compress::pack(summary)` |
//!
//! - **Flags:** bits 0–1 precedence (Routine, Welfare, Priority,
//!   Emergency), bit 2 for the net (net control passes it on) rather than
//!   for net control.
//! - **Announcement fields** (bit: field): 0 net name, 1 net control,
//!   2 alternate net control, each a packed call (5 bytes,
//!   `ion::link::net::pack_call`) or, with bit 11, 12 or 13 set, text
//!   (`[length][UTF-8]`) when it does not pack; 3 frequency and 4 backup,
//!   Hz (u32); 5 start, 6 end and 7 next net, UTC minutes since
//!   1970-01-01 (u32); 8 mode and 9 purpose (a byte each); 10 the Markdown
//!   body (the rest). Integers are little-endian.
//! - **Summary:** `[net length][net][net control, packed][opened u32][closed u32][frequency u32]`
//!   (UTC seconds since 1970; frequency 0: none), then
//!   - check-ins: `[n]` × `[call][at u16][traffic][via call][line length][line]`;
//!   - traffic: `[n u16]` × `[from][to][at u16][prec | kind << 2 | state << 3][subject length][subject]`;
//!   - bulletins: `[n]` × `[at u16][subject length][subject]`;
//!   - missed polls: `[n]` × `[call][count]`.
//!
//!   Calls are packed (all zero: none, or the whole net); `at` is seconds
//!   from the opening (at most 65535); traffic is the check-in byte
//!   (`Traffic::to_byte`).
//!
//! `compress::pack` keeps text as it is when coding does not shorten it,
//! at the cost of one byte.

use ion::link::compress;
use ion::link::net::{Precedence, Traffic, pack_call, unpack_call};
use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 2] = [0xFF, b'I'];
const T_MESSAGE: u8 = 1;
const T_FILE: u8 = 2;
const T_ANNOUNCE: u8 = 3;
const T_SUMMARY: u8 = 4;

/// Longest file a member may send: its uplink is slow.
pub const MAX_FILE: usize = 32 * 1024;
/// Longest subject kept in a summary, bytes.
pub const MAX_SUBJECT: usize = 60;

/// Whom a member's traffic is for. On the air it always goes to net
/// control; `Net` asks net control to pass it on to all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum To {
    #[default]
    Ncs,
    Net,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Message,
    File,
}

/// Where an item of traffic stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    /// Ours, not yet held by its addressee.
    #[default]
    Queued,
    /// Ours, held.
    Delivered,
    /// Theirs, here.
    Received,
    /// Net control took it up.
    Handled,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Queued => "Queued",
            State::Delivered => "Delivered",
            State::Received => "Received",
            State::Handled => "Handled",
        }
    }
}

/// The operating mode a net runs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Ion,
    Usb,
    Lsb,
    Fm,
    Cw,
    Am,
    Mixed,
}

impl Mode {
    pub const ALL: [Mode; 7] = [Mode::Ion, Mode::Usb, Mode::Lsb, Mode::Fm, Mode::Cw, Mode::Am, Mode::Mixed];

    pub fn name(self) -> &'static str {
        match self {
            Mode::Ion => "Ion",
            Mode::Usb => "USB",
            Mode::Lsb => "LSB",
            Mode::Fm => "FM",
            Mode::Cw => "CW",
            Mode::Am => "AM",
            Mode::Mixed => "Mixed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Purpose {
    #[default]
    Regular,
    Training,
    Exercise,
    Emergency,
    Weather,
    Traffic,
    Test,
}

impl Purpose {
    pub const ALL: [Purpose; 7] = [
        Purpose::Regular,
        Purpose::Training,
        Purpose::Exercise,
        Purpose::Emergency,
        Purpose::Weather,
        Purpose::Traffic,
        Purpose::Test,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Purpose::Regular => "Regular",
            Purpose::Training => "Training",
            Purpose::Exercise => "Exercise",
            Purpose::Emergency => "Emergency",
            Purpose::Weather => "Weather",
            Purpose::Traffic => "Traffic",
            Purpose::Test => "Test",
        }
    }
}

pub const PRECEDENCES: [Precedence; 4] = [
    Precedence::Routine,
    Precedence::Welfare,
    Precedence::Priority,
    Precedence::Emergency,
];

pub fn precedence_word(p: Precedence) -> &'static str {
    match p {
        Precedence::Routine => "Routine",
        Precedence::Welfare => "Welfare",
        Precedence::Priority => "Priority",
        Precedence::Emergency => "Emergency",
    }
}

fn precedence_of(code: u8) -> Precedence {
    PRECEDENCES[(code & 3) as usize]
}

/// A precedence as its letter (R, W, P, E) in the log.
pub mod prec_letter {
    use super::*;

    pub fn serialize<S: serde::Serializer>(p: &Precedence, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::ion_engine::precedence_letter(*p).to_string())
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Precedence, D::Error> {
        let s = String::deserialize(d)?;
        Ok(crate::ion_engine::precedence_of(&s).unwrap_or_default())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    pub precedence: Precedence,
    pub to: To,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct File {
    pub precedence: Precedence,
    pub to: To,
    pub name: String,
    pub data: Vec<u8>,
}

/// Net control's announcement: the predefined fields, then Markdown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Announcement {
    pub net: String,
    pub ncs: String,
    pub alt: String,
    pub freq_hz: Option<u32>,
    pub backup_hz: Option<u32>,
    /// UTC minutes since 1970.
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub next: Option<u32>,
    pub mode: Option<Mode>,
    pub purpose: Option<Purpose>,
    pub body: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SumCheckIn {
    pub call: String,
    /// Seconds from the opening.
    pub at: u16,
    pub traffic: Traffic,
    pub via: String,
    pub line: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SumItem {
    pub from: String,
    /// Empty: the whole net.
    pub to: String,
    pub at: u16,
    pub precedence: Precedence,
    pub kind: Kind,
    pub state: State,
    pub subject: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SumBulletin {
    pub at: u16,
    pub subject: String,
}

/// What net control sends all stations at the close.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub net: String,
    pub ncs: String,
    /// UTC seconds since 1970.
    pub opened: u32,
    pub closed: u32,
    pub freq_hz: Option<u32>,
    pub checkins: Vec<SumCheckIn>,
    pub items: Vec<SumItem>,
    pub bulletins: Vec<SumBulletin>,
    pub missed: Vec<(String, u8)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    Message(Message),
    File(File),
    Announcement(Announcement),
    Summary(Summary),
    /// Not one of ours: shown as text.
    Text(String),
}

/// `s` cut to at most `max` bytes, on a character boundary.
pub fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// The first line of `text`, cut to [`MAX_SUBJECT`] bytes.
pub fn subject(text: &str) -> String {
    clip(text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim(), MAX_SUBJECT).to_string()
}

fn flags(p: Precedence, to: To) -> u8 {
    p as u8 | ((to == To::Net) as u8) << 2
}

fn short_text(out: &mut Vec<u8>, s: &str) {
    let s = clip(s, 255);
    out.push(s.len() as u8);
    out.extend_from_slice(s.as_bytes());
}

/// A call as 5 packed bytes; `None` if it does not pack as it is.
fn packs(call: &str) -> Option<[u8; 5]> {
    let b = pack_call(call);
    (unpack_call(&b) == call.trim().to_ascii_uppercase() && call.len() <= ion::link::net::MAX_CALL).then_some(b)
}

fn call5(out: &mut Vec<u8>, call: &str) {
    out.extend_from_slice(&pack_call(call));
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Record::Message(m) => {
                out.push(T_MESSAGE);
                out.push(flags(m.precedence, m.to));
                out.extend(compress::pack(m.text.as_bytes()));
            }
            Record::File(f) => {
                out.push(T_FILE);
                out.push(flags(f.precedence, f.to));
                short_text(&mut out, &f.name);
                out.extend(compress::pack(&f.data));
            }
            Record::Announcement(a) => {
                out.push(T_ANNOUNCE);
                encode_announcement(a, &mut out);
            }
            Record::Summary(s) => {
                out.push(T_SUMMARY);
                out.extend(compress::pack(&encode_summary(s)));
            }
            Record::Text(t) => return t.as_bytes().to_vec(),
        }
        out
    }

    /// Anything that is not a whole record of ours is text.
    pub fn decode(b: &[u8]) -> Record {
        let text = || Record::Text(String::from_utf8_lossy(b).into_owned());
        let Some(rest) = b.strip_prefix(&MAGIC[..]) else {
            return text();
        };
        let Some((&t, body)) = rest.split_first() else {
            return text();
        };
        let r = match t {
            T_MESSAGE => decode_message(body),
            T_FILE => decode_file(body),
            T_ANNOUNCE => decode_announcement(body).map(Record::Announcement),
            T_SUMMARY => compress::unpack(body)
                .and_then(|p| decode_summary(&p))
                .map(Record::Summary),
            _ => None,
        };
        r.unwrap_or_else(text)
    }
}

fn decode_message(b: &[u8]) -> Option<Record> {
    let (&f, rest) = b.split_first()?;
    let text = String::from_utf8(compress::unpack(rest)?).ok()?;
    Some(Record::Message(Message {
        precedence: precedence_of(f),
        to: if f & 4 != 0 { To::Net } else { To::Ncs },
        text,
    }))
}

fn decode_file(b: &[u8]) -> Option<Record> {
    let mut r = Reader(b);
    let f = r.u8()?;
    let name = r.short_text()?;
    let data = compress::unpack(r.rest())?;
    Some(Record::File(File {
        precedence: precedence_of(f),
        to: if f & 4 != 0 { To::Net } else { To::Ncs },
        name,
        data,
    }))
}

// Bits 0–2: net name, net control, alternate; from A_FREQ on: frequency,
// backup, start, end, next.
const A_FREQ: u16 = 1 << 3;
const A_MODE: u16 = 1 << 8;
const A_PURPOSE: u16 = 1 << 9;
const A_BODY: u16 = 1 << 10;
/// Net name, net control, alternate as text, not packed.
const A_TEXT: u16 = 1 << 11;

fn encode_announcement(a: &Announcement, out: &mut Vec<u8>) {
    let at = out.len();
    out.extend([0, 0]);
    let mut fields = 0u16;
    for (i, s) in [&a.net, &a.ncs, &a.alt].into_iter().enumerate() {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        fields |= 1 << i;
        match packs(s) {
            Some(b) => out.extend_from_slice(&b),
            None => {
                fields |= A_TEXT << i;
                short_text(out, s);
            }
        }
    }
    for (i, v) in [a.freq_hz, a.backup_hz, a.start, a.end, a.next].into_iter().enumerate() {
        if let Some(v) = v {
            fields |= A_FREQ << i;
            out.extend(v.to_le_bytes());
        }
    }
    if let Some(m) = a.mode {
        fields |= A_MODE;
        out.push(m as u8);
    }
    if let Some(p) = a.purpose {
        fields |= A_PURPOSE;
        out.push(p as u8);
    }
    if !a.body.trim().is_empty() {
        fields |= A_BODY;
        out.extend(compress::pack(a.body.as_bytes()));
    }
    out[at..at + 2].copy_from_slice(&fields.to_le_bytes());
}

fn decode_announcement(b: &[u8]) -> Option<Announcement> {
    let mut r = Reader(b);
    let fields = r.u16()?;
    let mut a = Announcement::default();
    for (i, s) in [&mut a.net, &mut a.ncs, &mut a.alt].into_iter().enumerate() {
        if fields & 1 << i == 0 {
            continue;
        }
        *s = if fields & A_TEXT << i != 0 {
            r.short_text()?
        } else {
            unpack_call(r.take(5)?)
        };
    }
    for (i, v) in [
        &mut a.freq_hz,
        &mut a.backup_hz,
        &mut a.start,
        &mut a.end,
        &mut a.next,
    ]
    .into_iter()
    .enumerate()
    {
        if fields & A_FREQ << i != 0 {
            *v = Some(r.u32()?);
        }
    }
    if fields & A_MODE != 0 {
        a.mode = Some(*Mode::ALL.get(r.u8()? as usize)?);
    }
    if fields & A_PURPOSE != 0 {
        a.purpose = Some(*Purpose::ALL.get(r.u8()? as usize)?);
    }
    if fields & A_BODY != 0 {
        a.body = String::from_utf8(compress::unpack(r.rest())?).ok()?;
    }
    Some(a)
}

fn encode_summary(s: &Summary) -> Vec<u8> {
    let mut out = Vec::new();
    short_text(&mut out, &s.net);
    call5(&mut out, &s.ncs);
    out.extend(s.opened.to_le_bytes());
    out.extend(s.closed.to_le_bytes());
    out.extend(s.freq_hz.unwrap_or(0).to_le_bytes());
    let checkins = &s.checkins[..s.checkins.len().min(255)];
    out.push(checkins.len() as u8);
    for c in checkins {
        call5(&mut out, &c.call);
        out.extend(c.at.to_le_bytes());
        out.push(c.traffic.to_byte());
        call5(&mut out, &c.via);
        short_text(&mut out, &c.line);
    }
    let items = &s.items[..s.items.len().min(u16::MAX as usize)];
    out.extend((items.len() as u16).to_le_bytes());
    for i in items {
        call5(&mut out, &i.from);
        call5(&mut out, &i.to);
        out.extend(i.at.to_le_bytes());
        out.push(i.precedence as u8 | (i.kind as u8) << 2 | (i.state as u8) << 3);
        short_text(&mut out, clip(&i.subject, MAX_SUBJECT));
    }
    let bulletins = &s.bulletins[..s.bulletins.len().min(255)];
    out.push(bulletins.len() as u8);
    for b in bulletins {
        out.extend(b.at.to_le_bytes());
        short_text(&mut out, clip(&b.subject, MAX_SUBJECT));
    }
    let missed = &s.missed[..s.missed.len().min(255)];
    out.push(missed.len() as u8);
    for (call, n) in missed {
        call5(&mut out, call);
        out.push(*n);
    }
    out
}

fn decode_summary(b: &[u8]) -> Option<Summary> {
    let mut r = Reader(b);
    let mut s = Summary {
        net: r.short_text()?,
        ncs: r.call()?,
        opened: r.u32()?,
        closed: r.u32()?,
        freq_hz: Some(r.u32()?).filter(|&f| f != 0),
        ..Summary::default()
    };
    for _ in 0..r.u8()? {
        s.checkins.push(SumCheckIn {
            call: r.call()?,
            at: r.u16()?,
            traffic: Traffic::from_byte(r.u8()?),
            via: r.call()?,
            line: r.short_text()?,
        });
    }
    for _ in 0..r.u16()? {
        let from = r.call()?;
        let to = r.call()?;
        let at = r.u16()?;
        let f = r.u8()?;
        s.items.push(SumItem {
            from,
            to,
            at,
            precedence: precedence_of(f),
            kind: if f & 4 != 0 { Kind::File } else { Kind::Message },
            state: [State::Queued, State::Delivered, State::Received, State::Handled][(f >> 3 & 3) as usize],
            subject: r.short_text()?,
        });
    }
    for _ in 0..r.u8()? {
        s.bulletins.push(SumBulletin {
            at: r.u16()?,
            subject: r.short_text()?,
        });
    }
    for _ in 0..r.u8()? {
        s.missed.push((r.call()?, r.u8()?));
    }
    Some(s)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn call(&mut self) -> Option<String> {
        Some(unpack_call(self.take(5)?))
    }

    fn short_text(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

// -- shown --------------------------------------------------------------------------

/// "3.975 MHz".
pub fn freq_text(hz: u32) -> String {
    let mhz = format!("{:.6}", hz as f64 / 1e6);
    let mhz = mhz.trim_end_matches('0');
    let mhz = if mhz.ends_with('.') {
        format!("{mhz}0")
    } else {
        mhz.to_string()
    };
    format!("{mhz} MHz")
}

/// A frequency as typed: MHz below 1000 ("3.975"), kHz below a million
/// ("3975"), else Hz; or with its unit ("7071.5 kHz").
pub fn parse_freq(s: &str) -> Option<u32> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let (num, scale) = if let Some(n) = t.strip_suffix("mhz") {
        (n, 1e6)
    } else if let Some(n) = t.strip_suffix("khz") {
        (n, 1e3)
    } else if let Some(n) = t.strip_suffix("hz") {
        (n, 1.0)
    } else {
        let v: f64 = t.parse().ok()?;
        let scale = if v < 1000.0 {
            1e6
        } else if v < 1e6 {
            1e3
        } else {
            1.0
        };
        (t.as_str(), scale)
    };
    let hz = num.trim().parse::<f64>().ok()? * scale;
    (hz > 0.0 && hz < u32::MAX as f64).then(|| hz.round() as u32)
}

/// "2026-11-03 01:00Z".
pub fn minutes_text(m: u32) -> String {
    chrono::DateTime::from_timestamp(m as i64 * 60, 0)
        .map(|t| t.format("%Y-%m-%d %H:%MZ").to_string())
        .unwrap_or_default()
}

/// "2026-11-03 01:00" (UTC, a trailing Z allowed) as UTC minutes.
pub fn parse_minutes(s: &str) -> Option<u32> {
    let t = s.trim().trim_end_matches(['Z', 'z']).trim();
    let dt = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M").ok()?;
    u32::try_from(dt.and_utc().timestamp() / 60).ok()
}

/// "01:00" (UTC) on the day of `day` (UTC seconds), as UTC minutes.
pub fn minutes_on(day: i64, hhmm: &str) -> Option<u32> {
    let t = chrono::NaiveTime::parse_from_str(hhmm.trim().trim_end_matches(['Z', 'z']), "%H:%M").ok()?;
    let d = chrono::DateTime::from_timestamp(day, 0)?.date_naive();
    u32::try_from(d.and_time(t).and_utc().timestamp() / 60).ok()
}

impl Announcement {
    /// The same as Markdown, as a program without these records would send
    /// it.
    pub fn plain_markdown(&self) -> String {
        let mut s = format!("# {}\n\n", self.net);
        let mut row = |k: &str, v: String| {
            if !v.is_empty() {
                s.push_str(&format!("- **{k}:** {v}\n"));
            }
        };
        row("Net control", self.ncs.clone());
        row("Alternate", self.alt.clone());
        row("Frequency", self.freq_hz.map(freq_text).unwrap_or_default());
        row("Backup", self.backup_hz.map(freq_text).unwrap_or_default());
        row("Start", self.start.map(minutes_text).unwrap_or_default());
        row("End", self.end.map(minutes_text).unwrap_or_default());
        row("Next", self.next.map(minutes_text).unwrap_or_default());
        row("Mode", self.mode.map(|m| m.name().to_string()).unwrap_or_default());
        row("Purpose", self.purpose.map(|p| p.name().to_string()).unwrap_or_default());
        s.push('\n');
        s.push_str(&self.body);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Announcement {
        Announcement {
            net: "TNARES".into(),
            ncs: "N0JLF".into(),
            alt: "KJ5ET".into(),
            freq_hz: Some(3_975_000),
            backup_hz: Some(7_262_000),
            start: parse_minutes("2026-11-03 01:00"),
            end: parse_minutes("2026-11-03 02:00"),
            next: parse_minutes("2026-11-10 01:00"),
            mode: Some(Mode::Ion),
            purpose: Some(Purpose::Training),
            body: "## Tonight\n\n\
                   Welcome to the Trousdale County ARES training net. This is a directed net: \
                   wait for net control to call you.\n\n\
                   1. Check in with your call, your county and any traffic.\n\
                   2. Priority and Emergency traffic is taken first.\n\
                   3. Bulletins follow the check-ins.\n\n\
                   ## Agenda\n\n\
                   | Item | Who |\n|---|---|\n| Weather briefing | KJ5ET |\n\
                   | Shelter drill | AG5B |\n| Generator report | all stations |\n\n\
                   *Next week:* an exercise with the county EOC. Bring your go-kit list.\n\n\
                   73 de N0JLF"
                .into(),
        }
    }

    #[test]
    fn plain_bytes_are_text() {
        assert_eq!(Record::decode(b"KJ5ET: all well"), Record::Text("KJ5ET: all well".into()));
        assert_eq!(Record::decode(b""), Record::Text(String::new()));
        // Our magic with a type we do not know, or cut short.
        assert!(matches!(Record::decode(&[0xFF, b'I', 99, 1]), Record::Text(_)));
        assert!(matches!(Record::decode(&[0xFF, b'I', T_ANNOUNCE, 0xFF]), Record::Text(_)));
        assert!(matches!(Record::decode(&[0xFF, b'I']), Record::Text(_)));
    }

    #[test]
    fn a_message_round_trips() {
        for (p, to) in [
            (Precedence::Routine, To::Ncs),
            (Precedence::Priority, To::Net),
            (Precedence::Emergency, To::Ncs),
        ] {
            let m = Record::Message(Message {
                precedence: p,
                to,
                text: "AG5B: generator test complete, shelter open at 1900".into(),
            });
            let b = m.encode();
            assert_eq!(&b[..3], &[0xFF, b'I', T_MESSAGE]);
            assert_eq!(Record::decode(&b), m);
        }
        let empty = Record::Message(Message::default());
        assert_eq!(Record::decode(&empty.encode()), empty);
    }

    #[test]
    fn a_file_round_trips() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let f = Record::File(File {
            precedence: Precedence::Welfare,
            to: To::Net,
            name: "ics213.txt".into(),
            data,
        });
        assert_eq!(Record::decode(&f.encode()), f);
    }

    #[test]
    fn an_announcement_round_trips_and_is_compact() {
        let a = sample();
        let rec = Record::Announcement(a.clone());
        let b = rec.encode();
        assert_eq!(Record::decode(&b), rec);
        let plain = a.plain_markdown();
        let packed = compress::pack(plain.as_bytes());
        let body = a.body.len();
        let body_packed = compress::pack(a.body.as_bytes()).len();
        println!(
            "announcement: record {} B (fields {} B, body {body} B packed to {body_packed}); \
             plain Markdown {} B, packed {} B",
            b.len(),
            b.len() - body_packed,
            plain.len(),
            packed.len()
        );
        assert!(b.len() < plain.len());
        assert!(b.len() <= packed.len() + 8, "{} against {}", b.len(), packed.len());
        assert!(body_packed < body, "compression helps the body");
    }

    #[test]
    fn announcement_fields_that_do_not_pack_go_as_text() {
        let a = Announcement {
            net: "Trousdale County ARES".into(),
            ncs: "N0JLF".into(),
            alt: "VE3/N0JLF".into(),
            ..Announcement::default()
        };
        let r = Record::Announcement(a);
        assert_eq!(Record::decode(&r.encode()), r);
        let empty = Record::Announcement(Announcement::default());
        assert_eq!(empty.encode().len(), 5);
        assert_eq!(Record::decode(&empty.encode()), empty);
    }

    #[test]
    fn a_summary_round_trips() {
        let s = Summary {
            net: "TNARES".into(),
            ncs: "N0JLF".into(),
            opened: 1_793_000_000,
            closed: 1_793_003_600,
            freq_hz: Some(3_975_000),
            checkins: vec![
                SumCheckIn {
                    call: "KJ5ET".into(),
                    at: 40,
                    traffic: Traffic::default(),
                    via: String::new(),
                    line: "Trousdale County ARES, no traffic".into(),
                },
                SumCheckIn {
                    call: "AG5B".into(),
                    at: 95,
                    traffic: Traffic {
                        items: 1,
                        precedence: Precedence::Priority,
                        short: true,
                    },
                    via: "KJ5ET".into(),
                    line: String::new(),
                },
            ],
            items: vec![SumItem {
                from: "AG5B".into(),
                to: "N0JLF".into(),
                at: 300,
                precedence: Precedence::Priority,
                kind: Kind::File,
                state: State::Handled,
                subject: "Shelter roster".into(),
            }],
            bulletins: vec![SumBulletin {
                at: 120,
                subject: "Net closes at 2130".into(),
            }],
            missed: vec![("W5XYZ".into(), 2)],
        };
        let r = Record::Summary(s);
        let b = r.encode();
        println!("summary of 2 check-ins, 1 item, 1 bulletin: {} B", b.len());
        assert_eq!(Record::decode(&b), r);
    }

    #[test]
    fn frequencies_and_times_parse() {
        assert_eq!(parse_freq("3.975"), Some(3_975_000));
        assert_eq!(parse_freq("7071.5"), Some(7_071_500));
        assert_eq!(parse_freq("14.105 MHz"), Some(14_105_000));
        assert_eq!(parse_freq("7071.5 kHz"), Some(7_071_500));
        assert_eq!(parse_freq("x"), None);
        assert_eq!(freq_text(3_975_000), "3.975 MHz");
        assert_eq!(freq_text(7_000_000), "7.0 MHz");
        let m = parse_minutes("2026-11-03 01:00Z").unwrap();
        assert_eq!(minutes_text(m), "2026-11-03 01:00Z");
        let day = m as i64 * 60 + 3600 * 5;
        assert_eq!(minutes_on(day, "01:00"), Some(m));
        assert_eq!(subject("\n  Shelter roster \nline two"), "Shelter roster");
        assert_eq!(clip("añb", 2), "a");
    }
}
