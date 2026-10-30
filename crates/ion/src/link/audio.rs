//! Bursts to audio and back: frames through the LDPC codec and the right
//! PHY (MFSK floor, slice, long-guard or normal OFDM), and every receiver
//! listening at once. Tiny messages are header-only bursts. A cued burst
//! ([`Outgoing::cue`]) goes out behind an MFSK cue that the receivers hand
//! to the OFDM receiver (`PhyRx::cue`).

use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};
use ion_dsp::ionos::{self, Ionos};

use super::harq::Soft;
use super::sim::{Medium, ModelAir};
use super::station::{Air, CONTROL, CUE_MODE, Config, Expected, Incoming, Outgoing, TINY, frame_bits, user_code};
use crate::fec;
use crate::level::{FrameSize, Guard, LADDER, Wave};
use crate::phy::{self, Constellation, DataMode, Numerology, PhyRx, Pilots, TxOptions};
use crate::phy_mfsk::{self, MfskBurst, MfskRx};
use crate::slots::{OFFSETS_HZ, SLOTS, SlotMeter};

/// OFDM width. Off: 2.45 kHz (275–2725 Hz around a 1500 Hz centre), inside
/// the US 2.8 kHz HF data limit by FCC §97.3(a)(8). On: the 2.75 kHz
/// layouts (`Numerology::NormalWide`, `LongWide`), whose normal-guard
/// levels occupy more than 2.8 kHz. Both ends of a link must use the same
/// width.
pub const WIDE: bool = false;

/// Audio centre of every Ion waveform.
pub const CENTER_HZ: f32 = if WIDE { 1600.0 } else { 1500.0 };

/// The OFDM numerology of guard `g` at the link's width (the slice is the
/// same at either width, on [`CENTER_HZ`] like every other burst).
pub fn numerology(g: Guard) -> Numerology {
    match g {
        Guard::Normal => Numerology::of(false, WIDE),
        Guard::Long => Numerology::of(true, WIDE),
        Guard::Slice => Numerology::Slice,
    }
}

/// The guard of a received OFDM burst's numerology.
pub fn guard_of(num: Numerology) -> Guard {
    match num {
        Numerology::Slice | Numerology::Slim | Numerology::Half | Numerology::Quarter => Guard::Slice,
        Numerology::Long | Numerology::LongWide => Guard::Long,
        Numerology::Normal | Numerology::NormalWide => Guard::Normal,
    }
}

/// Lowest and highest OFDM carrier, audio Hz (for display).
pub fn ofdm_band_hz() -> (f32, f32) {
    let half = phy::info_for(numerology(Guard::Normal)).bandwidth_hz / 2.0;
    (CENTER_HZ - half, CENTER_HZ + half)
}

/// Constellation and pilot pattern of burst `code`.
fn shape(code: usize) -> (Constellation, Pilots) {
    match code {
        TINY | CONTROL => (Constellation::Qpsk, Pilots::Robust),
        l => (LADDER[l].constellation, LADDER[l].pilots),
    }
}

/// Transmit options of an OFDM burst `code`: the level's low-PAPR mode
/// when it has one, else the plain clip for its constellation.
pub fn tx_options(code: usize) -> TxOptions {
    let (c, p) = shape(code);
    if let Some((headroom_db, spread)) = td_clip_pick(code) {
        return TxOptions {
            clip_db: None,
            spread,
            data_gain_db: 0.0,
            hdr_clip_db: None,
            td_clip: Some(phy::TdClip {
                headroom_db,
                overshoot: &[1.0, 2.0, 3.0],
            }),
            ..TxOptions::for_burst(c, p)
        };
    }
    match code {
        l if l < LADDER.len() && LADDER[l].low_papr => TxOptions::low_papr(c, p),
        _ => TxOptions::for_burst(c, p),
    }
}

/// Experiment (`ION_TDCLIP=1`): the whole-burst clipper's headroom (dB) and
/// spreading for ladder level `code`, as picked in PR #1's ladder table.
fn td_clip_pick(code: usize) -> Option<(f32, bool)> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("ION_TDCLIP").is_ok_and(|v| v == "1")) {
        return None;
    }
    Some(match code {
        5..=7 | 9 | 10 => (-3.0, true),
        8 => (-1.0, true),
        11 => (0.0, true),
        12 => (1.0, true),
        13 => (3.5, false),
        14 => (4.0, false),
        15 => (7.0, false),
        16 => (6.0, false),
        _ => return None,
    })
}

/// Air time from the PHYs' burst arithmetic.
pub struct PhyAir;

impl Air for PhyAir {
    fn duration(&self, wave: Wave, code: usize, bits: usize) -> f64 {
        let (c, p) = shape(code);
        let samples = match wave {
            Wave::Ofdm(g) => phy::burst_samples_with(numerology(g), p, c, bits),
            // GF(32) short frames: 205 tones each (`fec::nb`).
            Wave::Mfsk(m) => phy_mfsk::burst_samples(m, 5 * mfsk_symbols(code, bits)),
        };
        if samples == 0 {
            // A PHY not built yet: use the model's numbers.
            return ModelAir::default().duration(wave, code, bits);
        }
        samples as f64 / AUDIO_RATE as f64
    }

    fn max_bits(&self, wave: Wave, code: usize) -> usize {
        let (c, p) = shape(code);
        match wave {
            Wave::Ofdm(g) => phy::capacity_bits_with(numerology(g), p, c, phy::MAX_DATA_SYMBOLS),
            Wave::Mfsk(m) => phy_mfsk::max_bits(m),
        }
    }
}

/// A station config for `call` (sizes come from the ladder and codes).
pub fn config(call: &str) -> Config {
    Config::new(call)
}

pub fn bytes_to_bits(b: &[u8]) -> Vec<u8> {
    b.iter().flat_map(|&x| (0..8).rev().map(move |i| x >> i & 1)).collect()
}

pub fn bits_to_bytes(b: &[u8]) -> Vec<u8> {
    b.chunks(8)
        .map(|c| c.iter().fold(0u8, |a, &x| a << 1 | (x & 1)))
        .collect()
}

/// Long frames (the main code) rather than short ones.
fn is_long(code: usize) -> bool {
    code < LADDER.len() && LADDER[code].frame == FrameSize::Long
}

fn encode_frames(b: &Outgoing) -> Vec<u8> {
    let mut bits = Vec::new();
    for (i, f) in b.frames.iter().enumerate() {
        let info = bytes_to_bits(f);
        if is_long(b.code) {
            let (from, n) = b.span(i);
            bits.extend(fec::encode_span(&info, from, n));
        } else {
            bits.extend(fec::encode_short(&info));
        }
    }
    bits
}

/// Extra level for OFDM bursts on the air, dB. Every waveform is built to
/// peak at `ion_dsp::TX_LEVEL`, where the steady MFSK and CW carriers set the
/// station's power. OFDM at the same peak reads lower on a power meter;
/// this default brings it level. Simulations leave OFDM at 0 dB (they set
/// SNR against each burst's own power).
pub const OFDM_AIR_GAIN_DB: f32 = 1.2;
/// The most extra OFDM level allowed: steady carriers never change, and
/// OFDM never peaks more than 3 dB above them.
pub const OFDM_AIR_GAIN_MAX_DB: f32 = 3.0;

/// Linear OFDM gain for `db` (clamped to 0..=[`OFDM_AIR_GAIN_MAX_DB`]).
pub fn ofdm_gain(db: f32) -> f32 {
    10f32.powf(db.clamp(0.0, OFDM_AIR_GAIN_MAX_DB) / 20.0)
}

/// Extra on-air level of OFDM level `code`, dB, so it averages the same
/// power as the full-width QPSK levels at the same drive. The slice's 13
/// carriers clip to a lower mean at the same peak; the long-guard levels
/// are already there, and 8PSK and QAM average lower by their shape and
/// stay as they are.
pub fn level_trim_db(code: usize) -> f32 {
    match LADDER.get(code) {
        Some(l) if l.wave == Wave::Ofdm(Guard::Slice) => match l.rate {
            fec::Rate::R1_3 => 0.6,
            fec::Rate::R1_2 => 0.95,
            _ => 0.0,
        },
        _ => 0.0,
    }
}

/// [`transmit`] as it goes on the air: OFDM bursts scaled by `ofdm_gain`
/// and their level's [`level_trim_db`], together never more than
/// [`OFDM_AIR_GAIN_MAX_DB`] above the steady carriers. A cue stays at
/// `TX_LEVEL`, as loud as every MFSK burst: the gain is the OFDM part's.
pub fn transmit_with(b: &Outgoing, ofdm_gain: f32) -> Vec<f32> {
    let mut a = modulate(b);
    if matches!(b.wave, Wave::Ofdm(_)) {
        let cap = 10f32.powf(OFDM_AIR_GAIN_MAX_DB / 20.0);
        let g = (ofdm_gain * 10f32.powf(level_trim_db(b.code) / 20.0)).min(cap);
        if g != 1.0 {
            for x in &mut a {
                *x = (*x * g).clamp(-1.0, 1.0);
            }
        }
    }
    behind_cue(b, a)
}

/// Data symbols field of a cue's `extra`; the 6 bits above it carry the
/// burst's `extra` (its HARQ parity field).
const CUE_NSYM_BITS: u32 = 10;

/// `a` (burst `b`'s audio) behind `b`'s cue, if it has one: an MFSK
/// header-only burst of its own kind carrying `b.user`, and in its `extra`
/// the OFDM data symbols and `b.extra`. The rest of the OFDM header follows
/// from the level in `user`.
fn behind_cue(b: &Outgoing, a: Vec<f32>) -> Vec<f32> {
    let Wave::Ofdm(g) = b.wave else {
        return a;
    };
    if !b.cue {
        return a;
    }
    debug_assert!(
        g != Guard::Slice && b.code < LADDER.len(),
        "a cue is for full-band data bursts"
    );
    let (c, p) = shape(b.code);
    let nsym = phy::data_symbols_with(numerology(g), p, c, b.coded_bits());
    let extra = nsym as u16 | (b.extra & 0x3F) << CUE_NSYM_BITS;
    let mut out = phy_mfsk::modulate_cue(CUE_MODE, b.user, extra, CENTER_HZ);
    out.extend_from_slice(&a);
    out
}

/// What a cue announces: the OFDM guard and the burst as the OFDM receiver
/// should expect it. `None` for a cue no full-band level matches.
pub fn cue_of(b: &MfskBurst) -> Option<(Guard, phy::Cue)> {
    let code = user_code(Wave::Ofdm(Guard::Normal), b.user);
    let lv = LADDER.get(code)?;
    let g = match lv.wave {
        Wave::Ofdm(g @ (Guard::Normal | Guard::Long)) => g,
        _ => return None,
    };
    let nsym = (b.extra & ((1 << CUE_NSYM_BITS) - 1)) as usize;
    if nsym == 0 || nsym > phy::MAX_DATA_SYMBOLS {
        return None;
    }
    let opt = tx_options(code);
    Some((
        g,
        phy::Cue {
            start: b.start_sample + phy_mfsk::cue_samples(b.mode) as u64,
            f_hz: b.freq_offset_hz,
            constellation: lv.constellation,
            data_symbols: nsym,
            user: b.user,
            extra: b.extra >> CUE_NSYM_BITS,
            pilots: lv.pilots,
            mode: DataMode::new(opt.spread, opt.data_gain_db),
            // The cue's own CRC vouched for the burst: it is taken
            // whatever its preamble correlation.
            trust: true,
        },
    ))
}

/// GF(32) code rate of the short frames of burst `code` (control frames
/// and the floor: 1/4).
fn nb_rate(code: usize) -> fec::Rate {
    match LADDER.get(code) {
        Some(l) if l.frame == FrameSize::Short => l.rate,
        _ => fec::Rate::R1_4,
    }
}

/// Code symbols an MFSK burst `code` of `bits` coded bits (whole short
/// frames as the station counts them, [`frame_bits`]) sends: one GF(32)
/// codeword of [`fec::nb::n_for`] tones per frame.
fn mfsk_symbols(code: usize, bits: usize) -> usize {
    if bits == 0 {
        return 0;
    }
    bits.div_ceil(frame_bits(code, 1)) * fec::nb::n_for(nb_rate(code))
}

/// The GF(32) codewords of an MFSK burst's frames (all short: the MFSK
/// levels and control frames), one tone per symbol.
fn encode_frames_nb(b: &Outgoing) -> Vec<u8> {
    let rate = nb_rate(b.code);
    b.frames
        .iter()
        .flat_map(|f| fec::nb::encode_bits_at(rate, fec::nb::link_layout(), &bytes_to_bits(f)))
        .collect()
}

/// Encodes and modulates a burst (behind its cue, if it has one).
pub fn transmit(b: &Outgoing) -> Vec<f32> {
    behind_cue(b, modulate(b))
}

/// [`transmit`] without the cue.
fn modulate(b: &Outgoing) -> Vec<f32> {
    match b.wave {
        Wave::Mfsk(m) => {
            debug_assert!(!is_long(b.code), "long frames on MFSK");
            phy_mfsk::modulate_symbols(m, b.user, b.extra, &encode_frames_nb(b), CENTER_HZ)
        }
        Wave::Ofdm(g) => {
            let (c, _) = shape(b.code);
            // A tiny message has no frames: a header-only burst. A data
            // burst's header has no room for `extra`.
            let bits = encode_frames(b);
            let extra = if bits.is_empty() { b.extra } else { 0 };
            phy::modulate_full(
                numerology(g),
                c,
                b.user,
                extra,
                &bits,
                center_of(b),
                &tx_options(b.code),
            )
        }
    }
}

/// The slot a data burst asks for in its `extra` (`Station`'s
/// `slice_slots`; 0 the centre).
pub fn slot_of(extra: u16) -> usize {
    match (extra & 3) as usize {
        s if s < SLOTS => s,
        _ => 0,
    }
}

/// Audio centre of burst `b`: a slice data burst in its slot
/// ([`crate::slots`]), everything else at [`CENTER_HZ`] (tiny and control
/// bursts too, whose `extra` is their message).
pub fn center_of(b: &Outgoing) -> f32 {
    match b.wave {
        Wave::Ofdm(Guard::Slice) if b.code < LADDER.len() => CENTER_HZ + OFFSETS_HZ[slot_of(b.extra)],
        _ => CENTER_HZ,
    }
}

/// Splits LLRs into codewords and decodes them, every codeword whole and on
/// its own (no HARQ).
fn decode_frames(code: usize, llrs: &[f32]) -> Vec<Option<Vec<u8>>> {
    let (n, short) = match code {
        CONTROL => (fec::n_short(), true),
        l if l < LADDER.len() => (LADDER[l].coded_bits(), LADDER[l].frame == FrameSize::Short),
        _ => return Vec::new(),
    };
    llrs.chunks_exact(n)
        .map(|w| {
            let bits = if short {
                fec::decode_short(w)
            } else {
                fec::decode(LADDER[code].rate, w)
            };
            bits.map(|b| bits_to_bytes(&b))
        })
        .collect()
}

/// Frames and soft bits of a data part: long-frame data bursts go to the
/// station undecoded (it knows which slots are HARQ increments).
fn split(code: usize, llrs: &[f32]) -> (Vec<Option<Vec<u8>>>, Option<Soft>) {
    if is_long(code) {
        (Vec::new(), Some(Soft::Llrs(llrs.to_vec())))
    } else {
        (decode_frames(code, llrs), None)
    }
}

/// The frames of `inc` decoded each on its own, as if nothing were stored
/// (a long-frame burst without HARQ increments, or a test).
pub fn decode_alone(inc: &Incoming) -> Vec<Option<Vec<u8>>> {
    match &inc.soft {
        Some(Soft::Llrs(llrs)) => decode_frames(inc.code, llrs),
        _ => inc.frames.clone(),
    }
}

/// A received OFDM burst as the link sees it.
pub fn decode(b: &phy::RxBurst) -> Incoming {
    let code = user_code(Wave::Ofdm(guard_of(b.numerology)), b.user);
    // A header-only burst is a tiny message whatever its code claims.
    let (code, frames, soft) = if b.header_only {
        (TINY, Vec::new(), None)
    } else {
        let (frames, soft) = split(code, &b.llrs);
        (code, frames, soft)
    };
    Incoming {
        wave: Wave::Ofdm(guard_of(b.numerology)),
        code,
        user: b.user,
        extra: b.extra,
        frames,
        soft,
        snr_db: b.snr_db,
        chan: b.chan,
        slot: 0,
        grid: b.grid.clone(),
    }
}

/// A received MFSK burst as the link sees it.
pub fn decode_mfsk(b: &MfskBurst) -> Incoming {
    let code = user_code(Wave::Mfsk(b.mode), b.user);
    let (frames, soft) = if code == TINY {
        (Vec::new(), None)
    } else if is_long(code) {
        split(code, &b.llrs)
    } else if code == CONTROL || code < LADDER.len() {
        // Short frames: GF(32) codewords on the tone metrics.
        let frames = phy_mfsk::decode_short_frames_at(b, nb_rate(code), &[])
            .into_iter()
            .map(|f| f.map(|bits| bits_to_bytes(&bits)))
            .collect();
        (frames, None)
    } else {
        (Vec::new(), None)
    };
    Incoming {
        wave: Wave::Mfsk(b.mode),
        code,
        user: b.user,
        extra: b.extra,
        frames,
        soft,
        snr_db: b.snr_db,
        chan: b.chan,
        slot: 0,
        grid: None,
    }
}

/// One received burst with what a net receiver needs beyond
/// [`Incoming`]: where it sat in the audio, and (MFSK) the burst itself,
/// for decoding again with a priori knowledge.
#[derive(Clone, Debug)]
pub struct Heard {
    pub inc: Incoming,
    /// Audio samples (counted from the first sample given to the
    /// receivers) of the burst's first and one-past-last sample.
    pub start_sample: u64,
    pub end_sample: u64,
    pub mfsk: Option<MfskBurst>,
}

/// Audio samples of a received MFSK burst.
fn mfsk_len(b: &MfskBurst) -> u64 {
    phy_mfsk::burst_samples(b.mode, b.llrs.len()) as u64
}

/// Audio samples of a received OFDM burst.
fn ofdm_len(b: &phy::RxBurst) -> u64 {
    let pilots = if b.header_only { Pilots::Robust } else { b.pilots };
    phy::burst_samples_with(b.numerology, pilots, b.constellation, b.llrs.len()) as u64
}

/// The PHY header of an expected OFDM burst in guard `g`, as
/// [`transmit`] would send it.
fn expected_header(g: Guard, x: &Expected) -> phy::ExpectedHeader {
    let (c, p) = shape(x.code);
    let opt = tx_options(x.code);
    let data = x.bits > 0;
    phy::ExpectedHeader {
        constellation: c,
        data_symbols: if data {
            phy::data_symbols_with(numerology(g), p, c, x.bits)
        } else {
            0
        },
        user: x.user,
        extra: if data { 0 } else { x.extra },
        pilots: p,
        mode: phy::DataMode::new(opt.spread, opt.data_gain_db),
    }
}

/// What a received burst's data looked like to the receiver, for a decode
/// plot; at most [`PLOT_MAX`] points, evenly over the burst.
#[derive(Clone, Debug)]
pub enum RxPlot {
    /// OFDM: the equalised data symbols (`phy::RxBurst::eq`) and the
    /// constellation they were sent in.
    Iq {
        constellation: Constellation,
        symbols: Vec<ion_dsp::dsp::Complex>,
    },
    /// MFSK, as Data2G plots its tones: per data symbol, 1 less the second
    /// likeliest value's share and the likeliest value's share (of the
    /// symbol's 32), from the tone metrics.
    Tones(Vec<[f32; 2]>),
}

pub const PLOT_MAX: usize = 1000;

fn ofdm_plot(b: &phy::RxBurst) -> Option<RxPlot> {
    (!b.eq.is_empty()).then(|| RxPlot::Iq {
        constellation: b.constellation,
        symbols: b.eq.clone(),
    })
}

fn mfsk_plot(b: &MfskBurst) -> Option<RxPlot> {
    // R0 and R1 both send one tone of 32 values a symbol.
    const VALS: usize = 32;
    let n = b.metrics.len() / VALS;
    if n == 0 {
        return None;
    }
    let shares = b
        .metrics
        .as_chunks::<VALS>()
        .0
        .iter()
        .step_by(n.div_ceil(PLOT_MAX))
        .map(|m| {
            let (mut top, mut second, mut sum) = (0f32, 0f32, 0f32);
            for &l in m {
                let p = l.exp();
                sum += p;
                if p > top {
                    second = top;
                    top = p;
                } else if p > second {
                    second = p;
                }
            }
            [1.0 - second / sum, top / sum]
        })
        .collect();
    Some(RxPlot::Tones(shares))
}

/// Every receiver at once, on the same audio: the full-band OFDM guards,
/// the slice in each of the three thirds of the passband
/// ([`crate::slots`]), MFSK, and the slot meter.
pub struct Receivers {
    ofdm: PhyRx,
    ofdm_long: PhyRx,
    ofdm_slice: [PhyRx; SLOTS],
    mfsk: MfskRx,
    ofdm_out: Vec<phy::RxBurst>,
    /// Per entry of `ofdm_out`: the slot of the slice receiver that heard
    /// it, or none (a full-band burst).
    ofdm_slot: Vec<Option<usize>>,
    mfsk_out: Vec<MfskBurst>,
    meter: SlotMeter,
    /// The meter's pick, stamped on every burst heard.
    slot: u8,
    cues_heard: u64,
    /// What the OFDM receivers were last told to expect.
    expected: Vec<Expected>,
    /// Decode plots are wanted ([`Receivers::set_plot`]); those of the
    /// last [`Receivers::process`], in its bursts' order.
    plot: bool,
    plots: Vec<Option<RxPlot>>,
}

/// Audio samples [`Receivers`] hand each receiver at a time (20 ms).
const RUN_BLOCK: usize = 960;

/// How old the meter's frames are before they count: a burst decoded at
/// its end masks the frames it covered, and in real time a busy receiver
/// masks as it goes.
const METER_LAG_S: f32 = 1.0;

impl Receivers {
    pub fn new() -> Self {
        Self {
            ofdm: PhyRx::with_numerology(numerology(Guard::Normal), CENTER_HZ),
            ofdm_long: PhyRx::with_numerology(numerology(Guard::Long), CENTER_HZ),
            ofdm_slice: std::array::from_fn(|k| {
                PhyRx::with_numerology(numerology(Guard::Slice), CENTER_HZ + OFFSETS_HZ[k])
            }),
            mfsk: MfskRx::new(CENTER_HZ),
            ofdm_out: Vec::new(),
            ofdm_slot: Vec::new(),
            mfsk_out: Vec::new(),
            meter: SlotMeter::new(CENTER_HZ, AUDIO_RATE, METER_LAG_S),
            slot: 0,
            cues_heard: 0,
            expected: Vec::new(),
            plot: false,
            plots: Vec::new(),
        }
    }

    /// Runs every receiver and the meter over `samples`: the bursts land in
    /// `ofdm_out` (slots in `ofdm_slot`) and `mfsk_out`, and each burst's
    /// stretch of audio is left out of the meter's thirds it covered.
    fn run(&mut self, samples: &[f32]) {
        self.ofdm_out.clear();
        self.ofdm_slot.clear();
        self.mfsk_out.clear();
        let from = self.meter.samples();
        self.meter.push(samples);
        let to = self.meter.samples();
        // In blocks, MFSK first: a cue reaches the OFDM receiver while the
        // burst it announces is still in that receiver's ring (2 s).
        for block in samples.chunks(RUN_BLOCK) {
            let m0 = self.mfsk_out.len();
            self.mfsk.process(block, &mut self.mfsk_out);
            let mut k = m0;
            while k < self.mfsk_out.len() {
                if !self.mfsk_out[k].cue {
                    k += 1;
                    continue;
                }
                let b = self.mfsk_out.remove(k);
                self.cues_heard += 1;
                match cue_of(&b) {
                    Some((Guard::Long, c)) => self.ofdm_long.cue(c),
                    Some((_, c)) => self.ofdm.cue(c),
                    None => {}
                }
            }
            self.ofdm.process(block, &mut self.ofdm_out);
            self.ofdm_long.process(block, &mut self.ofdm_out);
            self.ofdm_slot.resize(self.ofdm_out.len(), None);
            for (k, rx) in self.ofdm_slice.iter_mut().enumerate() {
                rx.process(block, &mut self.ofdm_out);
                self.ofdm_slot.resize(self.ofdm_out.len(), Some(k));
            }
        }
        // Bursts heard: out of the thirds they covered (all, full-band).
        for (b, slot) in self.ofdm_out.iter().zip(&self.ofdm_slot) {
            self.meter.mask(b.start_sample, b.start_sample + ofdm_len(b), *slot);
        }
        for b in &self.mfsk_out {
            self.meter.mask(b.start_sample, b.start_sample + mfsk_len(b), None);
        }
        // Bursts under way.
        if self.ofdm.busy() || self.ofdm_long.busy() || self.mfsk.busy() {
            self.meter.mask(from, to, None);
        }
        for (k, rx) in self.ofdm_slice.iter().enumerate() {
            if rx.busy() {
                self.meter.mask(from, to, Some(k));
            }
        }
        self.slot = self.meter.best() as u8;
    }

    /// Feeds 48 kHz audio; appends every burst completed in it.
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<Incoming>) {
        self.run(samples);
        let slot = self.slot;
        out.extend(self.ofdm_out.iter().map(|b| Incoming { slot, ..decode(b) }));
        out.extend(self.mfsk_out.iter().map(|b| Incoming { slot, ..decode_mfsk(b) }));
        self.plots.clear();
        if self.plot {
            self.plots.extend(self.ofdm_out.iter().map(ofdm_plot));
            self.plots.extend(self.mfsk_out.iter().map(mfsk_plot));
        }
    }

    /// Make decode plots of the bursts heard (off by default).
    pub fn set_plot(&mut self, on: bool) {
        self.plot = on;
    }

    /// The decode plots of the bursts the last [`Receivers::process`]
    /// appended, in order (none while plots are off).
    pub fn take_plots(&mut self) -> Vec<Option<RxPlot>> {
        std::mem::take(&mut self.plots)
    }

    /// [`Receivers::process`], with each burst's place in the audio and
    /// the raw MFSK burst.
    pub fn process_heard(&mut self, samples: &[f32], out: &mut Vec<Heard>) {
        self.run(samples);
        let slot = self.slot;
        out.extend(self.ofdm_out.iter().map(|b| Heard {
            inc: Incoming { slot, ..decode(b) },
            start_sample: b.start_sample,
            end_sample: b.start_sample + ofdm_len(b),
            mfsk: None,
        }));
        out.extend(self.mfsk_out.drain(..).map(|b| Heard {
            inc: Incoming {
                slot,
                ..decode_mfsk(&b)
            },
            start_sample: b.start_sample,
            end_sample: b.start_sample + mfsk_len(&b),
            mfsk: Some(b),
        }));
    }

    /// The slot meter's pick (0 the centre) and each third's score.
    pub fn slot(&self) -> (u8, [f32; SLOTS]) {
        (self.slot, self.meter.scores())
    }

    /// The OFDM bursts the station expects next ([`super::Station::expected`]):
    /// each OFDM receiver tests a header that fails its check against
    /// those of its own numerology ([`PhyRx::expect`]).
    pub fn expect(&mut self, e: &[Expected]) {
        if self.expected == e {
            return;
        }
        self.expected = e.to_vec();
        let of = |g: Guard| -> Vec<phy::ExpectedHeader> {
            e.iter()
                .filter(|x| x.wave == Wave::Ofdm(g))
                .map(|x| expected_header(g, x))
                .collect()
        };
        self.ofdm.expect(&of(Guard::Normal));
        self.ofdm_long.expect(&of(Guard::Long));
        let slice = of(Guard::Slice);
        for rx in &mut self.ofdm_slice {
            rx.expect(&slice);
        }
    }

    /// See [`MfskRx::expect`]: a burst the MFSK receiver should look for.
    pub fn expect_mfsk(&mut self, e: Option<phy_mfsk::Expect>) {
        self.mfsk.expect(e);
    }

    #[doc(hidden)]
    pub fn mfsk_stats(&self) -> phy_mfsk::debug::RxStats {
        self.mfsk.stats()
    }

    /// Cues heard so far.
    pub fn cues_heard(&self) -> u64 {
        self.cues_heard
    }

    pub fn busy(&self) -> bool {
        self.ofdm.busy() || self.ofdm_long.busy() || self.ofdm_slice.iter().any(PhyRx::busy) || self.mfsk.busy()
    }
}

impl Default for Receivers {
    fn default() -> Self {
        Self::new()
    }
}

/// The two directions' channels.
enum Paths {
    /// Watterson channels (`ion_dsp::channel`); SNR in 2.4 kHz against the
    /// burst's mean power.
    Watterson(Box<[Channel; 2]>),
    /// IONOS SIM (`ion_dsp::ionos`); S:N in its bandwidth against the
    /// reference.
    Ionos(Box<[Ionos; 2]>, ionos::Reference),
}

/// Both directions through their own fading channel, each into its own
/// set of receivers.
pub struct AudioMedium {
    /// SNR in dB: in 2.4 kHz against the burst's mean power, or with
    /// [`AudioMedium::peak_referenced`] against a steady tone at the
    /// bursts' common peak (`TX_LEVEL`), as a transmitter limited by its
    /// peak power sends them; on IONOS, in 3 kHz against the
    /// [`ionos::Reference`] given.
    pub snr_db: f32,
    pep: bool,
    paths: Paths,
    receivers: [Receivers; 2],
    noise: NoiseGen,
    heard: Vec<Incoming>,
}

impl AudioMedium {
    pub fn new(profile: Profile, snr_db: f32, seed: u64) -> Self {
        Self {
            snr_db,
            paths: Paths::Watterson(Box::new([
                Channel::from_profile(profile, seed),
                Channel::from_profile(profile, seed ^ 0x5555),
            ])),
            receivers: [Receivers::new(), Receivers::new()],
            noise: NoiseGen::new(seed ^ 0xA5A5),
            heard: Vec::new(),
            pep: false,
        }
    }

    /// Through IONOS SIM (`paths` 2 or 4, 3 kHz bandwidth). With
    /// [`ionos::Reference::Peak`] the bursts are the on-air ones (OFDM at
    /// [`OFDM_AIR_GAIN_DB`] and its level trim) and S:N is against a sine
    /// at each burst's own peak, as the box is levelled (on the modem's
    /// peaks) and calibrated (with a sine); with `Mean`, against each
    /// burst's mean power.
    pub fn ionos(mode: ionos::Mode, paths: usize, reference: ionos::Reference, snr_db: f32, seed: u64) -> Self {
        Self {
            snr_db,
            paths: Paths::Ionos(
                Box::new([
                    Ionos::new(mode, paths, 3000, seed),
                    Ionos::new(mode, paths, 3000, seed ^ 0x5555),
                ]),
                reference,
            ),
            receivers: [Receivers::new(), Receivers::new()],
            noise: NoiseGen::new(seed ^ 0xA5A5),
            heard: Vec::new(),
            pep: false,
        }
    }

    /// Noise against a steady tone at `ion_dsp::TX_LEVEL` (the peak every
    /// burst is built to) instead of each burst's own mean power: the
    /// constant-envelope MFSK sees the same SNR either way, OFDM as much
    /// less as its mean power sits below its peak.
    pub fn peak_referenced(mut self, on: bool) -> Self {
        self.pep = on;
        self
    }

    /// The receivers of side `k` (0 or 1).
    pub fn receivers(&self, k: usize) -> &Receivers {
        &self.receivers[k]
    }
}

impl Medium for AudioMedium {
    fn expect(&mut self, to: usize, e: &[Expected]) {
        self.receivers[to].expect(e);
    }

    fn carry(&mut self, from: usize, burst: &Outgoing, _start: f64, _end: f64) -> Option<Incoming> {
        let on_air = matches!(self.paths, Paths::Ionos(_, ionos::Reference::Peak));
        let tx = if on_air {
            transmit_with(burst, ofdm_gain(OFDM_AIR_GAIN_DB))
        } else {
            transmit(burst)
        };
        if tx.is_empty() {
            return None;
        }
        // S:N is the burst's own: a cue in front of it does not move the
        // reference.
        let body = &tx[if burst.cue { phy_mfsk::cue_samples(CUE_MODE) } else { 0 }..];
        let power = if self.pep {
            ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0
        } else {
            body.iter().map(|x| x * x).sum::<f32>() / body.len() as f32
        };
        // Lead-in and tail of noise around the burst, and the channel delay.
        let lead = (0.1 * AUDIO_RATE) as usize;
        let latency = match &self.paths {
            Paths::Watterson(ch) => ch[from].latency(),
            Paths::Ionos(ch, _) => ch[from].latency(),
        };
        let mut input = vec![0.0; lead];
        input.extend_from_slice(&tx);
        input.resize(input.len() + latency + lead, 0.0);
        let mut out = vec![0.0; input.len()];
        match &mut self.paths {
            Paths::Watterson(ch) => {
                ch[from].process(&input, &mut out);
                let sigma = noise_sigma(power, self.snr_db, 2400.0, AUDIO_RATE);
                add_awgn(&mut out, sigma, &mut self.noise);
            }
            Paths::Ionos(ch, reference) => {
                // Peak: a sine at this burst's own peak, as the box is
                // levelled on each modem's peaks (OFDM's air gain and level
                // trim buy nothing here).
                let p = match reference {
                    ionos::Reference::Peak => ion_dsp::ionos::reference_power(body, ionos::Reference::Peak),
                    ionos::Reference::Mean => power,
                };
                ch[from].process(&input, Some((p, self.snr_db)), &mut out);
            }
        }
        self.heard.clear();
        self.receivers[1 - from].process(&out, &mut self.heard);
        self.heard.drain(..).next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_burst(cue: bool) -> Outgoing {
        let level = LADDER.iter().position(|l| l.wave == Wave::Ofdm(Guard::Normal)).unwrap();
        Outgoing {
            wave: LADDER[level].wave,
            code: level,
            user: super::super::station::user_bits(level, 0x5A, 2),
            extra: 3,
            frames: vec![vec![0x6B; LADDER[level].frame_bytes()]; 2],
            spans: Vec::new(),
            cue,
        }
    }

    #[test]
    fn a_cue_is_no_louder_than_mfsk_and_leaves_the_ofdm_as_it_was() {
        let cue_len = phy_mfsk::cue_samples(CUE_MODE);
        let peak = |a: &[f32]| a.iter().fold(0f32, |m, x| m.max(x.abs()));
        let mfsk = phy_mfsk::modulate(CUE_MODE, 0x1234, 0x5678, &[], CENTER_HZ);
        // At the most OFDM gain there is: the cue does not follow it.
        for gain in [1.0, ofdm_gain(OFDM_AIR_GAIN_MAX_DB)] {
            let plain = transmit_with(&data_burst(false), gain);
            let cued = transmit_with(&data_burst(true), gain);
            assert_eq!(cued.len(), cue_len + plain.len());
            assert!(peak(&cued[..cue_len]) <= peak(&mfsk) * 1.0001);
            assert!(peak(&cued[..cue_len]) <= ion_dsp::TX_LEVEL * 1.0001);
            assert_eq!(&cued[cue_len..], &plain[..]);
        }
        let b = data_burst(true);
        assert!((b.duration(&PhyAir) - data_burst(false).duration(&PhyAir) - cue_len as f64 / 48_000.0).abs() < 1e-9);
    }

    #[test]
    fn a_cued_burst_is_heard_with_its_extra() {
        let b = data_burst(true);
        let mut audio = vec![0.0; 4_800];
        audio.extend(transmit(&b));
        audio.extend(vec![0.0; 48_000]);
        let mut rx = Receivers::new();
        let mut out = Vec::new();
        rx.process(&audio, &mut out);
        assert_eq!(rx.cues_heard(), 1);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!((out[0].code, out[0].user, out[0].extra), (b.code, b.user, 3));
        assert!(decode_alone(&out[0]).iter().all(Option::is_some));
    }

    #[test]
    fn bits_and_bytes_round_trip() {
        let b = vec![0x00, 0xFF, 0xA5, 0x3C];
        let bits = bytes_to_bits(&b);
        assert_eq!(&bits[16..24], &[1, 0, 1, 0, 0, 1, 0, 1]);
        assert_eq!(bits_to_bytes(&bits), b);
    }
}
