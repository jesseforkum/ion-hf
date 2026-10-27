//! Burst framing shared by the modulator and the receiver: preamble, hop
//! sequence, header layout and code, data scrambling, interleaving and
//! Gray mapping.

use super::MfskMode;
use super::conv;
use super::util::{crc16, interleave_mult, mix64, push_bits, read_bits};

/// Symbols of the preamble.
pub const PRE_LEN: usize = 16;
/// Tone bins on R0's 50 Hz grid (0..48), which the preamble also uses.
pub const NBINS: usize = 48;
/// Most data symbols in one burst.
pub const MAX_DATA_SYMBOLS: usize = 4095;

/// Welch Costas array of order 16 (p = 17, primitive root 3):
/// c_i = 3^i mod 17, i = 0..15, values 1..16.
pub const COSTAS: [u8; PRE_LEN] = [1, 3, 9, 10, 13, 5, 15, 11, 16, 14, 8, 7, 4, 12, 2, 6];

/// Tone bin of preamble symbol `i`: Costas values spread 150 Hz apart.
pub fn costas_bin(i: usize) -> usize {
    3 * COSTAS[i] as usize - 2
}

/// Per-mode numerology and tone format of the header and data symbols
/// (the preamble is always R0's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fmt {
    /// Simultaneous tones.
    pub tones: usize,
    /// Bits per tone.
    pub bpt: usize,
    /// Tone bins of the grid; with several tones, each gets
    /// `nbins / tones` of them.
    pub nbins: usize,
    /// Symbol, baseband samples.
    pub sym: usize,
    /// Orthogonal analysis window, baseband samples: the tone spacing is
    /// `BB_RATE / win`.
    pub win: usize,
}

impl Fmt {
    pub const fn of(mode: MfskMode) -> Fmt {
        match mode {
            // 28 ms symbols, 20 ms window: 50 Hz grid, 8 ms guard.
            MfskMode::R0 => Fmt {
                tones: 1,
                bpt: 5,
                nbins: NBINS,
                sym: 224,
                win: 160,
            },
            // 14 ms symbols, all window: 71.4 Hz grid, no guard.
            MfskMode::R1 => Fmt {
                tones: 1,
                bpt: 5,
                nbins: 32,
                sym: 112,
                win: 112,
            },
        }
    }
    pub const fn vals(self) -> usize {
        1 << self.bpt
    }
    pub const fn bps(self) -> usize {
        self.tones * self.bpt
    }
    /// Tone spacing, Hz.
    pub fn spacing_hz(self) -> f32 {
        super::BB_RATE / self.win as f32
    }
    /// Baseband frequency of bin `b` (may be outside 0..nbins for
    /// neighbours).
    pub fn bin_hz(self, b: i32) -> f32 {
        (b as f32 - (self.nbins as f32 - 1.0) / 2.0) * self.spacing_hz()
    }
    /// Hop offset (0..nbins) of header/data symbol `i` (counted from the
    /// first header symbol).
    pub fn hop(self, i: usize) -> usize {
        (mix64(i as u64 ^ 0x4D46_534B_484F_5000) % self.nbins as u64) as usize
    }
    /// Bin of value `v` of tone `t` in a symbol with hop `h`.
    pub fn tone_bin(self, h: usize, t: usize, v: usize) -> usize {
        (h + self.nbins / self.tones * t + v) % self.nbins
    }
}

pub fn gray(v: usize) -> usize {
    v ^ (v >> 1)
}

pub fn gray_inv(mut g: usize) -> usize {
    let mut v = g;
    while g > 1 {
        g >>= 1;
        v ^= g;
    }
    v
}

/// Header type: ACK-style (user + extra only) or followed by data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    HeaderOnly,
    Data,
}

/// Header hypotheses in the order the receiver tries them (shortest
/// first).
pub const HYPS: [(MfskMode, Kind); 4] = [
    (MfskMode::R1, Kind::HeaderOnly),
    (MfskMode::R1, Kind::Data),
    (MfskMode::R0, Kind::HeaderOnly),
    (MfskMode::R0, Kind::Data),
];

/// Information bits of a header, CRC included.
pub fn header_info_bits(kind: Kind) -> usize {
    match kind {
        Kind::HeaderOnly => 16 + 16 + 16,
        Kind::Data => 16 + 16 + 16 + 16,
    }
}

/// Trellis steps (= tone slots) of a header: one per information bit (the
/// code is tail-biting).
pub fn header_steps(fmt: Fmt, kind: Kind) -> usize {
    assert_eq!(header_info_bits(kind) % fmt.tones, 0);
    header_info_bits(kind)
}

pub fn header_symbols(mode: MfskMode, kind: Kind) -> usize {
    let fmt = Fmt::of(mode);
    header_steps(fmt, kind) / fmt.tones
}

/// The CRC's initial value carries the mode, the header type and the
/// layout's time scale `k`, so a header decoded under the wrong hypothesis,
/// or by a receiver of another layout, fails its check. The full band
/// (k = 1) keeps the values it always had.
fn crc_init(k: usize, mode: MfskMode, kind: Kind) -> u16 {
    let base = match (mode, kind) {
        (MfskMode::R0, Kind::HeaderOnly) => 0xFFFF,
        (MfskMode::R0, Kind::Data) => 0x1D0F,
        (MfskMode::R1, Kind::HeaderOnly) => 0xB4C8,
        (MfskMode::R1, Kind::Data) => 0x6A39,
    };
    base ^ layout_mask(k)
}

/// What a layout of time scale `k` adds to the CRC's initial value: none
/// for the full band, never none for a scaled layout.
fn layout_mask(k: usize) -> u16 {
    if k == 1 {
        0
    } else {
        mix64(0x4C41_594F_5554 ^ k as u64) as u16 | 1
    }
}

/// Bit 15 of a data header's length field: the data symbols carry whole
/// code symbols ([`data_values_sym`]) rather than interleaved bits. The
/// length itself (data bits, five per symbol) never reaches 2¹⁵.
pub const SYM_FLAG: usize = 1 << 15;

/// Header information bits: user, extra, [nbits, with [`SYM_FLAG`] for a
/// symbol-mapped burst], CRC-16, for a burst in a layout of time scale `k`.
pub fn header_bits(k: usize, mode: MfskMode, kind: Kind, user: u16, extra: u16, nbits: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    push_bits(&mut b, user as u32, 16);
    push_bits(&mut b, extra as u32, 16);
    if kind == Kind::Data {
        push_bits(&mut b, nbits as u32, 16);
    }
    let crc = crc16(&b, crc_init(k, mode, kind));
    push_bits(&mut b, crc as u32, 16);
    b
}

/// Checks the CRC (layout of time scale `k`) and the fields; returns
/// (user, extra, nbits, symbol mapped).
pub fn parse_header(k: usize, mode: MfskMode, kind: Kind, b: &[u8]) -> Option<(u16, u16, usize, bool)> {
    let n = header_info_bits(kind) - 16;
    let crc = read_bits(&b[n..], 16) as u16;
    if crc16(&b[..n], crc_init(k, mode, kind)) != crc {
        return None;
    }
    let user = read_bits(b, 16) as u16;
    let extra = read_bits(&b[16..], 16) as u16;
    let (nbits, sym) = if kind == Kind::Data {
        let f = read_bits(&b[32..], 16) as usize;
        let (v, sym) = (f & !SYM_FLAG, f & SYM_FLAG != 0);
        let fmt = Fmt::of(mode);
        if v == 0 || v > max_bits(mode) || (sym && (fmt.tones != 1 || v % fmt.bpt != 0)) {
            return None;
        }
        (v, sym)
    } else {
        (0, false)
    };
    Some((user, extra, nbits, sym))
}

pub fn max_bits(mode: MfskMode) -> usize {
    (MAX_DATA_SYMBOLS * Fmt::of(mode).bps()).min(u16::MAX as usize)
}

/// Header slot of trellis step `j` (interleaved over all slots).
pub fn header_slot(fmt: Fmt, kind: Kind, j: usize) -> usize {
    let n = header_steps(fmt, kind);
    (interleave_mult(n) * j) % n
}

/// Tone values of the header, per symbol (`tones` values each).
pub fn header_values(mode: MfskMode, kind: Kind, info: &[u8]) -> Vec<[u16; 3]> {
    let fmt = Fmt::of(mode);
    let steps = header_steps(fmt, kind);
    debug_assert_eq!(fmt.bpt, 5);
    let labels = conv::encode(info);
    debug_assert_eq!(labels.len(), steps);
    let mut slots = vec![0u16; steps];
    for (j, &l) in labels.iter().enumerate() {
        slots[header_slot(fmt, kind, j)] = l;
    }
    slots
        .chunks(fmt.tones)
        .map(|c| {
            let mut v = [0u16; 3];
            v[..c.len()].copy_from_slice(c);
            v
        })
        .collect()
}

pub fn data_symbols(mode: MfskMode, nbits: usize) -> usize {
    nbits.div_ceil(Fmt::of(mode).bps())
}

/// Scrambling bit for data bit `p` (keeps all-zero data from repeating a
/// tone pattern).
pub fn scramble(p: usize) -> u8 {
    (mix64(p as u64 ^ 0x5343_5241_4D42_4C45) & 1) as u8
}

/// Tone values of the data symbols: scramble, pad, interleave over the
/// whole burst, Gray-map each tone's bits.
pub fn data_values(mode: MfskMode, bits: &[u8]) -> Vec<[u16; 3]> {
    let fmt = Fmt::of(mode);
    let nsym = data_symbols(mode, bits.len());
    let n = nsym * fmt.bps();
    let a = interleave_mult(n);
    let mut slots = vec![0u8; n];
    for p in 0..n {
        let b = bits.get(p).copied().unwrap_or(0) & 1;
        slots[(a * p) % n] = b ^ scramble(p);
    }
    (0..nsym)
        .map(|i| {
            let mut v = [0u16; 3];
            for (t, vt) in v.iter_mut().enumerate().take(fmt.tones) {
                let base = i * fmt.bps() + t * fmt.bpt;
                let label = (0..fmt.bpt).fold(0usize, |acc, k| acc | ((slots[base + k] as usize) << k));
                *vt = gray_inv(label) as u16;
            }
            v
        })
        .collect()
}

/// Scrambling value (0..32) of code symbol `p` of a symbol-mapped burst.
pub fn sym_scramble(p: usize) -> u8 {
    (mix64(p as u64 ^ 0x5359_4D42_5343_524D) & 31) as u8
}

/// Tone values of a symbol-mapped data part: each 5-bit code symbol is
/// scrambled and becomes one whole tone, the symbols interleaved over the
/// whole burst (`i → a·i mod n`). Single-tone modes only.
pub fn data_values_sym(mode: MfskMode, syms: &[u8]) -> Vec<[u16; 3]> {
    let fmt = Fmt::of(mode);
    assert!(fmt.tones == 1 && fmt.bpt == 5, "symbol mapping needs one 32-ary tone");
    let n = syms.len();
    let a = interleave_mult(n);
    let mut v = vec![[0u16; 3]; n];
    for (p, &x) in syms.iter().enumerate() {
        v[(a * p) % n][0] = ((x & 31) ^ sym_scramble(p)) as u16;
    }
    v
}

/// Tone metrics of a symbol-mapped data part (`n` × 32 in slot order, tone
/// values) back in code-symbol order and code-symbol values.
pub fn deinterleave_sym(slot: &[f32]) -> Vec<f32> {
    let n = slot.len() / 32;
    let a = interleave_mult(n);
    let mut out = vec![0f32; n * 32];
    for p in 0..n {
        let s = (a * p) % n;
        let r = sym_scramble(p) as usize;
        for c in 0..32 {
            out[p * 32 + c] = slot[s * 32 + (c ^ r)];
        }
    }
    out
}

/// Maps slot LLRs (in symbol/tone/bit order) back to data-bit order and
/// removes the scrambling; returns `nbits` LLRs.
pub fn deinterleave(slot_llrs: &[f32], nbits: usize) -> Vec<f32> {
    let n = slot_llrs.len();
    let a = interleave_mult(n);
    (0..nbits)
        .map(|p| {
            let l = slot_llrs[(a * p) % n];
            if scramble(p) == 1 { -l } else { l }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costas_property() {
        // All displacement vectors (di, dv) distinct.
        let mut seen = std::collections::HashSet::new();
        for (i, &ci) in COSTAS.iter().enumerate() {
            for (j, &cj) in COSTAS.iter().enumerate() {
                if i != j {
                    let d = (j as i32 - i as i32, cj as i32 - ci as i32);
                    assert!(seen.insert(d), "{d:?}");
                }
            }
        }
        assert!((0..PRE_LEN).all(|i| costas_bin(i) < NBINS));
    }

    #[test]
    fn gray_round_trip() {
        for v in 0..32 {
            assert_eq!(gray_inv(gray(v)), v);
            assert_eq!((gray(v) ^ gray(v + 1)).count_ones(), 1);
        }
    }

    #[test]
    fn header_round_trip_fields() {
        for k in [1, 2, 7, 8] {
            for (mode, kind) in HYPS {
                let b = header_bits(k, mode, kind, 0xBEEF, 0x1234, 777);
                let got = parse_header(k, mode, kind, &b).unwrap();
                assert_eq!((got.0, got.1), (0xBEEF, 0x1234));
                for k2 in [1, 2, 7, 8] {
                    for (m2, kind2) in HYPS {
                        if (k2, m2, kind2) != (k, mode, kind) && header_info_bits(kind2) == b.len() {
                            assert!(parse_header(k2, m2, kind2, &b).is_none(), "k {k} as {k2}");
                        }
                    }
                }
            }
        }
        // The full band's checks are what they always were.
        assert_eq!(crc_init(1, MfskMode::R0, Kind::HeaderOnly), 0xFFFF);
        assert_eq!(crc_init(1, MfskMode::R1, Kind::Data), 0x6A39);
    }
}
