//! Streaming receiver: detection, synchronisation and the header; the
//! data stage is in [`demod`], clock-drift tracking in [`drift`].

use std::collections::VecDeque;

use ion_dsp::dsp::{AUDIO_RATE, Complex, Decimator, Nco};

use crate::blanker::Blanker;
use crate::chan::ChannelMeas;

use super::est;
use super::frame::*;
use super::tx::{b_sync, preamble_boost, preamble_template, twiddles};
use super::{Constellation, HO_COPIES, Numerology, Pilots, check_center, front_end_taps_for};

mod demod;
mod drift;

use drift::Drift;

/// One received burst.
#[derive(Clone, Debug)]
pub struct RxBurst {
    /// Data constellation (QPSK, the header's own, for a header-only burst).
    pub constellation: Constellation,
    /// The 16 link-layer bits from the header.
    pub user: u16,
    /// One LLR per data bit, padding included (`capacity_bits_with` long);
    /// empty for a header-only burst.
    pub llrs: Vec<f32>,
    /// Estimated SNR in 2.4 kHz, dB (over the header symbols for a
    /// header-only burst).
    pub snr_db: f32,
    /// Measured carrier offset, Hz (mean over the burst).
    pub freq_offset_hz: f32,
    /// Index of the burst's first sample in the stream fed to `process`.
    pub start_sample: u64,
    /// The receiver's (and so the burst's) numerology.
    pub numerology: Numerology,
    /// The 16 extra bits of a header-only burst; 0 otherwise.
    pub extra: u16,
    /// True for a header-only burst (no data symbols, `llrs` empty).
    pub header_only: bool,
    /// Pilot pattern of the data symbols (from the header).
    pub pilots: Pilots,
    /// Sample-clock offset the receiver tracked, parts per million
    /// (positive: the burst arrived stretched, i.e. the sender's clock is
    /// slow against ours). Shrunk towards 0 when the burst is too short or
    /// noisy to tell.
    pub clock_ppm: f32,
    /// Delay and Doppler spread and effective SNR over the burst (only
    /// the effective SNR, equal to `snr_db`, for a header-only burst).
    pub chan: ChannelMeas,
    /// How the data symbols were sent (from the header).
    pub mode: DataMode,
}

/// Receiver counters.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RxStats {
    /// Detector triggers (Schmidl-Cox metric over threshold).
    pub candidates: u64,
    /// Candidates whose preamble checks passed (header attempts).
    pub syncs: u64,
    pub header_ok: u64,
    pub header_fail: u64,
    pub bursts: u64,
}

/// Ring buffer of baseband samples (2.05 s).
const RING: usize = 1 << 14;
const MASK: u64 = RING as u64 - 1;

#[derive(Clone, Copy, Debug, Default)]
struct Cand {
    n_pk: u64,
    f1: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Sync,
    Collect,
}

/// Audio samples behind each baseband sample's blanked count.
const BLANK_SPAN: usize = 2 * (DEC / 2) + 1;
/// A symbol whose FFT window is blanked more than this is erased.
const BLANK_ERASE: f32 = 0.9;

/// Streaming receiver for one numerology: feed 48 kHz audio in any chunk
/// sizes. To receive both guards, run one receiver of each on the same
/// audio; their preambles do not trigger each other.
pub struct PhyRx {
    num: Numerology,
    l: &'static Layout,
    nco: Nco,
    dec: Decimator,
    ring: Vec<Complex>,
    written: u64,

    // Schmidl-Cox detector.
    sc_p: (f64, f64),
    sc_e: f64,
    sc_track: bool,
    sc_best: f32,
    sc_best_n: u64,
    sc_best_p: (f64, f64),
    sc_hold: u64,
    sc_count: u32,
    cands: VecDeque<Cand>,

    // Burst state.
    phase: Phase,
    need: u64,
    cur: Cand,
    win0: u64,
    first_path: u64,
    f_hz: f64,
    df_res: f64,
    g_next: usize,
    ng: usize,
    hdr: Option<Header>,
    /// The first header copy failed: decode both copies at `ng`.
    ho_retry: bool,
    last_end: u64,
    sigma2: [f32; NC_MAX],
    /// Pilot map of the current burst (robust until the header is known).
    map: Map,
    /// FFT window shift of each grid symbol, samples (clock drift).
    wshift: Vec<i32>,
    drift: Drift,
    track_drift: bool,
    /// Impulse blanker on the audio, how many of the DEC audio samples
    /// around each baseband sample it zeroed (ring like `ring`), and the
    /// blanked fraction of each grid symbol's FFT window.
    blanker: Blanker,
    blanked: Vec<u8>,
    bfrac: Vec<f32>,

    // Workspace.
    tw: Vec<Complex>,
    bt: Vec<Complex>,
    /// Transmitted preamble (A with its ramp, then B with its CP).
    pre: Vec<Complex>,
    center_rot: Vec<Complex>,
    /// Data carriers by pilot phase (`g % period`), per pattern.
    dcar_r: Vec<Vec<u8>>,
    dcar_s: Vec<Vec<u8>>,
    grid: Vec<Complex>,
    est: est::Estimator,
    seg: Vec<Complex>,
    corr: Vec<f32>,
    rho: Vec<f32>,
    ews: Vec<f32>,
    hbuf: Vec<Complex>,
    mbuf: Vec<f32>,
    slot_llr: Vec<f32>,
    genie: Option<Genie>,
    stats: RxStats,
    debug: Option<Vec<CellDebug>>,
    rho_log: Option<Vec<f32>>,
    /// Channel measurement: Hann taper across the carriers, and the delay
    /// span (bins of the power delay profile) of a single path through it.
    hann: Vec<f32>,
    span0: usize,
}

/// Test hook: header contents and the burst's expected start (audio
/// samples). Candidates more than 5 ms from `start` are rejected.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Genie {
    pub constellation: Constellation,
    pub data_symbols: usize,
    pub user: u16,
    pub start: u64,
    pub pilots: Pilots,
}

/// Per data cell of the last burst (debug hook).
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct CellDebug {
    pub g: usize,
    pub c: usize,
    pub y: Complex,
    pub h: Complex,
    pub mse: f32,
    pub sigma2: f32,
}

/// Known value of cell (`c`, `g`) under `map`, or `None` for a data cell.
fn known(map: &Map, c: usize, g: usize) -> Option<Complex> {
    if g == 0 {
        Some(map.l.b_value(c, preamble_boost()))
    } else if map.is_pilot(c, g) {
        Some(map.pilot_value(c, g))
    } else {
        None
    }
}

impl PhyRx {
    /// A Normal-numerology receiver.
    ///
    /// `center_hz` is the audio frequency of the middle carrier; see
    /// [`modulate`](super::modulate) for the valid range.
    pub fn new(center_hz: f32) -> Self {
        Self::with_numerology(Numerology::Normal, center_hz)
    }

    /// A receiver for bursts of numerology `num`.
    pub fn with_numerology(num: Numerology, center_hz: f32) -> Self {
        check_center(num, center_hz);
        let l = num.layout();
        let max_g = l.pre_syms() + MAX_DATA_SYMS;
        let center_rot = (0..l.nc)
            .map(|c| {
                let a = core::f64::consts::TAU * l.bin(c) as f64 * l.delay_center() / l.n as f64;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let nl = 2 * l.mf_span + 1;
        let dc_max = l.data_carriers(Pilots::Robust).max(l.data_carriers(Pilots::Sparse));
        let cells = (MAX_DATA_SYMS * dc_max).max(HO_COPIES * l.hdr_slots() / 2);
        Self {
            num,
            l,
            nco: Nco::new(center_hz, AUDIO_RATE),
            dec: Decimator::new(DEC, front_end_taps_for(num)),
            ring: vec![Complex::ZERO; RING],
            written: 0,
            sc_p: (0.0, 0.0),
            sc_e: 0.0,
            sc_track: false,
            sc_best: 0.0,
            sc_best_n: 0,
            sc_best_p: (0.0, 0.0),
            sc_hold: 0,
            sc_count: 0,
            cands: VecDeque::with_capacity(64),
            phase: Phase::Idle,
            need: u64::MAX,
            cur: Cand::default(),
            win0: 0,
            first_path: 0,
            f_hz: 0.0,
            df_res: 0.0,
            g_next: 0,
            ng: 0,
            hdr: None,
            ho_retry: false,
            last_end: 0,
            sigma2: [0.0; NC_MAX],
            map: Map::new(l, l.pre_syms(), Pilots::Robust),
            wshift: vec![0; max_g],
            drift: Drift::default(),
            track_drift: l.drift,
            blanker: Blanker::new(center_hz),
            blanked: vec![0; RING],
            bfrac: vec![0.0; max_g],
            tw: twiddles(l.n),
            bt: b_sync(l),
            pre: preamble_template(l),
            center_rot,
            dcar_r: data_carrier_table(l, &l.robust),
            dcar_s: data_carrier_table(l, &l.sparse),
            grid: vec![Complex::ZERO; max_g * l.nc],
            est: est::Estimator::new(l, max_g),
            seg: vec![Complex::ZERO; nl + l.n],
            corr: vec![0.0; nl],
            rho: vec![0.0; nl],
            ews: vec![0.0; nl],
            hbuf: vec![Complex::ZERO; cells],
            mbuf: vec![0.0; cells],
            slot_llr: vec![0.0; cells * BPS_MAX],
            genie: None,
            stats: RxStats::default(),
            debug: None,
            rho_log: None,
            hann: (0..l.nc)
                .map(|c| {
                    let x = core::f32::consts::TAU * (c + 1) as f32 / (l.nc + 1) as f32;
                    0.5 - 0.5 * x.cos()
                })
                .collect(),
            span0: 0,
        }
        .with_span0()
    }

    /// The numerology this receiver decodes.
    pub fn numerology(&self) -> Numerology {
        self.num
    }

    /// Appends every burst completed within `samples` to `out`.
    pub fn process(&mut self, samples: &[f32], out: &mut Vec<RxBurst>) {
        let half = (self.l.filter_len - 1) / 2;
        for &x in samples {
            let x = self.blanker.push(x);
            if let Some(y) = self.dec.push(self.nco.mix_down(x)) {
                // Audio zeroed around the front-end filter's centre.
                let b = self.blanker.blanked_count(half, DEC / 2);
                self.blanked[(self.written & MASK) as usize] = b as u8;
                self.push_baseband(y, out);
            }
        }
    }

    /// True while a burst is being received (for half-duplex timing): from
    /// the moment its preamble is accepted until it is delivered or its
    /// header fails (when the first header copy fails, one header length
    /// later: it may be a header-only burst). Detector triggers on noise do
    /// not count.
    pub fn busy(&self) -> bool {
        self.phase == Phase::Collect
    }

    /// Test hook: when set, the header is not decoded but taken as given.
    /// Detection, sync and channel estimation run as usual. For measuring
    /// raw BER only.
    #[doc(hidden)]
    pub fn set_genie_header(&mut self, h: Option<Genie>) {
        self.genie = h;
    }

    #[doc(hidden)]
    pub fn stats(&self) -> RxStats {
        self.stats
    }

    /// Debug hook: keep per-cell data of the last burst.
    #[doc(hidden)]
    pub fn set_debug(&mut self, on: bool) {
        self.debug = on.then(Vec::new);
        self.rho_log = on.then(Vec::new);
    }

    /// Debug hook: whole-preamble correlation of every candidate that
    /// passed the B pre-filter.
    #[doc(hidden)]
    pub fn debug_rho(&self) -> &[f32] {
        self.rho_log.as_deref().unwrap_or(&[])
    }

    /// Debug hook: channel model (Doppler Hz, delay span samples) and
    /// error-variance scale picked for the last burst.
    #[doc(hidden)]
    pub fn debug_model(&self) -> (f64, f64, f32) {
        (self.est.model.0, self.est.model.1, self.est.mse_scale)
    }

    #[doc(hidden)]
    pub fn debug_cells(&self) -> &[CellDebug] {
        self.debug.as_deref().unwrap_or(&[])
    }

    #[inline]
    fn at(&self, n: u64) -> Complex {
        self.ring[(n & MASK) as usize]
    }

    fn push_baseband(&mut self, y: Complex, out: &mut Vec<RxBurst>) {
        let n = self.written;
        self.ring[(n & MASK) as usize] = y;
        self.written += 1;
        self.detect(n);
        while self.written >= self.need {
            self.step(out);
        }
    }

    // -- detection ---------------------------------------------------------

    fn sc_term(&self, n: u64) -> (f64, f64, f64) {
        let a = self.at(n);
        let b = self.at(n - self.l.a_period() as u64);
        let p = a * b.conj();
        (p.re as f64, p.im as f64, (a.norm_sqr() + b.norm_sqr()) as f64)
    }

    fn detect(&mut self, n: u64) {
        let lag = self.l.a_period() as u64;
        let win = self.l.sc_win as u64;
        if n < lag {
            return;
        }
        self.sc_count += 1;
        if self.sc_count >= 2048 && n >= lag + win {
            // Exact recompute so rounding never accumulates.
            self.sc_count = 0;
            let (mut pr, mut pi, mut e) = (0.0, 0.0, 0.0);
            for i in n + 1 - win..=n {
                let t = self.sc_term(i);
                pr += t.0;
                pi += t.1;
                e += t.2;
            }
            self.sc_p = (pr, pi);
            self.sc_e = e;
        } else {
            let t = self.sc_term(n);
            self.sc_p.0 += t.0;
            self.sc_p.1 += t.1;
            self.sc_e += t.2;
            if n >= lag + win {
                let o = self.sc_term(n - win);
                self.sc_p.0 -= o.0;
                self.sc_p.1 -= o.1;
                self.sc_e -= o.2;
            }
        }
        if n + 1 < lag + win {
            return;
        }
        let r = 0.5 * self.sc_e;
        let m = if r > 1e-20 * win as f64 {
            ((self.sc_p.0 * self.sc_p.0 + self.sc_p.1 * self.sc_p.1) / (r * r)) as f32
        } else {
            0.0
        };
        if self.sc_track {
            if m > self.sc_best {
                self.sc_best = m;
                self.sc_best_n = n;
                self.sc_best_p = self.sc_p;
            } else if n >= self.sc_best_n + self.l.sc_drop as u64 * lag {
                self.sc_track = false;
                self.sc_hold = self.sc_best_n + 2 * self.l.sc_drop as u64 * lag;
                let f1 = self.sc_best_p.1.atan2(self.sc_best_p.0) * FS / (core::f64::consts::TAU * lag as f64);
                self.stats.candidates += 1;
                if self.cands.len() < self.cands.capacity() {
                    self.cands.push_back(Cand {
                        n_pk: self.sc_best_n,
                        f1,
                    });
                }
                if self.phase == Phase::Idle {
                    self.need = self.written;
                }
            }
        } else if m > self.l.m_on && n >= self.sc_hold {
            self.sc_track = true;
            self.sc_best = m;
            self.sc_best_n = n;
            self.sc_best_p = self.sc_p;
        }
    }

    // -- state machine -----------------------------------------------------

    fn oldest(&self) -> u64 {
        self.written.saturating_sub(RING as u64 - 64)
    }

    fn step(&mut self, out: &mut Vec<RxBurst>) {
        let l = self.l;
        match self.phase {
            Phase::Idle => {
                while let Some(c) = self.cands.pop_front() {
                    let lo = c.n_pk as i64 - l.sc_peak_to_a() - l.mf_span as i64;
                    if c.n_pk < self.last_end || lo < self.oldest() as i64 {
                        continue;
                    }
                    self.cur = c;
                    self.phase = Phase::Sync;
                    self.need = (c.n_pk as i64 - l.sc_peak_to_a() + l.a_to_b() as i64 + l.mf_span as i64) as u64
                        + l.n as u64
                        + 1;
                    return;
                }
                self.need = u64::MAX;
            }
            Phase::Sync => {
                if self.sync() {
                    self.stats.syncs += 1;
                    self.phase = Phase::Collect;
                    self.g_next = 0;
                    self.ng = l.pre_syms();
                    self.hdr = None;
                    self.ho_retry = false;
                    self.map = Map::new(l, l.pre_syms(), Pilots::Robust);
                    self.drift = Drift::default();
                    self.wshift[0] = 0;
                    self.need = self.window_start(0) + l.n as u64;
                } else {
                    self.fail();
                }
            }
            Phase::Collect => {
                let g = self.g_next;
                if self.window_start(g) < self.oldest() {
                    self.fail();
                    return;
                }
                self.extract(g);
                self.drift_update(g);
                self.g_next += 1;
                let pre = l.pre_syms();
                let ho_end = pre + (HO_COPIES - 1) * l.hdr_syms;
                if self.g_next == pre && self.hdr.is_none() {
                    match self.decode_header(1) {
                        Some(h) => {
                            self.stats.header_ok += 1;
                            self.ng = if h.header_only() { ho_end } else { pre + h.nsym as usize };
                            self.map = if h.header_only() {
                                Map::new(l, ho_end, Pilots::Robust)
                            } else {
                                Map::new(l, pre, h.pilots()).with_gain(h.mode.gain())
                            };
                            self.hdr = Some(h);
                        }
                        None if self.genie.is_none() => {
                            // Maybe a header-only burst: retry with both
                            // copies once the second has arrived.
                            self.ng = ho_end;
                            self.ho_retry = true;
                        }
                        None => {
                            self.stats.header_fail += 1;
                            self.fail();
                            return;
                        }
                    }
                }
                if self.g_next == self.ng && self.ho_retry {
                    self.ho_retry = false;
                    match self.decode_header(HO_COPIES) {
                        Some(h) if h.header_only() => {
                            self.stats.header_ok += 1;
                            self.map = Map::new(l, ho_end, Pilots::Robust);
                            self.hdr = Some(h);
                        }
                        _ => {
                            self.stats.header_fail += 1;
                            self.fail();
                            return;
                        }
                    }
                }
                if self.g_next == self.ng {
                    let b = self.finish();
                    self.stats.bursts += 1;
                    out.push(b);
                    self.last_end = self.window_start(self.ng - 1) + l.n as u64;
                    self.phase = Phase::Idle;
                    self.need = self.written;
                } else {
                    let gn = self.g_next;
                    self.wshift[gn] = self.next_shift(gn);
                    self.need = self.window_start(gn) + l.n as u64;
                }
            }
        }
    }

    fn fail(&mut self) {
        self.phase = Phase::Idle;
        self.need = self.written;
    }

    fn window_start(&self, g: usize) -> u64 {
        (self.win0 as i64 + (g * self.l.sym()) as i64 + self.wshift[g] as i64) as u64
    }

    /// Test hook: turn the sample-clock drift tracking off (on by
    /// default).
    #[doc(hidden)]
    pub fn set_drift_tracking(&mut self, on: bool) {
        self.track_drift = on;
    }

    /// Impulse blanker ([`crate::blanker`]) on (the default) or off.
    ///
    /// # Panics
    /// After the first audio (the output delay would jump).
    pub fn set_blanker(&mut self, on: bool) {
        self.blanker.set_on(on);
    }

    /// Blanked fraction of the audio behind baseband `[start, start + n)`.
    fn blanked_frac(&self, start: u64, n: usize) -> f32 {
        if !self.blanker.is_on() {
            return 0.0;
        }
        let s: u32 = (start..start + n as u64)
            .map(|i| self.blanked[(i & MASK) as usize] as u32)
            .sum();
        s as f32 / (n * BLANK_SPAN) as f32
    }

    /// DFT of the carriers over `[start, start + n)`, derotated by the
    /// current frequency estimate (phase referenced to `win0`).
    fn dft(&self, start: u64, out: &mut [Complex]) {
        let n = self.l.n;
        let mut bufa = [Complex::ZERO; N_MAX];
        let buf = &mut bufa[..n];
        let w = -core::f64::consts::TAU * self.f_hz / FS;
        let base = start as f64 - self.win0 as f64;
        for (i, v) in buf.iter_mut().enumerate() {
            let (s, c) = (w * (base + i as f64)).sin_cos();
            *v = self.at(start + i as u64) * Complex::new(c as f32, s as f32);
        }
        for (c, o) in out.iter_mut().enumerate() {
            let k = self.l.bin_mod(c, n);
            let mut idx = 0;
            let mut acc = Complex::ZERO;
            for v in buf.iter() {
                acc += *v * self.tw[idx];
                idx += k;
                if idx >= n {
                    idx -= n;
                }
            }
            *o = acc;
        }
    }

    /// Lag-`lag` autocorrelation over `[lo, hi)`.
    fn autocorr(&self, lo: u64, hi: u64, lag: u64) -> Complex {
        let mut acc = (0f64, 0f64);
        for i in lo..hi {
            let p = self.at(i + lag) * self.at(i).conj();
            acc.0 += p.re as f64;
            acc.1 += p.im as f64;
        }
        Complex::new(acc.0 as f32, acc.1 as f32)
    }

    /// Matched filter on B, fine timing, fine frequency and noise.
    fn sync(&mut self) -> bool {
        let l = self.l;
        let (n, nc, a_to_b, e_win) = (l.n, l.nc, l.a_to_b(), l.e_win);
        let c = self.cur;
        let lo = c.n_pk as i64 - l.sc_peak_to_a() + a_to_b as i64 - l.mf_span as i64;
        if lo < self.oldest() as i64 + a_to_b as i64 {
            return false;
        }
        let lo = lo as u64;
        let nl = 2 * l.mf_span + 1;
        let w = -core::f64::consts::TAU * c.f1 / FS;
        for m in 0..nl + n - 1 {
            let (s, co) = (w * m as f64).sin_cos();
            self.seg[m] = self.at(lo + m as u64) * Complex::new(co as f32, s as f32);
        }
        let eb: f32 = self.bt.iter().map(|v| v.norm_sqr()).sum();
        let mut es: f32 = self.seg[..n].iter().map(|v| v.norm_sqr()).sum();
        let mut best = 0usize;
        for d in 0..nl {
            if d > 0 {
                es += self.seg[d + n - 1].norm_sqr() - self.seg[d - 1].norm_sqr();
            }
            let mut acc = Complex::ZERO;
            for (s, b) in self.seg[d..d + n].iter().zip(self.bt.iter()) {
                acc += *s * b.conj();
            }
            self.corr[d] = acc.norm_sqr();
            self.rho[d] = self.corr[d] / (eb * es.max(1e-30));
            if self.corr[d] > self.corr[best] {
                best = d;
            }
        }
        if self.rho[best] < l.rho_min {
            return false;
        }
        // Energy window: middle of the plateau of windows that capture
        // nearly the maximal energy (`Layout::plateau`), so a weak late path
        // at sync time, which can be the stronger one a second later, stays
        // inside it.
        let mut ew: f32 = self.corr[..e_win].iter().sum();
        self.ews.fill(0.0);
        self.ews[0] = ew;
        for t in 1..nl - e_win + 1 {
            ew += self.corr[t + e_win - 1] - self.corr[t - 1];
            self.ews[t] = ew;
        }
        let ews = &self.ews;
        let emax = ews[..=nl - e_win].iter().fold(0f32, |m, v| m.max(*v));
        let thr = l.plateau;
        let first = ews.iter().position(|&v| v >= thr * emax).unwrap_or(0);
        let last = (0..=nl - e_win).rev().find(|&t| ews[t] >= thr * emax).unwrap_or(first);
        let tm = (first + last) / 2;
        let tau = lo + tm as u64;
        let cmax = self.corr[tm..tm + e_win].iter().fold(0f32, |m, v| m.max(*v));
        let fp = (tm..tm + e_win).find(|&d| self.corr[d] >= 0.3 * cmax).unwrap_or(tm);
        self.first_path = lo + fp as u64;
        self.win0 = tau - l.b_backoff as u64;
        self.df_res = 0.0;

        // Fine frequency from A's clean region (every path inside A):
        // lags of one, two and four periods.
        let a_lo = tau + e_win as u64 + RAMP as u64 - a_to_b as u64;
        let a_hi = tau - l.cp as u64;
        let tau_l = core::f64::consts::TAU;
        let p1 = l.a_period() as u64;
        let pa = self.autocorr(a_lo, a_hi - p1, p1);
        let mut f = pa.arg() as f64 * FS / (tau_l * p1 as f64);
        for lag in [2 * p1, 4 * p1] {
            let p = self.autocorr(a_lo, a_hi - lag, lag);
            let pred = Complex::from_polar(1.0, (-tau_l * f * lag as f64 / FS) as f32);
            f += (p * pred).arg() as f64 * FS / (tau_l * lag as f64);
        }
        // Data-aided refinement: correlate the known A+B waveform at the
        // strongest path in six segments (four over A after its ramp, two
        // over B's CP and useful part) and fit the phase slope. Uses all of
        // the known signal coherently, so at low SNR it is several times
        // more precise than the lag products above.
        let t0 = lo + best as u64 - a_to_b as u64;
        let qa = (l.a_len - 2 * RAMP) / 4;
        let qb = (a_to_b + n - l.a_len) / 2;
        let segs: [(usize, usize); 6] = [
            (2 * RAMP, 2 * RAMP + qa),
            (2 * RAMP + qa, 2 * RAMP + 2 * qa),
            (2 * RAMP + 2 * qa, 2 * RAMP + 3 * qa),
            (2 * RAMP + 3 * qa, l.a_len),
            (l.a_len, l.a_len + qb),
            (l.a_len + qb, a_to_b + n),
        ];
        let mut ph = [(0f64, 0f64, 0f64); 6];
        for (k, &(m0, m1)) in segs.iter().enumerate() {
            let w = -tau_l * f / FS;
            let mut acc = (0f64, 0f64);
            for m in m0..m1 {
                let t = t0 + m as u64;
                let (sn, cs) = (w * (t as f64 - tau as f64)).sin_cos();
                let v = self.at(t) * Complex::new(cs as f32, sn as f32) * self.pre[m].conj();
                acc.0 += v.re as f64;
                acc.1 += v.im as f64;
            }
            ph[k] = (
                0.5 * (m0 + m1) as f64,
                acc.1.atan2(acc.0),
                (acc.0 * acc.0 + acc.1 * acc.1).sqrt(),
            );
        }
        // Unwrap along the segments, then weighted least squares.
        for k in 1..6 {
            let mut d = ph[k].1 - ph[k - 1].1;
            d -= tau_l * (d / tau_l).round();
            ph[k].1 = ph[k - 1].1 + d;
        }
        let sw: f64 = ph.iter().map(|p| p.2).sum();
        if sw > 0.0 {
            let tm: f64 = ph.iter().map(|p| p.2 * p.0).sum::<f64>() / sw;
            let pm: f64 = ph.iter().map(|p| p.2 * p.1).sum::<f64>() / sw;
            let num: f64 = ph.iter().map(|p| p.2 * (p.0 - tm) * (p.1 - pm)).sum();
            let den: f64 = ph.iter().map(|p| p.2 * (p.0 - tm) * (p.0 - tm)).sum();
            if den > 0.0 {
                f += num / den * FS / tau_l;
            }
        }
        self.f_hz = f;

        // Accept or reject on the whole known preamble (A and B) at the
        // refined frequency: its normalised correlation has a much lower
        // noise floor than B's alone, and taking the best lag in the
        // energy window copes with two-path fading.
        let et: f32 = self.pre.iter().map(|v| v.norm_sqr()).sum();
        let w = -tau_l * f / FS;
        let mut rho_full = 0f32;
        for d in tm..tm + e_win {
            let t0 = lo + d as u64 - a_to_b as u64;
            let (mut acc, mut er) = (Complex::ZERO, 0f32);
            for (m, &p) in self.pre.iter().enumerate() {
                let t = t0 + m as u64;
                let (sn, cs) = (w * (t as f64 - tau as f64)).sin_cos();
                let v = self.at(t) * Complex::new(cs as f32, sn as f32);
                acc += v * p.conj();
                er += v.norm_sqr();
            }
            rho_full = rho_full.max(acc.norm_sqr() / (et * er).max(1e-30));
        }
        if let Some(lg) = self.rho_log.as_mut() {
            lg.push(rho_full);
        }
        if rho_full < l.rho_full_min {
            return false;
        }

        // Noise per bin from A's empty carriers: at least four windows
        // spanning the clean part of A, more (half-overlapping) when A is
        // long, since a narrow numerology has few empty bins per window.
        let mut ya = [Complex::ZERO; NC_MAX];
        let y = &mut ya[..nc];
        let mut tot = 0f32;
        let mut cnt = 0usize;
        let mut prof = [0f32; NC_MAX];
        let mut pcnt = [0f32; NC_MAX];
        let span = a_hi - n as u64 - a_lo;
        let nwin = (1 + 2 * span / n as u64).max(4);
        for k in 0..nwin {
            let start = a_lo + span * k / (nwin - 1);
            self.dft(start, y);
            for (cc, v) in y.iter().enumerate() {
                if l.a_empty(cc) {
                    let e = v.norm_sqr();
                    tot += e;
                    cnt += 1;
                    for (k, (p, q)) in prof[..nc].iter_mut().zip(pcnt[..nc].iter_mut()).enumerate() {
                        if k.abs_diff(cc) <= l.noise_half {
                            *p += e;
                            *q += 1.0;
                        }
                    }
                }
            }
        }
        let glob = tot / cnt as f32;
        // Local average shrunk towards the global one.
        let ps = l.noise_pseudo;
        for (s, (p, q)) in self.sigma2[..nc].iter_mut().zip(prof.iter().zip(pcnt.iter())) {
            *s = ((p + ps * glob) / (q + ps)).max(1e-30);
        }
        true
    }

    /// Extracts grid symbol `g`, as if its FFT window had not been moved
    /// for clock drift (the shift's phase slope is taken out).
    fn extract(&mut self, g: usize) {
        let l = self.l;
        let nc = l.nc;
        let mut ya = [Complex::ZERO; NC_MAX];
        self.dft(self.window_start(g), &mut ya[..nc]);
        // A window partly zeroed by the blanker keeps (1 − b) of every
        // carrier: scale it back (the gap's interference goes into the
        // noise in `finish`).
        let bf = self.blanked_frac(self.window_start(g), l.n);
        self.bfrac[g] = bf;
        let gain = if bf > 0.0 {
            1.0 / (1.0 - bf.min(BLANK_ERASE))
        } else {
            1.0
        };
        let m = self.wshift[g];
        for (c, v) in ya[..nc].iter().enumerate() {
            let mut y = *v * self.center_rot[c] * gain;
            if m != 0 {
                let a = -core::f64::consts::TAU * (l.bin(c) as f64 * m as f64) / l.n as f64;
                y = y * Complex::from_polar(1.0, a as f32);
            }
            self.grid[g * nc + c] = y;
        }
    }

    /// Decodes the header from `copies` header copies (2 only for the
    /// retry of a header-only burst). No frequency correction here: from B
    /// and the header pilots it is noisier at low SNR than the preamble
    /// estimate it would refine; the Wiener model absorbs the residual and
    /// the data stage tracks it over the whole burst.
    fn decode_header(&mut self, copies: usize) -> Option<Header> {
        let l = self.l;
        let nc = l.nc;
        // Grid symbols up to the end of the last header copy.
        let ng = l.pre_syms() + (copies - 1) * l.hdr_syms;
        if let Some(gn) = self.genie {
            let start = self.start_sample() as i64;
            if (start - gn.start as i64).abs() > (AUDIO_RATE * 0.005) as i64 {
                return None;
            }
            return Some(Header {
                constellation: gn.constellation.code(),
                nsym: gn.data_symbols as u16,
                user: gn.user,
                extra: 0,
                sparse: gn.pilots == Pilots::Sparse,
                mode: DataMode::PLAIN,
            });
        }
        let map = Map::new(l, ng, Pilots::Robust);
        self.est
            .load(&self.grid, ng, &self.sigma2[..nc], |c, g| known(&map, c, g));
        let p_h = self.est.channel_power();
        self.est.select_model(ng, p_h, &l.fd);
        let pq = qpsk();
        let dc = l.data_carriers(Pilots::Robust);
        let per = l.robust.period;
        for g in 1..ng {
            for (j, &cc) in self.dcar_r[g % per].iter().enumerate() {
                let c = cc as usize;
                let (h, mse) = self.est.estimate(c, g, ng, p_h);
                let cell = (g - 1) * dc + j;
                est::demap(
                    self.grid[g * nc + c],
                    h,
                    mse,
                    self.sigma2[c],
                    &pq,
                    &mut self.slot_llr[cell * 2..cell * 2 + 2],
                );
            }
        }
        let hs = l.hdr_slots();
        let a = interleave_step(hs);
        let mut coded = [0f32; HDR_CODED];
        for j in 0..copies * hs {
            let (k, i) = (j / hs, j % hs);
            let s = 1.0 - 2.0 * scramble(SEED_HDR, j) as f32;
            coded[j % HDR_CODED] += s * self.slot_llr[k * hs + (a * i) % hs];
        }
        parse_header(&viterbi(&coded), l)
    }

    /// Stream index of the burst's first audio sample, from the first
    /// path's timing.
    fn start_sample(&self) -> u64 {
        let a_start = self.first_path as i64 - self.l.a_to_b() as i64;
        // TX and RX filters, (len − 1)/2 samples of delay each, and the
        // blanker's.
        let gd = (self.l.filter_len - 1) as i64 + self.blanker.delay() as i64;
        let start = DEC as i64 * a_start + (DEC as i64 - 1) - gd;
        start.max(0) as u64
    }
}
