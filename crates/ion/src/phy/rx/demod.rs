//! The data stage of a burst: phase tracking, channel estimation, noise,
//! LLRs, the SNR report and the channel measurement.

use std::sync::Arc;

use ion_dsp::dsp::Complex;

use crate::chan::{self, ChannelMeas, Eesm};
use crate::phy::est;
use crate::phy::frame::*;
use crate::phy::tx::{td_clipped, tx_distortion_db};
use crate::phy::{Constellation, DataMode};

use super::{BLANK_ERASE, BurstGrid, CellDebug, PhyRx, RxBurst, known};

/// Data cells in each window of the receiver's local noise estimate.
const LOCAL_CELLS: usize = 48;
/// Share of the delay profile's power the reported delay span holds.
const PDP_FRAC: f64 = 0.9;
/// Echoes past a 4 ms guard (Normal, Slice) hide from the decision-directed
/// delay profile (the decisions follow the channel estimate, whose model
/// spans at most the guard) and show as noise instead: the
/// decision-directed noise floor above the preamble's. From this excess,
/// dB, with no fast fading to explain it, the echo counts as long.
const ISI_EXCESS_DB: f32 = 2.0;
/// The delay reported for such an echo, ms: past the guard.
const ISI_DELAY_MS: f32 = 6.0;

impl PhyRx {
    /// The single-path delay span through the taper.
    pub(super) fn with_span0(mut self) -> Self {
        let flat = vec![Complex::new(1.0, 0.0); self.l.nc];
        let mut p = vec![0f64; self.l.n];
        self.pdp_row(&flat, &mut p);
        let peak = p.iter().cloned().fold(0.0, f64::max);
        for v in &mut p {
            *v = (*v - 1e-4 * peak).max(0.0);
        }
        self.span0 = chan::span_bins(&p, PDP_FRAC);
        self
    }

    /// Adds the power delay profile of one symbol's channel `row` (one
    /// value per carrier) to `out` (`n` delay bins of one sample).
    fn pdp_row(&self, row: &[Complex], out: &mut [f64]) {
        let l = self.l;
        let n = l.n;
        for (k, o) in out.iter_mut().enumerate() {
            let mut acc = Complex::ZERO;
            for (c, z) in row.iter().enumerate() {
                let t = self.tw[(l.bin_mod(c, n) * k) % n];
                acc += (*z * t).scale(self.hann[c]);
            }
            *o += acc.norm_sqr() as f64;
        }
    }

    /// Delay and Doppler spread from the decision-directed channel `z` of
    /// the data symbols (`nsym` x `nc`, pilots exact), and the effective
    /// SNR from the demapper's cells.
    fn measure(&self, z: &[Complex], nsym: usize, snr_db: f32, eesm: &Eesm, excess_db: f32) -> ChannelMeas {
        let l = self.l;
        let (nc, n, pre) = (l.nc, l.n, l.pre_syms());
        let ts = l.sym() as f32 / FS as f32;
        // Delay: the profile over all data symbols, its floor (a quantile:
        // the echoes take less than half the bins) taken off.
        let mut p = vec![0f64; n];
        for row in z.chunks_exact(nc).take(nsym) {
            self.pdp_row(row, &mut p);
        }
        let floor = chan::quantile(&p, 0.25);
        for v in &mut p {
            *v = (*v - floor).max(0.0);
        }
        let span = chan::span_bins(&p, PDP_FRAC);
        // A span past the guard is noise: the decision-directed channel
        // cannot hold echoes beyond it (they show as ISI instead).
        let delay_ms = if span > l.cp + self.span0 {
            f32::NAN
        } else {
            (span.saturating_sub(self.span0)) as f32 / FS as f32 * 1e3
        };
        // Doppler: pilot pairs one pattern period apart, and the continual
        // pilot (the top carrier) one symbol apart; the noise of each pilot
        // (the preamble's estimate, which leaves out what the estimator
        // misses) taken out of the power.
        let map = self.map;
        let per = map.pattern(self.ng - 1).period;
        let corr = |lag: usize, only_top: bool| -> f32 {
            let (mut num, mut den) = (Complex::ZERO, 0f64);
            for k in 0..nsym.saturating_sub(lag) {
                for c in 0..nc {
                    if only_top && c != nc - 1 {
                        continue;
                    }
                    let (Some(p0), Some(p1)) = (known(&map, c, pre + k), known(&map, c, pre + k + lag)) else {
                        continue;
                    };
                    let (a, b) = (z[k * nc + c], z[(k + lag) * nc + c]);
                    num += a * b.conj();
                    let nv = self.sigma2[c] as f64 * 0.5 * (1.0 / p0.norm_sqr() as f64 + 1.0 / p1.norm_sqr() as f64);
                    den += 0.5 * (a.norm_sqr() + b.norm_sqr()) as f64 - nv;
                }
            }
            if den <= 0.0 {
                return 0.0;
            }
            (num.norm() as f64 / den).clamp(0.0, 1.0) as f32
        };
        let rho_p = corr(per, false);
        let rho_1 = corr(1, true);
        let doppler_hz = if rho_p >= 0.3 {
            chan::doppler_from_rho(rho_p, per as f32 * ts)
        } else {
            chan::doppler_from_rho(rho_1.max(0.05), ts)
        };
        let short_guard = (l.cp as f32 / FS as f32) < 0.005;
        let delay_ms = if short_guard && excess_db > ISI_EXCESS_DB && doppler_hz < chan::DOPPLER_HZ {
            delay_ms.max(ISI_DELAY_MS)
        } else {
            delay_ms
        };
        ChannelMeas {
            delay_ms,
            doppler_hz,
            eff_snr_db: eesm.eff_db(snr_db),
            raw: [rho_p, rho_1, span as f32 / FS as f32 * 1e3, excess_db],
        }
    }

    /// Frequency trajectory over the whole burst from pilot pairs one
    /// pattern period apart (offset + drift), applied as a phase
    /// correction.
    fn track_phase(&mut self) {
        let l = self.l;
        let nc = l.nc;
        let ng = self.ng;
        let map = self.map;
        let ts = l.sym() as f64 / FS;
        const BLOCK: usize = 8;
        let lag = map.pattern(ng - 1).period;
        let nb = (ng.saturating_sub(lag)).div_ceil(BLOCK);
        let (mut sw, mut swx, mut swy, mut swxx, mut swxy) = (0f64, 0f64, 0f64, 0f64, 0f64);
        // Pairs alias every 1/(lag·Ts); blocks are unwrapped against the
        // previous one so a slow drift can exceed half of that.
        let alias = 1.0 / (lag as f64 * ts);
        let mut prev: Option<f64> = None;
        for b in 0..nb {
            let mut q = Complex::ZERO;
            for g in b * BLOCK..((b + 1) * BLOCK).min(ng - lag) {
                for c in 0..nc {
                    let (Some(x0), Some(x1)) = (known(&map, c, g), known(&map, c, g + lag)) else {
                        continue;
                    };
                    let h0 = self.grid[g * nc + c] * x0.conj();
                    let h1 = self.grid[(g + lag) * nc + c] * x1.conj();
                    q += h1 * h0.conj();
                }
            }
            let wgt = q.norm() as f64;
            if wgt <= 0.0 {
                continue;
            }
            let mut f = q.arg() as f64 / (core::f64::consts::TAU * lag as f64 * ts);
            if let Some(p) = prev {
                f += alias * ((p - f) / alias).round();
            }
            prev = Some(f);
            let x = (b * BLOCK) as f64 + BLOCK as f64 / 2.0 + lag as f64 / 2.0;
            sw += wgt;
            swx += wgt * x;
            swy += wgt * f;
            swxx += wgt * x * x;
            swxy += wgt * x * f;
        }
        if sw <= 0.0 {
            return;
        }
        let xm = swx / sw;
        let f0 = swy / sw;
        let var = swxx / sw - xm * xm;
        // Fit a slope only over long bursts (2 s or more).
        let f1 = if nb >= 3 && ng as f64 * ts >= 2.0 && var > 0.0 {
            (swxy / sw - xm * f0) / var
        } else {
            0.0
        };
        self.df_res += f0;
        for g in 1..ng {
            let gf = g as f64;
            let th = core::f64::consts::TAU * ts * (f0 * gf + f1 * (gf * gf / 2.0 - xm * gf));
            let r = Complex::from_polar(1.0, -th as f32);
            for c in 0..nc {
                self.grid[g * nc + c] = self.grid[g * nc + c] * r;
            }
        }
    }

    /// SNR in 2.4 kHz from grid symbols `g0..g1`: signal power summed over
    /// the carriers against the noise in 2400 Hz worth of bins.
    fn snr_db(&self, g0: usize, g1: usize, sig2: &[f32]) -> f32 {
        let nc = self.l.nc;
        let mut sum_s = 0f64;
        let mut sum_n = 0f64;
        for (c, &s2) in sig2.iter().enumerate() {
            let mut p = 0f64;
            for g in g0..g1 {
                p += self.grid[g * nc + c].norm_sqr() as f64;
            }
            p /= (g1 - g0) as f64;
            sum_s += (p - s2 as f64).max(0.0);
            sum_n += s2 as f64;
        }
        let bins = 2400.0 / self.l.spacing_hz();
        let snr = sum_s / (sum_n / nc as f64 * bins);
        (10.0 * snr.max(1e-6).log10()) as f32
    }

    pub(super) fn finish(&mut self) -> RxBurst {
        let l = self.l;
        let (nc, pre) = (l.nc, l.pre_syms());
        let h = self.hdr.expect("header decoded");
        let ng = self.ng;
        if h.header_only() {
            // Header-only: nothing more to estimate.
            let sig2 = self.sigma2;
            return RxBurst {
                constellation: Constellation::Qpsk,
                user: h.user,
                eq: Vec::new(),
                llrs: Vec::new(),
                snr_db: self.snr_db(1, ng, &sig2[..nc]),
                freq_offset_hz: self.f_hz as f32,
                start_sample: self.start_sample(),
                numerology: self.num,
                extra: h.extra,
                header_only: true,
                pilots: Pilots::Robust,
                clock_ppm: 0.0,
                chan: ChannelMeas::of_snr(self.snr_db(1, ng, &sig2[..nc])),
                mode: DataMode::PLAIN,
                grid: None,
            };
        }
        self.rereference();
        let nsym = ng - pre;
        let cons = Constellation::from_code(h.constellation);
        let mode = h.mode;
        let gain = mode.gain();
        let g2 = gain * gain;
        // Unit points (despread DFT-s symbols) and the points as the data
        // cells of a plain burst carry them (times the data gain).
        let pts = cons.points();
        let mut pts_s = cons.points();
        for v in pts_s.pts.iter_mut() {
            *v = v.scale(gain);
        }
        let bps = pts.bps;
        let es = pts.pts[..pts.count()].iter().map(|p| p.norm_sqr()).sum::<f32>() / pts.count() as f32;
        let map = self.map;
        let eps = self.apply_drift();
        self.track_phase();
        self.est
            .load(&self.grid, ng, &self.sigma2[..nc], |c, g| known(&map, c, g));
        let p_h = self.est.channel_power();
        let fd = match map.pilots {
            Pilots::Robust => &l.fd,
            Pilots::Sparse => &l.fd_sparse,
        };
        self.est.select_model(ng, p_h, fd);
        let (dtab, dc) = match map.pilots {
            Pilots::Robust => (&self.dcar_r, l.data_carriers(Pilots::Robust)),
            Pilots::Sparse => (&self.dcar_s, l.data_carriers(Pilots::Sparse)),
        };
        let per = dtab.len();

        // Channel at every data cell, then a decision-directed noise floor.
        // `res` keeps each data cell's residual (NaN at pilots) for the
        // local noise below.
        let mut dd = [0f32; NC_MAX];
        let mut ddn = [0f32; NC_MAX];
        let mut hp = [0f32; NC_MAX];
        let mut res = vec![f32::NAN; nsym * nc];
        // Decision-directed channel of every cell (pilots exact), for the
        // delay and Doppler measurement.
        let mut zdd = vec![Complex::ZERO; nsym * nc];
        let mut sp = Spreader::new(dc);
        let mut ya = [Complex::ZERO; NC_MAX];
        let mut va = [0f32; NC_MAX];
        let mut za = [Complex::ZERO; NC_MAX];
        for g in pre..ng {
            let row = &dtab[g % per];
            let base = (g - pre) * dc;
            for (j, &cc) in row.iter().enumerate() {
                let c = cc as usize;
                let (hh, mse) = self.est.estimate(c, g, ng, p_h);
                self.hbuf[base + j] = hh;
                self.mbuf[base + j] = mse;
                hp[c] += hh.norm_sqr() * g2;
            }
            // The cells as sent, decided: each cell's own point (plain) or
            // the DFT of the despread symbols' decisions (spread).
            let xa = &mut za[..dc];
            if mode.spread {
                for (j, &cc) in row.iter().enumerate() {
                    let c = cc as usize;
                    ya[j] = self.grid[g * nc + c];
                    va[j] = self.sigma2[c] + self.mbuf[base + j] * g2;
                }
                let (mu, _) = est::fde(&ya[..dc], &self.hbuf[base..base + dc], &va[..dc], gain, xa, &mut sp);
                for v in xa.iter_mut() {
                    *v = est::decide(*v, Complex::new(mu, 0.0), &pts).scale(gain);
                }
                sp.forward(xa);
            } else {
                for (j, &cc) in row.iter().enumerate() {
                    let y = self.grid[g * nc + cc as usize];
                    xa[j] = est::decide(y, self.hbuf[base + j], &pts_s);
                }
            }
            for (j, &cc) in row.iter().enumerate() {
                let c = cc as usize;
                let (hh, mse, x) = (self.hbuf[base + j], self.mbuf[base + j], xa[j]);
                let y = self.grid[g * nc + c];
                let r = (y - hh * x).norm_sqr() - mse * x.norm_sqr();
                res[(g - pre) * nc + c] = r;
                // Spread data: one wrong despread decision, spread again,
                // puts a phase ramp across every carrier of its symbol,
                // which the delay profile would read as an echo. Its cells
                // take the pilots' estimate instead.
                zdd[(g - pre) * nc + c] = if mode.spread {
                    hh
                } else {
                    (y * x.conj()).scale(1.0 / x.norm_sqr().max(1e-12))
                };
                dd[c] += r;
                ddn[c] += 1.0;
            }
        }
        // The LLRs see everything the decisions do (channel noise, TX
        // clipping, ICI, timing error). The SNR report leaves out the
        // sender's own clipping distortion, known from the constellation
        // and mode, so that it measures the channel.
        let kappa = 10f32.powf(-tx_distortion_db(cons, mode, self.num) / 10.0);
        let mut sig2 = self.sigma2;
        let mut sig2_rep = self.sigma2;
        let (mut band_a, mut band_h) = (0f32, 0f32);
        for c in 0..nc {
            let (mut a, mut n, mut h) = (0f32, 0f32, 0f32);
            for k in c.saturating_sub(l.dd_half)..(c + l.dd_half + 1).min(nc) {
                a += dd[k];
                n += ddn[k];
                h += hp[k];
            }
            if n > 0.0 {
                sig2[c] = sig2[c].max(a / n);
                sig2_rep[c] = sig2_rep[c].max((a - kappa * h) / n);
                band_a += a / n;
                band_h += h / n;
            }
        }

        // Band noise is not steady, and a level held for the whole burst
        // makes the LLRs of the cells it rose on far too sure. Each cell
        // takes the largest of the burst's floor above, the residual on its
        // neighbouring carriers over a few symbols (narrow interference)
        // and the residual over its own and neighbouring symbols on every
        // carrier (wideband bursts), each window about LOCAL_CELLS cells.
        let f = l.dd_half;
        let tc = (LOCAL_CELLS / (2 * f + 1) / 2).max(1);
        let ts = LOCAL_CELLS.div_ceil(dc) / 2;
        let mut sym = vec![(0f32, 0f32); nsym];
        for (k, row) in res.chunks_exact(nc).enumerate() {
            for &r in row.iter().filter(|r| !r.is_nan()) {
                sym[k].0 += r;
                sym[k].1 += 1.0;
            }
        }
        let sym_noise: Vec<f32> = (0..nsym)
            .map(|k| {
                let (mut a, mut n) = (0f32, 0f32);
                for s in &sym[k.saturating_sub(ts)..(k + ts + 1).min(nsym)] {
                    a += s.0;
                    n += s.1;
                }
                if n > 0.0 { a / n } else { 0.0 }
            })
            .collect();
        let local_noise = |k: usize, c: usize| -> f32 {
            let (mut a, mut n) = (0f32, 0f32);
            for kk in k.saturating_sub(tc)..(k + tc + 1).min(nsym) {
                for cc in c.saturating_sub(f)..(c + f + 1).min(nc) {
                    let r = res[kk * nc + cc];
                    if !r.is_nan() {
                        a += r;
                        n += 1.0;
                    }
                }
            }
            if n > 0.0 { a / n } else { 0.0 }
        };

        for g in pre..ng {
            for c in 0..nc {
                if let Some(p) = known(&map, c, g) {
                    zdd[(g - pre) * nc + c] = (self.grid[g * nc + c] * p.conj()).scale(1.0 / p.norm_sqr());
                }
            }
        }

        let eq_step = (nsym * dc).div_ceil(super::EQ_MAX).max(1);
        let mut eq = Vec::with_capacity(super::EQ_MAX);
        // LLRs per slot.
        if let Some(d) = self.debug.as_mut() {
            d.clear();
        }
        let mut eesm = Eesm::default();
        let mut cell_s2 = vec![0f32; nsym * dc];
        for g in pre..ng {
            let row = &dtab[g % per];
            let base = (g - pre) * dc;
            for (j, &cc) in row.iter().enumerate() {
                let c = cc as usize;
                let cell = base + j;
                let mut s2 = sig2[c].max(sym_noise[g - pre]).max(local_noise(g - pre, c));
                // A symbol the blanker cut into (scaled back in `extract`):
                // its noise grew by 1 / (1 − b) and the gap spreads
                // b / (1 − b) of the signal over the carriers.
                let bf = self.bfrac[g];
                if bf >= BLANK_ERASE {
                    // Erased: every LLR ≈ 0.
                    s2 = 1e30;
                } else if bf > 0.0 {
                    let k = 1.0 / (1.0 - bf);
                    s2 = s2.max(k * sig2[c] + bf * k * self.hbuf[cell].norm_sqr() * es);
                }
                cell_s2[cell] = s2;
                eesm.add(self.hbuf[cell].norm_sqr(), s2 + self.mbuf[cell]);
                if let Some(d) = self.debug.as_mut() {
                    d.push(CellDebug {
                        g,
                        c,
                        y: self.grid[g * nc + c],
                        h: self.hbuf[cell],
                        mse: self.mbuf[cell],
                        sigma2: s2,
                    });
                }
                if mode.spread {
                    ya[j] = self.grid[g * nc + c];
                    va[j] = s2 + self.mbuf[cell] * g2;
                    continue;
                }
                if cell % eq_step == 0 {
                    let h = self.hbuf[cell];
                    eq.push((self.grid[g * nc + c] * h.conj()).scale(1.0 / (h.norm_sqr() * gain).max(1e-12)));
                }
                est::demap(
                    self.grid[g * nc + c],
                    self.hbuf[cell],
                    self.mbuf[cell],
                    s2,
                    &pts_s,
                    &mut self.slot_llr[cell * bps..cell * bps + bps],
                );
            }
            if mode.spread {
                // Per-carrier MMSE equalisation, inverse DFT, then each
                // despread symbol is its point times the mean gain `mu`
                // plus noise and residual interference of variance `var`.
                let z = &mut za[..dc];
                let (mu, var) = est::fde(&ya[..dc], &self.hbuf[base..base + dc], &va[..dc], gain, z, &mut sp);
                // The clip's distortion lands on the despread symbols of
                // the envelope's peaks, the outer QAM points, not evenly as
                // `var` has it; a share growing with the point's energy
                // keeps those symbols' LLRs from being sure and wrong.
                // Phase-only constellations are untouched.
                let clip_var = if bps >= 4 { kappa * mu * mu } else { 0.0 };
                for (m, &zm) in z.iter().enumerate() {
                    let cell = base + m;
                    if cell % eq_step == 0 && mu > 0.0 {
                        eq.push(zm.scale(1.0 / mu));
                    }
                    est::demap(
                        zm,
                        Complex::new(mu, 0.0),
                        clip_var,
                        var,
                        &pts,
                        &mut self.slot_llr[cell * bps..cell * bps + bps],
                    );
                }
            }
        }
        let nbits = nsym * dc * bps;
        let a = interleave_step(nbits) as u64;
        let llrs: Vec<f32> = (0..nbits)
            .map(|i| {
                let l = self.slot_llr[((a * i as u64) % nbits as u64) as usize];
                if scramble(SEED_DATA, i) == 1 { -l } else { l }
            })
            .collect();

        let snr_db = self.snr_db(pre, ng, &sig2_rep[..nc]);
        let excess_db = {
            let pre_n: f32 = self.sigma2[..nc].iter().sum();
            if td_clipped(cons, mode) {
                // The whole-burst clipper's distortion is uneven over the
                // carriers: the band's total against the preamble's noise
                // and the expected distortion, not carrier by carrier.
                10.0 * (band_a / (pre_n + kappa * band_h).max(1e-30)).max(1e-6).log10()
            } else {
                let dd: f32 = sig2_rep[..nc].iter().sum();
                10.0 * (dd / pre_n.max(1e-30)).max(1e-6).log10()
            }
        };
        let mut chan = self.measure(&zdd, nsym, snr_db, &eesm, excess_db);
        if self.weak_sync {
            // Timing from a preamble the receiver could not see: the
            // spreads would be noise (the effective SNR stays).
            chan.delay_ms = f32::NAN;
            chan.doppler_hz = f32::NAN;
        }
        let grid = BurstGrid::new(l, h, map, &self.grid[..ng * nc], self.sigma2, cell_s2, p_h);
        RxBurst {
            constellation: cons,
            user: h.user,
            eq,
            llrs,
            snr_db,
            freq_offset_hz: (self.f_hz + self.df_res) as f32,
            start_sample: self.start_sample(),
            numerology: self.num,
            extra: h.extra,
            header_only: false,
            pilots: map.pilots,
            clock_ppm: (eps * 1e6) as f32,
            chan,
            mode,
            grid: Some(Arc::new(grid)),
        }
    }
}
