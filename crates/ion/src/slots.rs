//! Slice placement: the receiving side watches the three thirds of the
//! passband a 650 Hz slice burst can sit in and names the cleanest.
//!
//! [`SlotMeter`] compares each third against the whole passband in the same
//! FFT frame, so the radio's AGC, which moves all three together, cancels.
//! A third's score is its share of frames with a hit ([`HIT_DB`]), in per
//! cent, plus [`EXCESS_PCT_PER_DB`] for each dB its median sits above the
//! passband's, averaged over [`MEMORY_S`]. Frames under the link's own
//! bursts are left out ([`SlotMeter::mask`]).
//!
//! The scores cannot tell our own slice from QRM: with the slice in the one
//! clean third and QRM over the other two, the passband's median is the
//! QRM's. [`SlotMeter::qrm_db`] measures the other thirds against the noise
//! under a slice burst of ours instead, which its S/N gives.

use std::collections::VecDeque;

use ion_dsp::dsp::Complex;
use ion_dsp::fft::Fft;

/// The thirds, as offsets from the link's centre: 0 the centre (the
/// default), 1 below, 2 above.
pub const OFFSETS_HZ: [f32; 3] = [0.0, -800.0, 800.0];
pub const SLOTS: usize = 3;
/// Half the width a third is watched over: the slice's 650 Hz and a
/// little.
pub const HALF_HZ: f32 = 350.0;
/// A bin this far over the passband's median in the same frame is a
/// signal: white noise alone crosses it in about one frame in 30 000.
pub const HIT_DB: f32 = 13.0;
/// Score: per cent of hit frames that each dB of excess counts as.
pub const EXCESS_PCT_PER_DB: f32 = 1.0;
/// Time constant of the meter's averages, seconds: signals in the passband
/// come and go within tens of seconds.
pub const MEMORY_S: f32 = 8.0;
/// Score points a third must beat the current one by to take over.
pub const HYSTERESIS: f32 = 5.0;
/// Frames the meter needs before it names anything but the centre.
const MIN_FRAMES: f32 = 100.0;
/// The slice's width, Hz.
pub const SLICE_HZ: f32 = 650.0;
/// How long the meter keeps each frame's power for [`SlotMeter::qrm_db`]:
/// the longest burst and a little, s.
const POWER_S: f32 = 30.0;
/// Frames a burst must span for [`SlotMeter::qrm_db`].
const QRM_MIN_FRAMES: usize = 10;
/// [`SlotMeter::qrm_db`]'s floor: QRM this far under the noise, or none.
pub const QRM_FLOOR_DB: f32 = -20.0;

const N: usize = 2048;
const HOP: usize = N / 2;

/// One frame's measurements, kept until it is old enough to be sure no
/// burst of ours covered it.
#[derive(Clone, Debug)]
struct Frame {
    /// Audio samples (from the first given to the meter) it spans.
    start: u64,
    end: u64,
    hit: [bool; SLOTS],
    excess_db: [f32; SLOTS],
    /// Thirds a burst of the link's covered.
    masked: [bool; SLOTS],
}

/// The passband watcher (48 kHz audio).
pub struct SlotMeter {
    rate: f32,
    center_hz: f32,
    fft: Fft,
    win: Vec<f32>,
    pending: Vec<f32>,
    /// Audio samples taken so far.
    taken: u64,
    /// Frames not yet averaged in, newest last.
    recent: VecDeque<Frame>,
    /// How long a frame waits before it counts, samples.
    lag: u64,
    /// Averages per third: hit share, excess (dB), and frames counted.
    hit: [f32; SLOTS],
    excess: [f32; SLOTS],
    count: [f32; SLOTS],
    current: usize,
    /// Per frame, newest last: its first sample, and each third's mean
    /// power per bin over [`HALF_HZ`] and over the slice's width.
    power: VecDeque<(u64, [f32; SLOTS], [f32; SLOTS])>,
    buf: Vec<Complex>,
    spec: Vec<Complex>,
}

impl SlotMeter {
    /// A meter for a link centred on `center_hz`; frames count once they
    /// are `lag_s` old (longer than the longest burst, so a burst decoded
    /// at its end can still mask the frames it covered).
    pub fn new(center_hz: f32, rate: f32, lag_s: f32) -> Self {
        let win = (0..N)
            .map(|i| 0.5 - 0.5 * (core::f32::consts::TAU * i as f32 / N as f32).cos())
            .collect();
        Self {
            rate,
            center_hz,
            fft: Fft::new(N),
            win,
            pending: Vec::new(),
            taken: 0,
            recent: VecDeque::new(),
            lag: (lag_s * rate) as u64,
            hit: [0.0; SLOTS],
            excess: [0.0; SLOTS],
            count: [0.0; SLOTS],
            current: 0,
            power: VecDeque::new(),
            buf: vec![Complex::ZERO; N],
            spec: vec![Complex::ZERO; N],
        }
    }

    /// Audio samples taken so far (the clock [`Self::mask`] counts in).
    pub fn samples(&self) -> u64 {
        self.taken
    }

    /// The next stretch of the receiver's audio. Its frames count from
    /// the next [`Self::fold`] (or [`Self::best`]) on, once [`Self::mask`]
    /// has had the chance to leave out the link's bursts in it.
    pub fn push(&mut self, x: &[f32]) {
        self.pending.extend_from_slice(x);
        while self.pending.len() >= N {
            let start = self.taken;
            let f = self.frame(start);
            self.recent.push_back(f);
            let keep = (POWER_S * self.rate) as u64;
            while self.power.front().is_some_and(|p| p.0 + keep < start) {
                self.power.pop_front();
            }
            self.pending.drain(..HOP);
            self.taken += HOP as u64;
        }
    }

    /// Leaves samples `start..end` out of third `slot` (`None`: all of
    /// them): one of the link's own bursts, there.
    pub fn mask(&mut self, start: u64, end: u64, slot: Option<usize>) {
        for f in self.recent.iter_mut().filter(|f| f.end > start && f.start < end) {
            match slot {
                Some(k) => f.masked[k] = true,
                None => f.masked = [true; SLOTS],
            }
        }
    }

    /// Enough frames for the scores to mean something.
    pub fn ready(&self) -> bool {
        self.count.iter().all(|&c| c >= MIN_FRAMES)
    }

    /// Each third's score: lower is cleaner.
    pub fn scores(&self) -> [f32; SLOTS] {
        std::array::from_fn(|k| 100.0 * self.hit[k] + EXCESS_PCT_PER_DB * self.excess[k].max(0.0))
    }

    /// The third to use: the current one unless another beats it by
    /// [`HYSTERESIS`]; the centre until there is enough to go on.
    pub fn best(&mut self) -> usize {
        self.fold();
        if self.count.iter().any(|&c| c < MIN_FRAMES) {
            return self.current;
        }
        let s = self.scores();
        let k = (0..SLOTS).min_by(|&a, &b| s[a].total_cmp(&s[b])).unwrap_or(0);
        if s[k] + HYSTERESIS < s[self.current] {
            self.current = k;
        }
        self.current
    }

    fn frame(&mut self, start: u64) -> Frame {
        for i in 0..N {
            self.buf[i] = Complex::new(self.pending[i] * self.win[i], 0.0);
        }
        self.fft.forward(&self.buf, &mut self.spec);
        let hz = self.rate / N as f32;
        let bin = |f: f32| (f / hz).round() as usize;
        let db = |c: Complex| 10.0 * (c.norm_sqr() + 1e-20).log10();
        let lo = bin(self.center_hz + OFFSETS_HZ[1] - HALF_HZ);
        let hi = bin(self.center_hz + OFFSETS_HZ[2] + HALF_HZ);
        let mut all: Vec<f32> = self.spec[lo..=hi].iter().map(|&c| db(c)).collect();
        all.sort_by(f32::total_cmp);
        let floor = all[all.len() / 2];
        let mean = |a: f32, b: f32| {
            let s = &self.spec[bin(a)..=bin(b)];
            s.iter().map(|c| c.norm_sqr()).sum::<f32>() / s.len() as f32
        };
        let inner = SLICE_HZ / 2.0 - hz;
        let pw: [f32; SLOTS] = std::array::from_fn(|k| {
            let c = self.center_hz + OFFSETS_HZ[k];
            mean(c - HALF_HZ, c + HALF_HZ)
        });
        let pi: [f32; SLOTS] = std::array::from_fn(|k| {
            let c = self.center_hz + OFFSETS_HZ[k];
            mean(c - inner, c + inner)
        });
        self.power.push_back((start, pw, pi));
        let mut hit = [false; SLOTS];
        let mut excess_db = [0f32; SLOTS];
        for k in 0..SLOTS {
            let c = self.center_hz + OFFSETS_HZ[k];
            let mut v: Vec<f32> = self.spec[bin(c - HALF_HZ)..=bin(c + HALF_HZ)]
                .iter()
                .map(|&c| db(c))
                .collect();
            hit[k] = v.iter().any(|&d| d > floor + HIT_DB);
            v.sort_by(f32::total_cmp);
            excess_db[k] = v[v.len() / 2] - floor;
        }
        Frame {
            start,
            end: start + N as u64,
            hit,
            excess_db,
            masked: [false; SLOTS],
        }
    }

    /// QRM over the noise in each third, dB (at least [`QRM_FLOOR_DB`]), over
    /// samples `start..end`: a slice burst of ours in third `own`, heard at
    /// `snr_db` in 2.4 kHz. Its third's power is the signal and the noise,
    /// and the S/N splits the two; the other thirds hold the same noise and
    /// whatever QRM. NaN for `own`, and for all three over too short a
    /// stretch.
    pub fn qrm_db(&self, start: u64, end: u64, own: usize, snr_db: f32) -> [f32; SLOTS] {
        let mut sum = [0f64; SLOTS];
        let mut own_p = 0f64;
        let mut n = 0usize;
        for (s, pw, pi) in &self.power {
            if *s >= start && s + N as u64 <= end {
                for k in 0..SLOTS {
                    sum[k] += pw[k] as f64;
                }
                own_p += pi[own] as f64;
                n += 1;
            }
        }
        if n < QRM_MIN_FRAMES || !snr_db.is_finite() {
            return [f32::NAN; SLOTS];
        }
        // The S/N per Hz within the slice.
        let r = 10f64.powf(snr_db as f64 / 10.0) * 2400.0 / SLICE_HZ as f64;
        let noise = own_p / (1.0 + r);
        std::array::from_fn(|k| {
            if k == own {
                return f32::NAN;
            }
            let q = sum[k] / noise - 1.0;
            (10.0 * q.max(1e-9).log10()).max(QRM_FLOOR_DB as f64) as f32
        })
    }

    /// Averages in the frames old enough to be sure of.
    pub fn fold(&mut self) {
        let a = HOP as f32 / (MEMORY_S * self.rate);
        while let Some(f) = self.recent.front() {
            if f.end + self.lag > self.taken {
                break;
            }
            let f = self.recent.pop_front().unwrap();
            for k in 0..SLOTS {
                if f.masked[k] {
                    continue;
                }
                // A running mean until the memory is full, then exponential.
                self.count[k] += 1.0;
                let w = (1.0 / self.count[k]).max(a);
                self.hit[k] += w * (f.hit[k] as u8 as f32 - self.hit[k]);
                self.excess[k] += w * (f.excess_db[k] - self.excess[k]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ion_dsp::dsp::NoiseGen;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut g = NoiseGen::new(seed);
        (0..n).map(|_| 0.05 * g.gaussian()).collect()
    }

    #[test]
    fn white_noise_stays_at_the_centre() {
        let mut m = SlotMeter::new(1500.0, 48_000.0, 0.0);
        m.push(&noise(48_000 * 20, 1));
        assert_eq!(m.best(), 0, "{:?}", m.scores());
        assert!(m.scores().iter().all(|&s| s < 1.0), "{:?}", m.scores());
    }

    #[test]
    fn a_keyed_carrier_moves_it() {
        // A carrier at 1750 Hz (in the centre third), on half the time.
        let mut x = noise(48_000 * 20, 2);
        for (i, v) in x.iter_mut().enumerate() {
            if (i / 24_000) % 2 == 0 {
                *v += 0.05 * (core::f32::consts::TAU * 1750.0 * i as f32 / 48_000.0).sin();
            }
        }
        let mut m = SlotMeter::new(1500.0, 48_000.0, 0.0);
        m.push(&x);
        m.fold();
        let s = m.scores();
        assert!(s[0] > 30.0 && s[1] < 1.0 && s[2] < 1.0, "{s:?}");
        assert_ne!(m.best(), 0);
    }

    #[test]
    fn qrm_over_two_thirds_moves_it_to_the_third() {
        // A station over the low and middle thirds, 12 dB over the noise.
        let mut x = noise(48_000 * 20, 5);
        let mut q = ion_dsp::qrm::Qrm::new(300.0, 1850.0, ion_dsp::qrm::QrmKind::Steady, 6);
        q.add(&mut x, 0, 0.05 * 0.05 * 10f32.powf(1.2) * 2400.0 / 24_000.0);
        let mut m = SlotMeter::new(1500.0, 48_000.0, 0.0);
        m.push(&x);
        let best = m.best();
        assert_eq!(best, 2, "{:?}", m.scores());
    }

    /// Our slice in the third above at S/N `snr_db` in 2.4 kHz (13 tones,
    /// 50 Hz apart), and QRM `qrm_db` over the noise across the other two.
    fn slice_under_qrm(snr_db: f32, qrm_db: Option<f32>) -> SlotMeter {
        let sigma = 0.05f32;
        let mut x = noise(48_000 * 10, 7);
        // Signal over the noise in 2400 Hz: 13 a²/2 against σ² 2400/24 000.
        let a = (10f32.powf(snr_db / 10.0) * sigma * sigma * 0.1 * 2.0 / 13.0).sqrt();
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f32 / 48_000.0;
            for c in -6..=6 {
                let f = 2300.0 + 50.0 * c as f32;
                *v += a * (core::f32::consts::TAU * f * t + c as f32).sin();
            }
        }
        if let Some(q) = qrm_db {
            let mut g = ion_dsp::qrm::Qrm::new(300.0, 1850.0, ion_dsp::qrm::QrmKind::Steady, 8);
            g.add(&mut x, 0, sigma * sigma * 10f32.powf(q / 10.0) * 1550.0 / 24_000.0);
        }
        let mut m = SlotMeter::new(1500.0, 48_000.0, 0.0);
        m.push(&x);
        m
    }

    #[test]
    fn qrm_is_measured_against_the_noise_under_our_slice() {
        let n = 48_000 * 10;
        let m = slice_under_qrm(10.0, None);
        let q = m.qrm_db(0, n, 2, 10.0);
        assert!(q[2].is_nan() && q[0] < -8.0 && q[1] < -8.0, "{q:?}");
        for want in [0.0, 10.0] {
            let m = slice_under_qrm(10.0, Some(want));
            let q = m.qrm_db(0, n, 2, 10.0);
            assert!((q[0] - want).abs() < 2.0 && (q[1] - want).abs() < 2.0, "{want}: {q:?}");
        }
    }

    #[test]
    fn the_scores_take_qrm_under_our_slice_for_clean() {
        // Why qrm_db: QRM 10 dB over the noise, and the scores call it clean.
        let mut m = slice_under_qrm(16.0, Some(10.0));
        m.fold();
        let s = m.scores();
        assert!(s[0] < 4.0 && s[1] < 4.0, "{s:?}");
    }

    #[test]
    fn our_own_bursts_are_left_out() {
        // A strong signal over the centre third, but it was one of ours.
        let mut x = noise(48_000 * 20, 3);
        for (i, v) in x.iter_mut().enumerate().skip(48_000).take(48_000 * 8) {
            *v += 0.2 * (core::f32::consts::TAU * 1500.0 * i as f32 / 48_000.0).sin();
        }
        // Masked when it is decoded, at its end; then 20 s more to count.
        let mut m = SlotMeter::new(1500.0, 48_000.0, 10.0);
        m.push(&x[..48_000 * 9]);
        m.mask(48_000, 48_000 * 9, Some(0));
        m.push(&x[48_000 * 9..]);
        m.push(&noise(48_000 * 20, 4));
        assert_eq!(m.best(), 0, "{:?}", m.scores());
        assert!(m.scores()[0] < 1.0, "{:?}", m.scores());
    }
}
