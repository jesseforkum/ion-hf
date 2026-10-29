//! The stream's outer erasure code: systematic Cauchy over GF(2^16).
//!
//! A block of `k` source pieces of [`PIECE`] bytes goes out as pieces
//! numbered `0..65536`. Pieces `0..k` are the sources themselves; piece
//! `i >= k` is the sum over every source `s` of `s · 1 / (i ^ s)`. Every
//! square part of a Cauchy matrix is invertible, so any `k` distinct pieces
//! give the block back (MDS): the receiver counts distinct pieces, and
//! decoding is an m × m solve over the m sources it is missing.

use std::sync::OnceLock;

/// Bytes in one piece (14 field elements).
pub const PIECE: usize = 28;
/// Pieces a block may have, sources and repairs together.
pub const MAX_INDEX: usize = 1 << 16;

const W: usize = PIECE / 2;
/// x^16 + x^12 + x^3 + x + 1.
const POLY: u32 = 0x1100B;

struct Tables {
    exp: Vec<u16>,
    log: Vec<u16>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = vec![0u16; 2 * 65535];
        let mut log = vec![0u16; 65536];
        let mut x: u32 = 1;
        for i in 0..65535 {
            exp[i] = x as u16;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x10000 != 0 {
                x ^= POLY;
            }
        }
        for i in 65535..2 * 65535 {
            exp[i] = exp[i - 65535];
        }
        Tables { exp, log }
    })
}

fn mul(a: u16, b: u16) -> u16 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

fn inv(a: u16) -> u16 {
    assert!(a != 0, "no inverse of 0");
    let t = tables();
    t.exp[65535 - t.log[a as usize] as usize]
}

type Sym = [u16; W];

fn to_sym(p: &[u8; PIECE]) -> Sym {
    std::array::from_fn(|i| u16::from_be_bytes([p[2 * i], p[2 * i + 1]]))
}

fn to_piece(s: &Sym) -> [u8; PIECE] {
    let mut p = [0u8; PIECE];
    for (i, v) in s.iter().enumerate() {
        p[2 * i..2 * i + 2].copy_from_slice(&v.to_be_bytes());
    }
    p
}

/// `dst += c · src`.
fn axpy(dst: &mut Sym, c: u16, src: &Sym) {
    if c == 0 {
        return;
    }
    let t = tables();
    let lc = t.log[c as usize] as usize;
    for (d, &s) in dst.iter_mut().zip(src) {
        if s != 0 {
            *d ^= t.exp[lc + t.log[s as usize] as usize];
        }
    }
}

/// Coefficient of source `s` in repair piece `index`.
fn coeff(index: usize, s: usize) -> u16 {
    inv((index ^ s) as u16)
}

/// Source pieces of a block of `len` bytes.
pub fn k_of(len: usize) -> usize {
    len.div_ceil(PIECE).max(1)
}

/// The sending side of one block.
#[derive(Clone, Debug)]
pub struct Encoder {
    src: Vec<Sym>,
}

impl Encoder {
    pub fn new(block: &[u8]) -> Self {
        let k = k_of(block.len());
        assert!(k < MAX_INDEX / 2, "block too big");
        let src = (0..k)
            .map(|i| {
                let mut p = [0u8; PIECE];
                let a = (i * PIECE).min(block.len());
                let b = ((i + 1) * PIECE).min(block.len());
                p[..b - a].copy_from_slice(&block[a..b]);
                to_sym(&p)
            })
            .collect();
        Self { src }
    }

    pub fn k(&self) -> usize {
        self.src.len()
    }

    /// Piece `index` (< [`MAX_INDEX`]).
    pub fn piece(&self, index: usize) -> [u8; PIECE] {
        if index < self.src.len() {
            return to_piece(&self.src[index]);
        }
        let mut acc = [0u16; W];
        for (s, x) in self.src.iter().enumerate() {
            axpy(&mut acc, coeff(index, s), x);
        }
        to_piece(&acc)
    }
}

/// The receiving side of one block: collects distinct pieces until it
/// has `k`, then solves for the missing sources.
#[derive(Clone, Debug)]
pub struct Decoder {
    k: usize,
    len: usize,
    src: Vec<Option<Sym>>,
    repairs: Vec<(usize, Sym)>,
    have_src: usize,
}

impl Decoder {
    /// A block of `len` bytes.
    pub fn new(len: usize) -> Self {
        let k = k_of(len);
        Self {
            k,
            len,
            src: vec![None; k],
            repairs: Vec::new(),
            have_src: 0,
        }
    }

    pub fn k(&self) -> usize {
        self.k
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Distinct pieces held.
    pub fn have(&self) -> usize {
        self.have_src + self.repairs.len()
    }

    /// Enough pieces to decode.
    pub fn complete(&self) -> bool {
        self.have() >= self.k
    }

    /// Adds piece `index`; false if it was a duplicate or out of range.
    pub fn add(&mut self, index: usize, piece: &[u8; PIECE]) -> bool {
        if index >= MAX_INDEX {
            return false;
        }
        if index < self.k {
            if self.src[index].is_some() {
                return false;
            }
            self.src[index] = Some(to_sym(piece));
            self.have_src += 1;
            return true;
        }
        if self.complete() || self.repairs.iter().any(|r| r.0 == index) {
            return false;
        }
        self.repairs.push((index, to_sym(piece)));
        true
    }

    /// The block, once [`Decoder::complete`].
    pub fn decode(&self) -> Option<Vec<u8>> {
        if !self.complete() {
            return None;
        }
        let missing: Vec<usize> = (0..self.k).filter(|&s| self.src[s].is_none()).collect();
        let m = missing.len();
        let mut src: Vec<Sym> = self.src.iter().map(|s| s.unwrap_or([0; W])).collect();
        if m > 0 {
            let rows = &self.repairs[..m];
            // Each repair less the sources already held: the m × m system
            // over the missing ones.
            let mut a: Vec<Vec<u16>> = rows
                .iter()
                .map(|&(i, _)| missing.iter().map(|&s| coeff(i, s)).collect())
                .collect();
            let mut y: Vec<Sym> = rows
                .iter()
                .map(|&(i, ref d)| {
                    let mut v = *d;
                    for (s, x) in self.src.iter().enumerate() {
                        if let Some(x) = x {
                            axpy(&mut v, coeff(i, s), x);
                        }
                    }
                    v
                })
                .collect();
            for c in 0..m {
                let p = (c..m).find(|&r| a[r][c] != 0)?;
                a.swap(c, p);
                y.swap(c, p);
                let iv = inv(a[c][c]);
                for v in a[c][c..].iter_mut() {
                    *v = mul(*v, iv);
                }
                let yc = y[c];
                y[c] = [0; W];
                axpy(&mut y[c], iv, &yc);
                let (pivot_a, pivot_y) = (a[c].clone(), y[c]);
                for r in 0..m {
                    let f = a[r][c];
                    if r == c || f == 0 {
                        continue;
                    }
                    for (v, &pv) in a[r][c..].iter_mut().zip(&pivot_a[c..]) {
                        *v ^= mul(f, pv);
                    }
                    axpy(&mut y[r], f, &pivot_y);
                }
            }
            for (j, &s) in missing.iter().enumerate() {
                src[s] = y[j];
            }
        }
        let mut out: Vec<u8> = src.iter().flat_map(to_piece).collect();
        out.truncate(self.len);
        Some(out)
    }
}

/// CRC-32 of a block (the link's frame CRC), carried in the stream's start
/// so the receiver can check what it decoded.
pub fn block_crc(block: &[u8]) -> u32 {
    crate::link::frame::crc32(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn the_field_is_a_field() {
        let t = tables();
        let mut seen = vec![false; 65536];
        for i in 0..65535 {
            assert!(!seen[t.exp[i] as usize], "the polynomial is not primitive");
            seen[t.exp[i] as usize] = true;
        }
        for a in [1u16, 2, 3, 0x1234, 0xFFFF, 0x8000] {
            assert_eq!(mul(a, inv(a)), 1);
            assert_eq!(mul(a, 1), a);
        }
        assert_eq!(mul(0x1234, mul(0x5678, 0x9ABC)), mul(mul(0x1234, 0x5678), 0x9ABC));
    }

    #[test]
    fn any_k_pieces_give_the_block_back() {
        let data = block(1_000, 7);
        let enc = Encoder::new(&data);
        let k = enc.k();
        assert_eq!(k, 36);
        // Every other source, then repairs from far up the index range.
        let mut idx: Vec<usize> = (0..k).step_by(2).collect();
        idx.extend((0..k).map(|j| 40_000 + 37 * j));
        let mut dec = Decoder::new(data.len());
        for &i in &idx {
            if dec.complete() {
                break;
            }
            assert!(dec.add(i, &enc.piece(i)));
        }
        assert!(!dec.add(0, &enc.piece(0)), "a duplicate counts once");
        assert_eq!(dec.decode().as_deref(), Some(&data[..]));
        // Repairs only.
        let mut dec = Decoder::new(data.len());
        for i in k..2 * k {
            dec.add(i, &enc.piece(i));
        }
        assert_eq!(dec.decode().as_deref(), Some(&data[..]));
        // One short: nothing.
        let mut dec = Decoder::new(data.len());
        for i in 1..k {
            dec.add(i, &enc.piece(i));
        }
        assert!(!dec.complete());
        assert_eq!(dec.decode(), None);
    }

    #[test]
    fn a_large_block_with_most_sources_lost_decodes_in_time() {
        let data = block(16_384, 3);
        let enc = Encoder::new(&data);
        let k = enc.k();
        let mut dec = Decoder::new(data.len());
        // A fifth of the sources, the rest repairs.
        for i in (0..k).filter(|i| i % 5 == 0) {
            dec.add(i, &enc.piece(i));
        }
        let mut i = k;
        while !dec.complete() {
            dec.add(i, &enc.piece(i));
            i += 1;
        }
        let t = std::time::Instant::now();
        assert_eq!(dec.decode().as_deref(), Some(&data[..]));
        let s = t.elapsed().as_secs_f64();
        assert!(s < if cfg!(debug_assertions) { 60.0 } else { 3.0 }, "{s:.2} s");
    }
}
