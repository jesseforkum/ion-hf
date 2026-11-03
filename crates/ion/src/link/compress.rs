//! Text compression for chat, net and keyboard traffic.
//!
//! **Model.** A small context-mixing model after lpaq1: each byte is coded
//! bit by bit with a binary arithmetic coder, and the probability of each
//! bit is a learned mix of predictions from the last 0, 1, 2 and 3 bytes and
//! from the current word. Before the first message both ends feed the same
//! priming text ([`PRIME`]) through the model, so short messages compress
//! from their first byte. Everything is integer arithmetic, so the two ends
//! agree bit for bit on any machine. Changing the priming text, the
//! parameters or the model is a change of format: a new record type
//! ([`CM1`] → `CM2`).
//!
//! **End of message.** Before each byte the coder sends one "end here?"
//! flag, predicted from the last two bytes, so a message needs no length
//! of its own.
//!
//! **Formats.**
//! - A *message* ([`pack`] / [`unpack`], net mode): one type byte, then
//!   the body. [`RAW`]: the bytes as they are, so incompressible data grows
//!   by exactly one byte. [`CM1`]: coded with the primed model, fresh for
//!   each message (net messages reach different stations in different
//!   orders, so nothing is carried between them).
//! - A *record stream* ([`StreamEncoder`] / [`StreamDecoder`], links): the
//!   sender's bytes cut into records of at most [`CHUNK`] bytes, each a
//!   header and a body. Header byte: type in bits 6–7 ([`RAW`] or [`CM1`]),
//!   length in bits 0–5: 0–61 mean 1–62 bytes; 62 means 63 + the next
//!   byte; 63 means 319 + the next two bytes (little-endian).

use std::sync::OnceLock;

/// Record or message type: the bytes as they are.
pub const RAW: u8 = 0;
/// Record or message type: coded with [`V1`] primed by [`PRIME`].
pub const CM1: u8 = 1;
/// Longest piece of a sender's data in one stream record. Each record is
/// delivered whole, so this bounds how long the receiver waits to see
/// part of a long message.
pub const CHUNK: usize = 1024;

/// The priming text both ends feed through the model before the first
/// message: one message per line. Part of the format.
pub const PRIME: &str = include_str!("../../data/chat_train.txt");

/// Model parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Byte-context orders, each with its own hashed table (order 0, the
    /// bits of the current byte alone, is always there).
    pub orders: &'static [u8],
    /// A context of the current word (letters and digits, case folded).
    pub word: bool,
    /// log2 of the slots in each hashed table.
    pub table_bits: u32,
    /// A slot's adaptation slows down as it sees more bits, until this
    /// many (then it keeps a 1/(limit+1.5) rate).
    pub limit: u8,
    /// Mixer learning rate.
    pub mix_rate: i32,
}

/// The parameters of [`CM1`].
pub const V1: Params = Params {
    orders: &[1, 2, 3],
    word: true,
    table_bits: 18,
    limit: 127,
    mix_rate: 24,
};

const EOM_BITS: u32 = 12;
/// Initial "end here" probability (16-bit scale): about 1 in 20.
const EOM_INIT: u32 = 3277;
const EOM_LIMIT: u8 = 60;
/// Longest message [`Model::decode`] will produce (malformed input).
pub const MAX_MESSAGE: usize = 1 << 20;

// -- logistic helpers (integer, from lpaq1) --------------------------------------

/// 1/(1+e^-x/256), 12-bit result, by interpolating 33 points.
fn squash(d: i32) -> i32 {
    const T: [i32; 33] = [
        1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901,
        3975, 4022, 4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094,
    ];
    if d > 2047 {
        return 4095;
    }
    if d < -2047 {
        return 1;
    }
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (T[i] * (128 - w) + T[i + 1] * w + 64) >> 7
}

/// Inverse of [`squash`]: 12-bit probability → ±2047.
fn stretch_table() -> &'static [i16; 4096] {
    static T: OnceLock<[i16; 4096]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i16; 4096];
        let mut pi = 0usize;
        for x in -2047..=2047 {
            let v = squash(x) as usize;
            for slot in t.iter_mut().take(v + 1).skip(pi) {
                *slot = x as i16;
            }
            pi = pi.max(v + 1);
        }
        for slot in t.iter_mut().skip(pi) {
            *slot = 2047;
        }
        t
    })
}

/// 65536 / (n + 1.5): a slot's adaptation rate after `n` updates.
fn recip_table() -> &'static [i32; 256] {
    static T: OnceLock<[i32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i32; 256];
        for (n, r) in t.iter_mut().enumerate() {
            *r = (131_072 / (2 * n as i32 + 3)).max(1);
        }
        t
    })
}

// -- binary arithmetic coder (32-bit, carry-less, from lpaq1) --------------------

/// What a model step does with each bit: code it, decode it, or only learn.
trait BitIo {
    /// `p`: probability of a 1, 12 bits. Returns the bit.
    fn bit(&mut self, p: u32, bit: u32) -> u32;
}

struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self {
            x1: 0,
            x2: u32::MAX,
            out: Vec::new(),
        }
    }

    /// The fewest bytes that pin the final interval, given that the
    /// decoder reads zeros past the end; trailing zeros dropped.
    fn finish(mut self) -> Vec<u8> {
        let top = self.x1 >> 24;
        // x1 itself if its low bytes are zero, else the next multiple of
        // 2^24, which is still at most x2 (their top bytes differ).
        let v = if self.x1 & 0xFF_FFFF == 0 { top } else { top + 1 };
        self.out.push(v as u8);
        while self.out.last() == Some(&0) {
            self.out.pop();
        }
        self.out
    }
}

fn split(x1: u32, x2: u32, p: u32) -> u32 {
    let r = x2 - x1;
    x1 + (r >> 12) * p + (((r & 0xFFF) * p) >> 12)
}

impl BitIo for Encoder {
    fn bit(&mut self, p: u32, bit: u32) -> u32 {
        let xmid = split(self.x1, self.x2, p);
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xFF00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = self.x2 << 8 | 255;
        }
        bit
    }
}

struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    data: &'a [u8],
    at: usize,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut d = Self {
            x1: 0,
            x2: u32::MAX,
            x: 0,
            data,
            at: 0,
        };
        for _ in 0..4 {
            d.x = d.x << 8 | d.next() as u32;
        }
        d
    }

    fn next(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }
}

impl BitIo for Decoder<'_> {
    fn bit(&mut self, p: u32, _: u32) -> u32 {
        let xmid = split(self.x1, self.x2, p);
        let bit = (self.x <= xmid) as u32;
        if bit != 0 {
            self.x2 = xmid;
        } else {
            self.x1 = xmid + 1;
        }
        while (self.x1 ^ self.x2) & 0xFF00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = self.x2 << 8 | 255;
            self.x = self.x << 8 | self.next() as u32;
        }
        bit
    }
}

/// Learning only: every bit as given.
struct Learn;

impl BitIo for Learn {
    fn bit(&mut self, _: u32, bit: u32) -> u32 {
        bit
    }
}

// -- the model ---------------------------------------------------------------------

/// A slot: probability of a 1 (16 bits) and how many updates it has seen
/// (bits 16–23).
type Slot = u32;

fn slot_p(s: Slot) -> i32 {
    (s & 0xFFFF) as i32
}

fn slot_new() -> Slot {
    1 << 15
}

fn slot_update(s: &mut Slot, bit: u32, limit: u8) {
    let n = (*s >> 16) as u8;
    let p = slot_p(*s);
    let target = if bit != 0 { 65535 } else { 0 };
    let r = recip_table()[n as usize];
    // Wraps (from a fresh end-of-message slot, say), as release builds
    // always have: the wrapped value is part of the CM1 format.
    let p = (p + ((target - p).wrapping_mul(r) >> 16)).clamp(32, 65503);
    let n = if n < limit { n + 1 } else { n };
    *s = p as u32 | (n as u32) << 16;
}

fn hash(a: u32, b: u32) -> u32 {
    (a ^ b.wrapping_mul(0x2F0F_3C25))
        .wrapping_mul(0x9E37_79B1)
        .rotate_left(13)
}

/// The predictor and its state. Cloning it copies everything learned
/// (a few MB with [`V1`]).
#[derive(Clone)]
pub struct Model {
    params: Params,
    /// Order 0: 256 slots by the partial byte.
    order0: Vec<Slot>,
    /// One table per order, then the word table.
    tables: Vec<Vec<Slot>>,
    /// 256 weight sets (by partial byte) of inputs + bias.
    weights: Vec<i32>,
    eom: Vec<Slot>,
    /// Last 8 bytes, newest in the low byte.
    hist: u64,
    word: u32,
    /// Per table, the hash of the current byte's context.
    base: Vec<u32>,
    // Scratch for the bit being coded.
    idx: Vec<usize>,
    st: Vec<i32>,
}

impl Model {
    /// An empty model (nothing learned).
    pub fn new(params: Params) -> Self {
        let n = params.orders.len() + params.word as usize;
        let inputs = n + 2; // order 0 and the bias
        Self {
            params,
            order0: vec![slot_new(); 256],
            tables: vec![vec![slot_new(); 1 << params.table_bits]; n],
            weights: vec![(1 << 16) * 3 / 10; 256 * inputs],
            eom: vec![EOM_INIT; 1 << EOM_BITS],
            hist: 0,
            word: 0,
            base: vec![0; n],
            idx: vec![0; n],
            st: vec![0; inputs],
        }
    }

    /// A model with these parameters that has learned `lines` (one
    /// message per line; `\r` ignored, empty lines skipped).
    pub fn primed_with(params: Params, text: &str) -> Self {
        let mut m = Self::new(params);
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if !line.is_empty() {
                m.learn(line.as_bytes());
            }
        }
        m
    }

    /// [`V1`] primed with [`PRIME`]: the model of [`CM1`] (built once,
    /// then cloned).
    pub fn primed() -> Self {
        static BASE: OnceLock<Model> = OnceLock::new();
        BASE.get_or_init(|| Model::primed_with(V1, PRIME)).clone()
    }

    /// Learns a message as if it had been coded.
    pub fn learn(&mut self, msg: &[u8]) {
        self.message(&mut Learn, msg.len(), |i| msg[i]);
    }

    /// Codes a message, learning from it. The result is the body only:
    /// the decoder needs it whole and knows where it ends.
    pub fn encode(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut e = Encoder::new();
        self.message(&mut e, msg.len(), |i| msg[i]);
        e.finish()
    }

    /// Decodes a body made by [`Model::encode`] on a model in the same
    /// state, learning from it. `None` if it runs past `max` bytes
    /// (malformed input, or models out of step).
    pub fn decode(&mut self, body: &[u8], max: usize) -> Option<Vec<u8>> {
        let mut d = Decoder::new(body);
        let mut out = Vec::new();
        self.start();
        loop {
            if self.eom_bit(&mut d, 0) == 1 {
                break;
            }
            if out.len() >= max {
                return None;
            }
            out.push(self.byte(&mut d, 0));
        }
        // A well-formed body is read to its end and no further than the
        // coder's look-ahead.
        if d.at < body.len() {
            return None;
        }
        Some(out)
    }

    fn message(&mut self, io: &mut impl BitIo, len: usize, at: impl Fn(usize) -> u8) {
        self.start();
        for i in 0..len {
            self.eom_bit(io, 0);
            self.byte(io, at(i));
        }
        self.eom_bit(io, 1);
    }

    /// Every message starts as if after a line break.
    fn start(&mut self) {
        self.hist = b'\n' as u64;
        self.word = 0;
    }

    fn eom_bit(&mut self, io: &mut impl BitIo, bit: u32) -> u32 {
        let i = (hash(self.hist as u32 & 0xFFFF, 0xE0E) >> (32 - EOM_BITS)) as usize;
        let p = ((slot_p(self.eom[i]) >> 4) as u32).clamp(1, 4095);
        let y = io.bit(p, bit);
        slot_update(&mut self.eom[i], y, EOM_LIMIT);
        y
    }

    fn byte(&mut self, io: &mut impl BitIo, byte: u8) -> u8 {
        // Context hashes for this byte.
        for (k, &o) in self.params.orders.iter().enumerate() {
            let mask = if o >= 8 { u64::MAX } else { (1u64 << (8 * o)) - 1 };
            let h = self.hist & mask;
            self.base[k] = hash(hash(o as u32 + 1, h as u32), (h >> 32) as u32);
        }
        if self.params.word {
            let k = self.params.orders.len();
            self.base[k] = hash(self.word, 0x5170);
        }
        let n = self.tables.len();
        let shift = 32 - self.params.table_bits;
        let st = stretch_table();
        let mut c0: u32 = 1;
        for i in (0..8).rev() {
            for k in 0..n {
                self.idx[k] = (hash(self.base[k], c0) >> shift) as usize;
                self.st[k] = st[(slot_p(self.tables[k][self.idx[k]]) >> 4) as usize] as i32;
            }
            self.st[n] = st[(slot_p(self.order0[c0 as usize]) >> 4) as usize] as i32;
            self.st[n + 1] = 256;
            let inputs = n + 2;
            let w = &mut self.weights[c0 as usize * inputs..(c0 as usize + 1) * inputs];
            let dot: i64 = w.iter().zip(&self.st).map(|(&a, &b)| a as i64 * b as i64).sum();
            let dot = ((dot >> 16) as i32).clamp(-2047, 2047);
            let p = squash(dot).clamp(1, 4095);
            let y = io.bit(p as u32, (byte >> i & 1) as u32);
            // Mixer: gradient step on coding cost.
            let err = ((y as i32) << 12) - p;
            let rate = self.params.mix_rate;
            for (wi, &x) in w.iter_mut().zip(&self.st) {
                *wi += (x * err * rate + (1 << 15)) >> 16;
            }
            for k in 0..n {
                slot_update(&mut self.tables[k][self.idx[k]], y, self.params.limit);
            }
            slot_update(&mut self.order0[c0 as usize], y, self.params.limit);
            c0 = c0 << 1 | y;
        }
        let b = c0 as u8;
        self.hist = self.hist << 8 | b as u64;
        let lower = b.to_ascii_lowercase();
        self.word = if lower.is_ascii_alphanumeric() {
            hash(self.word, lower as u32 + 1)
        } else {
            0
        };
        b
    }
}

// -- messages (net mode) -----------------------------------------------------------

/// One message, on its own: [`CM1`] with a freshly primed model if that
/// is shorter, else [`RAW`] (one byte longer than `msg`).
pub fn pack(msg: &[u8]) -> Vec<u8> {
    let body = Model::primed().encode(msg);
    let mut out = Vec::with_capacity(1 + body.len().min(msg.len()));
    if body.len() < msg.len() {
        out.push(CM1);
        out.extend(body);
    } else {
        out.push(RAW);
        out.extend_from_slice(msg);
    }
    out
}

/// The message [`pack`] made; `None` if it is malformed.
pub fn unpack(b: &[u8]) -> Option<Vec<u8>> {
    let (&t, body) = b.split_first()?;
    match t {
        RAW => Some(body.to_vec()),
        CM1 => Model::primed().decode(body, MAX_MESSAGE),
        _ => None,
    }
}

// -- record streams (links) --------------------------------------------------------

/// Header of a record of `len` (1..=65854) bytes of type `t`.
pub fn record_header(t: u8, len: usize, out: &mut Vec<u8>) {
    debug_assert!((1..=319 + 0xFFFF).contains(&len) && t < 4);
    let t = t << 6;
    if len <= 62 {
        out.push(t | (len - 1) as u8);
    } else if len <= 318 {
        out.extend([t | 62, (len - 63) as u8]);
    } else {
        let l = (len - 319) as u16;
        out.push(t | 63);
        out.extend(l.to_le_bytes());
    }
}

/// (type, header bytes, body length), if `b` holds a whole header.
fn parse_header(b: &[u8]) -> Option<(u8, usize, usize)> {
    let &h = b.first()?;
    let (t, l) = (h >> 6, (h & 63) as usize);
    match l {
        0..=61 => Some((t, 1, l + 1)),
        62 => Some((t, 2, 63 + *b.get(1)? as usize)),
        _ => Some((t, 3, 319 + u16::from_le_bytes([*b.get(1)?, *b.get(2)?]) as usize)),
    }
}

/// The sending end of a compressed link direction.
#[derive(Clone)]
pub struct StreamEncoder {
    model: Model,
}

impl Default for StreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder {
    pub fn new() -> Self {
        Self { model: Model::primed() }
    }

    /// Appends records carrying `data` to `out`. Each piece of up to
    /// [`CHUNK`] bytes goes [`CM1`] when that is shorter (the model learns
    /// it), else [`RAW`] (the model is left as it was).
    pub fn push(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for piece in data.chunks(CHUNK) {
            let mut m = self.model.clone();
            let body = m.encode(piece);
            if !body.is_empty() && body.len() < piece.len() {
                self.model = m;
                record_header(CM1, body.len(), out);
                out.extend(body);
            } else {
                record_header(RAW, piece.len(), out);
                out.extend_from_slice(piece);
            }
        }
    }
}

/// The receiving end: takes the stream as it arrives, hands out whole
/// records' data.
#[derive(Clone)]
pub struct StreamDecoder {
    model: Model,
    buf: Vec<u8>,
    /// Records that failed to decode (delivered as they were) or had an
    /// unknown type. Zero unless the two ends disagree.
    pub errors: u32,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            model: Model::primed(),
            buf: Vec::new(),
            errors: 0,
        }
    }

    /// Bytes received but not yet part of a whole record.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Takes stream bytes; appends the data of every record now whole.
    pub fn feed(&mut self, wire: &[u8], out: &mut Vec<u8>) {
        self.buf.extend_from_slice(wire);
        let mut at = 0;
        while let Some((t, h, len)) = parse_header(&self.buf[at..]) {
            if self.buf.len() < at + h + len {
                break;
            }
            let body = &self.buf[at + h..at + h + len];
            match t {
                RAW => out.extend_from_slice(body),
                CM1 => {
                    let mut m = self.model.clone();
                    match m.decode(body, CHUNK) {
                        Some(d) if !d.is_empty() => {
                            self.model = m;
                            out.extend(d);
                        }
                        _ => {
                            self.errors += 1;
                            out.extend_from_slice(body);
                        }
                    }
                }
                _ => {
                    self.errors += 1;
                    out.extend_from_slice(body);
                }
            }
            at += h + len;
        }
        self.buf.drain(..at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn stretch_inverts_squash() {
        let st = stretch_table();
        for x in (-1000..1000).step_by(37) {
            let p = squash(x);
            assert!(
                (st[p as usize] as i32 - x).abs() < 40,
                "{x} -> {p} -> {}",
                st[p as usize]
            );
        }
    }

    #[test]
    fn messages_round_trip() {
        for t in [
            &b""[..],
            b"7",
            b"73",
            b"N0ABC, Bob, Springfield, no traffic.",
            b"CQ CQ DE N0ABC N0ABC K",
            "Ünïcödé ✓ and the weather".as_bytes(),
            b"\x00\xff\x80 control bytes \r\n\t",
        ] {
            let p = pack(t);
            assert!(p.len() <= t.len() + 1);
            assert_eq!(unpack(&p).as_deref(), Some(t), "{t:?} -> {p:?}");
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unpack(&pack(&all)), Some(all.clone()));
        // Text compresses.
        let line = b"Shelter status 2000: 64 occupants, generator refueled, still need diapers.";
        assert!(pack(line).len() * 2 < line.len(), "{}", pack(line).len());
    }

    #[test]
    fn random_messages_round_trip_and_grow_by_one_byte_at_most() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        let pool = b"the you 73 QSL thanks for N0ABC\n,.\x00\xff";
        for i in 0..300 {
            let len = (rng(&mut s) % 200) as usize;
            let t: Vec<u8> = (0..len)
                .map(|_| {
                    let r = rng(&mut s);
                    if i % 3 == 0 {
                        r as u8
                    } else {
                        pool[(r % pool.len() as u64) as usize]
                    }
                })
                .collect();
            let p = pack(&t);
            assert!(p.len() <= t.len() + 1);
            assert_eq!(unpack(&p), Some(t));
        }
    }

    #[test]
    fn malformed_input_is_rejected_not_panicked() {
        assert_eq!(unpack(&[]), None);
        assert_eq!(unpack(&[7, 1, 2]), None);
        let mut s = 99u64;
        for _ in 0..200 {
            let len = (rng(&mut s) % 40) as usize;
            let mut b = vec![CM1];
            b.extend((0..len).map(|_| rng(&mut s) as u8));
            let _ = unpack(&b); // must not panic or hang
        }
    }

    #[test]
    fn record_headers_round_trip() {
        for len in [1, 2, 61, 62, 63, 64, 318, 319, 320, 1024, 65_854] {
            for t in [RAW, CM1] {
                let mut h = Vec::new();
                record_header(t, len, &mut h);
                let want = if len <= 62 {
                    1
                } else if len <= 318 {
                    2
                } else {
                    3
                };
                assert_eq!(h.len(), want, "{len}");
                assert_eq!(parse_header(&h), Some((t, want, len)));
                assert_eq!(parse_header(&h[..want - 1]), None);
            }
        }
    }

    #[test]
    fn streams_round_trip_fed_in_any_pieces() {
        let lines: Vec<&[u8]> = vec![
            b"Net control, N0CALL with a SKYWARN report.",
            b"",
            b"Quarter size hail at State Route 3 and Polaris Parkway at 1945 local.",
            &[0xFF, 0x00, 0x13, 0x37],
            b"Roger N0CALL, relaying to the weather service. 73",
        ];
        let mut big = Vec::new();
        let mut s = 5u64;
        for i in 0..5000 {
            big.push(if i % 2 == 0 {
                rng(&mut s) as u8
            } else {
                b"ion hf "[i % 7]
            });
        }
        let mut enc = StreamEncoder::new();
        let mut wire = Vec::new();
        let mut want = Vec::new();
        for l in &lines {
            enc.push(l, &mut wire);
            want.extend_from_slice(l);
        }
        enc.push(&big, &mut wire);
        want.extend_from_slice(&big);
        let text: String = PRIME.lines().take(40).collect::<Vec<_>>().join("\n");
        enc.push(text.as_bytes(), &mut wire);
        want.extend_from_slice(text.as_bytes());
        assert!(wire.len() < want.len());
        // Whole, byte by byte, and in random pieces.
        for step in [usize::MAX, 1, 7, 100] {
            let mut dec = StreamDecoder::new();
            let mut got = Vec::new();
            let mut at = 0;
            let mut s = 3u64;
            while at < wire.len() {
                let n = if step == 100 {
                    1 + (rng(&mut s) % 100) as usize
                } else {
                    step.min(wire.len() - at)
                };
                let n = n.min(wire.len() - at);
                dec.feed(&wire[at..at + n], &mut got);
                at += n;
            }
            assert_eq!(dec.errors, 0);
            assert_eq!(dec.buffered(), 0);
            assert_eq!(got, want, "step {step}");
        }
    }

    #[test]
    fn incompressible_data_costs_a_header_per_record() {
        let mut s = 77u64;
        let data: Vec<u8> = (0..3000).map(|_| rng(&mut s) as u8).collect();
        let mut enc = StreamEncoder::new();
        let mut wire = Vec::new();
        enc.push(&data[..50], &mut wire);
        assert_eq!(wire.len(), 51);
        wire.clear();
        enc.push(&data, &mut wire);
        // Three records (1024, 1024, 952 bytes), three header bytes each.
        assert_eq!(wire.len(), 3000 + 9);
    }
}
