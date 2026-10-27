//! End to end: the DSP thread, the loopback radio with its "LOOP" station,
//! the Ion worker and the host interface, no window. Runs in real time, so
//! each test takes several seconds.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ion::link::State;

use crate::engine::{Engine, Msg, SAMPLE_RATE, Shared, UiEvent};
use crate::ion_engine::{self, IonCmd, IonEvent, IonHandle};
use crate::radio::{self, LoopCmd, RadioThread};

const CALL: &str = "N0ABC";

struct Rig {
    msgs: Sender<Msg>,
    ui_tx: Sender<UiEvent>,
    events: Receiver<UiEvent>,
    radio: RadioThread,
    shared: Arc<Shared>,
    engine: Option<std::thread::JoinHandle<()>>,
    keyed: bool,
    saw_keyed: bool,
    ion_rx: Vec<u8>,
    ion_notes: Vec<String>,
    ion_state: Option<State>,
}

impl Rig {
    fn new() -> Rig {
        let (rx_prod, rx_cons) = rtrb::RingBuffer::<f32>::new(2 * SAMPLE_RATE);
        let (tx_prod, tx_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE);
        let shared = Arc::new(Shared::default());
        let (ui_tx, ui_rx) = crossbeam_channel::bounded(16_384);
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (_spec_tx, spec_rx) = crossbeam_channel::bounded(32);
        let radio = radio::spawn_loopback(20.0, rx_prod, tx_cons, shared.clone(), ui_tx.clone(), Box::new(|| {}));
        let engine = Engine::new(
            rx_cons,
            tx_prod,
            shared.clone(),
            radio.ctl.clone(),
            ui_tx.clone(),
            msg_rx,
            spec_rx,
            Box::new(|| {}),
        );
        let engine = std::thread::spawn(move || engine.run());
        Rig {
            msgs: msg_tx,
            ui_tx,
            events: ui_rx,
            radio,
            shared,
            engine: Some(engine),
            keyed: false,
            saw_keyed: false,
            ion_rx: Vec::new(),
            ion_notes: Vec::new(),
            ion_state: None,
        }
    }

    /// The Ion worker on this rig, with LOOP on the air.
    fn ion(&self) -> IonHandle {
        let h = ion_engine::spawn(CALL.into(), self.msgs.clone(), self.ui_tx.clone(), Box::new(|| {}));
        self.radio.ctl.loopback().unwrap().send(LoopCmd::IonPeer(true)).unwrap();
        self.msgs.send(Msg::Ion(Some(h.audio.clone()))).unwrap();
        h
    }

    /// Pumps events until `done` or the timeout.
    fn run_until(&mut self, timeout: Duration, mut done: impl FnMut(&Rig) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            while let Ok(ev) = self.events.try_recv() {
                match ev {
                    UiEvent::Status(s) => {
                        self.saw_keyed |= s.keyed;
                        self.keyed = s.keyed;
                    }
                    UiEvent::Ion(IonEvent::Received(b)) => self.ion_rx.extend(b),
                    UiEvent::Ion(IonEvent::Note(n)) => self.ion_notes.push(n),
                    UiEvent::Ion(IonEvent::Status(s)) => self.ion_state = Some(s.state),
                    _ => {}
                }
            }
            if done(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn id_sent(&self) -> bool {
        self.ion_notes.iter().any(|n| n.contains("CW ID"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.msgs.send(Msg::Shutdown);
        if let Some(h) = self.engine.take() {
            let _ = h.join();
        }
        self.radio.stop();
    }
}

/// A program on the host interface's command and data ports.
struct HostClient {
    cmd: TcpStream,
    lines: BufReader<TcpStream>,
    data: TcpStream,
    said: Vec<String>,
}

impl HostClient {
    fn connect(port: u16) -> HostClient {
        let cmd = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let data = TcpStream::connect(("127.0.0.1", port + 1)).unwrap();
        cmd.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        data.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        HostClient {
            lines: BufReader::new(cmd.try_clone().unwrap()),
            cmd,
            data,
            said: Vec::new(),
        }
    }

    fn send(&mut self, line: &str) {
        self.cmd.write_all(format!("{line}\r").as_bytes()).unwrap();
    }

    /// Reads command-port lines (CR-terminated) into `said`, pumping the
    /// rig, until `want(said)`.
    fn read_until(&mut self, rig: &mut Rig, secs: u64, want: impl Fn(&[String]) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            rig.run_until(Duration::from_millis(50), |_| false);
            let mut buf = Vec::new();
            if let Ok(n) = self.lines.read_until(b'\r', &mut buf)
                && n > 0
            {
                self.said.push(String::from_utf8_lossy(&buf).trim().to_string());
                if want(&self.said) {
                    return true;
                }
            }
        }
        false
    }

    /// Reads the data port, pumping the rig, until `want` has arrived.
    fn data_until(&mut self, rig: &mut Rig, secs: u64, want: &[u8]) -> Vec<u8> {
        let mut got = Vec::new();
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end && !got.windows(want.len()).any(|x| x == want) {
            rig.run_until(Duration::from_millis(50), |_| false);
            let mut b = [0u8; 256];
            if let Ok(n) = self.data.read(&mut b) {
                got.extend_from_slice(&b[..n]);
            }
        }
        got
    }
}

fn last_is(s: &[String], line: &str) -> bool {
    s.last().is_some_and(|l| l == line)
}

#[test]
fn ion_connects_to_the_loop_station_and_chats() {
    let mut rig = Rig::new();
    let h = rig.ion();
    h.cmds.send(IonCmd::Connect("LOOP".into())).unwrap();
    h.cmds.send(IonCmd::Send(b"hello loop\n".to_vec())).unwrap();
    let want = b"LOOP copies: hello loop\n";
    let ok = rig.run_until(Duration::from_secs(60), |r| {
        r.ion_rx.windows(want.len()).any(|w| w == want)
    });
    assert!(
        ok,
        "no echo; got {:?}; notes {:?}",
        String::from_utf8_lossy(&rig.ion_rx),
        rig.ion_notes
    );
    assert!(rig.saw_keyed, "we never transmitted");
    assert_eq!(rig.shared.underruns.load(Ordering::Relaxed), 0);
    h.cmds.send(IonCmd::Disconnect).unwrap();
    let ok = rig.run_until(Duration::from_secs(30), |r| r.ion_state == Some(State::Disconnected));
    assert!(ok, "state {:?}; notes {:?}", rig.ion_state, rig.ion_notes);
    // We closed the link, so our CW ID goes out at once; Stop cuts it.
    let ok = rig.run_until(Duration::from_secs(20), |r| r.id_sent() && r.keyed);
    assert!(ok, "no CW ID on the air; notes {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::Stop).unwrap();
    let ok = rig.run_until(Duration::from_secs(1), |r| !r.keyed);
    assert!(ok, "Stop did not unkey the CW ID");
}

/// Abort (Stop, then Listen with answering on) mid-link: the transmitter
/// goes quiet at once and stays quiet, no CW ID.
#[test]
fn ion_abort_mid_link_keys_nothing_more() {
    let mut rig = Rig::new();
    let h = rig.ion();
    h.cmds.send(IonCmd::Connect("LOOP".into())).unwrap();
    h.cmds.send(IonCmd::Send(vec![b'x'; 20_000])).unwrap();
    let ok = rig.run_until(Duration::from_secs(60), |r| {
        r.ion_state == Some(State::Connected) && r.keyed
    });
    assert!(ok, "never sending; notes {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::Stop).unwrap();
    h.cmds.send(IonCmd::Listen).unwrap();
    let ok = rig.run_until(Duration::from_millis(500), |r| !r.keyed);
    assert!(ok, "Abort did not unkey");
    rig.run_until(Duration::from_secs(15), |r| r.keyed);
    assert!(!rig.keyed, "keyed again after Abort; notes {:?}", rig.ion_notes);
    assert!(!rig.id_sent(), "CW ID after Abort: {:?}", rig.ion_notes);
}

/// Disconnect with a transfer still queued: the link ends within seconds
/// (the rest is dropped) and our CW ID signs off.
#[test]
fn ion_disconnect_mid_transfer_ends_promptly_and_ids() {
    let mut rig = Rig::new();
    let h = rig.ion();
    h.cmds.send(IonCmd::Connect("LOOP".into())).unwrap();
    h.cmds.send(IonCmd::Send(vec![b'x'; 20_000])).unwrap();
    let ok = rig.run_until(Duration::from_secs(60), |r| r.ion_state == Some(State::Connected));
    assert!(ok, "never connected; notes {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::Disconnect).unwrap();
    let ok = rig.run_until(Duration::from_secs(40), |r| r.ion_state == Some(State::Disconnected));
    assert!(ok, "still {:?}; notes {:?}", rig.ion_state, rig.ion_notes);
    let ok = rig.run_until(Duration::from_secs(20), Rig::id_sent);
    assert!(ok, "no CW ID; notes {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::Stop).unwrap();
}

/// A host program (standing in for Pat) drives Ion through the VARA-style
/// interface: connect to LOOP, send a line, read the echo, wait for
/// BUFFER 0, disconnect.
#[test]
fn winlink_host_drives_a_ion_link() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let port = 18_300;
    let _host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).expect("ports free");
    let mut c = HostClient::connect(port);
    for l in [
        "VERSION",
        &format!("MYCALL {CALL}"),
        "BW2300",
        "COMPRESSION TEXT",
        "P2P SESSION",
        "LISTEN OFF",
        "FOOBAR",
    ] {
        c.send(l);
    }
    assert!(c.read_until(&mut rig, 5, |s| last_is(s, "WRONG")), "{:?}", c.said);
    assert!(c.said.iter().any(|l| l.starts_with("VERSION")), "{:?}", c.said);
    assert_eq!(c.said.iter().filter(|l| *l == "OK").count(), 5, "{:?}", c.said);

    c.send(&format!("CONNECT {CALL} LOOP"));
    let up = |s: &[String]| s.last().is_some_and(|l| l.starts_with("CONNECTED"));
    assert!(c.read_until(&mut rig, 60, up), "{:?}", c.said);
    assert!(c.said.contains(&format!("CONNECTED {CALL} LOOP 2300")), "{:?}", c.said);
    c.data.write_all(b"hello loop\n").unwrap();
    let want = b"LOOP copies: hello loop\n";
    let got = c.data_until(&mut rig, 60, want);
    assert!(
        got.windows(want.len()).any(|x| x == want),
        "data port got {:?}; said {:?}",
        String::from_utf8_lossy(&got),
        c.said
    );
    assert!(c.read_until(&mut rig, 30, |s| last_is(s, "BUFFER 0")), "{:?}", c.said);
    assert!(
        c.said.iter().any(|l| l == "PTT ON") && c.said.iter().any(|l| l == "PTT OFF"),
        "{:?}",
        c.said
    );

    c.send("DISCONNECT");
    assert!(
        c.read_until(&mut rig, 60, |s| last_is(s, "DISCONNECTED")),
        "{:?}",
        c.said
    );
}

/// A client made for Data2G's host (Ragtime) comes up on Ion: it opens the
/// KISS port beside the command and data ports, sends its start-up
/// commands, gets its broadcast group and Ion's one mode, holds a session,
/// and (with `DATA2G ON`) is told why it ended.
#[test]
fn ragtime_style_client_comes_up_and_holds_a_session() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let port = 18_400;
    let _host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).expect("ports free");
    let mut kiss = TcpStream::connect(("127.0.0.1", port - 200)).expect("KISS port");
    let mut c = HostClient::connect(port);
    // Ragtime's start-up, in its order; then its KISS persistence frame.
    for l in [
        &format!("MYCALL {CALL}"),
        "CHAT ON",
        "DATA2G ON",
        "LISTEN ON",
        "BW2300",
        &format!("BCAST OPEN RAGTIME FROM {CALL}"),
        "MODES",
    ] {
        c.send(l);
    }
    kiss.write_all(&[0xC0, 0x02, 191, 0xC0]).unwrap();
    assert!(c.read_until(&mut rig, 5, |s| s.len() >= 8), "{:?}", c.said);
    assert_eq!(
        c.said[..8],
        [
            "OK",
            "OK",
            "OK",
            "OK",
            "OK",
            "BCAST PORT 1",
            "MODE ION 2400 255 16 3.0 30.0",
            "OK"
        ],
        "{:?}",
        c.said
    );

    c.send(&format!("CONNECT {CALL} LOOP"));
    let up = |s: &[String]| s.iter().any(|l| l.starts_with("CONNECTED"));
    assert!(c.read_until(&mut rig, 60, up), "{:?}", c.said);
    c.data.write_all(b"hello loop\n").unwrap();
    let want = b"LOOP copies: hello loop\n";
    let got = c.data_until(&mut rig, 60, want);
    assert!(
        got.windows(want.len()).any(|x| x == want),
        "data port got {:?}",
        String::from_utf8_lossy(&got)
    );
    c.send("SN");
    let sn = |s: &[String]| s.last().is_some_and(|l| l.starts_with("SN "));
    assert!(c.read_until(&mut rig, 5, sn), "{:?}", c.said);
    c.send("DISCONNECT");
    assert!(
        c.read_until(&mut rig, 60, |s| last_is(s, "DISCONNECTED CLIENT")),
        "{:?}",
        c.said
    );
}

/// 200 lines of 100 bytes: line `i` starts at offset `100 i`.
fn numbered_lines() -> Vec<u8> {
    (0..200)
        .flat_map(|i| format!("{i:05} {}\n", "x".repeat(93)).into_bytes())
        .collect()
}

/// What LOOP sends back for `sent`: each whole line, prefixed.
fn loop_echo(sent: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in sent.split_inclusive(|&b| b == b'\n').filter(|l| l.ends_with(b"\n")) {
        out.extend(b"LOOP copies: ");
        out.extend(l);
    }
    out
}

/// Waits for a line starting with `head` said after line `from`.
fn said_after(c: &HostClient, from: usize, head: &str) -> Option<String> {
    c.said[from..].iter().find(|l| l.starts_with(head)).cloned()
}

/// Data2G's host extensions as Ragtime uses them, against LOOP: TIMEOUT
/// and TXPOWER at start-up (TXPOWER only down), data written before
/// CONNECTED dropped, and Data2G's mid-burst clean (its
/// test_cleantxbuffer_mid_burst): LATE for an offset already on air, then a
/// clean that keeps the last 1000 bytes. The burst on air settles
/// delivered, and LOOP echoes exactly the bytes before the limbo's end and
/// the kept tail.
#[test]
fn data2g_client_cleans_mid_burst() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let port = 18_910;
    let _host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).expect("ports free");
    let mut c = HostClient::connect(port);
    for l in [
        &format!("MYCALL {CALL}"),
        "DATA2G ON",
        "TIMEOUT",
        "TIMEOUT 86400",
        "TIMEOUT",
        "TXPOWER",
        "TXPOWER -3",
        "TXPOWER 3",
        "TXPOWER",
        "TXPOWER 0",
        "CLEANTXBUFFER",
    ] {
        c.send(l);
    }
    assert!(c.read_until(&mut rig, 5, |s| s.len() >= 11), "{:?}", c.said);
    assert_eq!(
        c.said[..11],
        [
            "OK",
            "OK",
            "TIMEOUT 1000000000.0",
            "OK",
            "TIMEOUT 86400.0",
            "TXPOWER 0.0",
            "OK",
            "WRONG",
            "TXPOWER -3.0",
            "OK",
            "CLEANTXBUFFER BUFFEREMPTY"
        ]
    );
    c.data.write_all(b"early\n").unwrap();
    c.send(&format!("CONNECT {CALL} LOOP"));
    let up = |s: &[String]| s.iter().any(|l| l.starts_with("CONNECTED"));
    assert!(c.read_until(&mut rig, 60, up), "{:?}", c.said);
    let sent = numbered_lines();
    c.data.write_all(&sent).unwrap();
    let n0 = c.said.len();
    assert!(
        c.read_until(&mut rig, 30, |s| s[n0..].iter().any(|l| l == "PTT ON")),
        "{:?}",
        c.said
    );
    c.send("CLEANTXBUFFER 0");
    c.send("CLEANTXBUFFER 19000");
    let answered = |s: &[String]| s[n0..].iter().filter(|l| l.starts_with("CLEANTXBUFFER")).count() >= 2;
    assert!(c.read_until(&mut rig, 5, answered), "{:?}", c.said);
    let replies: Vec<&String> = c.said[n0..].iter().filter(|l| l.starts_with("CLEANTXBUFFER")).collect();
    assert_eq!(replies[0], "CLEANTXBUFFER LATE");
    let w: Vec<u64> = replies[1]
        .strip_prefix("CLEANTXBUFFER OK ")
        .unwrap_or_else(|| panic!("{:?}", c.said))
        .split(' ')
        .map(|x| x.parse().unwrap())
        .collect();
    let (drop_from, limbo_to) = (w[0], w[3]);
    assert_eq!(w[1], 19_000);
    assert!(w[2] < limbo_to && limbo_to == drop_from, "{w:?}");
    let settled = format!("CLEANTXBUFFER SETTLED {} {limbo_to} {limbo_to}", w[2]);
    assert!(c.read_until(&mut rig, 90, |s| s.contains(&settled)), "{:?}", c.said);
    let kept = [&sent[..limbo_to as usize], &sent[19_000..]].concat();
    let echo = loop_echo(&kept);
    let got = c.data_until(&mut rig, 180, b"LOOP copies: 00199");
    let got = [got, c.data_until(&mut rig, 1, b"\n")].concat();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&echo),
        "said {:?}",
        c.said
    );
    assert!(c.read_until(&mut rig, 30, |s| last_is(s, "BUFFER 0")), "{:?}", c.said);
    c.send("DISCONNECT");
    assert!(
        c.read_until(&mut rig, 60, |s| last_is(s, "DISCONNECTED CLIENT")),
        "{:?}",
        c.said
    );
}

/// A VARA client's CLEANTXBUFFER mid-burst: `CLEANTXBUFFER OK`, BUFFER 0
/// at once (the burst on air still goes, but it counts as gone), no
/// offsets and no SETTLED; LOOP echoes a prefix of what was written.
#[test]
fn vara_client_cleans_mid_burst() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let port = 18_920;
    let _host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).expect("ports free");
    let mut c = HostClient::connect(port);
    c.send(&format!("MYCALL {CALL}"));
    c.send(&format!("CONNECT {CALL} LOOP"));
    let up = |s: &[String]| s.iter().any(|l| l.starts_with("CONNECTED"));
    assert!(c.read_until(&mut rig, 60, up), "{:?}", c.said);
    let sent = numbered_lines();
    c.data.write_all(&sent).unwrap();
    let n0 = c.said.len();
    assert!(
        c.read_until(&mut rig, 30, |s| s[n0..].iter().any(|l| l == "PTT ON")),
        "{:?}",
        c.said
    );
    let n1 = c.said.len();
    c.send("CLEANTXBUFFER 5");
    c.send("CLEANTXBUFFER");
    let ok_at = |s: &[String]| s[n1..].iter().position(|l| l == "CLEANTXBUFFER OK").map(|i| n1 + i);
    let answered = |s: &[String]| ok_at(s).is_some_and(|i| said_after_in(s, i, "BUFFER"));
    assert!(c.read_until(&mut rig, 5, answered), "{:?}", c.said);
    let i = ok_at(&c.said).unwrap();
    assert!(c.said[n1..i].iter().any(|l| l == "WRONG"), "{:?}", c.said);
    let buffer = c.said[i..].iter().find(|l| l.starts_with("BUFFER"));
    assert_eq!(buffer.map(String::as_str), Some("BUFFER 0"), "{:?}", c.said);
    let mut got = c.data_until(&mut rig, 120, b"\n");
    loop {
        let more = c.data_until(&mut rig, 15, b"\x00");
        if more.is_empty() {
            break;
        }
        got.extend(more);
    }
    let echo = loop_echo(&sent);
    assert!(
        !got.is_empty() && echo.starts_with(&got),
        "{}",
        String::from_utf8_lossy(&got)
    );
    assert!(got.len() < echo.len());
    assert!(said_after(&c, 0, "CLEANTXBUFFER SETTLED").is_none(), "{:?}", c.said);
    c.send("DISCONNECT");
    assert!(
        c.read_until(&mut rig, 60, |s| last_is(s, "DISCONNECTED")),
        "{:?}",
        c.said
    );
}

fn said_after_in(s: &[String], from: usize, head: &str) -> bool {
    s[from..].iter().any(|l| l.starts_with(head))
}

/// TIMEOUT 20: a quiet link closes about 20 s after the last data
/// (`DISCONNECTED IDLE`); the setting goes when the client does.
#[test]
fn data2g_timeout_closes_an_idle_link() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let port = 18_930;
    let _host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).expect("ports free");
    let mut c = HostClient::connect(port);
    for l in [&format!("MYCALL {CALL}"), "DATA2G ON", "TIMEOUT 20"] {
        c.send(l);
    }
    c.send(&format!("CONNECT {CALL} LOOP"));
    let up = |s: &[String]| s.iter().any(|l| l.starts_with("CONNECTED"));
    assert!(c.read_until(&mut rig, 60, up), "{:?}", c.said);
    assert_eq!(c.said[..4], ["OK", "OK", "OK", "OK"]);
    c.data.write_all(b"hi\n").unwrap();
    let got = c.data_until(&mut rig, 60, b"LOOP copies: hi\n");
    assert!(got.ends_with(b"LOOP copies: hi\n"), "{:?}", c.said);
    let quiet = Instant::now();
    assert!(
        c.read_until(&mut rig, 90, |s| s.iter().any(|l| l.starts_with("DISCONNECTED"))),
        "{:?}",
        c.said
    );
    assert_eq!(said_after(&c, 0, "DISCONNECTED").as_deref(), Some("DISCONNECTED IDLE"));
    let t = quiet.elapsed().as_secs_f64();
    assert!(t > 12.0 && t < 60.0, "closed {t:.0} s after the echo");
    drop(c);
    let mut c = HostClient::connect(port);
    c.send("DATA2G ON");
    c.send("TIMEOUT");
    assert!(c.read_until(&mut rig, 5, |s| s.len() >= 2), "{:?}", c.said);
    assert_eq!(c.said[..2], ["OK", "TIMEOUT 1000000000.0"]);
}
