//! The stream gear (`docs/link.md`, Stream gear): a big transfer goes as
//! fixed-length periods of outer-coded pieces ([`code`]), each period at
//! one inner option ([`OPTS`]), with a pause after each in which the
//! receiver may answer with two tones ([`tones`]): DONE, SLOWER or FASTER;
//! silence means go on. The receiver keeps time with the periods
//! ([`sync`]), so none of them needs its header heard.
//!
//! This module holds what both ends share: the options, the schedule,
//! the frames and the receiver's rule. `Station` runs the protocol,
//! `audio` the waveforms and the timekeeping.

pub mod acq;
pub mod code;
pub mod sync;
pub mod tones;

use std::collections::VecDeque;

use super::frame::crc32;
use crate::fec::{self, Rate};
use crate::phy::{self, Constellation, Numerology, Pilots};
use crate::phy_mfsk::{self, MfskMode};
pub use tones::Msg;

/// Burst code of a stream period ([`super::Outgoing::code`]).
pub const STREAM: usize = 61;
/// Burst code of a tone answer.
pub const TONES: usize = 60;

/// A block that ends at this option or faster hands the link back to ARQ
/// for a while: from about +3 dB IONOS S:N the link's HARQ does as well or
/// better (`stream-gear.md`, Sessions), and the SNR reports at connect do
/// not tell 0 dB from +3 dB.
pub const HIGH_OPT: usize = 7;
const _: () = assert!(matches!(OPTS[HIGH_OPT].fam, Fam::Ofdm) && OPTS[HIGH_OPT].n == fec::K * 4);

/// The link must have sent data on ARQ this long before it can judge
/// whether to stream: its goodput is the yardstick (`stream-gear.md`,
/// Decision).
pub const MEASURE_S: f64 = 75.0;
/// The stream is used only while the link's own ARQ goodput is below this,
/// bytes a second (1850 B/min): measured on IONOS SIM, ARQ ran at most
/// 1700 B/min at 0 dB, where the stream wins 1.2–1.9×, and at least 1976
/// B/min at +3 dB, where it does not.
pub const REGION_BPS: f64 = 1850.0 / 60.0;
/// Break-even: the stream pays only for what would take ARQ longer than
/// this at its measured goodput, seconds. The gear-on link overtook the
/// gear-off one 82–374 s into the 0 dB sessions, 191–551 s into the −6 dB
/// ones, and over six seeds at −6 dB on fading it was still behind after
/// 5 minutes; 600 s is past every crossover measured.
pub const BREAK_EVEN_S: f64 = 600.0;
/// Data bursts send only what is in flight (so the queue can stream) for
/// at most this long, seconds; then the stream waits [`DRAIN_HOLD_S`].
pub const DRAIN_S: f64 = 40.0;
pub const DRAIN_HOLD_S: f64 = 300.0;
/// The first block of a run: about this long at the link's ARQ goodput (the
/// stream should do at least as well), so it completes within a couple of
/// minutes wherever the stream settles; seconds.
pub const FIRST_BLOCK_S: f64 = 120.0;
/// The stream is used while the SNR the gear aims by is below this (dB
/// in 2.4 kHz, as the ACKs report it; about +4 dB IONOS S:N). From +3 dB
/// up the stream draws with the link's HARQ at best (`rateless-stream.md`,
/// section 2; `stream-gear.md`).
pub const BELOW_DB: f32 = 2.0;
/// Longest data burst of a period, audio seconds (the PTT lead comes on
/// top).
pub const BURST_S: f64 = 9.8;
/// One period's slot on the sender's clock, seconds: the lead, the burst
/// and the pause, whatever the option, so a receiver unsure of the option
/// still knows when the next period starts. The pause holds the
/// receiver's turnaround, its lead, the two tones and slack.
pub const SLOT_S: f64 = 11.4;
/// A block takes about this long at its option's nominal rate, between
/// [`BLOCK_MIN`] and [`BLOCK_MAX`] bytes: [`BLOCK_LONG_S`] after a block
/// that needed little more than its own pieces (its source pieces arrive
/// in order and are delivered as they come, so a long block costs
/// nothing and saves Start/Go exchanges, about 20 s each in MFSK R1),
/// [`BLOCK_SHORT_S`] otherwise (lost pieces hold back what follows them
/// until the block decodes).
pub const BLOCK_SHORT_S: f32 = 60.0;
pub const BLOCK_LONG_S: f32 = 150.0;
/// A block whose K pieces took at most this many times K sent counts as
/// clean.
pub const CLEAN_EFF: f32 = 0.95;
pub const BLOCK_MIN: usize = 1024;
pub const BLOCK_MAX: usize = 16384;
/// The sender gives a block up after sending this many times its pieces.
pub const GIVE_UP_X: usize = 4;
/// A receiver's answer more than this late after its turnaround is held
/// for the next pause (it would run into the next period).
pub const LATE_S: f64 = 0.1;
/// The `user` hi nibble of a stream period.
pub const HI: u8 = 0xF;

/// A waveform family's burst shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fam {
    /// MFSK, GF(32) short frames at a rate, one piece each.
    Mfsk(MfskMode, Rate),
    /// Full-band normal-guard low-PAPR QPSK OFDM (as ladder level 10),
    /// LDPC long frames of six pieces.
    Ofdm,
}

/// A speed the stream can run at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Opt {
    pub fam: Fam,
    /// OFDM: coded bits per codeword, [`fec::K`] information bits each;
    /// past [`fec::N_MOTHER`] the mother code repeats (chase).
    pub n: usize,
    pub name: &'static str,
}

const fn mfsk(m: MfskMode, r: Rate, name: &'static str) -> Opt {
    Opt {
        fam: Fam::Mfsk(m, r),
        n: 0,
        name,
    }
}

const fn qpsk(num: usize, den: usize, name: &'static str) -> Opt {
    Opt {
        fam: Fam::Ofdm,
        n: fec::K * den / num,
        name,
    }
}

/// Slowest first; SLOWER and FASTER move one step.
pub const OPTS: [Opt; 11] = [
    mfsk(MfskMode::R0, Rate::R1_4, "R0 1/4"),
    mfsk(MfskMode::R1, Rate::R1_4, "R1 1/4"),
    mfsk(MfskMode::R1, Rate::R1_3, "R1 1/3"),
    mfsk(MfskMode::R1, Rate::R1_2, "R1 1/2"),
    mfsk(MfskMode::R1, Rate::R2_3, "R1 2/3"),
    qpsk(1, 8, "QPSK 1/8"),
    qpsk(1, 6, "QPSK 1/6"),
    qpsk(1, 4, "QPSK 1/4"),
    qpsk(1, 3, "QPSK 1/3"),
    qpsk(1, 2, "QPSK 1/2"),
    qpsk(2, 3, "QPSK 2/3"),
];

/// The ladder level whose OFDM shape the stream borrows (header code,
/// pilots, low-PAPR TX options).
pub const OFDM_LEVEL: usize = 10;
const _: () = assert!(matches!(
    crate::level::LADDER[OFDM_LEVEL].constellation,
    Constellation::Qpsk
));

pub fn opt_index(name: &str) -> usize {
    OPTS.iter().position(|o| o.name == name).expect("option")
}

pub fn up(k: usize) -> Option<usize> {
    (k + 1 < OPTS.len()).then_some(k + 1)
}

pub fn down(k: usize) -> Option<usize> {
    k.checked_sub(1)
}

/// The ladder level whose header code an MFSK option's periods carry.
fn mfsk_level(m: MfskMode, r: Rate) -> usize {
    match (m, r) {
        (MfskMode::R0, _) => 0,
        (_, Rate::R1_3) => 2,
        (_, Rate::R1_2) => 3,
        (_, Rate::R2_3) => 4,
        _ => 1,
    }
}

/// The `user` word of a period in option `o`.
pub fn period_user(o: usize, session: u8) -> u16 {
    let level = match OPTS[o].fam {
        Fam::Ofdm => OFDM_LEVEL,
        Fam::Mfsk(m, r) => mfsk_level(m, r),
    };
    super::station::user_bits(level, session, HI)
}

/// Whether a burst heard with this header could be a period of the
/// stream of `session` (any option).
pub fn is_period_user(user: u16, session: u8) -> bool {
    super::station::user_hi(user) == HI
        && super::station::user_session(user) == session
        && (0..OPTS.len()).any(|o| period_user(o, session) == user)
}

pub fn pilots() -> Pilots {
    crate::level::LADDER[OFDM_LEVEL].pilots
}

/// OFDM codewords in one period of option `o` (whole codewords, the burst
/// no longer than [`BURST_S`]).
pub fn codewords(o: usize) -> usize {
    let n = OPTS[o].n;
    let max = phy::capacity_bits_with(Numerology::Normal, pilots(), Constellation::Qpsk, max_symbols());
    (max / n).max(1)
}

/// Data symbols that fit [`BURST_S`] of normal-guard OFDM.
fn max_symbols() -> usize {
    let room = (BURST_S * ion_dsp::dsp::AUDIO_RATE as f64) as usize - phy::OVERHEAD_AUDIO;
    (room / phy::SYMBOL_AUDIO).min(phy::MAX_DATA_SYMBOLS)
}

/// MFSK frames in one period of option `o` (at least one).
pub fn mfsk_frames(o: usize) -> usize {
    let Fam::Mfsk(m, r) = OPTS[o].fam else { return 0 };
    let room = (BURST_S * ion_dsp::dsp::AUDIO_RATE as f64) as usize;
    let per = 5 * fec::nb::n_for(r);
    (1..)
        .take_while(|&k| phy_mfsk::burst_samples(m, k * per) <= room)
        .last()
        .unwrap_or(1)
}

/// Frames in one period of option `o`.
pub fn frames(o: usize) -> usize {
    match OPTS[o].fam {
        Fam::Ofdm => codewords(o),
        Fam::Mfsk(..) => mfsk_frames(o),
    }
}

/// Coded bits of a period's data part (MFSK: 5 per tone).
pub fn period_bits(o: usize) -> usize {
    match OPTS[o].fam {
        Fam::Ofdm => codewords(o) * OPTS[o].n,
        Fam::Mfsk(_, r) => mfsk_frames(o) * 5 * fec::nb::n_for(r),
    }
}

/// OFDM data symbols of a period of option `o`.
pub fn ofdm_symbols(o: usize) -> usize {
    phy::data_symbols_with(Numerology::Normal, pilots(), Constellation::Qpsk, period_bits(o))
}

/// Audio samples of a period's burst (no lead).
pub fn burst_samples(o: usize) -> usize {
    match OPTS[o].fam {
        Fam::Ofdm => phy::burst_samples_with(Numerology::Normal, pilots(), Constellation::Qpsk, period_bits(o)),
        Fam::Mfsk(m, _) => phy_mfsk::burst_samples(m, period_bits(o)),
    }
}

/// Pieces one frame of option `o` carries.
pub fn pieces_per_frame(o: usize) -> usize {
    match OPTS[o].fam {
        Fam::Ofdm => OFDM_PIECES,
        Fam::Mfsk(..) => 1,
    }
}

/// Information bytes a second of option `o`, every frame decoded, after
/// the frames' own overhead and the slot's lead, header and pause.
pub fn nominal_bps(o: usize) -> f32 {
    let pieces = (frames(o) * pieces_per_frame(o) * code::PIECE) as f32;
    pieces / SLOT_S as f32
}

/// The seed of a stream's tone plan.
pub fn plan_seed(session: u8, id: u8) -> u64 {
    (session as u64) << 8 | id as u64
}

/// What a receiving station's timekeeping needs from it
/// (`Station::stream_listen`).
#[derive(Clone, Debug, PartialEq)]
pub struct Listen {
    pub session: u8,
    pub id: u8,
    /// Changes with each Go: the timekeeping starts again from it.
    pub epoch: u32,
    /// The options the next period may be in, likeliest first.
    pub opts: Vec<usize>,
    /// The first period starts this long after our Go ends (the sender's
    /// turnaround and PTT lead).
    pub after_tx_s: f64,
    /// Periods the station has taken in, and whether the last one gave
    /// a piece (only then does it anchor the timekeeping).
    pub seen: u32,
    pub last_ok: bool,
}

/// What a sending station listens for in a pause (`Station::stream_watch`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Watch {
    /// The tone plan's seed ([`plan_seed`]).
    pub seed: u64,
    /// When our period ended (the pause began), on our clock.
    pub from: f64,
}

/// How long after its period ends the sender listens before deciding,
/// seconds: the receiver's turnaround and lead, the tones, and both
/// sound cards' latencies.
pub const WATCH_S: f64 = 1.15;

// -- frames -------------------------------------------------------------------

/// Pieces in one OFDM long frame (180 bytes: id, option, first index,
/// six pieces, CRC-32).
pub const OFDM_PIECES: usize = 6;
const OFDM_HEAD: usize = 4;
/// Bytes of an MFSK frame: index, one piece, CRC-16.
const MFSK_BYTES: usize = fec::K_SHORT / 8;
const LONG_BYTES: usize = fec::K / 8;
const _: () = assert!(OFDM_HEAD + OFDM_PIECES * code::PIECE + 4 <= LONG_BYTES);
const _: () = assert!(2 + code::PIECE + 2 == MFSK_BYTES);

/// CRC-16/CCITT-FALSE, its initial value from the stream id so a piece
/// of another stream never passes.
fn crc16(data: &[u8], id: u8) -> u16 {
    let mut crc: u16 = 0xFFFF ^ (id as u16) << 8 ^ 0x5A;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { crc << 1 ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// One frame's information bytes: pieces `first..` of a block.
pub fn encode_frame(o: usize, id: u8, first: usize, pieces: &[[u8; code::PIECE]]) -> Vec<u8> {
    match OPTS[o].fam {
        Fam::Ofdm => {
            debug_assert!(pieces.len() == OFDM_PIECES);
            let mut b = vec![id, o as u8];
            b.extend_from_slice(&(first as u16).to_be_bytes());
            for p in pieces {
                b.extend_from_slice(p);
            }
            b.resize(LONG_BYTES - 4, 0);
            let c = crc32(&b) ^ id as u32;
            b.extend_from_slice(&c.to_le_bytes());
            b
        }
        Fam::Mfsk(..) => {
            let mut b = (first as u16).to_be_bytes().to_vec();
            b.extend_from_slice(&pieces[0]);
            let c = crc16(&b, id);
            b.extend_from_slice(&c.to_be_bytes());
            b
        }
    }
}

/// The pieces in a frame of stream `id` that decoded: (index, piece), and
/// for OFDM the option the sender put in it. Empty if the frame is not
/// one of this stream's.
pub fn decode_frame(ofdm: bool, id: u8, b: &[u8]) -> (Vec<(usize, [u8; code::PIECE])>, Option<usize>) {
    let mut out = Vec::new();
    if ofdm {
        if b.len() != LONG_BYTES {
            return (out, None);
        }
        let (body, tail) = b.split_at(LONG_BYTES - 4);
        let c = u32::from_le_bytes(tail.try_into().unwrap());
        if crc32(body) ^ id as u32 != c || body[0] != id {
            return (out, None);
        }
        let first = u16::from_be_bytes([body[2], body[3]]) as usize;
        let (pieces, _) = body[OFDM_HEAD..OFDM_HEAD + OFDM_PIECES * code::PIECE].as_chunks::<{ code::PIECE }>();
        for (i, p) in pieces.iter().enumerate() {
            out.push((first + i, *p));
        }
        (out, Some(body[1] as usize).filter(|&o| o < OPTS.len()))
    } else {
        if b.len() != MFSK_BYTES {
            return (out, None);
        }
        let c = u16::from_be_bytes([b[MFSK_BYTES - 2], b[MFSK_BYTES - 1]]);
        if crc16(&b[..MFSK_BYTES - 2], id) != c {
            return (out, None);
        }
        let first = u16::from_be_bytes([b[0], b[1]]) as usize;
        out.push((first, b[2..2 + code::PIECE].try_into().unwrap()));
        (out, None)
    }
}

/// Decodes one OFDM codeword's LLRs (`n` of them, in transmission order,
/// repeats past the mother code combined): its information bytes, and
/// the blind mutual information per coded bit the LLRs carry.
pub fn decode_codeword(llrs: &[f32]) -> (Option<Vec<u8>>, f32) {
    let blind = llrs
        .iter()
        .map(|&l| {
            let a = l.abs().min(40.0) as f64;
            let p = 1.0 / (1.0 + a.exp());
            if p > 0.0 {
                1.0 + p * p.log2() + (1.0 - p) * (1.0 - p).log2()
            } else {
                1.0
            }
        })
        .sum::<f64>()
        / llrs.len().max(1) as f64;
    let m = llrs.len().min(fec::N_MOTHER);
    let mut comb = vec![0.0f32; m];
    for (i, &l) in llrs.iter().enumerate() {
        comb[i % fec::N_MOTHER] += l;
    }
    let bits = fec::Decoder::new().decode_prefix(&comb);
    (bits.map(|b| super::audio::bits_to_bytes(&b)), blind as f32)
}

/// About where each option loses 1% of its frames, dB in 2.4 kHz, for
/// the model channel (`sim::ModelMedium`): the MFSK ladder levels', and
/// the QPSK rates spaced as their codes need.
pub fn model_threshold_db(o: usize) -> f32 {
    const T: [f32; 11] = [-12.0, -9.0, -8.0, -6.5, -4.5, -1.5, -0.5, 1.0, 2.5, 5.5, 8.0];
    T[o.min(T.len() - 1)]
}

// -- block sizing and the start ------------------------------------------------

/// Bytes of the next block for `queued` bytes at start option `o`.
/// The fastest option a stream starts in on a link whose ARQ moves `arq`
/// bytes a second: one whose nominal rate is at most four times that (the
/// stream gains 1.2–1.9× where it runs; a start far above where it
/// settles costs periods before SLOWER).
pub fn start_cap(arq: f64) -> usize {
    (0..OPTS.len())
        .rev()
        .find(|&o| (nominal_bps(o) as f64) <= 4.0 * arq)
        .unwrap_or(0)
}

pub fn block_bytes(queued: usize, o: usize, target_s: f32) -> usize {
    let target = (nominal_bps(o) * target_s) as usize;
    queued.min(target.clamp(BLOCK_MIN, BLOCK_MAX))
}

/// The start option for an SNR (dB in 2.4 kHz, effective, as the link
/// reports it), against the ladder's effective-SNR thresholds: QPSK 1/4
/// about where the full-band QPSK 1/3 level is (0 dB), each halving of
/// the rate 1.5 dB lower (chase), MFSK at its ladder levels' thresholds.
pub fn start_pick(snr_db: f32) -> usize {
    const T: [(f32, &str); 10] = [
        (5.0, "QPSK 1/2"),
        (2.0, "QPSK 1/3"),
        (0.0, "QPSK 1/4"),
        (-1.0, "QPSK 1/6"),
        (-2.5, "QPSK 1/8"),
        (-4.5, "R1 2/3"),
        (-6.5, "R1 1/2"),
        (-8.0, "R1 1/3"),
        (-10.0, "R1 1/4"),
        (f32::NEG_INFINITY, "R0 1/4"),
    ];
    opt_index(T.iter().find(|(s, _)| snr_db >= *s).unwrap().1)
}

// -- the receiver's rule -------------------------------------------------------

/// Blind mutual information per coded bit at which half the codewords of
/// each OFDM option decode (`lab_runs/stream/calib.txt`).
fn i_half(o: usize) -> f32 {
    match OPTS[o].name {
        "QPSK 1/8" => 0.165,
        "QPSK 1/6" => 0.205,
        "QPSK 1/4" => 0.285,
        "QPSK 1/3" => 0.365,
        "QPSK 1/2" => 0.50,
        "QPSK 2/3" => 0.69,
        _ => 1.0,
    }
}

/// SNR (dB in 2.4 kHz, the receiver's estimate on MFSK) the last two
/// periods must both show before the receiver asks to go up from MFSK into
/// QPSK: one period's estimate swings by several dB on fading, and each
/// failed probe costs a period (the lab's −1 dB on one period probed
/// every few periods at −3 dB).
const UP_SNR_QPSK: f32 = 0.0;

/// One period as the receiver saw it.
#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub opt: usize,
    /// OFDM: the burst was received; MFSK: its header decoded.
    pub heard: bool,
    pub tried: usize,
    pub ok: usize,
    /// Blind information per coded bit of each OFDM codeword.
    pub ivals: Vec<f32>,
    pub snr_db: f32,
}

/// The receiver's speed rule's memory.
#[derive(Clone, Debug, Default)]
pub struct Rule {
    hist: VecDeque<Stat>,
    /// Periods in a row at the current option, how many of them clean,
    /// and whether it was reached by going up.
    at_opt: usize,
    clean_run: usize,
    came_up: bool,
    /// Per option: probes into it that failed (backs off 2, 4, 8, 16).
    fails: [u8; OPTS.len()],
}

/// Information bits per data cell of OFDM option `o`.
fn bits_per_cell(o: usize) -> f32 {
    match OPTS[o].fam {
        Fam::Ofdm => 2.0 * fec::K as f32 / OPTS[o].n as f32,
        Fam::Mfsk(..) => 0.0,
    }
}

impl Rule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a period.
    pub fn push(&mut self, st: Stat) {
        let clean = st.tried > 0 && st.ok == st.tried;
        let o = st.opt;
        match self.hist.front() {
            Some(prev) if prev.opt == o => {
                self.at_opt += 1;
                self.clean_run = if clean { self.clean_run + 1 } else { 0 };
            }
            prev => {
                self.came_up = prev.is_some_and(|p| p.opt < o);
                self.at_opt = 1;
                self.clean_run = clean as usize;
            }
        }
        self.hist.push_front(st);
        self.hist.truncate(5);
    }

    /// SLOWER or FASTER after the last period, or nothing. MFSK: SLOWER
    /// under 60% of frames, FASTER after two clean periods (more after a
    /// failed probe; into QPSK only above [`UP_SNR_QPSK`]). OFDM, over the
    /// last two periods: each codeword's blind information predicts the
    /// neighbours' decode rates; move to the better by 3% (up only if 80%
    /// would decode); down into MFSK when, over four periods, MFSK would
    /// carry more even if all its frames decoded. Two periods in a row
    /// with nothing decoded: SLOWER, whatever the prediction
    /// (`stream-sync.md`, finding 6).
    pub fn decide(&mut self) -> Option<Msg> {
        let m = self.rule();
        if m == Some(Msg::Slower) && self.came_up && self.at_opt <= 2 {
            let o = self.hist.front().map_or(0, |s| s.opt);
            self.fails[o] = (self.fails[o] + 1).min(3);
        }
        m
    }

    fn rule(&self) -> Option<Msg> {
        let st = self.hist.front()?;
        let o = st.opt;
        let fam = OPTS[o].fam;
        let ofdm = |k: usize| OPTS[k].fam == Fam::Ofdm;
        let (lo, hi) = (down(o), up(o));
        let empty2 = self.hist.len() >= 2 && self.hist.iter().take(2).all(|s| s.ok == 0);
        if empty2 && lo.is_some() {
            return Some(Msg::Slower);
        }
        match fam {
            Fam::Mfsk(..) => {
                if st.tried > 0 && st.ok * 10 < st.tried * 6 {
                    return lo.map(|_| Msg::Slower);
                }
                if let Some(h) = hi
                    && self.clean_run >= 2 << self.fails[h]
                    && (!ofdm(h) || self.hist.iter().take(2).all(|s| s.snr_db >= UP_SNR_QPSK))
                {
                    return Some(Msg::Faster);
                }
                None
            }
            Fam::Ofdm => {
                let ivals: Vec<f32> = self
                    .hist
                    .iter()
                    .take(2)
                    .filter(|s| ofdm(s.opt) && s.heard)
                    .flat_map(|s| s.ivals.iter().copied())
                    .collect();
                let (ok, tried) = self
                    .hist
                    .iter()
                    .take(2)
                    .filter(|s| s.opt == o && s.heard)
                    .fold((0, 0), |a, s| (a.0 + s.ok, a.1 + s.tried));
                if tried == 0 {
                    return None;
                }
                let seen = ok as f32 / tried as f32;
                let pred = |k: usize| {
                    if ivals.is_empty() {
                        return seen;
                    }
                    ivals.iter().filter(|&&i| i >= i_half(k)).count() as f32 / ivals.len() as f32
                };
                let g = bits_per_cell(o) * seen;
                if let Some(l) = lo {
                    if ofdm(l) && bits_per_cell(l) * pred(l).max(seen) > 1.03 * g {
                        return Some(Msg::Slower);
                    }
                    let (ok4, tried4) = self
                        .hist
                        .iter()
                        .take(4)
                        .filter(|s| s.opt == o)
                        .fold((0, 0), |a, s| (a.0 + s.ok, a.1 + s.tried));
                    let seen4 = ok4 as f32 / tried4.max(1) as f32;
                    if !ofdm(l) && (self.at_opt >= 4 || seen4 == 0.0) && nominal_bps(o) * seen4 < 1.03 * nominal_bps(l)
                    {
                        return Some(Msg::Slower);
                    }
                }
                let h = hi?;
                let p_up = pred(h).min(seen);
                if self.at_opt >= 1 << self.fails[h] && bits_per_cell(h) * p_up > 1.03 * g && p_up >= 0.8 {
                    return Some(Msg::Faster);
                }
                None
            }
        }
    }
}

// -- control messages ------------------------------------------------------------

/// The stream's control frames (`Kind::Stream`, in a control burst).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ctl {
    /// Sender: a block of `len` bytes at stream offset `offset` (24 bits),
    /// CRC-32 `crc`, first period in option `opt`.
    Start {
        id: u8,
        opt: u8,
        offset: u32,
        len: u32,
        crc: u32,
    },
    /// Receiver: ready; first period in option `opt`; everything before
    /// `delivered` (24 bits) is delivered (it confirms the block before,
    /// when blocks follow one another).
    Go { id: u8, opt: u8, delivered: u32 },
    /// Sender: the stream is over (heard DONE, or gave up).
    End { id: u8 },
    /// Receiver: whether it has the block, everything before `delivered`
    /// (24 bits), the SNR it heard the stream at.
    Report {
        id: u8,
        done: bool,
        delivered: u32,
        snr_db: f32,
    },
}

impl Ctl {
    pub fn to_bytes(self) -> Vec<u8> {
        match self {
            Ctl::Start {
                id,
                opt,
                offset,
                len,
                crc,
            } => {
                let mut b = vec![1, id, opt];
                b.extend_from_slice(&offset.to_le_bytes()[..3]);
                b.extend_from_slice(&len.to_le_bytes()[..3]);
                b.extend_from_slice(&crc.to_le_bytes());
                b
            }
            Ctl::Go { id, opt, delivered } => {
                let mut b = vec![2, id, opt];
                b.extend_from_slice(&delivered.to_le_bytes()[..3]);
                b
            }
            Ctl::End { id } => vec![3, id],
            Ctl::Report {
                id,
                done,
                delivered,
                snr_db,
            } => {
                let mut b = vec![4, id, done as u8];
                b.extend_from_slice(&delivered.to_le_bytes()[..3]);
                b.push(super::frame::snr_q(snr_db, 8) as u8);
                b
            }
        }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Ctl> {
        let u24 = |s: &[u8]| u32::from_le_bytes([s[0], s[1], s[2], 0]);
        Some(match *b.first()? {
            1 if b.len() >= 13 => Ctl::Start {
                id: b[1],
                opt: b[2],
                offset: u24(&b[3..6]),
                len: u24(&b[6..9]),
                crc: u32::from_le_bytes(b[9..13].try_into().ok()?),
            },
            2 if b.len() >= 6 => Ctl::Go {
                id: b[1],
                opt: b[2],
                delivered: u24(&b[3..6]),
            },
            3 if b.len() >= 2 => Ctl::End { id: b[1] },
            4 if b.len() >= 7 => Ctl::Report {
                id: b[1],
                done: b[2] != 0,
                delivered: u24(&b[3..6]),
                snr_db: super::frame::snr_from_q(b[6] as u32),
            },
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_fit_their_slot() {
        let rate = ion_dsp::dsp::AUDIO_RATE as f64;
        for (o, opt) in OPTS.iter().enumerate() {
            let s = burst_samples(o) as f64 / rate;
            assert!(s <= BURST_S + 1e-9, "{}: {s:.2} s", opt.name);
            assert!(s >= 0.6 * BURST_S, "{}: only {s:.2} s", opt.name);
            // With a 0.2 s lead, the pause holds a turnaround, a lead, the
            // tones and slack.
            assert!(
                SLOT_S - 0.2 - s >= 0.3 + 0.2 + 2.0 * tones::TONE_S + 0.3,
                "{}",
                opt.name
            );
        }
        // Every QPSK option fills the same symbols: one head for them all.
        let q: Vec<usize> = (0..OPTS.len())
            .filter(|&o| OPTS[o].fam == Fam::Ofdm)
            .map(ofdm_symbols)
            .collect();
        assert!(q.windows(2).all(|w| w[0] == w[1]), "{q:?}");
    }

    #[test]
    #[ignore]
    fn option_table() {
        let rate = ion_dsp::dsp::AUDIO_RATE as f64;
        for (o, opt) in OPTS.iter().enumerate() {
            println!(
                "| {} | {} | {} | {:.2} s | {:.0} B/min |",
                opt.name,
                frames(o),
                frames(o) * pieces_per_frame(o),
                burst_samples(o) as f64 / rate,
                60.0 * nominal_bps(o)
            );
        }
    }

    #[test]
    fn frames_round_trip_and_reject_another_stream() {
        let pieces: Vec<[u8; code::PIECE]> = (0..6).map(|i| [i as u8 * 3 + 1; code::PIECE]).collect();
        for o in [0, opt_index("QPSK 1/4")] {
            let n = pieces_per_frame(o);
            let ofdm = OPTS[o].fam == Fam::Ofdm;
            let b = encode_frame(o, 9, 700, &pieces[..n]);
            let (got, opt) = decode_frame(ofdm, 9, &b);
            assert_eq!(got.len(), n);
            assert_eq!(got[0], (700, pieces[0]));
            assert_eq!(opt, ofdm.then_some(o));
            assert!(decode_frame(ofdm, 10, &b).0.is_empty());
            let mut bad = b.clone();
            bad[5] ^= 1;
            assert!(decode_frame(ofdm, 9, &bad).0.is_empty());
        }
    }

    #[test]
    fn control_messages_round_trip() {
        for c in [
            Ctl::Start {
                id: 3,
                opt: 5,
                offset: 0xAB_CDEF,
                len: 16_384,
                crc: 0xDEAD_BEEF,
            },
            Ctl::Go {
                id: 3,
                opt: 4,
                delivered: 0x12_3456,
            },
            Ctl::End { id: 200 },
            Ctl::Report {
                id: 7,
                done: true,
                delivered: 77_777,
                snr_db: -3.0,
            },
        ] {
            let b = c.to_bytes();
            assert!(b.len() <= super::super::frame::Frame::max_payload(super::super::station::CONTROL_BYTES));
            assert_eq!(Ctl::from_bytes(&b), Some(c));
        }
    }

    #[test]
    fn two_empty_periods_ask_for_slower() {
        let mut r = Rule::new();
        let o = opt_index("QPSK 1/3");
        for _ in 0..2 {
            r.push(Stat {
                opt: o,
                heard: true,
                tried: 8,
                ok: 0,
                ivals: vec![0.05; 8],
                snr_db: 0.0,
            });
        }
        assert_eq!(r.decide(), Some(Msg::Slower));
    }
}
