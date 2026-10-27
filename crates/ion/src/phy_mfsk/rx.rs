//! Streaming MFSK receiver. All work happens at fixed points of the
//! baseband sample count (every quarter preamble symbol), so the output
//! does not depend on how the audio is chunked.

use core::f32::consts::TAU;
use std::collections::VecDeque;

use ion_dsp::dsp::{AUDIO_RATE, Complex, Decimator, Nco, lowpass_taps};

use super::frame::{Fmt, NBINS, PRE_LEN};
use super::util::Fft320;
use super::{BB_RATE, Expect, MfskBurst, MfskLayout, MfskMode, SYM};
use crate::blanker::Blanker;

mod acquire;
mod data;
mod detect;
mod model;

use detect::{DWIN, NCOL, NK};
use model::Params;

/// Baseband ring: 16.4 s.
const RING: usize = 1 << 17;
const MASK: usize = RING - 1;
/// Detector hop: a quarter symbol.
const Q: usize = SYM / 4;
/// Neighbour bins kept on each side of a grid.
const EXT: usize = 2;
/// Bins per symbol in the energy arrays, the largest grid's (R0: −2..50);
/// a smaller grid uses the first `Grid::ne` of them.
pub(super) const NE: usize = NBINS + 2 * EXT;
/// Front-end filter (48 kHz): Blackman-windowed sinc, flat to ±1.30 kHz
/// (both grids and their neighbour bins), stop band from ±1.82 kHz. The
/// mix-down's mirror image of a tone at f lands at −2·`center_hz` − f, as
/// close as −1.83 kHz at a 1500 Hz centre: R0's 50 Hz grid would shrug it
/// off (the image falls on the grid), R1's 71.4 Hz grid would not.
const FE_TAPS: usize = 511;
const FE_CUTOFF_HZ: f32 = 1560.0;
/// A candidate this strong makes the receiver busy before its header has
/// decoded (noise rarely gets there; a burst at −14 dB does).
const Z_BUSY: f32 = 3.0;
/// Extra samples to wait for beyond a window's end.
const MARGIN: i64 = 8;
/// LLR clamp.
pub(super) const LLR_MAX: f32 = 12.0;

type Energies = [f32; NE];

/// Counters for diagnostics.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RxStats {
    /// Detector peaks above threshold.
    pub detections: u64,
    /// Candidates accepted after fine sync.
    pub accepted: u64,
    /// Headers that passed their CRC.
    pub headers: u64,
    /// Analysis window (baseband samples) and model of the last burst
    /// delivered: Rician A and S at offsets −2..+2.
    pub last_window: usize,
    pub last_a: [f32; 5],
    pub last_s: [f32; 5],
    /// Per analysis window (shortest first) of the last header decoded:
    /// estimated mutual information, window-end offset, signal energy.
    pub last_mi: [f32; 3],
    pub last_d: [i32; 3],
    pub last_gtot: [f32; 3],
    /// Bursts accepted on the expected-header test (not decoded).
    pub known_headers: u64,
    /// Score (nats) of the last expected-header test, and its margin over
    /// the best other header.
    pub last_known_score: f32,
    pub last_known_margin: f32,
}

/// The tone grid and analysis windows of one mode's header and data
/// symbols (R0's is also the preamble's).
struct Grid {
    fmt: Fmt,
    /// Bins in the energy arrays: the grid plus `EXT` each side.
    ne: usize,
    /// Analysis windows, all ending at the symbol end, longest last (the
    /// whole symbol, the timing reference).
    wins: [usize; 3],
    /// Tone tables e^{−j2π f_b n / 8000} for ext bins, n < sym.
    tones: Vec<Complex>,
}

impl Grid {
    fn new(mode: MfskMode) -> Self {
        let fmt = Fmt::of(mode);
        let ne = fmt.nbins + 2 * EXT;
        let mut tones = vec![Complex::ZERO; ne * fmt.sym];
        for e in 0..ne {
            let f = fmt.bin_hz(e as i32 - EXT as i32) as f64;
            for n in 0..fmt.sym {
                let a = -core::f64::consts::TAU * f * n as f64 / BB_RATE as f64;
                tones[e * fmt.sym + n] = Complex::new(a.cos() as f32, a.sin() as f32);
            }
        }
        let wins = match mode {
            // The last 20 ms (immune to echoes up to ~7 ms), 24 ms, and the
            // whole 28 ms symbol.
            MfskMode::R0 => [160, 192, 224],
            // The last 10 and 12 ms (past 4 and 2 ms echoes), and the
            // whole 14 ms symbol.
            MfskMode::R1 => [80, 96, 112],
        };
        Self { fmt, ne, wins, tones }
    }

    /// Baseband end (from `t0c`, before any window offset) of header/data
    /// symbol `g` (0 = the first header symbol).
    fn sym_end(&self, g: usize) -> i64 {
        (PRE_LEN * SYM + (g + 1) * self.fmt.sym) as i64
    }
}

/// A detector candidate.
struct Cand {
    /// Coarse start of preamble symbol 0 (baseband index).
    t0c: i64,
    f0: f32,
    z: f32,
    synced: bool,
    /// Preamble energy over noise after fine sync.
    zfine: f32,
    f: f32,
    /// Window-end offset per preamble analysis window.
    d: [i32; 3],
    /// Energies of the preamble and of the R0 header symbols received so
    /// far, per R0 window.
    e: [Vec<Energies>; 3],
    /// Energies of the R1 header symbols received so far, per R1 window.
    e1: [Vec<Energies>; 3],
    tried: [bool; 4],
    /// Inside the expected slot.
    slot: bool,
}

/// Tone ext-bins of one known symbol.
#[derive(Clone, Copy)]
struct Known {
    n: usize,
    e: [usize; 3],
    /// Baseband frequency of each tone, Hz.
    hz: [f32; 3],
    /// Nominal end of the symbol, baseband samples from `t0c`.
    end: i64,
    vals: usize,
    /// Value index of each tone (header symbols), for the MI estimate.
    v: [u16; 3],
    hop: usize,
    fmt: Fmt,
}

/// A burst whose header has decoded.
struct Active {
    mode: MfskMode,
    /// Symbol-mapped data (`modulate_symbols`).
    symbols: bool,
    user: u16,
    extra: u16,
    nbits: usize,
    hsyms: usize,
    dsyms: usize,
    t0c: i64,
    /// Window-end offset of the chosen window.
    d: i32,
    w: usize,
    f: f32,
    f_sum: f64,
    params: Params,
    next: usize,
    energies: Vec<Energies>,
    acc: Complex,
    mag: f32,
    /// Channel measurement over the data symbols: Σ c2·c1*, Σ|c1|²,
    /// Σ|c2|² and their noise of the two halves of each decided tone
    /// (Doppler); the previous symbol's tone (ext bin) and, per window
    /// (whole symbol, and starting 4 ms in), Σ of its energy and of the
    /// current tone's, noise taken out (echoes).
    coh: Complex,
    coh_p: [f32; 2],
    coh_n: f32,
    prev_e: Option<usize>,
    isi: [[f32; 2]; 2],
    snr_db: f32,
    start_sample: u64,
    /// Clock drift: window-end offset of the whole-symbol window (the
    /// timing reference), the integer shift applied to each data symbol,
    /// the early-late sums of the current block and the block results
    /// (data symbol index, drift in samples).
    dref: i32,
    shifts: Vec<i32>,
    shift: i32,
    tnum: f64,
    tden: f64,
    tcnt: usize,
    blocks: Vec<(f64, f64)>,
    /// Drift applied, samples per symbol.
    drift: f64,
    /// Blanked fraction of each data symbol's analysis window.
    blanked: Vec<f32>,
}

pub struct MfskRx {
    center_hz: f32,
    layout: MfskLayout,
    /// Front-end filter taps.
    fe_taps: usize,
    nco: Nco,
    dec: Decimator,
    ring: Vec<Complex>,
    n_bb: u64,
    fft: Fft320,
    /// R0's grid (also the preamble's) and R1's.
    grids: [Grid; 2],
    cols: Vec<f32>,
    col_mean: Vec<f32>,
    n_col: u64,
    bavg: Vec<f32>,
    zhist: VecDeque<(i64, f32, f32, i32)>,
    cands: Vec<Cand>,
    bursts: Vec<Active>,
    /// Extents of decoded bursts (baseband), for suppressing detections
    /// inside them.
    spans: VecDeque<(i64, i64)>,
    stats: RxStats,
    track_drift: bool,
    /// The modes whose headers are tried (R0, R1).
    modes: [bool; 2],
    expect: Option<Expect>,
    /// Impulse blanker on the audio, and how many of the audio samples
    /// around each baseband sample it zeroed (ring like `ring`).
    blanker: Blanker,
    blanked: Vec<u8>,
}

fn gi(mode: MfskMode) -> usize {
    match mode {
        MfskMode::R0 => 0,
        MfskMode::R1 => 1,
    }
}

impl MfskRx {
    pub fn new(center_hz: f32) -> Self {
        Self::with_layout(MfskLayout::FULL, center_hz)
    }

    /// A receiver of R0 and R1 in `layout` (one layout per receiver).
    /// Scaled layouts get the full band's filter in their own samples,
    /// widened by the extra detector range.
    pub fn with_layout(layout: MfskLayout, center_hz: f32) -> Self {
        let k = layout.k;
        let fe_taps = (FE_TAPS - 1) * k + 1;
        let cutoff = (FE_CUTOFF_HZ + 25.0 * (layout.hmax - MfskLayout::FULL.hmax).max(0) as f32) / k as f32;
        Self {
            center_hz,
            layout,
            fe_taps,
            nco: Nco::new(center_hz, AUDIO_RATE),
            dec: Decimator::new(layout.dec(), lowpass_taps(fe_taps, cutoff, AUDIO_RATE)),
            ring: vec![Complex::ZERO; RING],
            n_bb: 0,
            fft: Fft320::new(),
            grids: [Grid::new(MfskMode::R0), Grid::new(MfskMode::R1)],
            cols: vec![0.0; NCOL * NK],
            col_mean: vec![0.0; NCOL],
            n_col: 0,
            bavg: vec![0.0; NK],
            zhist: VecDeque::new(),
            cands: Vec::new(),
            bursts: Vec::new(),
            spans: VecDeque::new(),
            stats: RxStats::default(),
            track_drift: true,
            modes: [true; 2],
            expect: None,
            blanker: Blanker::new(center_hz),
            blanked: vec![0; RING],
        }
    }

    /// Test hook: turn the sample-clock drift tracking off (on by
    /// default).
    #[doc(hidden)]
    pub fn set_drift_tracking(&mut self, on: bool) {
        self.track_drift = on;
    }

    /// Try only the headers of `modes` (both by default): a candidate that
    /// is none of them is given up as soon as their headers fail.
    pub fn set_modes(&mut self, modes: &[MfskMode]) {
        self.modes = [modes.contains(&MfskMode::R0), modes.contains(&MfskMode::R1)];
    }

    /// Impulse blanker ([`crate::blanker`]) on (the default) or off.
    ///
    /// # Panics
    /// After the first audio (the output delay would jump).
    pub fn set_blanker(&mut self, on: bool) {
        self.blanker.set_on(on);
    }

    /// Blanked fraction of the audio behind baseband `[start, start + w)`.
    fn blanked_frac(&self, start: i64, w: usize) -> f32 {
        if !self.blanker.is_on() || start < 0 {
            return 0.0;
        }
        let s: u32 = (0..w).map(|n| self.blanked[(start as usize + n) & MASK] as u32).sum();
        // Audio samples behind each baseband sample's count.
        let span = 2 * (self.layout.dec() / 2) + 1;
        s as f32 / (w * span) as f32
    }

    pub fn layout(&self) -> MfskLayout {
        self.layout
    }

    pub fn center_hz(&self) -> f32 {
        self.center_hz
    }

    /// What to expect, from a shared clock and a known schedule: a burst
    /// starting near `start_sample`, perhaps with a known header (see
    /// [`Expect`]). Bursts elsewhere are still received as usual. `None`
    /// goes back to plain blind reception.
    pub fn expect(&mut self, e: Option<Expect>) {
        self.expect = e;
    }

    /// Audio sample of a candidate's preamble start (baseband `t0c`): the
    /// audio index of baseband sample k is dec·k minus the filter delay.
    fn audio_start(&self, t0c: i64) -> i64 {
        let dec = self.layout.dec() as i64;
        let fe_delay = (self.fe_taps as i64 - 1) / 2 - (dec - 1);
        dec * t0c - fe_delay - self.blanker.delay() as i64
    }

    /// True when a preamble starting at baseband `t0c` falls in the
    /// expected slot (one detector hop of slack).
    fn in_slot(&self, t0c: i64) -> bool {
        self.expect.is_some_and(|e| {
            let off = self.audio_start(t0c) - e.start_sample as i64;
            off.unsigned_abs() <= e.tol_samples + (self.layout.dec() * Q) as u64
        })
    }

    pub fn process(&mut self, samples: &[f32], out: &mut Vec<MfskBurst>) {
        for &x in samples {
            let x = self.blanker.push(x);
            let z = self.nco.mix_down(x);
            if let Some(b) = self.dec.push(z) {
                self.ring[self.n_bb as usize & MASK] = b;
                // Audio zeroed around the front-end filter's centre.
                self.blanked[self.n_bb as usize & MASK] =
                    self.blanker
                        .blanked_count((self.fe_taps - 1) / 2, self.layout.dec() / 2) as u8;
                self.n_bb += 1;
                if self.n_bb.is_multiple_of(Q as u64) {
                    self.on_column(out);
                }
            }
        }
    }

    /// True while a burst is being received.
    pub fn busy(&self) -> bool {
        !self.bursts.is_empty() || self.cands.iter().any(|c| c.synced && c.zfine >= Z_BUSY)
    }

    #[doc(hidden)]
    pub fn stats(&self) -> RxStats {
        self.stats
    }

    fn on_column(&mut self, out: &mut Vec<MfskBurst>) {
        if self.n_bb as usize >= DWIN {
            self.detect();
        }
        let mut cands = std::mem::take(&mut self.cands);
        cands.retain_mut(|c| self.service_cand(c));
        self.cands = cands;
        let mut bursts = std::mem::take(&mut self.bursts);
        bursts.retain_mut(|b| !self.service_burst(b, out));
        self.bursts = bursts;
        let now = self.n_bb as i64;
        while self.spans.front().is_some_and(|s| s.1 < now - 4 * RING as i64 / 8) {
            self.spans.pop_front();
        }
    }

    fn available(&self, start: i64, len: usize) -> bool {
        start >= 0 && start + len as i64 <= self.n_bb as i64 && (self.n_bb as i64 - start) < RING as i64
    }

    /// Energies of the ext bins of grid `g` over `[start, start + w)`,
    /// frequency offset `f` removed.
    fn energies(&self, g: &Grid, start: i64, w: usize, f: f32, out: &mut Energies) {
        let step = Complex::from_polar(1.0, -TAU * f / BB_RATE);
        let mut ph = Complex::new(1.0, 0.0);
        let mut y = [Complex::ZERO; SYM];
        for (n, v) in y.iter_mut().enumerate().take(w) {
            *v = self.ring[(start as usize + n) & MASK] * ph;
            ph = ph * step;
        }
        let sym = g.fmt.sym;
        for (e, o) in out.iter_mut().enumerate().take(g.ne) {
            let t = &g.tones[e * sym..e * sym + w];
            let (mut re, mut im) = (0f32, 0f32);
            for (a, b) in y[..w].iter().zip(t) {
                re += a.re * b.re - a.im * b.im;
                im += a.re * b.im + a.im * b.re;
            }
            *o = re * re + im * im;
        }
    }

    /// Correlation with one tone at `freq` (baseband Hz) over
    /// `[start, start + w)`.
    fn corr1(&self, start: i64, w: usize, freq: f32) -> Complex {
        let step = Complex::from_polar(1.0, -TAU * freq / BB_RATE);
        let mut ph = Complex::new(1.0, 0.0);
        let mut acc = Complex::ZERO;
        for n in 0..w {
            acc += self.ring[(start as usize + n) & MASK] * ph;
            ph = ph * step;
        }
        acc
    }
}
