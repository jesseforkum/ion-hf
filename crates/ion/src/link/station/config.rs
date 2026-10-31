//! Station configuration.

use crate::level::{Guard, LADDER, Wave};

/// Link settings; [`Config::new`] holds the defaults.
#[derive(Clone, Debug)]
pub struct Config {
    pub call: String,
    /// From the end of the other side's burst to our audio starting: PTT,
    /// TCI and processing delays.
    pub turnaround_s: f64,
    /// Longest data burst, seconds.
    pub max_burst_s: f64,
    /// Gear-shift targets per ladder level, dB in 2.4 kHz.
    pub thresholds_db: Vec<f32>,
    /// Extra SNR wanted above a level's threshold before using it. Negative
    /// by default: a level a little past its threshold loses a few frames,
    /// and with HARQ each costs a fraction of a frame.
    pub margin_db: f32,
    pub max_level: usize,
    /// Tries for the call and the turn.
    pub retries: u32,
    /// Tries for the goodbye before closing anyway.
    pub disc_retries: u32,
    /// Wait for an answer this long beyond its expected end, then ask again.
    pub reply_slack_s: f64,
    /// Sending side with nothing to send: check in this often.
    pub idle_s: f64,
    /// Sending side asked for the turn: hand it over within this long, with
    /// data still queued (it goes once the peer is done).
    pub break_in_s: f64,
    /// No progress for this long (nothing acknowledged, delivered or handed
    /// over, however much is heard): the link is gone.
    pub link_timeout_s: f64,
    /// A level that lost a whole burst is not tried again for this long,
    /// doubled for each further failure (a fast-fading channel kills the
    /// OFDM levels whatever the SNR, and the SNR report alone would climb
    /// straight back into them).
    pub fail_hold_s: f64,
    pub fail_hold_max_s: f64,
    /// A level that lost part of a burst is held off this long.
    pub loss_hold_s: f64,
    /// The climb steps over the slice levels when the SNR report allows
    /// this level or higher (`LADDER.len()`: never).
    pub skip_slices_from: usize,
    /// Incremental redundancy for long frames. Off: failed frames are
    /// neither kept nor continued, only resent.
    pub harq: bool,
    /// Iterative receiver for OFDM long-frame data bursts with a failed
    /// codeword: up to this many passes of channel re-estimation and
    /// interference cancellation fed by the decoder (0: off), fewer on
    /// long bursts ([`super::TURBO_SYMBOLS`]).
    pub turbo: usize,
    /// Tell the receivers which headers the peer may send next
    /// ([`super::Station::expected`]), so that a weak header that fails
    /// its check can still be recognised.
    pub expect_headers: bool,
    /// Offer (calling) or accept (called) compression of the stream
    /// ([`crate::link::compress`]): used only when both ends have it on, agreed
    /// in the ConnReq and ConnAck. Each [`super::Station::send`] then goes out as
    /// records of compressed or raw bytes, and [`super::Station::take_received`]
    /// hands back the original bytes, a whole record at a time.
    pub compress: bool,
    /// Channel report, sending side: keep off the waveforms the peer's
    /// channel class rules out (long echoes: the MFSK floor and the long
    /// guard only; fast fading: the MFSK floor only). See
    /// [`crate::link::report`].
    pub pick_family: bool,
    /// Channel report, both sides: ACKs carry the effective SNR (EESM)
    /// instead of the mean, and the targets are `eff_thresholds_db` (each
    /// level's AWGN threshold: the effective SNR already holds the fading)
    /// plus `eff_margin_db`.
    pub eff_snr: bool,
    pub eff_thresholds_db: Vec<f32>,
    pub eff_margin_db: f32,
    // -- overhead per round trip (ACK cadence and burst sizing; these
    // never choose the level) --
    /// Longer data bursts on a clean channel: after each burst whose fresh
    /// frames all arrived, with the level unchanged, the next may be twice
    /// as long, up to [`super::Station::clean_burst_s`] for its level (at most
    /// `max_burst_long_s`); any loss or level change goes back to
    /// `max_burst_s`. Off: every burst is at most `max_burst_s`.
    pub burst_adapt: bool,
    /// Longest data burst `burst_adapt` may reach.
    pub max_burst_long_s: f64,
    /// `burst_adapt` lengthens a level's bursts until its fixed cost per
    /// round trip (preamble and header, the tiny ACK, both turnarounds)
    /// is at most this share of the cycle.
    pub burst_overhead: f64,
    /// `burst_adapt` lengthens only while the peer's SNR reports stay
    /// within this many dB of each other from one ACK to the next. A
    /// channel that is changing wants the ACKs (each one a chance to
    /// change level) more often.
    pub burst_steady_db: f32,
    /// `Some(d)`: `burst_adapt` lengthens only while the SNR report is at
    /// least `d` dB short of what any faster level needs (its threshold
    /// plus `margin_db`), since close to that every ACK is a chance to
    /// climb. Off by default.
    pub burst_climb_db: Option<f32>,
    /// A burst of more frames than a tiny ACK can list, every one of which
    /// arrived, is answered by a tiny ACK with every bit set instead of a
    /// full ACK frame. A sender that does not know this reads it as "the
    /// first six arrived" and resends the rest: slower, never wrong.
    pub tiny_ack_all: bool,
    /// Answer a data burst with a tiny ACK in a shorter waveform than the
    /// burst's own when the burst's SNR is at least this many dB above
    /// where that waveform's header-only burst gets through 90% of the
    /// time on a fading channel ([`super::cheap_ack_waves`]): slice data → an
    /// OFDM-normal ACK, MFSK R0 data → MFSK R1. `None`: always the burst's
    /// own waveform.
    /// Every receiver listens to every waveform, so the peer hears it
    /// either way; a poll (a lost ACK) turns it off for a while.
    pub cheap_ack_margin_db: Option<f32>,
    /// Aim from a cautious recent SNR (the lower of the last report and
    /// the median of the last three) instead of the last report alone: on
    /// a slow-fading channel one good fade's report otherwise sends the
    /// link up into levels the next fade kills.
    pub smooth_target: bool,
    /// A lost burst does not give up a level whose recent success still
    /// beats the next one down on expected throughput (its success rate
    /// above their speed ratio): on a fading channel one burst lost in a
    /// fade is the channel's normal behaviour, not a sign to retreat.
    pub hold_by_goodput: bool,
    /// An SNR report holds for the level it was measured on: each level
    /// is aimed at with the report moved by the difference in their mean
    /// powers on the air ([`crate::level::AIR_MEAN_DB`]). Off by default:
    /// the thresholds' margins are tuned for the report as measured.
    pub power_aware: bool,
    /// The first data burst goes at the highest level the ConnAck's SNR
    /// report allows less this margin (the report was measured on our MFSK
    /// call, stronger than OFDM on the air), and a loss steps down from
    /// there. `None`: start at the floor.
    pub start_margin_db: Option<f32>,
    /// Slice bursts go in the third of the passband the peer names in its
    /// ACKs (`crate::slots`); off: always the centre. The receiving side
    /// names its pick either way, and listens in all three.
    pub slice_slots: bool,
    /// How the sending side picks each data burst's level.
    pub gear: Gear,
    /// HARQ increments may stretch a burst to `max_burst_long_s` rather
    /// than be dropped (with every stored codeword sent fresh again) when
    /// they no longer fit the burst length a loss shrank it to.
    pub harq_stretch: bool,
    /// Long-frame bursts join contiguous pieces in flight (re-cut small by
    /// a visit to the MFSK floor) back up to the frame size.
    pub merge_pieces: bool,
    /// Offer (calling) or accept (called) MFSK cues
    /// ([`crate::link::frame::caps::CUE`]): used only when both ends have
    /// it on. A full-band OFDM data burst then goes behind a cue
    /// (`link::audio::transmit`) while the SNR the gear aims by is below
    /// `cue_below_db`: the cue is heard 11–14 dB below the OFDM header.
    pub cue: bool,
    /// See `cue`: dB in 2.4 kHz, as the ACKs report it (the effective SNR
    /// with `eff_snr`).
    pub cue_below_db: f32,
    /// With a cue: when the HARQ increments for everything the peer
    /// holds do not fit the burst, send those that do (the longest stored
    /// first) and name how many in the cue, rather than stretching the
    /// burst (`harq_stretch`, still used for bursts without a cue).
    pub harq_partial: bool,
    /// Offer (calling) or accept (called) the stream gear
    /// ([`crate::link::frame::caps::STREAM`]): used only when both ends
    /// have it on. Off: the station never advertises it.
    pub stream: bool,
    /// Lab: every data burst at this level, whatever the reports and
    /// losses say.
    #[doc(hidden)]
    pub pin_level: Option<usize>,
    // -- the lifeline (`docs/link.md`, Lifeline) --
    /// With no data moving for `life_enter_s` (nothing acknowledged or
    /// delivered, no idle check-in answered), hold the link in R1/7
    /// header-only bursts instead of retrying until `link_timeout_s`:
    /// the sending side probes, the other answers with how it heard the
    /// probe, and the link resumes once both hear each other at
    /// `life_resume_db` over the floor's threshold. Offered (calling) or
    /// accepted (called) as [`crate::link::frame::caps::LIFELINE`] and used
    /// only when both ends have it on. Off: the old timeout.
    pub lifeline: bool,
    pub life_enter_s: f64,
    /// The first probe at once, the next after this, doubling up to
    /// `life_probe_max_s` (back to it when the SNR comes within 3 dB of
    /// resuming).
    pub life_probe_s: f64,
    pub life_probe_max_s: f64,
    /// Between probes, the sending side knocks this often: a header-only
    /// R0 poll, which a path that has come back answers at once (`None`:
    /// resume on the probes' reports only).
    pub life_knock_s: Option<f64>,
    /// Nothing heard from the peer for this long in a hold: the link is
    /// lost (the answering side waits `life_probe_max_s` longer).
    pub life_silent_s: f64,
    /// A hold this long: goodbye in the lifeline, the path too weak.
    pub life_max_s: f64,
    /// Over the floor's threshold (`thresholds_db`, mean SNR), dB.
    pub life_resume_db: f32,
}

/// The sending side's choice of level ([`Config::gear`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Gear {
    /// The level expected to move the most: each level's chance from how
    /// its recent bursts did, relaxing back to what the SNR report
    /// predicts, times its speed. No holds.
    #[default]
    Goodput,
    /// Up the ladder on clean ACKs to the report's target, down on losses,
    /// with holds (Ion HF 0.1.0-alpha.4 and before).
    Ladder,
}

/// Default [`Config::cue_below_db`]: IONOS S:N about +9 dB, where the
/// OFDM data header still misses bursts on the fading channels
/// (`docs/experiments/next-link.md`, the cue rule).
pub const CUE_BELOW_DB: f32 = 7.0;

/// Default [`Config::skip_slices_from`]: one past the first long-guard
/// level.
const SKIP_SLICES_FROM: usize = {
    let mut i = 0;
    while i < LADDER.len() && !matches!(LADDER[i].wave, Wave::Ofdm(Guard::Long)) {
        i += 1;
    }
    i + 1
};

impl Config {
    pub fn new(call: &str) -> Self {
        Self {
            call: call.to_ascii_uppercase(),
            turnaround_s: 0.3,
            max_burst_s: 8.0,
            thresholds_db: crate::level::thresholds_db(),
            margin_db: -1.0,
            max_level: LADDER.len() - 1,
            retries: 6,
            disc_retries: 3,
            reply_slack_s: 2.0,
            idle_s: 10.0,
            break_in_s: 20.0,
            link_timeout_s: 120.0,
            fail_hold_s: 30.0,
            fail_hold_max_s: 480.0,
            loss_hold_s: 15.0,
            skip_slices_from: SKIP_SLICES_FROM,
            harq: true,
            turbo: super::TURBO_PASSES,
            expect_headers: true,
            compress: false,
            pick_family: true,
            eff_snr: true,
            eff_thresholds_db: crate::level::EFF_THRESHOLDS_DB.to_vec(),
            eff_margin_db: 0.0,
            burst_adapt: true,
            max_burst_long_s: 32.0,
            burst_overhead: 0.12,
            burst_steady_db: 3.0,
            burst_climb_db: None,
            tiny_ack_all: true,
            cheap_ack_margin_db: Some(0.0),
            smooth_target: true,
            hold_by_goodput: true,
            power_aware: false,
            start_margin_db: Some(3.0),
            slice_slots: true,
            gear: Gear::Goodput,
            harq_stretch: true,
            merge_pieces: true,
            cue: true,
            cue_below_db: CUE_BELOW_DB,
            harq_partial: true,
            stream: true,
            pin_level: None,
            lifeline: true,
            life_enter_s: 90.0,
            life_probe_s: 60.0,
            life_probe_max_s: 120.0,
            life_knock_s: Some(30.0),
            life_silent_s: 300.0,
            life_max_s: 900.0,
            life_resume_db: 1.0,
        }
    }
}
