//! Burst layout: numerologies, pilot patterns, preamble sequences,
//! scrambler, bit interleaver and constellations; the header code is in
//! [`header`].
//!
//! Grid symbol `g = 0` is the full-band reference symbol B, `g = 1..=H`
//! carry the header (`H` = [`Layout::hdr_syms`]) and `g > H` carry data.
//! Carrier `c` (0..nc) sits on FFT bin `c - (nc - 1)/2`, i.e. at
//! `center + spacing·(c - (nc - 1)/2)` Hz.

use ion_dsp::dsp::Complex;

mod header;

pub use header::*;

/// Baseband sample rate, Hz.
pub const FS: f64 = 8000.0;
/// Decimation from 48 kHz audio.
pub const DEC: usize = 6;
/// Raised-cosine ramp at both ends of a burst (2 ms).
pub const RAMP: usize = 16;
/// Largest number of data symbols the 10-bit header field can name.
pub const MAX_DATA_SYMS: usize = 1023;
/// Scattered/continual pilot amplitude (+2.5 dB, as DVB-T).
pub const PILOT_AMP: f32 = 4.0 / 3.0;
/// Largest FFT length and carrier count of any numerology (stack buffers).
pub const N_MAX: usize = 320;
pub const NC_MAX: usize = 109;
/// Most bits per cell of any constellation (64QAM).
pub const BPS_MAX: usize = 6;

/// Which pilot pattern the data symbols of a burst use. The header always
/// uses its layout's robust pattern; a header bit says which one follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Pilots {
    /// The layout's dense pattern: low SNR, fast fading.
    #[default]
    Robust,
    /// Fewer pilots for the upper levels (same frequency grid, sampled
    /// less often in time). Long layouts have only one pattern.
    Sparse,
}

/// Scattered pilots: carrier `c` is a pilot in symbol `g` when
/// `c mod step = shift·(g mod period)`, plus the top carrier in every
/// symbol. With `nc = step·k + 1` every symbol has `k + 1` pilots, and
/// after time interpolation every `shift`-th carrier is sampled every
/// `period` symbols (`shift · period = step`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pattern {
    pub step: usize,
    pub shift: usize,
    pub period: usize,
}

impl Pattern {
    #[inline]
    pub fn is_pilot(&self, nc: usize, c: usize, g: usize) -> bool {
        c == nc - 1 || c % self.step == self.shift * (g % self.period)
    }

    /// Pilots per symbol on `nc` carriers.
    pub const fn pilots(&self, nc: usize) -> usize {
        (nc - 1) / self.step + 1
    }
}

/// DVB-T's pattern: every 12th carrier, shifted by 3 each symbol (150 Hz ×
/// 4 symbols at 50 Hz spacing).
const P12: Pattern = Pattern {
    step: 12,
    shift: 3,
    period: 4,
};

/// Everything that differs between the numerologies: the waveform, and
/// the receiver constants derived from it (see `docs/phy.md`).
#[derive(Debug)]
pub struct Layout {
    /// FFT length (useful symbol), samples at 8 kHz.
    pub n: usize,
    /// Cyclic prefix, samples.
    pub cp: usize,
    /// Symbol windowing (WOLA), samples: the first `wola` samples of each
    /// cyclic prefix cross-fade (raised cosine) with a cyclic continuation
    /// of the symbol before; costs `wola` samples of echo tolerance. Off
    /// everywhere: it barely narrows the occupied bandwidth (FCC
    /// §97.3(a)(8)), which the outer carriers' own spectra set.
    pub wola: usize,
    /// Used carriers (`step·k + 1` for both patterns, so every symbol of a
    /// pattern has the same number of pilots).
    pub nc: usize,
    /// Pilot patterns: header and robust data symbols; sparse data symbols.
    pub robust: Pattern,
    pub sparse: Pattern,
    /// Preamble A uses the bins that are multiples of `a_step`, so it
    /// repeats every `n / a_step` samples.
    pub a_step: usize,
    /// Length of preamble A, ramp included.
    pub a_len: usize,
    /// Header symbols: their QPSK slots (352..=408 per copy) hold the
    /// 384 coded header bits once (a few truncated or repeated).
    pub hdr_syms: usize,
    /// XOR masks on the header CRC: data bursts, header-only bursts.
    pub crc_data: u16,
    pub crc_ho: u16,
    /// Cutoff of the 48 kHz TX interpolation / RX front-end filter, Hz,
    /// and its length in taps (odd; Blackman-windowed sinc, so the
    /// transition band is about 5.5 · 48 kHz / `filter_len` wide).
    pub filter_cutoff: f32,
    pub filter_len: usize,
    /// Lowest valid audio centre, Hz: the lowest carrier stays above
    /// ~75 Hz and the band does not fold over 0 Hz.
    pub min_center_hz: f32,
    // -- receiver --
    /// Schmidl-Cox window (A_LEN − period − RAMP − CP: the metric plateau
    /// is then one CP long).
    pub sc_win: usize,
    /// Detection threshold on |P|²/R².
    pub m_on: f32,
    /// A detector peak counts once the metric has stayed below it for this
    /// many periods of A. Two suffice for a short window; a long window's
    /// metric wanders slowly and has local peaks hundreds of samples early
    /// on its rising edge.
    pub sc_drop: usize,
    /// B matched-filter search, ± samples.
    pub mf_span: usize,
    /// B's matched filter uses only the carriers A leaves empty: a long A
    /// on few carriers, all of them B's, otherwise correlates with B
    /// nearly as well as B itself at low SNR.
    pub mf_a_free: bool,
    /// Normalised B matched-filter peak needed to go on with a candidate.
    pub rho_min: f32,
    /// Normalised whole-preamble correlation needed to accept it.
    pub rho_full_min: f32,
    /// Energy window for timing and the FFT window back-off; the two add
    /// up to less than the CP.
    pub e_win: usize,
    pub b_backoff: usize,
    /// Timing: the energy window is the middle of the windows capturing
    /// at least this share of the best window's energy.
    pub plateau: f32,
    /// Track sample-clock drift. Off for Half and Sixth: across so few
    /// carriers a frequency-selective fade looks like a delay slope, and on
    /// CCIR poor the estimate runs to its ±300 ppm limit.
    pub drift: bool,
    /// Preamble noise estimate: smoothing half-width (carriers) and
    /// shrinkage towards the global mean (pseudo-samples).
    pub noise_half: usize,
    pub noise_pseudo: f32,
    /// Decision-directed noise: smoothing half-width (carriers).
    pub dd_half: usize,
    /// Channel-model grid: one-sided Doppler (Hz) × delay span (samples),
    /// for the robust and the sparse pattern (whose time grid aliases at a
    /// lower Doppler).
    pub fd: [f64; 3],
    pub fd_sparse: [f64; 3],
    pub tm: [f64; 3],
    /// Pilot search window of the estimator, symbols and carriers.
    pub gw: usize,
    pub cw: usize,
    /// Pilots per channel estimate (at most `est::KMAX`), and a factor on
    /// the time scale of the distance that picks them (1: half the first
    /// zero of the time correlation).
    pub est_k: usize,
    pub est_lg: f32,
}

/// 50 Hz spacing, 4 ms CP, 49 carriers (2.45 kHz).
pub const NORMAL: Layout = Layout {
    n: 160,
    cp: 32,
    wola: 0,
    nc: 49,
    robust: P12,
    // Every 24th carrier, shift 3: 150 Hz × 8 symbols (±2.6 Hz).
    sparse: Pattern {
        step: 24,
        shift: 3,
        period: 8,
    },
    a_step: 4,
    a_len: 352,
    hdr_syms: 4,
    crc_data: 0,
    crc_ho: 0xB7E1,
    filter_cutoff: 1600.0,
    filter_len: 481,
    min_center_hz: 1300.0,
    sc_win: 264,
    m_on: 0.035,
    sc_drop: 2,
    mf_span: 80,
    mf_a_free: false,
    rho_min: 0.06,
    rho_full_min: 0.08,
    e_win: 24,
    b_backoff: 4,
    plateau: 0.95,
    drift: true,
    noise_half: 6,
    noise_pseudo: 24.0,
    dd_half: 2,
    fd: [0.3, 1.0, 2.5],
    fd_sparse: [0.3, 1.0, 2.0],
    // Up to 6 ms: an echo at the CP's length (MPD's 4 ms) plus the
    // filters' tails, once the window straddles both paths.
    tm: [8.0, 16.0, 48.0],
    gw: 12,
    cw: 12,
    est_k: 20,
    est_lg: 1.0,
};

/// 25 Hz spacing, 10 ms CP, 97 carriers: long echoes (NVIS).
pub const LONG: Layout = Layout {
    n: 320,
    cp: 80,
    wola: 0,
    nc: 97,
    robust: P12,
    sparse: P12,
    a_step: 5,
    a_len: 640,
    hdr_syms: 2,
    crc_data: 0x5A0F,
    crc_ho: 0xE45B,
    filter_cutoff: 1600.0,
    filter_len: 481,
    min_center_hz: 1300.0,
    sc_win: 480,
    m_on: 0.02,
    sc_drop: 2,
    mf_span: 160,
    mf_a_free: false,
    rho_min: 0.05,
    rho_full_min: 0.055,
    e_win: 68,
    b_backoff: 6,
    plateau: 0.99,
    drift: true,
    noise_half: 12,
    noise_pseudo: 48.0,
    dd_half: 4,
    fd: [0.3, 1.0, 2.0],
    fd_sparse: [0.3, 1.0, 2.0],
    tm: [16.0, 40.0, 80.0],
    gw: 12,
    cw: 24,
    est_k: 20,
    est_lg: 1.0,
};

/// Normal, 2.75 kHz wide: 55 carriers (54 = 18·3 = 27·2, so the patterns
/// are step 18 and 27 instead of 12 and 24).
pub const NORMAL_WIDE: Layout = Layout {
    nc: 55,
    // Every 18th carrier, shift 3: 150 Hz × 6 symbols (±3.5 Hz).
    robust: Pattern {
        step: 18,
        shift: 3,
        period: 6,
    },
    // Every 27th carrier, shift 3: 150 Hz × 9 symbols (±2.3 Hz).
    sparse: Pattern {
        step: 27,
        shift: 3,
        period: 9,
    },
    crc_data: 0x3C96,
    crc_ho: 0x8D2B,
    filter_cutoff: 1720.0,
    min_center_hz: 1450.0,
    ..NORMAL
};

/// Long, 2.75 kHz wide: 109 carriers (9·12 + 1).
pub const LONG_WIDE: Layout = Layout {
    nc: 109,
    crc_data: 0xC3A5,
    crc_ho: 0x71D8,
    filter_cutoff: 1720.0,
    min_center_hz: 1450.0,
    ..LONG
};

/// A 650 Hz slice of the passband: Normal's 50 Hz spacing and 4 ms CP on
/// 13 carriers (bins −6..6), so three slices fit side by side in an SSB
/// channel (centres 750, 1500, 2250 Hz); see `docs/phy.md`, "Slice
/// numerology".
///
/// - pilots every 6th carrier, shift 3, period 2; sparse = Normal's DVB-T
///   pattern;
/// - A on every 2nd bin (period 10 ms, pull-in ±50 Hz), four times
///   Normal's length: a slice carries ~4× Normal's energy per cell, so the
///   preamble needs ~4× the duration to keep its margin over the data; the
///   18 header symbols likewise;
/// - a 1681-tap filter with a 370 Hz cutoff: flat over ±300 Hz of
///   carriers, ≥ 70 dB down beyond ±450 Hz, where the neighbouring slice's
///   carriers start.
pub const SLICE: Layout = Layout {
    n: 160,
    cp: 32,
    wola: 0,
    nc: 13,
    // Every 6th carrier, shift 3: 150 Hz × 2 symbols (±10.4 Hz).
    robust: Pattern {
        step: 6,
        shift: 3,
        period: 2,
    },
    sparse: P12,
    a_step: 2,
    a_len: 1408,
    hdr_syms: 18,
    crc_data: 0x6E3A,
    crc_ho: 0x1BC7,
    filter_cutoff: 370.0,
    filter_len: 1681,
    min_center_hz: 500.0,
    sc_win: 1280,
    m_on: 0.035,
    sc_drop: 2,
    mf_span: 160,
    mf_a_free: false,
    rho_min: 0.06,
    rho_full_min: 0.11,
    e_win: 24,
    b_backoff: 4,
    plateau: 0.95,
    drift: true,
    noise_half: 3,
    noise_pseudo: 24.0,
    dd_half: 2,
    fd: [0.3, 1.0, 2.5],
    fd_sparse: [0.3, 1.0, 2.5],
    tm: [8.0, 16.0, 32.0],
    gw: 12,
    cw: 12,
    // With Normal's rule the nearest pilots all sit on the two pilot
    // carriers next to a cell; take every pilot carrier of the slice over
    // a short stretch of time instead.
    est_k: 40,
    est_lg: 0.15,
};

/// Slim: the other way to 650 Hz, Long's 10 ms CP on 25 carriers 26.7 Hz
/// apart. n = 300 gives preamble A a 75-sample period, unrelated to the
/// others' 40, 64 and 80.
pub const SLIM: Layout = Layout {
    n: 300,
    cp: 80,
    nc: 25,
    a_step: 4,
    a_len: 700,
    hdr_syms: 8,
    crc_data: 0x96E3,
    crc_ho: 0x2B5C,
    filter_cutoff: 650.0,
    min_center_hz: 450.0,
    sc_win: 529,
    m_on: 0.04,
    mf_span: 150,
    rho_min: 0.10,
    rho_full_min: 0.12,
    noise_half: 6,
    noise_pseudo: 24.0,
    cw: 12,
    ..LONG
};

/// Half: 350 Hz, the slice's spacing and CP on 7 carriers (bins −3..3),
/// scaled from the slice as the slice is from Normal. The filter is
/// [`SIXTH`]'s, for six slices 400 Hz apart.
pub const HALF: Layout = Layout {
    nc: 7,
    a_len: 2464,
    hdr_syms: 36,
    crc_data: 0x4D27,
    crc_ho: 0xA9C3,
    filter_cutoff: SIXTH.filter_cutoff,
    filter_len: SIXTH.filter_len,
    min_center_hz: 300.0,
    sc_win: 2336,
    sc_drop: SIXTH.sc_drop,
    rho_full_min: SIXTH.rho_full_min,
    drift: false,
    noise_half: 2,
    est_k: 24,
    sparse: Pattern {
        step: 6,
        shift: 3,
        period: 2,
    },
    ..SLICE
};

/// A sixth of the passband: Long's 25 Hz spacing and 10 ms CP on the
/// slice's 13 carriers (bins −6..6, 325 Hz), so six fit 400 Hz apart
/// (centres 500..2500 Hz); see `docs/phy.md`, "Six slices".
///
/// - pilots as the slice's in carrier indices: 75 Hz × 2 symbols (100 ms,
///   ±5 Hz), unaliased to 13.3 ms of delay;
/// - A on every 4th bin (3 carriers, period 10 ms, pull-in ±50 Hz): every
///   2nd bin would be a 20 ms period, ±25 Hz;
/// - A as long as Half's: the same bandwidth and power, so the same energy
///   per Hz;
/// - the filter is flat to the outer carriers (±150 Hz) and ≥ 70 dB down
///   from ±250 Hz, where the neighbour's carriers start;
/// - the detector waits 14 periods for its peak, and B's matched filter
///   leaves out A's three carriers: with a long A on few carriers both
///   otherwise lock hundreds of samples off at low SNR.
pub const SIXTH: Layout = Layout {
    n: 320,
    cp: 80,
    wola: 0,
    nc: 13,
    robust: SLICE.robust,
    sparse: SLICE.robust,
    a_step: 4,
    a_len: 2464,
    hdr_syms: 18,
    crc_data: 0x5C71,
    crc_ho: 0xE2B9,
    filter_cutoff: 197.0,
    filter_len: 2521,
    min_center_hz: 300.0,
    sc_win: 2464 - 80 - RAMP - 80,
    m_on: 0.035,
    sc_drop: 14,
    mf_span: 320,
    mf_a_free: true,
    rho_min: 0.06,
    rho_full_min: 0.13,
    e_win: 68,
    b_backoff: 6,
    plateau: 0.99,
    drift: false,
    noise_half: 3,
    noise_pseudo: 24.0,
    dd_half: 2,
    fd: [0.3, 1.0, 2.0],
    fd_sparse: [0.3, 1.0, 2.0],
    tm: [16.0, 40.0, 80.0],
    gw: 12,
    cw: 12,
    est_k: 40,
    est_lg: 0.15,
};

/// Quarter: 250 Hz, 5 carriers (bins −2..2).
pub const QUARTER: Layout = Layout {
    nc: 5,
    robust: Pattern {
        step: 4,
        shift: 2,
        period: 2,
    },
    sparse: Pattern {
        step: 4,
        shift: 2,
        period: 2,
    },
    a_len: 3520,
    hdr_syms: 60,
    crc_data: 0x3B95,
    crc_ho: 0xD64E,
    filter_cutoff: 180.0,
    min_center_hz: 300.0,
    sc_win: 3392,
    noise_half: 2,
    est_k: 20,
    ..SLICE
};

impl Layout {
    /// Whole OFDM symbol, samples.
    pub const fn sym(&self) -> usize {
        self.n + self.cp
    }

    /// Grid symbols before the data: B and the header.
    pub const fn pre_syms(&self) -> usize {
        1 + self.hdr_syms
    }

    /// FFT bin of carrier 0.
    pub const fn k_first(&self) -> i32 {
        -((self.nc as i32 - 1) / 2)
    }

    /// Period of preamble A, samples.
    pub const fn a_period(&self) -> usize {
        self.n / self.a_step
    }

    /// Active carriers of preamble A.
    pub const fn a_carriers(&self) -> usize {
        2 * ((self.nc - 1) / 2 / self.a_step) + 1
    }

    /// Carrier index of the `m`-th carrier of preamble A.
    pub const fn a_carrier(&self, m: usize) -> usize {
        let half = (self.nc - 1) / 2;
        half - (half / self.a_step) * self.a_step + m * self.a_step
    }

    pub const fn pattern(&self, p: Pilots) -> &Pattern {
        match p {
            Pilots::Robust => &self.robust,
            Pilots::Sparse => &self.sparse,
        }
    }

    /// Data carriers per symbol of pattern `p`.
    pub const fn data_carriers(&self, p: Pilots) -> usize {
        self.nc - self.pattern(p).pilots(self.nc)
    }

    /// QPSK slots of one header copy.
    pub const fn hdr_slots(&self) -> usize {
        self.hdr_syms * self.data_carriers(Pilots::Robust) * 2
    }

    /// Σ|X|² of one symbol of pattern `p`: unit data cells and boosted
    /// pilots.
    pub fn p_sym_of(&self, p: Pilots) -> f32 {
        self.data_carriers(p) as f32 + self.pattern(p).pilots(self.nc) as f32 * PILOT_AMP * PILOT_AMP
    }

    /// [`p_sym_of`](Self::p_sym_of) the robust pattern: the reference for
    /// the preamble's power.
    pub fn p_sym(&self) -> f32 {
        self.p_sym_of(Pilots::Robust)
    }

    /// Baseband samples from the start of A to the start of B's useful part.
    pub const fn a_to_b(&self) -> usize {
        self.a_len + self.cp
    }

    /// Carrier spacing, Hz.
    pub fn spacing_hz(&self) -> f64 {
        FS / self.n as f64
    }

    /// Baseband samples of a burst that are not data symbols.
    pub const fn overhead_bb(&self) -> usize {
        self.a_len + self.pre_syms() * self.sym() + RAMP
    }

    /// Offset from the detector peak to A's start: the middle of the
    /// metric's plateau.
    pub const fn sc_peak_to_a(&self) -> i64 {
        ((RAMP + self.sc_win + self.a_period() - 1 + self.a_len - 1) / 2) as i64
    }

    /// The frequency-correlation model is centred on this delay (samples
    /// after the FFT window start): the middle of the energy window.
    pub fn delay_center(&self) -> f64 {
        (self.b_backoff + self.e_win / 2) as f64
    }

    /// FFT bin of carrier `c`.
    #[inline]
    pub fn bin(&self, c: usize) -> i32 {
        self.k_first() + c as i32
    }

    /// Bin index modulo `n` (for twiddle tables).
    #[inline]
    pub fn bin_mod(&self, c: usize, n: usize) -> usize {
        (self.bin(c) + n as i32) as usize % n
    }

    /// True when carrier `c` is an empty bin of preamble A.
    #[inline]
    pub fn a_empty(&self, c: usize) -> bool {
        self.bin(c).rem_euclid(self.a_step as i32) != 0
    }

    /// Known value of a pilot cell (`g >= 1`): BPSK from a hash, boosted.
    #[inline]
    pub fn pilot_value(&self, c: usize, g: usize) -> Complex {
        let s = if mix32((g * self.nc + c) as u32 ^ 0x5eed_1e75) & 1 == 0 {
            PILOT_AMP
        } else {
            -PILOT_AMP
        };
        Complex::new(s, 0.0)
    }

    /// Reference symbol B: every carrier, Newman phases (low PAPR, sharp
    /// autocorrelation), scaled to `p_sym` and boosted by `boost`
    /// (amplitude).
    pub fn b_value(&self, c: usize, boost: f32) -> Complex {
        let amp = (self.p_sym() / self.nc as f32).sqrt() * boost;
        let ph = core::f64::consts::PI * (c * c) as f64 / self.nc as f64;
        Complex::new((amp as f64 * ph.cos()) as f32, (amp as f64 * ph.sin()) as f32)
    }

    /// Value of the `m`-th carrier of preamble A, Newman phases.
    pub fn a_value(&self, m: usize, boost: f32) -> Complex {
        let na = self.a_carriers();
        let amp = (self.p_sym() / na as f32).sqrt() * boost;
        let ph = core::f64::consts::PI * (m * m) as f64 / na as f64;
        Complex::new((amp as f64 * ph.cos()) as f32, (amp as f64 * ph.sin()) as f32)
    }
}

/// Where the pilots of one burst are: symbol B (`g = 0`) is all known,
/// the header symbols (`1..hdr_end`) use the robust pattern, the data
/// symbols the burst's own.
#[derive(Clone, Copy, Debug)]
pub struct Map {
    pub l: &'static Layout,
    /// First data grid symbol (`pre_syms` + further header copies).
    pub hdr_end: usize,
    pub pilots: Pilots,
    /// Amplitude of the data symbols (pilots included) against the
    /// header's ([`DataMode::gain`]).
    pub gain: f32,
}

impl Map {
    pub fn new(l: &'static Layout, hdr_end: usize, pilots: Pilots) -> Self {
        Self {
            l,
            hdr_end,
            pilots,
            gain: 1.0,
        }
    }

    pub fn with_gain(self, gain: f32) -> Self {
        Self { gain, ..self }
    }

    /// Known value of pilot cell (`c`, `g >= 1`).
    #[inline]
    pub fn pilot_value(&self, c: usize, g: usize) -> Complex {
        let v = self.l.pilot_value(c, g);
        if g >= self.hdr_end { v.scale(self.gain) } else { v }
    }

    /// Pattern of grid symbol `g >= 1`.
    #[inline]
    pub fn pattern(&self, g: usize) -> &'static Pattern {
        if g < self.hdr_end {
            &self.l.robust
        } else {
            self.l.pattern(self.pilots)
        }
    }

    /// True when cell (`c`, `g`) carries a known value.
    #[inline]
    pub fn is_pilot(&self, c: usize, g: usize) -> bool {
        g == 0 || self.pattern(g).is_pilot(self.l.nc, c, g)
    }

    /// Data carriers of symbol `g >= 1`.
    #[inline]
    pub fn data_carriers(&self, g: usize) -> usize {
        self.l.nc - self.pattern(g).pilots(self.l.nc)
    }

    /// Data carriers of grid symbol `g >= 1`, ascending, into `out`.
    pub fn data_carriers_of(&self, g: usize, out: &mut [u8]) {
        let mut j = 0;
        for c in 0..self.l.nc {
            if !self.is_pilot(c, g) {
                out[j] = c as u8;
                j += 1;
            }
        }
        debug_assert_eq!(j, self.data_carriers(g));
    }
}

/// Step of the data gain a header can name, dB.
pub const GAIN_STEP_DB: f32 = 0.75;
/// Largest data gain code (2 bits): caps the data symbols' rise over the
/// preamble and header at 2.25 dB.
pub const GAIN_CODES: u8 = 3;

/// How the data symbols of a burst are sent (header bits 3..1): plain
/// OFDM, or DFT-spread (`Spreader`), and the data symbols' gain over the
/// preamble and header, in [`GAIN_STEP_DB`] steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DataMode {
    pub spread: bool,
    pub gain_code: u8,
}

impl DataMode {
    pub const PLAIN: DataMode = DataMode {
        spread: false,
        gain_code: 0,
    };

    /// The mode nearest a requested gain (rounded to the step, clamped to
    /// `GAIN_CODES` steps).
    pub fn new(spread: bool, gain_db: f32) -> Self {
        let q = (gain_db / GAIN_STEP_DB).round().clamp(0.0, GAIN_CODES as f32) as u8;
        Self { spread, gain_code: q }
    }

    pub fn gain_db(self) -> f32 {
        self.gain_code as f32 * GAIN_STEP_DB
    }

    /// Amplitude factor of the data symbols.
    pub fn gain(self) -> f32 {
        10f32.powf(self.gain_db() / 20.0)
    }

    /// The three header bits (3..1 of the reserved field).
    pub fn bits(self) -> u32 {
        (self.spread as u32) << 1 | (self.gain_code as u32) << 2
    }

    pub fn from_bits(r: u32) -> Self {
        Self {
            spread: r >> 1 & 1 == 1,
            gain_code: (r >> 2 & 3) as u8,
        }
    }
}

/// DFT spreading (DFT-s-OFDM, SC-FDMA): the `m` data cells of a symbol
/// are replaced by their unitary `m`-point DFT before the carriers are
/// synthesised, so the data part of the symbol is a single-carrier signal
/// (lower envelope peaks); the receiver equalises per carrier and takes
/// the inverse DFT. Pilots stay where they are.
pub struct Spreader {
    m: usize,
    /// e^{-j2πk/m}.
    tw: Vec<Complex>,
    a: Vec<Complex>,
    b: Vec<Complex>,
}

impl Spreader {
    pub fn new(m: usize) -> Self {
        let tw = (0..m.max(1))
            .map(|k| {
                let a = -core::f64::consts::TAU * k as f64 / m.max(1) as f64;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Self {
            m,
            tw,
            a: vec![Complex::ZERO; m],
            b: vec![Complex::ZERO; m],
        }
    }

    /// Unitary DFT (`inverse`: IDFT) of `self.a` into `self.b`.
    fn transform(&mut self, inverse: bool) {
        let m = self.m;
        let s = 1.0 / (m as f32).sqrt();
        for (k, o) in self.b.iter_mut().enumerate() {
            let mut acc = Complex::ZERO;
            let mut idx = 0;
            for v in &self.a {
                let w = self.tw[idx];
                acc += *v * if inverse { w.conj() } else { w };
                idx += k;
                if idx >= m {
                    idx -= m;
                }
            }
            *o = acc.scale(s);
        }
    }

    /// Spreads the cells `x[dcar[j]]` in place.
    pub fn spread_cells(&mut self, x: &mut [Complex], dcar: &[u8]) {
        debug_assert_eq!(dcar.len(), self.m);
        for (j, &c) in dcar.iter().enumerate() {
            self.a[j] = x[c as usize];
        }
        self.transform(false);
        for (j, &c) in dcar.iter().enumerate() {
            x[c as usize] = self.b[j];
        }
    }

    /// Forward DFT of `v` in place (`m` long).
    pub fn forward(&mut self, v: &mut [Complex]) {
        self.a.copy_from_slice(v);
        self.transform(false);
        v.copy_from_slice(&self.b);
    }

    /// Inverse DFT of `v` in place (`m` long).
    pub fn inverse(&mut self, v: &mut [Complex]) {
        self.a.copy_from_slice(v);
        self.transform(true);
        v.copy_from_slice(&self.b);
    }
}

/// Data-carrier lists of a pattern's phases: `table[g % period]`.
pub fn data_carrier_table(l: &Layout, p: &Pattern) -> Vec<Vec<u8>> {
    (0..p.period)
        .map(|ph| {
            (0..l.nc)
                .filter(|&c| !p.is_pilot(l.nc, c, ph))
                .map(|c| c as u8)
                .collect()
        })
        .collect()
}

/// 32-bit integer hash (a public-domain "lowbias32" mixer).
#[inline]
pub fn mix32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Scrambler bit `i` of stream `seed` (random access).
#[inline]
pub fn scramble(seed: u32, i: usize) -> u8 {
    (mix32((i as u32).wrapping_mul(0x9e37_79b9) ^ seed) >> 7) as u8 & 1
}

pub const SEED_DATA: u32 = 0x4865_726d;
pub const SEED_HDR: u32 = 0x6573_4844;

/// Step of the linear bit interleaver `i -> (a·i) mod n`: the integer
/// nearest `n/φ` that is coprime with `n` (golden-ratio spreading).
pub fn interleave_step(n: usize) -> usize {
    if n <= 2 {
        return 1;
    }
    let mut a = ((n as f64) * 0.618_033_988_75).round() as usize;
    while gcd(a, n) != 1 {
        a += 1;
    }
    a
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

// ---------------------------------------------------------------------------
// Constellations
// ---------------------------------------------------------------------------

/// Points indexed by their Gray label (bit 0 of the label is the last bit
/// of the group), unit average energy.
pub struct Points {
    pub bps: usize,
    pub pts: [Complex; 64],
}

impl Points {
    pub fn count(&self) -> usize {
        1 << self.bps
    }
}

pub fn qpsk() -> Points {
    let mut pts = [Complex::ZERO; 64];
    let k = core::f32::consts::FRAC_1_SQRT_2;
    for (label, p) in pts.iter_mut().enumerate().take(4) {
        let b0 = (label >> 1) & 1;
        let b1 = label & 1;
        *p = Complex::new(k * (1.0 - 2.0 * b0 as f32), k * (1.0 - 2.0 * b1 as f32));
    }
    Points { bps: 2, pts }
}

pub fn psk8() -> Points {
    const GRAY: [usize; 8] = [0, 1, 3, 2, 6, 7, 5, 4];
    let mut pts = [Complex::ZERO; 64];
    for (m, &label) in GRAY.iter().enumerate() {
        let a = core::f64::consts::FRAC_PI_4 * m as f64 + core::f64::consts::PI / 8.0;
        pts[label] = Complex::new(a.cos() as f32, a.sin() as f32);
    }
    Points { bps: 3, pts }
}

/// Square QAM with `b` Gray-coded bits per axis (label = I bits, Q bits).
fn qam(b: usize) -> Points {
    let m = 1usize << b;
    // Mean energy of the per-axis levels ±1, ±3, ...: (m² − 1)/3 per axis.
    let k = 1.0 / ((2.0 * (m * m - 1) as f32) / 3.0).sqrt();
    let mut pts = [Complex::ZERO; 64];
    for i in 0..m {
        for q in 0..m {
            let label = ((i ^ (i >> 1)) << b) | (q ^ (q >> 1));
            pts[label] = Complex::new(
                k * (2.0 * i as f32 - (m - 1) as f32),
                k * (2.0 * q as f32 - (m - 1) as f32),
            );
        }
    }
    Points { bps: 2 * b, pts }
}

/// Gray per axis: 00 -> -3, 01 -> -1, 11 -> +1, 10 -> +3.
pub fn qam16() -> Points {
    qam(2)
}

/// Gray per axis over −7..+7.
pub fn qam64() -> Points {
    qam(3)
}

/// Every layout, for the checks below and the tests.
pub const LAYOUTS: [&Layout; 9] = [
    &NORMAL,
    &LONG,
    &NORMAL_WIDE,
    &LONG_WIDE,
    &SLICE,
    &SLIM,
    &HALF,
    &QUARTER,
    &SIXTH,
];

const _: () = {
    let mut i = 0;
    while i < LAYOUTS.len() {
        let l = LAYOUTS[i];
        assert!(l.hdr_slots() >= 352 && l.hdr_slots() <= HDR_SLOTS_MAX);
        assert!(l.nc <= NC_MAX && l.n <= N_MAX);
        assert!(l.filter_len % 2 == 1 && l.n.is_multiple_of(l.a_step));
        assert!((l.nc - 1).is_multiple_of(l.robust.step) && (l.nc - 1).is_multiple_of(l.sparse.step));
        assert!(l.robust.shift * l.robust.period == l.robust.step);
        assert!(l.sparse.shift * l.sparse.period == l.sparse.step);
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_constant_data_carriers() {
        for l in LAYOUTS {
            for p in [Pilots::Robust, Pilots::Sparse] {
                let pat = l.pattern(p);
                for g in 1..40 {
                    let n = (0..l.nc).filter(|&c| !pat.is_pilot(l.nc, c, g)).count();
                    assert_eq!(n, l.data_carriers(p), "nc={} g={g}", l.nc);
                }
                // After time interpolation every `shift`-th carrier is a
                // pilot.
                for c in (0..l.nc).step_by(pat.shift) {
                    assert!((0..pat.period).any(|g| pat.is_pilot(l.nc, c, g)), "c={c}");
                }
                assert_eq!(data_carrier_table(l, pat).len(), pat.period);
            }
            // A's carriers are exactly the non-empty bins.
            let a: Vec<usize> = (0..l.a_carriers()).map(|m| l.a_carrier(m)).collect();
            let full: Vec<usize> = (0..l.nc).filter(|&c| !l.a_empty(c)).collect();
            assert_eq!(a, full);
            assert_eq!(l.bin(a[0]), -l.bin(*a.last().unwrap()));
        }
        assert_eq!(NORMAL.sc_peak_to_a(), 335);
        assert_eq!(NORMAL.data_carriers(Pilots::Robust), 44);
        assert_eq!(NORMAL.hdr_slots(), 352);
        assert_eq!(LONG.hdr_slots(), 352);
        assert_eq!(NORMAL_WIDE.data_carriers(Pilots::Robust), 51);
        assert_eq!(NORMAL_WIDE.data_carriers(Pilots::Sparse), 52);
        assert_eq!(LONG_WIDE.data_carriers(Pilots::Robust), 99);
        assert_eq!(SLICE.data_carriers(Pilots::Robust), 10);
        assert_eq!(SLICE.data_carriers(Pilots::Sparse), 11);
        assert_eq!(SLICE.hdr_slots(), 360);
        assert_eq!(SLICE.a_carriers(), 7);
        assert_eq!(SIXTH.data_carriers(Pilots::Robust), 10);
        assert_eq!(SIXTH.hdr_slots(), 360);
        assert_eq!((SIXTH.a_carriers(), SIXTH.a_period()), (3, 80));
        assert_eq!((HALF.a_carriers(), HALF.a_period()), (3, 80));
    }

    #[test]
    fn constellations_unit_energy() {
        for p in [qpsk(), psk8(), qam16(), qam64()] {
            let e: f32 = p.pts[..p.count()].iter().map(|x| x.norm_sqr()).sum::<f32>() / p.count() as f32;
            assert!((e - 1.0).abs() < 1e-5);
        }
        // 16QAM labels are part of the wire format.
        let q = qam16();
        let k = 1.0 / 10f32.sqrt();
        assert!((q.pts[0b1001] - Complex::new(3.0 * k, -k)).norm() < 1e-6);
        // 64QAM: Gray, neighbours differ in one bit.
        let q = qam64();
        for a in 0..64 {
            for b in 0..64 {
                let d = (q.pts[a] - q.pts[b]).norm() * 42f32.sqrt();
                if (d - 2.0).abs() < 1e-3 {
                    assert_eq!((a ^ b).count_ones(), 1, "{a} {b}");
                }
            }
        }
    }
}
