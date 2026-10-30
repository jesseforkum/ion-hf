//! Two stations talking over a simulated half-duplex channel, on a
//! simulated clock. The [`Medium`] decides what arrives; [`ModelMedium`]
//! does it statistically (frame loss from each level's threshold and a
//! slowly fading SNR, HARQ combining by accumulated mutual information) so
//! the protocol can be exercised without audio.

use std::sync::Arc;

use ion_dsp::dsp::NoiseGen;

use super::harq::{ModelSlot, Soft};
use super::station::{Air, CONTROL, Expected, Incoming, Outgoing, Station, TINY};
use super::stream::{self, Listen};
use crate::fec::Rate;
use crate::level::{FrameSize, Guard, LADDER, Wave};
use crate::phy_mfsk::MfskMode;

/// Turns one station's burst into what the other receives (or nothing, if
/// the burst was not even detected).
pub trait Medium {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, end: f64) -> Option<Incoming>;

    /// What station `to` expects to hear next ([`Station::expected`]),
    /// before each burst it may receive.
    fn expect(&mut self, _to: usize, _e: &[Expected]) {}

    /// What station `to`'s stream timekeeping needs
    /// ([`Station::stream_listen`]), before each burst it may receive.
    fn stream(&mut self, _to: usize, _l: Option<Listen>) {}
}

/// Air-time arithmetic from a handful of numbers per waveform, roughly what
/// the PHYs do (OFDM: 44 data carriers of 1/24 ms; long guard ~15% slower,
/// slice ~4.4x; MFSK: a few hundred bit/s).
#[derive(Clone, Debug)]
pub struct ModelAir {
    pub ofdm_carriers: usize,
    pub ofdm_symbol_s: f64,
    pub ofdm_overhead_s: f64,
    pub long_guard_factor: f64,
    pub slice_factor: f64,
    /// Raw (coded) bit/s and preamble+header seconds, MFSK R0 and R1.
    pub mfsk_bps: [f64; 2],
    pub mfsk_overhead_s: [f64; 2],
}

impl Default for ModelAir {
    fn default() -> Self {
        Self {
            ofdm_carriers: 44,
            ofdm_symbol_s: 0.024,
            ofdm_overhead_s: 0.18,
            long_guard_factor: 1.15,
            slice_factor: 4.4,
            mfsk_bps: [150.0, 450.0],
            mfsk_overhead_s: [0.6, 0.5],
        }
    }
}

impl Air for ModelAir {
    fn duration(&self, wave: Wave, code: usize, coded_bits: usize) -> f64 {
        if code == stream::TONES {
            return 2.0 * stream::tones::TONE_S;
        }
        let bits_per_sym = match code {
            TINY | CONTROL | stream::STREAM => 2,
            l => LADDER[l].constellation.bits_per_symbol(),
        };
        match wave {
            Wave::Mfsk(m) => {
                let i = (m != MfskMode::R0) as usize;
                self.mfsk_overhead_s[i] + coded_bits as f64 / self.mfsk_bps[i]
            }
            Wave::Ofdm(g) => {
                let f = match g {
                    Guard::Normal => 1.0,
                    Guard::Long => self.long_guard_factor,
                    Guard::Slice => self.slice_factor,
                };
                let symbols = (coded_bits as f64 / (self.ofdm_carriers * bits_per_sym) as f64).ceil();
                f * (self.ofdm_overhead_s + symbols * self.ofdm_symbol_s)
            }
        }
    }
}

/// SNR (dB in 2.4 kHz) at which header-only and control bursts in `wave`
/// still get through ~99% of the time: about what each PHY's header manages.
pub fn control_threshold_db(wave: Wave) -> f32 {
    match wave {
        Wave::Mfsk(MfskMode::R0) => -12.0,
        Wave::Mfsk(MfskMode::R1) => -8.0,
        Wave::Ofdm(Guard::Slice) => -6.0,
        Wave::Ofdm(Guard::Long) => -4.0,
        Wave::Ofdm(Guard::Normal) => -3.0,
    }
}

/// Frame loss from thresholds, over an SNR that fades slowly.
pub struct ModelMedium {
    /// Mean SNR, dB in 2.4 kHz.
    pub snr_db: f32,
    /// Standard deviation of the slow fade, dB (0 = steady).
    pub fade_db: f32,
    /// Correlation time of the fade, seconds.
    pub fade_tau_s: f64,
    /// ~1% frame-loss SNR per ladder level.
    pub thresholds_db: Vec<f32>,
    /// dB from 1% loss to 50% loss.
    pub knee_db: f32,
    /// Anything to this station index is lost entirely (nobody there).
    pub deaf: Option<usize>,
    /// Added to the SNR of what each station sends, dB: a lopsided link.
    pub dir_db: [f32; 2],
    /// From this time on, the mean SNR is this instead: a band that dies.
    pub drop_at: Option<(f64, f32)>,
    /// Delay span (ms) and Doppler spread (Hz) the receivers measure on
    /// every burst with data (`None`: nothing measured).
    pub spreads: Option<(f32, f32)>,
    noise: NoiseGen,
    fade: f32,
    fade_t: f64,
}

impl ModelMedium {
    pub fn new(snr_db: f32, seed: u64) -> Self {
        Self {
            snr_db,
            fade_db: 0.0,
            fade_tau_s: 30.0,
            thresholds_db: crate::level::thresholds_db(),
            knee_db: 2.0,
            deaf: None,
            dir_db: [0.0; 2],
            drop_at: None,
            spreads: None,
            noise: NoiseGen::new(seed),
            fade: 0.0,
            fade_t: 0.0,
        }
    }

    fn snr_at(&mut self, t: f64) -> f32 {
        if self.fade_db > 0.0 {
            // Ornstein-Uhlenbeck in dB.
            let dt = (t - self.fade_t).max(0.0);
            let a = (-dt / self.fade_tau_s).exp() as f32;
            self.fade = a * self.fade + (1.0 - a * a).sqrt() * self.fade_db * self.noise.gaussian();
            self.fade_t = t;
        }
        let mean = match self.drop_at {
            Some((at, snr)) if t >= at => snr,
            _ => self.snr_db,
        };
        mean + self.fade
    }

    /// Probability a frame at `threshold` survives `snr`: 99% at the
    /// threshold, 50% at `knee_db` below it, logistic in between.
    fn p_ok(&self, threshold: f32, snr: f32) -> f32 {
        let s = 4.595 / self.knee_db; // ln(99)
        let mid = threshold - self.knee_db;
        1.0 / (1.0 + (-(snr - mid) * s).exp())
    }

    /// Equivalent SNR per coded bit (linear, see [`ModelSlot::gamma`]) of
    /// one long-frame transmission at `level` and `snr`, for the uniform
    /// draw `u`.
    ///
    /// The draw becomes an effective SNR on the same logistic as
    /// [`Self::p_ok`]: a fresh frame alone decodes exactly when
    /// `u <= p_ok`. Its information per coded bit is the Shannon capacity
    /// of the level's constellation (`b` bits per symbol), shifted by a gap
    /// that puts one whole frame's 50% point at the level's threshold
    /// minus the knee. Increments and repeats then add information as the
    /// receiver combines them: a rough model of IR and chase gain.
    fn gamma(&self, level: usize, snr: f32, u: f32) -> f32 {
        let s = 4.595 / self.knee_db;
        let mid = self.thresholds_db[level] - self.knee_db;
        let u = u.clamp(1e-6, 1.0 - 1e-6);
        let eff = snr - (u / (1.0 - u)).ln() / s;
        let lv = LADDER[level];
        let b = lv.constellation.bits_per_symbol() as f32;
        let gap = mid - 10.0 * (2f32.powf(lv.rate.value() as f32 * b) - 1.0).log10();
        let info = ((1.0 + 10f32.powf((eff - gap) / 10.0)).log2() / b).min(1.0);
        2f32.powf(info) - 1.0
    }
}

impl Medium for ModelMedium {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, _end: f64) -> Option<Incoming> {
        if self.deaf == Some(1 - from) {
            return None;
        }
        let snr = self.snr_at(start) + self.dir_db[from];
        if burst.code == stream::TONES || burst.code == stream::STREAM {
            return self.carry_stream(burst, snr);
        }
        let control_th = control_threshold_db(burst.wave);
        // The header is a little more robust than a control frame.
        if self.noise.uniform() > self.p_ok(control_th - 1.0, snr) {
            return None;
        }
        let th = match burst.code {
            TINY | CONTROL => control_th,
            l => self.thresholds_db[l],
        };
        let long = burst.code < LADDER.len() && LADDER[burst.code].frame == FrameSize::Long;
        let (frames, soft) = if long {
            let slots = burst
                .frames
                .iter()
                .enumerate()
                .map(|(i, f)| {
                    let (from, bits) = burst.span(i);
                    let u = self.noise.uniform();
                    ModelSlot {
                        info: f.clone(),
                        from,
                        bits,
                        gamma: self.gamma(burst.code, snr, u),
                    }
                })
                .collect();
            (Vec::new(), Some(Soft::Model(slots)))
        } else {
            let frames = burst
                .frames
                .iter()
                .map(|f| (self.noise.uniform() <= self.p_ok(th, snr)).then(|| f.clone()))
                .collect();
            (frames, None)
        };
        let snr_db = snr + 0.5 * self.noise.gaussian();
        Some(Incoming {
            wave: burst.wave,
            code: burst.code,
            user: burst.user,
            extra: burst.extra,
            frames,
            soft,
            snr_db,
            // No echoes or Doppler in the model, whose frames survive by the
            // ladder's thresholds, midway between AWGN and CCIR poor.
            chan: {
                let mut c = crate::chan::ChannelMeas::of_snr(snr_db - MODEL_FADING_DB);
                if let Some((d, f)) = self.spreads.filter(|_| !burst.frames.is_empty()) {
                    c.delay_ms = d;
                    c.doppler_hz = f;
                }
                c
            },
            slot: 0,
            grid: None,
        })
    }
}

impl ModelMedium {
    /// A stream period (its timing is known, so it is always "heard";
    /// each frame survives by its option's threshold) or a tone answer
    /// (heard down to a few dB under the MFSK floor's control threshold).
    fn carry_stream(&mut self, burst: &Outgoing, snr: f32) -> Option<Incoming> {
        let mut inc = Incoming {
            wave: burst.wave,
            code: burst.code,
            user: burst.user,
            extra: burst.extra,
            frames: Vec::new(),
            soft: None,
            snr_db: snr + 0.5 * self.noise.gaussian(),
            chan: crate::chan::ChannelMeas::of_snr(snr - MODEL_FADING_DB),
            slot: 0,
            grid: None,
        };
        if burst.code == stream::TONES {
            return (self.noise.uniform() <= self.p_ok(control_threshold_db(FLOOR_WAVE_MODEL) - 3.0, snr))
                .then_some(inc);
        }
        let th = stream::model_threshold_db((burst.extra & 0xFF) as usize);
        inc.frames = burst
            .frames
            .iter()
            .map(|f| (self.noise.uniform() <= self.p_ok(th, snr)).then(|| f.clone()))
            .collect();
        Some(inc)
    }
}

const FLOOR_WAVE_MODEL: Wave = Wave::Mfsk(MfskMode::R0);

/// What the model's channel costs against AWGN: its effective SNR is the
/// mean less this.
pub const MODEL_FADING_DB: f32 = 2.5;

/// One burst on the air, for the log.
#[derive(Clone, Debug)]
pub struct AirEvent {
    pub from: usize,
    pub start: f64,
    pub end: f64,
    pub wave: Wave,
    pub code: usize,
    pub frames: usize,
    pub delivered: bool,
    /// Sent behind a cue ([`Outgoing::cue`]).
    pub cue: bool,
}

/// Where the air time of a run went, for the data bursts of one ladder
/// level (see [`Sim::airtime`]). Seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct LevelAirtime {
    /// Data bursts sent at this level.
    pub bursts: u32,
    /// Their data symbols: frames, HARQ increments, resends, padding.
    pub data_s: f64,
    /// Their preambles and headers (and the caller's lead-in, if its
    /// [`Air`] counts one): a 1-bit burst's length each.
    pub preamble_s: f64,
    /// ACK bursts from the other side (answers to polls included).
    pub ack_s: f64,
    /// Other control bursts from the data side: polls, turn, goodbye.
    pub control_s: f64,
    /// Nobody on the air: turnarounds, and waits for bursts that were lost.
    pub gap_s: f64,
}

impl LevelAirtime {
    pub fn total_s(&self) -> f64 {
        self.data_s + self.preamble_s + self.ack_s + self.control_s + self.gap_s
    }

    pub fn add(&mut self, o: &LevelAirtime) {
        self.bursts += o.bursts;
        self.data_s += o.data_s;
        self.preamble_s += o.preamble_s;
        self.ack_s += o.ack_s;
        self.control_s += o.control_s;
        self.gap_s += o.gap_s;
    }

    /// A markdown row: label, bursts, seconds per cycle, then the share of
    /// air time in data, preamble, ACK, other control and gaps.
    pub fn row(&self, label: &str) -> String {
        let t = self.total_s().max(1e-9);
        let pc = |x: f64| format!("{:.0}%", 100.0 * x / t);
        format!(
            "| {label} | {} | {:.2} | {} | {} | {} | {} | {} |",
            self.bursts,
            t / self.bursts.max(1) as f64,
            pc(self.data_s),
            pc(self.preamble_s),
            pc(self.ack_s),
            pc(self.control_s),
            pc(self.gap_s)
        )
    }

    /// Header for [`Self::row`].
    pub const HEADER: &'static str = "| Level | data bursts | s per cycle | data | preamble | ACK | other control | gaps |\n|---|---:|---:|---:|---:|---:|---:|---:|";
}

/// Per-level air time, summed over runs: (setup, then one per ladder level).
#[derive(Clone, Debug)]
pub struct Airtime {
    /// Before the first data burst: the call and its answer.
    pub setup: LevelAirtime,
    pub levels: Vec<LevelAirtime>,
}

impl Default for Airtime {
    fn default() -> Self {
        Self {
            setup: LevelAirtime::default(),
            levels: vec![LevelAirtime::default(); LADDER.len()],
        }
    }
}

impl Airtime {
    pub fn add(&mut self, o: &Airtime) {
        self.setup.add(&o.setup);
        for (a, b) in self.levels.iter_mut().zip(&o.levels) {
            a.add(b);
        }
    }

    /// All levels together (setup excluded).
    pub fn total(&self) -> LevelAirtime {
        let mut t = LevelAirtime::default();
        for l in &self.levels {
            t.add(l);
        }
        t
    }

    /// A markdown table: one row per level used, then all of them.
    pub fn table(&self) -> String {
        let mut s = String::from(LevelAirtime::HEADER);
        for (l, a) in self.levels.iter().enumerate() {
            if a.bursts > 0 {
                s += "\n";
                s += &a.row(&format!("{l} {}", level_name(l)));
            }
        }
        s += "\n";
        s += &self.total().row("all");
        s
    }
}

/// Short name of ladder level `l`, e.g. "R0", "slice 1/3", "long 1/2",
/// "64QAM 5/6".
pub fn level_name(l: usize) -> String {
    let lv = LADDER[l];
    let r = match lv.rate {
        Rate::R1_4 => "1/4",
        Rate::R1_3 => "1/3",
        Rate::R1_2 => "1/2",
        Rate::R2_3 => "2/3",
        Rate::R3_4 => "3/4",
        Rate::R5_6 => "5/6",
    };
    match lv.wave {
        Wave::Mfsk(m) if lv.rate == Rate::R1_4 => format!("{m:?}"),
        Wave::Mfsk(m) => format!("{m:?} {r}"),
        Wave::Ofdm(Guard::Slice) => format!("slice {r}"),
        Wave::Ofdm(Guard::Long) => format!("long {r}"),
        Wave::Ofdm(Guard::Normal) => format!("{:?} {r}", lv.constellation),
    }
}

pub struct Sim {
    pub stations: [Station; 2],
    pub medium: Box<dyn Medium>,
    pub air: Arc<dyn Air>,
    pub t: f64,
    pub dt: f64,
    pub log: Vec<AirEvent>,
    busy_until: [f64; 2],
    /// (end time, sender, what the other side receives)
    in_flight: Vec<(f64, usize, Option<Incoming>)>,
}

impl Sim {
    pub fn new(a: Station, b: Station, medium: Box<dyn Medium>, air: Arc<dyn Air>) -> Self {
        Self {
            stations: [a, b],
            medium,
            air,
            t: 0.0,
            dt: 0.01,
            log: Vec::new(),
            busy_until: [0.0; 2],
            in_flight: Vec::new(),
        }
    }

    /// Runs until `done` or `t_max` seconds; returns whether `done` held.
    pub fn run_until(&mut self, t_max: f64, mut done: impl FnMut(&Sim) -> bool) -> bool {
        while self.t < t_max {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    /// Where the air time so far went, from the log. Each data burst opens
    /// a cycle at its level that lasts until the next data burst: the
    /// burst itself (preamble and data), the other side's ACKs, any polls
    /// and the gaps between bursts all count to that level.
    pub fn airtime(&self) -> Airtime {
        let mut out = Airtime::default();
        // (level, sender) of the cycle in progress.
        let mut cycle: Option<(usize, usize)> = None;
        let mut prev_end: Option<f64> = None;
        for e in &self.log {
            let slot = match cycle {
                Some((l, _)) => &mut out.levels[l],
                None => &mut out.setup,
            };
            if let Some(p) = prev_end {
                slot.gap_s += (e.start - p).max(0.0);
            }
            prev_end = Some(prev_end.map_or(e.end, |p| p.max(e.end)));
            let dur = e.end - e.start;
            if e.code < LADDER.len() {
                cycle = Some((e.code, e.from));
                let a = &mut out.levels[e.code];
                let cue = if e.cue { super::station::cue_s() } else { 0.0 };
                let pre = (self.air.duration(e.wave, e.code, 1) + cue).min(dur);
                a.bursts += 1;
                a.preamble_s += pre;
                a.data_s += dur - pre;
                continue;
            }
            match cycle {
                Some((l, from)) if e.from != from => out.levels[l].ack_s += dur,
                Some((l, _)) => out.levels[l].control_s += dur,
                None => out.setup.control_s += dur,
            }
        }
        out
    }

    pub fn step(&mut self) {
        let t = self.t;
        // Bursts that have finished: the sender learns it is done, the
        // other side gets what the medium let through.
        let mut i = 0;
        while i < self.in_flight.len() {
            if self.in_flight[i].0 <= t {
                let (end, from, inc) = self.in_flight.swap_remove(i);
                self.stations[from].on_tx_done(end);
                if let Some(inc) = inc {
                    self.stations[1 - from].on_rx(&inc, end);
                }
            } else {
                i += 1;
            }
        }
        for s in 0..2 {
            let other = 1 - s;
            // Carrier sense: nobody keys over a burst in progress.
            if t < self.busy_until[s] || t < self.busy_until[other] {
                continue;
            }
            if let Some(b) = self.stations[s].poll_tx(t) {
                let end = t + b.duration(&*self.air);
                self.busy_until[s] = end;
                self.medium.expect(other, &self.stations[other].expected());
                self.medium.stream(other, self.stations[other].stream_listen());
                let inc = self.medium.carry(s, &b, t, end);
                self.log.push(AirEvent {
                    from: s,
                    start: t,
                    end,
                    wave: b.wave,
                    code: b.code,
                    frames: b.frames.len(),
                    delivered: inc.is_some(),
                    cue: b.cue,
                });
                self.in_flight.push((end, s, inc));
            }
        }
        self.t += self.dt;
    }
}
