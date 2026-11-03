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

/// The host interface, once its ports are open, and its command port.
/// Free ports are taken (`host::free_port`); one taken meanwhile by another
/// test is given up for the next.
fn host_on(rig: &Rig, h: &IonHandle) -> (crate::host::HostHandle, u16) {
    for _ in 0..10 {
        let port = crate::host::free_port();
        let host = crate::host::spawn(port, h.cmds.clone(), rig.ui_tx.clone()).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while !host.listening() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        if host.listening() {
            return (host, port);
        }
    }
    panic!("no free host ports");
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

/// A call nobody answers, given up by the host: the call stops and our CW
/// ID is the last thing on the air.
#[test]
fn ion_call_given_up_ends_with_an_id() {
    let mut rig = Rig::new();
    let h = rig.ion();
    h.cmds.send(IonCmd::Connect("N0BODY".into())).unwrap();
    let ok = rig.run_until(Duration::from_secs(20), |r| r.keyed);
    assert!(ok, "never called; notes {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::GiveUp).unwrap();
    h.cmds.send(IonCmd::Listen).unwrap();
    let ok = rig.run_until(Duration::from_secs(10), |r| r.id_sent() && r.keyed);
    assert!(ok, "no CW ID; notes {:?}", rig.ion_notes);
    let ok = rig.run_until(Duration::from_secs(15), |r| !r.keyed);
    assert!(ok, "still keyed");
    rig.run_until(Duration::from_secs(5), |r| r.keyed);
    assert!(!rig.keyed, "keyed again after the ID; notes {:?}", rig.ion_notes);
}

/// Broadcast traffic identifies after its run of bursts, not before or
/// between them.
#[test]
fn a_broadcast_run_ends_with_one_id() {
    let mut rig = Rig::new();
    let h = rig.ion();
    h.cmds.send(IonCmd::GroupOpen(1, "NET".into())).unwrap();
    h.cmds.send(IonCmd::GroupSend(1, b"one".to_vec(), None)).unwrap();
    let ok = rig.run_until(Duration::from_secs(20), |r| r.keyed);
    assert!(ok, "no broadcast; notes {:?}", rig.ion_notes);
    let ok = rig.run_until(Duration::from_secs(30), |r| !r.keyed);
    assert!(ok, "broadcast never ended");
    assert!(!rig.id_sent(), "ID before the run ended: {:?}", rig.ion_notes);
    h.cmds.send(IonCmd::GroupSend(1, b"two".to_vec(), None)).unwrap();
    let ok = rig.run_until(Duration::from_secs(20), |r| r.keyed);
    assert!(ok, "second broadcast never went");
    let ok = rig.run_until(Duration::from_secs(30), |r| !r.keyed);
    assert!(ok, "second broadcast never ended");
    assert!(!rig.id_sent(), "ID between the bursts: {:?}", rig.ion_notes);
    let ok = rig.run_until(Duration::from_secs(15), Rig::id_sent);
    assert!(ok, "no CW ID after the run; notes {:?}", rig.ion_notes);
    let n = rig.ion_notes.iter().filter(|n| n.contains("CW ID")).count();
    assert_eq!(n, 1, "{:?}", rig.ion_notes);
    h.cmds.send(IonCmd::Stop).unwrap();
}

/// A host program (standing in for Pat) drives Ion through the VARA-style
/// interface: connect to LOOP, send a line, read the echo, wait for
/// BUFFER 0, disconnect.
#[test]
fn winlink_host_drives_a_ion_link() {
    let mut rig = Rig::new();
    let h = rig.ion();
    let (_host, port) = host_on(&rig, &h);
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
    let (_host, port) = host_on(&rig, &h);
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

/// One station's host program on the simulated band: its command port
/// and its KISS port.
struct NetClient {
    cmd: TcpStream,
    lines: BufReader<TcpStream>,
    kiss: TcpStream,
    kiss_in: Vec<u8>,
    /// A line not yet whole.
    line: Vec<u8>,
    said: Vec<String>,
    /// (to, from, data) of every frame on the net's port.
    frames: Vec<(String, String, Vec<u8>)>,
}

impl NetClient {
    fn connect(port: u16) -> NetClient {
        let cmd = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let kiss = TcpStream::connect(("127.0.0.1", port - 200)).unwrap();
        cmd.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        kiss.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        NetClient {
            lines: BufReader::new(cmd.try_clone().unwrap()),
            cmd,
            kiss,
            kiss_in: Vec::new(),
            line: Vec::new(),
            said: Vec::new(),
            frames: Vec::new(),
        }
    }

    fn send(&mut self, line: &str) {
        self.cmd.write_all(format!("{line}\r").as_bytes()).unwrap();
    }

    /// A frame for the net (port 1): to `to`, or the whole net.
    fn frame(&mut self, to: &str, data: &[u8]) {
        let f = crate::host::net_frame(to, "", data);
        self.kiss.write_all(&crate::host::kiss_frame(1 << 4, &f)).unwrap();
    }

    /// What has come in on both ports.
    fn pump(&mut self) {
        // A timeout keeps what came of a line.
        while let Ok(n) = self.lines.read_until(b'\r', &mut self.line)
            && n > 0
            && self.line.ends_with(b"\r")
        {
            self.said.push(String::from_utf8_lossy(&self.line).trim().to_string());
            self.line.clear();
        }
        let mut b = [0u8; 4096];
        while let Ok(n) = self.kiss.read(&mut b)
            && n > 0
        {
            self.kiss_in.extend_from_slice(&b[..n]);
        }
        for (command, payload) in crate::host::kiss_frames(&mut self.kiss_in) {
            if command == 1 << 4
                && let Some((to, from, data)) = crate::host::net_unframe(&payload)
            {
                self.frames.push((to, from, data.to_vec()));
            }
        }
    }

    fn said(&self, line: &str) -> bool {
        self.said.iter().any(|l| l == line)
    }

    fn said_like(&self, start: &str) -> bool {
        self.said.iter().any(|l| l.starts_with(start))
    }

    fn got(&self, to: &str, from: &str, data: &[u8]) -> bool {
        self.frames.iter().any(|f| f.0 == to && f.1 == from && f.2 == data)
    }
}

/// Pumps every client until `done` or `secs` have passed.
fn net_until(cs: &mut [NetClient], secs: u64, done: impl Fn(&[NetClient]) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for c in cs.iter_mut() {
            c.pump();
        }
        if done(cs) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A net on the simulated band (`sim`), in real time, driven only through
/// each station's host interface: N0NCS runs TNARES, KJ5ET and AG5B join
/// it and check in with a line and traffic listed, their traffic reaches
/// net control, and its object and bulletin reach them, and the Close too. About 3 minutes of three
/// stations decoding: `cargo test -p ionhf -- --ignored a_net_runs`.
#[test]
#[ignore]
fn a_net_runs_over_the_host_interface() {
    let calls = ["N0NCS", "KJ5ET", "AG5B"];
    let ports: Vec<u16> = calls.iter().map(|_| crate::host::free_port()).collect();
    let o = crate::sim::Opts {
        stations: calls.len(),
        mode: ion_dsp::ionos::Mode::Wgn,
        snr_db: vec![20.0],
        calls: calls.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    };
    let band = crate::sim::Band::start(&o, &ports).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !band.listening() {
        assert!(Instant::now() < end, "host ports never opened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut cs: Vec<NetClient> = ports.iter().map(|&p| NetClient::connect(p)).collect();
    for (c, call) in cs.iter_mut().zip(calls) {
        c.send(&format!("MYCALL {call}"));
    }
    cs[1].send("NET LINE Trousdale County ARES");
    cs[2].send("NET TRAFFIC 1 P");
    for c in &mut cs[1..] {
        c.send("NET JOIN TNARES");
    }
    cs[0].send("NET OPEN TNARES");
    cs[0].send("NET OFFER");
    let ok = net_until(&mut cs, 10, |cs| {
        cs.iter().all(|c| c.said("NET PORT 1")) && cs[0].said.iter().filter(|l| *l == "OK").count() >= 2
    });
    assert!(ok, "{:?}", cs.iter().map(|c| &c.said).collect::<Vec<_>>());
    let announcement = b"# TNARES\nNet control N0NCS, 3.975 MHz".to_vec();
    let id = ion::link::net::ObjectId::of(&announcement);
    cs[0].frame("", &announcement);
    cs[0].frame("", b"Net closes at 2130. de N0NCS");
    cs[1].frame("", b"KJ5ET: no traffic, all well");
    cs[2].frame("N0NCS", b"AG5B: generator test complete");

    let t = Instant::now();
    let ok = net_until(&mut cs, 600, |cs| {
        cs[0].said_like("NET IN KJ5ET ")
            && cs[0].said_like("NET IN AG5B ")
            && cs[0].said("NET LINE KJ5ET Trousdale County ARES")
            && cs[0].said("NET TRAFFIC AG5B 1 P")
            && cs[1..]
                .iter()
                .all(|c| c.said(&format!("NET OBJECT {id}")) && c.got(&format!("#{id}"), "N0NCS", &announcement))
            && cs[0].got("N0NCS", "KJ5ET", b"KJ5ET: no traffic, all well")
            && cs[0].got("N0NCS", "AG5B", b"AG5B: generator test complete")
            && cs[1..]
                .iter()
                .all(|c| c.said_like("NET JOINED N0NCS TNARES ") && c.got("", "N0NCS", b"Net closes at 2130. de N0NCS"))
    });
    let report = |cs: &[NetClient]| {
        cs.iter()
            .map(|c| format!("said {:?}\nframes {:?}", c.said, c.frames))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(ok, "after {:?}:\n{}", t.elapsed(), report(&cs));
    assert!(cs[0].said_like("NET HEARD "), "{}", report(&cs));
    println!(
        "net over the host interface: checked in, traffic and bulletin in {:?}",
        t.elapsed()
    );

    cs[0].send("NET CLOSE");
    let ok = net_until(&mut cs, 60, |cs| cs.iter().all(|c| c.said("NET CLOSED")));
    assert!(ok, "{}", report(&cs));
}

/// The Net section's records over the simulated band, driven through the
/// host interface: N0NCS opens TNARES and offers its announcement, AG5B
/// checks in with a line and a Priority message, net control logs both,
/// offers the summary, and closes once AG5B has it; AG5B merges it into
/// its own log. About 3 minutes of two stations decoding:
/// `cargo test -p ionhf -- --ignored a_net_logs_and_sends_its_summary`.
#[test]
#[ignore]
fn a_net_logs_and_sends_its_summary() {
    use crate::net_doc::{self, Announcement, Record};
    use crate::net_log::{Item, NetLog, NetRecord, Role, tests::TempDir};
    use ion::link::net::{ObjectId, Precedence};

    let calls = ["N0NCS", "AG5B"];
    let ports: Vec<u16> = calls.iter().map(|_| crate::host::free_port()).collect();
    let o = crate::sim::Opts {
        stations: calls.len(),
        mode: ion_dsp::ionos::Mode::Wgn,
        snr_db: vec![20.0],
        calls: calls.iter().map(|c| c.to_string()).collect(),
        ..Default::default()
    };
    let band = crate::sim::Band::start(&o, &ports).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !band.listening() {
        assert!(Instant::now() < end, "host ports never opened");
        std::thread::sleep(Duration::from_millis(20));
    }
    let dir = TempDir::new("net-e2e");
    let now = crate::net_log::now;
    let mut ncs_log = NetLog::create(
        &dir.0.join("N0NCS"),
        NetRecord {
            net: "TNARES".into(),
            ncs: "N0NCS".into(),
            operator: "N0NCS".into(),
            opened: now(),
            ..NetRecord::default()
        },
    );
    let mut cs: Vec<NetClient> = ports.iter().map(|&p| NetClient::connect(p)).collect();
    for (c, call) in cs.iter_mut().zip(calls) {
        c.send(&format!("MYCALL {call}"));
    }
    cs[1].send("NET LINE Trousdale County ARES");
    cs[1].send("NET TRAFFIC 1 P");
    cs[1].send("NET JOIN TNARES");
    cs[0].send("NET OPEN TNARES");
    cs[0].send("NET OFFER");
    let ok = net_until(&mut cs, 10, |cs| {
        cs.iter().all(|c| c.said("NET PORT 1")) && cs[0].said.iter().filter(|l| *l == "OK").count() >= 2
    });
    assert!(ok, "{:?}", cs.iter().map(|c| &c.said).collect::<Vec<_>>());
    let ann = Announcement {
        net: "TNARES".into(),
        ncs: "N0NCS".into(),
        freq_hz: Some(3_975_000),
        body: "# Tonight\n\n1. Check-ins\n2. Traffic, **Priority** first\n".into(),
        ..Announcement::default()
    };
    let ann_bytes = Record::Announcement(ann.clone()).encode();
    let ann_id = ObjectId::of(&ann_bytes);
    cs[0].frame("", &ann_bytes);
    let msg = Record::Message(net_doc::Message {
        precedence: Precedence::Priority,
        to: net_doc::To::Ncs,
        text: "AG5B: shelter open, 40 cots".into(),
    });
    cs[1].frame("", &msg.encode());

    // Net control's side of the log, from what its host interface says.
    let mut seen = (0, 0);
    let mut log_from_host = |c: &NetClient, log: &mut NetLog| {
        for l in &c.said[seen.0..] {
            let w: Vec<&str> = l.split(' ').collect();
            match w[..] {
                ["NET", "IN", call, slot, ref via @ ..] => {
                    let via = via.get(1).copied();
                    log.checked_in(call, slot.parse().unwrap_or(0), via, now());
                }
                ["NET", "LINE", call, ..] => {
                    let line = l.splitn(4, ' ').nth(3).unwrap_or("");
                    log.line(call, line, now());
                }
                ["NET", "TRAFFIC", call, n, p, ..] => {
                    let t = ion::link::net::Traffic {
                        items: n.parse().unwrap_or(0),
                        precedence: crate::ion_engine::precedence_of(p).unwrap_or_default(),
                        short: w.len() > 5,
                    };
                    log.traffic_listed(call, t, now());
                }
                _ => {}
            }
        }
        for (to, from, data) in &c.frames[seen.1..] {
            if let Record::Message(m) = Record::decode(data) {
                log.item(Item {
                    at: now(),
                    from: from.clone(),
                    to: to.clone(),
                    precedence: m.precedence,
                    subject: net_doc::subject(&m.text),
                    text: m.text,
                    state: net_doc::State::Received,
                    ..Item::default()
                });
            }
        }
        seen = (c.said.len(), c.frames.len());
    };
    let report = |cs: &[NetClient]| {
        cs.iter()
            .map(|c| format!("said {:?}\nframes {:?}", c.said, c.frames))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let t = Instant::now();
    let ok = net_until(&mut cs, 600, |cs| {
        cs[0].said_like("NET IN AG5B ")
            && cs[0].said("NET LINE AG5B Trousdale County ARES")
            && cs[0].said("NET TRAFFIC AG5B 1 P")
            && cs[0].frames.iter().any(|f| f.1 == "AG5B")
            && cs[1].got(&format!("#{ann_id}"), "N0NCS", &ann_bytes)
    });
    assert!(ok, "after {:?}:\n{}", t.elapsed(), report(&cs));
    log_from_host(&cs[0], &mut ncs_log);
    let r = &NetLog::load(&ncs_log.path).unwrap().rec;
    assert_eq!(r.checkins[0].call, "AG5B");
    assert_eq!(r.checkins[0].line, "Trousdale County ARES");
    assert_eq!(r.traffic[0].precedence, Precedence::Priority);
    assert_eq!(r.traffic[0].from, "AG5B");
    let got = cs[1].frames.iter().find(|f| f.0 == format!("#{ann_id}")).unwrap();
    assert_eq!(Record::decode(&got.2), Record::Announcement(ann));
    println!("announcement, check-in and Priority message in {:?}", t.elapsed());

    // The close: the summary to all, then NET CLOSE once the member has it.
    let summary = ncs_log.summary(now());
    let sum_bytes = Record::Summary(summary.clone()).encode();
    let sum_id = ObjectId::of(&sum_bytes);
    println!("summary {} B", sum_bytes.len());
    cs[0].send("NET OFFER");
    cs[0].frame("", &sum_bytes);
    let t = Instant::now();
    let ok = net_until(&mut cs, 300, |cs| cs[1].got(&format!("#{sum_id}"), "N0NCS", &sum_bytes));
    assert!(ok, "after {:?}:\n{}", t.elapsed(), report(&cs));
    println!("summary held in {:?}", t.elapsed());
    // At once, likely while the member is sending its report: net control
    // lets that turn end before its Closes.
    cs[0].send("NET CLOSE");
    let t = Instant::now();
    let ok = net_until(&mut cs, 120, |cs| cs.iter().all(|c| c.said("NET CLOSED")));
    println!("closed in {:?}", t.elapsed());
    assert!(ok, "{}", report(&cs));
    ncs_log.close(now());

    let mut member = NetLog::create(
        &dir.0.join("AG5B"),
        NetRecord {
            net: "TNARES".into(),
            operator: "AG5B".into(),
            role: Role::Member,
            opened: now(),
            ..NetRecord::default()
        },
    );
    let Record::Summary(s) = Record::decode(&sum_bytes) else {
        panic!("not a summary")
    };
    assert_eq!(s, summary);
    member.merge(&s);
    let m = &NetLog::load(&member.path).unwrap().rec;
    assert!(m.summary);
    assert_eq!(m.ncs, "N0NCS");
    assert_eq!(m.checkins[0].line, "Trousdale County ARES");
    assert_eq!(m.traffic[0].precedence, Precedence::Priority);
    assert_eq!(m.traffic[0].subject, "AG5B: shelter open, 40 cots");
}
