//! Objects by hash: opaque bytes NCS offers, named by their SHA-256, kept
//! by members in a store so one already held never costs airtime.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use super::*;

/// Most objects on offer at once (one bit each in a report).
pub const MAX_OFFERS: usize = 8;

/// Bulletin-stream block numbers from here on are object blocks:
/// `OBJ_BLOCK | offer << 5 | block`.
pub(super) const OBJ_BLOCK: u16 = 0x8000;

/// An object's name: the first 8 bytes of its SHA-256 and its length.
/// Shown (and stored) as 20 hex digits: the hash, then the length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    pub hash: [u8; 8],
    pub len: u16,
}

impl ObjectId {
    pub fn of(data: &[u8]) -> Self {
        let h = sha256(data);
        Self {
            hash: h[..8].try_into().unwrap(),
            len: data.len().min(u16::MAX as usize) as u16,
        }
    }

    /// Hash and length as they go on the air (10 bytes).
    pub(super) fn to_bytes(self) -> [u8; 10] {
        let mut b = [0u8; 10];
        b[..8].copy_from_slice(&self.hash);
        b[8..].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    pub(super) fn from_bytes(b: &[u8]) -> Self {
        Self {
            hash: b[..8].try_into().unwrap(),
            len: u16::from_le_bytes([b[8], b[9]]),
        }
    }

    /// Blocks of the bulletin code it takes.
    pub(super) fn blocks(self) -> usize {
        (self.len as usize).div_ceil(BLOCK_BYTES)
    }

    /// Length of its block `j`.
    pub(super) fn block_len(self, j: usize) -> usize {
        (self.len as usize - j * BLOCK_BYTES).min(BLOCK_BYTES)
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.hash {
            write!(f, "{b:02x}")?;
        }
        write!(f, "{:04x}", self.len)
    }
}

impl FromStr for ObjectId {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        if s.len() != 20 || !s.is_ascii() {
            return Err(());
        }
        let byte = |i: usize| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| ());
        let mut hash = [0u8; 8];
        for (i, h) in hash.iter_mut().enumerate() {
            *h = byte(i)?;
        }
        let len = u16::from_str_radix(&s[16..], 16).map_err(|_| ())?;
        Ok(Self { hash, len })
    }
}

/// Block `j` of offer `i` in the bulletin code's numbering.
pub(super) fn obj_block(i: usize, j: usize) -> u16 {
    OBJ_BLOCK | (i as u16) << 5 | j as u16
}

/// (offer, block) of an object block number.
pub(super) fn obj_of_block(b: u16) -> Option<(usize, usize)> {
    let i = ((b & !OBJ_BLOCK) >> 5) as usize;
    (b & OBJ_BLOCK != 0 && i < MAX_OFFERS).then_some((i, (b & 31) as usize))
}

/// The object `id` from the store, if its file is there and matches.
pub(super) fn load(dir: &Path, id: ObjectId) -> Option<Vec<u8>> {
    let data = std::fs::read(dir.join(id.to_string())).ok()?;
    (ObjectId::of(&data) == id).then_some(data)
}

pub(super) fn save(dir: &Path, id: ObjectId, data: &[u8]) {
    if std::fs::create_dir_all(dir).is_ok() {
        let _ = std::fs::write(dir.join(id.to_string()), data);
    }
}

/// The offer list as `C_OFFER` frames to `slot`: two entries a frame,
/// after the index of the first and (high nibble) the list's length.
pub(super) fn offer_frames(ids: &[Option<ObjectId>; MAX_OFFERS], slot: u8) -> Vec<Frame> {
    let total = ids.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let mut p = vec![C_OFFER, i as u8 | (total as u8) << 4];
        for id in ids.iter().take(total).skip(i).take(2) {
            p.extend(id.unwrap_or_default().to_bytes());
        }
        out.push(ctrl_frame(slot, p));
        i += 2;
        if i >= total {
            return out;
        }
    }
}

/// One object NCS offers.
#[derive(Debug)]
pub(super) struct Offer {
    pub(super) id: ObjectId,
    pub(super) data: Vec<u8>,
    /// Pieces of each block sent so far.
    pub(super) sent: Vec<u32>,
    /// Order of offering (the oldest goes when the list is full).
    pub(super) seq: u64,
}

impl NetStation {
    /// NCS: offer `i` changed: every member's holding of it is unknown
    /// again, and the list goes out.
    pub(super) fn offer_changed(&mut self, i: usize) {
        for m in self.members.values_mut() {
            if m.reported & 1 << i != 0 {
                m.stale |= 1 << i;
            }
            m.holds &= !(1 << i);
        }
        self.offers_due = true;
    }

    /// NCS: some member that has every bulletin block neither holds offer
    /// `i` nor is fetching it: it missed the list.
    fn unaware(&self) -> bool {
        let blocks = self.bul_tx.len();
        self.direct().filter(|m| m.missed == 0).any(|m| {
            m.bul.is_some_and(|(b, _)| b < OBJ_BLOCK && b as usize >= blocks)
                && (0..MAX_OFFERS).any(|i| self.offers[i].is_some() && m.holds >> i & 1 == 0)
        })
    }

    /// NCS: the offer list, when it changed, after a beacon, or while a
    /// member seems to have missed it; once a round at most.
    pub(super) fn offer_burst(&mut self) -> Option<Outgoing> {
        if self.offer_round == Some(self.round_no) || !(self.offers_due || self.unaware()) {
            return None;
        }
        self.direct().next()?;
        self.offer_round = Some(self.round_no);
        self.offers_due = false;
        let ids = self.offers();
        let level = self.direct().map(|m| self.down_level(m)).min().unwrap_or(FLOOR);
        Some(self.ctrl(offer_frames(&ids, ALL), LADDER[level].wave))
    }

    /// NCS: pieces to send this round of each offer some member it polls
    /// is fetching: (offer, block, pieces). All its sources once, then as
    /// many repairs as the member furthest short lacks; a member with none
    /// of a block lacks every block from there on.
    pub(super) fn obj_needs(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for (i, o) in self.offers.iter().enumerate() {
            let Some(o) = o else { continue };
            let fetching: Vec<(usize, u8)> = self
                .direct()
                .filter_map(|m| m.bul)
                .filter_map(|(b, short)| match obj_of_block(b) {
                    Some((oi, j)) if oi == i => Some((j, short)),
                    _ => None,
                })
                .collect();
            if fetching.is_empty() {
                continue;
            }
            for j in 0..o.id.blocks() {
                let k = net_code::k_of(o.id.block_len(j));
                let sent = o.sent[j] as usize;
                let n = if sent < k {
                    k - sent
                } else {
                    fetching
                        .iter()
                        .filter(|&&(fj, short)| fj == j || fj < j && short == 255)
                        .map(|&(_, short)| lacking(short, k))
                        .max()
                        .unwrap_or(0)
                };
                if n > 0 {
                    out.push((i, j, n));
                }
            }
        }
        out
    }

    /// Block `b` of the bulletin code, if we have all of it: a bulletin
    /// block, or a block of an object we hold or have decoded.
    pub(super) fn block_data(&self, b: u16) -> Option<Vec<u8>> {
        let Some((i, j)) = obj_of_block(b) else {
            return self.bul_rx.get(&b)?.data.clone();
        };
        let id = self.known_offers[i]?;
        if let Some(d) = self.objects.get(&id) {
            let at = j * BLOCK_BYTES;
            return (j < id.blocks()).then(|| d[at..at + id.block_len(j)].to_vec());
        }
        self.obj_rx.get(&b)?.data.clone()
    }

    /// Which of the offers we hold (bit i: offer i).
    pub(super) fn holds(&self) -> u8 {
        (0..MAX_OFFERS).fold(0, |a, i| {
            a | (self.known_offers[i].is_some_and(|id| self.objects.contains_key(&id)) as u8) << i
        })
    }

    /// Member: the first object block we lack of the offers we know, and
    /// how many pieces short of it we are (255: none of it).
    pub(super) fn obj_status(&self) -> Option<(u16, u8)> {
        for (i, id) in self.known_offers.iter().enumerate() {
            let Some(id) = *id else { continue };
            if self.objects.contains_key(&id) {
                continue;
            }
            for j in 0..id.blocks() {
                let b = obj_block(i, j);
                match self.obj_rx.get(&b) {
                    Some(r) if r.data.is_some() => {}
                    Some(r) => return Some((b, r.dec.short().min(254) as u8)),
                    None => return Some((b, 255)),
                }
            }
        }
        None
    }

    /// Member: we hold `id` (in memory or in the store). Hands it to the
    /// host the first time.
    fn find_held(&mut self, id: ObjectId) -> bool {
        if !self.objects.contains_key(&id) {
            let found = match &self.cfg.store {
                _ if id == ObjectId::of(&[]) => Some(Vec::new()),
                Some(dir) => load(dir, id),
                None => None,
            };
            let Some(data) = found else { return false };
            self.objects.insert(id, data);
        }
        if self.obj_given.insert(id) {
            let data = self.objects[&id].clone();
            self.events.push(NetEvent::Object { id, data, cached: true });
        }
        true
    }

    /// Member: a `C_OFFER` frame (from NCS, or our relay).
    pub(super) fn offer_frame(&mut self, p: &[u8]) {
        if p.len() < 2 {
            return;
        }
        let first = (p[1] & 15) as usize;
        let total = ((p[1] >> 4) as usize).min(MAX_OFFERS);
        let mut list = self.known_offers;
        for (k, e) in p[2..].as_chunks::<10>().0.iter().enumerate() {
            if first + k < total {
                let id = ObjectId::from_bytes(e);
                list[first + k] = (id != ObjectId::default()).then_some(id);
            }
        }
        for x in list.iter_mut().skip(total) {
            *x = None;
        }
        for (i, new) in list.into_iter().enumerate() {
            if new == self.known_offers[i] {
                continue;
            }
            self.known_offers[i] = new;
            self.offer_ver += 1;
            self.obj_rx
                .retain(|&b, _| obj_of_block(b).is_none_or(|(oi, _)| oi != i));
            if let Some(id) = new {
                self.find_held(id);
            }
        }
    }

    /// Member: coded pieces of an object block.
    pub(super) fn obj_piece(&mut self, f: &Frame, block: u16, len: usize) {
        let Some((i, j)) = obj_of_block(block) else { return };
        let Some(id) = self.known_offers[i] else { return };
        if self.objects.contains_key(&id) || j >= id.blocks() || len != id.block_len(j) {
            return;
        }
        let rx = self.obj_rx.entry(block).or_insert_with(|| BulRx {
            len,
            dec: Decoder::new(net_code::k_of(len)),
            data: None,
        });
        if rx.data.is_some() || rx.len != len {
            return;
        }
        let first = f.field as u8;
        for (k, c) in f.payload[2..].as_chunks::<PIECE>().0.iter().enumerate() {
            rx.dec.add(first.wrapping_add(k as u8), c);
        }
        rx.data = rx.dec.data(len);
        let blocks: Option<Vec<Vec<u8>>> = (0..id.blocks())
            .map(|j| self.obj_rx.get(&obj_block(i, j)).and_then(|r| r.data.clone()))
            .collect();
        let Some(blocks) = blocks else { return };
        self.obj_rx
            .retain(|&b, _| obj_of_block(b).is_none_or(|(oi, _)| oi != i));
        let data = blocks.concat();
        if ObjectId::of(&data) != id {
            // Not what was offered: start over.
            self.stats.obj_rejected += 1;
            return;
        }
        if let Some(dir) = &self.cfg.store {
            save(dir, id, &data);
        }
        self.objects.insert(id, data.clone());
        if self.obj_given.insert(id) {
            self.events.push(NetEvent::Object {
                id,
                data,
                cached: false,
            });
        }
    }
}

/// SHA-256 (FIPS 180-4).
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend((data.len() as u64 * 8).to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*c);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ w[i - 15] >> 3;
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ w[i - 2] >> 10;
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = e & f ^ !e & g;
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = a & b ^ a & c ^ b & c;
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (o, x) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = x.to_be_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn ids_print_and_parse() {
        let id = ObjectId::of(b"abc");
        assert_eq!(id.to_string(), "ba7816bf8f01cfea0003");
        assert_eq!("ba7816bf8f01cfea0003".parse(), Ok(id));
        assert_eq!(ObjectId::from_bytes(&id.to_bytes()), id);
        assert!("ba7816bf8f01cfea003".parse::<ObjectId>().is_err());
        assert_eq!(obj_of_block(obj_block(7, 25)), Some((7, 25)));
        assert_eq!(obj_of_block(12), None);
        let id = ObjectId::of(&vec![0; 6000]);
        assert_eq!((id.blocks(), id.block_len(2)), (3, 6000 - 2 * BLOCK_BYTES));
    }
}
