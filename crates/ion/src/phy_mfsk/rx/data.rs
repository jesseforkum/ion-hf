//! Data symbols of a burst whose header has decoded: tone metrics and
//! LLRs, frequency and sample-clock tracking, channel measurement.

use core::f32::consts::TAU;

use ion_dsp::dsp::Complex;

use super::model::{local_noise, metric};
use super::{Active, EXT, LLR_MAX, MARGIN, MfskRx, NE, gi};
use crate::chan::{self, ChannelMeas};
use crate::phy_mfsk::frame::{self, Fmt};
use crate::phy_mfsk::util::lse;
use crate::phy_mfsk::{BB_RATE, MfskBurst, MfskMode, SYM};

/// Data symbols per frequency-tracking update.
const BLOCK: usize = 16;
/// Tone metrics kept for iterative demapping are at most this far below
/// the symbol's best (nats).
const METRIC_FLOOR: f32 = 40.0;
/// A data symbol whose analysis window the blanker zeroed more than this
/// is erased (every tone value equally likely).
const BLANK_ERASE: f32 = 0.5;
/// Early-late offset of the drift gate, baseband samples (2 ms): the gate
/// is linear for misalignments up to that, which leaves room for the
/// constant bias of a long echo.
const TD: usize = 16;
/// R0 data symbols per drift sample (0.9 s; other modes scale it to the
/// same time).
const TBLOCK: usize = 32;

impl MfskRx {
    /// Returns true when the burst has been delivered.
    pub(super) fn service_burst(&mut self, b: &mut Active, out: &mut Vec<MfskBurst>) -> bool {
        let g = &self.grids[gi(b.mode)];
        let fmt = g.fmt;
        let now = self.n_bb as i64;
        while b.next < b.dsyms {
            let end = b.t0c + g.sym_end(b.hsyms + b.next) + b.d as i64 + b.shift as i64;
            if now < end + MARGIN {
                break;
            }
            let start = end - b.w as i64;
            let mut x = [0f32; NE];
            if self.available(start, b.w) {
                self.energies(g, start, b.w, b.f, &mut x);
            }
            // Decision-directed frequency discriminator: phase advance
            // between the two halves of the window at the likeliest tones.
            let h = fmt.hop(b.hsyms + b.next);
            let tm = b.params.terms(&x);
            for t in 0..fmt.tones {
                let mut bestv = (f32::MIN, 0usize);
                for v in 0..fmt.vals() {
                    let l = metric(&tm, fmt.tone_bin(h, t, v) + EXT);
                    if l > bestv.0 {
                        bestv = (l, v);
                    }
                }
                let fb = b.f + fmt.bin_hz(fmt.tone_bin(h, t, bestv.1) as i32);
                // A window as long as the symbol holds the frequency sweeps
                // at both ends; leave them out.
                let trim = disc_trim(fmt, b.w);
                let half = b.w / 2 - trim;
                if self.available(start, b.w) {
                    let s1 = start + trim as i64;
                    let c1 = self.corr1(s1, half, fb);
                    // Same phase reference: the second half starts `half`
                    // samples later.
                    let rot = Complex::from_polar(1.0, -TAU * fb * half as f32 / BB_RATE);
                    let c2 = self.corr1(s1 + half as i64, half, fb) * rot;
                    b.acc += c2 * c1.conj();
                    b.mag += c1.norm() * c2.norm();
                    let e = fmt.tone_bin(h, t, bestv.1) + EXT;
                    if t == 0 {
                        b.coh += c2 * c1.conj();
                        b.coh_p[0] += c1.norm_sqr();
                        b.coh_p[1] += c2.norm_sqr();
                        b.coh_n += b.params.noise[e] * half as f32 / b.w as f32;
                        // Echo: the previous tone's energy in the windows
                        // that start before the current symbol's last
                        // `b.w` samples.
                        let wend = start + b.w as i64;
                        if let Some(pe) = b.prev_e.filter(|&pe| pe.abs_diff(e) >= 3) {
                            let fp = b.f + fmt.bin_hz(pe as i32 - EXT as i32);
                            for (k, ww) in [fmt.sym, fmt.sym - 32].into_iter().enumerate() {
                                let s0 = wend - ww as i64;
                                if !self.available(s0, ww) {
                                    continue;
                                }
                                let sc = ww as f32 / b.w as f32;
                                let ep = self.corr1(s0, ww, fp).norm_sqr() - b.params.noise[pe] * sc;
                                let ec = self.corr1(s0, ww, fb).norm_sqr() - b.params.noise[e] * sc;
                                b.isi[k][0] += ep;
                                b.isi[k][1] += ec;
                            }
                        }
                        b.prev_e = Some(e);
                    }
                }
            }
            b.energies.push(x);
            b.blanked.push(self.blanked_frac(start, b.w));
            b.shifts.push(b.shift);
            b.f_sum += b.f as f64;
            b.next += 1;
            if self.track_drift {
                self.track_timing(b);
            }
            if b.next.is_multiple_of(BLOCK) {
                if b.acc.norm() > 0.3 * b.mag {
                    let half = b.w / 2 - disc_trim(fmt, b.w);
                    let df = b.acc.arg() * BB_RATE / (TAU * half as f32);
                    b.f += 0.3 * df;
                }
                b.acc = Complex::ZERO;
                b.mag = 0.0;
            }
        }
        if b.next < b.dsyms {
            return false;
        }
        let mut metrics = Vec::new();
        // Tone energies of the decided values, noise units less one (the
        // SNR of each symbol): their spread is the fading.
        let (mut f1, mut f2, mut fnum) = (0f64, 0f64, 0f64);
        let llrs = if b.dsyms == 0 {
            Vec::new()
        } else {
            let mut slot = vec![0f32; b.dsyms * fmt.bps()];
            let vals = fmt.vals();
            let mut lam = vec![0f32; vals];
            metrics.reserve(b.dsyms * fmt.tones * vals);
            let local = local_noise(&b.energies, &b.params, fmt, b.hsyms);
            for (i, (x, k)) in b.energies.iter().zip(&local).enumerate() {
                let h = fmt.hop(b.hsyms + i);
                let tm = b.params.terms_local(x, k);
                // Mostly blanked: erased.
                let erased = b.blanked[i] > BLANK_ERASE;
                for t in 0..fmt.tones {
                    for (v, l) in lam.iter_mut().enumerate() {
                        *l = if erased {
                            0.0
                        } else {
                            metric(&tm, fmt.tone_bin(h, t, v) + EXT)
                        };
                    }
                    let mx = lam.iter().fold(f32::MIN, |m, &l| m.max(l));
                    metrics.extend(lam.iter().map(|&l| (l - mx).max(-METRIC_FLOOR)));
                    if let Some(vb) = lam.iter().position(|&l| l == mx) {
                        let e = fmt.tone_bin(h, t, vb) + EXT;
                        let gk = (x[e] / b.params.noise[e]) as f64 - 1.0;
                        f1 += gk;
                        f2 += gk * gk;
                        fnum += 1.0;
                    }
                    for k in 0..fmt.bpt {
                        let (mut l0, mut l1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
                        for (v, &l) in lam.iter().enumerate() {
                            if (frame::gray(v) >> k) & 1 == 0 {
                                l0 = lse(l0, l);
                            } else {
                                l1 = lse(l1, l);
                            }
                        }
                        slot[i * fmt.bps() + t * fmt.bpt + k] = (l0 - l1).clamp(-LLR_MAX, LLR_MAX);
                    }
                }
            }
            if b.symbols {
                // Code-symbol order and values; bit LLRs of each symbol's
                // natural-binary label (MSB first).
                metrics = frame::deinterleave_sym(&metrics);
                let mut out = Vec::with_capacity(b.nbits);
                for m in metrics.chunks_exact(vals) {
                    for k in (0..fmt.bpt).rev() {
                        let (mut l0, mut l1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
                        for (v, &l) in m.iter().enumerate() {
                            if (v >> k) & 1 == 0 {
                                l0 = lse(l0, l);
                            } else {
                                l1 = lse(l1, l);
                            }
                        }
                        out.push((l0 - l1).clamp(-LLR_MAX, LLR_MAX));
                    }
                }
                out.truncate(b.nbits);
                out
            } else {
                frame::deinterleave(&slot, b.nbits)
            }
        };
        let freq_offset_hz = if b.dsyms == 0 {
            b.f
        } else {
            (b.f_sum / b.dsyms as f64) as f32
        };
        self.stats.last_window = b.w;
        self.stats.last_a = b.params.a;
        self.stats.last_s = b.params.s;
        let k = self.layout.k as f32;
        let chan = if b.dsyms == 0 {
            ChannelMeas::of_snr(b.snr_db)
        } else {
            let mut m = measure(b, fmt.sym, b.w / 2 - disc_trim(fmt, b.w), (f1, f2, fnum));
            m.delay_ms *= k;
            m.doppler_hz /= k;
            m
        };
        out.push(MfskBurst {
            mode: b.mode,
            user: b.user,
            extra: b.extra,
            llrs,
            metrics,
            symbols: b.symbols,
            snr_db: b.snr_db,
            freq_offset_hz: freq_offset_hz / k,
            start_sample: b.start_sample,
            clock_ppm: (b.drift / fmt.sym as f64 * 1e6) as f32,
            chan,
            cue: b.cue,
        });
        true
    }

    /// Sample-clock drift: an early-late gate on the decided tones of data
    /// symbol `next − 3` (all its samples have arrived), with the
    /// whole-symbol window shifted by ±`TD` around the timing reference.
    /// For a tone held over the symbol the energy difference is linear in
    /// the misalignment `δ` (`E₊ − E₋ ≈ 4(W − TD)·δ` against a mean of
    /// `(W − TD)²`); the noise expected in the window is taken out of the
    /// mean. Each block of symbols (0.9 s) gives one drift sample; a
    /// least-squares line through them gives the drift per symbol, and the
    /// windows of the following symbols are shifted along it, one sample
    /// at a time. Echoes and the frequency sweep at each symbol start bias
    /// the gate by a constant, which the line's intercept absorbs.
    fn track_timing(&self, b: &mut Active) {
        if b.next < 3 {
            return;
        }
        let g = &self.grids[gi(b.mode)];
        let fmt = g.fmt;
        let tblock = TBLOCK * SYM / fmt.sym;
        let i = b.next - 3;
        let w = fmt.sym;
        let end = b.t0c + g.sym_end(b.hsyms + i) + b.dref as i64 + b.shifts[i] as i64;
        let start = end - w as i64;
        if self.available(start - TD as i64, w + 2 * TD) {
            let h = fmt.hop(b.hsyms + i);
            let tm = b.params.terms(&b.energies[i]);
            for t in 0..fmt.tones {
                let mut bestv = (f32::MIN, 0usize);
                for v in 0..fmt.vals() {
                    let l = metric(&tm, fmt.tone_bin(h, t, v) + EXT);
                    if l > bestv.0 {
                        bestv = (l, v);
                    }
                }
                let bin = fmt.tone_bin(h, t, bestv.1);
                let fb = b.f + fmt.bin_hz(bin as i32);
                let ep = self.corr1(start + TD as i64, w, fb).norm_sqr() as f64;
                let em = self.corr1(start - TD as i64, w, fb).norm_sqr() as f64;
                let noise = b.params.noise[bin + EXT] as f64 * w as f64 / b.w as f64;
                b.tnum += ep - em;
                b.tden += (0.5 * (ep + em) - noise).max(0.0);
            }
        }
        b.tcnt += 1;
        if b.tcnt == tblock {
            if b.tden > 0.0 {
                let mis = ((w - TD) as f64 / 4.0 * b.tnum / b.tden).clamp(-2.0 * TD as f64, 2.0 * TD as f64);
                let s0 = i + 1 - tblock;
                let ms = b.shifts[s0..=i].iter().sum::<i32>() as f64 / tblock as f64;
                b.blocks.push(((s0 + i) as f64 / 2.0, ms + mis));
                b.drift = fit_drift(&b.blocks, w);
            }
            b.tnum = 0.0;
            b.tden = 0.0;
            b.tcnt = 0;
        }
        let target = b.drift * b.next as f64;
        if target > b.shift as f64 + 0.7 {
            b.shift += 1;
        } else if target < b.shift as f64 - 0.7 {
            b.shift -= 1;
        }
    }
}

/// Per-cell SNR of full-band OFDM (55 carriers, 50 Hz) over the SNR in
/// 2.4 kHz: what the MFSK floor's fading measurement is mapped onto.
const OFDM_CELL_SCALE: f32 = 2400.0 / (55.0 * 50.0);

/// Share of the current tone's energy that the previous symbol's tone
/// leaves in the window starting 4 ms into the symbol with no echo past
/// 4 ms: the sweep between tones, timing error and noise.
const ISI_FLOOR: f32 = 0.005;

/// The channel over a burst's data symbols (see [`crate::chan`]):
/// Doppler from the coherence of the two halves of each decided tone,
/// echoes from the previous tone's energy in the window that starts 4 ms
/// into the symbol, fading (and so the effective SNR) from the spread of
/// the decided tones' energies. `sym`: symbol length, samples; `half`:
/// spacing of the two halves, samples; `fade`: Σ, Σ² and count of the
/// decided tones' SNRs.
///
/// Delay and Doppler for R0 only (NaN for R1): R1's 14 ms symbols have no
/// guard, so echoes reach both halves of every tone and spoil its
/// coherence.
fn measure(b: &Active, sym: usize, half: usize, fade: (f64, f64, f64)) -> ChannelMeas {
    let half = half.max(1) as f32;
    let p1 = b.coh_p[0] - b.coh_n;
    let p2 = b.coh_p[1] - b.coh_n;
    let rho = if p1 > 0.0 && p2 > 0.0 {
        (b.coh.norm() / (p1 * p2).sqrt()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let doppler_hz = chan::doppler_from_rho(rho.max(0.05), half / BB_RATE);
    let r = |k: usize| {
        if b.isi[k][1] > 0.0 {
            (b.isi[k][0] / b.isi[k][1]).max(0.0)
        } else {
            0.0
        }
    };
    let (r_full, r_mid) = (r(0), r(1));
    // An echo of relative power q and delay τ > 4 ms fills the first
    // τ − 4 ms of the window starting 4 ms in (W = sym − 4 ms long) with
    // the previous tone: q((τ − 4)/W)² of the energy. Over the floor, τ
    // for q = 1/2; under it, a value below 4 ms that only says "no echo
    // past the guard" (scaled so the smoothing in `link::report` moves
    // smoothly).
    let t_ms = sym as f32 / BB_RATE * 1e3;
    let delay_ms = if r_mid > ISI_FLOOR {
        4.0 + (t_ms - 4.0) * (2.0 * (r_mid - ISI_FLOOR)).sqrt()
    } else {
        4.0 * r_mid / ISI_FLOOR
    };
    let r0 = b.mode == MfskMode::R0;
    let (s1, s2, n) = fade;
    let af = if n >= 8.0 {
        let m = s1 / n;
        let var = s2 / n - m * m;
        if m > 0.0 {
            ((var - (1.0 + 2.0 * m)).max(0.0) / (m * m)).min(2.0) as f32
        } else {
            0.0
        }
    } else {
        0.0
    };
    ChannelMeas {
        delay_ms: if r0 { delay_ms.min(20.0) } else { f32::NAN },
        doppler_hz: if r0 { doppler_hz } else { f32::NAN },
        eff_snr_db: chan::nakagami_eff_db(b.snr_db, af, OFDM_CELL_SCALE),
        raw: [rho, r_full, r_mid, af],
    }
}

/// Samples the frequency discriminator leaves out at each end of window
/// `w`: 1 ms when the window spans the whole symbol of a guard-less mode
/// (R0's guard keeps the sweeps out of its 20 ms window, and its 28 ms
/// window halves are long enough not to care).
fn disc_trim(fmt: Fmt, w: usize) -> usize {
    if fmt.win == fmt.sym && w + 16 > fmt.sym { 8 } else { 0 }
}

/// Drift per symbol (of `sym` samples) from the block samples `(symbol,
/// drift)`: the least-squares slope, shrunk towards 0 by its uncertainty
/// (×4: the samples are not independent on fading channels) and by a prior
/// of ±100 ppm, from 6 blocks (5.4 s) on, so that a short or noisy burst is
/// left alone. Drift beyond 300 ppm is not believed.
fn fit_drift(blocks: &[(f64, f64)], sym: usize) -> f64 {
    let n = blocks.len() as f64;
    if blocks.len() < 6 {
        return 0.0;
    }
    let xm = blocks.iter().map(|b| b.0).sum::<f64>() / n;
    let ym = blocks.iter().map(|b| b.1).sum::<f64>() / n;
    let sxx: f64 = blocks.iter().map(|b| (b.0 - xm) * (b.0 - xm)).sum();
    let sxy: f64 = blocks.iter().map(|b| (b.0 - xm) * (b.1 - ym)).sum();
    if sxx <= 0.0 {
        return 0.0;
    }
    let s = sxy / sxx;
    let res: f64 = blocks.iter().map(|b| (b.1 - ym - s * (b.0 - xm)).powi(2)).sum::<f64>() / (n - 2.0).max(1.0);
    let var = 4.0 * res / sxx;
    let prior = (100e-6 * sym as f64).powi(2);
    let s = s * prior / (prior + var);
    let max = 300e-6 * sym as f64;
    (s * s * s / (s * s + 4.0 * var).max(1e-30)).clamp(-max, max)
}
