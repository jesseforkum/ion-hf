//! Net mode: one net control station (NCS) polls a pool of members that
//! share a channel. Like [`super::Station`], [`NetStation`] knows nothing
//! about audio: it hands out [`Outgoing`] bursts and takes [`Incoming`]
//! ones on the caller's clock. [`NetNode`] adds the receivers.

mod bulletin;
mod member;
mod ncs;
mod node;
mod prime;
mod relay;
mod stream;
mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use ion_dsp::dsp::NoiseGen;

use super::frame::{Frame, Kind, crc32};
use super::harq::HarqRx;
use super::net_code::{self, BLOCK_BYTES, Decoder, PIECE};
use super::station::{Air, Incoming, Outgoing, TINY, air_code, frame_bits, user_bits, user_hi, user_session};
use crate::level::{FLOOR, FrameSize, LADDER, Wave};
use crate::phy_mfsk::ExpectedHeader;

use bulletin::{BulRx, BulTx, bul_frames, lacking, pieces_per_frame};
use member::{Applicant, Piece, Sent};
use ncs::{Member, Wait};
use prime::chorus_slot_s;
use relay::{Kid, Stage, Turn};
use stream::{Held, Inbox, MsgIn, Outbox, msg_field};
use wire::*;

pub use ncs::MAX_MEMBERS;
pub use node::NetNode;
pub use prime::{CHORUS_FMT, ChorusPlan, NetExpect, PrimeStats};
pub use stream::MAX_MSG;
pub use wire::{
    ALL, CAPS_COMPRESS, Caps, MAX_CALL, PROTOCOL, checkin_call, checkin_frame_bytes, checkin_user, floor_reply_headers,
    idle_header, net_id, pack_call, report_frame_bytes, software_version, unpack_call, version_string,
};

#[derive(Clone, Debug)]
pub struct NetConfig {
    pub call: String,
    /// Net name. A member with an empty name joins the first net it hears.
    pub net: String,
    /// This station runs the net.
    pub ncs: bool,
    pub turnaround_s: f64,
    /// Longest burst.
    pub max_burst_s: f64,
    /// Extra wait for an answer.
    pub reply_slack_s: f64,
    /// How long after a burst ends a reply has surely started (preamble
    /// detected) if one is coming.
    pub detect_s: f64,
    /// Check-in slots after each beacon.
    pub window_slots: u8,
    /// NCS: beacon this often while the net is open...
    pub beacon_every_s: f64,
    /// ...or this soon after a window with check-ins or collisions.
    pub quick_beacon_s: f64,
    /// NCS: pause between rounds in which nobody had traffic.
    pub idle_round_gap_s: f64,
    /// Level whose waveform carries beacons and check-ins (frames are
    /// always short-code).
    pub beacon_level: usize,
    pub thresholds_db: Vec<f32>,
    pub margin_db: f32,
    /// A member heard at least this well can relay for another.
    pub relay_snr_db: f32,
    /// After this many missed polls in a row a member is polled only
    /// every fourth round.
    pub miss_limit: u32,
    /// Member: with no beacon heard for this long, call "looking for a
    /// net" now and then (a member that hears one relays for us).
    pub seek_after_s: f64,
    /// ...about this often (randomised ±50%)...
    pub seek_every_s: f64,
    /// ...once the channel has been quiet this long.
    pub seek_quiet_s: f64,
    pub caps: Caps,
    /// Compressed direct messages ([`super::compress`]): a member
    /// announces it at check-in ([`CAPS_COMPRESS`]); NCS with this on
    /// compresses its messages to members that did. Bulletins and
    /// members' traffic go as they are.
    pub compress: bool,
    /// Software version as sent at check-in (see [`software_version`]).
    pub version: u8,
    pub seed: u64,
    /// NCS: prime the receivers with what NCS knows. Check-ins are expected
    /// in their window's slots, whose timing the beacon carries, with their
    /// header known and the check-in format and roster as priors; a polled
    /// member's reply is expected when it should start, with its header
    /// (at the floor) and its report's format as priors. Bursts outside the
    /// expected slots are received as usual. Off: the beacon carries no
    /// timing and nothing is primed.
    pub prime: bool,
    /// How far an expected burst may start from where NCS expects it
    /// (clock and audio latencies left after NCS's own calibration), s.
    pub slot_tol_s: f64,
    /// A guess at how long a receiver takes to hand a burst over after it
    /// ends: NCS sets check-in slots and expects replies that much later.
    /// NCS then learns the real offset from the bursts it decodes.
    pub rx_latency_s: f64,
    /// NCS: calls expected to check in (regulars), tried as priors on a
    /// check-in that does not decode on its own, after the members'.
    pub regulars: Vec<String>,
    /// NCS, primed: chorus check-in slots after the window's own slots
    /// (0: none). Stations whose caps say `chorus` check in there, all at
    /// once, each in a sub-band it picks at random; others use the
    /// ordinary slots. Over audio only ([`NetNode`]).
    pub chorus_slots: u8,
}

impl NetConfig {
    pub fn new(call: &str, net: &str, ncs: bool) -> Self {
        Self {
            call: call.trim().to_ascii_uppercase(),
            net: net.trim().to_ascii_uppercase(),
            ncs,
            turnaround_s: 0.3,
            max_burst_s: 8.0,
            reply_slack_s: 1.0,
            detect_s: 1.0,
            window_slots: 4,
            beacon_every_s: 300.0,
            quick_beacon_s: 45.0,
            idle_round_gap_s: 15.0,
            beacon_level: FLOOR,
            thresholds_db: crate::level::thresholds_db(),
            margin_db: 1.0,
            relay_snr_db: -8.0,
            miss_limit: 2,
            seek_after_s: 360.0,
            seek_every_s: 90.0,
            seek_quiet_s: 5.0,
            caps: Caps::default(),
            compress: false,
            version: software_version(),
            seed: crc32(call.as_bytes()) as u64 | 1,
            prime: true,
            slot_tol_s: 0.06,
            rx_latency_s: 0.0,
            regulars: Vec::new(),
            chorus_slots: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NetEvent {
    /// NCS: a station joined (`via`: only another member heard it).
    /// Member: someone joined (including us).
    CheckedIn {
        call: String,
        slot: u8,
        via: Option<String>,
    },
    /// Member: we are in the net, as `slot`.
    Joined {
        ncs: String,
        net: String,
        slot: u8,
    },
    /// A message arrived. `to` is `None` for a bulletin; `overheard` when it
    /// was meant for someone else (another member's report to NCS).
    Message {
        from: String,
        to: Option<String>,
        data: Vec<u8>,
        overheard: bool,
    },
    /// NCS: a member did not answer its poll.
    Missed {
        call: String,
    },
    /// NCS: traffic or a check-in arrived through another member.
    Relayed {
        source: String,
        via: String,
    },
    /// NCS: a station now goes through `via` (`None`: polled directly).
    Routed {
        call: String,
        via: Option<String>,
    },
    Closed,
}

/// NCS's view of one member.
#[derive(Clone, Debug, PartialEq)]
pub struct RosterEntry {
    pub slot: u8,
    pub call: String,
    /// Checked in through this member.
    pub via: Option<String>,
    /// How NCS hears it, and how it hears NCS (dB in 2.4 kHz).
    pub snr_up: Option<f32>,
    pub snr_down: Option<f32>,
    /// Its current uplink level cap.
    pub cap: usize,
    /// Seconds since NCS last heard from it (directly or relayed).
    pub last_heard_s: Option<f64>,
    pub missed: u32,
    /// Bytes of messages received from it.
    pub bytes_in: u64,
    /// Whom it hears, and how well.
    pub hears: Vec<(String, f32)>,
    /// What it announced at check-in (`None`: not known yet).
    pub caps: Option<Caps>,
    pub version: Option<u8>,
    /// Stations NCS reaches through it.
    pub children: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct NetStats {
    pub bursts: u32,
    pub polls: u32,
    /// NCS: messages that arrived through another member.
    pub relays: u32,
    /// Member: frames of other stations' traffic it forwarded.
    pub relayed_frames: u32,
    /// NCS: received lists sent.
    pub rx_lists: u32,
    /// Bulletin pieces sent (NCS, or a relay to its leaves), and how many
    /// of them were repairs.
    pub bul_pieces: u32,
    pub bul_repairs: u32,
    pub airtime_s: f64,
    pub rounds: u32,
    /// NCS: answers heard from the member it polled.
    pub replies: u32,
}

pub struct NetStation {
    cfg: NetConfig,
    air: Arc<dyn Air>,
    rng: NoiseGen,
    id: u8,
    net_name: String,
    /// Slot → call, as announced by NCS.
    calls: BTreeMap<u8, String>,
    events: Vec<NetEvent>,
    busy: bool,
    /// Decodes long-frame bursts (used without combining).
    harq: HarqRx,
    tx_at: f64,
    reply: Option<Outgoing>,
    on_air: bool,
    pub stats: NetStats,
    closed: bool,

    // NCS.
    members: BTreeMap<u8, Member>,
    open: bool,
    next_beacon_at: f64,
    window_end: f64,
    /// Slots to announce (new or re-checked-in).
    to_ack: Vec<u8>,
    round: VecDeque<u8>,
    round_no: u32,
    round_had_traffic: bool,
    next_round_at: f64,
    wait: Option<Wait>,
    deadline: Option<f64>,
    bulletin: Vec<u8>,
    /// The bulletin cut into coded blocks as it goes out.
    bul_tx: Vec<BulTx>,
    bul_sent_round: Option<u32>,
    /// Close messages still to send (repeated: one may collide).
    close_left: u32,
    /// Something happened in the last check-in window (a check-in, or a
    /// busy channel that may have been a collision): beacon again soon.
    window_activity: bool,
    /// The clock at the last [`NetStation::poll_tx`].
    last_now: f64,
    /// The received list went out for this round.
    rx_sent_round: Option<u32>,
    /// Something a received list should tell (a relayed message, a hole).
    rx_dirty: bool,
    /// Bulletin blocks announced in a received list.
    bul_told: u16,
    /// Leaves whose relay changed, to announce.
    tree_news: BTreeSet<u8>,
    /// Silence before each of our bursts (the host's PTT lead), s.
    lead_s: f64,
    /// Where the burst being handled sat on our clock (start of its
    /// preamble, end), when the host knows.
    rx_air: Option<(f64, f64)>,
    /// NCS, primed: the check-in window's slots (preamble of slot 0, slot
    /// length, slots), on our clock, as the beacon announced them.
    slots: Option<(f64, f64, u8)>,
    /// NCS, primed: the polled member's reply (predicted start before
    /// calibration, slot, headers).
    reply_exp: Option<(f64, u8, Option<ExpectedHeader>, Option<ExpectedHeader>)>,
    /// NCS: how much later than predicted check-ins and replies start (s).
    checkin_cal: Option<f64>,
    reply_cal: Option<f64>,
    pub prime_stats: PrimeStats,
    /// NCS: the window's chorus slots (start of the first, length, count).
    chorus: Option<(f64, f64, u8)>,
    /// Member: our chorus check-in (when to key, sub-band), and how many
    /// times we have tried to check in.
    chorus_at: Option<(f64, usize)>,
    checkin_tries: u32,

    // Member.
    ncs_call: Option<String>,
    my_slot: Option<u8>,
    checkin_at: Option<f64>,
    outbox: Outbox,
    burst_id: u8,
    last_sent: Vec<Sent>,
    level: usize,
    cap: usize,
    hold_until: f64,
    hold_ceiling: usize,
    /// The bulletin as decoded so far (blocks in order), split into messages.
    bul_in: Inbox,
    bul_rx: BTreeMap<u16, BulRx>,
    /// Blocks we know exist, and the next to append to `bul_in`.
    bul_known: u16,
    bul_next: u16,
    dir_in: Inbox,
    acked_report: Option<[u8; 7]>,
    snr_ncs: Option<f32>,
    heard: BTreeMap<u8, (f32, f64)>,
    acked_hears: BTreeMap<u8, f32>,
    overheard_up: BTreeMap<u8, MsgIn>,
    /// What NCS holds of each other station's messages.
    held: BTreeMap<u8, Held>,
    /// NCS asked us to forward this station's traffic this turn.
    relay_src: Option<u8>,
    overheard_checkins: Vec<Applicant>,
    /// Who the channel belongs to now (the slot NCS last polled).
    talker: Option<u8>,
    /// The last burst we overheard from each station, frame by frame: the
    /// uplink piece in it (source, message, offset, length), if any.
    overheard_last: BTreeMap<u8, Vec<Option<Piece>>>,
    /// Leaf: our relay (we never hear NCS's polls, only its).
    parent: Option<u8>,
    /// Every leaf's relay, as NCS announced.
    parents: BTreeMap<u8, u8>,
    /// Relay: our leaves.
    kids: BTreeMap<u8, Kid>,
    /// Relay: polling our leaves before answering NCS.
    turn: Option<Turn>,
    /// Relay: (leaf, answered) from the last turn, for NCS.
    kid_status: Vec<(u8, bool)>,
    started_at: Option<f64>,
    last_beacon_at: Option<f64>,
    last_rx_at: f64,
    next_seek_at: f64,
    /// The id of a net we hear traffic of (no beacon), to seek it.
    heard_net: Option<u8>,
}

impl NetStation {
    pub fn new(cfg: NetConfig, air: Arc<dyn Air>) -> Self {
        let id = net_id(&cfg.net);
        let rng = NoiseGen::new(cfg.seed);
        let open = cfg.ncs;
        let net_name = cfg.net.clone();
        Self {
            cfg,
            air,
            rng,
            id,
            net_name,
            calls: BTreeMap::new(),
            events: Vec::new(),
            busy: false,
            harq: HarqRx::new(),
            tx_at: 0.0,
            reply: None,
            on_air: false,
            stats: NetStats::default(),
            closed: false,
            members: BTreeMap::new(),
            open,
            next_beacon_at: 0.0,
            window_end: 0.0,
            to_ack: Vec::new(),
            round: VecDeque::new(),
            round_no: 0,
            round_had_traffic: false,
            next_round_at: 0.0,
            wait: None,
            deadline: None,
            bulletin: Vec::new(),
            bul_tx: Vec::new(),
            bul_sent_round: None,
            close_left: 0,
            window_activity: false,
            last_now: 0.0,
            rx_sent_round: None,
            rx_dirty: false,
            bul_told: 0,
            tree_news: BTreeSet::new(),
            lead_s: 0.0,
            rx_air: None,
            slots: None,
            reply_exp: None,
            checkin_cal: None,
            reply_cal: None,
            prime_stats: PrimeStats::default(),
            chorus: None,
            chorus_at: None,
            checkin_tries: 0,
            ncs_call: None,
            my_slot: None,
            checkin_at: None,
            outbox: Outbox::default(),
            burst_id: 0,
            last_sent: Vec::new(),
            level: FLOOR,
            cap: FLOOR,
            hold_until: f64::NEG_INFINITY,
            hold_ceiling: LADDER.len() - 1,
            bul_in: Inbox::default(),
            bul_rx: BTreeMap::new(),
            bul_known: 0,
            bul_next: 0,
            dir_in: Inbox::default(),
            acked_report: None,
            snr_ncs: None,
            heard: BTreeMap::new(),
            acked_hears: BTreeMap::new(),
            overheard_up: BTreeMap::new(),
            held: BTreeMap::new(),
            relay_src: None,
            overheard_checkins: Vec::new(),
            talker: None,
            overheard_last: BTreeMap::new(),
            parent: None,
            parents: BTreeMap::new(),
            kids: BTreeMap::new(),
            turn: None,
            kid_status: Vec::new(),
            started_at: None,
            last_beacon_at: None,
            last_rx_at: f64::NEG_INFINITY,
            next_seek_at: 0.0,
            heard_net: None,
        }
    }

    pub fn call(&self) -> &str {
        &self.cfg.call
    }

    pub fn is_ncs(&self) -> bool {
        self.cfg.ncs
    }

    pub fn net_name(&self) -> &str {
        &self.net_name
    }

    /// Member: our slot once NCS has taken our check-in.
    pub fn slot(&self) -> Option<u8> {
        self.my_slot
    }

    pub fn ncs_call(&self) -> Option<&str> {
        self.ncs_call.as_deref()
    }

    /// Member: the relay we go through, if NCS cannot reach us directly.
    pub fn relay_call(&self) -> Option<String> {
        self.calls.get(&self.parent?).cloned()
    }

    /// Member: the stations we relay for.
    pub fn leaf_calls(&self) -> Vec<String> {
        self.kids.keys().filter_map(|c| self.calls.get(c).cloned()).collect()
    }

    pub fn caps(&self) -> Caps {
        self.cfg.caps
    }

    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Everything that happened since the last call.
    pub fn take_events(&mut self) -> Vec<NetEvent> {
        std::mem::take(&mut self.events)
    }

    /// Member: a message for NCS (sent when polled). NCS: a bulletin for
    /// everyone.
    pub fn send(&mut self, data: &[u8]) {
        if self.cfg.ncs {
            let n = data.len().min(u16::MAX as usize);
            self.bulletin.extend((n as u16).to_le_bytes());
            self.bulletin.extend(&data[..n]);
        } else {
            self.outbox.push_msg(data);
        }
    }

    /// NCS: a message for one member. Returns false if it is not checked in.
    pub fn send_to(&mut self, call: &str, data: &[u8]) -> bool {
        let up = call.trim().to_ascii_uppercase();
        let Some(m) = self.members.values_mut().find(|m| m.call == up) else {
            return false;
        };
        if self.cfg.compress && m.compress {
            // Packing adds at most one byte: the result fits the 15 bits.
            let p = super::compress::pack(&data[..data.len().min(!LEN_PACKED as usize - 1)]);
            m.dir.extend((p.len() as u16 | LEN_PACKED).to_le_bytes());
            m.dir.extend(p);
            return true;
        }
        let n = data.len().min(u16::MAX as usize);
        m.dir.extend((n as u16).to_le_bytes());
        m.dir.extend(&data[..n]);
        true
    }

    /// NCS: allow check-ins (beacons) or stop them.
    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// NCS: end the net.
    pub fn close(&mut self) {
        if self.cfg.ncs && !self.closed {
            self.closed = true;
            self.wait = None;
            self.deadline = None;
            self.close_left = 3;
            self.events.push(NetEvent::Closed);
        }
    }

    /// Bytes still to send (member: to NCS; NCS: bulletins not yet held by
    /// every active member).
    pub fn pending(&self) -> usize {
        if self.cfg.ncs {
            let sealed: usize = self.bul_tx.iter().map(|b| b.len).sum();
            let low = self
                .direct()
                .map(|m| m.bul.map_or(0, |b| b.0 as usize))
                .min()
                .unwrap_or(self.bul_tx.len());
            self.bulletin.len() - sealed + self.bul_tx.iter().skip(low).map(|b| b.len).sum::<usize>()
        } else {
            self.outbox.pending()
        }
    }

    /// Member: the other stations, by slot.
    pub fn members(&self) -> &BTreeMap<u8, String> {
        &self.calls
    }

    /// The host is receiving a burst (or the channel is otherwise busy):
    /// do not key, and keep waiting for an answer that is on its way.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        if busy && self.cfg.ncs && self.last_now < self.window_end {
            self.window_activity = true;
        }
    }

    /// The burst to send now, if any.
    pub fn poll_tx(&mut self, now: f64) -> Option<Outgoing> {
        self.last_now = now;
        if self.on_air {
            return None;
        }
        self.started_at.get_or_insert(now);
        if self.busy {
            if let Some(d) = self.deadline.as_mut() {
                *d = d.max(now + self.cfg.reply_slack_s);
            }
            if let Some(d) = self.turn.as_mut().and_then(|t| t.deadline.as_mut()) {
                *d = d.max(now + self.cfg.reply_slack_s);
            }
            return None;
        }
        if now < self.tx_at {
            return None;
        }
        if let Some(r) = self.reply.take() {
            return Some(self.out(r));
        }
        if self.cfg.ncs {
            self.ncs_next(now).map(|b| self.out(b))
        } else {
            self.member_next(now).map(|b| self.out(b))
        }
    }

    fn out(&mut self, b: Outgoing) -> Outgoing {
        self.on_air = true;
        self.stats.bursts += 1;
        let dur = self.air.duration(b.wave, b.code, b.coded_bits());
        self.stats.airtime_s += dur;
        if self.cfg.ncs {
            self.expect_reply(&b, self.last_now + dur);
        }
        b
    }

    /// The host's PTT lead: silence it puts before each of our bursts.
    pub fn set_tx_lead(&mut self, lead_s: f64) {
        self.lead_s = lead_s;
    }

    /// Our burst has finished at `t`.
    pub fn on_tx_done(&mut self, t: f64) {
        self.on_air = false;
        self.tx_at = t;
        if self.cfg.ncs {
            match self.wait {
                Some(Wait::Reply { slot, .. }) => {
                    // A member we cannot hear may still be answering: wait
                    // out a whole burst, or the next poll lands on top of
                    // its reply where others hear it.
                    let unheard = self
                        .members
                        .get(&slot)
                        .is_none_or(|m| m.snr_up.is_none() || m.missed > 0);
                    let reply = if unheard {
                        self.cfg.max_burst_s
                    } else {
                        self.cfg.detect_s
                    };
                    self.deadline = Some(t + self.cfg.turnaround_s + reply + self.cfg.reply_slack_s);
                }
                Some(Wait::Nothing) => self.wait = None,
                None => {}
            }
        } else if let Some(turn) = self.turn.as_mut()
            && matches!(turn.stage, Some(Stage::Wait(_)))
            && turn.deadline.is_none()
        {
            // Our leaf's answer has surely started by then, if it is coming.
            turn.deadline = Some(t + self.cfg.turnaround_s + self.cfg.detect_s + self.cfg.reply_slack_s);
        }
    }

    /// A burst arrived, ending at `now`.
    pub fn on_rx(&mut self, inc: &Incoming, now: f64) {
        self.on_rx_at(inc, now, None);
    }

    /// [`NetStation::on_rx`], when the host knows where the burst sat on
    /// our clock: `air` is (start of its preamble, its end). A member times
    /// its check-in from the beacon's; NCS learns how late bursts start
    /// against its predictions.
    pub fn on_rx_at(&mut self, inc: &Incoming, now: f64, air: Option<(f64, f64)>) {
        self.rx_air = air;
        self.on_rx_inner(inc, now);
        self.rx_air = None;
    }

    fn on_rx_inner(&mut self, inc: &Incoming, now: f64) {
        self.tx_at = self.tx_at.max(now + self.cfg.turnaround_s);
        self.last_rx_at = now;
        let session = user_session(inc.user);
        self.heard_net.get_or_insert(session);
        // Another net's burst (a member with no net name takes the first
        // beacon's).
        let committed = self.cfg.ncs || self.ncs_call.is_some() || !self.cfg.net.is_empty();
        if committed && session != self.id {
            return;
        }
        // Long-frame bursts arrive as soft bits (HARQ's interface); a net
        // does not combine across bursts, so decode each burst on its own.
        let decoded: Vec<Option<Vec<u8>>> = match &inc.soft {
            Some(soft) if inc.code < LADDER.len() && LADDER[inc.code].frame == FrameSize::Long => {
                self.harq.receive(inc.code, 0, soft).frames
            }
            _ => inc.frames.clone(),
        };
        let frames: Vec<Option<Frame>> = decoded.iter().map(|f| f.as_deref().and_then(Frame::decode)).collect();
        if self.cfg.ncs {
            self.ncs_rx(inc, &frames, now);
        } else {
            self.member_rx(inc, &frames, now);
        }
    }

    fn tiny(&self, kind: u8, extra: u16, wave: Wave) -> Outgoing {
        Outgoing {
            td_ease: false,
            cue: false,
            wave,
            code: TINY,
            user: user_bits(TINY, self.id, kind),
            extra,
            frames: Vec::new(),
            spans: Vec::new(),
        }
    }

    fn ctrl(&self, frames: Vec<Frame>, wave: Wave) -> Outgoing {
        self.ctrl_hi(frames, wave, 0)
    }

    fn ctrl_hi(&self, frames: Vec<Frame>, wave: Wave, hi: u8) -> Outgoing {
        let size = LADDER[CTRL_CODE].frame_bytes();
        Outgoing {
            td_ease: false,
            cue: false,
            wave,
            code: CTRL_CODE,
            user: user_bits(CTRL_CODE, self.id, hi),
            extra: 0,
            frames: frames.iter().map(|f| f.encode(size)).collect(),
            spans: Vec::new(),
        }
    }

    fn level_for(&self, snr: f32) -> usize {
        level_of(&self.cfg, snr)
    }

    /// The fastest waveform whose header-only bursts get through at `snr`.
    fn tiny_wave(&self, snr: Option<f32>) -> Wave {
        let Some(snr) = snr else { return LADDER[FLOOR].wave };
        LADDER[self.level_for(snr)].wave
    }

    /// Full frames that fit in one burst at `level`: at least one, at most `cap`.
    fn room(&self, level: usize, cap: usize) -> usize {
        let mut room = 1;
        while room < cap
            && self
                .air
                .duration(LADDER[level].wave, level, frame_bits(level, room + 1))
                <= self.cfg.max_burst_s
        {
            room += 1;
        }
        room
    }

    fn slot_len(&self) -> f64 {
        self.air
            .duration(LADDER[self.cfg.beacon_level].wave, CTRL_CODE, frame_bits(CTRL_CODE, 1))
            + 2.0 * self.cfg.turnaround_s
            + SLOT_SLACK_S
    }
}

/// Slack at the end of a check-in slot, s.
const SLOT_SLACK_S: f64 = 0.3;

/// The fastest level whose frames get through at `snr`.
fn level_of(cfg: &NetConfig, snr: f32) -> usize {
    (0..cfg.thresholds_db.len().min(LADDER.len()))
        .rev()
        .find(|&l| snr >= cfg.thresholds_db[l] + cfg.margin_db)
        .unwrap_or(FLOOR)
}

fn smooth(old: Option<f32>, new: f32) -> f32 {
    match old {
        Some(o) => 0.5 * o + 0.5 * new,
        None => new,
    }
}
