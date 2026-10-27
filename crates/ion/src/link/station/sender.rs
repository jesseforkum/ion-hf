//! Sending side: ACKs in, data bursts out.

use super::{
    Awaiting, End, Incoming, MAX_FRAMES, Outgoing, Sent, State, Station, TINY, TINY_ALL, WINDOW_BYTES, top_level_in,
    user_bits,
};
use crate::level::{FLOOR, FrameSize, Guard, LADDER, Wave};
use crate::link::frame::{AckInfo, Frame, Kind, TINY_ACK_FRAMES, Tiny, flags};
use crate::link::harq;

impl Station {
    pub(super) fn sender_rx(&mut self, inc: &Incoming, tiny: Option<Tiny>, frames: &[Option<Frame>], now: f64) {
        let eff = inc.chan.eff_snr_db;
        self.heard_peer_db = Some(if self.cfg.eff_snr && eff.is_finite() {
            eff
        } else {
            inc.snr_db
        });
        if let Some(t) = tiny {
            match (t, self.awaiting) {
                (
                    Tiny::ConnAck {
                        snr_db,
                        compress,
                        class,
                        slot,
                    },
                    Awaiting::ConnAck,
                ) => {
                    self.last_progress = now;
                    self.last_moved = now;
                    self.link_t0 = Some(now - self.air.duration(inc.wave, TINY, 0));
                    self.state = State::Connected;
                    self.clear_wait();
                    self.agree(self.cfg.compress && compress);
                    self.peer_class = class;
                    self.peer_slot = slot;
                    self.target = self.level_for(snr_db - self.air_db(FLOOR));
                    // Start where the report allows, less a margin, and
                    // step down on a loss (`Config::start_margin_db`).
                    self.level = match self.cfg.start_margin_db {
                        Some(m) => self.usable_below(
                            self.level_for(snr_db - self.air_db(FLOOR) - m)
                                .min(self.target)
                                .min(self.cfg.max_level),
                            now,
                        ),
                        None => FLOOR,
                    };
                }
                (
                    Tiny::Ack {
                        burst,
                        bitmap,
                        snr_db,
                        want_turn,
                        want_disc,
                        stored,
                        class,
                        slot,
                    },
                    Awaiting::Ack,
                ) => {
                    self.peer_wants_disc = want_disc;
                    self.peer_class = class;
                    self.peer_slot = slot;
                    // A tiny ACK with every bit set for a burst of more
                    // frames than it can list: all of them arrived
                    // (`Config::tiny_ack_all`).
                    let all = burst == self.burst_id
                        && self.last_burst.len() > TINY_ACK_FRAMES
                        && u64::from(bitmap) == TINY_ALL;
                    let bitmap = if all { u64::MAX } else { bitmap as u64 };
                    self.note_turn_ask(want_turn, now);
                    self.peer_heard = Some(burst);
                    self.peer_ack_wave = Some(inc.wave);
                    self.last_progress = now;
                    self.ack_in(burst, bitmap, stored, snr_db, now);
                    self.clear_wait();
                }
                (Tiny::DiscAck, Awaiting::DiscAck) => {
                    self.clear_wait();
                    self.finish(State::Disconnected, self.disc_why);
                }
                // Unasked: the peer has something to send, or wants out.
                (
                    Tiny::Ack {
                        burst,
                        want_turn,
                        want_disc,
                        ..
                    },
                    Awaiting::Nothing,
                ) => {
                    self.note_turn_ask(want_turn, now);
                    self.peer_wants_disc = want_disc;
                    self.peer_heard = Some(burst);
                }
                // They want to close too: acknowledge and finish.
                (Tiny::DiscReq, _) => {
                    self.clear_wait();
                    self.reply = Some(self.tiny(Tiny::DiscAck, inc.wave));
                    self.close_after_tx = true;
                    self.close_end = End::Peer;
                }
                // A Turn while we already send: the peer never heard us take
                // over. Our burst is lost; send again now, no faster than
                // the Turn reached us.
                (Tiny::Turn { snr_db }, _) => {
                    let report = snr_db.unwrap_or(inc.snr_db);
                    if !self.last_burst.is_empty() {
                        self.on_ack(self.burst_id.wrapping_add(1) & 3, 0, false, report, now);
                    }
                    self.level = self.level.min(top_level_in(inc.wave));
                    self.burst_s = self.cfg.max_burst_s;
                    self.clear_wait();
                    self.last_tx_end = f64::NEG_INFINITY;
                }
                _ => {}
            }
            return;
        }
        for f in frames.iter().flatten() {
            if f.kind == Kind::Ack
                && self.awaiting == Awaiting::Nothing
                && let Some(a) = AckInfo::from_bytes(&f.payload)
            {
                self.note_turn_ask(f.flags & flags::WANT_TURN != 0, now);
                self.peer_wants_disc = f.flags & flags::WANT_DISC != 0;
                self.peer_heard = Some(a.burst);
                continue;
            }
            if f.kind == Kind::Ack
                && self.awaiting == Awaiting::Ack
                && let Some(a) = AckInfo::from_bytes(&f.payload)
            {
                self.note_turn_ask(f.flags & flags::WANT_TURN != 0, now);
                self.peer_wants_disc = f.flags & flags::WANT_DISC != 0;
                let stored = f.flags & flags::STORED != 0;
                let snr = match a.report {
                    Some(r) if self.cfg.eff_snr => r.eff_snr_db,
                    _ => a.snr_db,
                };
                self.peer_class = a.report.map(|r| r.class).unwrap_or_default();
                self.peer_slot = a.report.map_or(0, |r| r.slot);
                self.peer_ack_wave = Some(inc.wave);
                self.peer_heard = Some(a.burst);
                self.last_progress = now;
                self.ack_in(a.burst, a.bitmap, stored, snr, now);
                self.clear_wait();
            }
        }
    }

    /// An ACK awaited: the gear's [`Station::on_ack`], and whether data
    /// moved (frames acknowledged, or an idle check-in answered).
    fn ack_in(&mut self, burst: u8, bitmap: u64, stored: bool, snr_db: f32, now: f64) {
        let (acked, idle) = (self.stats.bytes_acked, self.last_burst.is_empty());
        self.on_ack(burst, bitmap, stored, snr_db, now);
        if idle || self.stats.bytes_acked > acked {
            self.last_moved = now;
        }
    }

    /// The peer asks for the turn (or no longer does).
    fn note_turn_ask(&mut self, want: bool, now: f64) {
        if want && !self.peer_wants_turn {
            self.turn_asked_at = now;
        }
        self.peer_wants_turn = want;
    }

    /// Applies the peer's ACK for the burst just sent: delivered frames
    /// leave `inflight`, and `at_peer` becomes what the peer now stores.
    /// Returns, per slot, whether it was delivered.
    pub(super) fn settle_burst(&mut self, bitmap: u64, stored: bool) -> Vec<bool> {
        let at = self.level;
        let keep = stored && self.cfg.harq && LADDER[at].frame == FrameSize::Long;
        self.at_peer.clear();
        self.peer_wave = LADDER[at].wave;
        let mut ok = Vec::with_capacity(self.last_burst.len());
        for (i, s) in self.last_burst.iter().enumerate() {
            let got = bitmap >> i & 1 == 1;
            if got {
                if let Some(p) = self.inflight.remove(&s.off) {
                    self.stats.bytes_acked += p.len() as u64;
                }
            } else if keep && s.held < harq::H_MAX && self.inflight.contains_key(&s.off) {
                self.at_peer.push((s.off, s.held));
            }
            ok.push(got);
        }
        ok
    }

    pub(super) fn data_burst(&mut self) -> Outgoing {
        let level = self.level;
        let lv = LADDER[level];
        let size = lv.frame_bytes();
        let max_payload = Frame::max_payload(size);
        let slot = lv.coded_bits();
        let max_s = self.burst_len_s(level);
        let cap = match lv.frame {
            FrameSize::Short => TINY_ACK_FRAMES,
            FrameSize::Long => MAX_FRAMES,
        };
        // HARQ increments for everything the peer holds go first: all of
        // them or none (then those frames go out fresh again). Only in the
        // waveform they were stored from: a change of guard means the
        // echoes defeated the old one, and its LLRs are the suspect ones.
        let mut parity: Vec<(u32, usize, usize, usize)> = Vec::new(); // offset, held, start, bits
        if lv.frame == FrameSize::Long && lv.wave == self.peer_wave {
            parity.extend(
                self.at_peer
                    .iter()
                    .map(|&(off, h)| (off, h, harq::start(h), harq::increment(h, slot))),
            );
        }
        let mut parity_bits: usize = parity.iter().map(|p| p.3).sum();
        if parity.len() > cap || (!parity.is_empty() && self.air.duration(lv.wave, level, parity_bits) > max_s) {
            parity.clear();
            parity_bits = 0;
        }
        if parity.is_empty() {
            self.at_peer.clear();
        }
        let mut room = 0;
        let max_bits = self.air.max_bits(lv.wave, level);
        while parity.len() + room < cap
            && self.air.duration(lv.wave, level, parity_bits + (room + 1) * slot) <= max_s
            && parity_bits + (room + 1) * slot <= max_bits
        {
            room += 1;
        }
        if parity.is_empty() {
            room = room.max(1);
        }
        // Re-cut anything still in flight to this level's frame size.
        let big: Vec<u32> = self
            .inflight
            .iter()
            .filter(|(_, v)| v.len() > max_payload)
            .map(|(&k, _)| k)
            .collect();
        for k in big {
            let data = self.inflight.remove(&k).unwrap();
            for (i, chunk) in data.chunks(max_payload).enumerate() {
                self.inflight.insert(k + (i * max_payload) as u32, chunk.to_vec());
            }
        }
        let mut pieces: Vec<u32> = self
            .inflight
            .keys()
            .copied()
            .filter(|k| !parity.iter().any(|p| p.0 == *k))
            .take(room)
            .collect();
        let resent = pieces.len();
        let mut in_flight: usize = self.inflight.values().map(Vec::len).sum();
        while pieces.len() < room && !self.out.is_empty() && in_flight < WINDOW_BYTES {
            let n = self.out.len().min(max_payload);
            let chunk: Vec<u8> = self.out.drain(..n).collect();
            self.inflight.insert(self.next_offset, chunk);
            pieces.push(self.next_offset);
            self.next_offset = self.next_offset.wrapping_add(n as u32);
            in_flight += n;
        }
        self.burst_id = (self.burst_id + 1) & 3;
        if Some(self.burst_id) == self.peer_heard {
            self.burst_id = (self.burst_id + 1) & 3;
        }
        let n = parity.len() + pieces.len();
        let mut frames = Vec::with_capacity(n);
        let mut spans = Vec::with_capacity(n);
        self.last_burst.clear();
        let slots = parity
            .iter()
            .map(|&(off, h, from, bits)| (off, h + bits, from, bits))
            .chain(pieces.iter().map(|&off| (off, slot, 0, slot)));
        for (off, held, from, bits) in slots {
            // Within the long levels a piece never changes, so this is the
            // same codeword every time.
            let mut f = Frame::new(Kind::Data, off);
            f.payload = self.inflight[&off].clone();
            self.last_burst.push(Sent { off, bits, held });
            frames.push(f.encode(size));
            spans.push((from, bits));
        }
        self.stats.data_bursts += 1;
        self.stats.frames_sent += pieces.len() as u32;
        self.stats.frames_resent += resent as u32;
        self.stats.increments_sent += parity.len() as u32;
        self.stats.increment_bits += parity_bits as u64;
        self.stats.levels.push(level);
        self.stats.classes.push((self.peer_class, self.target));
        self.awaiting = Awaiting::Ack;
        self.tries = 0;
        self.deadline = None;
        let hq = harq::check_bits(parity.len());
        // A slice burst goes in the peer's third (`audio::transmit` reads
        // it from `extra`, which a data burst does not otherwise use).
        let slot = if self.cfg.slice_slots && lv.wave == Wave::Ofdm(Guard::Slice) {
            self.peer_slot
        } else {
            0
        };
        Outgoing {
            wave: lv.wave,
            code: level,
            user: user_bits(level, self.session, self.burst_id | hq << 2),
            extra: slot as u16,
            frames,
            spans,
        }
    }
}
