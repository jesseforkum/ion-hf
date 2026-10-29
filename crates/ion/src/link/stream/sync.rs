//! The stream receiver's timekeeping (`docs/phy.md`, Stream timekeeping;
//! measured in `docs/experiments/stream-sync.md`): anchors from the periods
//! it decoded predict where the next one starts and at what carrier
//! offset ([`Track`]), and a search for the period's known head (its
//! preamble and header, which the receiver can synthesise) finds it in a
//! window around the prediction ([`locate`]). Times are audio samples:
//! the sender's schedule on one side, the receiver's own sample clock on
//! the other.

use std::collections::VecDeque;

use crate::link::audio::CENTER_HZ;
use ion_dsp::dsp::{AUDIO_RATE, Complex, Decimator, Nco, lowpass_taps};

const FS: f64 = AUDIO_RATE as f64;

/// Baseband rate of the search (48 kHz / 6).
pub const LOC_DEC: usize = 6;
const LOC_TAPS: usize = 97;
/// Segments of the known head summed non-coherently (each about 20 ms:
/// tolerant of a few Hz of residual offset and of fading).
const LOC_SEGS: usize = 8;

/// Complex baseband of `x` (centre `CENTER_HZ + f_hz`) at 8 kHz. The
/// reference and the received audio go through the same filter, so only
/// their relative lag matters.
pub fn baseband(x: &[f32], f_hz: f64) -> Vec<Complex> {
    let mut nco = Nco::new((CENTER_HZ as f64 + f_hz) as f32, AUDIO_RATE);
    let mut dec = Decimator::new(LOC_DEC, lowpass_taps(LOC_TAPS, 1500.0, AUDIO_RATE));
    let mut out = Vec::with_capacity(x.len() / LOC_DEC + 1);
    for &v in x {
        if let Some(y) = dec.push(nco.mix_down(v)) {
            out.push(y);
        }
    }
    out
}

/// What [`locate`] found.
#[derive(Clone, Copy, Debug, Default)]
pub struct Found {
    /// Where the head starts in the audio searched, samples (fractional).
    pub at: f64,
    /// Normalised correlation there (0..1).
    pub rho: f32,
    /// Lags searched.
    pub lags: usize,
}

/// Searches `audio` for the known head `refb` ([`baseband`] of the
/// waveform as sent, at `CENTER_HZ`) starting within `half` samples of
/// `center`, the received audio taken down at `CENTER_HZ + f_hz`.
/// Normalised non-coherent correlation over [`LOC_SEGS`] segments, best
/// lag, parabolic refinement.
pub fn locate(audio: &[f32], center: f64, half: f64, f_hz: f64, refb: &[Complex]) -> Option<Found> {
    let lo_a = (center - half).floor().max(0.0) as usize;
    let hi_a = ((center + half).ceil().max(0.0) as usize).min(audio.len());
    let span = refb.len() * LOC_DEC;
    let lo_a = lo_a - lo_a % LOC_DEC;
    if refb.len() < LOC_SEGS || lo_a >= hi_a || lo_a + span > audio.len() {
        return None;
    }
    let end = (hi_a + span + LOC_DEC).min(audio.len());
    let warm = (LOC_TAPS / LOC_DEC + 1) * LOC_DEC;
    let a0 = lo_a.saturating_sub(warm);
    let a0 = a0 - a0 % LOC_DEC;
    let y = baseband(&audio[a0..end], f_hz);
    let base = (lo_a - a0) / LOC_DEC;
    let n = refb.len();
    if y.len() < base + n + 1 {
        return None;
    }
    let lags = ((hi_a - lo_a) / LOC_DEC + 1).min(y.len() - base - n + 1);
    let mut pe = vec![0f64; y.len() + 1];
    for (i, v) in y.iter().enumerate() {
        pe[i + 1] = pe[i] + v.norm_sqr() as f64;
    }
    let seg = n / LOC_SEGS;
    let er: Vec<f64> = (0..LOC_SEGS)
        .map(|s| refb[s * seg..(s + 1) * seg].iter().map(|v| v.norm_sqr() as f64).sum())
        .collect();
    let mut rho = vec![0f32; lags];
    for (d, r) in rho.iter_mut().enumerate() {
        let o = base + d;
        let (mut num, mut den) = (0f64, 0f64);
        for (s, &e) in er.iter().enumerate() {
            let (mut re, mut im) = (0f64, 0f64);
            for k in s * seg..(s + 1) * seg {
                let p = y[o + k] * refb[k].conj();
                re += p.re as f64;
                im += p.im as f64;
            }
            num += re * re + im * im;
            den += e * (pe[o + (s + 1) * seg] - pe[o + s * seg]);
        }
        *r = (num / den.max(1e-30)) as f32;
    }
    let best = (0..lags).max_by(|&a, &b| rho[a].total_cmp(&rho[b]))?;
    let mut off = 0.0;
    if best > 0 && best + 1 < lags {
        let (a, b, c) = (rho[best - 1] as f64, rho[best] as f64, rho[best + 1] as f64);
        let den = a - 2.0 * b + c;
        if den < 0.0 {
            off = (0.5 * (a - c) / den).clamp(-0.5, 0.5);
        }
    }
    Some(Found {
        at: lo_a as f64 + (best as f64 + off) * LOC_DEC as f64,
        rho: rho[best],
        lags,
    })
}

/// Detection mark on [`locate`]'s peak for a window of `lags` lags: noise
/// alone tops it in about 1 window in 1000 (`stream-sync.md`, section 1:
/// the 99.9th percentile at six widths, linear in ln(lags) between them).
pub fn mark(lags: usize) -> f32 {
    const MARKS: [(f32, f32); 6] = [
        (3.892, 0.0528),
        (5.081, 0.0556),
        (6.176, 0.0601),
        (7.378, 0.0637),
        (8.477, 0.0669),
        (11.385, 0.0736),
    ];
    let x = (lags.max(1) as f32).ln();
    let mut prev = MARKS[0];
    if x <= prev.0 {
        return prev.1;
    }
    for &p in &MARKS[1..] {
        if x <= p.0 {
            return prev.1 + (p.1 - prev.1) * (x - prev.0) / (p.0 - prev.0);
        }
        prev = p;
    }
    prev.1
}

/// A prediction: where the period's head should start (receiver
/// samples), how far off it may be (half-window, samples), the carrier
/// offset (Hz).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pred {
    pub center: f64,
    pub half: f64,
    pub f_hz: f64,
}

/// Anchors kept for the clock fit, and those the offset is averaged over.
const FIT_N: usize = 16;
const OFF_N: usize = 8;
/// Half-window before three anchors, and its bounds, seconds. The first
/// is wide: the handshake's anchor carries both sound cards' latencies.
pub const W_INIT_S: f64 = 0.6;
const W_SEEDED_S: f64 = 0.25;
pub const W_MAX_S: f64 = 0.6;
const W_MIN_S: f64 = 0.003;

/// Anchors are (period's start on the sender's schedule, where the
/// receiver measured it on its own clock), from every period that
/// decoded a frame; the first from the handshake.
#[derive(Clone, Debug, Default)]
pub struct Track {
    anchors: VecDeque<(f64, f64)>,
    ppms: VecDeque<f64>,
    /// Carrier offset, Hz.
    pub f_hat: f64,
    f_n: usize,
    /// The first anchor is a guess from the handshake, not a measurement.
    guess: bool,
}

impl Track {
    pub fn new() -> Self {
        Self::default()
    }

    /// Anchors held (the handshake's guess included).
    pub fn anchors(&self) -> usize {
        self.anchors.len()
    }

    /// The handshake: the first period is expected at `rx` for schedule
    /// time `t`, give or take [`W_INIT_S`].
    pub fn seed(&mut self, t: f64, rx: f64) {
        self.anchors.clear();
        self.ppms.clear();
        self.anchors.push_back((t, rx));
        self.f_hat = 0.0;
        self.f_n = 0;
        self.guess = true;
    }

    /// A period confirmed by a decoded frame: its scheduled start, its
    /// measured start, the receiver's in-burst clock estimate (ppm, if it
    /// gave one) and carrier offset.
    pub fn update(&mut self, t: f64, rx: f64, ppm: Option<f64>, f_hz: f64) {
        if self.guess {
            self.anchors.clear();
            self.guess = false;
        }
        self.anchors.push_back((t, rx));
        while self.anchors.len() > FIT_N {
            self.anchors.pop_front();
        }
        if let Some(p) = ppm.filter(|p| p.abs() > 0.5) {
            self.ppms.push_back(p);
            while self.ppms.len() > FIT_N {
                self.ppms.pop_front();
            }
        }
        self.f_n += 1;
        let a = if self.f_n <= 2 { 0.5 } else { 0.3 };
        self.f_hat += a * (f_hz - self.f_hat);
    }

    /// Least squares of (rx − t) on t over the anchors: slope, its
    /// standard error, residual standard deviation.
    fn fit(&self) -> Option<(f64, f64, f64)> {
        let n = self.anchors.len();
        if n < 3 {
            return None;
        }
        let tm = self.anchors.iter().map(|a| a.0).sum::<f64>() / n as f64;
        let ym = self.anchors.iter().map(|a| a.1 - a.0).sum::<f64>() / n as f64;
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(t, r) in &self.anchors {
            sxx += (t - tm) * (t - tm);
            sxy += (t - tm) * (r - t - ym);
        }
        if sxx <= 0.0 {
            return None;
        }
        let b = sxy / sxx;
        let ss: f64 = self
            .anchors
            .iter()
            .map(|&(t, r)| {
                let e = r - t - ym - b * (t - tm);
                e * e
            })
            .sum();
        let s = (ss / (n - 2) as f64).sqrt();
        Some((b, s / sxx.sqrt(), s))
    }

    fn median_ppm(&self) -> Option<f64> {
        if self.ppms.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.ppms.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        Some(v[v.len() / 2])
    }

    /// Where the period scheduled at `t` should start.
    pub fn predict(&self, t: f64) -> Pred {
        let f_hz = self.f_hat;
        let Some(&(t_last, r_last)) = self.anchors.back() else {
            return Pred {
                center: t,
                half: W_MAX_S * FS,
                f_hz,
            };
        };
        if self.guess {
            return Pred {
                center: r_last + (t - t_last),
                half: W_INIT_S * FS,
                f_hz,
            };
        }
        let fit = self.fit();
        // Clock: the starts' slope when it is sharp (no jitter), else the
        // receivers' in-burst estimates.
        let (eps, eps_se) = match (fit, self.median_ppm()) {
            (Some((b, se, _)), _) if se < 3e-6 => (b, se),
            (Some((b, se, _)), None) => (b, se),
            (_, Some(p)) => (p * 1e-6, 15e-6),
            (None, None) => (0.0, 100e-6),
        };
        let dt = t - t_last;
        if fit.is_none() {
            return Pred {
                center: r_last + dt * (1.0 + eps),
                half: (W_SEEDED_S * FS + 3.0 * eps_se * dt).min(W_MAX_S * FS),
                f_hz,
            };
        }
        // Offset averaged over the latest anchors (jitter averages out;
        // the schedule is absolute, so a lost period loses nothing).
        let m = self.anchors.len().min(OFF_N);
        let offs: Vec<f64> = self
            .anchors
            .iter()
            .rev()
            .take(m)
            .map(|&(ta, ra)| ra - ta * (1.0 + eps))
            .collect();
        let off = offs.iter().sum::<f64>() / m as f64;
        let sd = (offs.iter().map(|o| (o - off) * (o - off)).sum::<f64>() / (m - 1) as f64).sqrt();
        let half = 4.0 * sd * (1.0 + 1.0 / m as f64).sqrt() + 3.0 * eps_se * dt + 0.001 * FS;
        Pred {
            center: t * (1.0 + eps) + off,
            half: half.clamp(W_MIN_S * FS, W_MAX_S * FS),
            f_hz,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_and_its_offset_are_tracked() {
        let mut t = Track::new();
        let p = 11.0 * FS;
        let eps = 80e-6;
        let off = 12_345.0;
        t.seed(0.0, off + 2_000.0);
        let first = t.predict(0.0);
        assert!((first.center - off).abs() <= first.half);
        for k in 0..12 {
            let tk = k as f64 * p;
            t.update(tk, off + tk * (1.0 + eps), Some(80.0), 3.0);
        }
        let q = t.predict(20.0 * p);
        let truth = off + 20.0 * p * (1.0 + eps);
        assert!((q.center - truth).abs() < 2.0, "{} vs {truth}", q.center);
        assert!(q.half <= 0.004 * FS, "{}", q.half);
        assert!((q.f_hz - 3.0).abs() < 0.1);
    }

    #[test]
    fn a_known_head_is_found_in_noise() {
        use ion_dsp::dsp::NoiseGen;
        let mut g = NoiseGen::new(4);
        let head: Vec<f32> = (0..7_872)
            .map(|i| {
                let t = i as f32 / FS as f32;
                0.3 * (core::f32::consts::TAU * (1500.0 + 300.0 * (t * 37.0).sin()) * t).sin()
                    + 0.2 * (core::f32::consts::TAU * 900.0 * t).sin() * ((i / 480) % 2) as f32
            })
            .collect();
        let refb = baseband(&head, 0.0);
        let at = 21_111usize;
        let mut audio: Vec<f32> = (0..48_000).map(|_| 0.2 * g.gaussian()).collect();
        for (x, h) in audio[at..].iter_mut().zip(&head) {
            *x += h;
        }
        let f = locate(&audio, 20_000.0, 6_000.0, 0.0, &refb).unwrap();
        assert!((f.at - at as f64).abs() < 12.0, "{f:?}");
        assert!(f.rho > mark(f.lags), "{f:?}");
    }
}
