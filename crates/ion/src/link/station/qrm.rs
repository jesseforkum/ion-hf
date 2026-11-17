//! Narrowing for QRM: another station over part of our passband.
//!
//! The receiving side decides, since the QRM is where it listens. It
//! narrows when the slot meter names a clean third while another is dirty
//! *and* the link is in trouble there: data bursts that fail, polls (the
//! peer heard nothing back), or a fading class that turns up only with the
//! QRM (the QRM spoils the MFSK spreads and the SNR measured over the
//! whole band). The meter alone says where the QRM is, not whether it
//! hurts: weak QRM over a third leaves full-band levels well ahead of a
//! slice.
//!
//! Narrowed, it asks for slice bursts in the clean third, reports the
//! class and SNR it had before the QRM (then those of the slice bursts it
//! hears there), and tells the sender in its full ACKs
//! ([`ChanReport::narrow`]), which then keeps to the slice levels. It
//! widens again once a full-band burst would have had [`WIDEN_DB`] in the
//! other thirds for [`CLEAR_S`], and tries the full band after
//! [`TRY_WIDE_S`] (doubling while the QRM is still there) in case the QRM,
//! though there, no longer hurts. The meter's scores cannot judge the
//! other thirds while narrowed (our slice in the clean one makes the QRM
//! the passband's median): the slice bursts measure them against the noise
//! under our own ([`crate::slots::SlotMeter::qrm_db`]).

use super::{Incoming, Station};
use crate::level::{Guard, LADDER, Wave};
use crate::link::frame::caps;
use crate::link::harq::Soft;
use crate::link::report::ChanClass;

/// Meter score of a third that counts as clean, and as dirty
/// ([`crate::slots::SlotMeter::scores`]).
pub const CLEAN_SCORE: f32 = 4.0;
pub const DIRTY_SCORE: f32 = 10.0;
/// QRM: the cleanest third this far below another, and no worse than
/// [`CLEANEST_MAX`] (QRM next door raises a clean third's score a little).
pub const CONTRAST: f32 = 8.0;
pub const CLEANEST_MAX: f32 = 15.0;
/// Trouble events within [`TROUBLE_S`] that narrow the link.
pub const TROUBLE_EVENTS: usize = 2;
pub const TROUBLE_S: f64 = 90.0;
/// The clean effective S/N (dB) a link must have had for the watch to
/// narrow it.
pub const NARROW_MIN_DB: f32 = 6.0;
/// How long [`NARROW_MIN_DB`]'s S/N counts, s.
pub const SPARE_S: f64 = 300.0;
/// Trouble less than this long after the last counts with it, s.
pub const TROUBLE_GAP_S: f64 = 8.0;
/// A slice-wave poll after this long without a burst heard is the peer
/// searching ([`Station::search_poll`]), s.
pub const SEARCHED_S: f64 = 15.0;
/// Narrowed, a slice burst half lost: another third that had QRM
/// [`MOVE_CONTRAST_DB`] or more over the noise at the last slice burst heard
/// well, now with no more than [`MOVE_CLEAN_DB`] and that far under the
/// remaining third's, is where the QRM came from
/// ([`super::Config::qrm_quick`]).
pub const MOVE_CLEAN_DB: f32 = 0.0;
pub const MOVE_CONTRAST_DB: f32 = 6.0;
/// A link in trouble under QRM that never showed S/N to spare narrows on
/// trial ([`super::Config::qrm_trial`]): this long for a slice burst to
/// show it, then this long before the next trial (doubling up to
/// [`TRY_WIDE_MAX_S`]).
pub const TRIAL_S: f64 = 60.0;
pub const TRIAL_AGAIN_S: f64 = 300.0;
/// A trial wants a third this far over the cleanest on the meter: fading
/// alone at 0 to 6 dB S/N showed up to 12.6, QRM 15.9 or more
/// (docs/link.md, "Narrowing for QRM").
pub const TRIAL_CONTRAST: f32 = 14.0;
/// Data burst frames lost (share) that count as trouble.
pub const TROUBLE_LOSS: f32 = 0.5;
/// Narrowed: the S/N (dB in 2.4 kHz) a full-band burst would have in the
/// worst of the other thirds ([`wide_snr_db`]) that counts them clear.
pub const WIDEN_DB: f32 = -1.5;
/// The other thirds clear this long: widen.
pub const CLEAR_S: f64 = 30.0;
/// Narrowed this long with the QRM still there: try the full band again
/// (doubling each time it narrows again, up to [`TRY_WIDE_MAX_S`]).
pub const TRY_WIDE_S: f64 = 300.0;
pub const TRY_WIDE_MAX_S: f64 = 1200.0;

/// [`Station::search_poll`]: the peer reported at least this S/N (dB)
/// within the last [`SEARCH_S`].
pub const SEARCH_AIM_DB: f32 = 3.0;
pub const SEARCH_S: f64 = 300.0;

/// Full ACKs after a change of narrowing.
pub const NARROW_REPEAT: u8 = 3;

/// The highest slice level.
pub const SLICE_TOP: usize = {
    let mut i = LADDER.len() - 1;
    while !matches!(LADDER[i].wave, Wave::Ofdm(Guard::Slice)) {
        i -= 1;
    }
    i
};

/// Receiving side's watch.
#[derive(Clone, Debug, Default)]
pub(super) struct QrmWatch {
    /// Narrowed: the third, and since when.
    pub on: Option<(u8, f64)>,
    /// While narrowed, since when the other thirds have been clean.
    clear_since: Option<f64>,
    /// Trouble seen while the meter showed QRM.
    trouble: Vec<f64>,
    /// The last clean report: effective SNR and class.
    pub clean: Option<(f32, ChanClass)>,
    /// While narrowed, the SNR of the last slice burst heard in our third.
    pub slice_snr: Option<f32>,
    /// While narrowed, the QRM in each third at the last slice burst heard
    /// well in ours.
    others: Option<[f32; 3]>,
    /// How long the next narrowing lasts before a try at the full band.
    try_wide_s: f64,
    /// When we last heard a burst (with a meter reading).
    last_rx: f64,
    /// The best clean effective S/N lately, and when.
    best_clean: Option<(f32, f64)>,
    /// Narrowed on trial, since when.
    pub trial: Option<f64>,
    /// No trial before this time, and the wait after the next that fails.
    trial_after: f64,
    trial_wait_s: f64,
}

/// Narrowed in third `s`: the S/N (dB in 2.4 kHz) a full-band burst would
/// have in the worst of the other thirds, from a slice burst of the peer's
/// in ours: its S/N less the QRM there. `None` from any other burst.
pub(super) fn wide_snr_db(inc: &Incoming, s: u8) -> Option<f32> {
    if inc.third != Some(s) {
        return None;
    }
    let worst = (0..3)
        .filter(|&j| j != s as usize)
        .map(|j| inc.qrm_db[j])
        .try_fold(f32::MIN, |m, q| q.is_finite().then(|| m.max(q)))?;
    Some(inc.snr_db - 10.0 * (1.0 + 10f32.powf(worst / 10.0)).log10())
}

/// The cleanest third, and whether another is dirty, from meter scores.
pub(super) fn contrast(t: &[f32; 3]) -> Option<(u8, bool)> {
    if t.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let k = (0..3).min_by(|&a, &b| t[a].total_cmp(&t[b]))?;
    let dirty = (0..3).any(|j| j != k && t[j] >= DIRTY_SCORE && t[j] - t[k] >= CONTRAST);
    Some((k as u8, t[k] <= CLEANEST_MAX && dirty))
}

impl Station {
    /// Narrowed for QRM, or held narrow by hand ([`super::Config::narrow`]).
    pub(super) fn narrowed(&self) -> bool {
        self.cfg.narrow || self.qrm.on.is_some()
    }

    /// Receiving side: a burst heard; `loss`: the share of a data burst's
    /// frames that failed (`None`: not a data burst); `poll`: the peer
    /// polled us.
    pub(super) fn qrm_watch(&mut self, inc: &Incoming, loss: Option<f32>, poll: bool, now: f64) {
        if !self.cfg.qrm_narrow && !self.cfg.narrow {
            return;
        }
        let reading = contrast(&inc.thirds);
        let class = self.chan.class();
        let last_rx = std::mem::replace(&mut self.qrm.last_rx, now);
        // The clean S/N, whether or not the meter has enough to go on yet.
        let clean = loss.is_some_and(|l| l < TROUBLE_LOSS) && class == ChanClass::default();
        if self.qrm.on.is_none() && clean && !reading.is_some_and(|r| r.1) {
            self.qrm.clean = Some((self.last_rx_eff, class));
            // The best of the last few minutes: the first burst the QRM
            // hits may still decode, at an S/N it spoilt.
            let e = self.last_rx_eff;
            if self.qrm.best_clean.is_none_or(|(b, t)| e >= b || now - t > SPARE_S) {
                self.qrm.best_clean = Some((e, now));
            }
        }
        let Some((k, qrm)) = reading else {
            return;
        };
        if self.cfg.narrow && self.qrm.on.is_none() {
            self.qrm.on = Some((self.cfg.slot_pick.unwrap_or(k), now));
            self.qrm.slice_snr = None;
        }
        match self.qrm.on {
            None => {
                if !qrm {
                    self.qrm.trouble.clear();
                    return;
                }
                // A slice burst heard well in the clean third: S/N to spare
                // where the QRM is not, though it may have been there from
                // the start.
                let e = inc.chan.eff_snr_db;
                if inc.third == Some(k)
                    && loss.is_none_or(|l| l < TROUBLE_LOSS)
                    && e >= NARROW_MIN_DB
                    && self.qrm.best_clean.is_none_or(|(b, t)| e >= b || now - t > SPARE_S)
                {
                    self.qrm.best_clean = Some((e, now));
                }
                let flipped = class != ChanClass::default() && self.qrm.clean.is_some_and(|c| c.1 != class);
                // One lost burst brings a poll or two in quick succession:
                // trouble counts once per TROUBLE_GAP_S.
                let fresh = self.qrm.trouble.last().is_none_or(|&t| now - t >= TROUBLE_GAP_S);
                if fresh && (poll || flipped || loss.is_some_and(|l| l >= TROUBLE_LOSS)) {
                    self.qrm.trouble.push(now);
                }
                self.qrm.trouble.retain(|&t| now - t <= TROUBLE_S);
                // A poll in the slice wave after a long silence: the peer
                // searching the thirds for one we hear.
                let searching = poll && inc.wave == Wave::Ofdm(Guard::Slice) && now - last_rx >= SEARCHED_S;
                // Only a link that had S/N to spare: lower down the gear
                // is on MFSK and slices anyway, and fading alone can show a
                // third clean and another not.
                let spare = self
                    .qrm
                    .best_clean
                    .is_some_and(|(e, t)| e >= NARROW_MIN_DB && now - t <= SPARE_S);
                // No S/N to spare shown, as with QRM there from the start:
                // the slice bursts of a trial can show it.
                let trial = self.cfg.qrm_trial
                    && !spare
                    && inc.thirds.iter().any(|t| t - inc.thirds[k as usize] >= TRIAL_CONTRAST)
                    && self.narrow_agreed()
                    && self.qrm.trouble.len() >= TROUBLE_EVENTS
                    && now >= self.qrm.trial_after;
                if spare && (self.qrm.trouble.len() >= TROUBLE_EVENTS || searching) || trial {
                    self.qrm.trial = trial.then_some(now);
                    self.stats.narrowings += 1;
                    self.qrm.on = Some((k, now));
                    self.qrm.others = None;
                    self.qrm.clear_since = None;
                    self.qrm.slice_snr = None;
                    self.qrm.trouble.clear();
                    if self.qrm.try_wide_s == 0.0 {
                        self.qrm.try_wide_s = TRY_WIDE_S;
                    }
                }
            }
            Some((s, since)) => {
                // Our own third gone dirty: the cleanest one now. QRM that
                // moves is QRM still there: the try at the full band waits.
                if self.cfg.slot_pick.is_none()
                    && inc.thirds[s as usize] >= DIRTY_SCORE
                    && k != s
                    && inc.thirds[k as usize] <= CLEAN_SCORE
                {
                    self.qrm.on = Some((k, now));
                    self.qrm.slice_snr = None;
                }
                // A slice burst half lost while one of the other thirds, dirty
                // at the last burst heard well, was clean through it and the
                // other not: the QRM has moved onto ours. (A third clean all
                // along says nothing: the burst may have been lost to a fade.)
                if self.cfg.qrm_quick
                    && self.cfg.slot_pick.is_none()
                    && self.qrm.on == Some((s, since))
                    && inc.third == Some(s)
                    && loss.is_some_and(|l| l >= TROUBLE_LOSS)
                {
                    let (a, b) = match s {
                        0 => (1, 2),
                        1 => (0, 2),
                        _ => (0, 1),
                    };
                    let (a, b) = if inc.qrm_db[a] <= inc.qrm_db[b] { (a, b) } else { (b, a) };
                    if inc.qrm_db[a] <= MOVE_CLEAN_DB
                        && inc.qrm_db[b] - inc.qrm_db[a] >= MOVE_CONTRAST_DB
                        && self.qrm.others.is_some_and(|o| o[a] >= MOVE_CONTRAST_DB)
                    {
                        self.qrm.on = Some((a as u8, now));
                        self.qrm.slice_snr = None;
                    }
                }
                if self.qrm.on != Some((s, since)) {
                    self.qrm.others = None;
                } else if inc.third == Some(s) && loss.is_none_or(|l| l < TROUBLE_LOSS) {
                    self.qrm.others = Some(inc.qrm_db);
                }
                let (s, since) = self.qrm.on.unwrap_or((s, since));
                if let Some(t0) = self.qrm.trial {
                    let e = inc.chan.eff_snr_db;
                    if inc.third == Some(s) && loss.is_none_or(|l| l < TROUBLE_LOSS) && e >= NARROW_MIN_DB {
                        self.qrm.trial = None;
                        self.qrm.trial_wait_s = 0.0;
                        self.qrm.best_clean = Some((e, now));
                    } else if now - t0 >= TRIAL_S {
                        self.widen(false);
                        self.qrm.trial = None;
                        self.qrm.trial_wait_s = (self.qrm.trial_wait_s * 2.0).clamp(TRIAL_AGAIN_S, TRY_WIDE_MAX_S);
                        self.qrm.trial_after = now + self.qrm.trial_wait_s;
                        return;
                    }
                }
                if inc.wave == Wave::Ofdm(Guard::Slice) && inc.code < LADDER.len() {
                    self.qrm.slice_snr = Some(self.last_rx_eff);
                }
                if self.cfg.narrow {
                    self.qrm.clear_since = None;
                    return;
                }
                // Other bursts say nothing either way.
                match wide_snr_db(inc, s) {
                    Some(w) if w >= WIDEN_DB => {
                        // Clear since this burst began: it measured all of it.
                        let began = match &inc.soft {
                            Some(Soft::Llrs(l)) if self.cfg.qrm_quick => {
                                now - self.air.duration(inc.wave, inc.code, l.len())
                            }
                            _ => now,
                        };
                        let t0 = *self.qrm.clear_since.get_or_insert(began);
                        if now - t0 >= self.cfg.qrm_clear_s {
                            self.widen(false);
                            return;
                        }
                    }
                    Some(_) => self.qrm.clear_since = None,
                    None => {}
                }
                if self.qrm.clear_since.is_none() && now - since >= self.qrm.try_wide_s {
                    self.widen(true);
                    // The S/N to spare still holds: should the try fail, the
                    // trouble is the QRM's, and the link narrows again.
                    if let Some((e, _)) = self.qrm.best_clean {
                        self.qrm.best_clean = Some((e, now));
                    }
                }
            }
        }
    }

    /// Back to the full band; `trying`: the QRM is still there.
    pub(super) fn widen(&mut self, trying: bool) {
        self.qrm.on = None;
        self.qrm.others = None;
        self.qrm.trial = None;
        self.qrm.clear_since = None;
        self.qrm.trouble.clear();
        self.qrm.try_wide_s = if trying {
            (self.qrm.try_wide_s * 2.0).min(TRY_WIDE_MAX_S)
        } else {
            TRY_WIDE_S
        };
    }

    /// The third we ask slice bursts in: by hand, narrowed, or the meter's.
    pub(super) fn wanted_slot(&self, meter: u8) -> u8 {
        match (self.cfg.slot_pick, self.qrm.on) {
            (Some(k), _) => k.min(2),
            (None, Some((k, _))) => k,
            (None, None) => meter,
        }
    }

    /// Both ends narrow for QRM ([`caps::NARROW`]): with an older peer the
    /// watch only moves the slot, and the reports stay as measured.
    pub(super) fn narrow_agreed(&self) -> bool {
        self.caps & caps::NARROW != 0
    }

    /// The class and effective SNR our ACKs report: narrowed, those from
    /// before the QRM (the SNR then from the slice bursts in our third).
    pub(super) fn reported_channel(&self) -> (ChanClass, f32) {
        if self.narrowed() && self.narrow_agreed() {
            // On trial, with no slice burst heard yet: enough for the
            // slower slice level.
            if self.qrm.trial.is_some() && self.qrm.slice_snr.is_none() {
                let l = SLICE_TOP - 1;
                return (
                    ChanClass::default(),
                    self.cfg.thresholds_db[l] + self.cfg.margin_db + 1.0,
                );
            }
            let (eff, class) = self.qrm.clean.unwrap_or((self.last_rx_eff, ChanClass::default()));
            return (class, self.qrm.slice_snr.unwrap_or(eff));
        }
        (self.chan.class(), self.last_rx_eff)
    }

    /// Sending side: a poll that searches the thirds for one the peer can
    /// hear. Every other poll from the third unanswered on goes as a slice
    /// burst, in the third above, below, then the centre, while the link
    /// had S/N to spare when it went quiet (a fade at low S/N wants the
    /// MFSK polls): QRM over the rest of the passband at the peer's end
    /// leaves it deaf to everything full-band, the polls included.
    pub(super) fn search_poll(&self, now: f64) -> Option<super::Outgoing> {
        if !self.cfg.qrm_narrow || !self.narrow_agreed() || self.tries < 3 || self.tries.is_multiple_of(2) {
            return None;
        }
        if now - self.good_aim_at > SEARCH_S {
            return None;
        }
        let k = [2u8, 1, 0][(self.tries as usize - 3) / 2 % 3];
        let mut b = self.tiny(super::Tiny::Poll, Wave::Ofdm(Guard::Slice));
        b.slot = Some(k);
        Some(b)
    }

    /// Sending side: whether the peer's last full ACK asks for the slice
    /// levels. On the way in, the slice levels start afresh (whatever they
    /// did in another third is no guide) and the gear drops to them.
    pub(super) fn peer_narrows(&mut self, on: bool, now: f64) {
        let on = on && self.narrow_agreed();
        if !on && self.peer_narrow {
            // Back to the full band: what its levels lost was the QRM's
            // doing.
            for k in SLICE_TOP + 1..LADDER.len() {
                self.success[k] = None;
                self.fails[k] = 0;
                self.blocked_until[k] = 0.0;
            }
            // The gear picked the next level from this ACK still capped:
            // pick again, or one more slice burst goes out first.
            self.peer_narrow = false;
            if let Some(aim) = self.aim_db {
                self.target = self.level_for(aim);
                self.level = match self.cfg.gear {
                    super::Gear::Goodput => self.pick_by_goodput(aim, now),
                    _ => self.usable_below(self.target.min(self.level_cap()), now),
                };
                self.burst_s = self.cfg.max_burst_s;
            }
        }
        if on && !self.peer_narrow {
            for k in (0..=SLICE_TOP).filter(|&k| LADDER[k].wave == Wave::Ofdm(Guard::Slice)) {
                self.success[k] = None;
                self.fails[k] = 0;
                self.blocked_until[k] = 0.0;
            }
            self.peer_narrow = true;
            // The reports from before were measured through the QRM.
            self.snr_reports.clear();
            self.target = self.target.min(SLICE_TOP);
            self.level = self.usable_below(self.level.min(SLICE_TOP), now);
        }
        self.peer_narrow = on;
    }

    /// A data burst's length `s` capped while narrowed
    /// ([`super::Config::narrow_burst_s`]).
    pub(super) fn narrow_cap_s(&self, s: f64) -> f64 {
        match self.cfg.narrow_burst_s {
            Some(cap) if self.peer_narrow || self.cfg.narrow => s.min(cap),
            _ => s,
        }
    }

    /// The highest level the sending side may use: the slice levels while
    /// the peer (or our own setting) has narrowed the link.
    pub(super) fn level_cap(&self) -> usize {
        if self.peer_narrow || self.cfg.narrow {
            self.cfg.max_level.min(SLICE_TOP)
        } else {
            self.cfg.max_level
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_top_is_the_faster_slice_level() {
        assert_eq!(LADDER[SLICE_TOP].wave, Wave::Ofdm(Guard::Slice));
        assert!(LADDER[SLICE_TOP + 1].wave != Wave::Ofdm(Guard::Slice));
    }

    #[test]
    fn contrast_needs_a_clean_third_and_a_dirty_one() {
        assert_eq!(contrast(&[17.0, 20.0, 0.0]), Some((2, true)));
        assert_eq!(contrast(&[0.5, 1.0, 0.0]), Some((2, false)));
        assert_eq!(contrast(&[17.0, 19.0, 12.0]), Some((2, false)));
        assert_eq!(contrast(&[17.5, 20.7, 12.0]), Some((2, true)));
        assert_eq!(contrast(&[25.0, 28.0, 16.0]), Some((2, false)));
        assert_eq!(contrast(&[f32::NAN, 0.0, 0.0]), None);
    }
}
