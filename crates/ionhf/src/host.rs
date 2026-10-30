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

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ion::link::State;

use crate::engine::UiEvent;
use crate::ion_engine::{IonCmd, IonStatus};

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
                    self.say("DISCONNECTED");
                    self.relisten();
                }
            }
            ["ABORT"] => {
                self.say("OK");
                let _ = self.ion.send(IonCmd::Stop);
                self.calling = None;
                if self.connected.take().is_some() {
                    self.say("DISCONNECTED");
                }
                self.relisten();
            }
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
                        self.say("DISCONNECTED");
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
