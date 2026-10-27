//! A primed NCS ([`NetConfig::prime`]): when and with which header it
//! expects bursts, a priori LLRs for frames that do not decode on their
//! own, and chorus check-in slots.

use super::*;

/// Most roster calls tried as priors on one check-in.
const MAX_ROSTER_PRIORS: usize = 30;

/// A burst NCS expects (see [`NetConfig::prime`]): where its preamble
/// should start on the station's clock, how far off it may be, and the
/// headers it may carry (MFSK only).
#[derive(Clone, Copy, Debug)]
pub struct NetExpect {
    pub start_s: f64,
    pub tol_s: f64,
    pub header: Option<ExpectedHeader>,
    pub alt: Option<ExpectedHeader>,
}

/// The chorus format of a net's chorus check-in slots: 5 sub-bands of 8
/// tones (2.46 s).
pub const CHORUS_FMT: crate::phy_chorus::ChorusFmt = crate::phy_chorus::ChorusFmt::T8;

/// One chorus slot: the burst, turnarounds either side and a little slack.
pub(super) fn chorus_slot_s(turnaround_s: f64) -> f64 {
    CHORUS_FMT.burst_s() as f64 + 2.0 * turnaround_s + SLOT_SLACK_S
}

/// NCS's chorus slots in the window now open (see
/// [`NetConfig::chorus_slots`]), on the station's clock.
#[derive(Clone, Copy, Debug)]
pub struct ChorusPlan {
    pub fmt: crate::phy_chorus::ChorusFmt,
    pub first_s: f64,
    pub len_s: f64,
    pub count: u8,
    pub tol_s: f64,
    pub net: u8,
}

/// What a primed NCS has seen, for tests and tuning.
#[derive(Clone, Debug, Default)]
pub struct PrimeStats {
    /// Start of every decoded check-in and polled reply against where NCS
    /// predicted it, s: (check-in?, before calibration, after the
    /// calibration the receiver was given).
    pub offsets: Vec<(bool, f32, f32)>,
    /// Frames that decoded only with a prior: check-ins, reports.
    pub prior_checkins: u32,
    pub prior_reports: u32,
    /// Check-ins taken from chorus slots.
    pub chorus_checkins: u32,
}

impl NetStation {
    /// NCS, primed: a poll (or a grant of another burst) is going out,
    /// ending at `end`: its member's answer should start a turnaround, a
    /// receiver's latency and a lead-in after it. At the floor its header
    /// is known too, but for a bit or two: an Idle with how well it hears
    /// us, or one short frame with the "more" flag (and, unless the poll
    /// named it, the burst number).
    pub(super) fn expect_reply(&mut self, b: &Outgoing, end: f64) {
        self.reply_exp = None;
        let kind = user_hi(b.user);
        if !self.cfg.prime || b.code != TINY || !matches!(kind, K_POLL | K_POLLACK | K_RELAY) {
            return;
        }
        let Some(Wait::Reply { slot, .. }) = self.wait else {
            return;
        };
        let Some(m) = self.members.get(&slot) else { return };
        let start = end + self.cfg.turnaround_s + self.lead_s + self.cfg.rx_latency_s;
        let has_kids = self.members.values().any(|x| x.via == Some(slot));
        let mut headers = (None, None);
        if self.cap_for(m) == FLOOR && !has_kids {
            let (idle, data) = floor_reply_headers(self.id, slot, (kind == K_POLL).then_some(m.burst));
            headers = (Some(idle), Some(data));
        }
        self.reply_exp = Some((start, slot, headers.0, headers.1));
    }

    /// NCS, primed: the burst its receiver should look for now, if any.
    pub fn expectation(&self, now: f64) -> Option<NetExpect> {
        if !self.cfg.ncs || !self.cfg.prime || self.closed {
            return None;
        }
        let tol_s = self.cfg.slot_tol_s;
        if let Some((s0, len, n)) = self.slots
            && now < self.window_end
        {
            // The slot whose burst may still be arriving: held well past
            // its header (2.3 s at R0), switched long before the next.
            let hold = (0.5 * len).min(4.0);
            let cal = self.checkin_cal.unwrap_or(0.0);
            let k = (0..n).find(|&k| now < s0 + cal + k as f64 * len + hold)?;
            let header = match LADDER[self.cfg.beacon_level].wave {
                Wave::Mfsk(mode) => Some(ExpectedHeader::exact(
                    mode,
                    checkin_user(self.id),
                    0,
                    frame_bits(CTRL_CODE, 1),
                )),
                Wave::Ofdm(_) | Wave::Lifeline => None,
            };
            return Some(NetExpect {
                start_s: s0 + cal + k as f64 * len,
                tol_s,
                header,
                alt: None,
            });
        }
        let (start, slot, header, alt) = self.reply_exp?;
        let cal = self
            .members
            .get(&slot)
            .and_then(|m| m.reply_cal)
            .or(self.reply_cal)
            .unwrap_or(0.0);
        (now < start + cal + 4.0).then_some(NetExpect {
            start_s: start + cal,
            tol_s,
            header,
            alt,
        })
    }

    /// NCS, primed: a priori LLRs to try, in order, on a short-frame burst
    /// with PHY header `user` whose frame did not decode on its own.
    pub fn priors_for(&self, user: u16) -> Vec<Vec<f32>> {
        use crate::link::apriori::{Assume, checkin_prior, report_prior};
        if !self.cfg.ncs || !self.cfg.prime || user_session(user) != self.id || user & 0xF != air_code(CTRL_CODE) {
            return Vec::new();
        }
        let now = self.last_now;
        if user == checkin_user(self.id) {
            if now >= self.window_end {
                return Vec::new();
            }
            let mut calls: Vec<&str> = self.members.values().map(|m| m.call.as_str()).collect();
            for r in &self.cfg.regulars {
                if !calls.contains(&r.as_str()) {
                    calls.push(r);
                }
            }
            let mut out = vec![checkin_prior(Assume::Format)];
            out.extend(
                calls
                    .into_iter()
                    .take(MAX_ROSTER_PRIORS)
                    .map(|c| checkin_prior(Assume::Call(c))),
            );
            return out;
        }
        match self.wait {
            Some(Wait::Reply { slot, .. }) if user_hi(user) & ASIDE == 0 => {
                (0..=4).map(|h| report_prior(slot, h)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// NCS: the chorus slots of the window now open, if any.
    pub fn chorus_plan(&self) -> Option<ChorusPlan> {
        let (first, len, count) = self.chorus?;
        (self.cfg.ncs && self.last_now < self.window_end + 10.0).then_some(ChorusPlan {
            fmt: CHORUS_FMT,
            first_s: first,
            len_s: len,
            count,
            tol_s: self.cfg.slot_tol_s.max(0.05),
            net: self.id,
        })
    }

    /// NCS: what the chorus slots held (`busy`: something was sent there,
    /// decoded or not).
    pub fn on_chorus(&mut self, heard: &[crate::phy_chorus::Heard], busy: bool, now: f64) {
        self.window_activity |= busy || !heard.is_empty();
        for h in heard {
            let c = &h.checkin;
            let Some(s) = self.admit(&c.call, None, now) else {
                continue;
            };
            if let Some(m) = self.members.get_mut(&s) {
                m.snr_up = Some(smooth(m.snr_up, h.snr_db));
                m.snr_down = Some(c.snr_ncs_db as f32);
                m.last_heard = Some(now);
                m.caps = Some(Caps::from_byte(c.caps));
                m.version = Some(c.version);
            }
            self.prime_stats.chorus_checkins += 1;
        }
    }

    /// Member: our chorus check-in, when it is time to send it: sub-band
    /// and message. Like [`NetStation::poll_tx`], call
    /// [`NetStation::on_tx_done`] when it has been sent.
    pub fn poll_chorus(&mut self, now: f64) -> Option<(usize, crate::phy_chorus::ChorusCheckin)> {
        let (at, sub) = self.chorus_at?;
        if self.on_air || now < at {
            return None;
        }
        self.chorus_at = None;
        if self.my_slot.is_some() || self.closed {
            return None;
        }
        self.on_air = true;
        self.stats.bursts += 1;
        self.stats.airtime_s += CHORUS_FMT.burst_s() as f64 + self.lead_s;
        let msg = crate::phy_chorus::ChorusCheckin {
            call: self.cfg.call.clone(),
            snr_ncs_db: self.snr_ncs.unwrap_or(0.0).round().clamp(-24.0, 39.0) as i8,
            caps: self.cfg.caps.to_byte(),
            version: self.cfg.version,
        };
        Some((sub, msg))
    }

    /// NCS, primed: a frame decoded only with a prior.
    pub fn note_prior_decode(&mut self, checkin: bool) {
        if checkin {
            self.prime_stats.prior_checkins += 1;
        } else {
            self.prime_stats.prior_reports += 1;
        }
    }

    /// NCS, primed: learn how far from our prediction a burst started.
    pub(super) fn calibrate(&mut self, checkin: bool, slot: Option<u8>) {
        let Some((start, _)) = self.rx_air else { return };
        let predicted = if checkin {
            let Some((s0, len, n)) = self.slots else { return };
            let k = (((start - s0) / len).round().max(0.0) as u8).min(n.saturating_sub(1));
            s0 + k as f64 * len
        } else {
            match self.reply_exp {
                Some((p, s, ..)) if Some(s) == slot => p,
                _ => return,
            }
        };
        let off = start - predicted;
        // Far off: not the burst we predicted (a late or relayed answer).
        if off.abs() > 1.0 {
            return;
        }
        let cal = if checkin {
            self.checkin_cal
        } else {
            slot.and_then(|s| self.members.get(&s)?.reply_cal).or(self.reply_cal)
        };
        self.prime_stats
            .offsets
            .push((checkin, off as f32, (off - cal.unwrap_or(0.0)) as f32));
        let ewma = |c: Option<f64>| Some(c.map_or(off, |c| 0.7 * c + 0.3 * off));
        if checkin {
            self.checkin_cal = ewma(self.checkin_cal);
        } else {
            self.reply_cal = ewma(self.reply_cal);
            if let Some(m) = slot.and_then(|s| self.members.get_mut(&s)) {
                m.reply_cal = ewma(m.reply_cal);
            }
        }
    }
}
