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
//! Ion Net (`docs/ion-net.md`, Host interface): `NET OPEN name` runs a net
//! as net control, `NET JOIN [name]` joins one (the first heard), each
//! answered `NET PORT n` (a KISS port, 1–15, beside the groups'), or
//! `WRONG` while a link is up or being called. Then `NET CHECKINS ON|OFF`,
//! `NET LINE text`, `NET TRAFFIC n R|W|P|E [SHORT]`, `NET OFFER` (the next
//! frame on the net's port is an object to offer) and `NET CLOSE` (net
//! control: close the net; member: leave), each `OK` or `WRONG`. Events:
//! `NET JOINED ncs net slot`, `NET IN call slot [VIA call]`,
//! `NET LINE call text`, `NET TRAFFIC call n R|W|P|E [SHORT]`,
//! `NET HEARD call up_db down_db` (net control, as the roster changes;
//! `-` not known yet), `NET MISSED call`, `NET RELAY call VIA call`,
//! `NET ROUTE call VIA call|DIRECT`, `NET OBJECT id [CACHED]` (`id`: 20
//! hex digits, `ObjectId`), `NET CLOSED` (`NET CLOSED LOST`: a member
//! heard nothing of net control for `NetConfig::lost_after_s`). Frames on
//! the net's port (KISS data) carry
//! `[to length][to][from length][from][data]`, both ways: from the host
//! `from` is empty and an empty `to` is the whole net (net control: a
//! bulletin; member: traffic for net control); to the host both are
//! filled in, an empty `to` was a bulletin and `#id` an object (after its
//! `NET OBJECT` line). Messages overheard between other stations are not
//! passed on. `CONNECT` is `WRONG` while a net is up. When the command
//! client goes the net keeps running (the Net screen shows it) and a new
//! client attaches to it with `NET OPEN` or `NET JOIN`.
//!
//! `DATA2G ON` (off again with `DATA2G OFF` or when the command client
//! leaves): `DISCONNECTED` says why, as Data2G's host does (its
//! `docs/arq.md` §9): `CLIENT`, `PEER`, `ABORT`, `LINK_LOST`,
//! `LINK_FAILED weak path` (the lifeline's hold ran out), `NO_ANSWER`,
//! with `unconfirmed` when our goodbye went unacknowledged. Data2G's other
//! `DATA2G ON` extensions are not offered (`WRONG`, as before).

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ion::link::State;
use ion::link::net::NetEvent;
use ion::link::station::End;

use crate::engine::UiEvent;
use ion::link::net::Traffic;

use crate::ion_engine::{IonCmd, IonStatus, precedence_of, traffic_words};

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
    /// Something happened in the net ([`NetEvent::Closed`] also when it
    /// was refused, left or dropped).
    Net(NetEvent),
}

/// A `NET` command.
#[derive(Debug, PartialEq)]
enum NetCmd {
    Open(String),
    Join(String),
    Checkins(bool),
    Line(String),
    Traffic(Traffic),
    Offer,
    Close,
}

/// A `NET` command line; `LINE` keeps its text as typed.
fn parse_net(text: &str) -> Option<NetCmd> {
    let up = text.to_ascii_uppercase();
    let words: Vec<&str> = up.split_whitespace().collect();
    match words.as_slice() {
        ["NET", "OPEN", name @ ..] if !name.is_empty() => Some(NetCmd::Open(name.join(" "))),
        ["NET", "JOIN", name @ ..] => Some(NetCmd::Join(name.join(" "))),
        ["NET", "CHECKINS", on @ ("ON" | "OFF")] => Some(NetCmd::Checkins(*on == "ON")),
        ["NET", "LINE", ..] => {
            let rest = text.trim().split_once(char::is_whitespace)?.1.trim_start();
            let line = rest.split_once(char::is_whitespace).map_or("", |(_, t)| t.trim());
            Some(NetCmd::Line(line.to_string()))
        }
        ["NET", "TRAFFIC", n, p, rest @ ..] if matches!(rest, [] | ["SHORT"]) => Some(NetCmd::Traffic(Traffic {
            items: n.parse().ok()?,
            precedence: precedence_of(p)?,
            short: !rest.is_empty(),
        })),
        ["NET", "OFFER"] => Some(NetCmd::Offer),
        ["NET", "CLOSE"] => Some(NetCmd::Close),
        _ => None,
    }
}

/// A frame on the net's KISS port: `[to length][to][from length][from][data]`.
pub(crate) fn net_frame(to: &str, from: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + to.len() + from.len() + data.len());
    for call in [to, from] {
        let b = &call.as_bytes()[..call.len().min(255)];
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out.extend_from_slice(data);
    out
}

/// (to, from, data) of a frame on the net's KISS port.
pub(crate) fn net_unframe(b: &[u8]) -> Option<(String, String, &[u8])> {
    let call = |b: &[u8]| -> Option<(String, usize)> {
        let n = *b.first()? as usize;
        let c = b.get(1..1 + n)?;
        c.is_ascii().then(|| (String::from_utf8_lossy(c).into_owned(), 1 + n))
    };
    let (to, a) = call(b)?;
    let (from, c) = call(&b[a..])?;
    Some((to, from, &b[a + c..]))
}

/// S/N for `NET HEARD`, `-` not known.
fn db_word(v: Option<f32>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1}"))
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

pub(crate) fn kiss_frame(command: u8, payload: &[u8]) -> Vec<u8> {
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
pub(crate) fn kiss_frames(buf: &mut Vec<u8>) -> Vec<(u8, Vec<u8>)> {
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
    listening: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HostHandle {
    /// The command and data ports are open (else still waiting for them).
    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Longest wait between tries at a busy host port.
const BIND_RETRY_MAX: Duration = Duration::from_secs(2);

fn listen(port: u16) -> std::io::Result<TcpListener> {
    let l = TcpListener::bind(("127.0.0.1", port))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Sleeps `d` in short steps; false once `stop` is set.
fn wait(stop: &AtomicBool, d: Duration) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !stop.load(Ordering::Relaxed)
}

/// Opens the two ports and serves one host at a time. Ports another
/// program holds (an Ion still closing, say) are tried again until they
/// are free or the handle is dropped.
pub fn spawn(port: u16, ion: Sender<IonCmd>, ui: Sender<UiEvent>) -> std::io::Result<HostHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let listening = Arc::new(AtomicBool::new(false));
    let (stop2, listening2) = (stop.clone(), listening.clone());
    let thread = std::thread::Builder::new().name("winlink-host".into()).spawn(move || {
        let mut retry = Duration::from_millis(250);
        let mut warned = false;
        let (cmd, data) = loop {
            match listen(port).and_then(|c| Ok((c, listen(port + 1)?))) {
                Ok(pair) => break pair,
                Err(e) => {
                    if !warned {
                        warned = true;
                        let _ = ui.try_send(UiEvent::Warning(format!("host port {port}: {e}; waiting")));
                    }
                    if !wait(&stop2, retry) {
                        return;
                    }
                    retry = (retry * 2).min(BIND_RETRY_MAX);
                }
            }
        };
        let kiss_port = port.checked_sub(200);
        let mut kiss = kiss_port.and_then(|p| listen(p).ok());
        if kiss.is_none() {
            log::info!("Winlink host: no KISS port {}", port.saturating_sub(200));
        }
        let (tx, rx) = crossbeam_channel::unbounded();
        let _ = ion.send(IonCmd::AttachHost(Some(tx)));
        listening2.store(true, Ordering::Relaxed);
        let _ = ui.try_send(UiEvent::Host(format!("Host 127.0.0.1:{port} / {}", port + 1)));
        let mut h = Host::new(ion.clone(), ui);
        let mut kiss_tried = Instant::now();
        while !stop2.load(Ordering::Relaxed) {
            if kiss.is_none() && kiss_port.is_some() && kiss_tried.elapsed() >= BIND_RETRY_MAX {
                kiss_tried = Instant::now();
                kiss = kiss_port.and_then(|p| listen(p).ok());
            }
            h.service(&cmd, &data, kiss.as_ref(), &rx);
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = ion.send(IonCmd::AttachHost(None));
    })?;
    Ok(HostHandle {
        stop,
        listening,
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
    /// The command client sent `DATA2G ON`: DISCONNECTED says why.
    data2g: bool,
    /// The command client's net: its KISS port, and whether we run it.
    net_port: Option<u8>,
    net_ncs: bool,
    /// A net is up (the client's, or one started on the screen).
    net_up: bool,
    /// Its net control, who objects come from.
    net_ncs_call: String,
    /// The next frame on the net's port is an object to offer.
    offer_next: bool,
    /// S/N last told with `NET HEARD`, rounded to dB, by call.
    net_heard: std::collections::BTreeMap<String, (Option<i32>, Option<i32>)>,
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
            net_port: None,
            net_ncs: false,
            net_up: false,
            net_ncs_call: String::new(),
            offer_next: false,
            net_heard: std::collections::BTreeMap::new(),
        }
    }

    fn note(&self, s: String) {
        let _ = self.ui.try_send(UiEvent::Host(s));
    }

    fn say(&mut self, line: &str) {
        if let Some(c) = self.cmd.as_mut() {
            let mut b = line.as_bytes().to_vec();
            b.push(b'\r');
            if c.write_all(&b).is_err() {
                self.cmd = None;
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
            self.cmd = Some(s);
            self.line.clear();
            self.note(format!("Host connected from {addr}"));
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
                Ok(0) => {
                    self.cmd = None;
                    self.note("Host disconnected".into());
                    // Its groups go with it ("KISS 0" stays with the KISS
                    // client).
                    self.bcast_client = false;
                    self.data2g = false;
                    self.forget_net();
                    let open: Vec<u8> = self.ports.keys().copied().filter(|&p| p != 0).collect();
                    for p in open {
                        self.close_port(p);
                    }
                }
                Ok(n) => self.line.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.cmd = None,
            }
        }
        while let Some(i) = self.line.iter().position(|&b| b == b'\r' || b == b'\n') {
            let l: Vec<u8> = self.line.drain(..=i).collect();
            let text = String::from_utf8_lossy(&l[..l.len() - 1]).trim().to_string();
            if !text.is_empty() {
                self.command(&text);
            }
        }
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
                KISS_DATA if self.net_port == Some(port) => self.net_frame_in(&payload),
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
        if let Some(d) = self.data.as_mut() {
            match d.read(&mut buf) {
                Ok(0) => self.data = None,
                Ok(n) => {
                    let _ = self.ion.send(IonCmd::Send(buf[..n].to_vec()));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => self.data = None,
            }
        }
        for m in rx.try_iter() {
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
                HostIn::Net(e) => self.net_event(e),
                _ => {}
            }
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
                    let _ = self.ion.send(IonCmd::GiveUp);
                }
                self.say("OK");
            }
            ["CONNECT", ..] if self.net_up || self.net_port.is_some() => self.say("WRONG"),
            ["CONNECT", src, dst, ..] => {
                if self.mycall.is_empty() {
                    self.mycall = src.to_string();
                }
                let dst = dst.split('-').next().unwrap_or(dst).to_string();
                self.calling = Some(dst.clone());
                let _ = self.ion.send(IonCmd::Connect(dst.clone()));
                self.note(format!("Host: calling {dst}"));
                self.say("OK");
            }
            ["DISCONNECT"] => {
                self.say("OK");
                if self.connected.is_some() {
                    let _ = self.ion.send(IonCmd::DisconnectWhenSent);
                } else {
                    // Nothing up (or still calling): give up now, and ID.
                    let _ = self.ion.send(IonCmd::GiveUp);
                    self.calling = None;
                    self.disconnected("CLIENT");
                    self.relisten();
                }
            }
            ["ABORT"] => {
                self.say("OK");
                let _ = self.ion.send(IonCmd::Stop);
                self.calling = None;
                if self.connected.take().is_some() {
                    self.disconnected("ABORT");
                }
                self.relisten();
            }
            ["DATA2G", on @ ("ON" | "OFF")] => {
                self.data2g = *on == "ON";
                self.say("OK");
            }
            ["SN"] => match self.last_snr {
                Some(s) => self.say(&format!("SN {s:.1}")),
                None => self.say("WRONG"),
            },
            ["VERSION"] => self.say(&format!("VERSION IonHF {}", env!("CARGO_PKG_VERSION"))),
            ["BCAST", "OPEN", rest @ ..] => {
                self.bcast_client = true;
                let name: Vec<&str> = rest.iter().take_while(|w| **w != "FROM").copied().collect();
                let free = (1..=MAX_PORT).find(|p| !self.ports.contains_key(p) && self.net_port != Some(*p));
                match (name.is_empty(), free) {
                    (false, Some(p)) => {
                        let name = name.join(" ");
                        self.open_port(p, &name);
                        self.note(format!("Host: group {name} on KISS port {p}"));
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
            ["NET", ..] => match parse_net(text) {
                Some(c) => self.net_command(c),
                None => self.say("WRONG"),
            },
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

    fn net_command(&mut self, c: NetCmd) {
        let ncs = matches!(c, NetCmd::Open(_));
        let ok = match c {
            NetCmd::Open(_) | NetCmd::Join(_) if self.connected.is_some() || self.calling.is_some() => false,
            NetCmd::Open(name) | NetCmd::Join(name) if self.net_port.is_none() => {
                let free = (1..=MAX_PORT).find(|p| !self.ports.contains_key(p));
                let Some(p) = free else {
                    self.say("WRONG");
                    return;
                };
                self.net_ncs = ncs;
                self.net_port = Some(p);
                self.net_heard.clear();
                let _ = self.ion.send(if self.net_ncs {
                    IonCmd::NetStart(name.clone())
                } else {
                    IonCmd::NetJoin(name.clone())
                });
                self.note(format!("Host: net {name} on KISS port {p}"));
                self.say(&format!("NET PORT {p}"));
                return;
            }
            NetCmd::Open(_) | NetCmd::Join(_) => {
                let p = self.net_port.unwrap_or_default();
                self.say(&format!("NET PORT {p}"));
                return;
            }
            NetCmd::Checkins(on) if self.net_port.is_some() && self.net_ncs => {
                let _ = self.ion.send(IonCmd::NetCheckins(on));
                true
            }
            NetCmd::Line(text) => {
                let _ = self.ion.send(IonCmd::NetLine(text));
                true
            }
            NetCmd::Traffic(t) => {
                let _ = self.ion.send(IonCmd::NetTraffic(t));
                true
            }
            NetCmd::Offer if self.net_port.is_some() && self.net_ncs => {
                self.offer_next = true;
                true
            }
            NetCmd::Close if self.net_port.is_some() => {
                let _ = self.ion.send(IonCmd::NetClose);
                true
            }
            _ => false,
        };
        self.say(if ok { "OK" } else { "WRONG" });
    }

    /// A frame from the host on the net's port.
    fn net_frame_in(&mut self, payload: &[u8]) {
        let Some((to, _, data)) = net_unframe(payload) else {
            log::info!("Winlink host: net frame without its header");
            return;
        };
        let cmd = if std::mem::take(&mut self.offer_next) {
            IonCmd::NetOffer(data.to_vec())
        } else if to.is_empty() {
            IonCmd::NetSend(data.to_vec())
        } else {
            IonCmd::NetSendTo(to, data.to_vec())
        };
        let _ = self.ion.send(cmd);
    }

    fn net_event(&mut self, e: NetEvent) {
        let Some(p) = self.net_port else { return };
        match e {
            NetEvent::Joined { ncs, net, slot } => {
                self.say(&format!("NET JOINED {ncs} {net} {slot}"));
                self.net_ncs_call = ncs;
            }
            NetEvent::Line { call, line } => self.say(format!("NET LINE {call} {line}").trim_end()),
            NetEvent::Traffic { call, traffic } => {
                self.say(&format!("NET TRAFFIC {call} {}", traffic_words(traffic)));
            }
            NetEvent::Object { id, data, cached } => {
                self.say(&format!("NET OBJECT {id}{}", if cached { " CACHED" } else { "" }));
                let frame = net_frame(&format!("#{id}"), &self.net_ncs_call, &data);
                self.kiss_out.extend(kiss_frame(p << 4 | KISS_DATA, &frame));
            }
            NetEvent::CheckedIn { call, slot, via } => {
                let via = via.map(|v| format!(" VIA {v}")).unwrap_or_default();
                self.say(&format!("NET IN {call} {slot}{via}"));
            }
            NetEvent::Message {
                from,
                to,
                data,
                overheard: false,
            } => {
                let frame = net_frame(to.as_deref().unwrap_or(""), &from, &data);
                self.kiss_out.extend(kiss_frame(p << 4 | KISS_DATA, &frame));
            }
            NetEvent::Message { .. } => {}
            NetEvent::Missed { call } => self.say(&format!("NET MISSED {call}")),
            NetEvent::Relayed { source, via } => self.say(&format!("NET RELAY {source} VIA {via}")),
            NetEvent::Routed { call, via } => match via {
                Some(v) => self.say(&format!("NET ROUTE {call} VIA {v}")),
                None => self.say(&format!("NET ROUTE {call} DIRECT")),
            },
            NetEvent::Closed => {
                self.say("NET CLOSED");
                self.forget_net();
            }
            NetEvent::Lost => {
                self.say("NET CLOSED LOST");
                self.forget_net();
            }
        }
    }

    fn forget_net(&mut self) {
        self.net_port = None;
        self.offer_next = false;
        self.net_heard.clear();
    }

    /// `NET HEARD` for each roster entry whose S/N changed by a dB or more.
    fn net_heard(&mut self, net: &crate::ion_engine::NetStatus) {
        let mut lines = Vec::new();
        for r in &net.roster {
            let now = (r.snr_up.map(|v| v.round() as i32), r.snr_down.map(|v| v.round() as i32));
            if now == (None, None) || self.net_heard.get(&r.call) == Some(&now) {
                continue;
            }
            self.net_heard.insert(r.call.clone(), now);
            lines.push(format!(
                "NET HEARD {} {} {}",
                r.call,
                db_word(r.snr_up),
                db_word(r.snr_down)
            ));
        }
        for l in lines {
            self.say(&l);
        }
    }

    /// DISCONNECTED, with `why` for a client that sent `DATA2G ON`.
    fn disconnected(&mut self, why: &str) {
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
        self.net_up = s.net.is_some();
        if let Some(n) = s.net.as_deref()
            && self.net_port.is_some()
        {
            self.net_ncs = n.ncs;
            if let Some(c) = &n.ncs_call {
                self.net_ncs_call.clone_from(c);
            }
            self.net_heard(n);
        }
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
                    self.note(format!("Host: connected {} to {}", pair.0, pair.1));
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
        if self.connected.is_some() && self.last_buffer != Some(s.pending) {
            self.last_buffer = Some(s.pending);
            self.say(&format!("BUFFER {}", s.pending));
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

/// A command port for a test whose data and KISS ports are free too:
/// another on every call, and in every test process, as test runs go side
/// by side (from other worktrees, say) and the user's Ion holds 8300.
#[cfg(test)]
pub(crate) fn free_port() -> u16 {
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    for _ in 0..2_500 {
        let k = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = 20_000 + (std::process::id().wrapping_add(k.wrapping_mul(7)) % 2_500) as u16 * 10;
        if [port - 200, port, port + 1]
            .iter()
            .all(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok())
        {
            return port;
        }
    }
    panic!("no free host ports");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_commands_parse() {
        use NetCmd::*;
        let t = |items, precedence, short| {
            Some(Traffic(ion::link::net::Traffic {
                items,
                precedence,
                short,
            }))
        };
        let cases = [
            ("NET OPEN tnares", Some(Open("TNARES".into()))),
            ("NET OPEN TN  ARES", Some(Open("TN ARES".into()))),
            ("NET OPEN", None),
            ("net join", Some(Join(String::new()))),
            ("NET JOIN TNARES", Some(Join("TNARES".into()))),
            ("NET CHECKINS ON", Some(Checkins(true))),
            ("NET CHECKINS off", Some(Checkins(false))),
            ("NET CHECKINS", None),
            (
                "NET LINE Trousdale County  ARES ",
                Some(Line("Trousdale County  ARES".into())),
            ),
            ("net line  w5abc portable", Some(Line("w5abc portable".into()))),
            ("NET LINE", Some(Line(String::new()))),
            (
                "NET TRAFFIC 2 P SHORT",
                t(2, ion::link::net::Precedence::Priority, true),
            ),
            ("NET TRAFFIC 0 R", t(0, ion::link::net::Precedence::Routine, false)),
            (
                "net traffic 7 e short",
                t(7, ion::link::net::Precedence::Emergency, true),
            ),
            ("NET TRAFFIC 1 W", t(1, ion::link::net::Precedence::Welfare, false)),
            ("NET TRAFFIC 1 X", None),
            ("NET TRAFFIC 1 PR", None),
            ("NET TRAFFIC 1 E LONG", None),
            ("NET TRAFFIC x R", None),
            ("NET TRAFFIC 300 R", None),
            ("NET TRAFFIC 1", None),
            ("NET OFFER", Some(Offer)),
            ("NET CLOSE", Some(Close)),
            ("NET CLOSE NOW", None),
            ("NET", None),
            ("NET PORT", None),
        ];
        for (line, want) in cases {
            assert_eq!(parse_net(line), want, "{line:?}");
        }
    }

    #[test]
    fn net_frame_header_round_trips() {
        let f = net_frame("N0AAA", "", b"hello");
        assert_eq!(f, b"\x05N0AAA\x00hello");
        assert_eq!(net_unframe(&f), Some(("N0AAA".into(), String::new(), &b"hello"[..])));
        let f = net_frame("", "KJ5ET", b"");
        assert_eq!(net_unframe(&f), Some((String::new(), "KJ5ET".into(), &b""[..])));
        // Through KISS, with bytes that need escaping.
        let data = [0xC0, 0xDB, 0x00, 0xDC, 7];
        let mut wire = kiss_frame(3 << 4 | KISS_DATA, &net_frame("AG5B", "N0JLF", &data));
        let got = kiss_frames(&mut wire);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 3 << 4);
        assert_eq!(net_unframe(&got[0].1), Some(("AG5B".into(), "N0JLF".into(), &data[..])));
        for bad in [&b""[..], b"\x05N0A", b"\x00", b"\x02AB\x03C", b"\x01\xff\x00"] {
            assert_eq!(net_unframe(bad), None, "{bad:?}");
        }
    }

    /// A command client on a socket pair, its lines as Ion says them.
    fn client_host() -> (Host, Receiver<IonCmd>, BufLines) {
        let (ion, ion_rx) = crossbeam_channel::unbounded();
        let (ui, _ui_rx) = crossbeam_channel::unbounded();
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (server, _) = l.accept().unwrap();
        client.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut h = Host::new(ion, ui);
        h.cmd = Some(server);
        (h, ion_rx, BufLines(std::io::BufReader::new(client)))
    }

    struct BufLines(std::io::BufReader<TcpStream>);

    impl BufLines {
        fn next(&mut self) -> String {
            use std::io::BufRead;
            let mut b = Vec::new();
            self.0.read_until(b'\r', &mut b).unwrap();
            String::from_utf8_lossy(&b).trim().to_string()
        }
    }

    /// The `NET` commands reach the Ion worker, frames on the net's port go
    /// both ways with their header, and the events come back as lines.
    #[test]
    fn net_commands_and_frames_reach_the_worker_and_back() {
        let (mut h, ion, mut said) = client_host();
        h.command("BCAST OPEN RAGTIME");
        assert_eq!(said.next(), "BCAST PORT 1");
        h.command("NET CHECKINS ON");
        assert_eq!(said.next(), "WRONG");
        h.command("NET OPEN TNARES");
        assert_eq!(said.next(), "NET PORT 2");
        h.command("NET OPEN TNARES");
        assert_eq!(said.next(), "NET PORT 2");
        h.command("BCAST OPEN OTHER");
        assert_eq!(said.next(), "BCAST PORT 3");
        for (c, reply) in [
            ("NET CHECKINS OFF", "OK"),
            ("NET LINE Trousdale County ARES", "OK"),
            ("NET TRAFFIC 2 P SHORT", "OK"),
            ("NET OFFER", "OK"),
            ("CONNECT N0ABC LOOP", "WRONG"),
            ("NET TRAFFIC 2", "WRONG"),
        ] {
            h.command(c);
            assert_eq!(said.next(), reply, "{c}");
        }
        h.net_frame_in(&net_frame("", "", b"the announcement"));
        h.net_frame_in(&net_frame("", "", b"a bulletin"));
        h.net_frame_in(&net_frame("KJ5ET", "", b"for KJ5ET"));
        h.net_frame_in(b"\x09short");
        let cmds: Vec<IonCmd> = ion.try_iter().filter(|c| !matches!(c, IonCmd::GroupOpen(..))).collect();
        let want = format!(
            "{:?}",
            [
                IonCmd::NetStart("TNARES".into()),
                IonCmd::NetCheckins(false),
                IonCmd::NetLine("Trousdale County ARES".into()),
                IonCmd::NetTraffic(Traffic {
                    items: 2,
                    precedence: ion::link::net::Precedence::Priority,
                    short: true,
                }),
                IonCmd::NetOffer(b"the announcement".to_vec()),
                IonCmd::NetSend(b"a bulletin".to_vec()),
                IonCmd::NetSendTo("KJ5ET".into(), b"for KJ5ET".to_vec()),
            ]
        );
        assert_eq!(format!("{cmds:?}"), want);

        let events = [
            NetEvent::CheckedIn {
                call: "KJ5ET".into(),
                slot: 1,
                via: None,
            },
            NetEvent::CheckedIn {
                call: "W5FAR".into(),
                slot: 2,
                via: Some("AG5B".into()),
            },
            NetEvent::Missed { call: "KJ5ET".into() },
            NetEvent::Relayed {
                source: "W5FAR".into(),
                via: "AG5B".into(),
            },
            NetEvent::Routed {
                call: "W5FAR".into(),
                via: None,
            },
            NetEvent::Message {
                from: "KJ5ET".into(),
                to: Some("N0JLF".into()),
                data: b"all well".to_vec(),
                overheard: false,
            },
            NetEvent::Message {
                from: "AG5B".into(),
                to: Some("N0JLF".into()),
                data: b"not ours".to_vec(),
                overheard: true,
            },
            NetEvent::Line {
                call: "KJ5ET".into(),
                line: "Trousdale County ARES".into(),
            },
            NetEvent::Line {
                call: "KJ5ET".into(),
                line: String::new(),
            },
            NetEvent::Traffic {
                call: "AG5B".into(),
                traffic: Traffic {
                    items: 3,
                    precedence: ion::link::net::Precedence::Emergency,
                    short: true,
                },
            },
        ];
        for e in events {
            h.net_event(e);
        }
        for want in [
            "NET IN KJ5ET 1",
            "NET IN W5FAR 2 VIA AG5B",
            "NET MISSED KJ5ET",
            "NET RELAY W5FAR VIA AG5B",
            "NET ROUTE W5FAR DIRECT",
            "NET LINE KJ5ET Trousdale County ARES",
            "NET LINE KJ5ET",
            "NET TRAFFIC AG5B 3 E SHORT",
        ] {
            assert_eq!(said.next(), want);
        }
        let mut out = std::mem::take(&mut h.kiss_out);
        let frames = kiss_frames(&mut out);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].0, 2 << 4);
        assert_eq!(
            net_unframe(&frames[0].1),
            Some(("N0JLF".into(), "KJ5ET".into(), &b"all well"[..]))
        );

        // An object, as a member gets it: its line, then its frame.
        h.net_event(NetEvent::Joined {
            ncs: "N0JLF".into(),
            net: "TNARES".into(),
            slot: 3,
        });
        assert_eq!(said.next(), "NET JOINED N0JLF TNARES 3");
        let data = b"# TNARES\nFrequency 3.975 MHz".to_vec();
        let id = ion::link::net::ObjectId::of(&data);
        for cached in [false, true] {
            h.net_event(NetEvent::Object {
                id,
                data: data.clone(),
                cached,
            });
        }
        assert_eq!(said.next(), format!("NET OBJECT {id}"));
        assert_eq!(said.next(), format!("NET OBJECT {id} CACHED"));
        assert_eq!(id.to_string().len(), 20);
        let mut out = std::mem::take(&mut h.kiss_out);
        let frames = kiss_frames(&mut out);
        assert_eq!(frames.len(), 2, "{frames:?}");
        let to = format!("#{id}");
        assert_eq!(net_unframe(&frames[1].1), Some((to, "N0JLF".into(), &data[..])));

        let mut net = crate::ion_engine::NetStatus {
            ncs: true,
            ..Default::default()
        };
        let entry = |up, down| ion::link::net::RosterEntry {
            slot: 1,
            call: "KJ5ET".into(),
            via: None,
            snr_up: up,
            snr_down: down,
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
            holds: 0,
        };
        for (up, down) in [(Some(4.2), None), (Some(4.4), None), (Some(5.6), Some(-3.0))] {
            net.roster = vec![entry(up, down)];
            h.net_heard(&net);
        }
        assert_eq!(said.next(), "NET HEARD KJ5ET 4.2 -");
        assert_eq!(said.next(), "NET HEARD KJ5ET 5.6 -3.0");

        h.net_event(NetEvent::Closed);
        assert_eq!(said.next(), "NET CLOSED");
        h.command("NET JOIN");
        assert_eq!(said.next(), "NET PORT 2");
        h.net_event(NetEvent::Lost);
        assert_eq!(said.next(), "NET CLOSED LOST");
        h.net_event(NetEvent::Missed { call: "KJ5ET".into() });
        h.command("NET CLOSE");
        assert_eq!(said.next(), "WRONG");
        h.command("NET JOIN");
        assert_eq!(said.next(), "NET PORT 2");
        assert!(matches!(ion.try_iter().last(), Some(IonCmd::NetJoin(n)) if n.is_empty()));
    }

    /// While a link is called or up, no net; while a net is up, no link.
    #[test]
    fn a_net_and_a_link_exclude_each_other() {
        let (mut h, ion, mut said) = client_host();
        h.command("CONNECT N0ABC LOOP");
        assert_eq!(said.next(), "OK");
        h.command("NET JOIN");
        assert_eq!(said.next(), "WRONG");
        h.command("ABORT");
        assert_eq!(said.next(), "OK");
        assert!(ion.try_iter().any(|c| matches!(c, IonCmd::Connect(_))));
        h.command("NET JOIN");
        assert_eq!(said.next(), "NET PORT 1");
        h.command("CONNECT N0ABC LOOP");
        assert_eq!(said.next(), "WRONG");
        assert!(!ion.try_iter().any(|c| matches!(c, IonCmd::Connect(_))));
    }

    /// A port another program holds (as a closing Ion would): the host
    /// waits, says so once, keeps its handle, and opens the ports once they
    /// are free.
    #[test]
    fn a_busy_port_is_waited_for() {
        let port = free_port();
        for held in [port, port + 1] {
            let blocker = TcpListener::bind(("127.0.0.1", held)).unwrap();
            let (ion, ion_rx) = crossbeam_channel::unbounded();
            let (ui, ui_rx) = crossbeam_channel::unbounded();
            let host = spawn(port, ion, ui).unwrap();
            std::thread::sleep(Duration::from_millis(800));
            assert!(!host.listening(), "listening beside port {held}'s holder");
            let other = if held == port { port + 1 } else { port };
            assert!(
                TcpStream::connect(("127.0.0.1", other)).is_err(),
                "port {other} left open"
            );
            let warnings = ui_rx
                .try_iter()
                .filter(|e| matches!(e, UiEvent::Warning(w) if w.contains("waiting")))
                .count();
            assert_eq!(warnings, 1);
            assert!(!ion_rx.try_iter().any(|c| matches!(c, IonCmd::AttachHost(Some(_)))));
            drop(blocker);
            let end = Instant::now() + Duration::from_secs(5);
            while !host.listening() {
                assert!(Instant::now() < end, "never opened port {port} after {held} was freed");
                std::thread::sleep(Duration::from_millis(10));
            }
            TcpStream::connect(("127.0.0.1", port)).unwrap();
            TcpStream::connect(("127.0.0.1", port + 1)).unwrap();
            assert!(ion_rx.try_iter().any(|c| matches!(c, IonCmd::AttachHost(Some(_)))));
            drop(host);
        }
    }

    /// Turning the host off while it waits ends it at once.
    #[test]
    fn a_waiting_host_stops_promptly() {
        let port = free_port();
        let _blocker = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let (ion, _ion_rx) = crossbeam_channel::unbounded();
        let (ui, _ui_rx) = crossbeam_channel::unbounded();
        let host = spawn(port, ion, ui).unwrap();
        std::thread::sleep(Duration::from_millis(3_000));
        let t = Instant::now();
        drop(host);
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
    }

    /// Ion closes first while a client is connected, which leaves the
    /// connection in TIME_WAIT on the host port; a new host opens the port
    /// at once, without waiting.
    #[test]
    fn a_port_in_time_wait_reopens_at_once() {
        let port = free_port();
        let (ion, _ion_rx) = crossbeam_channel::unbounded();
        let (ui, ui_rx) = crossbeam_channel::unbounded();
        let host = spawn(port, ion.clone(), ui.clone()).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !host.listening() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut cmd = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut data = TcpStream::connect(("127.0.0.1", port + 1)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        drop(host);
        let mut b = [0u8; 64];
        for s in [&mut cmd, &mut data] {
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            while matches!(s.read(&mut b), Ok(n) if n > 0) {}
        }
        drop((cmd, data));
        let host = spawn(port, ion, ui).unwrap();
        let end = Instant::now() + Duration::from_millis(500);
        while !host.listening() {
            assert!(Instant::now() < end, "port {port} not reopened");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!ui_rx.try_iter().any(|e| matches!(e, UiEvent::Warning(_))));
    }
}
