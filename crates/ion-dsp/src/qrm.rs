//! Ham QRM for the simulator: another station's signal over part of the
//! passband, keyed on and off. Band-limited Gaussian noise, built a block
//! at a time in the frequency domain (sharp edges, so it stays out of the
//! thirds it does not cover), either steady (a data mode) or with a
//! syllabic envelope (SSB voice); or a recording of real signals, looped
//! ([`Qrm::recording`]).

use core::f64::consts::TAU;

use crate::dsp::{AUDIO_RATE, Complex, NoiseGen};
use crate::fft::Fft;

const N: usize = 4096;
const HOP: usize = N / 2;
/// Key-up and key-down ramps, seconds.
const RAMP_S: f64 = 0.02;
/// Syllabic rates of the voice envelope, Hz.
const SYLLABLES_HZ: [f64; 3] = [2.3, 3.7, 5.1];
/// Depth of the voice envelope (0: steady).
const VOICE_DEPTH: f64 = 0.9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrmKind {
    /// A data mode: steady power while keyed.
    Steady,
    /// SSB voice: the power comes and goes with the syllables.
    Voice,
}

/// One interfering station, as heard by one receiver.
pub struct Qrm {
    kind: QrmKind,
    /// (period, on), seconds; `None`: always on.
    keying: Option<(f64, f64)>,
    phase_s: f64,
    /// Nothing before this, seconds.
    from_s: f64,
    syllable_phase: [f64; 3],
    lo_bin: usize,
    hi_bin: usize,
    fft: Fft,
    win: Vec<f32>,
    scale: f32,
    noise: NoiseGen,
    /// Samples ready, the first of them audio sample `ready_at`.
    ready: Vec<f32>,
    ready_at: u64,
    tail: Vec<f32>,
    /// A recording, at unit mean power while it sounds, played in place of
    /// the noise.
    samples: Option<Vec<f32>>,
}

impl Qrm {
    /// Noise over `lo_hz..hi_hz` of the audio, at unit mean power while on.
    pub fn new(lo_hz: f32, hi_hz: f32, kind: QrmKind, seed: u64) -> Self {
        let hz = AUDIO_RATE / N as f32;
        let mut noise = NoiseGen::new(seed);
        let syllable_phase = std::array::from_fn(|_| TAU * noise.uniform() as f64);
        let win = (0..N)
            .map(|i| (core::f32::consts::PI * (i as f32 + 0.5) / N as f32).sin())
            .collect();
        let mut q = Self {
            kind,
            keying: None,
            phase_s: 0.0,
            from_s: f64::NEG_INFINITY,
            syllable_phase,
            lo_bin: (lo_hz / hz).round().max(1.0) as usize,
            hi_bin: (hi_hz / hz).round().min((N / 2 - 1) as f32) as usize,
            fft: Fft::new(N),
            win,
            scale: 1.0,
            noise,
            ready: Vec::new(),
            ready_at: 0,
            tail: vec![0.0; HOP],
            samples: None,
        };
        let probe: f64 = (0..32)
            .map(|_| q.block().iter().map(|&x| (x * x) as f64).sum::<f64>())
            .sum::<f64>()
            / (32 * HOP) as f64;
        q.scale = (1.0 / probe.max(1e-30)).sqrt() as f32;
        q
    }

    /// A recording (48 kHz), looped from audio sample 0. Its power is set
    /// by the 20 ms stretches within 30 dB of its loudest: the gaps of a
    /// station between overs do not count.
    pub fn recording(x: Vec<f32>) -> Self {
        let mut q = Self::new(300.0, 2700.0, QrmKind::Steady, 0);
        let block = AUDIO_RATE as usize / 50;
        let p: Vec<f64> = x
            .chunks(block)
            .map(|c| c.iter().map(|&v| (v * v) as f64).sum::<f64>() / c.len() as f64)
            .collect();
        let top = p.iter().cloned().fold(0.0, f64::max);
        let on: Vec<f64> = p.into_iter().filter(|&v| v > top * 1e-3).collect();
        let mean = on.iter().sum::<f64>() / on.len().max(1) as f64;
        let k = (1.0 / mean.max(1e-30)).sqrt() as f32;
        q.samples = Some(x.into_iter().map(|v| v * k).collect());
        q
    }

    /// Keyed: on for `on_s` of every `period_s`, starting `phase_s` into
    /// the period at sample 0.
    pub fn keyed(mut self, period_s: f64, on_s: f64, phase_s: f64) -> Self {
        self.keying = Some((period_s, on_s));
        self.phase_s = phase_s;
        self
    }

    /// Silent before `from_s` seconds.
    pub fn from(mut self, from_s: f64) -> Self {
        self.from_s = from_s;
        self
    }

    /// Amplitude of the key and the syllables at audio sample `t`.
    fn envelope(&self, t: u64) -> f32 {
        let s = t as f64 / AUDIO_RATE as f64;
        if s < self.from_s {
            return 0.0;
        }
        let key = match self.keying {
            None => 1.0,
            Some((period, on)) => {
                let u = (s + self.phase_s).rem_euclid(period);
                if u >= on {
                    0.0
                } else {
                    let edge = u.min(on - u) / RAMP_S;
                    if edge >= 1.0 {
                        1.0
                    } else {
                        0.5 - 0.5 * (core::f64::consts::PI * edge).cos()
                    }
                }
            }
        };
        let voice = match self.kind {
            QrmKind::Steady => 1.0,
            QrmKind::Voice => {
                let m: f64 = SYLLABLES_HZ
                    .iter()
                    .zip(self.syllable_phase)
                    .map(|(f, p)| (TAU * f * s + p).sin())
                    .sum::<f64>()
                    / SYLLABLES_HZ.len() as f64;
                // Mean square 1 + depth² / 6 over the three sines.
                (1.0 + VOICE_DEPTH * m) / (1.0 + VOICE_DEPTH * VOICE_DEPTH / 6.0).sqrt()
            }
        };
        (key * voice) as f32
    }

    /// One windowed block of band noise.
    fn block(&mut self) -> Vec<f32> {
        let mut spec = vec![Complex::ZERO; N];
        for b in &mut spec[self.lo_bin..=self.hi_bin] {
            *b = Complex::new(self.noise.gaussian(), self.noise.gaussian());
        }
        let mut out = vec![Complex::ZERO; N];
        self.fft.inverse(&spec, &mut out);
        out.iter().zip(&self.win).map(|(c, w)| c.re * w * self.scale).collect()
    }

    fn produce(&mut self) {
        let b = self.block();
        self.ready.extend(self.tail.iter().zip(&b[..HOP]).map(|(t, v)| t + v));
        self.tail.copy_from_slice(&b[HOP..]);
    }

    /// Adds the QRM to `x`, whose first sample is audio sample `at`, at
    /// mean power `power` while it is on. Calls go forward in time; a jump
    /// ahead starts the noise afresh there.
    pub fn add(&mut self, x: &mut [f32], at: u64, power: f32) {
        let amp = power.max(0.0).sqrt();
        if let Some(r) = &self.samples {
            for (i, v) in x.iter_mut().enumerate() {
                let t = at + i as u64;
                *v += amp * self.envelope(t) * r[(t % r.len() as u64) as usize];
            }
            return;
        }
        let end = self.ready_at + self.ready.len() as u64;
        if at < self.ready_at || at > end {
            self.ready.clear();
            self.ready_at = at;
            self.tail.iter_mut().for_each(|v| *v = 0.0);
            self.produce();
            self.ready.clear();
        } else {
            let drop = (at - self.ready_at) as usize;
            self.ready.drain(..drop);
            self.ready_at = at;
        }
        while self.ready.len() < x.len() {
            self.produce();
        }
        for (i, v) in x.iter_mut().enumerate() {
            *v += amp * self.envelope(at + i as u64) * self.ready[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band_power(x: &[f32], lo_hz: f32, hi_hz: f32) -> f32 {
        let n = 8192;
        let fft = Fft::new(n);
        let hz = AUDIO_RATE / n as f32;
        let mut p = 0f64;
        let mut total = 0f64;
        for c in x.chunks_exact(n) {
            let input: Vec<Complex> = c
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let w = 0.5 - 0.5 * (core::f32::consts::TAU * i as f32 / n as f32).cos();
                    Complex::new(v * w, 0.0)
                })
                .collect();
            let mut out = vec![Complex::ZERO; n];
            fft.forward(&input, &mut out);
            for (k, o) in out[..n / 2].iter().enumerate() {
                let e = o.norm_sqr() as f64;
                total += e;
                let f = k as f32 * hz;
                if f >= lo_hz && f <= hi_hz {
                    p += e;
                }
            }
        }
        (p / total.max(1e-30)) as f32
    }

    #[test]
    fn unit_power_inside_its_band() {
        let mut q = Qrm::new(400.0, 1800.0, QrmKind::Steady, 7);
        let mut x = vec![0f32; 48_000 * 10];
        q.add(&mut x, 0, 1.0);
        let p = x.iter().map(|v| (v * v) as f64).sum::<f64>() / x.len() as f64;
        assert!((p - 1.0).abs() < 0.05, "{p}");
        assert!(band_power(&x, 380.0, 1820.0) > 0.999);
        // Nothing worth the name in the top third (1975–2625 Hz).
        assert!(band_power(&x, 1975.0, 2625.0) < 1e-4);
    }

    #[test]
    fn a_recording_plays_at_its_power_while_it_sounds() {
        // A tone 1 s on, 1 s off: power 1 while on.
        let x: Vec<f32> = (0..96_000)
            .map(|i| {
                if i < 48_000 {
                    (TAU * 1000.0 * i as f64 / 48_000.0).sin() as f32
                } else {
                    0.0
                }
            })
            .collect();
        let mut q = Qrm::recording(x);
        let mut y = vec![0f32; 96_000 * 2];
        q.add(&mut y[..50_000], 0, 4.0);
        q.add(&mut y[50_000..], 50_000, 4.0);
        let pw = |a: usize, b: usize| y[a..b].iter().map(|v| (v * v) as f64).sum::<f64>() / (b - a) as f64;
        assert!((pw(0, 48_000) - 4.0).abs() < 0.05, "{}", pw(0, 48_000));
        assert!(pw(48_000, 96_000) < 1e-9);
        assert!((pw(96_000, 144_000) - 4.0).abs() < 0.05);
    }

    #[test]
    fn keyed_and_continuous_across_calls() {
        let mut q = Qrm::new(400.0, 1800.0, QrmKind::Voice, 8).keyed(10.0, 4.0, 0.0);
        let mut x = vec![0f32; 48_000 * 20];
        for (k, c) in x.chunks_mut(12_345).enumerate() {
            q.add(c, (k * 12_345) as u64, 1.0);
        }
        let pw = |a: f64, b: f64| {
            let s = &x[(a * 48_000.0) as usize..(b * 48_000.0) as usize];
            s.iter().map(|v| (v * v) as f64).sum::<f64>() / s.len() as f64
        };
        assert!((pw(0.1, 3.9) - 1.0).abs() < 0.25, "{}", pw(0.1, 3.9));
        assert!(pw(4.1, 9.9) < 1e-9);
        assert!((pw(10.1, 13.9) - 1.0).abs() < 0.25);
    }
}
