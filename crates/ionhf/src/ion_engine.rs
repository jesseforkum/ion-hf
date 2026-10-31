//! The Ion worker thread: owns the link [`Node`] and the broadcast groups
//! a host program opens ([`BcastStation`]); takes
//! received audio from the DSP thread and commands from the UI, and hands
//! back bursts to transmit, status and received bytes. Decoding a burst
//! takes tens of milliseconds, so it stays off the DSP and loopback threads.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use ion::link::bcast::{BcastConfig, BcastEvent, BcastStation};
use ion::link::node::{DEFAULT_LEAD_S, NodeAir};
use ion::link::{Air, Gear, Node, Role, State};

use crate::engine::{Msg, SAMPLE_RATE, UiEvent};

/// CW ID at least this often during a link, and once at its end (§97.119:
/// the calls inside Ion frames do not count while the protocol is not
/// publicly documented). Under 10 minutes, as the next burst may be late.
const ID_EVERY_S: f64 = 570.0;
/// A broadcast run has ended once nothing more has gone for this long:
/// its CW ID.
const BCAST_ID_HOLD_S: f64 = 10.0;
/// From the end of the closing station's CW ID to the start of ours.
const ID_GAP_S: f64 = 1.5;
/// How long a host program's DISCONNECT waits for what is queued to go.
pub const HOST_DRAIN_S: f64 = 30.0;

/// From the DSP thread.
#[derive(Debug)]
pub enum IonIn {
    /// Received audio (48 kHz), only while we are not keyed.
    Audio(Vec<f32>),
    /// Our burst has finished playing (or was aborted).
    TxDone,
    /// Received samples not passed on (we were keyed, or just after).
    Gap(usize),
}

/// From the UI.
#[derive(Clone, Debug)]
pub enum IonCmd {
    SetCall(String),
    Listen,
    Connect(String),
    Send(Vec<u8>),
    /// Close the link politely (this keys the radio for the goodbye).
    Disconnect,
    /// Close it once what is queued has been acknowledged, within
    /// [`HOST_DRAIN_S`] (a host program's DISCONNECT).
    DisconnectWhenSent,
    /// Drop everything locally, without transmitting.
    Stop,
    /// End a call or link now (a host program gave up): what is on the
    /// air stops, then our CW ID goes out if we have keyed since the last.
    GiveUp,
    /// Extra level for OFDM bursts on the air, dB (0 to 3).
    SetOfdmDb(f32),
    /// The link's choice of level, from the next link on (at once while
    /// none is up).
    SetGear(Gear),
    /// The whole-burst clipper on or off, from the next burst on.
    SetBurstClip(bool),
    /// Offer the stream gear, from the next link on (at once while none
    /// is up).
    SetStream(bool),
    /// A Winlink host interface takes the link's received bytes and status
    /// (`None`: it has gone).
    AttachHost(Option<Sender<crate::host::HostIn>>),
    /// A host program opens a broadcast group on its KISS port `port`.
    GroupOpen(u8, String),
    GroupClose(u8),
    /// A frame for the group on `port`; `tag`: told back once it is sent.
    GroupSend(u8, Vec<u8>, Option<u16>),
    /// KISS persistence P for every group.
    GroupPersist(u8),
    /// Decode plots of the bursts heard (off by default).
    Plot(bool),
}

#[derive(Clone, Debug)]
pub struct IonStatus {
    pub state: State,
    pub role: Role,
    pub peer: String,
    pub level: usize,
    /// SNR of the last burst heard.
    pub snr_db: Option<f32>,
    pub bytes_acked: u64,
    pub bytes_received: u64,
    pub pending: usize,
    pub data_bursts: u32,
    pub frames_resent: u32,
    /// HARQ increments we sent.
    pub increments_sent: u32,
    /// Receiving side: codewords decoded only thanks to HARQ combining,
    /// and codewords still lost after combining.
    pub harq_recovered: u32,
    pub codewords_failed: u32,
    pub polls: u32,
    pub on_air: bool,
    /// A burst is being received.
    pub busy: bool,
    /// Seconds since the link came up (0 when not connected).
    pub connected_s: f64,
    /// The level the peer's last SNR report allows.
    pub target_level: usize,
    pub frames_sent: u32,
    pub control_bursts: u32,
    /// Seconds we have been on the air this link.
    pub airtime_s: f64,
    /// Bursts sent and heard since the last status.
    pub bursts: Vec<ion::link::node::BurstNote>,
    /// The third of the passband our slice bursts go in (the peer's pick)
    /// and the one our receivers would like the peer's in, 0 the centre, 1
    /// below, 2 above (`ion::slots`); each third's score here (lower is
    /// cleaner).
    pub slot_tx: u8,
    pub slot_rx: u8,
    pub band: [f32; 3],
    /// The link holds below the floor (`ion` docs/link.md, Lifeline).
    pub holding: bool,
    /// Why the link ended, once it has.
    pub end: Option<ion::link::station::End>,
    /// Broadcast groups open, and the stations heard in them lately (call,
    /// S/N), newest first.
    pub groups: Vec<String>,
    pub heard: Vec<(String, f32)>,
    /// Our CW ID is on the air.
    pub cw_id: bool,
    /// The stream gear and the cues.
    pub stream: Box<StreamView>,
}

/// What the stream and cue lamps need.
#[derive(Clone, Debug, Default)]
pub struct StreamView {
    pub gear: ion::link::StreamStatus,
    /// Cues we sent and heard so far.
    pub cues: u64,
}

#[derive(Clone, Debug)]
pub enum IonEvent {
    Status(IonStatus),
    Received(Vec<u8>),
    Note(String),
    /// A burst heard, as its receiver saw it ([`IonCmd::Plot`]).
    Plot(Arc<ion::link::node::BurstPlot>),
}

/// The worker's command channel, and its audio channel for the DSP thread.
pub struct IonHandle {
    pub cmds: Sender<IonCmd>,
    pub audio: Sender<IonIn>,
    _thread: std::thread::JoinHandle<()>,
}

/// Our own station's node, with OFDM at the on-air level (the loopback test
/// stations stay at 0 dB).
fn on_air_node(call: &str, gear: Gear, stream: bool, ofdm_db: f32) -> Node {
    let mut cfg = Node::config(call);
    cfg.gear = gear;
    cfg.stream = stream;
    let mut n = Node::new(cfg, DEFAULT_LEAD_S);
    n.set_ofdm_gain_db(ofdm_db);
    n.set_id_every(Some(ID_EVERY_S));
    n
}

pub fn spawn(my_call: String, engine: Sender<Msg>, ui: Sender<UiEvent>, repaint: Box<dyn Fn() + Send>) -> IonHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (audio_tx, audio_rx) = crossbeam_channel::unbounded();
    let thread = std::thread::Builder::new()
        .name("ion".into())
        .spawn(move || Worker::new(my_call, engine, ui, repaint).run(cmd_rx, audio_rx))
        .expect("spawn ion thread");
    IonHandle {
        cmds: cmd_tx,
        audio: audio_tx,
        _thread: thread,
    }
}

struct Worker {
    node: Node,
    /// Extra level for our OFDM bursts, dB.
    ofdm_db: f32,
    gear: Gear,
    stream: bool,
    /// A Winlink host (Pat, Winlink Express) driving the link.
    host: Option<Sender<crate::host::HostIn>>,
    call: String,
    engine: Sender<Msg>,
    ui: Sender<UiEvent>,
    repaint: Box<dyn Fn() + Send>,
    start: Instant,
    last_state: State,
    connected_at: Option<f64>,
    /// We have keyed since our last CW ID.
    keyed_since_id: bool,
    /// When our CW ID goes out (the link has ended).
    id_due: Option<f64>,
    /// Our CW ID is playing.
    id_on_air: bool,
    /// We asked for the goodbye (so we ID first).
    closing: bool,
    /// Broadcast groups, beside the link; sessions go first.
    bcast: BcastStation,
    /// KISS port → group, and back.
    ports: BTreeMap<u8, usize>,
    /// The broadcast burst is on the air.
    bcast_on_air: bool,
    /// When we first keyed after our last CW ID.
    keyed_from: Option<f64>,
    /// Decode plots wanted (kept across new nodes).
    plot: bool,
    /// The stream was on at the last look (for the log).
    streaming: bool,
    /// Broadcast bursts sent since the last status, for the log.
    bcast_sent: Vec<ion::link::node::BurstNote>,
}

impl Worker {
    fn new(call: String, engine: Sender<Msg>, ui: Sender<UiEvent>, repaint: Box<dyn Fn() + Send>) -> Self {
        let bcast_call = call.clone();
        Self {
            node: on_air_node(&call, Gear::default(), true, ion::link::audio::OFDM_AIR_GAIN_DB),
            ofdm_db: ion::link::audio::OFDM_AIR_GAIN_DB,
            gear: Gear::default(),
            stream: true,
            host: None,
            call,
            engine,
            ui,
            repaint,
            start: Instant::now(),
            last_state: State::Idle,
            connected_at: None,
            keyed_since_id: false,
            id_due: None,
            id_on_air: false,
            closing: false,
            bcast: BcastStation::new(
                BcastConfig::new(&bcast_call),
                Arc::new(NodeAir { lead_s: DEFAULT_LEAD_S }) as Arc<dyn Air>,
            ),
            ports: BTreeMap::new(),
            bcast_on_air: false,
            keyed_from: None,
            plot: false,
            streaming: false,
            bcast_sent: Vec::new(),
        }
    }

    fn send_host(&mut self, m: crate::host::HostIn) {
        if let Some(h) = &self.host
            && h.send(m).is_err()
        {
            self.host = None;
        }
    }

    fn port_of(&self, group: usize) -> Option<u8> {
        self.ports.iter().find(|(_, g)| **g == group).map(|(&p, _)| p)
    }

    /// What the groups heard and sent, for the host.
    fn bcast_events(&mut self) {
        use crate::host::HostIn;
        for e in self.bcast.take_events() {
            let m = match e {
                BcastEvent::Frame { group, data } => self.port_of(group).map(|p| HostIn::GroupFrame(p, data)),
                BcastEvent::Heard { group, call, snr_db } => {
                    self.port_of(group).map(|p| HostIn::GroupHeard(p, call, snr_db))
                }
                BcastEvent::Lost { group, frames } => self.port_of(group).map(|p| HostIn::GroupLost(p, frames)),
                BcastEvent::Dropped { group, .. } => self.port_of(group).map(|p| HostIn::GroupDropped(p, 1)),
                BcastEvent::Sent { tag } => Some(HostIn::GroupSent(tag)),
            };
            if let Some(m) = m {
                self.send_host(m);
            }
        }
    }

    /// Our CW ID, with the PTT lead-in.
    fn id_audio(&self) -> Vec<f32> {
        let mut a = vec![0.0; (DEFAULT_LEAD_S * SAMPLE_RATE as f64) as usize];
        a.extend(ion::sounding::cw(&format!("DE {}", self.call)));
        a
    }

    /// The link has ended: our CW ID, now if we closed it (or it failed),
    /// else once the peer's own ID is over.
    fn schedule_id(&mut self, now: f64, peer: &str) {
        if !self.keyed_since_id || self.id_due.is_some() {
            return;
        }
        let wait = if self.closing || peer.is_empty() {
            0.0
        } else {
            DEFAULT_LEAD_S + ion::sounding::cw(&format!("DE {peer}")).len() as f64 / SAMPLE_RATE as f64 + ID_GAP_S
        };
        self.id_due = Some(now + wait);
    }

    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn event(&self, ev: IonEvent) {
        let _ = self.ui.try_send(UiEvent::Ion(ev));
        (self.repaint)();
    }

    fn note(&self, text: impl Into<String>) {
        self.event(IonEvent::Note(text.into()));
    }

    /// A fresh node (keeps nothing of the old link).
    fn reset(&mut self) {
        if self.node.on_air() {
            let _ = self.engine.send(Msg::Abort);
        }
        let cut_short = !matches!(
            self.node.station().state(),
            State::Idle | State::Listening | State::Disconnected | State::Failed
        );
        self.node = on_air_node(&self.call, self.gear, self.stream, self.ofdm_db);
        self.node.set_plot(self.plot);
        self.connected_at = None;
        self.closing = false;
        // A link cut short still gets its ID, before anything new; one
        // already scheduled (after the peer's own) or playing stands.
        if cut_short && self.keyed_since_id && self.id_due.is_none() && !self.id_on_air {
            self.id_due = Some(self.now());
        }
    }

    fn command(&mut self, cmd: IonCmd) {
        let now = self.now();
        match cmd {
            IonCmd::SetCall(c) => {
                let c = c.trim().to_ascii_uppercase();
                if c != self.call {
                    self.bcast.set_call(&c);
                    self.call = c;
                    self.reset();
                }
            }
            IonCmd::Listen => {
                if self.call.is_empty() {
                    self.note("! set your call in Settings first");
                    return;
                }
                self.reset();
                self.node.station_mut().listen();
                self.note(format!("Ion: listening as {}", self.call));
            }
            IonCmd::Connect(peer) => {
                let peer = peer.trim().to_ascii_uppercase();
                if self.call.is_empty() || peer.is_empty() || peer.contains(char::is_whitespace) {
                    self.note("! Ion needs your call (Settings) and one call to connect to");
                    return;
                }
                self.reset();
                self.node.station_mut().connect(&peer, now);
                self.note(format!("Ion: calling {peer}"));
            }
            IonCmd::Send(bytes) => self.node.station_mut().send(&bytes),
            IonCmd::Disconnect => {
                self.closing = true;
                self.node.station_mut().disconnect();
            }
            IonCmd::DisconnectWhenSent => {
                self.closing = true;
                self.node.station_mut().disconnect_when_sent(now, HOST_DRAIN_S);
            }
            IonCmd::Stop => {
                // Stop cuts a CW ID or a broadcast burst that is playing,
                // and drops the broadcast queue, too.
                if self.id_on_air || self.bcast_on_air {
                    self.id_on_air = false;
                    self.bcast_on_air = false;
                    let _ = self.engine.send(Msg::Abort);
                }
                let mut dropped: BTreeMap<usize, u32> = BTreeMap::new();
                for g in self.bcast.abort() {
                    *dropped.entry(g).or_default() += 1;
                }
                for (g, n) in dropped {
                    if let Some(p) = self.port_of(g) {
                        self.send_host(crate::host::HostIn::GroupDropped(p, n));
                    }
                }
                self.reset();
                // Stop keys nothing, not even the ID (it goes out at the end
                // of the next link instead).
                self.id_due = None;
                self.note("Ion: stopped");
            }
            IonCmd::GiveUp => {
                let calling = self.node.station().state() == State::Connecting;
                self.reset();
                self.note(if calling {
                    "Ion: stopped calling"
                } else {
                    "Ion: stopped"
                });
            }
            IonCmd::AttachHost(h) => self.host = h,
            IonCmd::GroupOpen(port, name) => {
                if let Some(old) = self.ports.remove(&port) {
                    self.bcast.close(old);
                }
                let g = self.bcast.open(&name);
                self.ports.insert(port, g);
            }
            IonCmd::GroupClose(port) => {
                if let Some(g) = self.ports.remove(&port) {
                    let n = self.bcast.close(g).len() as u32;
                    if n > 0 {
                        self.send_host(crate::host::HostIn::GroupDropped(port, n));
                    }
                }
            }
            IonCmd::GroupSend(port, data, tag) => {
                let sent = self.ports.get(&port).is_some_and(|&g| self.bcast.send(g, &data, tag));
                if !sent {
                    self.send_host(crate::host::HostIn::GroupDropped(port, 1));
                }
            }
            IonCmd::GroupPersist(p) => self.bcast.set_persistence(p),
            IonCmd::Plot(on) => {
                self.plot = on;
                self.node.set_plot(on);
            }
            IonCmd::SetOfdmDb(db) => {
                self.ofdm_db = db;
                self.node.set_ofdm_gain_db(db);
            }
            IonCmd::SetBurstClip(on) => ion::phy::set_td_clip(on),
            IonCmd::SetGear(g) => {
                self.gear = g;
                self.refresh_idle();
            }
            IonCmd::SetStream(on) => {
                self.stream = on;
                self.refresh_idle();
            }
        }
    }

    /// A fresh node for new settings, unless a link is up.
    fn refresh_idle(&mut self) {
        match self.node.station().state() {
            State::Idle | State::Disconnected | State::Failed => self.reset(),
            State::Listening => {
                self.reset();
                self.node.station_mut().listen();
            }
            _ => {}
        }
    }

    /// A log line when the stream gear starts or stops.
    fn stream_note(&mut self) {
        let s = self.node.station().stream_status();
        if s.on == self.streaming {
            return;
        }
        self.streaming = s.on;
        let opt = s.opt.map_or("", |o| ion::link::stream::OPTS[o].name);
        self.note(if s.on {
            format!(
                "Ion: stream on, {} {opt}",
                if s.sending { "sending" } else { "receiving" }
            )
        } else {
            format!("Ion: stream off after {} periods", s.period)
        });
    }

    fn status(&mut self) -> IonStatus {
        let mut bursts = self.node.take_journal();
        bursts.append(&mut self.bcast_sent);
        let (slot_rx, band) = self.node.band();
        let s = self.node.station();
        let st = s.stats();
        IonStatus {
            slot_tx: s.slots().0,
            slot_rx,
            band,
            target_level: s.target_level(),
            frames_sent: st.frames_sent,
            control_bursts: st.control_bursts,
            airtime_s: st.airtime_s,
            bursts,
            state: s.state(),
            role: s.role(),
            peer: s.peer().to_string(),
            level: s.level(),
            snr_db: self.node.last_snr_db,
            bytes_acked: st.bytes_acked,
            bytes_received: st.bytes_received,
            pending: s.pending(),
            data_bursts: st.data_bursts,
            frames_resent: st.frames_resent,
            increments_sent: st.increments_sent,
            harq_recovered: st.harq_recovered,
            codewords_failed: st.codewords_failed,
            polls: st.polls,
            on_air: self.node.on_air() || self.bcast_on_air,
            busy: self.node.busy(),
            connected_s: self.connected_at.map_or(0.0, |t| self.now() - t),
            holding: s.holding(),
            end: s.end(),
            groups: self.bcast.groups(),
            heard: self.bcast.heard(self.now()),
            cw_id: self.id_on_air,
            stream: Box::new(StreamView {
                gear: s.stream_status(),
                cues: st.cued_bursts as u64 + self.node.cues_heard(),
            }),
        }
    }

    fn run(mut self, cmds: Receiver<IonCmd>, audio: Receiver<IonIn>) {
        let mut last_status = Instant::now();
        loop {
            loop {
                match cmds.try_recv() {
                    Ok(c) => self.command(c),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => return,
                }
            }
            let first = match audio.recv_timeout(Duration::from_millis(10)) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            for m in first.into_iter().chain(audio.try_iter()) {
                let now = self.now();
                if matches!(m, IonIn::TxDone) && self.id_on_air {
                    self.id_on_air = false;
                    self.keyed_since_id = false;
                    self.keyed_from = None;
                    continue;
                }
                if matches!(m, IonIn::TxDone) && self.bcast_on_air {
                    self.bcast_on_air = false;
                    self.bcast.on_tx_done(now);
                    // Broadcast traffic identifies at the end of each run,
                    // and at least every ID_EVERY_S while it goes on.
                    let due = if self.keyed_from.is_some_and(|t| now - t >= ID_EVERY_S) {
                        now
                    } else {
                        now + BCAST_ID_HOLD_S
                    };
                    self.id_due = Some(match self.id_due {
                        Some(d) if d <= now => d,
                        _ => due,
                    });
                    continue;
                }
                match m {
                    IonIn::Audio(samples) => self.node.push_audio(&samples, now),
                    IonIn::TxDone => self.node.tx_done(now),
                    IonIn::Gap(n) => self.node.push_gap(n),
                }
            }
            let now = self.now();
            // The groups' bursts, as the link's receivers decoded them.
            self.node.set_groups(&self.bcast.ids());
            for inc in self.node.take_group() {
                self.bcast.on_rx(&inc, now);
            }
            self.bcast.set_busy(self.node.busy());
            self.bcast_events();
            // The CW ID when it is due and the channel is ours; nothing
            // else keys while it plays (status still goes out).
            if !self.id_on_air && !self.bcast_on_air && self.id_due.is_some_and(|t| now >= t) && !self.node.busy() {
                self.id_due = None;
                self.id_on_air = true;
                let _ = self.engine.send(Msg::IonTx(self.id_audio()));
                self.note(format!("Ion: CW ID (DE {})", self.call));
            }
            let hold_tx = self.id_on_air;
            // Broadcast only between sessions, as Data2G's host.
            let link_idle = matches!(
                self.node.station().state(),
                State::Idle | State::Listening | State::Disconnected | State::Failed
            );
            if !hold_tx
                && link_idle
                && !self.bcast_on_air
                && !self.node.on_air()
                && let Some(b) = self.bcast.poll_tx(now)
            {
                let mut audio = vec![0.0; (DEFAULT_LEAD_S * SAMPLE_RATE as f64) as usize];
                audio.extend(ion::link::audio::transmit_with(
                    &b,
                    ion::link::audio::ofdm_gain(self.ofdm_db),
                ));
                self.bcast_on_air = true;
                self.keyed_since_id = true;
                self.keyed_from.get_or_insert(now);
                self.bcast_sent.push(ion::link::node::BurstNote {
                    tx: true,
                    level: Some(b.code),
                    what: format!(
                        "BCAST {} ×{}",
                        ion::sounding::level_name(b.code, b.wave),
                        b.frames.len()
                    ),
                    snr_db: None,
                    lost: 0,
                    secs: audio.len() as f64 / SAMPLE_RATE as f64,
                    calls: None,
                    reported_db: None,
                });
                let _ = self.engine.send(Msg::IonTx(audio));
            }
            if !hold_tx
                && !self.bcast_on_air
                && let Some(burst) = self.node.poll(now)
            {
                self.keyed_since_id = true;
                self.keyed_from.get_or_insert(now);
                let _ = self.engine.send(Msg::IonTx(burst));
            }
            for p in self.node.take_plots() {
                self.event(IonEvent::Plot(Arc::new(p)));
            }
            let got = self.node.station_mut().take_received();
            if !got.is_empty() {
                match &self.host {
                    // Winlink traffic is for the host program, not the screen.
                    Some(h) => {
                        let n = got.len();
                        if h.send(crate::host::HostIn::Received(got)).is_err() {
                            self.host = None;
                        }
                        log::debug!("Winlink host: {n} bytes received");
                    }
                    None => self.event(IonEvent::Received(got)),
                }
            }
            self.stream_note();
            let state = self.node.station().state();
            if state != self.last_state {
                match state {
                    State::Connected => {
                        self.connected_at.get_or_insert(now);
                    }
                    State::Disconnecting => {}
                    _ => self.connected_at = None,
                }
                let peer = self.node.station().peer().to_string();
                if matches!(state, State::Disconnected | State::Failed) {
                    self.schedule_id(now, &peer);
                    self.closing = false;
                }
                use ion::link::station::End;
                let end = self.node.station().end();
                self.note(match (state, end) {
                    (State::Connected, _) => format!("Ion: connected to {peer}"),
                    (_, Some(End::Weak { .. })) => format!("Ion: disconnected from {peer}, the path too weak"),
                    (_, Some(End::LinkLost)) => format!("Ion: link to {peer} lost"),
                    (_, Some(End::NoAnswer)) => format!("Ion: no answer from {peer}"),
                    (State::Disconnected, _) => "Ion: disconnected".to_string(),
                    (State::Failed, _) => format!("Ion: link to {peer} failed"),
                    (other, _) => format!("Ion: {other:?}"),
                });
                self.last_state = state;
            }
            if last_status.elapsed() >= Duration::from_millis(250) {
                last_status = Instant::now();
                let s = self.status();
                if let Some(h) = &self.host
                    && h.send(crate::host::HostIn::Status(s.clone())).is_err()
                {
                    self.host = None;
                }
                self.event(IonEvent::Status(s));
            }
        }
    }
}

// -- loopback test station ------------------------------------------------------

/// The loopback radio's line to the simulated Ion station.
pub struct PeerLink {
    /// What the peer hears (48 kHz blocks).
    pub to_peer: Sender<Vec<f32>>,
    /// Bursts the peer transmits, to mix into our receive audio.
    pub from_peer: Receiver<Vec<f32>>,
}

/// "LOOP": listens, answers calls and sends every line back prefixed with
/// "LOOP copies: ". Runs on its own thread so its decoding never stalls the
/// loopback audio. Dropping the [`PeerLink`] ends it.
pub fn spawn_peer() -> PeerLink {
    let (to_peer, heard) = crossbeam_channel::unbounded::<Vec<f32>>();
    let (send, from_peer) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("ion-peer".into())
        .spawn(move || {
            let start = Instant::now();
            let fresh = || {
                let mut n = Node::new(Node::config("LOOP"), DEFAULT_LEAD_S);
                n.station_mut().listen();
                n
            };
            let mut node = fresh();
            let mut on_air_until: Option<f64> = None;
            let mut line: Vec<u8> = Vec::new();
            loop {
                let first = match heard.recv_timeout(Duration::from_millis(20)) {
                    Ok(b) => Some(b),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                for block in first.into_iter().chain(heard.try_iter()) {
                    node.push_audio(&block, start.elapsed().as_secs_f64());
                }
                let now = start.elapsed().as_secs_f64();
                if on_air_until.is_some_and(|t| now >= t) {
                    on_air_until = None;
                    node.tx_done(now);
                }
                if let Some(burst) = node.poll(now) {
                    on_air_until = Some(now + burst.len() as f64 / SAMPLE_RATE as f64);
                    if send.send(burst).is_err() {
                        return;
                    }
                }
                // Echo whole lines.
                line.extend(node.station_mut().take_received());
                while let Some(i) = line.iter().position(|&b| b == b'\n') {
                    let mut reply = b"LOOP copies: ".to_vec();
                    reply.extend(line.drain(..=i));
                    node.station_mut().send(&reply);
                }
                if matches!(node.station().state(), State::Disconnected | State::Failed) && on_air_until.is_none() {
                    node = fresh();
                    line.clear();
                }
            }
        })
        .expect("spawn ion peer");
    PeerLink { to_peer, from_peer }
}
