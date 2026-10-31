//! One end of an Ion link: connect, ARQ with gear shifting, HARQ,
//! turn-over and disconnect, burst by burst and half duplex. It knows
//! nothing about audio: it hands out [`Outgoing`] bursts and takes
//! [`Incoming`] ones on the caller's clock. The protocol: `docs/link.md`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use super::compress::{StreamDecoder, StreamEncoder};
use super::frame::{Frame, Kind, TINY_ACK_FRAMES, Tiny, caps, flags};
use super::harq::{self, HarqRx, Soft};
use super::report::{ChanClass, ChanTrack};
use crate::chan::ChannelMeas;
use crate::fec;
use crate::level::{FLOOR, FLOOR_WAVE, Guard, LADDER, Wave};
use crate::phy_mfsk::MfskMode;

mod config;
mod expect;
mod gear;
mod receiver;
mod sender;
mod stream;

pub use config::{Config, Gear};
pub use expect::Expected;
pub use receiver::cheap_ack_waves;
pub use stream::StreamStatus;

/// Burst code of a header-only burst (on the air: 14, [`air_code`]).
pub const TINY: usize = 62;
/// Burst code of a burst holding one control frame (short code; on the
/// air: 15).
pub const CONTROL: usize = 63;
/// Bytes of a control frame.
pub const CONTROL_BYTES: usize = fec::K_SHORT / 8;
/// Most bytes in flight.
pub const WINDOW_BYTES: usize = 64 * 1024;
/// Most frames in one burst (a full ACK's bitmap is 64 bits).
const MAX_FRAMES: usize = harq::MAX_SLOTS;
/// Bitmap of a tiny ACK with every bit set: for a burst of more than
/// [`TINY_ACK_FRAMES`] frames, "all of them arrived".
const TINY_ALL: u64 = (1 << TINY_ACK_FRAMES) - 1;

/// The MFSK mode of a cue ([`Outgoing::cue`]).
pub const CUE_MODE: MfskMode = MfskMode::R1;

/// Seconds of a cue on the air (1.12 s).
pub fn cue_s() -> f64 {
    crate::phy_mfsk::cue_samples(CUE_MODE) as f64 / ion_dsp::dsp::AUDIO_RATE as f64
}

/// Air time of bursts, from the PHYs and FEC in use.
pub trait Air: Send + Sync {
    /// Seconds on air: `bits` coded bits of `code` (a ladder level, or
    /// [`CONTROL`]; [`TINY`] with 0 bits) in `wave`. See
    /// [`Outgoing::coded_bits`].
    fn duration(&self, wave: Wave, code: usize, bits: usize) -> f64;

    /// Most coded bits one data burst of ladder level `code` can carry
    /// (the PHY's symbol count limit). Unlimited unless the PHY says.
    fn max_bits(&self, _wave: Wave, _code: usize) -> usize {
        usize::MAX
    }

    /// Silence before every burst (the PTT lead), seconds, included in
    /// [`Air::duration`].
    fn lead_s(&self) -> f64 {
        0.0
    }
}

/// Coded bits of `frames` whole frames of burst `code`.
pub fn frame_bits(code: usize, frames: usize) -> usize {
    match code {
        TINY | crate::link::stream::TONES | crate::link::stream::STREAM => 0,
        CONTROL => frames * fec::n_short(),
        l => frames * LADDER[l].coded_bits(),
    }
}

/// Default [`Config::turbo`]: passes of the iterative receiver.
pub const TURBO_PASSES: usize = 2;
/// The iterative receiver's budget per burst: passes times data symbols
/// (a pass costs about 0.5 ms of CPU per symbol), so that the ACK of a
/// long burst is not held up.
pub const TURBO_SYMBOLS: usize = 1000;

/// No progress for this long: every retry and ACK goes in the floor
/// waveform until something gets through.
const STALL_S: f64 = 45.0;

/// Receiving side with something to send: ask for the turn once the
/// channel has been quiet this long, and again this much later each time.
const SPEAK_UP_S: f64 = 4.0;
const SPEAK_UP_AGAIN_S: f64 = 10.0;

/// Waveforms from the most robust up.
fn wave_rank(w: Wave) -> usize {
    match w {
        Wave::Mfsk(MfskMode::R0) => 0,
        Wave::Mfsk(MfskMode::R1) => 1,
        Wave::Ofdm(Guard::Slice) => 2,
        Wave::Ofdm(Guard::Long) => 3,
        Wave::Ofdm(Guard::Normal) => 4,
    }
}

/// The next more robust waveform for a tiny or control burst (not the
/// slice: only the receivers of one third of the band hear it).
fn wave_down(w: Wave) -> Wave {
    match w {
        Wave::Ofdm(Guard::Normal) => Wave::Ofdm(Guard::Long),
        Wave::Ofdm(_) => Wave::Mfsk(MfskMode::R1),
        _ => FLOOR_WAVE,
    }
}

fn more_robust(a: Wave, b: Wave) -> Wave {
    if wave_rank(a) <= wave_rank(b) { a } else { b }
}

/// The highest level whose waveform is no faster than `w`.
fn top_level_in(w: Wave) -> usize {
    (FLOOR..LADDER.len())
        .rev()
        .find(|&l| wave_rank(LADDER[l].wave) <= wave_rank(w))
        .unwrap_or(FLOOR)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Listening,
    Connecting,
    Connected,
    Disconnecting,
    Disconnected,
    /// Gave up: no answer after every retry, or the peer went silent.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Information sending station: sends data, receives ACKs.
    Sender,
    /// Information receiving station: receives data, sends ACKs.
    Receiver,
}

/// A burst to transmit.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub wave: Wave,
    /// Ladder level, [`CONTROL`] or [`TINY`].
    pub code: usize,
    /// For the PHY header: code in bits 0–3, session 4–11, burst number or
    /// tiny kind 12–15.
    pub user: u16,
    /// Tiny message body (header-only bursts); a slice data burst's slot;
    /// a cued burst's HARQ parity field ([`harq::partial_field`]).
    pub extra: u16,
    /// Encoded frames, each one codeword's information bytes.
    pub frames: Vec<Vec<u8>>,
    /// Per frame, the span of its transmission order sent: (start, bits).
    /// Empty: every frame whole, from position 0. A long frame sent from
    /// a later position is a HARQ increment (see [`harq`]).
    pub spans: Vec<(usize, usize)>,
    /// A full-band OFDM data burst that goes behind an MFSK cue
    /// ([`CUE_MODE`], [`cue_s`] long, carrying its header and `extra`), only
    /// to a peer that agreed to [`caps::CUE`].
    pub cue: bool,
    /// Experiment (`ION_TDCLIP=1`): the whole-burst clipper eased by
    /// `audio::TD_EASE_DB`, for a peer reporting fast fading.
    pub td_ease: bool,
}

impl Outgoing {
    /// Seconds on the air, the cue included.
    pub fn duration(&self, air: &dyn Air) -> f64 {
        let cue = if self.cue { cue_s() } else { 0.0 };
        air.duration(self.wave, self.code, self.coded_bits()) + cue
    }

    /// Coded bits of the burst's data part.
    pub fn coded_bits(&self) -> usize {
        if self.spans.is_empty() {
            frame_bits(self.code, self.frames.len())
        } else {
            self.spans.iter().map(|s| s.1).sum()
        }
    }

    /// Span of frame `i`: (start, bits).
    pub fn span(&self, i: usize) -> (usize, usize) {
        self.spans.get(i).copied().unwrap_or((0, frame_bits(self.code, 1)))
    }
}

/// A burst received.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub wave: Wave,
    pub code: usize,
    pub user: u16,
    pub extra: u16,
    /// One entry per codeword; `None` if it failed to decode. Empty for
    /// long-frame data bursts, whose codewords arrive in `soft`.
    pub frames: Vec<Option<Vec<u8>>>,
    /// Long-frame data bursts: the soft bits, for the station to lay out,
    /// combine with what it has stored, and decode.
    pub soft: Option<Soft>,
    /// Mean SNR, dB in 2.4 kHz.
    pub snr_db: f32,
    /// The channel the receiver measured over the burst (delay and
    /// Doppler spread, effective SNR; see [`crate::chan`]).
    pub chan: ChannelMeas,
    /// The receiving side's pick of the third of the passband for slice
    /// bursts (`crate::slots`; 0 the centre), as of this burst.
    pub slot: u8,
    /// OFDM data bursts: what the receiver saw, to decode the burst again
    /// with what the decoder learnt (`Config::turbo`).
    pub grid: Option<Arc<crate::phy::BurstGrid>>,
}

/// For the PHY header: `code`'s [`air_code`] in bits 0–3, `session`
/// 4–11, `hi` 12–15 (a data burst's number in 12–13 and HARQ check bits in
/// 14–15; a tiny message's kind).
pub fn user_bits(code: usize, session: u8, hi: u8) -> u16 {
    air_code(code) | (session as u16) << 4 | (hi as u16 & 0xF) << 12
}

/// Ladder levels in MFSK (they come first).
fn mfsk_levels() -> usize {
    LADDER.iter().take_while(|l| matches!(l.wave, Wave::Mfsk(_))).count()
}

/// The 4-bit code burst `code` goes out with. The receiver knows which
/// waveform it heard, so levels are numbered per waveform: the MFSK levels
/// from 0, the OFDM levels from 2 (OFDM codes 0 and 1 keep meaning short
/// frames, as MFSK R0's and R1's: net control bursts are code 0 in any
/// waveform); [`TINY`] 14, [`CONTROL`] 15.
pub fn air_code(code: usize) -> u16 {
    match code {
        TINY => 14,
        CONTROL => 15,
        l if l < mfsk_levels() => l as u16,
        l => (l - mfsk_levels() + 2) as u16,
    }
}

/// Burst code of a burst heard in `wave` with PHY header `user` (the
/// inverse of [`air_code`]).
pub fn user_code(wave: Wave, user: u16) -> usize {
    match (user & 0xF) as usize {
        14 => TINY,
        15 => CONTROL,
        c => match wave {
            Wave::Ofdm(_) if c >= 2 => c + mfsk_levels() - 2,
            _ => c,
        },
    }
}

pub fn user_session(user: u16) -> u8 {
    (user >> 4) as u8
}

pub fn user_hi(user: u16) -> u8 {
    (user >> 12) as u8 & 0xF
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Payload bytes the peer has acknowledged.
    pub bytes_acked: u64,
    /// Payload bytes delivered to us, in order (stream bytes: compressed
    /// records when the link compresses).
    pub bytes_received: u64,
    /// Bytes given to [`Station::send`], and bytes handed out by
    /// [`Station::take_received`]'s queue: the application's data, before
    /// compression and after it is undone.
    pub text_sent: u64,
    pub text_received: u64,
    /// Compressed records that failed to decode (delivered as they were).
    pub codec_errors: u32,
    pub data_bursts: u32,
    /// Data bursts sent behind a cue.
    pub cued_bursts: u32,
    pub control_bursts: u32,
    /// Whole frames sent, and how many of them were resends.
    pub frames_sent: u32,
    pub frames_resent: u32,
    /// HARQ increments sent, and their coded bits.
    pub increments_sent: u32,
    pub increment_bits: u64,
    /// Receiving side: codewords that failed (after combining), and
    /// codewords decoded only thanks to combining.
    pub codewords_failed: u32,
    pub harq_recovered: u32,
    /// Codewords the iterative receiver (`Config::turbo`) decoded that a
    /// plain decode had not, and the bursts it ran on.
    pub turbo_recovered: u32,
    pub turbo_bursts: u32,
    pub polls: u32,
    /// Stream periods sent or taken in, and blocks delivered.
    pub stream_periods: u32,
    pub stream_blocks: u32,
    /// Receiving side: each period's option, frames decoded and tried,
    /// SNR, and the answer to it (0 DONE, 1 SLOWER, 2 FASTER, 3 none).
    pub stream_log: Vec<(u8, u8, u8, f32, u8)>,
    /// Seconds we were on the air.
    pub airtime_s: f64,
    /// Level of each data burst we sent, in order.
    pub levels: Vec<usize>,
    /// Data bursts of which nothing got through (the ACK said so, or the
    /// peer never heard the burst), and how many of them were OFDM.
    pub failed_bursts: u32,
    pub failed_ofdm: u32,
    /// Sending side: per data burst, the peer's channel class in force
    /// and the target level (`super::report`).
    pub classes: Vec<(ChanClass, usize)>,
    /// Receiving side: per data burst heard, its level, the burst's
    /// measurement (delay ms, Doppler Hz, effective and mean SNR dB) and
    /// the class we report back.
    pub heard: Vec<(usize, [f32; 4], ChanClass)>,
}

/// One slot of a data burst sent.
#[derive(Clone, Copy, Debug)]
struct Sent {
    off: u32,
    /// Coded bits on the air.
    bits: usize,
    /// Bits of this codeword the peer holds once this slot arrives.
    held: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Awaiting {
    Nothing,
    ConnAck,
    Ack,
    /// We sent Turn: waiting for the peer's first burst.
    TurnTaken,
    DiscAck,
    /// Stream gear: our Start's Go, our End's Report.
    StreamGo,
    StreamReport,
}

pub struct Station {
    cfg: Config,
    air: Arc<dyn Air>,
    state: State,
    role: Role,
    peer: String,
    session: u8,
    /// Earliest time we may key (the channel was busy until just before).
    tx_at: f64,
    awaiting: Awaiting,
    /// When to give up waiting for `awaiting`; set once our burst ends.
    deadline: Option<f64>,
    /// The wait that set `deadline`.
    reply_wait: f64,
    /// The link's ID clock ([`Station::link_t0`]), and the ID period and
    /// the length of the peer's CW ID ([`Station::set_ids`]).
    link_t0: Option<f64>,
    ids: Option<(f64, f64)>,
    /// Until we hear the caller after our ConnAck, each ConnAck we send
    /// restarts the ID clock (the caller times it from the one it heard).
    answering: bool,
    /// Burst to resend when the deadline passes (call, turn, goodbye).
    retry: Option<Outgoing>,
    tries: u32,
    /// Answer to send at the next opportunity.
    reply: Option<Outgoing>,
    /// The burst just handed out was the last one (DiscAck): close after it.
    close_after_tx: bool,
    last_heard: f64,
    /// Last time the link moved: an ACK, data, a turn or the connection.
    last_progress: f64,
    last_tx_end: f64,
    last_tx_wave: Wave,
    last_tx_code: usize,
    /// A tone answer in `reply` goes only until then (it would run into
    /// the next period).
    reply_by: Option<f64>,
    // Stream gear.
    stream_tx: Option<stream::StreamTx>,
    /// A block whose DONE was heard and the next block followed at once:
    /// the next Go confirms it.
    stream_prev: Option<stream::StreamTx>,
    stream_rx: Option<stream::StreamRx>,
    stream_ids: u8,
    stream_hold_until: f64,
    /// The last block's pieces needed over pieces sent.
    stream_eff: f32,
    /// ARQ goodput yardstick: data bytes ARQ got acknowledged, the seconds
    /// it spent sending data, and when its current stretch began.
    arq_bytes: u64,
    arq_time: f64,
    arq_from: Option<f64>,
    /// The current run of stream blocks: when its first Start went, the
    /// bytes of its blocks that heard DONE, and how many.
    run_from: Option<f64>,
    run_bytes: u64,
    run_blocks: u32,
    /// Since when data bursts have sent only what was in flight, so the
    /// queue can stream.
    drain_since: Option<f64>,
    tone_count: u32,
    last_tone: Option<(bool, crate::link::stream::Msg)>,
    // Sending side.
    out: VecDeque<u8>,
    /// Stream offset of `out[0]`.
    next_offset: u32,
    /// Sent, not yet acknowledged: offset → bytes.
    inflight: BTreeMap<u32, Vec<u8>>,
    level: usize,
    /// Highest level the peer's SNR report allows.
    target: usize,
    /// Whole bursts lost at each level since it last got one through.
    fails: [u8; LADDER.len()],
    /// Each level's recent share of fresh frames delivered (EWMA, newest
    /// weighted [`SUCCESS_WEIGHT`]); `None` until it has been tried.
    success: [Option<f32>; LADDER.len()],
    /// When each level's `success` last changed.
    success_at: [f64; LADDER.len()],
    /// The last ACKs' SNR reports, newest last (for `smooth_target`).
    snr_reports: VecDeque<f32>,
    /// The SNR the gear last aimed by (for `cue_below_db`).
    aim_db: Option<f32>,
    /// Each level is not used before this time, nor are faster ones: all
    /// of them, or (`hold_all` false) only those whose guard is no longer
    /// than its own (see [`Station::usable`]).
    blocked_until: [f64; LADDER.len()],
    hold_all: [bool; LADDER.len()],
    burst_id: u8,
    /// The burst number the peer last showed it heard. A new burst never
    /// takes it: numbers are 2 bits, and a poll is answered with the ACK of
    /// the last burst heard, which must never pass for the new burst's.
    peer_heard: Option<u8>,
    /// Longest next data burst before [`Station::clean_burst_s`] caps it
    /// (`Config::burst_adapt`).
    burst_s: f64,
    /// The peer's SNR report before the last one (`burst_steady_db`).
    last_report_db: Option<f32>,
    /// Receiving side: data bursts left before `cheap_ack_margin_db` may
    /// be used again and ACKs may go faster than `ack_cap` (a poll showed
    /// an ACK lost).
    cheap_ack_off: u8,
    /// The most a poll lets our ACKs go in while `cheap_ack_off` lasts (the
    /// way back may be weaker than the way here). Each poll after the cap
    /// ran out doubles `ack_cap_bursts`.
    ack_cap: Option<Wave>,
    ack_cap_bursts: u8,
    /// Sending side: the waveform of the last ACK heard (it may be slower
    /// than our data's), and how well we hear the peer, for the Turn.
    peer_ack_wave: Option<Wave>,
    heard_peer_db: Option<f32>,
    /// Each slot of the last data burst, in order.
    last_burst: Vec<Sent>,
    /// What the peer has stored for HARQ, in its order: (offset, bits it
    /// holds). Changes only on an ACK for the burst just sent.
    at_peer: Vec<(u32, usize)>,
    /// The waveform all of `at_peer` was sent in.
    peer_wave: Wave,
    /// The part of `at_peer` the last data burst left out
    /// (`Config::harq_partial`): it stays stored, ahead of the burst's
    /// failures.
    at_peer_rest: Vec<(u32, usize)>,
    peer_wants_turn: bool,
    /// When the peer asked for the turn (for `break_in_s`).
    turn_asked_at: f64,
    peer_wants_disc: bool,
    want_disc: bool,
    /// [`Station::disconnect_when_sent`]: when what is left is dropped.
    disc_by: Option<f64>,
    /// Receiving side: when we may next ask for the turn unasked.
    next_speak_up: f64,
    /// Stream bytes queued so far, and per `send` its end in the stream and
    /// in the application's bytes (for [`Station::text_progress`]).
    stream_total: u64,
    sends: VecDeque<(u64, u64)>,
    /// The peer's last channel class (sending side), or our own (the
    /// channel is the same both ways) after taking the turn.
    peer_class: ChanClass,
    /// The third the peer wants our slice bursts in, and the one our
    /// receivers want ours in (`crate::slots`).
    peer_slot: u8,
    /// The peer's last reported Doppler spread, Hz (NaN: none).
    peer_doppler_hz: f32,
    rx_slot: u8,
    // Receiving side.
    /// What we measure of the channel over the link (for the report).
    chan: ChanTrack,
    /// Effective SNR of the last burst heard.
    last_rx_eff: f32,
    delivered: u32,
    rx_pending: BTreeMap<u32, Vec<u8>>,
    inbox: Vec<u8>,
    last_rx_snr: f32,
    /// What we would ACK: (burst, bitmap, frames in that burst, failures
    /// stored).
    last_ack: (u8, u64, usize, bool),
    harq: HarqRx,
    /// Level of the peer's last data burst (where to start if we get the turn).
    rx_level: usize,
    stats: Stats,
    sessions: u8,
    // Compression (`cfg.compress`).
    /// Whether this link's compression has been settled (connected).
    agreed: bool,
    /// The [`caps`] both ends have (settled at connect).
    caps: u8,
    /// Data given to `send` before that, kept whole until we know.
    unsent: Vec<Vec<u8>>,
    tx_codec: Option<StreamEncoder>,
    rx_codec: Option<StreamDecoder>,
}

impl Station {
    pub fn new(cfg: Config, air: Arc<dyn Air>) -> Self {
        let burst_s = cfg.max_burst_s;
        let seed = cfg.call.bytes().fold(0u8, |a, b| a.wrapping_mul(31).wrapping_add(b));
        Self {
            cfg,
            air,
            state: State::Idle,
            role: Role::Receiver,
            peer: String::new(),
            session: 0,
            tx_at: 0.0,
            awaiting: Awaiting::Nothing,
            deadline: None,
            reply_wait: 0.0,
            link_t0: None,
            ids: None,
            answering: false,
            retry: None,
            tries: 0,
            reply: None,
            close_after_tx: false,
            last_heard: 0.0,
            last_progress: 0.0,
            last_tx_end: 0.0,
            last_tx_wave: FLOOR_WAVE,
            last_tx_code: TINY,
            reply_by: None,
            stream_tx: None,
            stream_prev: None,
            stream_rx: None,
            stream_ids: seed,
            stream_hold_until: 0.0,
            stream_eff: 0.0,
            arq_bytes: 0,
            arq_time: 0.0,
            arq_from: None,
            run_from: None,
            run_bytes: 0,
            run_blocks: 0,
            drain_since: None,
            tone_count: 0,
            last_tone: None,
            out: VecDeque::new(),
            next_offset: 0,
            inflight: BTreeMap::new(),
            level: FLOOR,
            target: FLOOR,
            fails: [0; LADDER.len()],
            success: [None; LADDER.len()],
            success_at: [f64::NEG_INFINITY; LADDER.len()],
            snr_reports: VecDeque::new(),
            aim_db: None,
            blocked_until: [f64::NEG_INFINITY; LADDER.len()],
            hold_all: [true; LADDER.len()],
            burst_id: 0,
            peer_heard: None,
            burst_s,
            last_report_db: None,
            cheap_ack_off: 0,
            ack_cap: None,
            ack_cap_bursts: receiver::CHEAP_ACK_OFF_BURSTS / 2,
            peer_ack_wave: None,
            heard_peer_db: None,
            last_burst: Vec::new(),
            at_peer: Vec::new(),
            peer_wave: FLOOR_WAVE,
            at_peer_rest: Vec::new(),
            peer_wants_turn: false,
            turn_asked_at: 0.0,
            peer_wants_disc: false,
            want_disc: false,
            disc_by: None,
            next_speak_up: 0.0,
            stream_total: 0,
            sends: VecDeque::new(),
            peer_class: ChanClass::default(),
            peer_slot: 0,
            peer_doppler_hz: f32::NAN,
            rx_slot: 0,
            chan: ChanTrack::default(),
            last_rx_eff: 0.0,
            delivered: 0,
            rx_pending: BTreeMap::new(),
            inbox: Vec::new(),
            last_rx_snr: 0.0,
            last_ack: (0, 0, 0, false),
            harq: HarqRx::new(),
            rx_level: FLOOR,
            stats: Stats::default(),
            sessions: seed,
            agreed: false,
            caps: 0,
            unsent: Vec::new(),
            tx_codec: None,
            rx_codec: None,
        }
    }

    /// Whether this link compresses (both ends agreed at connect).
    pub fn compressing(&self) -> bool {
        self.tx_codec.is_some()
    }

    /// The [`caps`] this link uses (both ends have them).
    pub fn caps(&self) -> u8 {
        self.caps
    }

    /// The [`caps`] we offer or accept.
    fn own_caps(&self) -> u8 {
        let mut c = 0;
        if self.cfg.cue {
            c |= caps::CUE;
        }
        if self.cfg.stream {
            c |= caps::STREAM;
        }
        c
    }

    // -- application side -----------------------------------------------------

    pub fn state(&self) -> State {
        self.state
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn peer(&self) -> &str {
        &self.peer
    }

    pub fn call(&self) -> &str {
        &self.cfg.call
    }

    /// Current data level (meaningful on the sending side).
    pub fn level(&self) -> usize {
        self.level
    }

    /// The SNR the gear aims by (dB in 2.4 kHz), once there is a report.
    pub fn aim_db(&self) -> Option<f32> {
        self.aim_db
    }

    /// The level the peer's last SNR report allows.
    pub fn target_level(&self) -> usize {
        self.target
    }

    /// The third of the passband our slice bursts go in (the peer's pick,
    /// 0 the centre, or 0 with `slice_slots` off), and the one our
    /// receivers asked for in our last ACK (`crate::slots`).
    pub fn slots(&self) -> (u8, u8) {
        (if self.cfg.slice_slots { self.peer_slot } else { 0 }, self.rx_slot)
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Wait for a call.
    pub fn listen(&mut self) {
        self.reset_link();
        self.state = State::Listening;
    }

    /// Call `peer`. The call goes out in the floor waveform.
    pub fn connect(&mut self, peer: &str, now: f64) {
        self.reset_link();
        self.peer = peer.to_ascii_uppercase();
        self.sessions = self.sessions.wrapping_add(97);
        self.session = self.sessions;
        self.state = State::Connecting;
        self.role = Role::Sender;
        self.tx_at = now;
        self.last_progress = now;
        let mut f = Frame::new(Kind::ConnReq, self.session as u32);
        let mut text = format!("{} {} {}", self.cfg.call, self.peer, self.cfg.max_level);
        text.truncate(Frame::max_payload(CONTROL_BYTES));
        f.payload = text.into_bytes();
        if self.cfg.compress {
            f.flags |= flags::COMPRESS;
        }
        f.flags |= self.own_caps() << flags::CAPS_SHIFT;
        // Armed as an expired wait: the next poll sends it as the first try.
        self.retry = Some(self.control(f, FLOOR_WAVE));
        self.awaiting = Awaiting::ConnAck;
        self.tries = 0;
        self.deadline = Some(now);
    }

    /// Queue bytes for the peer. With [`Config::compress`] on, each call
    /// is compressed on its own terms (records of up to
    /// [`super::compress::CHUNK`] bytes); before the link has agreed
    /// whether to compress, the data waits whole.
    pub fn send(&mut self, data: &[u8]) {
        self.stats.text_sent += data.len() as u64;
        if self.cfg.compress && !self.agreed {
            self.unsent.push(data.to_vec());
        } else {
            self.queue(data);
        }
    }

    fn queue(&mut self, data: &[u8]) {
        let before = self.out.len();
        match &mut self.tx_codec {
            Some(c) => {
                let mut wire = Vec::new();
                c.push(data, &mut wire);
                self.out.extend(wire);
            }
            None => self.out.extend(data),
        }
        self.stream_total += (self.out.len() - before) as u64;
        let text_end = self.sends.back().map_or(0, |s| s.1) + data.len() as u64;
        self.sends.push_back((self.stream_total, text_end));
    }

    /// Bytes given to [`Station::send`] that the peer has acknowledged, in
    /// order, and those whose transmission has begun (a message counts once
    /// its first byte has gone out).
    pub fn text_progress(&self) -> (u64, u64) {
        let sent = self.stream_total - self.out.len() as u64;
        // The oldest byte still unacknowledged, counted back from `next_offset`.
        let back = self
            .inflight
            .keys()
            .map(|&k| self.next_offset.wrapping_sub(k))
            .max()
            .unwrap_or(0);
        let acked = sent.saturating_sub(back as u64);
        let mut acked_text = 0;
        let mut started_text = 0;
        let mut start = 0;
        for &(end, text_end) in &self.sends {
            if end <= acked {
                acked_text = text_end;
            }
            if start < sent {
                started_text = text_end;
            }
            start = end;
        }
        (acked_text, started_text)
    }

    fn has_data(&self) -> bool {
        !self.out.is_empty() || !self.inflight.is_empty()
    }

    /// Settles compression for this link (`on`: both ends have it) and
    /// queues what was waiting.
    fn agree(&mut self, on: bool) {
        self.agreed = true;
        self.tx_codec = on.then(StreamEncoder::new);
        self.rx_codec = on.then(StreamDecoder::new);
        for d in std::mem::take(&mut self.unsent) {
            self.queue(&d);
        }
    }

    /// Bytes received so far, in order (taken).
    pub fn take_received(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.inbox)
    }

    /// Bytes queued or in flight (stream bytes, plus data still waiting
    /// for the link to agree on compression).
    pub fn pending(&self) -> usize {
        self.out.len()
            + self.inflight.values().map(Vec::len).sum::<usize>()
            + self.unsent.iter().map(Vec::len).sum::<usize>()
            + self.stream_tx.as_ref().map_or(0, |t| t.block.len())
            + self.stream_prev.as_ref().map_or(0, |t| t.block.len())
    }

    /// End the link now: drop what is still queued and say goodbye. A peer
    /// that does not answer gets [`Config::disc_retries`] tries, and the
    /// link closes anyway.
    pub fn disconnect(&mut self) {
        self.want_disc = true;
        self.disc_by = None;
        match self.state {
            State::Connecting => {
                self.clear_wait();
                self.state = State::Disconnected;
            }
            State::Connected => {
                self.stream_tx = None;
                self.stream_prev = None;
                self.stream_rx = None;
                self.out.clear();
                self.inflight.clear();
                self.unsent.clear();
                self.last_burst.clear();
                self.sends.clear();
                self.reply = None;
                let wave = match self.role {
                    Role::Sender => self.cur_wave(),
                    Role::Receiver => LADDER[self.rx_level].wave,
                };
                let b = self.tiny(Tiny::DiscReq, wave);
                self.state = State::Disconnecting;
                self.awaiting = Awaiting::DiscAck;
                self.retry = Some(b);
                self.tries = 0;
                self.deadline = Some(f64::NEG_INFINITY);
            }
            _ => {}
        }
    }

    /// End the link once what is queued has been acknowledged (a host
    /// program's DISCONNECT); after `within_s` as [`Station::disconnect`].
    pub fn disconnect_when_sent(&mut self, now: f64, within_s: f64) {
        if self.state == State::Connected && self.pending() > 0 {
            self.want_disc = true;
            self.disc_by = Some(now + within_s);
        } else {
            self.disconnect();
        }
    }

    // -- radio side -------------------------------------------------------------

    /// What to transmit now, if anything. The caller must not call it while
    /// the other side is on the air (carrier sense).
    pub fn poll_tx(&mut self, now: f64) -> Option<Outgoing> {
        if now < self.tx_at {
            return None;
        }
        if matches!(
            self.state,
            State::Idle | State::Listening | State::Disconnected | State::Failed
        ) {
            return None;
        }
        if self.disc_by.is_some_and(|t| now >= t) {
            self.disconnect();
        }
        if let Some(r) = self.reply.take() {
            if self.answering && r.code == TINY && user_hi(r.user) == Tiny::CONN_ACK_KIND {
                self.link_t0 = Some(now);
            }
            if r.code == crate::link::stream::TONES {
                if self.reply_by.take().is_some_and(|by| now > by) {
                    self.stream_late(&r);
                    return None;
                }
                self.stream_note_answer(&r);
            }
            return Some(self.note_tx(r));
        }
        // New data while waiting on a check-in's answer: send it, its ACK
        // settles both.
        if self.awaiting == Awaiting::Ack
            && self.role == Role::Sender
            && self.last_burst.is_empty()
            && !self.out.is_empty()
        {
            self.clear_wait();
        }
        if self.awaiting != Awaiting::Nothing {
            let deadline = self.deadline?;
            if now < deadline {
                return None;
            }
            self.tries += 1;
            // Once connected, ride out fades: give up only after a long
            // silence. The call and the goodbye get a fixed number of tries.
            let give_up = match self.awaiting {
                Awaiting::Ack | Awaiting::TurnTaken => now - self.last_progress > self.cfg.link_timeout_s,
                Awaiting::DiscAck => self.tries > self.cfg.disc_retries,
                _ => self.tries > self.cfg.retries,
            };
            if give_up && matches!(self.awaiting, Awaiting::StreamGo | Awaiting::StreamReport) {
                // The peer did not answer the stream's handshake: back to
                // the link with the block (the link's own timeout decides
                // whether the peer is gone).
                self.awaiting = Awaiting::Nothing;
                self.deadline = None;
                self.retry = None;
                self.stream_giveup();
                return None;
            }
            if give_up {
                self.state = match self.awaiting {
                    Awaiting::DiscAck => State::Disconnected,
                    _ => State::Failed,
                };
                self.awaiting = Awaiting::Nothing;
                return None;
            }
            self.deadline = None;
            let b = match self.awaiting {
                // A lost ACK: ask for it again rather than resending data.
                Awaiting::Ack => {
                    self.stats.polls += 1;
                    let wave = self.retry_wave(self.cur_wave(), now);
                    self.tiny(Tiny::Poll, wave)
                }
                _ => {
                    let mut b = self.retry.clone()?;
                    if b.code >= LADDER.len() {
                        b.wave = self.retry_wave(b.wave, now);
                    }
                    b
                }
            };
            return Some(self.note_tx(b));
        }
        if self.role == Role::Receiver {
            if self.state == State::Connected && now - self.last_progress > self.cfg.link_timeout_s {
                self.state = State::Failed;
                return None;
            }
            // Something to send and the sender has gone quiet: ask.
            if self.state == State::Connected
                && self.has_data()
                && !self.streaming()
                && now - self.last_heard >= SPEAK_UP_S
                && now - self.last_tx_end >= SPEAK_UP_S
                && now >= self.next_speak_up
            {
                self.next_speak_up = now + SPEAK_UP_AGAIN_S;
                let wave = self.capped_ack_wave(LADDER[self.rx_level].wave, now);
                let b = self.ack_burst(wave);
                return Some(self.note_tx(b));
            }
            return None;
        }
        if self.state != State::Connected {
            return None;
        }
        if let Some(b) = self.stream_sender_poll(now) {
            return b;
        }
        let wave = self.cur_wave();
        let nothing_left = self.out.is_empty() && self.inflight.is_empty();
        // A peer leaving with data still to send gets the turn first.
        if nothing_left && (self.want_disc || self.peer_wants_disc && !self.peer_wants_turn) {
            self.state = State::Disconnecting;
            let b = self.tiny(Tiny::DiscReq, wave);
            return Some(self.send_awaiting(b, Awaiting::DiscAck));
        }
        let break_in = self.peer_wants_turn && now - self.turn_asked_at >= self.cfg.break_in_s;
        if self.peer_wants_turn && (nothing_left || break_in) {
            let b = self.tiny(
                Tiny::Turn {
                    snr_db: self.heard_peer_db,
                },
                wave,
            );
            return Some(self.send_awaiting(b, Awaiting::TurnTaken));
        }
        if let Some(b) = self.stream_start(now) {
            return Some(b);
        }
        if nothing_left {
            if now - self.last_tx_end < self.cfg.idle_s {
                return None;
            }
            let b = self.tiny(Tiny::Idle, wave);
            return Some(self.send_awaiting(b, Awaiting::Ack));
        }
        self.arq_from.get_or_insert(now);
        let b = self.data_burst();
        Some(self.note_tx(b))
    }

    /// Our burst finished at `t` (the last sample left).
    pub fn on_tx_done(&mut self, t: f64) {
        self.last_tx_end = t;
        if self.last_tx_code == crate::link::stream::STREAM
            && let Some(s) = self.stream_tx.as_mut()
        {
            s.pause_from = Some(t);
        }
        if self.last_tx_code == CONTROL
            && let Some(r) = self.stream_rx.as_mut()
            && r.live
        {
            r.go_out = true;
        }
        if self.close_after_tx {
            self.close_after_tx = false;
            self.state = State::Disconnected;
            return;
        }
        if self.awaiting != Awaiting::Nothing {
            // The answer comes back in the waveform we just used, or an ACK
            // in the slower one the last came in.
            let answer_in = |w: Wave| {
                self.air
                    .duration(w, TINY, 0)
                    .max(self.air.duration(w, CONTROL, frame_bits(CONTROL, 1)))
            };
            let mut answer = answer_in(self.last_tx_wave);
            if self.awaiting == Awaiting::Ack
                && let Some(w) = self.peer_ack_wave
            {
                answer = answer.max(answer_in(w));
            }
            let mut reply = self.cfg.turnaround_s + answer;
            if self.awaiting == Awaiting::TurnTaken {
                // The peer answers a Turn with a data burst (its first is
                // never longer than `max_burst_s`, see `burst_adapt`).
                reply += self.cfg.max_burst_s;
            }
            self.reply_wait = reply + self.cfg.reply_slack_s;
            // The peer's CW ID goes ahead of its first burst after each ID
            // time: if one has come since it last keyed (counted back a
            // whole long burst, to be sure), its answer comes that much later.
            if let (Some(t0), Some((every, peer_id_s))) = (self.link_t0, self.ids) {
                let keyed = self.last_heard - self.cfg.max_burst_long_s;
                let next = t0 + (((keyed - t0) / every).floor() + 1.0).max(1.0) * every;
                if next <= t + self.reply_wait {
                    self.reply_wait += peer_id_s;
                }
            }
            self.deadline = Some(t + self.reply_wait);
        }
    }

    /// Mid-link CW IDs every `every_s`, on [`Station::link_t0`]'s clock; the
    /// peer's lasts `peer_id_s`.
    pub fn set_ids(&mut self, every_s: f64, peer_id_s: f64) {
        self.ids = Some((every_s, peer_id_s));
    }

    /// When the answering station keyed the ConnAck the caller heard: both
    /// ends' ID clock (the same instant at either end, to within the audio
    /// path's delay).
    pub fn link_t0(&self) -> Option<f64> {
        self.link_t0
    }

    /// A burst arrived, ending at `now`.
    pub fn on_rx(&mut self, inc: &Incoming, now: f64) {
        // Tones in a pause leave the stream's schedule alone.
        if inc.code != crate::link::stream::TONES {
            self.tx_at = now + self.cfg.turnaround_s;
        }
        self.next_speak_up = 0.0;
        let frames: Vec<Option<Frame>> = inc
            .frames
            .iter()
            .map(|f| f.as_deref().and_then(Frame::decode))
            .collect();
        let tiny = (inc.code == TINY)
            .then(|| Tiny::decode(user_hi(inc.user), inc.extra))
            .flatten();

        if self.state == State::Listening {
            self.listening_rx(inc, &frames, now);
            return;
        }
        if user_session(inc.user) != self.session {
            return;
        }
        self.last_heard = now;
        if self.answering && !frames.iter().flatten().any(|f| f.kind == Kind::ConnReq) {
            self.answering = false;
        }

        if self.state == State::Disconnecting {
            match tiny {
                Some(Tiny::DiscAck) => {
                    self.clear_wait();
                    self.state = State::Disconnected;
                }
                Some(Tiny::DiscReq) => {
                    self.clear_wait();
                    self.reply = Some(self.tiny(Tiny::DiscAck, inc.wave));
                    self.close_after_tx = true;
                }
                // The peer missed our goodbye: again now, no faster than
                // what reached us.
                _ => {
                    if let Some(r) = self.retry.as_mut() {
                        r.wave = more_robust(r.wave, inc.wave);
                        self.deadline = Some(now);
                    }
                }
            }
            return;
        }

        // The peer answered our Turn, or polls us for an ACK: it is the
        // sender now. None of its bursts has arrived yet, so our ACK says
        // so (an older one could name a burst number of the peer's and
        // acknowledge frames we never got).
        let stream_ctl = frames.iter().flatten().any(|f| f.kind == Kind::Stream);
        if self.awaiting == Awaiting::TurnTaken
            && (inc.code < LADDER.len() || matches!(tiny, Some(Tiny::Idle | Tiny::Poll)) || stream_ctl)
        {
            self.role = Role::Receiver;
            self.clear_wait();
            self.at_peer.clear();
            self.at_peer_rest.clear();
            self.harq.clear();
            self.last_ack = (0, 0, 0, false);
            self.last_progress = now;
        }

        if self.stream_rx_routed(inc, &frames, now) {
            return;
        }
        match self.role {
            Role::Sender => self.sender_rx(inc, tiny, &frames, now),
            Role::Receiver => self.receiver_rx(inc, tiny, &frames, now),
        }
    }

    fn listening_rx(&mut self, inc: &Incoming, frames: &[Option<Frame>], now: f64) {
        for f in frames.iter().flatten() {
            if f.kind != Kind::ConnReq {
                continue;
            }
            let text = String::from_utf8_lossy(&f.payload).to_string();
            let mut words = text.split_whitespace();
            let (Some(from), Some(to)) = (words.next(), words.next()) else {
                continue;
            };
            if !to.eq_ignore_ascii_case(&self.cfg.call) {
                continue;
            }
            self.peer = from.to_ascii_uppercase();
            self.session = user_session(inc.user);
            self.state = State::Connected;
            self.role = Role::Receiver;
            self.last_heard = now;
            self.last_progress = now;
            self.answering = true;
            self.reset_data();
            self.agree(self.cfg.compress && f.flags & flags::COMPRESS != 0);
            self.caps = self.own_caps() & f.flags >> flags::CAPS_SHIFT;
            self.note_rx_channel(inc);
            self.reply = Some(self.conn_ack(inc));
            return;
        }
    }

    fn cur_wave(&self) -> Wave {
        LADDER[self.level].wave
    }

    /// The waveform for try `self.tries` of a burst first sent in `w`: the
    /// same, one more robust, then the floor; the floor at once when the
    /// link has stalled.
    fn retry_wave(&self, w: Wave, now: f64) -> Wave {
        if now - self.last_progress > STALL_S {
            return FLOOR_WAVE;
        }
        match self.tries {
            0 | 1 => w,
            2 => wave_down(w),
            _ => FLOOR_WAVE,
        }
    }

    // -- helpers --------------------------------------------------------------

    fn tiny(&self, t: Tiny, wave: Wave) -> Outgoing {
        let (kind, extra) = t.encode();
        Outgoing {
            td_ease: false,
            wave,
            code: TINY,
            user: user_bits(TINY, self.session, kind),
            extra,
            frames: Vec::new(),
            spans: Vec::new(),
            cue: false,
        }
    }

    fn control(&self, f: Frame, wave: Wave) -> Outgoing {
        Outgoing {
            td_ease: false,
            wave,
            code: CONTROL,
            user: user_bits(CONTROL, self.session, 0),
            extra: 0,
            frames: vec![f.encode(CONTROL_BYTES)],
            spans: Vec::new(),
            cue: false,
        }
    }

    fn send_awaiting(&mut self, b: Outgoing, what: Awaiting) -> Outgoing {
        self.awaiting = what;
        self.tries = 0;
        self.deadline = None;
        self.retry = Some(b.clone());
        self.note_tx(b)
    }

    fn note_tx(&mut self, b: Outgoing) -> Outgoing {
        if b.code >= LADDER.len() && b.code != crate::link::stream::STREAM {
            self.stats.control_bursts += 1;
        }
        self.stats.airtime_s += b.duration(&*self.air);
        self.last_tx_wave = b.wave;
        self.last_tx_code = b.code;
        b
    }

    fn clear_wait(&mut self) {
        self.awaiting = Awaiting::Nothing;
        self.deadline = None;
        self.retry = None;
        self.tries = 0;
    }

    fn reset_data(&mut self) {
        self.stream_tx = None;
        self.stream_prev = None;
        self.stream_rx = None;
        self.stream_hold_until = 0.0;
        self.stream_eff = 0.0;
        self.arq_bytes = 0;
        self.arq_time = 0.0;
        self.arq_from = None;
        self.run_from = None;
        self.run_bytes = 0;
        self.run_blocks = 0;
        self.drain_since = None;
        self.reply_by = None;
        self.inflight.clear();
        self.next_offset = 0;
        self.delivered = 0;
        self.rx_pending.clear();
        self.last_burst.clear();
        self.at_peer.clear();
        self.at_peer_rest.clear();
        self.harq.clear();
        self.last_ack = (0, 0, 0, false);
        self.peer_wants_turn = false;
        self.peer_wants_disc = false;
        self.want_disc = false;
        self.disc_by = None;
        self.level = FLOOR;
        self.target = FLOOR;
        self.fails = [0; LADDER.len()];
        self.success = [None; LADDER.len()];
        self.success_at = [f64::NEG_INFINITY; LADDER.len()];
        self.blocked_until = [f64::NEG_INFINITY; LADDER.len()];
        self.hold_all = [true; LADDER.len()];
        self.rx_level = FLOOR;
        self.burst_id = 0;
        self.peer_heard = None;
        // Compression is agreed afresh on each connection; data queued
        // for it (`unsent`), like `out`, stays.
        self.agreed = false;
        self.caps = 0;
        self.aim_db = None;
        self.tx_codec = None;
        self.rx_codec = None;
        self.peer_class = ChanClass::default();
        self.peer_doppler_hz = f32::NAN;
        self.chan.clear();
        self.burst_s = self.cfg.max_burst_s;
        self.last_report_db = None;
        self.cheap_ack_off = 0;
        self.ack_cap = None;
        self.ack_cap_bursts = receiver::CHEAP_ACK_OFF_BURSTS / 2;
        self.peer_ack_wave = None;
        self.heard_peer_db = None;
        self.turn_asked_at = 0.0;
        self.next_speak_up = 0.0;
        self.sends.clear();
        self.stream_total = self.out.len() as u64;
    }

    fn reset_link(&mut self) {
        self.reset_data();
        self.link_t0 = None;
        self.answering = false;
        self.clear_wait();
        self.reply = None;
        self.close_after_tx = false;
        self.peer.clear();
    }
}
