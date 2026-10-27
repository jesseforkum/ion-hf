//! The lifeline: below the floor, both ends hold the link in R1/7
//! header-only bursts, report how they hear each other, and resume or
//! say goodbye there (`docs/link.md`, Lifeline).

use super::{Awaiting, End, Incoming, Outgoing, Role, State, Station, TINY, user_bits};
use crate::level::{FLOOR, FLOOR_WAVE, LADDER, Wave};
use crate::link::frame::{AckInfo, Frame, Kind, Life, Tiny};

/// A link holding below the floor.
#[derive(Clone, Copy, Debug)]
pub(super) struct Hold {
    since: f64,
    /// We probe (the sending side when the hold began); else we answer.
    probing: bool,
    /// When our next probe goes, and the gap to the one after it (doubling
    /// up to `life_probe_max_s`).
    next_probe: f64,
    gap: f64,
    /// When we next knock: a header-only R0 poll between probes, which a
    /// path that has come back answers at once.
    next_knock: f64,
    /// Waiting for the answer to our probe or knock until then.
    reply_by: Option<f64>,
    /// How we last heard the peer in this hold, and how it last said it
    /// hears us: mean SNR, dB in 2.4 kHz.
    heard_db: Option<f32>,
    peer_db: Option<f32>,
}

/// A hold that begins this soon after a resume counts as a flap: each
/// raises the SNR the next resume wants by [`FLAP_DB`] (up to three).
const FLAP_S: f64 = 300.0;
const FLAP_DB: f32 = 2.0;

impl Station {
    /// Whether the link is holding below the floor.
    pub fn holding(&self) -> bool {
        self.hold.is_some()
    }

    /// While holding: how we last heard the peer and how it last said it
    /// hears us (mean SNR, dB in 2.4 kHz).
    pub fn hold_report(&self) -> Option<(Option<f32>, Option<f32>)> {
        self.hold.map(|h| (h.heard_db, h.peer_db))
    }

    /// The SNR both ways that resumes a hold: the floor's threshold plus
    /// `life_resume_db`, more after flaps.
    pub(super) fn resume_db(&self) -> f32 {
        self.cfg.thresholds_db[FLOOR] + self.cfg.life_resume_db + FLAP_DB * self.flaps as f32
    }

    pub(super) fn life(&self, m: Life) -> Outgoing {
        let (kind, extra) = m.encode();
        Outgoing {
            wave: Wave::Lifeline,
            code: TINY,
            user: user_bits(TINY, self.session, kind),
            extra,
            frames: Vec::new(),
            spans: Vec::new(),
        }
    }

    /// No data has moved for `life_enter_s`: hold. The sending side
    /// probes, the other answers. What is in flight stays queued; the
    /// HARQ store is given up (the receiving side drops its own at the
    /// first probe).
    pub(super) fn enter_hold(&mut self, now: f64, probing: bool) {
        self.flaps = if now - self.resumed_at < FLAP_S {
            (self.flaps + 1).min(3)
        } else {
            self.hold_from = now;
            0
        };
        self.clear_wait();
        self.reply = None;
        self.last_burst.clear();
        self.at_peer.clear();
        self.stats.holds += 1;
        self.hold = Some(Hold {
            since: now,
            probing,
            next_probe: now,
            gap: self.cfg.life_probe_s,
            next_knock: now,
            reply_by: None,
            heard_db: None,
            peer_db: None,
        });
    }

    /// Ends a hold at `now`, if there is one, counting its time.
    pub(super) fn end_hold(&mut self, now: f64) {
        if let Some(h) = self.hold.take() {
            self.stats.hold_s += (now - h.since).max(0.0);
        }
    }

    /// The hold is over (`report`: how the peer hears us, from which the
    /// sending side starts its gear afresh).
    pub(super) fn leave_hold(&mut self, now: f64, report: Option<f32>) {
        if self.hold.is_none() {
            return;
        }
        self.end_hold(now);
        self.last_progress = now;
        self.last_moved = now;
        self.resumed_at = now;
        self.cheap_ack_off = 0;
        self.ack_cap = None;
        self.clear_wait();
        let Some(snr) = report else {
            return;
        };
        self.stats.resumes += 1;
        // A new channel: what the old one taught the gear no longer holds.
        self.fails = [0; LADDER.len()];
        self.success = [None; LADDER.len()];
        self.blocked_until = [f64::NEG_INFINITY; LADDER.len()];
        self.snr_reports.clear();
        self.last_report_db = None;
        self.burst_s = self.cfg.max_burst_s;
        self.peer_ack_wave = None;
        let snr = snr - self.air_db(FLOOR);
        self.target = self.level_for(snr);
        let start = self.level_for(snr - self.cfg.start_margin_db.unwrap_or(0.0));
        self.level = self.usable_below(start.min(self.target).min(self.cfg.max_level), now);
        // Straight on: data, or a check-in.
        self.last_tx_end = f64::NEG_INFINITY;
    }

    /// A burst of ours in a ladder waveform, heard while holding: whether
    /// it goes on to the usual handling (else it has been dealt with). The
    /// answering side leaves the hold at any, and answers a knock with its
    /// ACK (not as a poll for a lost one, which would hold its ACKs back
    /// after the resume). The sending side leaves for a goodbye, or for an
    /// ACK (the answer to a knock) heard at resuming SNR that reports the
    /// same of us; the peer starts its gear from that report.
    pub(super) fn hold_heard(
        &mut self,
        inc: &Incoming,
        tiny: Option<Tiny>,
        frames: &[Option<Frame>],
        now: f64,
    ) -> bool {
        let Some(h) = self.hold else {
            return true;
        };
        if !h.probing {
            self.leave_hold(now, None);
            if tiny == Some(Tiny::Poll) {
                self.note_rx_channel(inc);
                self.reply = Some(self.ack_burst(inc.wave));
                return false;
            }
            return true;
        }
        if tiny == Some(Tiny::DiscReq) {
            self.leave_hold(now, Some(inc.snr_db));
            return true;
        }
        let reported = match tiny {
            Some(Tiny::Ack { snr_db, .. }) => Some(snr_db),
            _ => frames
                .iter()
                .flatten()
                .find(|f| f.kind == Kind::Ack)
                .and_then(|f| AckInfo::from_bytes(&f.payload))
                .map(|a| match a.report {
                    Some(r) if self.cfg.eff_snr => r.eff_snr_db,
                    _ => a.snr_db,
                }),
        };
        // The ACK's report is in the gear's units (effective SNR, or mean).
        let floor = if self.cfg.eff_snr {
            self.cfg.eff_thresholds_db[FLOOR]
        } else {
            self.cfg.thresholds_db[FLOOR]
        };
        let want = floor + self.resume_db() - self.cfg.thresholds_db[FLOOR];
        match reported {
            Some(r) if r >= want && inc.snr_db >= self.resume_db() => {
                self.leave_hold(now, Some(r));
                true
            }
            _ => false,
        }
    }

    /// What to send while holding.
    pub(super) fn hold_tx(&mut self, now: f64) -> Option<Outgoing> {
        let h = self.hold?;
        let quiet = if h.probing {
            self.cfg.life_silent_s
        } else {
            self.cfg.life_silent_s + self.cfg.life_probe_max_s
        };
        if now - self.last_heard >= quiet {
            self.end_hold(now);
            self.finish(State::Failed, End::LinkLost);
            return None;
        }
        if !h.probing {
            return None;
        }
        if let Some(t) = h.reply_by {
            if now < t {
                return None;
            }
            // No answer to that probe.
            self.hold.as_mut()?.reply_by = None;
        }
        if now - self.hold_from >= self.cfg.life_max_s {
            self.bye(End::Weak { confirmed: true });
            return self.poll_tx(now);
        }
        if now < h.next_probe {
            // A knock, if there is time for its answer before the probe;
            // none after a flap (resumes then wait for the lifeline's
            // report, with its margin).
            let knock = self.cfg.life_knock_s.filter(|_| self.flaps == 0)?;
            let answer = self.answer_s(FLOOR_WAVE) + self.cfg.turnaround_s + self.cfg.reply_slack_s;
            if now < h.next_knock || h.next_probe - now < answer + 1.0 {
                return None;
            }
            self.hold.as_mut()?.next_knock = now + knock;
            self.stats.knocks += 1;
            let b = self.tiny(Tiny::Poll, FLOOR_WAVE);
            return Some(self.note_tx(b));
        }
        let probe = self.life(Life::Probe { snr_db: h.heard_db });
        let mut next = now + h.gap + self.probe_jitter_s();
        // Each ID time: a probe right after it, so the ID (which goes ahead
        // of the first burst after it, both ends) is never late.
        if let (Some(t0), Some((every, _))) = (self.link_t0, self.ids) {
            let id_at = t0 + (((now - t0) / every).floor() + 1.0) * every;
            next = next.min(id_at + 0.5);
        }
        let hm = self.hold.as_mut()?;
        hm.next_probe = next;
        hm.next_knock = now + self.cfg.life_knock_s.unwrap_or(0.0);
        hm.gap = (hm.gap * 2.0).min(self.cfg.life_probe_max_s);
        self.stats.probes += 1;
        Some(self.note_tx(probe))
    }

    /// 0–4.9 s, from our call: two stations that both think they should
    /// probe drift apart instead of meeting on every probe.
    fn probe_jitter_s(&self) -> f64 {
        let h = self
            .cfg
            .call
            .bytes()
            .fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
        (h % 50) as f64 / 10.0
    }

    /// Our probe or knock has gone out at `t`: wait for its answer.
    pub(super) fn probe_sent(&mut self, t: f64) {
        let mut wait = self.cfg.turnaround_s + self.answer_s(self.last_tx_wave) + self.cfg.reply_slack_s;
        wait += self.peer_id_s(t, wait);
        if let Some(h) = self.hold.as_mut().filter(|h| h.probing) {
            h.reply_by = Some(t + wait);
        }
    }

    /// Goodbye in the lifeline (`why`: what our end will say once the peer
    /// has acknowledged it).
    pub(super) fn bye(&mut self, why: End) {
        self.disc_why = why;
        self.want_disc = true;
        self.disc_by = None;
        self.state = State::Disconnecting;
        let b = self.life(Life::Bye {
            weak: matches!(why, End::Weak { .. }),
        });
        self.awaiting = Awaiting::DiscAck;
        self.retry = Some(b);
        self.tries = 0;
        self.deadline = Some(f64::NEG_INFINITY);
    }

    /// A lifeline burst of our session.
    pub(super) fn lifeline_rx(&mut self, inc: &Incoming, now: f64) {
        let Some(m) = Life::decode(super::user_hi(inc.user), inc.extra) else {
            return;
        };
        match m {
            Life::Bye { weak } => {
                self.end_hold(now);
                self.clear_wait();
                self.reply = Some(self.life(Life::ByeAck));
                self.close_after_tx = true;
                self.close_end = if weak { End::Weak { confirmed: true } } else { End::Peer };
                if self.state == State::Disconnecting {
                    // Both ends leaving: ours is confirmed too.
                    self.close_end = self.disc_why;
                }
                self.state = State::Disconnecting;
            }
            Life::ByeAck if self.awaiting == Awaiting::DiscAck => {
                self.end_hold(now);
                self.clear_wait();
                self.finish(State::Disconnected, self.disc_why);
            }
            Life::Probe { snr_db } if self.state == State::Connected => {
                // The peer probes: it holds as the sending side, and we
                // answer (if we were probing too, we stop).
                if self.hold.is_none_or(|h| h.probing) {
                    if self.hold.is_none() {
                        self.enter_hold(now, false);
                    }
                    if let Some(h) = self.hold.as_mut() {
                        h.probing = false;
                    }
                    self.role = Role::Receiver;
                    self.harq.clear();
                    self.last_ack = (0, 0, 0, false);
                }
                let h = self.hold.as_mut().expect("holding");
                h.heard_db = Some(inc.snr_db);
                h.peer_db = snr_db.or(h.peer_db);
                self.reply = Some(self.life(Life::Report { snr_db: inc.snr_db }));
            }
            Life::Report { snr_db } if self.state == State::Connected => {
                let Some(h) = self.hold.as_mut().filter(|h| h.probing) else {
                    return;
                };
                h.reply_by = None;
                h.heard_db = Some(inc.snr_db);
                h.peer_db = Some(snr_db);
                let (heard, peer) = (inc.snr_db, snr_db);
                let want = self.resume_db();
                if heard >= want && peer >= want {
                    self.leave_hold(now, Some(peer));
                } else if heard.min(peer) >= want - 3.0 {
                    // Close: look again soon.
                    let h = self.hold.as_mut().expect("holding");
                    h.gap = self.cfg.life_probe_s;
                    h.next_probe = h.next_probe.min(now + self.cfg.life_probe_s);
                }
            }
            _ => {}
        }
    }
}
