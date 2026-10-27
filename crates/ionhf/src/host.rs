//! Winlink host interface: Ion looks like a VARA HF modem on TCP, so Pat
//! or Winlink Express can drive it. Command port (default 8300) and data
//! port (one above), on 127.0.0.1 only, per "VARA Protocol Native TNC
//! Commands" (2022).
//!
//! Settings that mean nothing to Ion (`BW…`, `COMPRESSION …`, `CWID …`,
//! ...) are answered `OK`. `BUFFER n` counts bytes not yet acknowledged by
//! the far end (Pat waits for 0 before closing); `PTT ON/OFF` is for
//! information only, as Ion keys the radio itself; `SN` gives the last S/N
//! heard.
//!
//! Broadcast groups as Data2G's host offers them (its `docs/broadcast.md`),
//! on the KISS port 200 below the command port: port 0 is the group
//! "KISS 0" while a KISS client is connected; `BCAST OPEN group [FROM call]`
//! opens another (`BCAST PORT n`, 1–15) for the command client. A group's
//! frames go to every station of it once, with no resend
//! ([`ion::link::bcast`]), between sessions. KISS ACKMODE (0x0C) tags come
//! back once a frame has gone out; KISS persistence (command 2) sets P.
//! Once the command client has sent a `BCAST` command it gets
//! `BCAST n HEARD [call] snr`, `BCAST n LOST k` and `BCAST n DROPPED k`.
//! `MODES` lists one mode, `ION` (Ion picks its levels itself), and
//! `BCAST MODE n ...` is answered `OK` for an open port.
//!
//! `CLEANTXBUFFER` drops what has never been on air, as VARA's; what is
//! on air unacknowledged (the limbo) still goes, and a VARA client's
//! `BUFFER` leaves it out ([`ion::link::Clean`]).
//!
//! `DATA2G ON` (off again with `DATA2G OFF` or when the command client
//! leaves) turns on Data2G's host extensions (its `docs/arq.md` §9):
//! `DISCONNECTED` says why (`CLIENT`, `IDLE`, `PEER`, `ABORT`,
//! `LINK_LOST`, `LINK_FAILED weak path`, `NO_ANSWER`, with `unconfirmed`
//! when our goodbye went unacknowledged); `CLEANTXBUFFER [n]` answers in
//! host offsets, then `CLEANTXBUFFER SETTLED` (Data2G PR #118); data
//! written before `CONNECTED` is dropped; `TIMEOUT [s]` gets or sets the
//! link idle close and `TXPOWER [dB]` the output level below the
//! operator's drive (Data2G PR #122; never above it).

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ion::link::station::End;
use ion::link::{Clean, Settled, State};

use crate::engine::UiEvent;
use crate::ion_engine::{IonCmd, IonStatus, TX_POWER_MIN_DB};

/// What `TIMEOUT` says with no idle close set (Ion's default: a quiet link
/// stays up); setting it or more turns the idle close off. Finite, as
/// clients parse it as a number.
const NO_IDLE_CLOSE_S: f64 = 1e9;
/// Longest wait for the Ion worker's answer to a `CLEANTXBUFFER`.
const CLEAN_WAIT: Duration = Duration::from_secs(5);

/// Data2G's words for why a link ended (`DISCONNECTED reason [detail]
/// [unconfirmed]`).
fn end_words(end: Option<End>, state: State) -> String {
    let unconfirmed = |c: bool| if c { "" } else { " unconfirmed" };
    match end {
        Some(End::Client { confirmed }) => format!("CLIENT{}", unconfirmed(confirmed)),
        Some(End::Peer) => "PEER".into(),
        Some(End::NoAnswer) => "NO_ANSWER".into(),
        Some(End::LinkLost) => "LINK_LOST".into(),
        Some(End::Weak { confirmed }) => format!("LINK_FAILED weak path{}", unconfirmed(confirmed)),
        Some(End::Idle { confirmed }) => format!("IDLE{}", unconfirmed(confirmed)),
        None if state == State::Failed => "LINK_LOST".into(),
        None => "ABORT".into(),
    }
}

/// From the Ion worker to the host interface.
#[derive(Debug)]
pub enum HostIn {
    Status(IonStatus),
    Received(Vec<u8>),
    /// A frame of the group on this KISS port.
    GroupFrame(u8, Vec<u8>),
    /// A burst of the group from `call` (None: its header was lost), S/N.
    GroupHeard(u8, Option<String>, f32),
    /// At least this many frames of a burst of the group were lost.
    GroupLost(u8, u32),
    /// This many of our frames for the group were dropped unsent.
    GroupDropped(u8, u32),
    /// Our frame with this ACKMODE tag has gone out.
    GroupSent(u16),
    /// The answer to [`IonCmd::Clean`].
    Clean(Clean),
    /// A `CLEANTXBUFFER` limbo resolved.
    Settled(Settled),
}

/// KISS framing.
const FEND: u8 = 0xC0;
const FESC: u8 = 0xDB;
const TFEND: u8 = 0xDC;
const TFESC: u8 = 0xDD;
const KISS_DATA: u8 = 0x00;
const KISS_PERSIST: u8 = 0x02;
const KISS_ACKMODE: u8 = 0x0C;
/// KISS ports a command client may open groups on (0 is "KISS 0").
const MAX_PORT: u8 = 15;
/// What `MODES` says of Ion's broadcast, for a client sizing its frames and
/// timing its repairs: name, width (Hz), bytes a codeword, codewords a
/// burst at most, seconds at one codeword and at the most (a middling
/// level: Ion picks its own from the stations it hears).
const ION_MODE: &str = "MODE ION 2400 255 16 3.0 30.0";

fn kiss_frame(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![FEND, command];
    for &b in payload {
        match b {
            FEND => out.extend([FESC, TFEND]),
            FESC => out.extend([FESC, TFESC]),
            b => out.push(b),
        }
    }
    out.push(FEND);
    out
}

/// Whole KISS frames out of `buf` (command byte, payload), leaving a
/// partial one there.
fn kiss_frames(buf: &mut Vec<u8>) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let Some(start) = buf.iter().position(|&b| b == FEND) else {
            buf.clear();
            return out;
        };
        let Some(len) = buf[start + 1..].iter().position(|&b| b == FEND) else {
            buf.drain(..start);
            return out;
        };
        let raw: Vec<u8> = buf.drain(..start + 1 + len).skip(start + 1).collect();
        let mut frame = Vec::with_capacity(raw.len());
        let mut esc = false;
        for b in raw {
            if esc {
                frame.push(match b {
                    TFEND => FEND,
                    TFESC => FESC,
                    b => b,
                });
                esc = false;
            } else if b == FESC {
                esc = true;
            } else {
                frame.push(b);
            }
        }
        if let Some((&command, payload)) = frame.split_first() {
            out.push((command, payload.to_vec()));
        }
    }
}

pub struct HostHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Opens the two ports and serves one host at a time.
pub fn spawn(port: u16, ion: Sender<IonCmd>, ui: Sender<UiEvent>) -> std::io::Result<HostHandle> {
    let cmd = TcpListener::bind(("127.0.0.1", port))?;
    let data = TcpListener::bind(("127.0.0.1", port + 1))?;
    cmd.set_nonblocking(true)?;
    data.set_nonblocking(true)?;
    let kiss = port
        .checked_sub(200)
        .and_then(|p| TcpListener::bind(("127.0.0.1", p)).ok())
        .filter(|l| l.set_nonblocking(true).is_ok());
    if kiss.is_none() {
        log::info!("Winlink host: no KISS port {}", port.saturating_sub(200));
    }
    let (tx, rx) = crossbeam_channel::unbounded();
    let _ = ion.send(IonCmd::AttachHost(Some(tx)));
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new().name("winlink-host".into()).spawn(move || {
        let mut h = Host::new(ion.clone(), ui);
        while !stop2.load(Ordering::Relaxed) {
            h.service(&cmd, &data, kiss.as_ref(), &rx);
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = ion.send(IonCmd::AttachHost(None));
    })?;
    Ok(HostHandle {
        stop,
        thread: Some(thread),
    })
}

struct Host {
    ion: Sender<IonCmd>,
    ui: Sender<UiEvent>,
    cmd: Option<TcpStream>,
    data: Option<TcpStream>,
    kiss: Option<TcpStream>,
    line: Vec<u8>,
    mycall: String,
    listen: bool,
    /// We sent CONNECT (so we are the caller).
    calling: Option<String>,
    /// The link is up: (caller, callee).
    connected: Option<(String, String)>,
    last_buffer: Option<usize>,
    last_ptt: bool,
    last_busy: bool,
    last_state: Option<State>,
    last_snr: Option<f32>,
    alive_at: Instant,
    /// Bytes from the host waiting for the data socket to accept them
    /// (and received bytes waiting for the host).
    to_host: Vec<u8>,
    /// KISS bytes from the client not yet a whole frame, and ours for it.
    kiss_in: Vec<u8>,
    kiss_out: Vec<u8>,
    /// Groups open, by KISS port.
    ports: std::collections::BTreeMap<u8, String>,
    /// The command client has sent a BCAST command: it gets the statuses.
    bcast_client: bool,
    /// The command client sent `DATA2G ON`.
    data2g: bool,
    /// Lines for the command client, sent at the end of each service.
    replies: Vec<String>,
    /// A `CLEANTXBUFFER` waits for the worker's answer since then: later
    /// commands wait too, so every answer goes in order.
    waiting: Option<Instant>,
    /// DATA2G ON: the limbo we reported (host offsets), until SETTLED; and
    /// as the last status had it (from, first unacknowledged, to).
    limbo: Option<(u64, u64)>,
    status_limbo: Option<(u64, u64, u64)>,
    /// The client's TIMEOUT and TXPOWER (dB).
    timeout: Option<f64>,
    tx_db: f32,
}

impl Host {
    fn new(ion: Sender<IonCmd>, ui: Sender<UiEvent>) -> Self {
        Self {
            ion,
            ui,
            cmd: None,
            data: None,
            kiss: None,
            line: Vec::new(),
            mycall: String::new(),
            listen: false,
            calling: None,
            connected: None,
            last_buffer: None,
            last_ptt: false,
            last_busy: false,
            last_state: None,
            last_snr: None,
            alive_at: Instant::now(),
            to_host: Vec::new(),
            kiss_in: Vec::new(),
            kiss_out: Vec::new(),
            ports: std::collections::BTreeMap::new(),
            bcast_client: false,
            data2g: false,
            replies: Vec::new(),
            waiting: None,
            limbo: None,
            status_limbo: None,
            timeout: None,
            tx_db: 0.0,
        }
    }

    fn note(&self, s: String) {
        let _ = self.ui.try_send(UiEvent::Host(s));
    }

    fn say(&mut self, line: &str) {
        self.replies.push(line.to_string());
    }

    fn flush(&mut self) {
        let lines = std::mem::take(&mut self.replies);
        if let Some(c) = self.cmd.as_mut() {
            let mut b = Vec::new();
            for l in lines {
                b.extend(l.into_bytes());
                b.push(b'\r');
            }
            if !b.is_empty() && c.write_all(&b).is_err() {
                self.cmd = None;
                self.client_gone();
            }
        }
    }

    /// The command client has gone (or another took its place): what it
    /// set goes with it. Its groups close ("KISS 0" stays with the KISS
    /// client); its TIMEOUT and TXPOWER go back to Ion's own.
    fn client_gone(&mut self) {
        self.note("Winlink host disconnected".into());
        self.bcast_client = false;
        self.data2g = false;
        self.line.clear();
        self.waiting = None;
        self.limbo = None;
        let open: Vec<u8> = self.ports.keys().copied().filter(|&p| p != 0).collect();
        for p in open {
            self.close_port(p);
        }
        if self.timeout.take().is_some() {
            let _ = self.ion.send(IonCmd::IdleClose(None));
        }
        if self.tx_db != 0.0 {
            self.tx_db = 0.0;
            let _ = self.ion.send(IonCmd::TxPowerDb(0.0));
        }
    }

    /// One read of the data port onto the link; whether anything came.
    fn read_data(&mut self) -> bool {
        let mut buf = [0u8; 1024];
        let Some(d) = self.data.as_mut() else {
            return false;
        };
        match d.read(&mut buf) {
            Ok(0) => {
                self.data = None;
                false
            }
            Ok(n) => {
                if self.data2g && self.connected.is_none() {
                    log::info!("Winlink host: DATA2G: {n} B before CONNECTED dropped");
                } else {
                    let _ = self.ion.send(IonCmd::Send(buf[..n].to_vec()));
                }
                true
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(_) => {
                self.data = None;
                false
            }
        }
    }

    fn service(
        &mut self,
        cmd_l: &TcpListener,
        data_l: &TcpListener,
        kiss_l: Option<&TcpListener>,
        rx: &Receiver<HostIn>,
    ) {
        // New hosts replace old ones.
        if let Ok((s, addr)) = cmd_l.accept() {
            let _ = s.set_nonblocking(true);
            let _ = s.set_nodelay(true);
            if self.cmd.is_some() {
                self.client_gone();
            }
            self.replies.clear();
            self.cmd = Some(s);
            self.line.clear();
            self.note(format!("Winlink host connected from {addr}"));
        }
        if let Ok((s, _)) = data_l.accept() {
            let _ = s.set_nonblocking(true);
            let _ = s.set_nodelay(true);
            self.data = Some(s);
        }
        if let Some(Ok((s, _))) = kiss_l.map(TcpListener::accept) {
            let _ = s.set_nonblocking(true);
            self.kiss = Some(s);
            self.kiss_in.clear();
            self.kiss_out.clear();
            self.open_port(0, "KISS 0");
        }
        let mut buf = [0u8; 1024];
        if let Some(c) = self.cmd.as_mut() {
            match c.read(&mut buf) {
                Ok(n) if n > 0 => self.line.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                _ => {
                    self.cmd = None;
                    self.client_gone();
                }
            }
        }
        if self.waiting.is_some_and(|t| t.elapsed() >= CLEAN_WAIT) {
            self.waiting = None;
            self.say("CLEANTXBUFFER FAILED ");
        }
        self.commands();
        let mut kiss_gone = false;
        if let Some(k) = self.kiss.as_mut() {
            match k.read(&mut buf) {
                Ok(0) => kiss_gone = true,
                Ok(n) => self.kiss_in.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => kiss_gone = true,
            }
        }
        if kiss_gone {
            self.kiss = None;
            self.close_port(0);
        }
        for (command, payload) in kiss_frames(&mut self.kiss_in) {
            let port = command >> 4;
            match command & 0x0F {
                KISS_DATA if self.ports.contains_key(&port) && !payload.is_empty() => {
                    let _ = self.ion.send(IonCmd::GroupSend(port, payload, None));
                }
                KISS_ACKMODE if self.ports.contains_key(&port) && payload.len() > 2 => {
                    let tag = u16::from_be_bytes([payload[0], payload[1]]);
                    let _ = self.ion.send(IonCmd::GroupSend(port, payload[2..].to_vec(), Some(tag)));
                }
                KISS_PERSIST if !payload.is_empty() => {
                    let _ = self.ion.send(IonCmd::GroupPersist(payload[0]));
                }
                // Other ports, and settings that mean nothing to Ion.
                _ => {}
            }
        }
        // Data from the host goes onto the link.
        self.read_data();
        for m in rx.try_iter() {
            self.ion_in(m);
        }
        if !self.kiss_out.is_empty()
            && let Some(k) = self.kiss.as_mut()
        {
            match k.write(&self.kiss_out) {
                Ok(n) => {
                    self.kiss_out.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.kiss = None,
            }
        }
        if !self.to_host.is_empty()
            && let Some(d) = self.data.as_mut()
        {
            match d.write(&self.to_host) {
                Ok(n) => {
                    self.to_host.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.data = None,
            }
        }
        if self.alive_at.elapsed() >= Duration::from_secs(60) {
            self.alive_at = Instant::now();
            self.say("IAMALIVE");
        }
        self.flush();
    }

    fn ion_in(&mut self, m: HostIn) {
        match m {
            HostIn::Received(b) => self.to_host.extend(b),
            HostIn::Status(s) => self.status(&s),
            HostIn::GroupFrame(p, b) if self.ports.contains_key(&p) => {
                self.kiss_out.extend(kiss_frame(p << 4 | KISS_DATA, &b));
            }
            HostIn::GroupSent(tag) => {
                // To the KISS client that tagged it; the port byte is
                // the group's, which a client keys on the tag alone.
                self.kiss_out.extend(kiss_frame(KISS_ACKMODE, &tag.to_be_bytes()));
            }
            HostIn::GroupHeard(p, call, snr) if self.bcast_client => {
                let who = call.map(|c| format!("{c} ")).unwrap_or_default();
                self.say(&format!("BCAST {p} HEARD {who}{snr:.1}"));
            }
            HostIn::GroupLost(p, k) if self.bcast_client => self.say(&format!("BCAST {p} LOST {k}")),
            HostIn::GroupDropped(p, k) if self.bcast_client => self.say(&format!("BCAST {p} DROPPED {k}")),
            HostIn::Clean(r) if self.waiting.take().is_some() => {
                let line = match r {
                    Clean::Empty => "CLEANTXBUFFER BUFFEREMPTY".to_string(),
                    Clean::Failed => "CLEANTXBUFFER FAILED ".to_string(),
                    Clean::Late => "CLEANTXBUFFER LATE".to_string(),
                    Clean::Done { drop, limbo } if self.data2g => {
                        if limbo.0 < limbo.1 && self.limbo.is_none() {
                            self.limbo = Some(limbo);
                        }
                        format!("CLEANTXBUFFER OK {} {} {} {}", drop.0, drop.1, limbo.0, limbo.1)
                    }
                    Clean::Done { .. } => "CLEANTXBUFFER OK".to_string(),
                };
                self.say(&line);
            }
            HostIn::Settled(s) if self.data2g && self.limbo.is_some_and(|l| l.0 == s.from) => {
                self.limbo = None;
                let unknown = if s.unknown { " UNKNOWN" } else { "" };
                self.say(&format!(
                    "CLEANTXBUFFER SETTLED {} {} {}{unknown}",
                    s.from, s.split, s.to
                ));
            }
            _ => {}
        }
    }

    /// CLEANTXBUFFER [n]: what the client has written so far goes to the
    /// link first, then the worker answers.
    fn clean(&mut self, upto: Option<u64>) {
        while self.read_data() {}
        let _ = self.ion.send(IonCmd::Clean {
            upto,
            vara: !self.data2g,
        });
        self.waiting = Some(Instant::now());
    }

    /// The command lines in, in order (none while one waits for its answer).
    fn commands(&mut self) {
        while self.waiting.is_none()
            && let Some(i) = self.line.iter().position(|&b| b == b'\r' || b == b'\n')
        {
            let l: Vec<u8> = self.line.drain(..=i).collect();
            let text = String::from_utf8_lossy(&l[..l.len() - 1]).trim().to_string();
            if !text.is_empty() {
                self.command(&text);
            }
        }
    }

    fn command(&mut self, text: &str) {
        let up = text.to_ascii_uppercase();
        let words: Vec<&str> = up.split_whitespace().collect();
        match words.as_slice() {
            ["MYCALL", call, ..] => {
                self.mycall = call.split('-').next().unwrap_or(call).to_string();
                let _ = self.ion.send(IonCmd::SetCall(self.mycall.clone()));
                self.say("OK");
            }
            ["LISTEN", "ON"] => {
                self.listen = true;
                if self.connected.is_none() && self.calling.is_none() {
                    let _ = self.ion.send(IonCmd::Listen);
                }
                self.say("OK");
            }
            ["LISTEN", "OFF"] => {
                self.listen = false;
                if self.connected.is_none() && self.calling.is_none() {
                    let _ = self.ion.send(IonCmd::Stop);
                }
                self.say("OK");
            }
            ["CONNECT", src, dst, ..] => {
                if self.mycall.is_empty() {
                    self.mycall = src.to_string();
                }
                let dst = dst.split('-').next().unwrap_or(dst).to_string();
                self.calling = Some(dst.clone());
                let _ = self.ion.send(IonCmd::Connect(dst.clone()));
                self.note(format!("Winlink host: calling {dst}"));
                self.say("OK");
            }
            ["DISCONNECT"] => {
                self.say("OK");
                if self.connected.is_some() {
                    let _ = self.ion.send(IonCmd::DisconnectWhenSent);
                } else {
                    // Nothing up (or still calling): give up now.
                    let _ = self.ion.send(IonCmd::Stop);
                    self.calling = None;
                    self.disconnected("CLIENT");
                    self.relisten();
                }
            }
            ["ABORT"] => {
                self.say("OK");
                // Before its DISCONNECTED: the limbo as the last status had
                // it (acknowledged before the split).
                if let Some((from, to)) = self.limbo.take() {
                    let split = self
                        .status_limbo
                        .filter(|l| l.0 == from)
                        .map_or(from, |l| l.1.clamp(from, to));
                    self.say(&format!("CLEANTXBUFFER SETTLED {from} {split} {to} UNKNOWN"));
                }
                let _ = self.ion.send(IonCmd::Stop);
                self.calling = None;
                if self.connected.take().is_some() {
                    self.disconnected("ABORT");
                }
                self.relisten();
            }
            ["DATA2G", on @ ("ON" | "OFF")] => {
                self.data2g = *on == "ON";
                if !self.data2g {
                    self.limbo = None;
                }
                self.say("OK");
            }
            ["CLEANTXBUFFER"] => self.clean(None),
            ["CLEANTXBUFFER", n] if self.data2g && n.len() <= 18 && n.bytes().all(|b| b.is_ascii_digit()) => {
                self.clean(n.parse().ok());
            }
            ["TIMEOUT"] if self.data2g => {
                let s = self.timeout.unwrap_or(NO_IDLE_CLOSE_S);
                self.say(&format!("TIMEOUT {s:.1}"));
            }
            ["TIMEOUT", v] if self.data2g => match v.parse::<f64>() {
                Ok(s) if s > 0.0 && s.is_finite() => {
                    self.timeout = (s < NO_IDLE_CLOSE_S).then_some(s);
                    let _ = self.ion.send(IonCmd::IdleClose(self.timeout));
                    self.say("OK");
                }
                _ => self.say("WRONG"),
            },
            ["TXPOWER"] if self.data2g => self.say(&format!("TXPOWER {:.1}", self.tx_db)),
            // Reduce-only: 0 is the operator's drive, and nothing above it.
            ["TXPOWER", v] if self.data2g => match v.parse::<f32>() {
                Ok(db) if (TX_POWER_MIN_DB..=0.0).contains(&db) => {
                    self.tx_db = db + 0.0;
                    let _ = self.ion.send(IonCmd::TxPowerDb(self.tx_db));
                    self.say("OK");
                }
                _ => self.say("WRONG"),
            },
            ["SN"] => match self.last_snr {
                Some(s) => self.say(&format!("SN {s:.1}")),
                None => self.say("WRONG"),
            },
            ["VERSION"] => self.say(&format!("VERSION IonHF {}", env!("CARGO_PKG_VERSION"))),
            ["BCAST", "OPEN", rest @ ..] => {
                self.bcast_client = true;
                let name: Vec<&str> = rest.iter().take_while(|w| **w != "FROM").copied().collect();
                let free = (1..=MAX_PORT).find(|p| !self.ports.contains_key(p));
                match (name.is_empty(), free) {
                    (false, Some(p)) => {
                        let name = name.join(" ");
                        self.open_port(p, &name);
                        self.note(format!("Winlink host: group {name} on KISS port {p}"));
                        self.say(&format!("BCAST PORT {p}"));
                    }
                    _ => self.say("WRONG"),
                }
            }
            ["BCAST", "CLOSE", n] => {
                self.bcast_client = true;
                match n.parse::<u8>() {
                    Ok(p) if p > 0 && self.ports.contains_key(&p) => {
                        self.close_port(p);
                        self.say("OK");
                    }
                    _ => self.say("WRONG"),
                }
            }
            ["BCAST", "MODE", n, ..] => {
                self.bcast_client = true;
                let open = n.parse::<u8>().is_ok_and(|p| self.ports.contains_key(&p));
                self.say(if open { "OK" } else { "WRONG" });
            }
            ["MODES"] => {
                self.say(ION_MODE);
                self.say("OK");
            }
            [w, ..]
                if w.starts_with("BW")
                    || matches!(
                        *w,
                        "COMPRESSION" | "CHAT" | "PUBLIC" | "CWID" | "WINLINK" | "P2P" | "CQFRAME"
                    ) =>
            {
                self.say("OK")
            }
            _ => {
                log::info!("Winlink host: unknown command {text:?}");
                self.say("WRONG");
            }
        }
    }

    /// DISCONNECTED, with `why` for a client that sent `DATA2G ON`.
    fn disconnected(&mut self, why: &str) {
        self.limbo = None;
        if self.data2g {
            self.say(&format!("DISCONNECTED {why}"));
        } else {
            self.say("DISCONNECTED");
        }
    }

    fn open_port(&mut self, p: u8, name: &str) {
        self.ports.insert(p, name.to_string());
        let _ = self.ion.send(IonCmd::GroupOpen(p, name.to_string()));
    }

    fn close_port(&mut self, p: u8) {
        if self.ports.remove(&p).is_some() {
            let _ = self.ion.send(IonCmd::GroupClose(p));
        }
    }

    fn relisten(&mut self) {
        if self.listen {
            let _ = self.ion.send(IonCmd::Listen);
        }
    }

    fn status(&mut self, s: &IonStatus) {
        if s.snr_db.is_some() {
            self.last_snr = s.snr_db;
        }
        self.status_limbo = s.limbo;
        let state = s.state;
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            match state {
                State::Connected if self.connected.is_none() => {
                    let pair = match self.calling.take() {
                        Some(dst) => (self.mycall.clone(), dst),
                        None => (s.peer.clone(), self.mycall.clone()),
                    };
                    self.say(&format!("CONNECTED {} {} 2300", pair.0, pair.1));
                    self.note(format!("Winlink host: connected {} → {}", pair.0, pair.1));
                    self.connected = Some(pair);
                    self.last_buffer = None;
                }
                State::Disconnected | State::Failed | State::Idle | State::Listening => {
                    let was_up = self.connected.take().is_some();
                    let was_calling = self.calling.take().is_some();
                    if was_up || was_calling {
                        // Idle or Listening: the link was stopped here.
                        self.disconnected(&end_words(s.end, state));
                        if state != State::Listening {
                            self.relisten();
                        }
                    }
                }
                _ => {}
            }
        }
        // A VARA client sees a cleaned limbo gone at once.
        let buffer = if self.data2g {
            s.pending
        } else {
            s.pending.saturating_sub(s.limbo_bytes)
        };
        if self.connected.is_some() && self.last_buffer != Some(buffer) {
            self.last_buffer = Some(buffer);
            self.say(&format!("BUFFER {buffer}"));
        }
        if s.on_air != self.last_ptt {
            self.last_ptt = s.on_air;
            self.say(if s.on_air { "PTT ON" } else { "PTT OFF" });
        }
        if s.busy != self.last_busy {
            self.last_busy = s.busy;
            self.say(if s.busy { "BUSY ON" } else { "BUSY OFF" });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (Host, Receiver<IonCmd>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ui, _) = crossbeam_channel::bounded(1);
        (Host::new(tx, ui), rx)
    }

    /// Command lines as the client sends them.
    fn feed(h: &mut Host, ls: &[&str]) {
        for l in ls {
            h.line.extend(l.as_bytes());
            h.line.push(b'\r');
        }
        h.commands();
    }

    /// What has come back since the last call.
    fn take(h: &mut Host) -> Vec<String> {
        h.commands();
        std::mem::take(&mut h.replies)
    }

    fn lines(h: &mut Host, ls: &[&str]) -> Vec<String> {
        feed(h, ls);
        take(h)
    }

    fn status(state: State, pending: usize, limbo: Option<(u64, u64, u64)>, limbo_bytes: usize) -> IonStatus {
        IonStatus {
            state,
            role: ion::link::Role::Sender,
            peer: "LOOP".into(),
            level: 0,
            snr_db: None,
            bytes_acked: 0,
            bytes_received: 0,
            text_acked: 0,
            text_started: 0,
            pending,
            data_bursts: 0,
            frames_resent: 0,
            increments_sent: 0,
            harq_recovered: 0,
            codewords_failed: 0,
            polls: 0,
            on_air: false,
            busy: false,
            connected_s: 0.0,
            target_level: 0,
            frames_sent: 0,
            control_bursts: 0,
            airtime_s: 0.0,
            bursts: Vec::new(),
            slot_tx: 0,
            slot_rx: 0,
            band: [0.0; 3],
            holding: false,
            end: None,
            limbo,
            limbo_bytes,
        }
    }

    fn sent_db(rx: &Receiver<IonCmd>) -> Vec<f32> {
        rx.try_iter()
            .filter_map(|c| match c {
                IonCmd::TxPowerDb(db) => Some(db),
                _ => None,
            })
            .collect()
    }

    /// Data2G's test_txpower_gets_and_sets_the_output_volume, reduce-only:
    /// anything over the operator's drive is WRONG, and nothing over 0 dB
    /// ever reaches the worker. The client leaving puts it back to 0.
    #[test]
    fn txpower_reduces_only() {
        let (mut h, rx) = host();
        let said = lines(&mut h, &["TXPOWER", "DATA2G ON", "TXPOWER", "TXPOWER -1.2", "TXPOWER"]);
        assert_eq!(said, ["WRONG", "OK", "TXPOWER 0.0", "OK", "TXPOWER -1.2"]);
        for b in [
            "-60.1", "20.5", "abc", "nan", "1 2", "0.1", "3", "20", "+0.05", "inf", "-inf", "1e9",
        ] {
            assert_eq!(lines(&mut h, &[&format!("TXPOWER {b}")]), ["WRONG"], "{b}");
        }
        assert_eq!(
            lines(&mut h, &["TXPOWER", "TXPOWER -60", "TXPOWER -0", "TXPOWER"]),
            ["TXPOWER -1.2", "OK", "OK", "TXPOWER 0.0"]
        );
        let db = sent_db(&rx);
        assert_eq!(db, [-1.2, -60.0, 0.0]);
        assert!(db.iter().all(|&d| d <= 0.0));
        lines(&mut h, &["TXPOWER -6"]);
        h.client_gone();
        assert_eq!(sent_db(&rx), [-6.0, 0.0]);
        assert_eq!(lines(&mut h, &["DATA2G ON", "TXPOWER"]), ["OK", "TXPOWER 0.0"]);
    }

    /// Data2G's test_timeout_sets_the_idle_close, with Ion's default (no
    /// idle close) as a large number; setting that number turns it off.
    #[test]
    fn timeout_gets_sets_and_resets() {
        let (mut h, rx) = host();
        let said = lines(
            &mut h,
            &[
                "TIMEOUT",
                "DATA2G ON",
                "TIMEOUT",
                "TIMEOUT 0",
                "TIMEOUT -5",
                "TIMEOUT inf",
                "TIMEOUT x",
                "TIMEOUT 20",
                "TIMEOUT",
            ],
        );
        let mut want = vec!["WRONG", "OK", "TIMEOUT 1000000000.0"];
        want.extend(["WRONG"; 4]);
        want.extend(["OK", "TIMEOUT 20.0"]);
        assert_eq!(said, want);
        assert_eq!(lines(&mut h, &["TIMEOUT 86400", "TIMEOUT"]), ["OK", "TIMEOUT 86400.0"]);
        assert_eq!(
            lines(&mut h, &["TIMEOUT 1000000000.0", "TIMEOUT"]),
            ["OK", "TIMEOUT 1000000000.0"]
        );
        lines(&mut h, &["TIMEOUT 20"]);
        h.client_gone();
        let set: Vec<Option<f64>> = rx
            .try_iter()
            .filter_map(|c| match c {
                IonCmd::IdleClose(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(set, [Some(20.0), Some(86400.0), None, Some(20.0), None]);
        assert_eq!(lines(&mut h, &["DATA2G ON", "TIMEOUT"]), ["OK", "TIMEOUT 1000000000.0"]);
    }

    fn clean_asked(rx: &Receiver<IonCmd>) -> Vec<(Option<u64>, bool)> {
        rx.try_iter()
            .filter_map(|c| match c {
                IonCmd::Clean { upto, vara } => Some((upto, vara)),
                _ => None,
            })
            .collect()
    }

    /// VARA's CLEANTXBUFFER replies (FAILED with its trailing space); the
    /// offset form is Data2G's only; a VARA client's BUFFER leaves the
    /// limbo out; commands sent meanwhile are answered after it, in order.
    #[test]
    fn cleantxbuffer_as_vara() {
        let (mut h, rx) = host();
        assert!(lines(&mut h, &["CLEANTXBUFFER", "SN", "CLEANTXBUFFER 5"]).is_empty());
        assert_eq!(clean_asked(&rx), [(None, true)]);
        h.ion_in(HostIn::Clean(Clean::Empty));
        assert_eq!(take(&mut h), ["CLEANTXBUFFER BUFFEREMPTY", "WRONG", "WRONG"]);
        feed(&mut h, &["CLEANTXBUFFER"]);
        h.ion_in(HostIn::Clean(Clean::Failed));
        assert_eq!(take(&mut h), ["CLEANTXBUFFER FAILED "]);
        h.connected = Some(("N0ABC".into(), "LOOP".into()));
        feed(&mut h, &["CLEANTXBUFFER"]);
        h.ion_in(HostIn::Clean(Clean::Done {
            drop: (900, 20_000),
            limbo: (0, 900),
        }));
        h.ion_in(HostIn::Status(status(State::Connected, 400, Some((0, 500, 900)), 400)));
        h.ion_in(HostIn::Settled(Settled {
            from: 0,
            split: 900,
            to: 900,
            unknown: false,
        }));
        assert_eq!(take(&mut h), ["CLEANTXBUFFER OK", "BUFFER 0"]);
        // An answer nobody waits for (late) is dropped.
        h.ion_in(HostIn::Clean(Clean::Empty));
        assert!(take(&mut h).is_empty());
    }

    /// DATA2G ON: host offsets, LATE, SETTLED, a second clean repeating an
    /// unsettled limbo, BUFFER counting the limbo, and ABORT with a limbo
    /// unsettled (Data2G's test_cleantxbuffer_then_abort): the limbo
    /// first, its fate unknown past what was acknowledged.
    #[test]
    fn cleantxbuffer_as_data2g() {
        let (mut h, rx) = host();
        feed(&mut h, &["MYCALL N0ABC", "DATA2G ON"]);
        h.ion_in(HostIn::Status(status(State::Connected, 20_000, None, 0)));
        assert_eq!(take(&mut h), ["OK", "OK", "CONNECTED LOOP N0ABC 2300", "BUFFER 20000"]);
        feed(
            &mut h,
            &[
                "CLEANTXBUFFER 0",
                "CLEANTXBUFFER 19000",
                "CLEANTXBUFFER",
                "CLEANTXBUFFER 1x",
                "CLEANTXBUFFER 1234567890123456789",
            ],
        );
        h.ion_in(HostIn::Clean(Clean::Late));
        h.commands();
        h.ion_in(HostIn::Clean(Clean::Done {
            drop: (1_800, 19_000),
            limbo: (0, 1_800),
        }));
        h.ion_in(HostIn::Status(status(
            State::Connected,
            2_800,
            Some((0, 600, 1_800)),
            1_200,
        )));
        h.commands();
        h.ion_in(HostIn::Clean(Clean::Done {
            drop: (1_800, 1_800),
            limbo: (0, 1_800),
        }));
        assert_eq!(
            take(&mut h),
            [
                "CLEANTXBUFFER LATE",
                "CLEANTXBUFFER OK 1800 19000 0 1800",
                "BUFFER 2800",
                "CLEANTXBUFFER OK 1800 1800 0 1800",
                "WRONG",
                "WRONG",
            ]
        );
        assert_eq!(
            clean_asked(&rx),
            [(Some(0), false), (Some(19_000), false), (None, false)]
        );
        h.ion_in(HostIn::Settled(Settled {
            from: 0,
            split: 1_800,
            to: 1_800,
            unknown: false,
        }));
        assert_eq!(take(&mut h), ["CLEANTXBUFFER SETTLED 0 1800 1800"]);

        feed(&mut h, &["CLEANTXBUFFER"]);
        h.ion_in(HostIn::Clean(Clean::Done {
            drop: (5_000, 9_000),
            limbo: (3_000, 5_000),
        }));
        h.ion_in(HostIn::Status(status(
            State::Connected,
            1_000,
            Some((3_000, 4_000, 5_000)),
            1_000,
        )));
        feed(&mut h, &["ABORT"]);
        assert_eq!(
            take(&mut h),
            [
                "CLEANTXBUFFER OK 5000 9000 3000 5000",
                "BUFFER 1000",
                "OK",
                "CLEANTXBUFFER SETTLED 3000 4000 5000 UNKNOWN",
                "DISCONNECTED ABORT",
            ]
        );
        // The worker's own word on it comes too late: dropped.
        h.ion_in(HostIn::Settled(Settled {
            from: 3_000,
            split: 3_000,
            to: 5_000,
            unknown: true,
        }));
        assert!(take(&mut h).is_empty());
    }

    /// DATA2G ON drops what the client writes before CONNECTED; a VARA
    /// client's goes to the link (VARA queues it).
    #[test]
    fn data2g_drops_data_before_connected() {
        for data2g in [false, true] {
            let (mut h, rx) = host();
            let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let mut client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (server, _) = l.accept().unwrap();
            server.set_nonblocking(true).unwrap();
            h.data = Some(server);
            if data2g {
                feed(&mut h, &["DATA2G ON"]);
            }
            client.write_all(b"early").unwrap();
            while !h.read_data() {}
            h.connected = Some(("N0ABC".into(), "LOOP".into()));
            client.write_all(b"late").unwrap();
            while !h.read_data() {}
            let sent: Vec<u8> = rx
                .try_iter()
                .flat_map(|c| match c {
                    IonCmd::Send(b) => b,
                    _ => Vec::new(),
                })
                .collect();
            assert_eq!(sent, if data2g { &b"late"[..] } else { b"earlylate" });
        }
    }

    #[test]
    fn idle_end_words() {
        assert_eq!(
            end_words(Some(End::Idle { confirmed: true }), State::Disconnected),
            "IDLE"
        );
        assert_eq!(
            end_words(Some(End::Idle { confirmed: false }), State::Disconnected),
            "IDLE unconfirmed"
        );
    }
}
