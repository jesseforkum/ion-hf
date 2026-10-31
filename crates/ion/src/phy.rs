//! OFDM burst modem: scattered pilots, Gray QPSK / 8PSK / 16QAM / 64QAM
//! with exact soft demapping. Design and parameters: `docs/phy.md`.
//!
//! A burst is
//!
//! ```text
//! | A | B: one symbol | header: H symbols | data: n symbols | 2 ms ramp |
//! | A | B: one symbol | header | header, second copy | 2 ms ramp |   (header-only)
//! ```
//!
//! - **A** repeats with a short period: Schmidl-Cox detection and coarse
//!   frequency; its empty bins measure the noise. The numerologies' periods
//!   are unrelated, so one receiver does not trigger on another's A.
//! - **B** is a full-band known symbol for fine timing and the first
//!   channel estimate.
//! - The **header** (constellation, data symbols, 16 user bits, CRC-16) is
//!   protected by a tail-biting K=13 rate-1/8 convolutional code. A
//!   header-only burst carries 16 more `extra` bits in place of the data
//!   fields, flagged by a CRC mask, and sends the header twice.
//! - **Data** bits are scrambled, interleaved over the whole burst and
//!   Gray-mapped onto the data carriers; the rest are pilots.

use ion_dsp::dsp::{AUDIO_RATE, lowpass_taps};

mod est;
mod frame;
mod rx;
mod tx;

use frame::*;

pub use frame::{DataMode, GAIN_STEP_DB, Pilots};
pub use rx::{BurstGrid, CellDebug, Cue, ExpectedHeader, Genie, HYP_MARGIN, HYP_Z, PhyRx, REFINE_K, RxBurst, RxStats};
pub use tx::{
    DEFAULT_CLIP_DB, LOW_PAPR_HDR_CLIP_DB, PSK8_CLIP_DB, QAM16_CLIP_DB, QAM64_CLIP_DB, QPSK_CLIP_DB, TdClip, TxOptions,
    debug_tx_cells, low_papr_clip_db, low_papr_gain_db, modulate, modulate_full, modulate_with, set_td_clip,
    td_clip_on,
};

/// Data constellation of a burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Constellation {
    Qpsk,
    Psk8,
    Qam16,
    Qam64,
}

impl Constellation {
    pub const ALL: [Constellation; 4] = [
        Constellation::Qpsk,
        Constellation::Psk8,
        Constellation::Qam16,
        Constellation::Qam64,
    ];

    pub fn bits_per_symbol(self) -> usize {
        match self {
            Constellation::Qpsk => 2,
            Constellation::Psk8 => 3,
            Constellation::Qam16 => 4,
            Constellation::Qam64 => 6,
        }
    }

    fn code(self) -> u8 {
        match self {
            Constellation::Qpsk => 0,
            Constellation::Psk8 => 1,
            Constellation::Qam16 => 2,
            Constellation::Qam64 => 3,
        }
    }

    fn from_code(c: u8) -> Self {
        match c {
            0 => Constellation::Qpsk,
            1 => Constellation::Psk8,
            2 => Constellation::Qam16,
            _ => Constellation::Qam64,
        }
    }

    fn points(self) -> Points {
        match self {
            Constellation::Qpsk => qpsk(),
            Constellation::Psk8 => psk8(),
            Constellation::Qam16 => qam16(),
            Constellation::Qam64 => qam64(),
        }
    }

    /// The points at unit mean energy, as [`RxBurst::eq`] holds them.
    pub fn unit_points(self) -> Vec<ion_dsp::dsp::Complex> {
        let p = self.points();
        p.pts[..p.count()].to_vec()
    }
}

/// OFDM numerology: carrier spacing, cyclic prefix, width and everything
/// that follows from them (preamble, pilots, header length).
///
/// Each guard comes in two widths: 2.45 kHz (`Normal`, `Long`, centred on
/// 1500 Hz by default: 275–2725 Hz) for radios whose TX filter stops at
/// 2.7 kHz, and 2.75 kHz (`NormalWide`, `LongWide`, centred on 1600 Hz:
/// 225–2975 Hz). The two widths do not decode each other (different CRC
/// masks); both ends of a link must use the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Numerology {
    /// 50 Hz spacing, 4 ms cyclic prefix, 24 ms symbols, 49 carriers.
    Normal,
    /// 25 Hz spacing, 10 ms cyclic prefix, 50 ms symbols, 97 carriers: for
    /// echoes up to about 8 ms (NVIS).
    Long,
    /// `Normal` on 55 carriers.
    NormalWide,
    /// `Long` on 109 carriers.
    LongWide,
    /// A 650 Hz slice: Normal's spacing and cyclic prefix on 13 carriers,
    /// so three slices fit side by side in an SSB channel (centres 750,
    /// 1500 and 2250 Hz). Longer preamble and header, since each cell
    /// carries more energy than a Normal one. Not in [`Numerology::ALL`].
    Slice,
    /// 650 Hz with Long's guard: 25 carriers 26.7 Hz apart. Not used by
    /// the link.
    Slim,
    /// 350 Hz, 7 carriers. Not used by the link.
    Half,
    /// 250 Hz, 5 carriers. Not used by the link.
    Quarter,
}

impl Numerology {
    /// The four full-width numerologies (not [`Numerology::Slice`]).
    pub const ALL: [Numerology; 4] = [
        Numerology::Normal,
        Numerology::Long,
        Numerology::NormalWide,
        Numerology::LongWide,
    ];

    fn layout(self) -> &'static Layout {
        match self {
            Numerology::Normal => &NORMAL,
            Numerology::Long => &LONG,
            Numerology::NormalWide => &NORMAL_WIDE,
            Numerology::LongWide => &LONG_WIDE,
            Numerology::Slice => &SLICE,
            Numerology::Slim => &SLIM,
            Numerology::Half => &HALF,
            Numerology::Quarter => &QUARTER,
        }
    }

    /// The long-guard (25 Hz) numerologies.
    pub fn is_long(self) -> bool {
        matches!(self, Numerology::Long | Numerology::LongWide)
    }

    /// The 2.75 kHz numerologies.
    pub fn is_wide(self) -> bool {
        matches!(self, Numerology::NormalWide | Numerology::LongWide)
    }

    /// The 650 Hz slice and the narrower ones.
    pub fn is_slice(self) -> bool {
        matches!(self, Numerology::Slice | Numerology::Half | Numerology::Quarter)
    }

    /// The full-width numerology with this guard and width.
    pub fn of(long: bool, wide: bool) -> Numerology {
        match (long, wide) {
            (false, false) => Numerology::Normal,
            (true, false) => Numerology::Long,
            (false, true) => Numerology::NormalWide,
            (true, true) => Numerology::LongWide,
        }
    }

    /// Default audio centre: 1500 Hz (2.45 kHz wide, and the middle
    /// slice) or 1600 Hz (2.75 kHz).
    pub fn center_hz(self) -> f32 {
        if self.is_wide() { 1600.0 } else { 1500.0 }
    }

    /// Lowest valid audio centre: 1300 Hz ([`MIN_CENTER_HZ`]), 1450 Hz for
    /// the wide numerologies, 500 Hz for the slice.
    pub fn min_center_hz(self) -> f32 {
        self.layout().min_center_hz
    }
}

/// Fixed PHY numbers the link layer needs for timing and sizing.
#[derive(Clone, Copy, Debug)]
pub struct PhyInfo {
    /// Data carriers per OFDM symbol (excluding pilots) with the robust
    /// pilot pattern.
    pub data_carriers: usize,
    /// The same with the sparse pattern.
    pub sparse_data_carriers: usize,
    /// One OFDM symbol including cyclic prefix, seconds.
    pub symbol_s: f32,
    /// Preamble + header, seconds.
    pub overhead_s: f32,
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: f32,
}

/// Lowest valid audio centre of the narrow numerologies, Hz: below it the
/// lowest carriers come within 75 Hz of 0 Hz and the front-end filter's
/// skirt folds over it.
pub const MIN_CENTER_HZ: f32 = 1300.0;
pub const MAX_CENTER_HZ: f32 = 22_000.0;

/// The 48 kHz filter used for TX interpolation and the RX front end
/// (narrow numerologies).
#[doc(hidden)]
pub fn front_end_taps() -> Vec<f32> {
    front_end_taps_for(Numerology::Normal)
}

/// The 48 kHz filter of a numerology.
#[doc(hidden)]
pub fn front_end_taps_for(num: Numerology) -> Vec<f32> {
    let l = num.layout();
    lowpass_taps(l.filter_len, l.filter_cutoff, AUDIO_RATE)
}

/// Preamble (A and B) boost over the data symbols' mean power, dB. Their
/// PAPR is ~3 dB, so this stays below the data peaks.
pub const PREAMBLE_BOOST_DB: f32 = 3.0;
/// Audio samples of a Normal burst that are not data symbols (filter tails
/// included), so `modulate().len() == OVERHEAD_AUDIO + data_symbols·1152`.
/// The same for every numerology with 50 Hz spacing.
pub const OVERHEAD_AUDIO: usize = overhead_audio(&NORMAL);
/// Audio samples per Normal OFDM symbol.
pub const SYMBOL_AUDIO: usize = NORMAL.sym() * DEC;
/// Most data symbols in one burst (any numerology).
pub const MAX_DATA_SYMBOLS: usize = MAX_DATA_SYMS;

const fn overhead_audio(l: &Layout) -> usize {
    l.overhead_bb() * DEC + l.filter_len - 1
}

/// [`info_for`] the Normal numerology.
pub fn info() -> PhyInfo {
    info_for(Numerology::Normal)
}

/// Timing and sizing numbers of a numerology.
pub fn info_for(num: Numerology) -> PhyInfo {
    let l = num.layout();
    PhyInfo {
        data_carriers: l.data_carriers(Pilots::Robust),
        sparse_data_carriers: l.data_carriers(Pilots::Sparse),
        symbol_s: l.sym() as f32 / FS as f32,
        overhead_s: overhead_audio(l) as f32 / AUDIO_RATE,
        bandwidth_hz: l.nc as f32 * (FS as f32 / l.n as f32),
    }
}

/// Number of data bits a Normal burst of `symbols` OFDM data symbols
/// carries (robust pilots).
pub fn capacity_bits(c: Constellation, symbols: usize) -> usize {
    capacity_bits_for(Numerology::Normal, c, symbols)
}

/// Number of data bits a burst of `symbols` OFDM data symbols carries
/// (robust pilots).
pub fn capacity_bits_for(num: Numerology, c: Constellation, symbols: usize) -> usize {
    capacity_bits_with(num, Pilots::Robust, c, symbols)
}

/// Number of data bits a burst of `symbols` OFDM data symbols with pilot
/// pattern `p` carries.
pub fn capacity_bits_with(num: Numerology, p: Pilots, c: Constellation, symbols: usize) -> usize {
    num.layout().data_carriers(p) * c.bits_per_symbol() * symbols
}

/// Data symbols `modulate` uses for `nbits` bits (Normal, at least one).
pub fn data_symbols(c: Constellation, nbits: usize) -> usize {
    data_symbols_for(Numerology::Normal, c, nbits).max(1)
}

/// Data symbols [`modulate_full`] uses for `nbits` bits with robust
/// pilots: 0 for none (a header-only burst).
pub fn data_symbols_for(num: Numerology, c: Constellation, nbits: usize) -> usize {
    data_symbols_with(num, Pilots::Robust, c, nbits)
}

/// Data symbols for `nbits` bits with pilot pattern `p`.
pub fn data_symbols_with(num: Numerology, p: Pilots, c: Constellation, nbits: usize) -> usize {
    nbits.div_ceil(num.layout().data_carriers(p) * c.bits_per_symbol())
}

/// Length in audio samples of the burst `modulate` makes for `nbits`
/// (Normal; 0 bits still sends one data symbol).
pub fn burst_samples(c: Constellation, nbits: usize) -> usize {
    OVERHEAD_AUDIO + data_symbols(c, nbits) * SYMBOL_AUDIO
}

/// Length in audio samples of the burst [`modulate_full`] makes for
/// `nbits` with robust pilots. `nbits = 0` gives the header-only burst:
/// the preamble and two copies of the header.
pub fn burst_samples_for(num: Numerology, c: Constellation, nbits: usize) -> usize {
    burst_samples_with(num, Pilots::Robust, c, nbits)
}

/// [`burst_samples_for`] with pilot pattern `p`.
pub fn burst_samples_with(num: Numerology, p: Pilots, c: Constellation, nbits: usize) -> usize {
    let l = num.layout();
    let nsym = match data_symbols_with(num, p, c, nbits) {
        0 => (HO_COPIES - 1) * l.hdr_syms,
        n => n,
    };
    overhead_audio(l) + nsym * l.sym() * DEC
}

/// Header copies in a header-only burst: the first is an ordinary header
/// (decodable on its own); the second continues the coded sequence.
const HO_COPIES: usize = 2;

fn check_center(num: Numerology, center_hz: f32) {
    debug_assert!(
        (num.min_center_hz()..=MAX_CENTER_HZ).contains(&center_hz),
        "ion::phy: centre {center_hz} Hz out of range for {num:?}"
    );
}
