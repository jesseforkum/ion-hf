//! A whole net on a simulated clock: any number of [`NetStation`]s (or
//! other [`SimStation`]s, such as [`super::bcast::BcastStation`]), an SNR
//! for every (sender, receiver) pair, slow fading, collisions and carrier
//! sense. Frame loss comes from the ladder thresholds, as in
//! [`super::sim::ModelMedium`].

use std::sync::Arc;

use ion_dsp::dsp::NoiseGen;

use super::net::NetStation;
use super::sim::control_threshold_db;
use super::station::{Air, Incoming, Outgoing, TINY};

/// A station the simulator can drive: it keys bursts and hears them on
/// the simulator's clock.
pub trait SimStation {
    fn poll_tx(&mut self, now: f64) -> Option<Outgoing>;
    fn on_tx_done(&mut self, t: f64);
    /// A burst arrived, ending at `now`; `air`: (start, end) on our clock.
    fn on_rx_at(&mut self, inc: &Incoming, now: f64, air: Option<(f64, f64)>);
    fn set_busy(&mut self, busy: bool);
}

impl SimStation for NetStation {
    fn poll_tx(&mut self, now: f64) -> Option<Outgoing> {
        NetStation::poll_tx(self, now)
    }

    fn on_tx_done(&mut self, t: f64) {
        NetStation::on_tx_done(self, t)
    }

    fn on_rx_at(&mut self, inc: &Incoming, now: f64, air: Option<(f64, f64)>) {
        NetStation::on_rx_at(self, inc, now, air)
    }

    fn set_busy(&mut self, busy: bool) {
        NetStation::set_busy(self, busy)
    }
}

impl SimStation for super::bcast::BcastStation {
    fn poll_tx(&mut self, now: f64) -> Option<Outgoing> {
        super::bcast::BcastStation::poll_tx(self, now)
    }

    fn on_tx_done(&mut self, t: f64) {
        super::bcast::BcastStation::on_tx_done(self, t)
    }

    fn on_rx_at(&mut self, inc: &Incoming, now: f64, _air: Option<(f64, f64)>) {
        self.on_rx(inc, now)
    }

    fn set_busy(&mut self, busy: bool) {
        super::bcast::BcastStation::set_busy(self, busy)
    }
}

/// A burst on the air.
#[derive(Clone, Debug)]
struct Flight {
    from: usize,
    start: f64,
    end: f64,
    burst: Outgoing,
}

pub struct NetSim<S: SimStation = NetStation> {
    pub stations: Vec<S>,
    /// Stations switched on: one that is off neither hears nor sends.
    pub on: Vec<bool>,
    /// Mean SNR, dB in 2.4 kHz, from `[from][to]`.
    pub snr: Vec<Vec<f32>>,
    /// Slow fade on every path (standard deviation, dB) and its time scale.
    pub fade_db: f32,
    pub fade_tau_s: f64,
    pub thresholds_db: Vec<f32>,
    pub knee_db: f32,
    /// A burst within this many dB of another at the same receiver is
    /// lost with it; a stronger one survives a weaker one.
    pub capture_db: f32,
    /// Seconds into a burst before receivers notice the channel is busy.
    pub detect_s: f64,
    pub air: Arc<dyn Air>,
    pub t: f64,
    pub dt: f64,
    flights: Vec<Flight>,
    fades: Vec<Vec<(f32, f64)>>,
    noise: NoiseGen,
}

impl<S: SimStation> NetSim<S> {
    pub fn new(stations: Vec<S>, snr_db: f32, air: Arc<dyn Air>, seed: u64) -> Self {
        let n = stations.len();
        Self {
            stations,
            on: vec![true; n],
            snr: vec![vec![snr_db; n]; n],
            fade_db: 0.0,
            fade_tau_s: 30.0,
            thresholds_db: crate::level::thresholds_db(),
            knee_db: 2.0,
            capture_db: 6.0,
            detect_s: 0.5,
            air,
            t: 0.0,
            dt: 0.01,
            flights: Vec::new(),
            fades: vec![vec![(0.0, 0.0); n]; n],
            noise: NoiseGen::new(seed),
        }
    }

    /// Sets the SNR both ways between two stations.
    pub fn set_path(&mut self, a: usize, b: usize, snr_db: f32) {
        self.snr[a][b] = snr_db;
        self.snr[b][a] = snr_db;
    }

    pub fn run_until(&mut self, t_max: f64, mut done: impl FnMut(&Self) -> bool) -> bool {
        while self.t < t_max {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    fn snr_at(&mut self, from: usize, to: usize, t: f64) -> f32 {
        let mean = self.snr[from][to];
        if self.fade_db <= 0.0 {
            return mean;
        }
        let (f, ft) = self.fades[from][to];
        let a = (-(t - ft).max(0.0) / self.fade_tau_s).exp() as f32;
        let f = a * f + (1.0 - a * a).sqrt() * self.fade_db * self.noise.gaussian();
        self.fades[from][to] = (f, t);
        mean + f
    }

    /// Chance a frame gets through: 99% at `threshold`, 50% at
    /// [`NetSim::knee_db`] below it, logistic in between.
    fn p_ok(&self, threshold: f32, snr: f32) -> f32 {
        let s = 4.595 / self.knee_db; // ln(99)
        let mid = threshold - self.knee_db;
        1.0 / (1.0 + (-(snr - mid) * s).exp())
    }

    pub fn step(&mut self) {
        let t = self.t;
        // Finished bursts: decide, per receiver, what got through.
        let done: Vec<Flight> = {
            let (d, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.flights).into_iter().partition(|f| f.end <= t);
            self.flights = keep;
            d
        };
        // Bursts that ended this step still collide with the ones in flight
        // and with each other.
        let all: Vec<Flight> = done.iter().cloned().chain(self.flights.iter().cloned()).collect();
        for f in &done {
            self.stations[f.from].on_tx_done(f.end);
            for r in 0..self.stations.len() {
                if r == f.from || !self.on[r] {
                    continue;
                }
                // Half duplex: deaf while transmitting over any part of it.
                if all.iter().any(|o| o.from == r && o.start < f.end && o.end > f.start) {
                    continue;
                }
                let snr = self.snr_at(f.from, r, f.start);
                let collided = all.iter().any(|o| {
                    o.from != f.from
                        && o.from != r
                        && o.start < f.end
                        && o.end > f.start
                        && self.snr[o.from][r] > snr - self.capture_db
                });
                if collided {
                    continue;
                }
                let control_th = control_threshold_db(f.burst.wave);
                if self.noise.uniform() > self.p_ok(control_th - 1.0, snr) {
                    continue;
                }
                let th = match f.burst.code {
                    TINY => control_th,
                    l if l < self.thresholds_db.len() => self.thresholds_db[l],
                    _ => control_th,
                };
                let frames = f
                    .burst
                    .frames
                    .iter()
                    .map(|fr| (self.noise.uniform() <= self.p_ok(th, snr)).then(|| fr.clone()))
                    .collect();
                let snr_db = snr + 0.5 * self.noise.gaussian();
                let inc = Incoming {
                    wave: f.burst.wave,
                    code: f.burst.code,
                    user: f.burst.user,
                    extra: f.burst.extra,
                    frames,
                    soft: None,
                    snr_db,
                    chan: crate::chan::ChannelMeas::of_snr(snr_db),
                    slot: 0,
                    thirds: [f32::NAN; 3],
                    qrm_db: [f32::NAN; 3],
                    third: None,
                    grid: None,
                };
                // Everyone shares the simulator's clock: the burst's place
                // on it is known exactly.
                self.stations[r].on_rx_at(&inc, f.end, Some((f.start, f.end)));
            }
        }
        // Carrier sense: a station notices a burst it can detect, a moment
        // after it starts.
        for r in 0..self.stations.len() {
            let busy = self.flights.iter().any(|o| {
                o.from != r
                    && t >= o.start + self.detect_s
                    && self.snr[o.from][r] >= control_threshold_db(o.burst.wave) - 3.0
            });
            self.stations[r].set_busy(busy);
        }
        for s in 0..self.stations.len() {
            if !self.on[s] || self.flights.iter().any(|o| o.from == s) {
                continue;
            }
            if let Some(b) = self.stations[s].poll_tx(t) {
                let end = t + self.air.duration(b.wave, b.code, b.coded_bits());
                self.flights.push(Flight {
                    from: s,
                    start: t,
                    end,
                    burst: b,
                });
            }
        }
        self.t += self.dt;
    }
}
