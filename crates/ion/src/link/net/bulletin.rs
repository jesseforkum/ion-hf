//! Bulletins: NCS sends each block once, at a level the weakest member it
//! polls can decode, then repairs it with coded pieces ([`net_code`]).

use super::*;

/// One block of the bulletin at NCS.
#[derive(Debug)]
pub(super) struct BulTx {
    pub(super) start: usize,
    pub(super) len: usize,
    /// Pieces sent so far; the next is number `sent % 256`.
    pub(super) sent: u32,
}

/// One block of the bulletin coming in.
#[derive(Debug)]
pub(super) struct BulRx {
    pub(super) len: usize,
    pub(super) dec: Decoder,
    pub(super) data: Option<Vec<u8>>,
}

/// Pieces `first..first + n` (numbers mod 256) of a block, as frames of
/// `[block length u16][pieces]`, the frame's field holding block and first
/// number.
pub(super) fn bul_frames(block: u16, data: &[u8], first: u32, n: usize, per_frame: usize) -> Vec<Frame> {
    let src = net_code::sources(data);
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let m = (n - i).min(per_frame);
        let at = first + i as u32;
        let mut f = Frame::new(Kind::Data, (block as u32) << 8 | at & 0xFF);
        f.flags = T_DOWN << 6 | ALL;
        f.payload = (data.len() as u16).to_le_bytes().to_vec();
        for j in 0..m {
            f.payload.extend(net_code::encode(&src, (at + j as u32) as u8));
        }
        out.push(f);
        i += m;
    }
    out
}

/// Pieces a station lacks of a block of `k` sources, from the shortfall in
/// its report (0 or 255: all of them).
pub(super) fn lacking(short: u8, k: usize) -> usize {
    if short == 0 || short == 255 {
        k
    } else {
        (short as usize).min(k)
    }
}

/// Pieces that fit in one frame of `max` payload bytes.
pub(super) fn pieces_per_frame(max: usize) -> usize {
    (max - 2) / PIECE
}

impl NetStation {
    /// Pieces of block `b` to send this round: the sources not yet sent,
    /// then as many repairs as the member furthest short of it lacks.
    fn bul_need(&self, b: usize) -> usize {
        let blk = &self.bul_tx[b];
        let k = net_code::k_of(blk.len);
        if (blk.sent as usize) < k {
            return k - blk.sent as usize;
        }
        self.direct()
            .filter_map(|m| m.bul)
            .filter(|&(lb, _)| lb as usize == b)
            .map(|(_, short)| lacking(short, k))
            .max()
            .unwrap_or(0)
    }

    /// New bulletin text goes out in blocks of up to [`net_code::MAX_K`]
    /// pieces; each round, what the members furthest behind lack.
    pub(super) fn bulletin_burst(&mut self) -> Option<Outgoing> {
        self.direct().next()?;
        let mut sealed: usize = self.bul_tx.iter().map(|b| b.len).sum();
        while sealed < self.bulletin.len() {
            let len = (self.bulletin.len() - sealed).min(BLOCK_BYTES);
            self.bul_tx.push(BulTx {
                start: sealed,
                len,
                sent: 0,
            });
            sealed += len;
        }
        let level = self.direct().map(|m| self.down_level(m)).min().unwrap_or(FLOOR);
        let size = LADDER[level].frame_bytes();
        let per = pieces_per_frame(Frame::max_payload(size));
        let room = self.room(level, 64);
        let mut frames: Vec<Frame> = Vec::new();
        for b in 0..self.bul_tx.len() {
            let need = self.bul_need(b);
            let n = need.min((room - frames.len()) * per);
            if n == 0 {
                continue;
            }
            let blk = &self.bul_tx[b];
            let k = net_code::k_of(blk.len) as u32;
            let data = &self.bulletin[blk.start..blk.start + blk.len];
            frames.extend(bul_frames(b as u16, data, blk.sent, n, per));
            self.stats.bul_pieces += n as u32;
            self.stats.bul_repairs += (blk.sent..blk.sent + n as u32).filter(|i| i % 256 >= k).count() as u32;
            self.bul_tx[b].sent += n as u32;
            if frames.len() >= room {
                break;
            }
        }
        if frames.is_empty() {
            return None;
        }
        Some(Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, self.id, 0),
            extra: 0,
            frames: frames.iter().map(|f| f.encode(size)).collect(),
            spans: Vec::new(),
        })
    }

    /// Coded bulletin pieces: solve the block once enough are in, and pass
    /// whole blocks on in order.
    pub(super) fn bul_piece(&mut self, f: &Frame) {
        let p = &f.payload;
        if p.len() < 2 + PIECE {
            return;
        }
        let block = (f.field >> 8) as u16;
        let first = f.field as u8;
        let len = u16::from_le_bytes([p[0], p[1]]) as usize;
        if len == 0 || len > BLOCK_BYTES {
            return;
        }
        self.bul_known = self.bul_known.max(block + 1);
        let rx = self.bul_rx.entry(block).or_insert_with(|| BulRx {
            len,
            dec: Decoder::new(net_code::k_of(len)),
            data: None,
        });
        if rx.data.is_some() || rx.len != len {
            return;
        }
        for (i, c) in p[2..].as_chunks::<PIECE>().0.iter().enumerate() {
            rx.dec.add(first.wrapping_add(i as u8), c);
        }
        rx.data = rx.dec.data(len);
        while let Some(d) = self.bul_rx.get(&self.bul_next).and_then(|r| r.data.clone()) {
            let off = self.bul_in.delivered;
            self.bul_in.accept(off & 0xFF_FFFF, &d);
            self.bul_next += 1;
        }
        for data in self.bul_in.messages() {
            let from = self.ncs_call.clone().unwrap_or_default();
            self.events.push(NetEvent::Message {
                from,
                to: None,
                data,
                overheard: false,
            });
        }
    }

    /// The first bulletin block we lack, and how many pieces short of it we
    /// are (255: we know of it but have none; 0 with no such block: done).
    pub(super) fn bul_status(&self) -> (u16, u8) {
        let mut b = 0u16;
        while self.bul_rx.get(&b).is_some_and(|r| r.data.is_some()) {
            b += 1;
        }
        match self.bul_rx.get(&b) {
            Some(r) => (b, r.dec.short().min(254) as u8),
            None if b < self.bul_known => (b, 255),
            None => (b, 0),
        }
    }
}
