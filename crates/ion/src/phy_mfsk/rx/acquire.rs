//! Sync and header decoding of detector candidates.

use ion_dsp::dsp::Complex;

use super::model::{Params, estimate, header_prior, header_prior_r1, metric, mutual_info};
use super::{Active, Cand, EXT, Energies, Known, MARGIN, MfskRx, NE, RING, gi};
use crate::phy_mfsk::frame::{self, Fmt, HYPS, Kind, PRE_LEN};
use crate::phy_mfsk::util::lse;
use crate::phy_mfsk::{MfskMode, SYM, bin_hz, conv};

/// Timing search range around the coarse estimate, baseband samples.
const DSEARCH: i32 = 64;
/// Acceptance threshold after fine sync (preamble energy over noise; the
/// timing and frequency search lifts noise to about 2).
const Z_ACC: f32 = 2.0;
/// Acceptance threshold inside an expected slot (see `Z_DET_SLOT`).
const Z_ACC_SLOT: f32 = 1.0;

impl MfskRx {
    /// Σ over symbols of the energy at their known tones, window `w`
    /// ending `d` after each nominal symbol end.
    fn known_energy(&self, t0c: i64, known: &[Known], w: usize, d: i32, f: f32) -> f32 {
        let mut j = 0.0;
        for k in known {
            let start = t0c + k.end + d as i64 - w as i64;
            for t in 0..k.n {
                j += self.corr1(start, w, f + k.hz[t]).norm_sqr();
            }
        }
        j
    }

    /// Timing: middle of the region around the maximum where the smoothed
    /// known-tone energy stays within 10% of it.
    fn timing(
        &self,
        t0c: i64,
        known: &[Known],
        w: usize,
        f: f32,
        range: std::iter::StepBy<std::ops::RangeInclusive<i32>>,
    ) -> i32 {
        let ds: Vec<i32> = range.collect();
        let j: Vec<f32> = ds.iter().map(|&d| self.known_energy(t0c, known, w, d, f)).collect();
        let sm: Vec<f32> = (0..j.len())
            .map(|i| {
                let a = j[i.saturating_sub(1)];
                let c = j[(i + 1).min(j.len() - 1)];
                0.25 * a + 0.5 * j[i] + 0.25 * c
            })
            .collect();
        let (imax, &m) = sm
            .iter()
            .enumerate()
            .fold((0, &f32::MIN), |b, (i, v)| if *v > *b.1 { (i, v) } else { b });
        let (mut l, mut r) = (imax, imax);
        while l > 0 && sm[l - 1] >= 0.9 * m {
            l -= 1;
        }
        while r + 1 < sm.len() && sm[r + 1] >= 0.9 * m {
            r += 1;
        }
        (ds[l] + ds[r]) / 2
    }

    /// Frequency: parabolic peak of the known-tone energy over a grid
    /// (window `w` ending `d` after each symbol end).
    #[allow(clippy::too_many_arguments)]
    fn frequency(&self, t0c: i64, known: &[Known], w: usize, d: i32, f0: f32, half: f32, step: f32) -> f32 {
        let n = (half / step).round() as i32;
        let j: Vec<f32> = (-n..=n)
            .map(|i| self.known_energy(t0c, known, w, d, f0 + i as f32 * step))
            .collect();
        let imax = (0..j.len()).fold(0, |b, i| if j[i] > j[b] { i } else { b });
        let mut f = f0 + (imax as i32 - n) as f32 * step;
        if imax > 0 && imax + 1 < j.len() {
            let (a, b, c) = (j[imax - 1], j[imax], j[imax + 1]);
            let den = a - 2.0 * b + c;
            if den < 0.0 {
                f += 0.5 * (a - c) / den * step;
            }
        }
        f
    }

    pub(super) fn preamble_known() -> Vec<Known> {
        (0..PRE_LEN)
            .map(|i| Known {
                n: 1,
                e: [frame::costas_bin(i) + EXT, 0, 0],
                hz: [bin_hz(frame::costas_bin(i) as i32), 0.0, 0.0],
                end: (SYM * (i + 1)) as i64,
                vals: 0,
                v: [0; 3],
                hop: 0,
                fmt: Fmt::of(MfskMode::R0),
            })
            .collect()
    }

    /// Returns false when the candidate is finished (rejected or decoded).
    pub(super) fn service_cand(&mut self, c: &mut Cand) -> bool {
        let now = self.n_bb as i64;
        if now - c.t0c > RING as i64 / 2 {
            return false;
        }
        // A detection inside a decoded burst belongs to that burst.
        if self.spans.iter().any(|s| c.t0c >= s.0 && c.t0c <= s.1) {
            return false;
        }
        if !c.synced {
            let need = c.t0c + (PRE_LEN * SYM) as i64 + DSEARCH as i64 + MARGIN;
            if now < need {
                return true;
            }
            if !self.sync(c) {
                return false;
            }
            self.stats.accepted += 1;
        }
        let dmax = *c.d.iter().max().unwrap() as i64;
        let mut pending = false;
        for (hi, &(mode, kind)) in HYPS.iter().enumerate() {
            if c.tried[hi] {
                continue;
            }
            let hs = frame::header_symbols(mode, kind);
            let need = c.t0c + self.grids[gi(mode)].sym_end(hs - 1) + dmax + MARGIN;
            if now < need {
                pending = true;
                continue;
            }
            c.tried[hi] = true;
            match mode {
                MfskMode::R0 => self.fill_energies(c, PRE_LEN + hs),
                MfskMode::R1 => self.fill_energies_r1(c, hs),
            }
            if let Some(b) = self.try_header(c, mode, kind) {
                self.stats.headers += 1;
                // Nothing else can start inside this burst (half duplex):
                // detections there are its own preamble seen off time or
                // frequency, or its data matching a few Costas tones.
                let end = self.grids[gi(mode)].sym_end(b.hsyms + b.dsyms - 1);
                self.spans
                    .push_back((c.t0c - 4 * SYM as i64, c.t0c + end - 2 * SYM as i64));
                self.bursts.push(b);
                return false;
            }
        }
        pending
    }

    /// Energies of symbols 0..n (preamble + R0 header) per R0 window.
    fn fill_energies(&self, c: &mut Cand, n: usize) {
        let g = &self.grids[0];
        for (wi, &w) in g.wins.iter().enumerate() {
            while c.e[wi].len() < n {
                let s = c.e[wi].len();
                let start = c.t0c + (SYM * (s + 1)) as i64 + c.d[wi] as i64 - w as i64;
                let mut x = [0f32; NE];
                if self.available(start, w) {
                    self.energies(g, start, w, c.f, &mut x);
                }
                c.e[wi].push(x);
            }
        }
    }

    /// Energies of the first `n` R1 header symbols per R1 window, all
    /// ending where the preamble's whole-symbol window does.
    fn fill_energies_r1(&self, c: &mut Cand, n: usize) {
        let g = &self.grids[1];
        for (wi, &w) in g.wins.iter().enumerate() {
            while c.e1[wi].len() < n {
                let s = c.e1[wi].len();
                let start = c.t0c + g.sym_end(s) + c.d[2] as i64 - w as i64;
                let mut x = [0f32; NE];
                if self.available(start, w) {
                    self.energies(g, start, w, c.f, &mut x);
                }
                c.e1[wi].push(x);
            }
        }
    }

    fn sync(&self, c: &mut Cand) -> bool {
        let known = Self::preamble_known();
        let wins = self.grids[0].wins;
        let d1 = self.timing(c.t0c, &known, wins[0], c.f0, (-DSEARCH..=DSEARCH).step_by(4));
        let f1 = self.frequency(c.t0c, &known, wins[0], d1, c.f0, 16.0, 1.0);
        for (wi, &w) in wins.iter().enumerate() {
            c.d[wi] = self.timing(c.t0c, &known, w, f1, (-DSEARCH..=DSEARCH).step_by(2));
        }
        c.f = f1;
        self.fill_energies(c, PRE_LEN);
        // Accept on the preamble energy against the noise in its empty bins.
        let (mut sig, mut nsum, mut ncnt) = (0f32, 0f32, 0usize);
        for (i, x) in c.e[0].iter().enumerate().take(PRE_LEN) {
            let e = frame::costas_bin(i) + EXT;
            sig += x[e];
            for (j, v) in x.iter().enumerate() {
                if j.abs_diff(e) > 2 {
                    nsum += v;
                    ncnt += 1;
                }
            }
        }
        let z = sig / PRE_LEN as f32 / (nsum / ncnt as f32).max(1e-30);
        c.zfine = z;
        c.synced = z >= if c.slot { Z_ACC_SLOT } else { Z_ACC };
        c.synced
    }

    /// Header decode under one hypothesis, trying each analysis window.
    fn try_header(&mut self, c: &Cand, mode: MfskMode, kind: Kind) -> Option<Active> {
        let g = &self.grids[gi(mode)];
        let fmt = g.fmt;
        let steps = frame::header_steps(fmt, kind);
        let hs = steps / fmt.tones;
        let nbits = frame::header_info_bits(kind);
        for wi in [2usize, 1, 0] {
            let pre = &c.e[wi][..PRE_LEN];
            let (p, hdr) = match mode {
                MfskMode::R0 => (header_prior(pre), &c.e[wi][PRE_LEN..]),
                MfskMode::R1 => (
                    header_prior_r1(pre, &c.e1[wi][..hs], self.grids[0].wins[wi], g),
                    &c.e1[wi][..],
                ),
            };
            // Tone metrics per slot.
            let vals = fmt.vals();
            let mut lam = vec![0f32; steps * vals];
            for (s, x) in hdr.iter().take(hs).enumerate() {
                let tm = p.terms(x);
                let h = fmt.hop(s);
                for t in 0..fmt.tones {
                    let slot = s * fmt.tones + t;
                    for v in 0..vals {
                        lam[slot * vals + v] = metric(&tm, fmt.tone_bin(h, t, v) + EXT);
                    }
                }
            }
            debug_assert_eq!(steps, nbits);
            let bits = conv::decode(steps, |j, l| lam[frame::header_slot(fmt, kind, j) * vals + l]);
            if let Some((user, extra, nb, sym)) = frame::parse_header(mode, kind, &bits) {
                return Some(self.start_burst(c, mode, kind, user, extra, (nb, sym), &bits));
            }
        }
        self.try_known_header(c, mode, kind)
    }

    /// In the expected slot, when the header did not decode: is the
    /// expected header there? The score is the log-likelihood ratio of the
    /// expected tone sequence against a sequence of uniformly random tones,
    /// summed over the header symbols (best analysis window). For a true
    /// likelihood ratio, anything that is not the expected header
    /// (noise, or another burst) passes `min_score` with probability at
    /// most e^−min_score per window tried.
    ///
    /// That bound covers noise, not a *similar* header: another net's
    /// check-in differs from ours only in its net id, so most of its tones
    /// match ours. So the expected header must also beat every other value
    /// of the 16-bit `user` field (other nets, other burst codes) by
    /// `min_margin` nats: a near miss loses to its own true header.
    fn try_known_header(&mut self, c: &Cand, mode: MfskMode, kind: Kind) -> Option<Active> {
        let e = self.expect?;
        if !c.slot {
            return None;
        }
        // The expected header of this hypothesis (an expectation may hold
        // two: a header-only burst and a data burst).
        let h = [e.header, e.alt].into_iter().flatten().find(|h| {
            let want = if h.nbits == 0 { Kind::HeaderOnly } else { Kind::Data };
            h.mode == mode && want == kind
        })?;
        let cands = h.candidates();
        if cands.is_empty() {
            return None;
        }
        let g = &self.grids[gi(mode)];
        let fmt = g.fmt;
        // The length field as sent: a symbol-mapped burst flags it.
        let field = h.nbits | if h.symbols && h.nbits > 0 { frame::SYM_FLAG } else { 0 };
        let values_of = |user: u16, extra: u16| {
            frame::header_values(mode, kind, &frame::header_bits(mode, kind, user, extra, field))
        };
        let cand_values: Vec<Vec<[u16; 3]>> = cands.iter().map(|&(u, x)| values_of(u, x)).collect();
        let hs = cand_values[0].len();
        let vals = fmt.vals();
        let ln_vals = (vals as f32).ln();
        let ln_n = (cands.len() as f32).ln();
        // The metrics summed along one header's tones.
        let path = |lam: &[f32], values: &[[u16; 3]]| -> f32 {
            let mut sum = 0.0;
            for (s, v) in values.iter().enumerate() {
                for (t, &vt) in v.iter().enumerate().take(fmt.tones) {
                    sum += lam[(s * fmt.tones + t) * vals + vt as usize];
                }
            }
            sum
        };
        // Per window: the score, and the tone metrics per slot and value.
        // With unknown bits the score is that of the mixture of every
        // header the expectation allows (a likelihood ratio still, so the
        // e^−min_score bound holds).
        let mut best = (f32::MIN, Vec::new());
        for wi in [2usize, 1, 0] {
            let pre = &c.e[wi][..PRE_LEN];
            let (p, hdr) = match mode {
                MfskMode::R0 => (header_prior(pre), &c.e[wi][PRE_LEN..]),
                MfskMode::R1 => (
                    header_prior_r1(pre, &c.e1[wi][..hs], self.grids[0].wins[wi], g),
                    &c.e1[wi][..],
                ),
            };
            let mut norm = 0.0;
            let mut lam = Vec::with_capacity(hs * fmt.tones * vals);
            for (s, x) in hdr.iter().take(hs).enumerate() {
                let tm = p.terms(x);
                let hop = fmt.hop(s);
                for t in 0..fmt.tones {
                    let at = lam.len();
                    lam.extend((0..vals).map(|x| metric(&tm, fmt.tone_bin(hop, t, x) + EXT)));
                    norm += lam[at..].iter().fold(f32::NEG_INFINITY, |a, &l| lse(a, l)) - ln_vals;
                }
            }
            let mix = cand_values.iter().map(|v| path(&lam, v)).fold(f32::NEG_INFINITY, lse);
            let score = mix - ln_n - norm;
            if score > best.0 {
                best = (score, lam);
            }
        }
        let (score, lam) = best;
        self.stats.last_known_score = score;
        self.stats.last_known_margin = 0.0;
        if score < e.min_score {
            return None;
        }
        // The likeliest allowed header must beat every header outside the
        // set: every other `user` value, and (header-only bursts, whose
        // `extra` is their message) every other `extra`.
        let (bi, mine) = cand_values
            .iter()
            .map(|v| path(&lam, v))
            .enumerate()
            .fold((0, f32::MIN), |a, (i, s)| if s > a.1 { (i, s) } else { a });
        let (bu, bx) = cands[bi];
        let mut rival = (0..=u16::MAX)
            .filter(|&u| !h.allows_user(u))
            .map(|u| path(&lam, &values_of(u, bx)))
            .fold(f32::MIN, f32::max);
        if kind == Kind::HeaderOnly {
            rival = (0..=u16::MAX)
                .filter(|&x| !h.allows_extra(x))
                .map(|x| path(&lam, &values_of(bu, x)))
                .fold(rival, f32::max);
        }
        self.stats.last_known_margin = mine - rival;
        if mine - rival < e.min_margin {
            return None;
        }
        self.stats.known_headers += 1;
        let info = frame::header_bits(mode, kind, bu, bx, field);
        Some(self.start_burst(c, mode, kind, bu, bx, (h.nbits, h.symbols && h.nbits > 0), &info))
    }

    #[allow(clippy::too_many_arguments)]
    fn start_burst(
        &mut self,
        c: &Cand,
        mode: MfskMode,
        kind: Kind,
        user: u16,
        extra: u16,
        (nbits, symbols): (usize, bool),
        info: &[u8],
    ) -> Active {
        let g = &self.grids[gi(mode)];
        let fmt = g.fmt;
        let wins = g.wins;
        let hs = frame::header_symbols(mode, kind);
        // Every symbol up to the end of the header is now known. R0 also
        // models the preamble (same grid and windows); R1 its header alone.
        let mut known = if mode == MfskMode::R0 {
            Self::preamble_known()
        } else {
            Vec::new()
        };
        let npre = known.len();
        for (s, v) in frame::header_values(mode, kind, info).iter().enumerate() {
            let h = fmt.hop(s);
            let mut e = [0usize; 3];
            let mut hz = [0f32; 3];
            for t in 0..fmt.tones {
                let b = fmt.tone_bin(h, t, v[t] as usize);
                e[t] = b + EXT;
                hz[t] = fmt.bin_hz(b as i32);
            }
            known.push(Known {
                n: fmt.tones,
                e,
                hz,
                end: g.sym_end(s),
                vals: fmt.vals(),
                v: *v,
                hop: h,
                fmt,
            });
        }
        // Refine frequency and timing on all of them.
        let d0 = match mode {
            MfskMode::R0 => c.d,
            MfskMode::R1 => [c.d[2]; 3],
        };
        let f = self.frequency(c.t0c, &known, wins[0], d0[0], c.f, 5.0, 0.5);
        let mut d = d0;
        for (wi, &w) in wins.iter().enumerate() {
            d[wi] = self.timing(c.t0c, &known, w, f, (d0[wi] - 16..=d0[wi] + 16).step_by(2));
        }
        // Model per window and window position; keep the one whose model
        // explains the header tones best (highest estimated mutual
        // information): on long echoes a few samples decide whether the
        // window catches the previous symbol. When several are saturated at
        // the ceiling, take the most signal against noise plus interference.
        let mut cand: Vec<(f32, f32, usize, i32, Params)> = Vec::new();
        for (wi, &w) in wins.iter().enumerate() {
            let mut best_w: Option<(f32, f32, usize, i32, Params)> = None;
            for dd in [-12, -6, 0, 6, 12] {
                let dw = d[wi] + dd;
                let e: Vec<Energies> = known
                    .iter()
                    .map(|k| {
                        let start = c.t0c + k.end + dw as i64 - w as i64;
                        let mut x = [0f32; NE];
                        self.energies(g, start, w, f, &mut x);
                        x
                    })
                    .collect();
                let p = estimate(&e, &known, g.ne);
                let mi = mutual_info(&e[npre..], &known[npre..], &p);
                let g_eff = p.gtot / (1.0 + p.eps * (p.r - 1.0));
                if best_w.as_ref().is_none_or(|b| (mi, g_eff) > (b.0, b.1)) {
                    best_w = Some((mi, g_eff, wi, dw, p));
                }
            }
            let b = best_w.unwrap();
            self.stats.last_mi[wi] = b.0;
            self.stats.last_d[wi] = b.3;
            self.stats.last_gtot[wi] = b.4.gtot;
            cand.push(b);
        }
        let mi_max = cand.iter().fold(f32::MIN, |m, c| m.max(c.0));
        let tol = 0.02 * (fmt.vals() as f32).log2();
        let best = cand.into_iter().filter(|c| c.0 >= mi_max - tol).fold(
            None::<(f32, f32, usize, i32, Params)>,
            |b, c| match b {
                Some(b) if b.1 >= c.1 => Some(b),
                _ => Some(c),
            },
        );
        let (_, _, wi, dsel, params) = best.unwrap();
        let w = wins[wi];
        // The window's noise bandwidth is (8000/k)/w Hz: 2400 Hz is 0.3·k·w
        // of it.
        let k = self.layout.k as f32;
        let snr_db = 10.0 * (fmt.tones as f32 * params.gtot).max(1e-6).log10() - 10.0 * (0.3 * k * w as f32).log10();
        // The whole-symbol window peaks half a frequency sweep late (the
        // next symbol's sweep starts on the current tone).
        let dstart = if mode == MfskMode::R0 { d[2] } else { c.d[2] };
        let t0 = c.t0c + dstart as i64 - (self.layout.sweep / self.layout.dec() / 2) as i64;
        let start_sample = self.audio_start(t0).max(0) as u64;
        Active {
            mode,
            symbols,
            user,
            extra,
            nbits,
            hsyms: hs,
            dsyms: frame::data_symbols(mode, nbits),
            t0c: c.t0c,
            d: dsel,
            w,
            f,
            f_sum: 0.0,
            params,
            next: 0,
            energies: Vec::new(),
            acc: Complex::ZERO,
            mag: 0.0,
            coh: Complex::ZERO,
            coh_p: [0.0; 2],
            coh_n: 0.0,
            prev_e: None,
            isi: [[0.0; 2]; 2],
            snr_db,
            start_sample,
            dref: d[2],
            shifts: Vec::new(),
            shift: 0,
            tnum: 0.0,
            tden: 0.0,
            tcnt: 0,
            blocks: Vec::new(),
            drift: 0.0,
            blanked: Vec::new(),
        }
    }
}
