//! Gear shifting: the sending side's choice of level and burst length
//! from the peer's ACKs.

use super::{Sent, Station, TINY, frame_bits};
use crate::fec;
use crate::level::{FLOOR, FrameSize, Guard, LADDER, Wave};

/// A whole burst lost with the peer reporting at least this much SNR above
/// the level's threshold is the channel's doing, not the SNR's.
const CHANNEL_FAIL_DB: f32 = 4.0;

/// Weight of the newest burst in a level's success rate.
const SUCCESS_WEIGHT: f32 = 0.3;

/// Echo a waveform takes in its stride, ms (MFSK: its long symbols; only
/// OFDM channel failures compare guards).
fn guard_ms(w: Wave) -> f32 {
    match w {
        Wave::Mfsk(_) | Wave::Lifeline => 0.0,
        Wave::Ofdm(Guard::Long) => 10.0,
        Wave::Ofdm(_) => 4.0,
    }
}

impl Station {
    pub(super) fn on_ack(&mut self, burst: u8, bitmap: u64, stored: bool, snr_db: f32, now: f64) {
        let aim = self.aim_snr(snr_db - self.air_db(self.level));
        self.target = self.level_for(aim);
        if self.last_burst.is_empty() {
            return; // an Idle check-in
        }
        let at = self.level;
        // Loss is that of the fresh frames: how this level does on its
        // own. A failed increment says more about the earlier, stored
        // transmissions, so a burst of increments only that the peer heard
        // leaves the gear alone.
        let any_fresh = self.last_burst.iter().any(|s| s.held == s.bits);
        if !any_fresh && burst == self.burst_id {
            self.settle_burst(bitmap, stored);
            self.level = self.usable_below(at.min(self.target).min(self.cfg.max_level), now);
            if self.level != at {
                self.burst_s = self.cfg.max_burst_s;
            }
            self.last_burst.clear();
            return;
        }
        let counts = |s: &Sent| s.held == s.bits || !any_fresh;
        let sent = self.last_burst.iter().filter(|s| counts(s)).count();
        // An ACK for an older burst: the last one never arrived, and the
        // peer's HARQ store is as it was.
        let mut got = 0;
        if burst == self.burst_id {
            got = self
                .settle_burst(bitmap, stored)
                .iter()
                .zip(&self.last_burst)
                .filter(|(ok, s)| **ok && counts(s))
                .count();
        }
        let loss = 1.0 - got as f32 / sent as f32;
        let ok = 1.0 - loss;
        let success = match self.success[at] {
            Some(p) => p + SUCCESS_WEIGHT * (ok - p),
            None => ok,
        };
        self.success[at] = Some(success);
        if loss > 0.3 && self.worth_keeping(at, success, now) {
            // Lost in a fade, but this level still beats the next one down:
            // stay (HARQ completes what was missed), no hold.
            if loss > 0.99 {
                self.stats.failed_bursts += 1;
                if matches!(LADDER[at].wave, Wave::Ofdm(_)) {
                    self.stats.failed_ofdm += 1;
                }
            }
            self.level = self.usable_below(at.min(self.target).min(self.cfg.max_level), now);
            self.adapt_burst(at, loss, snr_db);
            self.last_burst.clear();
            return;
        }
        if loss > 0.99 {
            // Nothing got through: keep off this level, longer each time.
            // With SNR to spare the channel is to blame (echoes, fading
            // over the burst's width), and an OFDM waveform with a longer
            // guard may still work: the hold leaves it open. Each other
            // OFDM waveform held for the same reason doubles the hold, so
            // a channel that kills every OFDM level is not probed level by
            // level forever.
            self.fails[at] = self.fails[at].saturating_add(1);
            self.stats.failed_bursts += 1;
            if matches!(LADDER[at].wave, Wave::Ofdm(_)) {
                self.stats.failed_ofdm += 1;
            }
            let channel = matches!(LADDER[at].wave, Wave::Ofdm(_)) && snr_db >= self.threshold(at) + CHANNEL_FAIL_DB;
            let others = if channel {
                (FLOOR..LADDER.len())
                    .filter(|&k| {
                        k != at && now < self.blocked_until[k] && !self.hold_all[k] && LADDER[k].wave != LADDER[at].wave
                    })
                    .count() as i32
            } else {
                0
            };
            let hold =
                (self.cfg.fail_hold_s * 2f64.powi(self.fails[at] as i32 - 1 + others)).min(self.cfg.fail_hold_max_s);
            self.blocked_until[at] = self.blocked_until[at].max(now + hold);
            self.hold_all[at] = !channel;
        } else if loss > 0.3 {
            self.blocked_until[at] = self.blocked_until[at].max(now + self.cfg.loss_hold_s);
            self.hold_all[at] = true;
        } else {
            self.fails[at] = 0;
        }
        let next = if loss > 0.99 {
            self.step_down(at, 2)
        } else if loss > 0.3 {
            self.step_down(at, 1)
        } else if loss == 0.0 && at < self.target {
            // Halfway to the target while far below (two steps at least),
            // but one at a time into a level that has failed before (steps
            // over held-off levels).
            let one = self.climb_above(at, 1, now);
            let far = self.climb_above(at, (self.target - at).div_ceil(2).max(2), now);
            if self.target - at >= 3 && (one..=far).all(|k| self.fails[k] == 0) {
                far
            } else {
                one
            }
        } else {
            at
        };
        // The report rules out the waveform just used: straight to the
        // target, in a waveform it allows (rather than down the ladder and
        // up again through levels it rules out too).
        let next = if self.family_ok(at) { next } else { self.target };
        self.level = self.usable_below(next.min(self.target).min(self.cfg.max_level), now);
        self.adapt_burst(at, loss, snr_db);
        self.last_burst.clear();
    }

    /// The SNR to aim by: the report, or with `smooth_target` the lower of
    /// it and the median of the last three reports.
    fn aim_snr(&mut self, snr_db: f32) -> f32 {
        self.snr_reports.push_back(snr_db);
        while self.snr_reports.len() > 3 {
            self.snr_reports.pop_front();
        }
        if !self.cfg.smooth_target || self.snr_reports.len() < 3 {
            return snr_db;
        }
        let mut v: Vec<f32> = self.snr_reports.iter().copied().collect();
        v.sort_by(f32::total_cmp);
        snr_db.min(v[1])
    }

    /// Payload bits per second of air at level `l` in a burst of about
    /// `max_burst_s`: what a level is worth.
    fn level_rate(&self, l: usize) -> f64 {
        let lv = &LADDER[l];
        // A frame's air time, and the burst's fixed part (an MFSK burst
        // of one bit already sends a whole codeword).
        let one = self.air.duration(lv.wave, l, frame_bits(l, 1));
        let frame = self.air.duration(lv.wave, l, frame_bits(l, 2)) - one;
        let fixed = one - frame;
        let frames = ((self.cfg.max_burst_s - fixed) / frame.max(1e-3)).floor().max(1.0);
        let burst = self.air.duration(lv.wave, l, frame_bits(l, frames as usize));
        frames * (lv.frame_bytes() * 8) as f64 / burst.max(1e-3)
    }

    /// With `hold_by_goodput`: level `at`, whose recent success is
    /// `success`, still moves more than the level a loss would step down
    /// to.
    fn worth_keeping(&self, at: usize, success: f32, now: f64) -> bool {
        if !self.cfg.hold_by_goodput || at > self.target {
            return false;
        }
        let lower = self.usable_below(self.step_down(at, 1), now);
        lower < at && success as f64 * self.level_rate(at) > self.level_rate(lower)
    }

    /// Whether level `l` may be used at `now`. A held-off level rules out
    /// itself and the faster levels: all of them when it failed for want
    /// of SNR, but after a channel failure (SNR to spare) only those whose
    /// guard is no longer than its own. A slice level that a 7 ms echo
    /// breaks says nothing about the long-guard levels above it, which are
    /// built for that echo; a long-guard level that fails rules out every
    /// OFDM level above it.
    fn usable(&self, l: usize, now: f64) -> bool {
        self.family_ok(l)
            && (FLOOR..=l).all(|k| {
                now >= self.blocked_until[k]
                    || (k != l && !self.hold_all[k] && guard_ms(LADDER[l].wave) > guard_ms(LADDER[k].wave))
            })
    }

    /// The highest usable level at or below `l` (the floor if none is).
    pub(super) fn usable_below(&self, l: usize, now: f64) -> usize {
        (FLOOR..=l).rev().find(|&k| self.usable(k, now)).unwrap_or(FLOOR)
    }

    /// The `n`-th level to climb to above `at` (the last there is, or
    /// `at` if none): usable levels, and not the slice levels or the faster
    /// R1 rates when the SNR already allows a full-band level past the
    /// first long-guard one. Then they are only slow steps on the way up
    /// (on a short transfer, most of it), and on a long echo the slice
    /// fails where the long guard would not.
    fn climb_above(&self, at: usize, n: usize, now: f64) -> usize {
        let step = |k: usize| {
            LADDER[k].wave == Wave::Ofdm(Guard::Slice)
                || matches!(LADDER[k].wave, Wave::Mfsk(_)) && LADDER[k].rate != fec::Rate::R1_4
        };
        let skip_steps = self.target >= self.cfg.skip_slices_from && !step(self.target);
        (at + 1..LADDER.len())
            .filter(|&k| self.usable(k, now) && !(skip_steps && step(k)))
            .take(n)
            .last()
            .unwrap_or(at)
    }

    /// Mean power of level `l` on the air against the carrier, dB, as
    /// the gear shift counts it (0 without `power_aware`).
    pub(super) fn air_db(&self, l: usize) -> f32 {
        if self.cfg.power_aware {
            crate::level::AIR_MEAN_DB.get(l).copied().unwrap_or(0.0)
        } else {
            0.0
        }
    }

    /// The highest level an SNR allows (in a waveform the channel class
    /// allows). `snr_db` is as a steady carrier would read it: the report
    /// less the mean power of the level it was measured on ([`Self::air_db`]),
    /// so each level is aimed at with what it would see.
    pub(super) fn level_for(&self, snr_db: f32) -> usize {
        let (th, margin) = if self.cfg.eff_snr {
            (&self.cfg.eff_thresholds_db, self.cfg.eff_margin_db)
        } else {
            (&self.cfg.thresholds_db, self.cfg.margin_db)
        };
        (0..=self.cfg.max_level.min(th.len() - 1))
            .rev()
            .find(|&l| self.family_ok(l) && snr_db + self.air_db(l) >= th[l] + margin)
            .unwrap_or(FLOOR)
    }

    /// `n` levels below `at`, counting only those whose waveform the
    /// channel class allows (with the class ruling out the slice levels,
    /// one step down from long-guard QPSK 1/4 is MFSK R1, not a slice
    /// level that `usable_below` would then pass down to MFSK too). From
    /// a long-frame level it stops at the lowest allowed long-frame level
    /// below: the MFSK floor's short frames re-cut everything in flight
    /// into 22-byte pieces, which then ride one to a 180-byte frame for
    /// the rest of the transfer (without the class, two steps down from
    /// the long guard land on a slice level, which keeps long frames).
    fn step_down(&self, at: usize, n: usize) -> usize {
        let allowed = || (FLOOR..at).rev().filter(|&l| self.family_ok(l));
        let to = allowed().nth(n - 1).unwrap_or(FLOOR);
        let lowest_long = allowed().rfind(|&l| LADDER[l].frame == FrameSize::Long);
        match lowest_long {
            Some(l) if LADDER[at].frame == FrameSize::Long => to.max(l),
            _ => to,
        }
    }

    /// Gear-shift threshold of level `l` in the SNR the ACKs carry.
    fn threshold(&self, l: usize) -> f32 {
        if self.cfg.eff_snr {
            self.cfg.eff_thresholds_db[l]
        } else {
            self.cfg.thresholds_db[l]
        }
    }

    /// Whether the channel class allows level `l`'s waveform: long echoes
    /// leave the MFSK floor and the long guard, fast fading the MFSK floor.
    fn family_ok(&self, l: usize) -> bool {
        if !self.cfg.pick_family {
            return true;
        }
        let c = self.peer_class;
        match LADDER[l].wave {
            Wave::Mfsk(_) | Wave::Lifeline => true,
            Wave::Ofdm(Guard::Long) => !c.doppler,
            Wave::Ofdm(_) => !c.doppler && !c.echo,
        }
    }

    /// Longest data burst at `level` now: `max_burst_s`, or with
    /// `burst_adapt` what the run of clean bursts has earned, up to
    /// [`Self::clean_burst_s`].
    pub(super) fn burst_len_s(&self, level: usize) -> f64 {
        if !self.cfg.burst_adapt {
            return self.cfg.max_burst_s;
        }
        self.burst_s.min(self.clean_burst_s(level))
    }

    /// The burst length at which a level's fixed cost per round trip
    /// (preamble and header, the tiny ACK in its waveform, two
    /// turnarounds) is `burst_overhead` of the cycle, between
    /// `max_burst_s` and `max_burst_long_s`. Long for the slow levels,
    /// whose 8 s bursts hold one or two frames; about `max_burst_s` from
    /// the long-guard levels up.
    pub fn clean_burst_s(&self, level: usize) -> f64 {
        let w = LADDER[level].wave;
        let pre = self.air.duration(w, level, 1);
        let ack = self.air.duration(w, TINY, 0);
        let turn = 2.0 * self.cfg.turnaround_s;
        let lo = self.cfg.max_burst_s;
        ((pre + ack + turn) / self.cfg.burst_overhead - ack - turn).clamp(lo, self.cfg.max_burst_long_s.max(lo))
    }

    /// `burst_adapt`: after the ACK for a burst at `at` whose fresh frames
    /// lost `loss`, with the level now `self.level` and the peer reporting
    /// `snr_db`. A clean burst at a level that stays, on a steady channel
    /// (`burst_steady_db`; and, if set, not close to a faster level's
    /// entry, `burst_climb_db`), doubles the next one's budget; any loss,
    /// a new level or a jump in the SNR starts again from `max_burst_s`
    /// (never shorter: the gear shifting answers heavy loss by changing
    /// level).
    fn adapt_burst(&mut self, at: usize, loss: f32, snr_db: f32) {
        let steady = self
            .last_report_db
            .is_some_and(|p| (snr_db - p).abs() <= self.cfg.burst_steady_db);
        self.last_report_db = Some(snr_db);
        let top = self.cfg.max_level.min(self.cfg.thresholds_db.len() - 1);
        let next_entry = (at + 1..=top)
            .map(|k| self.cfg.thresholds_db[k] + self.cfg.margin_db)
            .fold(f32::INFINITY, f32::min);
        let far = self.cfg.burst_climb_db.is_none_or(|d| snr_db < next_entry - d);
        self.burst_s = if loss == 0.0 && self.level == at && steady && far {
            (self.burst_s * 2.0).min(self.cfg.max_burst_long_s.max(self.cfg.max_burst_s))
        } else {
            self.cfg.max_burst_s
        };
    }
}
