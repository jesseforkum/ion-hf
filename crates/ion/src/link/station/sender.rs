//! Sending side: ACKs in, data bursts out.

use super::{
    Awaiting, Incoming, MAX_FRAMES, Outgoing, Sent, State, Station, TINY, TINY_ALL, WINDOW_BYTES, top_level_in,
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
                        caps,
                    },
                    Awaiting::ConnAck,
                ) => {
                    self.last_progress = now;
                    self.link_t0 = Some(now - self.air.duration(inc.wave, TINY, 0));
                    self.state = State::Connected;
                    self.clear_wait();
                    self.agree(self.cfg.compress && compress);
                    self.caps = self.own_caps() & caps;
                    self.aim_db = Some(snr_db - self.air_db(FLOOR));
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
                    self.on_ack(burst, bitmap, stored, snr_db, now);
                    self.clear_wait();
                }
                (Tiny::DiscAck, Awaiting::DiscAck) => {
                    self.clear_wait();
                    self.state = State::Disconnected;
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
                self.on_ack(a.burst, a.bitmap, stored, snr, now);
                self.clear_wait();
            }
        }
    }

    /// The peer asks for the turn (or no longer does).
    pub(super) fn note_turn_ask(&mut self, want: bool, now: f64) {
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
        let rest = std::mem::take(&mut self.at_peer_rest);
        if keep {
            self.at_peer
                .extend(rest.into_iter().filter(|(off, _)| self.inflight.contains_key(off)));
        }
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

    /// Joins contiguous pieces in flight, up to `max` bytes, leaving those
    /// the peer holds HARQ bits of (`held`) as they are. The receiver
    /// places pieces by offset, so a join of pieces it already has part of
    /// only overlaps.
    fn merge_inflight(&mut self, held: &[u32], max: usize) {
        let keys: Vec<u32> = self.inflight.keys().copied().collect();
        let mut i = 0;
        while i < keys.len() {
            let k = keys[i];
            let mut j = i + 1;
            if !held.contains(&k) {
                while let Some(&n) = keys.get(j) {
                    let len = self.inflight[&k].len();
                    if held.contains(&n) || k.wrapping_add(len as u32) != n || len + self.inflight[&n].len() > max {
                        break;
                    }
                    let tail = self.inflight.remove(&n).unwrap();
                    self.inflight.get_mut(&k).unwrap().extend(tail);
                    j += 1;
                }
            }
            i = j;
        }
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
        let cue = self.cue_for(level);
        // HARQ increments for what the peer holds go first: all of them or
        // none (then those frames go out fresh again), or behind a cue
        // those that fit (`harq_partial`). Only in the waveform they were
        // stored from: a change of guard means the echoes defeated the old
        // one, and its LLRs are the suspect ones.
        let mut parity: Vec<(u32, usize, usize, usize)> = Vec::new(); // offset, held, start, bits
        if lv.frame == FrameSize::Long && lv.wave == self.peer_wave {
            parity.extend(
                self.at_peer
                    .iter()
                    .map(|&(off, h)| (off, h, harq::start(h), harq::increment(h, slot))),
            );
        }
        let mut parity_bits: usize = parity.iter().map(|p| p.3).sum();
        let too_big = |n: usize, bits: usize, s: f64| n > cap || (n > 0 && self.data_s(level, bits, cue) > s);
        let mut rest = Vec::new();
        if cue && self.cfg.harq_partial {
            if too_big(parity.len(), parity_bits, max_s) {
                while !parity.is_empty()
                    && (parity.len() > harq::PARTIAL_MAX || too_big(parity.len(), parity_bits, max_s))
                {
                    parity_bits -= parity.pop().map_or(0, |p| p.3);
                }
                if !parity.is_empty() {
                    rest = self.at_peer[parity.len()..].to_vec();
                }
            }
        } else {
            let parity_s = if self.cfg.harq_stretch {
                max_s.max(self.cfg.max_burst_long_s)
            } else {
                max_s
            };
            if too_big(parity.len(), parity_bits, parity_s) {
                parity.clear();
                parity_bits = 0;
            }
        }
        if parity.is_empty() {
            self.at_peer.clear();
        }
        self.at_peer_rest = rest;
        // Codewords the peer will hold when this burst arrives, before its
        // fresh frames.
        let stored = parity.len() + self.at_peer_rest.len();
        let held: Vec<u32> = parity
            .iter()
            .map(|p| p.0)
            .chain(self.at_peer_rest.iter().map(|r| r.0))
            .collect();
        let mut room = 0;
        let max_bits = self.air.max_bits(lv.wave, level);
        while parity.len() + room < cap
            && stored + room < harq::MAX_SLOTS
            && self.data_s(level, parity_bits + (room + 1) * slot, cue) <= max_s
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
        if self.cfg.merge_pieces && lv.frame == FrameSize::Long {
            self.merge_inflight(&held, max_payload);
        }
        let mut pieces: Vec<u32> = self
            .inflight
            .keys()
            .copied()
            .filter(|k| !held.contains(k))
            .take(room)
            .collect();
        let resent = pieces.len();
        let mut in_flight: usize = self.inflight.values().map(Vec::len).sum();
        // While the queue waits for a stream, only what is in flight goes.
        let draining = self.stream_wanted(self.last_heard.max(self.last_tx_end));
        while pieces.len() < room && !self.out.is_empty() && in_flight < WINDOW_BYTES && !draining {
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
        self.stats.cued_bursts += cue as u32;
        self.stats.frames_sent += pieces.len() as u32;
        self.stats.frames_resent += resent as u32;
        self.stats.increments_sent += parity.len() as u32;
        self.stats.increment_bits += parity_bits as u64;
        self.stats.levels.push(level);
        self.stats.classes.push((self.peer_class, self.target));
        self.awaiting = Awaiting::Ack;
        self.tries = 0;
        self.deadline = None;
        let field = harq::partial_field(parity.len(), stored);
        let hq = harq::burst_check_bits(stored, field);
        // A slice burst goes in the peer's third (`audio::transmit` reads
        // it from `extra`); a cued burst's cue carries the parity field.
        let extra = if self.cfg.slice_slots && lv.wave == Wave::Ofdm(Guard::Slice) {
            self.peer_slot as u16
        } else {
            field
        };
        Outgoing {
            wave: lv.wave,
            code: level,
            user: user_bits(level, self.session, self.burst_id | hq << 2),
            extra,
            frames,
            spans,
            cue,
        }
    }
}
