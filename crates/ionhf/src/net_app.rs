//! The Net section's state, apart from drawing: net control's open and
//! close (the announcement and the summary offered as objects), a
//! member's check-in form and outbox, and the net log kept as events come.
//! Each step returns the engine commands it needs.

use std::path::PathBuf;

use ion::link::net::{MAX_LINE, MAX_MSG, NetEvent, ObjectId, Precedence, Traffic};

use crate::config::Config;
use crate::ion_engine::{IonCmd, NetStatus};
use crate::net_doc::{self, Announcement, Kind, MAX_FILE, Record, State, To};
use crate::net_log::{self, Item, NetLog, NetRecord, Role};

/// Net control waits at most this long at the close for every member to
/// hold the summary, s.
pub const CLOSE_WAIT_S: f64 = 180.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Profile,
    Roster,
    Traffic,
    CheckIn,
    Inbox,
    Log,
}

impl Page {
    pub fn parse(s: &str) -> Option<Page> {
        Some(match s.to_ascii_lowercase().as_str() {
            "profile" => Page::Profile,
            "roster" => Page::Roster,
            "traffic" => Page::Traffic,
            "checkin" | "check-in" => Page::CheckIn,
            "inbox" => Page::Inbox,
            "log" => Page::Log,
            _ => return None,
        })
    }
}

/// What a member's form sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Draft {
    #[default]
    None,
    Message,
    File,
}

#[derive(Clone, Debug, Default)]
pub struct Form {
    pub kind: Draft,
    pub precedence: Precedence,
    pub to: To,
    pub short: bool,
    pub text: String,
    /// A file's path.
    pub file: String,
}

/// Net control is closing: waiting for the summary to be held.
#[derive(Clone, Copy, Debug)]
pub struct Closing {
    pub id: ObjectId,
    /// App seconds.
    pub until: f64,
}

/// A member's traffic not yet handed to the net (before it joined).
struct Out {
    item: Item,
    bytes: Vec<u8>,
}

pub struct NetApp {
    /// The nets folder.
    pub dir: PathBuf,
    pub page: Page,
    pub form: Form,
    /// The net we are in (or were in last).
    pub log: Option<NetLog>,
    /// Ours on offer (net control), or the last received (member).
    pub announcement: Option<Announcement>,
    offered: Option<ObjectId>,
    /// The profile changed and the edit is not finished (the field has
    /// the keyboard).
    pub edited: bool,
    /// Net control edited the profile: offer the announcement again.
    pub reoffer: bool,
    pub closing: Option<Closing>,
    queue: Vec<Out>,
    /// Member: our items handed to the net, not yet delivered: (index in
    /// the log, its count in [`NetStatus::items`]).
    sent: Vec<(usize, u32)>,
    /// [`IonCmd::NetItem`]s issued.
    items: u32,
    joined: bool,
    listed: Option<Traffic>,
    /// The Log page: past nets, newest first, and the one shown.
    pub past: Vec<NetLog>,
    pub shown: Option<usize>,
    /// The item of traffic shown in full.
    pub picked: Option<usize>,
    pub md_preview: crate::markdown::Cached,
    pub md_inbox: crate::markdown::Cached,
    pub md_log: crate::markdown::Cached,
}

/// A record as one line for the screen.
pub fn shown(data: &[u8]) -> String {
    match Record::decode(data) {
        Record::Message(m) => match m.precedence {
            Precedence::Routine => m.text,
            p => format!("[{}] {}", net_doc::precedence_word(p), m.text),
        },
        Record::File(f) => format!("File {}, {}", f.name, crate::theme::bytes(f.data.len() as u64)),
        Record::Announcement(a) => format!("Announcement {}", a.net),
        Record::Summary(s) => format!("Summary {}", s.net),
        Record::Text(t) => t,
    }
}

/// How long `bytes` take a member's uplink at the floor (one frame a
/// burst), s: an estimate, without the others' turns.
pub fn floor_estimate_s(bytes: usize) -> f64 {
    use ion::level::{FLOOR, LADDER};
    use ion::link::Air;
    let l = &LADDER[FLOOR];
    let air = ion::link::node::NodeAir {
        lead_s: ion::link::node::DEFAULT_LEAD_S,
    };
    let burst = air.duration(l.wave, FLOOR, ion::link::station::frame_bits(FLOOR, 1));
    bytes.div_ceil(l.frame_bytes()) as f64 * burst
}

impl NetApp {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            page: Page::default(),
            form: Form::default(),
            log: None,
            announcement: None,
            offered: None,
            edited: false,
            reoffer: false,
            closing: None,
            queue: Vec::new(),
            sent: Vec::new(),
            items: 0,
            joined: false,
            listed: None,
            past: Vec::new(),
            shown: None,
            picked: None,
            md_preview: Default::default(),
            md_inbox: Default::default(),
            md_log: Default::default(),
        }
    }

    pub fn refresh_past(&mut self) {
        self.past = NetLog::list(&self.dir);
        if self.shown.is_some_and(|i| i >= self.past.len()) {
            self.shown = None;
        }
    }

    /// Net control: runs the profile's net and offers its announcement.
    pub fn open(&mut self, cfg: &Config) -> Vec<IonCmd> {
        let now = net_log::now();
        let p = &cfg.net_profile;
        let name = p.name.trim().to_ascii_uppercase();
        let ann = p.announcement(&cfg.call, now);
        self.log = Some(NetLog::create(
            &self.dir,
            NetRecord {
                net: name.clone(),
                ncs: cfg.call.clone(),
                operator: cfg.call.clone(),
                role: Role::Ncs,
                freq_hz: ann.freq_hz,
                opened: now,
                announcement: ann.plain_markdown(),
                ..NetRecord::default()
            },
        ));
        self.closing = None;
        self.reoffer = false;
        self.sent.clear();
        self.page = Page::Roster;
        let bytes = Record::Announcement(ann.clone()).encode();
        self.offered = Some(ObjectId::of(&bytes));
        self.announcement = Some(ann);
        vec![
            IonCmd::NetRegulars(p.regulars()),
            IonCmd::NetStart(name),
            IonCmd::NetOffer(bytes),
        ]
    }

    /// The form has something to send.
    pub fn form_ready(&self) -> bool {
        match self.form.kind {
            Draft::None => false,
            Draft::Message => !self.form.text.trim().is_empty(),
            Draft::File => !self.form.file.trim().is_empty(),
        }
    }

    /// Member: joins `cfg.net`, the check-in line and traffic going with
    /// the check-in; what the form holds is queued first.
    pub fn join(&mut self, cfg: &Config) -> Result<Vec<IonCmd>, String> {
        if self.form_ready() {
            // Not joined yet: it is queued, and the listing goes below.
            self.send_form(&cfg.call)?;
        }
        self.closing = None;
        self.sent.clear();
        self.joined = false;
        self.page = Page::CheckIn;
        let t = self.listing();
        self.listed = Some(t);
        Ok(vec![
            IonCmd::NetLine(net_doc::clip(cfg.net_line.trim(), MAX_LINE).to_string()),
            IonCmd::NetTraffic(t),
            IonCmd::NetJoin(cfg.net.trim().to_ascii_uppercase()),
        ])
    }

    /// What we list: our traffic not yet delivered.
    pub fn listing(&self) -> Traffic {
        let log = self.log.as_ref();
        let precs = self.queue.iter().map(|o| o.item.precedence).chain(
            self.sent
                .iter()
                .filter_map(|&(i, _)| log.and_then(|l| l.rec.traffic.get(i)).map(|it| it.precedence)),
        );
        let (n, p) = precs.fold((0usize, Precedence::Routine), |(n, p), q| (n + 1, p.max(q)));
        Traffic {
            items: n.min(7) as u8,
            precedence: p,
            short: self.form.short,
        }
    }

    /// Commands for a listing that changed.
    fn sync_listing(&mut self) -> Vec<IonCmd> {
        let t = self.listing();
        if self.listed == Some(t) {
            return Vec::new();
        }
        self.listed = Some(t);
        vec![IonCmd::NetTraffic(t)]
    }

    /// Our traffic queued until we are in the net, then sent.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn is_joined(&self) -> bool {
        self.joined
    }

    /// Member: sends what the form holds (queued until joined) and clears it.
    pub fn send_form(&mut self, me: &str) -> Result<Vec<IonCmd>, String> {
        let f = &self.form;
        let (rec, item) = match f.kind {
            Draft::None => return Ok(Vec::new()),
            Draft::Message => {
                let text = f.text.trim_end().to_string();
                if text.trim().is_empty() {
                    return Ok(Vec::new());
                }
                (
                    Record::Message(net_doc::Message {
                        precedence: f.precedence,
                        to: f.to,
                        text: text.clone(),
                    }),
                    Item {
                        kind: Kind::Message,
                        subject: net_doc::subject(&text),
                        text,
                        ..Item::default()
                    },
                )
            }
            Draft::File => {
                let path = PathBuf::from(f.file.trim());
                let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if data.len() > MAX_FILE {
                    return Err(format!(
                        "{}: {} over {}",
                        path.display(),
                        crate::theme::bytes(data.len() as u64),
                        crate::theme::bytes(MAX_FILE as u64)
                    ));
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                (
                    Record::File(net_doc::File {
                        precedence: f.precedence,
                        to: f.to,
                        name: name.clone(),
                        data: data.clone(),
                    }),
                    Item {
                        kind: Kind::File,
                        subject: name.clone(),
                        name,
                        size: data.len() as u32,
                        saved: path.display().to_string(),
                        ..Item::default()
                    },
                )
            }
        };
        let item = Item {
            at: net_log::now(),
            from: me.to_string(),
            precedence: f.precedence,
            for_net: f.to == To::Net,
            state: State::Queued,
            ..item
        };
        self.form.text.clear();
        self.form.file.clear();
        Ok(self.send_item(item, rec.encode()))
    }

    /// Member: a routine message for net control.
    pub fn send_text(&mut self, me: &str, text: &str) -> Vec<IonCmd> {
        let rec = Record::Message(net_doc::Message {
            precedence: Precedence::Routine,
            to: To::Ncs,
            text: text.to_string(),
        });
        let item = Item {
            at: net_log::now(),
            from: me.to_string(),
            kind: Kind::Message,
            subject: net_doc::subject(text),
            text: text.to_string(),
            ..Item::default()
        };
        self.send_item(item, rec.encode())
    }

    fn send_item(&mut self, mut item: Item, bytes: Vec<u8>) -> Vec<IonCmd> {
        let mut cmds = Vec::new();
        match self.log.as_mut().filter(|_| self.joined) {
            Some(log) => {
                item.to = log.rec.ncs.clone();
                let i = log.item(item);
                self.items += 1;
                self.sent.push((i, self.items));
                cmds.push(IonCmd::NetItem(bytes));
            }
            None => self.queue.push(Out { item, bytes }),
        }
        cmds.extend(self.sync_listing());
        cmds
    }

    /// Net control: a bulletin for all (`to` empty or ALL), or a message
    /// for one member.
    pub fn bulletin(&mut self, me: &str, to: &str, text: &str) -> Vec<IonCmd> {
        let to = to.trim().to_ascii_uppercase();
        let all = to.is_empty() || to == "ALL";
        let rec = Record::Message(net_doc::Message {
            precedence: Precedence::Routine,
            to: if all { To::Net } else { To::Ncs },
            text: text.to_string(),
        })
        .encode();
        let now = net_log::now();
        if let Some(log) = self.log.as_mut() {
            if all {
                log.bulletin(me, text, now);
            } else {
                log.item(Item {
                    at: now,
                    from: me.to_string(),
                    to: to.clone(),
                    kind: Kind::Message,
                    subject: net_doc::subject(text),
                    text: text.to_string(),
                    state: State::Queued,
                    ..Item::default()
                });
            }
        }
        vec![if all {
            IonCmd::NetSend(rec)
        } else {
            IonCmd::NetSendTo(to, rec)
        }]
    }

    /// Net control: marks item `i` handled (or not).
    pub fn set_handled(&mut self, i: usize, handled: bool) {
        if let Some(log) = self.log.as_mut() {
            log.set_state(i, if handled { State::Handled } else { State::Received });
        }
    }

    /// Net control: passes item `i`, for the net, on to all.
    pub fn pass(&mut self, me: &str, i: usize) -> Vec<IonCmd> {
        let Some(log) = self.log.as_mut() else {
            return Vec::new();
        };
        let Some(it) = log.rec.traffic.get(i).cloned() else {
            return Vec::new();
        };
        let rec = match it.kind {
            Kind::Message => Record::Message(net_doc::Message {
                precedence: it.precedence,
                to: To::Net,
                text: format!("{}: {}", it.from, it.text),
            }),
            Kind::File => match std::fs::read(&it.saved) {
                Ok(data) => Record::File(net_doc::File {
                    precedence: it.precedence,
                    to: To::Net,
                    name: it.name.clone(),
                    data,
                }),
                Err(_) => return Vec::new(),
            },
        };
        let now = net_log::now();
        log.bulletin(me, &format!("{}: {}", it.from, it.subject), now);
        log.set_state(i, State::Handled);
        vec![IonCmd::NetSend(rec.encode())]
    }

    /// Net control: offers the summary and waits for the members to hold
    /// it (see [`NetApp::status`]) before the net closes.
    pub fn close(&mut self, now_s: f64) -> Vec<IonCmd> {
        let Some(log) = self.log.as_ref() else {
            return vec![IonCmd::NetClose];
        };
        let mut bytes = Record::Summary(log.summary(net_log::now())).encode();
        bytes.truncate(MAX_MSG);
        let id = ObjectId::of(&bytes);
        self.closing = Some(Closing {
            id,
            until: now_s + CLOSE_WAIT_S,
        });
        // Offered before the announcement goes, so it takes an index no
        // report in flight can claim it holds.
        let mut cmds = vec![IonCmd::NetCheckins(false), IonCmd::NetOffer(bytes)];
        if let Some(old) = self.offered.take() {
            cmds.push(IonCmd::NetWithdraw(old));
        }
        cmds
    }

    /// Net control: closes without waiting any longer.
    pub fn close_now(&mut self) -> Vec<IonCmd> {
        self.closing = None;
        vec![IonCmd::NetClose]
    }

    /// Each status: our traffic delivered, the listing, the close, and an
    /// edited announcement offered again.
    pub fn status(&mut self, n: Option<&NetStatus>, now_s: f64, cfg: &Config) -> Vec<IonCmd> {
        let Some(n) = n else { return Vec::new() };
        let mut cmds = Vec::new();
        if !n.ncs {
            // Nothing left to send of what the worker had been handed when
            // it built this status.
            if n.pending == 0 {
                let (done, left): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sent)
                    .into_iter()
                    .partition(|&(_, k)| k <= n.items);
                self.sent = left;
                if let Some(log) = self.log.as_mut() {
                    for (i, _) in done {
                        log.set_state(i, State::Delivered);
                    }
                }
            }
            cmds.extend(self.sync_listing());
            return cmds;
        }
        if let Some(c) = self.closing {
            let idx = n.offers.iter().position(|o| *o == Some(c.id));
            let held = idx.is_some_and(|i| n.roster.iter().all(|r| r.holds & 1 << i != 0));
            if n.roster.is_empty() || held || now_s >= c.until {
                cmds.extend(self.close_now());
            }
            return cmds;
        }
        if self.reoffer && !n.closed {
            self.reoffer = false;
            let opened = self.log.as_ref().map_or_else(net_log::now, |l| l.rec.opened);
            let ann = cfg.net_profile.announcement(&cfg.call, opened);
            let bytes = Record::Announcement(ann.clone()).encode();
            let id = ObjectId::of(&bytes);
            if self.offered != Some(id) {
                if let Some(old) = self.offered.replace(id) {
                    cmds.push(IonCmd::NetWithdraw(old));
                }
                cmds.push(IonCmd::NetOffer(bytes));
                if let Some(log) = self.log.as_mut() {
                    log.rec.announcement = ann.plain_markdown();
                    log.rec.freq_hz = ann.freq_hz;
                    log.save();
                }
                self.announcement = Some(ann);
            }
        }
        cmds
    }

    /// A net event into the log.
    pub fn event(&mut self, e: &NetEvent, me: &str) -> Vec<IonCmd> {
        let now = net_log::now();
        let mut cmds = Vec::new();
        match e {
            NetEvent::Joined { ncs, net, .. } => {
                self.joined = true;
                if self
                    .log
                    .as_ref()
                    .is_none_or(|l| l.rec.closed.is_some() || l.rec.role != Role::Member)
                {
                    self.log = Some(NetLog::create(
                        &self.dir,
                        NetRecord {
                            net: net.clone(),
                            ncs: ncs.clone(),
                            operator: me.to_string(),
                            role: Role::Member,
                            opened: now,
                            ..NetRecord::default()
                        },
                    ));
                }
                if let Some(log) = self.log.as_mut() {
                    for Out { mut item, bytes } in self.queue.drain(..) {
                        item.to = ncs.clone();
                        item.at = now;
                        self.items += 1;
                        self.sent.push((log.item(item), self.items));
                        cmds.push(IonCmd::NetItem(bytes));
                    }
                }
            }
            NetEvent::CheckedIn { call, slot, via } => {
                if let Some(log) = self.log.as_mut() {
                    log.checked_in(call, *slot, via.as_deref(), now);
                }
            }
            NetEvent::Line { call, line } => {
                if let Some(log) = self.log.as_mut() {
                    log.line(call, line, now);
                }
            }
            NetEvent::Traffic { call, traffic } => {
                if let Some(log) = self.log.as_mut() {
                    log.traffic_listed(call, *traffic, now);
                }
            }
            NetEvent::Routed { call, via } => {
                if let Some(log) = self.log.as_mut() {
                    log.routed(call, via.as_deref(), now);
                }
            }
            NetEvent::Missed { call } => {
                if let Some(log) = self.log.as_mut() {
                    log.missed(call, now);
                }
            }
            NetEvent::Message {
                from,
                to,
                data,
                overheard: false,
            } => self.message(from, to.as_deref(), data, now),
            NetEvent::Object { data, .. } => self.object(data),
            NetEvent::Closed | NetEvent::Lost => {
                if let Some(log) = self.log.as_mut() {
                    log.close(now);
                }
                self.joined = false;
                self.closing = None;
                self.sent.clear();
                self.offered = None;
                self.refresh_past();
            }
            NetEvent::Message { overheard: true, .. } | NetEvent::Relayed { .. } => {}
        }
        cmds
    }

    fn message(&mut self, from: &str, to: Option<&str>, data: &[u8], now: i64) {
        let Some(log) = self.log.as_mut() else { return };
        let rec = Record::decode(data);
        let Some(to) = to else {
            // A bulletin; a file for all is kept as an item.
            match rec {
                Record::File(f) => {
                    let saved = log.save_file(&f.name, &f.data).map(|p| p.display().to_string());
                    log.item(Item {
                        at: now,
                        from: from.to_string(),
                        precedence: f.precedence,
                        kind: Kind::File,
                        subject: f.name.clone(),
                        name: f.name,
                        size: f.data.len() as u32,
                        saved: saved.unwrap_or_default(),
                        state: State::Received,
                        ..Item::default()
                    });
                }
                Record::Announcement(_) | Record::Summary(_) => {
                    if log.rec.role == Role::Member && from == log.rec.ncs {
                        self.object(data);
                    }
                }
                _ => log.bulletin(from, &shown(data), now),
            }
            return;
        };
        let item = match rec {
            Record::Message(m) => Item {
                precedence: m.precedence,
                for_net: m.to == To::Net,
                kind: Kind::Message,
                subject: net_doc::subject(&m.text),
                text: m.text,
                ..Item::default()
            },
            Record::Text(t) => Item {
                kind: Kind::Message,
                subject: net_doc::subject(&t),
                text: t,
                ..Item::default()
            },
            Record::File(f) => {
                let saved = log.save_file(&f.name, &f.data).map(|p| p.display().to_string());
                Item {
                    precedence: f.precedence,
                    for_net: f.to == To::Net,
                    kind: Kind::File,
                    subject: f.name.clone(),
                    name: f.name,
                    size: f.data.len() as u32,
                    saved: saved.unwrap_or_default(),
                    ..Item::default()
                }
            }
            Record::Announcement(_) | Record::Summary(_) => {
                if log.rec.role == Role::Member && from == log.rec.ncs {
                    self.object(data);
                }
                return;
            }
        };
        log.item(Item {
            at: now,
            from: from.to_string(),
            to: to.to_string(),
            state: State::Received,
            ..item
        });
    }

    /// Member: an object net control offers (or sent us).
    fn object(&mut self, data: &[u8]) {
        if self.log.as_ref().is_some_and(|l| l.rec.role != Role::Member) {
            return;
        }
        match Record::decode(data) {
            Record::Announcement(a) => {
                if let Some(log) = self.log.as_mut() {
                    log.rec.announcement = a.plain_markdown();
                    log.rec.freq_hz = log.rec.freq_hz.or(a.freq_hz);
                    log.save();
                }
                self.announcement = Some(a);
            }
            Record::Summary(s) => {
                if let Some(log) = self.log.as_mut() {
                    log.merge(&s);
                }
            }
            _ => {}
        }
    }

    /// The announcement's id on offer (net control).
    pub fn offered(&self) -> Option<ObjectId> {
        self.offered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net_log::tests::TempDir;
    use ion::link::net::RosterEntry;

    fn entry(call: &str, holds: u8) -> RosterEntry {
        RosterEntry {
            slot: 1,
            call: call.into(),
            via: None,
            snr_up: None,
            snr_down: None,
            cap: 0,
            last_heard_s: None,
            missed: 0,
            bytes_in: 0,
            hears: Vec::new(),
            caps: None,
            version: None,
            children: Vec::new(),
            line: String::new(),
            traffic: Traffic::default(),
            holds,
        }
    }

    fn cfg(call: &str) -> Config {
        let mut c = Config {
            call: call.into(),
            net: "TNARES".into(),
            net_line: "Trousdale County ARES".into(),
            ..Config::default()
        };
        c.net_profile.name = "TNARES".into();
        c.net_profile.freq = "3.975".into();
        c.net_profile.regulars = "KJ5ET AG5B".into();
        c.net_profile.body = "# Tonight\n\nTraining.".into();
        c
    }

    #[test]
    fn net_control_opens_logs_and_closes_once_the_summary_is_held() {
        let d = TempDir::new("netapp-ncs");
        let c = cfg("N0JLF");
        let mut a = NetApp::new(d.0.clone());
        let cmds = a.open(&c);
        assert!(matches!(&cmds[0], IonCmd::NetRegulars(r) if r == &["KJ5ET", "AG5B"]));
        assert!(matches!(&cmds[1], IonCmd::NetStart(n) if n == "TNARES"));
        let IonCmd::NetOffer(ann) = &cmds[2] else { panic!() };
        assert!(matches!(Record::decode(ann), Record::Announcement(x) if x.freq_hz == Some(3_975_000)));

        a.event(
            &NetEvent::CheckedIn {
                call: "KJ5ET".into(),
                slot: 1,
                via: None,
            },
            "N0JLF",
        );
        a.event(
            &NetEvent::Line {
                call: "KJ5ET".into(),
                line: "Trousdale County ARES".into(),
            },
            "N0JLF",
        );
        let msg = Record::Message(net_doc::Message {
            precedence: Precedence::Priority,
            to: To::Net,
            text: "Shelter open\nat 1900".into(),
        });
        a.event(
            &NetEvent::Message {
                from: "KJ5ET".into(),
                to: Some("N0JLF".into()),
                data: msg.encode(),
                overheard: false,
            },
            "N0JLF",
        );
        let mut fake = NetLog::create(
            &d.0.join("fake"),
            NetRecord {
                net: "TNARES".into(),
                ncs: "N0JLF".into(),
                ..NetRecord::default()
            },
        );
        fake.close(1);
        let sum = Record::Summary(fake.summary(1)).encode();
        a.event(
            &NetEvent::Message {
                from: "KJ5ET".into(),
                to: Some("N0JLF".into()),
                data: sum,
                overheard: false,
            },
            "N0JLF",
        );
        let log = &a.log.as_ref().unwrap().rec;
        assert_eq!(log.checkins.len(), 1, "a member's summary rewrote the log");
        assert!(log.closed.is_none());
        assert_eq!(log.checkins[0].line, "Trousdale County ARES");
        let it = &log.traffic[0];
        assert_eq!(
            (it.precedence, it.for_net, it.state),
            (Precedence::Priority, true, State::Received)
        );
        assert_eq!(it.subject, "Shelter open");
        let pass = a.pass("N0JLF", 0);
        let IonCmd::NetSend(b) = &pass[0] else { panic!() };
        assert!(matches!(Record::decode(b), Record::Message(m) if m.text == "KJ5ET: Shelter open\nat 1900"));
        assert_eq!(a.log.as_ref().unwrap().rec.traffic[0].state, State::Handled);

        // An edit is offered again once committed, the old one withdrawn.
        let mut c2 = c.clone();
        c2.net_profile.body.push_str(" Bring water.");
        let old = a.offered().unwrap();
        let mut st = NetStatus {
            ncs: true,
            roster: vec![entry("KJ5ET", 0)],
            ..NetStatus::default()
        };
        assert!(a.status(Some(&st), 1.0, &c2).is_empty());
        a.reoffer = true;
        let cmds = a.status(Some(&st), 2.0, &c2);
        assert!(matches!(cmds[0], IonCmd::NetWithdraw(id) if id == old));
        assert!(matches!(&cmds[1], IonCmd::NetOffer(_)));

        let cmds = a.close(10.0);
        let IonCmd::NetOffer(sum) = &cmds[1] else { panic!() };
        assert!(matches!(cmds[2], IonCmd::NetWithdraw(_)));
        let Record::Summary(s) = Record::decode(sum) else {
            panic!()
        };
        assert_eq!(s.checkins[0].call, "KJ5ET");
        assert_eq!(s.items[0].state, State::Handled);
        let id = ObjectId::of(sum);
        st.offers[3] = Some(id);
        assert!(a.status(Some(&st), 11.0, &c).is_empty(), "not held yet");
        st.roster[0].holds = 1 << 3;
        assert!(matches!(a.status(Some(&st), 12.0, &c)[..], [IonCmd::NetClose]));
        // The bound: nobody holds it, the wait runs out.
        a.close(20.0);
        st.roster[0].holds = 0;
        assert!(a.status(Some(&st), 20.0 + CLOSE_WAIT_S - 1.0, &c).is_empty());
        assert!(matches!(
            a.status(Some(&st), 20.0 + CLOSE_WAIT_S, &c)[..],
            [IonCmd::NetClose]
        ));
        // No members: no wait.
        a.close(30.0);
        st.roster.clear();
        assert!(matches!(a.status(Some(&st), 30.0, &c)[..], [IonCmd::NetClose]));
        a.event(&NetEvent::Closed, "N0JLF");
        assert!(a.log.as_ref().unwrap().rec.closed.is_some());
        assert_eq!(a.past.len(), 1);
    }

    #[test]
    fn a_member_checks_in_with_what_the_form_holds() {
        let d = TempDir::new("netapp-member");
        let c = cfg("AG5B");
        let mut a = NetApp::new(d.0.clone());
        a.form = Form {
            kind: Draft::Message,
            precedence: Precedence::Priority,
            to: To::Ncs,
            short: true,
            text: "Generator test complete".into(),
            file: String::new(),
        };
        let cmds = a.join(&c).unwrap();
        assert!(matches!(&cmds[0], IonCmd::NetLine(l) if l == "Trousdale County ARES"));
        let want = Traffic {
            items: 1,
            precedence: Precedence::Priority,
            short: true,
        };
        assert!(matches!(cmds[1], IonCmd::NetTraffic(t) if t == want));
        assert!(matches!(&cmds[2], IonCmd::NetJoin(n) if n == "TNARES"));
        assert_eq!(a.queued(), 1);
        assert!(a.form.text.is_empty());

        let cmds = a.event(
            &NetEvent::Joined {
                ncs: "N0JLF".into(),
                net: "TNARES".into(),
                slot: 2,
            },
            "AG5B",
        );
        let IonCmd::NetItem(b) = &cmds[0] else {
            panic!("{cmds:?}")
        };
        assert!(matches!(Record::decode(b), Record::Message(m) if m.precedence == Precedence::Priority));
        assert_eq!(a.queued(), 0);
        let it = &a.log.as_ref().unwrap().rec.traffic[0];
        assert_eq!((it.to.as_str(), it.state), ("N0JLF", State::Queued));

        let mut st = NetStatus {
            pending: 0,
            ..NetStatus::default()
        };
        assert!(a.status(Some(&st), 1.0, &c).is_empty(), "a status from before the send");
        st.pending = 40;
        a.status(Some(&st), 2.0, &c);
        st.pending = 0;
        st.items = 1;
        // Another item, handed over after the worker built the status.
        a.send_text("AG5B", "Second report");
        let cmds = a.status(Some(&st), 3.0, &c);
        assert!(matches!(cmds[..], [IonCmd::NetTraffic(t)] if t.items == 1));
        let state = |a: &NetApp, i: usize| a.log.as_ref().unwrap().rec.traffic[i].state;
        assert_eq!(state(&a, 0), State::Delivered);
        assert_eq!(state(&a, 1), State::Queued, "delivered before the worker had it");
        st.items = 2;
        let cmds = a.status(Some(&st), 4.0, &c);
        assert!(matches!(cmds[..], [IonCmd::NetTraffic(t)] if t.items == 0));
        assert_eq!(state(&a, 1), State::Delivered);

        // The announcement, a bulletin, a file and the summary.
        let ann = Record::Announcement(c.net_profile.announcement("N0JLF", 0)).encode();
        a.event(
            &NetEvent::Object {
                id: ObjectId::of(&ann),
                data: ann,
                cached: false,
            },
            "AG5B",
        );
        assert_eq!(a.announcement.as_ref().unwrap().net, "TNARES");
        let bul = Record::Message(net_doc::Message {
            text: "Net closes at 2130".into(),
            ..Default::default()
        });
        a.event(
            &NetEvent::Message {
                from: "N0JLF".into(),
                to: None,
                data: bul.encode(),
                overheard: false,
            },
            "AG5B",
        );
        let file = Record::File(net_doc::File {
            name: "roster.txt".into(),
            data: b"KJ5ET\nAG5B\n".to_vec(),
            ..Default::default()
        });
        a.event(
            &NetEvent::Message {
                from: "N0JLF".into(),
                to: Some("AG5B".into()),
                data: file.encode(),
                overheard: false,
            },
            "AG5B",
        );
        let log = a.log.as_ref().unwrap();
        assert_eq!(log.rec.bulletins[0].text, "Net closes at 2130");
        let f = &log.rec.traffic[2];
        assert_eq!(std::fs::read(&f.saved).unwrap(), b"KJ5ET\nAG5B\n");

        let mut ncs_log = NetLog::create(
            &d.0.join("ncs"),
            NetRecord {
                net: "TNARES".into(),
                ncs: "N0JLF".into(),
                opened: log.rec.opened - 60,
                ..NetRecord::default()
            },
        );
        ncs_log.checked_in("AG5B", 2, None, log.rec.opened);
        let sum = Record::Summary(ncs_log.summary(log.rec.opened + 600)).encode();
        // Only net control's summary counts.
        a.event(
            &NetEvent::Message {
                from: "KJ5ET".into(),
                to: Some("AG5B".into()),
                data: sum.clone(),
                overheard: false,
            },
            "AG5B",
        );
        assert!(!a.log.as_ref().unwrap().rec.summary);
        a.event(
            &NetEvent::Object {
                id: ObjectId::of(&sum),
                data: sum,
                cached: false,
            },
            "AG5B",
        );
        a.event(&NetEvent::Closed, "AG5B");
        let r = &NetLog::load(&a.log.as_ref().unwrap().path).unwrap().rec;
        assert!(r.summary);
        assert_eq!(r.checkins[0].call, "AG5B");
        assert!(!a.is_joined());
    }

    #[test]
    fn files_over_the_cap_are_refused() {
        let d = TempDir::new("netapp-file");
        let big = d.0.join("big.bin");
        std::fs::write(&big, vec![0u8; MAX_FILE + 1]).unwrap();
        let mut a = NetApp::new(d.0.clone());
        a.form.kind = Draft::File;
        a.form.file = big.display().to_string();
        assert!(a.send_form("AG5B").unwrap_err().contains("over"));
        let ok = d.0.join("ok.txt");
        std::fs::write(&ok, b"hello").unwrap();
        a.form.file = ok.display().to_string();
        a.send_form("AG5B").unwrap();
        assert_eq!(a.queued(), 1);
        let t = floor_estimate_s(MAX_FILE);
        println!("{} B at the floor: {t:.0} s", MAX_FILE);
        assert!(t > 600.0, "{t}");
    }
}
