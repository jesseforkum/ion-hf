//! Receiving side: data bursts in, ACKs out.

use super::{
    End, FLOOR_WAVE, Incoming, Outgoing, Role, STALL_S, Station, TINY, TINY_ALL, more_robust, user_hi, wave_down,
};
use crate::level::{FrameSize, Guard, LADDER, Wave};
use crate::link::frame::{AckInfo, Frame, Kind, TINY_ACK_FRAMES, Tiny, caps, flags};
use crate::link::harq;
use crate::link::report::ChanReport;
use crate::phy::REFINE_K;
use crate::phy_mfsk::MfskMode;

/// For a data burst in `data`, the shorter waveforms its tiny ACK may
/// go in, with the SNR (dB in 2.4 kHz) from which their header-only
/// bursts get through at least 90% of the time on CCIR poor.
/// Only waveforms the data could have used anyway: OFDM for OFDM data
/// (an MFSK level at a high SNR means OFDM is failing: Doppler), R1 for
/// R0 data.
pub fn cheap_ack_waves(data: Wave) -> &'static [(Wave, f32)] {
    match data {
        Wave::Ofdm(Guard::Slice) => &[(Wave::Ofdm(Guard::Normal), 2.0), (Wave::Ofdm(Guard::Long), 2.0)],
        Wave::Mfsk(MfskMode::R0) => &[(Wave::Mfsk(MfskMode::R1), -8.0)],
        _ => &[],
    }
}

/// Data bursts a poll turns `cheap_ack_margin_db` off for.
pub(super) const CHEAP_ACK_OFF_BURSTS: u8 = 8;

impl Station {
    pub(super) fn receiver_rx(&mut self, inc: &Incoming, tiny: Option<Tiny>, frames: &[Option<Frame>], now: f64) {
        self.note_rx_channel(inc);
        if inc.code < LADDER.len() {
            let c = inc.chan;
            self.stats.heard.push((
                inc.code,
                [c.delay_ms, c.doppler_hz, c.eff_snr_db, inc.snr_db],
                self.chan.class(),
            ));
            self.rx_level = inc.code;
            self.last_progress = now;
            let long = LADDER[inc.code].frame == FrameSize::Long;
            let combined;
            let frames = match &inc.soft {
                Some(soft) if long => {
                    // Only a cue carries the field (`Config::harq_partial`).
                    let field =
                        if self.caps & caps::CUE != 0 && matches!(inc.wave, Wave::Ofdm(Guard::Normal | Guard::Long)) {
                            inc.extra & 0x3F
                        } else {
                            0
                        };
                    let hq = user_hi(inc.user) >> 2;
                    let r = match &inc.grid {
                        Some(grid) if self.cfg.turbo > 0 => {
                            let passes = self.cfg.turbo.min(super::TURBO_SYMBOLS / grid.data_symbols().max(1));
                            self.harq
                                .receive_turbo(inc.code, hq, field, soft, passes, &mut |p| grid.refine(p, REFINE_K))
                        }
                        _ => self.harq.receive_partial(inc.code, hq, field, soft),
                    };
                    self.stats.harq_recovered += r.recovered as u32;
                    self.stats.turbo_recovered += r.turbo as u32;
                    self.stats.turbo_bursts += (r.turbo_passes > 0) as u32;
                    combined = r
                        .frames
                        .iter()
                        .map(|f| f.as_deref().and_then(Frame::decode))
                        .collect::<Vec<_>>();
                    // A codeword that decoded to a bad frame has left the
                    // store while the peer will think it is there: drop
                    // the lot and have everything missing sent fresh.
                    let bad = r
                        .frames
                        .iter()
                        .zip(&combined)
                        .any(|(c, f)| c.is_some() && !f.as_ref().is_some_and(|f| f.kind == Kind::Data));
                    if bad || !self.cfg.harq {
                        self.harq.clear();
                    }
                    &combined[..]
                }
                _ => {
                    self.harq.clear();
                    frames
                }
            };
            let mut bitmap = 0u64;
            for (i, f) in frames.iter().enumerate() {
                if let Some(f) = f
                    && f.kind == Kind::Data
                {
                    self.accept(f);
                    if i < 64 {
                        bitmap |= 1 << i;
                    }
                } else {
                    self.stats.codewords_failed += 1;
                }
            }
            if bitmap != 0 {
                self.last_moved = now;
            }
            let stored = !self.harq.is_empty();
            self.last_ack = (user_hi(inc.user) & 3, bitmap, frames.len(), stored);
            let mut ack = self.ack_burst(inc.wave);
            if ack.code == TINY {
                ack.wave = self.ack_wave(inc.wave, inc.snr_db);
            }
            ack.wave = self.capped_ack_wave(ack.wave, now);
            self.cheap_ack_off = self.cheap_ack_off.saturating_sub(1);
            if self.cheap_ack_off == 0 {
                self.ack_cap = None;
            }
            self.reply = Some(ack);
            return;
        }
        match tiny {
            Some(Tiny::Poll | Tiny::Idle) => {
                // A poll: our ACK was lost. Answer in the poll's waveform,
                // and keep the ACKs to it for a while (longer each time the
                // way back fails again).
                if tiny == Some(Tiny::Poll) {
                    if self.cheap_ack_off == 0 {
                        self.ack_cap_bursts = self.ack_cap_bursts.saturating_mul(2).min(64);
                    }
                    self.cheap_ack_off = self.ack_cap_bursts;
                    // The next ACK a rung more robust than the lost one, and
                    // no faster than the poll (on a lopsided link the poll
                    // may come much faster).
                    let down = more_robust(wave_down(self.last_tx_wave), inc.wave);
                    self.ack_cap = Some(self.ack_cap.map_or(down, |c| more_robust(c, down)));
                } else {
                    self.last_progress = now;
                    self.last_moved = now;
                }
                let wave = self.capped_ack_wave(inc.wave, now);
                self.reply = Some(self.ack_burst(wave));
            }
            Some(Tiny::Turn { snr_db: report }) => {
                self.last_progress = now;
                self.last_moved = now;
                // Our turn to send: start where the peer was getting
                // through, capped by what we hear. With nothing queued,
                // check in at once so the peer sees we took over.
                self.role = Role::Sender;
                self.harq.clear();
                self.at_peer.clear();
                self.at_peer_rest.clear();
                self.peer_heard = None;
                // The channel is the same both ways: our own class, and
                // the lower of the Turn's SNR and the last data burst's.
                self.peer_class = self.chan.class();
                self.peer_doppler_hz = self.chan.spreads().1;
                let rep = self.report_snr();
                let snr = match self.chan.eff_snr_db() {
                    Some(e) if self.cfg.eff_snr => e.min(rep),
                    _ => rep,
                };
                // The peer's report of how it hears us is our own direction;
                // without one, the channel is taken to be the same both ways.
                let snr = report.unwrap_or(snr);
                self.aim_db = Some(snr - self.air_db(self.rx_level));
                self.target = self.level_for(snr - self.air_db(self.rx_level));
                let start = match report {
                    Some(r) => self.level_for(r - self.air_db(self.rx_level) - self.cfg.start_margin_db.unwrap_or(0.0)),
                    None => self.rx_level,
                };
                self.level = self.usable_below(start.min(self.target).min(self.cfg.max_level), now);
                self.peer_wants_turn = false;
                self.heard_peer_db = None;
                // The first burst after a turn is never longer than
                // `max_burst_s`: the peer's TurnTaken wait counts on it.
                self.burst_s = self.cfg.max_burst_s;
                self.clear_wait();
                self.last_tx_end = f64::NEG_INFINITY;
            }
            Some(Tiny::DiscReq) => {
                self.reply = Some(self.tiny(Tiny::DiscAck, inc.wave));
                self.close_after_tx = true;
                self.close_end = End::Peer;
            }
            _ => {}
        }
        // Our ConnAck was lost: the caller asks again.
        if frames.iter().flatten().any(|f| f.kind == Kind::ConnReq) {
            self.reply = Some(self.conn_ack(inc));
        }
    }

    /// Stores a data frame and delivers whatever is now contiguous.
    fn accept(&mut self, f: &Frame) {
        self.accept_bytes(f.field, &f.payload);
    }

    /// Stores `payload` found at 24-bit stream offset `field` and delivers
    /// whatever is now contiguous.
    pub(super) fn accept_bytes(&mut self, field: u32, payload: &[u8]) {
        // 24-bit offset on the air → full offset near `delivered`.
        let d = field.wrapping_sub(self.delivered) & 0xFF_FFFF;
        let off = if d < 0x80_0000 {
            self.delivered.wrapping_add(d)
        } else {
            self.delivered.wrapping_sub(0x100_0000 - d)
        };
        let end = off.wrapping_add(payload.len() as u32);
        if end.wrapping_sub(self.delivered) as i32 <= 0 {
            return; // already delivered
        }
        let skip = self.delivered.wrapping_sub(off) as i32;
        let (off, data) = if skip > 0 {
            (self.delivered, &payload[skip as usize..])
        } else {
            (off, payload)
        };
        let keep = self.rx_pending.get(&off).is_none_or(|v| v.len() < data.len());
        if keep {
            self.rx_pending.insert(off, data.to_vec());
        }
        // Deliver from `delivered` while the pieces join up.
        loop {
            let next = self
                .rx_pending
                .range(..=self.delivered)
                .next_back()
                .map(|(&k, v)| (k, v.len()));
            let Some((k, len)) = next else { break };
            let end = k + len as u32;
            if end > self.delivered {
                let v = &self.rx_pending[&k];
                let from = (self.delivered - k) as usize;
                let before = self.inbox.len();
                match &mut self.rx_codec {
                    Some(c) => {
                        c.feed(&v[from..], &mut self.inbox);
                        self.stats.codec_errors = c.errors;
                    }
                    None => self.inbox.extend_from_slice(&v[from..]),
                }
                self.stats.text_received += (self.inbox.len() - before) as u64;
                self.stats.bytes_received += (len - from) as u64;
                self.delivered = end;
            }
            // Drop everything that is now fully delivered.
            let done: Vec<u32> = self
                .rx_pending
                .range(..=self.delivered)
                .filter(|(k, v)| **k + v.len() as u32 <= self.delivered)
                .map(|(&k, _)| k)
                .collect();
            if done.is_empty() {
                break;
            }
            for k in done {
                self.rx_pending.remove(&k);
            }
        }
    }

    /// Receiving side: notes a burst's channel measurement.
    pub(super) fn note_rx_channel(&mut self, inc: &Incoming) {
        self.rx_slot = inc.slot;
        self.last_rx_snr = inc.snr_db;
        self.last_rx_eff = if inc.chan.eff_snr_db.is_finite() {
            inc.chan.eff_snr_db
        } else {
            inc.snr_db
        };
        self.chan.add(&inc.chan);
        if std::env::var("ION_TRACE_CHAN").is_ok_and(|v| v == "1") {
            eprintln!(
                "chan {:?} snr {:.1} delay {:.2} doppler {:.2} raw {:?} -> {:?}",
                inc.wave,
                inc.snr_db,
                inc.chan.delay_ms,
                inc.chan.doppler_hz,
                inc.chan.raw,
                self.chan.class()
            );
        }
    }

    /// The SNR our ACKs report: effective or mean.
    pub(super) fn report_snr(&self) -> f32 {
        if self.cfg.eff_snr {
            self.last_rx_eff
        } else {
            self.last_rx_snr
        }
    }

    /// Waveform of a tiny ACK for a data burst in `data` heard at
    /// `snr_db`: the shortest of `data` and [`cheap_ack_waves`] the SNR
    /// allows (`Config::cheap_ack_margin_db`). Never longer than `data`'s
    /// own, so the sender's wait for it still holds.
    fn ack_wave(&self, data: Wave, snr_db: f32) -> Wave {
        let Some(margin) = self.cfg.cheap_ack_margin_db else {
            return data;
        };
        if self.cheap_ack_off > 0 {
            return data;
        }
        let len = |w: Wave| self.air.duration(w, TINY, 0);
        cheap_ack_waves(data)
            .iter()
            .filter(|&&(w, th)| snr_db >= th + margin && len(w) < len(data))
            .map(|&(w, _)| w)
            .min_by(|&a, &b| len(a).total_cmp(&len(b)))
            .unwrap_or(data)
    }

    /// `w` no faster than the cap a poll set, and the floor once the link
    /// has stalled.
    pub(super) fn capped_ack_wave(&self, w: Wave, now: f64) -> Wave {
        if now - self.last_progress > STALL_S {
            return FLOOR_WAVE;
        }
        match self.ack_cap.filter(|_| self.cheap_ack_off > 0) {
            Some(cap) => more_robust(w, cap),
            None => w,
        }
    }

    pub(super) fn ack_burst(&self, wave: Wave) -> Outgoing {
        let (burst, bitmap, n, stored) = self.last_ack;
        let want_turn = self.has_data();
        let want_disc = self.want_disc;
        let all = n <= harq::MAX_SLOTS && bitmap == u64::MAX >> (64 - n.max(1));
        let all_tiny = self.cfg.tiny_ack_all && n > TINY_ACK_FRAMES && all && !stored;
        if n <= TINY_ACK_FRAMES || all_tiny {
            return self.tiny(
                Tiny::Ack {
                    burst,
                    bitmap: if all_tiny { TINY_ALL as u8 } else { bitmap as u8 },
                    snr_db: self.report_snr(),
                    want_turn,
                    want_disc,
                    stored,
                    class: self.chan.class(),
                    slot: self.rx_slot,
                },
                wave,
            );
        }
        let mut f = Frame::new(Kind::Ack, self.session as u32);
        f.payload = AckInfo {
            burst,
            bitmap,
            snr_db: self.last_rx_snr,
            delivered: self.delivered & 0xFF_FFFF,
            report: Some({
                let (delay_ms, doppler_hz) = self.chan.spreads();
                ChanReport {
                    class: self.chan.class(),
                    eff_snr_db: self.last_rx_eff,
                    delay_ms,
                    doppler_hz,
                    slot: self.rx_slot,
                }
            }),
        }
        .to_bytes();
        if want_turn {
            f.flags |= flags::WANT_TURN;
        }
        if want_disc {
            f.flags |= flags::WANT_DISC;
        }
        if stored {
            f.flags |= flags::STORED;
        }
        self.control(f, wave)
    }

    /// Our answer to a ConnReq: the SNR we heard it at (effective, with the
    /// channel report), the channel class, and whether we compress.
    pub(super) fn conn_ack(&self, inc: &Incoming) -> Outgoing {
        let t = Tiny::ConnAck {
            snr_db: self.report_snr(),
            compress: self.compressing(),
            class: self.chan.class(),
            slot: inc.slot,
            caps: self.caps,
        };
        self.tiny(t, inc.wave)
    }
}
