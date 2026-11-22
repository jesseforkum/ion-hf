//! Gear shifting: the sending side's choice of level and burst length
//! from the peer's ACKs.

use super::config::Gear;
use super::{Sent, Station, TINY, caps, cue_s, frame_bits};
use crate::fec;
use crate::level::{FLOOR, FrameSize, Guard, LADDER, Wave};

/// A whole burst lost with the peer reporting at least this much SNR above
/// the level's threshold is the channel's doing, not the SNR's.
const CHANNEL_FAIL_DB: f32 = 4.0;

/// Weight of the newest burst in a level's success rate.
const SUCCESS_WEIGHT: f32 = 0.3;

/// [`Gear::Goodput`]: a level's observed success fades back to the
/// report's prediction with this time constant, s.
const GOODPUT_MEMORY_S: f64 = 180.0;

/// [`Gear::Goodput`]: a level change must promise this much more
/// throughput.
const GOODPUT_SWITCH: f64 = 1.15;

/// [`Gear::Goodput`]: levels whose threshold is more than this far above
/// the report are not tried, dB.
const GOODPUT_REACH_DB: f32 = 2.0;

/// Echo a waveform takes in its stride, ms (MFSK: its long symbols; only
/// OFDM channel failures compare guards).
/// Longest data burst in the 500 Hz link's MFSK, seconds: 5 times
/// `max_burst_s`, as the waveform is 5 times slower (a burst's fixed part
/// alone is 6.7 s, and its ACK 5.6 s).
const MFSK500_BURST_S: f64 = 40.0;

fn guard_ms(w: Wave) -> f32 {
    match w {
        Wave::Mfsk(_) | Wave::Lifeline | Wave::Mfsk500 => 0.0,
        Wave::Ofdm(Guard::Long | Guard::Narrow) => 10.0,
        Wave::Ofdm(_) => 4.0,
    }
}

impl Station {
    pub(super) fn on_ack(&mut self, burst: u8, bitmap: u64, stored: bool, snr_db: f32, now: f64) {
        if self.class_passed != Some(self.peer_class) {
            self.class_passed = None;
        }
        let mut aim = self.aim_snr(snr_db - self.air_db(self.level));
        // One report does not take the link from the top to the floor:
        // the aim falls by so much an ACK at most (`Config::aim_fall_db`).
        if let (Some(fall), Some(before)) = (self.cfg.aim_fall_db, self.aim_db) {
            aim = aim.max(before - fall);
        }
        self.aim_db = Some(aim);
        if aim >= super::qrm::SEARCH_AIM_DB {
            self.good_aim_at = now;
        }
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
            self.level = self.usable_below(at.min(self.target).min(self.level_cap()), now);
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
        self.success_at[at] = now;
        if self.cfg.gear == Gear::Goodput {
            if loss > 0.99 {
                self.lost_run = self.lost_run.saturating_add(1);
                self.fails[at] = self.fails[at].saturating_add(1);
                self.stats.failed_bursts += 1;
                if matches!(LADDER[at].wave, Wave::Ofdm(_)) {
                    self.stats.failed_ofdm += 1;
                    if snr_db >= self.threshold(at) + CHANNEL_FAIL_DB {
                        self.channel_failed(at, now);
                    }
                }
                if self.lost_run >= 2 {
                    self.class_passed = None;
                }
            } else {
                self.lost_run = 0;
                if loss == 0.0 {
                    self.fails[at] = 0;
                    // `Config::class_soft`: this burst is the one the class
                    // was read from.
                    if self.class_against(at) {
                        self.class_passed = Some(self.peer_class);
                    }
                }
            }
            self.level = self.pick_by_goodput(aim, now);
            let step = self
                .cfg
                .loss_step_db
                .filter(|_| loss > 0.99 && self.cfg.pin_level.is_none());
            if let Some(step) = step {
                if self.lost_run == 1 {
                    // One burst lost: once more at its level, if the
                    // report still allows it.
                    if aim + self.air_db(at) >= self.threshold(at) + self.margin() && self.family_ok(at) {
                        self.level = at;
                    }
                } else {
                    // A ceiling under the level that failed, lower each
                    // further loss.
                    let ceiling = self.threshold(at) - step * (self.lost_run - 1) as f32;
                    let top = (self.floor()..=at)
                        .rev()
                        .find(|&l| self.family_ok(l) && self.threshold(l) <= ceiling)
                        .unwrap_or(self.floor());
                    self.level = self.level.min(top);
                }
            }
            self.adapt_burst(at, loss, snr_db);
            self.last_burst.clear();
            return;
        }
        if loss > 0.3 && self.worth_keeping(at, success, now) {
            // Lost in a fade, but this level still beats the next one down:
            // stay (HARQ completes what was missed), no hold.
            if loss > 0.99 {
                self.stats.failed_bursts += 1;
                if matches!(LADDER[at].wave, Wave::Ofdm(_)) {
                    self.stats.failed_ofdm += 1;
                }
            }
            self.level = self.usable_below(at.min(self.target).min(self.level_cap()), now);
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
        self.level = self.usable_below(next.min(self.target).min(self.level_cap()), now);
        self.adapt_burst(at, loss, snr_db);
        self.last_burst.clear();
    }

    /// [`Gear::Goodput`]: the level expected to move the most. A level's
    /// chance is a logistic in its distance from its threshold (0.88 at
    /// the threshold, where it loses 10% of frames, 0.12 a dB below: the
    /// codes' waterfall is that steep), blended with its observed success
    /// while that is recent.
    pub(super) fn pick_by_goodput(&self, aim: f32, now: f64) -> usize {
        let top = self.level_cap().min(LADDER.len() - 1);
        let score = |l: usize| {
            let room = aim + self.air_db(l) - self.threshold(l) - self.margin();
            let prior = 1.0 / (1.0 + (-(room + 0.5) / 0.25).exp());
            // Each further whole-burst loss doubles how long it is remembered.
            let memory = GOODPUT_MEMORY_S * 2f64.powi(self.fails[l].min(4) as i32);
            let w = (-(now - self.success_at[l]) / memory).exp() as f32;
            let p = match self.success[l] {
                Some(s) => w * s + (1.0 - w) * prior,
                None => prior,
            };
            p as f64 * self.level_rate(l)
        };
        let at = self.level;
        // A change must pay for itself: more so across frame sizes, which
        // re-cut what is in flight.
        let worth = |l: usize| {
            let bar = if l == at {
                1.0
            } else if LADDER[l].frame == LADDER[at].frame {
                GOODPUT_SWITCH
            } else {
                GOODPUT_SWITCH * GOODPUT_SWITCH
            };
            score(l) / bar
        };
        (self.floor()..=top)
            .filter(|&l| {
                l == at
                    || self.family_ok(l) && aim + self.air_db(l) >= self.threshold(l) + self.margin() - GOODPUT_REACH_DB
            })
            .max_by(|&a, &b| worth(a).total_cmp(&worth(b)))
            .map_or(self.floor(), |l| self.usable_below(l, f64::INFINITY))
    }

    /// [`Gear::Goodput`]: a whole burst at OFDM level `at` lost with SNR to
    /// spare. The channel broke the waveform (fast fading, an echo past
    /// the guard), so it counts as a lost burst at the other levels it
    /// breaks too: those in the same waveform, and the faster ones whose
    /// guard is no longer. Otherwise each would be found out by a burst
    /// of its own, and again each time its record fades.
    fn channel_failed(&mut self, at: usize, now: f64) {
        for (k, lv) in LADDER.iter().enumerate() {
            let w = lv.wave;
            let hit = matches!(w, Wave::Ofdm(_))
                && k != at
                && (w == LADDER[at].wave || k > at && guard_ms(w) <= guard_ms(LADDER[at].wave));
            if hit {
                self.success[k] = Some(self.success[k].map_or(0.0, |p| p * (1.0 - SUCCESS_WEIGHT)));
                self.success_at[k] = now;
                self.fails[k] = self.fails[k].saturating_add(1);
            }
        }
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
        let cue = self.cue_for(l);
        // A frame's air time, and the burst's fixed part (an MFSK burst
        // of one bit already sends a whole codeword).
        let one = self.data_s(l, frame_bits(l, 1), cue);
        let frame = self.data_s(l, frame_bits(l, 2), cue) - one;
        let fixed = one - frame;
        let max_s = if lv.wave == Wave::Mfsk500 {
            MFSK500_BURST_S
        } else {
            self.cfg.max_burst_s
        };
        let frames = ((max_s - fixed) / frame.max(1e-3)).floor().max(1.0);
        let burst = self.data_s(l, frame_bits(l, frames as usize), cue);
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
            && (self.floor()..=l).all(|k| {
                now >= self.blocked_until[k]
                    || (k != l && !self.hold_all[k] && guard_ms(LADDER[l].wave) > guard_ms(LADDER[k].wave))
            })
    }

    /// The highest usable level at or below `l` (the floor if none is).
    pub(super) fn usable_below(&self, l: usize, now: f64) -> usize {
        if let Some(p) = self.cfg.pin_level {
            return p;
        }
        (self.floor()..=l)
            .rev()
            .find(|&k| self.usable(k, now))
            .unwrap_or(self.floor())
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

    /// Whether a data burst at level `l` goes behind a cue: a full-band
    /// OFDM level, on a link that agreed to cues, while the SNR aimed by is
    /// below `cue_below_db` (where the OFDM header starts to miss bursts;
    /// above it the cue only costs air time).
    pub(super) fn cue_for(&self, l: usize) -> bool {
        self.caps & caps::CUE != 0
            && matches!(LADDER[l].wave, Wave::Ofdm(Guard::Normal | Guard::Long))
            && self.aim_db.is_none_or(|a| a < self.cfg.cue_below_db)
    }

    /// Seconds of a data burst at level `l` of `bits` coded bits, behind
    /// a cue or not.
    pub(super) fn data_s(&self, l: usize, bits: usize, cue: bool) -> f64 {
        let c = if cue { cue_s() } else { 0.0 };
        self.air.duration(LADDER[l].wave, l, bits) + c
    }

    /// Mean power of level `l` on the air against the carrier, dB, as
    /// the gear shift counts it (0 without `power_aware`).
    ///
    /// A 500 Hz link always counts it: its MFSK is at the carriers' level
    /// and its OFDM 3 to 7 dB under, so a report from the one would aim
    /// the other that much too high.
    pub(super) fn air_db(&self, l: usize) -> f32 {
        if self.cfg.power_aware || self.link500 {
            let table = if crate::phy::td_clip_on() {
                &crate::level::AIR_MEAN_TD_DB
            } else {
                &crate::level::AIR_MEAN_DB
            };
            table.get(l).copied().unwrap_or(0.0)
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
        (0..=self.level_cap().min(th.len() - 1))
            .rev()
            .find(|&l| self.family_ok(l) && snr_db + self.air_db(l) >= th[l] + margin)
            .unwrap_or(self.floor())
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
        let allowed = || (self.floor()..at).rev().filter(|&l| self.family_ok(l));
        let to = allowed().nth(n - 1).unwrap_or(self.floor());
        let lowest_long = allowed().rfind(|&l| LADDER[l].frame == FrameSize::Long);
        match lowest_long {
            Some(l) if LADDER[at].frame == FrameSize::Long => to.max(l),
            _ => to,
        }
    }

    fn margin(&self) -> f32 {
        if self.cfg.eff_snr {
            self.cfg.eff_margin_db
        } else {
            self.cfg.margin_db
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
    ///
    /// And whether it is one of this link's ladder at all: a 500 Hz link
    /// uses the narrow levels and no others, a 2.45 kHz link never those
    /// (they have nothing below them to fall back to when they fail, so
    /// the class does not rule them out).
    fn family_ok(&self, l: usize) -> bool {
        if crate::level::is_narrow(l) != self.link500 {
            return false;
        }
        if !self.cfg.pick_family || self.link500 {
            return true;
        }
        // `Config::class_soft`: not while a burst in a waveform the class
        // rules out has come through whole under that same class (a
        // filter's edge or a receiver's AGC reads as an echo or as fading
        // on a path that has neither).
        !self.class_against(l) || self.cfg.class_soft && self.class_passed == Some(self.peer_class)
    }

    /// The peer's channel class speaks against level `l`'s waveform.
    fn class_against(&self, l: usize) -> bool {
        let c = self.peer_class;
        match LADDER[l].wave {
            Wave::Mfsk(_) | Wave::Lifeline | Wave::Mfsk500 => false,
            Wave::Ofdm(Guard::Long) => c.doppler,
            Wave::Ofdm(_) => c.doppler || c.echo,
        }
    }

    /// Longest data burst at `level` now: `max_burst_s`, or with
    /// `burst_adapt` what the run of clean bursts has earned, up to
    /// [`Self::clean_burst_s`].
    pub(super) fn burst_len_s(&self, level: usize) -> f64 {
        if LADDER[level].wave == Wave::Mfsk500 {
            return MFSK500_BURST_S;
        }
        let s = if self.cfg.burst_adapt {
            self.burst_s.min(self.clean_burst_s(level))
        } else {
            self.cfg.max_burst_s
        };
        self.narrow_cap_s(s)
    }

    /// The burst length at which a level's fixed cost per round trip
    /// (preamble and header, the tiny ACK in its waveform, two
    /// turnarounds) is `burst_overhead` of the cycle, between
    /// `max_burst_s` and `max_burst_long_s`. Long for the slow levels,
    /// whose 8 s bursts hold one or two frames; about `max_burst_s` from
    /// the long-guard levels up.
    pub fn clean_burst_s(&self, level: usize) -> f64 {
        let w = LADDER[level].wave;
        let pre = self.data_s(level, 1, self.cue_for(level));
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
        let top = self.level_cap().min(self.cfg.thresholds_db.len() - 1);
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
