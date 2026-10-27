//! Robust floor of the Ion ladder: noncoherent 32-ary MFSK bursts for
//! flutter, long echoes and negative SNR. See `docs/mfsk.md`.
//!
//! One phase-continuous tone at a time, frequency changes swept over 1 ms.
//! R0: 28 ms symbols (20 ms window + 8 ms guard against echoes), 50 Hz
//! grid of 48 bins. R1: 14 ms symbols, all window, 71.4 Hz grid of 32 bins.
//!
//! A burst is a 16-symbol Costas preamble (always R0's numerology), a
//! convolutionally coded header (user, extra, length, CRC-16) and the data:
//! scrambled, interleaved over the whole burst and Gray-mapped onto tones
//! that hop across the band symbol by symbol.

mod conv;
mod frame;
mod id;
mod rx;
mod util;

use core::f64::consts::TAU;

pub use id::{decode_short_frames, decode_short_frames_at, decode_short_frames_prior, demap};
pub use rx::MfskRx;

use frame::{Fmt, Kind, PRE_LEN};
use ion_dsp::TX_LEVEL;

/// Robust waveforms, most robust first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MfskMode {
    /// The floor: as low an SNR as possible, flutter and long echoes.
    R0,
    /// Faster, a little less robust.
    R1,
}

impl MfskMode {
    pub const ALL: [MfskMode; 2] = [MfskMode::R0, MfskMode::R1];
}

/// Time–frequency scale of the waveform: every duration × `k`, every
/// tone's offset from the centre ÷ `k`. [`MfskLayout::FULL`] (k = 1) is
/// the full-band R0 and R1 the link uses; R1 at k = 7 fits a 400 Hz slot
/// (`docs/mfsk.md`, Below the floor). The receiver decimates k times more
/// and sees the same waveform in its own samples, so a channel with delay
/// spread τ and Doppler spread σ looks to it like (τ/k, k·σ).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MfskLayout {
    pub k: usize,
    /// Frequency sweep between tones and the end ramps, audio samples.
    pub sweep: usize,
    /// Detector frequency hypotheses: ±`hmax` steps of half the preamble's
    /// tone spacing (25/k Hz).
    pub hmax: i32,
}

impl MfskLayout {
    pub const FULL: MfskLayout = MfskLayout {
        k: 1,
        sweep: TRANSITION,
        hmax: 3,
    };

    /// Stretched `k` times, the sweep with it; the detector searches ±75 Hz
    /// or as far as its spectrum allows (±225/k Hz).
    pub const fn scaled(k: usize) -> MfskLayout {
        assert!(k >= 1);
        let h = 3 * k as i32;
        MfskLayout {
            k,
            sweep: TRANSITION * k,
            hmax: if h < 9 { h } else { 9 },
        }
    }

    /// Baseband decimation from 48 kHz audio.
    pub(crate) const fn dec(self) -> usize {
        DEC * self.k
    }
}

/// Fixed numbers the link layer needs for timing and sizing.
#[derive(Clone, Copy, Debug)]
pub struct MfskInfo {
    /// Coded bits per second during the data part of a burst.
    pub raw_bps: f32,
    /// Preamble + header, seconds.
    pub overhead_s: f32,
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: f32,
}

/// Decimation from 48 kHz audio to the 8 kHz complex baseband.
pub(crate) const DEC: usize = 6;
/// Baseband rate.
pub(crate) const BB_RATE: f32 = 8000.0;
/// Preamble (and R0) symbol, baseband samples (28 ms).
pub(crate) const SYM: usize = 224;
/// Preamble (and R0) symbol, audio samples.
pub const SYMBOL_SAMPLES: usize = SYM * DEC;
/// Preamble (and R0) symbol duration, seconds.
pub const SYMBOL_S: f32 = 0.028;
/// Preamble (and R0) tone spacing (= 1 / analysis window), Hz.
pub const SPACING_HZ: f32 = 50.0;
/// Frequency sweep between tones and the start/end ramps, audio samples
/// (1 ms; it eats into the guard, so keep it short).
pub(crate) const TRANSITION: usize = 48;

/// Baseband frequency of preamble (R0 grid) bin `b`.
pub(crate) fn bin_hz(b: i32) -> f32 {
    (b as f32 - 23.5) * SPACING_HZ
}

/// Header and data symbol duration of `mode`, seconds.
pub fn symbol_s(mode: MfskMode) -> f32 {
    Fmt::of(mode).sym as f32 / BB_RATE
}

pub fn info(mode: MfskMode) -> MfskInfo {
    info_with(MfskLayout::FULL, mode)
}

/// [`info`] of `mode` in `layout`.
pub fn info_with(layout: MfskLayout, mode: MfskMode) -> MfskInfo {
    let fmt = Fmt::of(mode);
    let k = layout.k as f32;
    MfskInfo {
        raw_bps: fmt.bps() as f32 / symbol_s(mode) / k,
        overhead_s: k * (PRE_LEN as f32 * SYMBOL_S + frame::header_symbols(mode, Kind::Data) as f32 * symbol_s(mode))
            + layout.sweep as f32 / 48_000.0,
        bandwidth_hz: (fmt.nbins + 1) as f32 * fmt.spacing_hz() / k,
    }
}

/// Duration of a header-only burst (`modulate` with no bits), seconds.
pub fn header_only_s(mode: MfskMode) -> f32 {
    burst_samples(mode, 0) as f32 / 48_000.0
}

/// [`header_only_s`] in `layout`.
pub fn header_only_s_with(layout: MfskLayout, mode: MfskMode) -> f32 {
    burst_samples_with(layout, mode, 0) as f32 / 48_000.0
}

/// Most data bits one burst can carry.
pub fn max_bits(mode: MfskMode) -> usize {
    frame::max_bits(mode)
}

/// Audio samples (48 kHz) of the burst `modulate` makes for `nbits` bits.
pub fn burst_samples(mode: MfskMode, nbits: usize) -> usize {
    burst_samples_with(MfskLayout::FULL, mode, nbits)
}

/// [`burst_samples`] in `layout`.
pub fn burst_samples_with(layout: MfskLayout, mode: MfskMode, nbits: usize) -> usize {
    let kind = if nbits == 0 { Kind::HeaderOnly } else { Kind::Data };
    let syms = frame::header_symbols(mode, kind) + frame::data_symbols(mode, nbits);
    layout.k * (PRE_LEN * SYMBOL_SAMPLES + syms * Fmt::of(mode).sym * DEC) + layout.sweep
}

/// Tones of one symbol: up to three baseband frequencies (Hz from
/// `center_hz`) and the symbol's length in audio samples.
#[derive(Clone, Copy)]
struct SymTones {
    n: usize,
    hz: [f32; 3],
    len: usize,
}

/// One burst: preamble, header (mode, length, `user`, `extra`), data.
/// `bits` may be empty: a header-only burst carrying just `user` and
/// `extra` (32 bits), used for tiny ACKs at the floor. 48 kHz audio
/// centred on `center_hz`, peak about `ion_dsp::TX_LEVEL`.
pub fn modulate(mode: MfskMode, user: u16, extra: u16, bits: &[u8], center_hz: f32) -> Vec<f32> {
    modulate_with(MfskLayout::FULL, mode, user, extra, bits, center_hz)
}

/// [`modulate`] in `layout`.
pub fn modulate_with(
    layout: MfskLayout,
    mode: MfskMode,
    user: u16,
    extra: u16,
    bits: &[u8],
    center_hz: f32,
) -> Vec<f32> {
    assert!(
        bits.len() <= max_bits(mode),
        "phy_mfsk: {} bits exceed the {} a burst can carry",
        bits.len(),
        max_bits(mode)
    );
    let data = frame::data_values(mode, bits);
    build(layout, mode, user, extra, bits.len(), data, center_hz)
}

/// [`modulate`] for whole 32-ary code symbols (0..32, e.g. the GF(32)
/// short code's): each becomes one tone, interleaved over the burst, and
/// the header marks the burst as symbol-mapped, so the receiver hands out
/// the tone metrics in code-symbol order ([`MfskBurst::symbols`]). The
/// burst is as long as `modulate`'s for `5 × syms.len()` bits.
pub fn modulate_symbols(mode: MfskMode, user: u16, extra: u16, syms: &[u8], center_hz: f32) -> Vec<f32> {
    modulate_symbols_with(MfskLayout::FULL, mode, user, extra, syms, center_hz)
}

/// [`modulate_symbols`] in `layout`.
pub fn modulate_symbols_with(
    layout: MfskLayout,
    mode: MfskMode,
    user: u16,
    extra: u16,
    syms: &[u8],
    center_hz: f32,
) -> Vec<f32> {
    let nbits = 5 * syms.len();
    assert!(
        nbits <= max_bits(mode),
        "phy_mfsk: {} symbols exceed a burst",
        syms.len()
    );
    if syms.is_empty() {
        return modulate_with(layout, mode, user, extra, &[], center_hz);
    }
    let data = frame::data_values_sym(mode, syms);
    build(layout, mode, user, extra, nbits | frame::SYM_FLAG, data, center_hz)
}

/// Preamble, header (length field `nbits_field`, 0 for header-only) and
/// the data symbols' tone values.
fn build(
    layout: MfskLayout,
    mode: MfskMode,
    user: u16,
    extra: u16,
    nbits_field: usize,
    data: Vec<[u16; 3]>,
    center_hz: f32,
) -> Vec<f32> {
    let fmt = Fmt::of(mode);
    let k = layout.k as f32;
    let nbits = nbits_field & !frame::SYM_FLAG;
    let kind = if nbits == 0 { Kind::HeaderOnly } else { Kind::Data };
    let info = frame::header_bits(mode, kind, user, extra, nbits_field);
    let mut values = frame::header_values(mode, kind, &info);
    values.extend(data);

    let mut syms: Vec<SymTones> = (0..PRE_LEN)
        .map(|i| SymTones {
            n: 1,
            hz: [bin_hz(frame::costas_bin(i) as i32) / k, 0.0, 0.0],
            len: SYMBOL_SAMPLES * layout.k,
        })
        .collect();
    for (i, v) in values.iter().enumerate() {
        let h = fmt.hop(i);
        let mut hz = [0f32; 3];
        for (t, f) in hz.iter_mut().enumerate().take(fmt.tones) {
            *f = fmt.bin_hz(fmt.tone_bin(h, t, v[t] as usize) as i32) / k;
        }
        syms.push(SymTones {
            n: fmt.tones,
            hz,
            len: fmt.sym * DEC * layout.k,
        });
    }
    let out = synthesize(&syms, center_hz, layout.sweep);
    debug_assert_eq!(out.len(), burst_samples_with(layout, mode, nbits));
    out
}

fn ramp(len: usize) -> Vec<f64> {
    (0..len)
        .map(|s| 0.5 - 0.5 * (core::f64::consts::PI * (s as f64 + 0.5) / len as f64).cos())
        .collect()
}

/// Tone frequencies (Hz) of every generator for one symbol; generators a
/// symbol does not use keep their previous frequency.
fn freqs(s: &SymTones, prev: &[f64; 3], center_hz: f32) -> [f64; 3] {
    let mut f = *prev;
    for (fk, &h) in f.iter_mut().zip(&s.hz).take(s.n) {
        *fk = (center_hz + h) as f64;
    }
    f
}

/// Envelope peak of each symbol's steady part (unit tone amplitude; 1 for
/// single-tone symbols), replaying the generator phases the way
/// `synthesize` advances them.
fn plan_peaks(syms: &[SymTones], center_hz: f32, sweep: usize) -> Vec<f64> {
    let fs = 48_000.0f64;
    let ramp = ramp(sweep);
    let mut phase = [0f64; 3];
    let mut freq = [0f64; 3];
    let mut on = [false; 3];
    syms.iter()
        .map(|s| {
            let f_new = freqs(s, &freq, center_hz);
            for k in 0..3 {
                if !on[k] {
                    freq[k] = f_new[k];
                }
            }
            let mut p_end = [0f64; 3];
            for k in 0..3 {
                let mut p = phase[k];
                for &w in &ramp {
                    p += (freq[k] + (f_new[k] - freq[k]) * w) / fs;
                }
                p_end[k] = p;
            }
            let mut peak = 1f64;
            if s.n > 1 {
                peak = 0.0;
                for m in 0..(s.len - sweep) / 4 {
                    let t = (4 * m) as f64 / fs;
                    let (mut re, mut im) = (0f64, 0f64);
                    for k in 0..s.n {
                        let (sn, cs) = (TAU * (p_end[k] + f_new[k] * t)).sin_cos();
                        re += cs;
                        im += sn;
                    }
                    peak = peak.max(re * re + im * im);
                }
                peak = peak.sqrt();
            }
            for k in 0..3 {
                let steady = (s.len - sweep) as f64 * f_new[k] / fs;
                phase[k] = p_end[k] + steady;
                phase[k] -= phase[k].floor();
                on[k] = k < s.n;
            }
            freq = f_new;
            peak
        })
        .collect()
}

/// Generates the audio. Every tone generator keeps its phase across
/// symbols; at a symbol start its frequency and amplitude move along a
/// raised cosine over `sweep` samples. A limiter on the analytic
/// envelope keeps the peak at `TX_LEVEL` where several tones briefly line
/// up inside a sweep.
fn synthesize(syms: &[SymTones], center_hz: f32, sweep: usize) -> Vec<f32> {
    let fs = 48_000.0f64;
    let total = syms.iter().map(|s| s.len).sum::<usize>() + sweep;
    let mut out = Vec::with_capacity(total);
    let mut phase = [0f64; 3];
    let mut freq = [0f64; 3];
    let mut amp = [0f64; 3];
    let ramp = ramp(sweep);
    let plan = plan_peaks(syms, center_hz, sweep);
    let cap = TX_LEVEL as f64;
    let emit = |re: f64, im: f64, out: &mut Vec<f32>| {
        let e = (re * re + im * im).sqrt();
        let g = if e > cap { cap / e } else { 1.0 };
        out.push((re * g) as f32);
    };
    for (s, peak) in syms.iter().zip(&plan) {
        // Each symbol's steady envelope peaks at TX_LEVEL (a multi-tone
        // symbol's tones share TX_LEVEL / peak).
        let a_new = TX_LEVEL as f64 / peak;
        let f_new = freqs(s, &freq, center_hz);
        let mut a_tgt = [0f64; 3];
        for k in 0..3 {
            if k < s.n {
                a_tgt[k] = a_new;
            }
            if amp[k] == 0.0 {
                // A generator starting from silence starts on its tone.
                freq[k] = f_new[k];
            }
        }
        let (f_old, a_old) = (freq, amp);
        for smp in 0..s.len {
            let (mut re, mut im) = (0f64, 0f64);
            for k in 0..3 {
                let (f, a) = if let Some(&w) = ramp.get(smp) {
                    (
                        f_old[k] + (f_new[k] - f_old[k]) * w,
                        a_old[k] + (a_tgt[k] - a_old[k]) * w,
                    )
                } else {
                    (f_new[k], a_tgt[k])
                };
                phase[k] += f / fs;
                phase[k] -= phase[k].floor();
                if a != 0.0 {
                    let (sn, cs) = (TAU * phase[k]).sin_cos();
                    re += a * cs;
                    im += a * sn;
                }
            }
            emit(re, im, &mut out);
        }
        freq = f_new;
        amp = a_tgt;
    }
    // Ramp down.
    for &r in &ramp {
        let w = 1.0 - r;
        let (mut re, mut im) = (0f64, 0f64);
        for k in 0..3 {
            phase[k] += freq[k] / fs;
            phase[k] -= phase[k].floor();
            let (sn, cs) = (TAU * phase[k]).sin_cos();
            re += amp[k] * w * cs;
            im += amp[k] * w * sn;
        }
        emit(re, im, &mut out);
    }
    out
}

/// A burst the receiver expects: stations that share a clock and a net's
/// schedule know when a reply or a check-in should start, and often what
/// its header holds. See [`MfskRx::expect`].
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    /// Audio sample, counted from the first sample the receiver was given,
    /// where the burst (its preamble) should start.
    pub start_sample: u64,
    /// How far off the start may be, audio samples (clock error, PTT and
    /// propagation delay). Inside it the detector takes weaker preambles.
    pub tol_samples: u64,
    /// The header the burst should carry, if known. When its header does
    /// not decode, the receiver tests for this one instead.
    pub header: Option<ExpectedHeader>,
    /// A second header it may carry instead (a polled station answers with
    /// a header-only burst or a data burst). Each is tested only against
    /// bursts of its own kind (header-only or data), so two headers double
    /// the false-alarm bound.
    pub alt: Option<ExpectedHeader>,
    /// Pass mark of that test, nats; see [`EXPECT_MIN_SCORE`].
    pub min_score: f32,
    /// How far the expected header must beat every other `user` value,
    /// nats; see [`EXPECT_MIN_MARGIN`].
    pub min_margin: f32,
}

/// The header fields of an expected burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedHeader {
    pub mode: MfskMode,
    pub user: u16,
    pub extra: u16,
    /// Data bits (0: a header-only burst).
    pub nbits: usize,
    /// The data is symbol-mapped ([`modulate_symbols`], the link's GF(32)
    /// frames; `nbits` is then five per code symbol).
    pub symbols: bool,
    /// Bits of `user` and `extra` the receiver does not know: any value
    /// of them is accepted (at most [`MAX_UNKNOWN_BITS`] in all). The test
    /// is then for the mixture of every allowed header, which costs up to
    /// ln(2^bits) nats of score.
    pub user_mask: u16,
    pub extra_mask: u16,
}

/// Most unknown header bits an [`ExpectedHeader`] may leave open.
pub const MAX_UNKNOWN_BITS: u32 = 8;

impl ExpectedHeader {
    /// A header known exactly.
    pub const fn exact(mode: MfskMode, user: u16, extra: u16, nbits: usize) -> Self {
        Self {
            mode,
            user,
            extra,
            nbits,
            symbols: false,
            user_mask: 0,
            extra_mask: 0,
        }
    }

    pub fn allows_user(&self, u: u16) -> bool {
        (u ^ self.user) & !self.user_mask == 0
    }

    pub fn allows_extra(&self, x: u16) -> bool {
        (x ^ self.extra) & !self.extra_mask == 0
    }

    /// Every (user, extra) the expectation allows; empty if it leaves more
    /// than [`MAX_UNKNOWN_BITS`] open.
    pub fn candidates(&self) -> Vec<(u16, u16)> {
        let bits = self.user_mask.count_ones() + self.extra_mask.count_ones();
        if bits > MAX_UNKNOWN_BITS {
            return Vec::new();
        }
        // Spread a counter over the mask's bits.
        let spread = |mut n: u32, mask: u16| -> u16 {
            let mut out = 0u16;
            for b in 0..16 {
                if mask >> b & 1 == 1 {
                    out |= ((n & 1) as u16) << b;
                    n >>= 1;
                }
            }
            out
        };
        let nu = 1u32 << self.user_mask.count_ones();
        let nx = 1u32 << self.extra_mask.count_ones();
        let mut out = Vec::with_capacity((nu * nx) as usize);
        for i in 0..nu {
            let u = self.user & !self.user_mask | spread(i, self.user_mask);
            for j in 0..nx {
                out.push((u, self.extra & !self.extra_mask | spread(j, self.extra_mask)));
            }
        }
        out
    }
}

/// Default pass mark of the expected-header test: anything but the
/// expected header passes it with probability at most 3·e^−12 ≈ 2·10⁻⁵
/// per slot (three analysis windows tried).
pub const EXPECT_MIN_SCORE: f32 = 12.0;

/// Default margin of the expected header over the likeliest other `user`
/// value (another net's id, another burst code): e^6 ≈ 400 to 1.
pub const EXPECT_MIN_MARGIN: f32 = 6.0;

/// One received MFSK burst.
#[derive(Clone, Debug)]
pub struct MfskBurst {
    pub mode: MfskMode,
    pub user: u16,
    pub extra: u16,
    /// One calibrated LLR per data bit (ln P(0)/P(1)); empty for a
    /// header-only burst.
    pub llrs: Vec<f32>,
    /// Tone metrics of the data symbols: per symbol and tone, the
    /// log-likelihood of every value, the best at 0; for
    /// [`decode_short_frames`]. Bit-mapped bursts: in tone-slot order and
    /// tone values (for iterative demapping). Symbol-mapped bursts
    /// ([`modulate_symbols`]): in code-symbol order and code-symbol values.
    /// Empty for a header-only burst.
    pub metrics: Vec<f32>,
    /// The burst was sent with [`modulate_symbols`]; `llrs` are then the
    /// code symbols' bits, five per symbol, MSB first.
    pub symbols: bool,
    /// Estimated SNR in 2.4 kHz, dB.
    pub snr_db: f32,
    pub freq_offset_hz: f32,
    pub start_sample: u64,
    /// Sample-clock offset tracked over the data, ppm (positive: the burst
    /// arrived stretched); 0 for bursts too short or noisy to tell.
    pub clock_ppm: f32,
    /// Delay and Doppler spread and effective SNR over the data (only the
    /// effective SNR, equal to `snr_db`, for a header-only burst).
    pub chan: crate::chan::ChannelMeas,
}

#[cfg(test)]
mod expect_tests {
    use super::*;

    #[test]
    fn candidates_cover_exactly_the_unknown_bits() {
        let h = ExpectedHeader::exact(MfskMode::R0, 0x1234, 0x0005, 0);
        assert_eq!(h.candidates(), vec![(0x1234, 0x0005)]);
        let h = ExpectedHeader {
            user_mask: 4 << 12,
            extra_mask: 63 << 6,
            ..h
        };
        let c = h.candidates();
        assert_eq!(c.len(), 128);
        let set: std::collections::BTreeSet<_> = c.iter().copied().collect();
        assert_eq!(set.len(), 128);
        assert!(c.iter().all(|&(u, x)| h.allows_user(u) && h.allows_extra(x)));
        assert!(c.contains(&(0x5234, 0x0005 | 37 << 6)));
        assert!(!h.allows_extra(0x0006) && !h.allows_user(0x1235));
        let wide = ExpectedHeader {
            extra_mask: 0x01FF,
            ..h
        };
        assert!(wide.candidates().is_empty());
    }
}

#[doc(hidden)]
pub mod debug {
    //! Test hooks for the diagnostics in `tests/phy_mfsk.rs`.
    pub use super::conv::free_symbol_distance;
    pub use super::rx::RxStats;

    /// Tone values of the data symbols `modulate` sends for `bits` (per
    /// symbol, per tone): for synthetic-channel tests of the demapper.
    pub fn data_tones(mode: super::MfskMode, bits: &[u8]) -> Vec<u16> {
        let fmt = super::Fmt::of(mode);
        super::frame::data_values(mode, bits)
            .iter()
            .flat_map(|v| v[..fmt.tones].to_vec())
            .collect()
    }
}
