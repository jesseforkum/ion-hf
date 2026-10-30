//! The receiving side's acquisition of stream periods: for each period, a
//! prediction from [`Track`], the known-head search ([`locate`]) in the
//! audio kept around it, and a fresh OFDM receiver cued at what it found
//! (trusted: no header needs to be heard); the receiver's own header
//! check when the search finds nothing. MFSK periods go to the link's MFSK
//! receiver with an expected header and slot. Each period becomes one
//! [`Incoming`], heard or not, so the station's rule sees every period.
//!
//! Positions are on the receiver's clock: every audio sample counted,
//! including those our own transmissions muted ([`Acq::skip`]).

use std::collections::VecDeque;

use super::sync::{Track, baseband, locate, mark};
use super::{Fam, Listen, OPTS, SLOT_S, STREAM};
use crate::level::{Guard, Wave};
use crate::link::audio::{CENTER_HZ, numerology, tx_options};
use crate::link::harq::Soft;
use crate::link::station::Incoming;
use crate::phy::{self, Constellation, Cue, DataMode, ExpectedHeader, PhyRx, RxBurst};
use crate::phy_mfsk;
use ion_dsp::dsp::{AUDIO_RATE, Complex};

const FS: f64 = AUDIO_RATE as f64;
/// Audio kept for the search, samples (3 s: the widest window, the head,
/// and a block in hand).
const HIST: usize = 3 * 48_000;
/// Audio fed to a period's receiver ahead of where its burst should
/// start, samples.
const PRE: usize = 9_600;

/// One period in hand.
struct Cur {
    /// The OFDM option tried, and its receiver with the clock sample its
    /// input starts at and where the head was found (none: uncued).
    ofdm: Option<(usize, PhyRx, u64, Option<f64>)>,
    /// An MFSK option the link's MFSK receiver expects.
    mfsk: Option<usize>,
    /// Searched yet (OFDM).
    searched: bool,
    center: f64,
    half: f64,
    f_hz: f64,
    /// Give up on it at this clock sample.
    deadline: u64,
}

/// What the timekeeping did, for the operator and the lab.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AcqStats {
    pub periods: u32,
    /// Heads found over the mark, the receiver's own header used instead,
    /// periods with nothing heard.
    pub found: u32,
    pub fallback: u32,
    pub missed: u32,
    /// The last prediction's half-window and the last search's error
    /// against it, ms.
    pub half_ms: f32,
    pub err_ms: f32,
}

pub struct Acq {
    listen: Option<Listen>,
    track: Track,
    k: u32,
    hist: VecDeque<f32>,
    hist_start: u64,
    clock: u64,
    last_tx_end: Option<u64>,
    cur: Option<Cur>,
    /// The last period handed over, waiting for the station's word on
    /// whether it decoded: (seen count it will bring, schedule, measured
    /// start, ppm, Hz).
    pending: Option<(u32, f64, f64, Option<f64>, f64)>,
    heads: Vec<(u16, usize, Vec<Complex>)>,
    pub stats: AcqStats,
}

impl Default for Acq {
    fn default() -> Self {
        Self::new()
    }
}

impl Acq {
    pub fn new() -> Self {
        Self {
            listen: None,
            track: Track::new(),
            k: 0,
            hist: VecDeque::with_capacity(HIST),
            hist_start: 0,
            clock: 0,
            last_tx_end: None,
            cur: None,
            pending: None,
            heads: Vec::new(),
            stats: AcqStats::default(),
        }
    }

    pub fn active(&self) -> bool {
        self.listen.is_some()
    }

    pub fn session(&self) -> Option<u8> {
        self.listen.as_ref().map(|l| l.session)
    }

    /// The carrier offset tracked, Hz (answers are pre-corrected by it).
    pub fn offset_hz(&self) -> f32 {
        if self.active() { self.track.f_hat as f32 } else { 0.0 }
    }

    /// The receiver's clock now, samples.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// Our own transmission ended at clock sample `at` (now if `None`).
    pub fn note_tx_end(&mut self, at: Option<u64>) {
        let at = at.unwrap_or(self.clock);
        self.last_tx_end = Some(at);
        // Our Go went after the station started listening: the first
        // period is timed from it.
        if let Some(l) = self.listen.as_ref()
            && self.k == 0
            && self.track.anchors() <= 1
            && self.cur.as_ref().is_none_or(|c| !c.searched)
        {
            self.track.seed(0.0, at as f64 + l.after_tx_s * FS);
            self.cur = None;
        }
    }

    /// The station's view (`Station::stream_listen`), before each block of
    /// audio. Returns the MFSK expectation to set, if it changed.
    pub fn set(&mut self, l: Option<Listen>) {
        let Some(l) = l else {
            self.listen = None;
            self.cur = None;
            self.pending = None;
            return;
        };
        let restart = self.listen.as_ref().is_none_or(|o| o.epoch != l.epoch || o.id != l.id);
        if restart {
            self.k = 0;
            self.cur = None;
            self.pending = None;
            let from = self.last_tx_end.unwrap_or(self.clock) as f64;
            self.track.seed(0.0, from + l.after_tx_s * FS);
            self.stats = AcqStats::default();
        } else if let Some((seen, t, rx, ppm, f)) = self.pending
            && l.seen >= seen
        {
            if l.last_ok && l.seen == seen {
                self.track.update(t, rx, ppm, f);
            }
            self.pending = None;
        }
        // The station answered since the period in hand was set up: set it
        // up again with the options it may now be in.
        if !restart
            && self.listen.as_ref().is_some_and(|o| o.opts != l.opts)
            && self.cur.as_ref().is_some_and(|c| !c.searched)
        {
            self.cur = None;
        }
        self.listen = Some(l);
    }

    /// `n` samples we did not hear (our own transmission).
    pub fn skip(&mut self, n: usize) {
        if n >= HIST {
            self.hist.clear();
            self.clock += n as u64;
            self.hist_start = self.clock;
        } else {
            self.push_hist(&vec![0.0; n]);
        }
    }

    fn push_hist(&mut self, x: &[f32]) {
        self.hist.extend(x);
        self.clock += x.len() as u64;
        while self.hist.len() > HIST {
            self.hist.pop_front();
            self.hist_start += 1;
        }
    }

    /// Kept audio from clock sample `a` to now (clamped to what is kept).
    fn since(&self, a: u64) -> (u64, Vec<f32>) {
        let a = a.max(self.hist_start);
        let i = (a - self.hist_start) as usize;
        (a, self.hist.range(i..).copied().collect())
    }

    fn head(&mut self, user: u16, nsym: usize) -> Vec<Complex> {
        if let Some(h) = self.heads.iter().find(|h| h.0 == user && h.1 == nsym) {
            return h.2.clone();
        }
        let nbits = phy::capacity_bits_with(phy::Numerology::Normal, super::pilots(), Constellation::Qpsk, nsym);
        let w: Vec<f32> = phy::modulate_full(
            numerology(Guard::Normal),
            Constellation::Qpsk,
            user,
            0,
            &vec![0; nbits],
            CENTER_HZ,
            &tx_options(super::OFDM_LEVEL),
        )
        .into_iter()
        .take(phy::OVERHEAD_AUDIO)
        .collect();
        let b = baseband(&w, 0.0);
        self.heads.push((user, nsym, b.clone()));
        b
    }

    fn cue_of(&self, user: u16, o: usize, start: u64, f_hz: f32) -> Cue {
        let opt = tx_options(super::OFDM_LEVEL);
        Cue {
            start,
            f_hz,
            constellation: Constellation::Qpsk,
            data_symbols: super::ofdm_symbols(o),
            user,
            extra: 0,
            pilots: super::pilots(),
            mode: DataMode::new(opt.spread, opt.data_gain_db),
            trust: true,
        }
    }

    /// The MFSK period the link's MFSK receiver should look for now:
    /// (clock sample of its start, tolerance, header), while one is due.
    pub fn mfsk_expect(&self) -> Option<(u64, u64, phy_mfsk::ExpectedHeader)> {
        let l = self.listen.as_ref()?;
        let c = self.cur.as_ref()?;
        let o = c.mfsk?;
        let Fam::Mfsk(m, _) = OPTS[o].fam else { return None };
        Some((
            c.center.max(0.0) as u64,
            c.half.max(480.0) as u64,
            phy_mfsk::ExpectedHeader {
                mode: m,
                user: super::period_user(o, l.session),
                extra: o as u16 | (l.id as u16) << 8,
                nbits: super::period_bits(o),
                symbols: true,
                user_mask: 0,
                extra_mask: 0,
            },
        ))
    }

    /// One block of audio heard (after the link's receivers took it).
    /// Periods it completes go to `out`.
    pub fn push(&mut self, x: &[f32], out: &mut Vec<Incoming>) {
        self.push_hist(x);
        if self.listen.is_none() {
            return;
        }
        if let Some(c) = self.cur.as_mut()
            && let Some((_, rx, _, _)) = c.ofdm.as_mut()
            && c.searched
        {
            let mut got = Vec::new();
            rx.process(x, &mut got);
            self.take_ofdm(got, out);
        }
        self.step(out);
    }

    fn sched(k: u32) -> f64 {
        k as f64 * SLOT_S * FS
    }

    fn step(&mut self, out: &mut Vec<Incoming>) {
        loop {
            let Some(l) = self.listen.clone() else { return };
            if self.cur.is_none() {
                let p = self.track.predict(Self::sched(self.k));
                let ofdm = l.opts.iter().copied().find(|&o| OPTS[o].fam == Fam::Ofdm);
                let mfsk = l.opts.iter().copied().find(|&o| OPTS[o].fam != Fam::Ofdm);
                let len = l.opts.iter().map(|&o| super::burst_samples(o)).max().unwrap_or(0);
                self.cur = Some(Cur {
                    ofdm: ofdm.map(|o| (o, PhyRx::with_numerology(numerology(Guard::Normal), CENTER_HZ), 0, None)),
                    mfsk,
                    searched: false,
                    center: p.center,
                    half: p.half,
                    f_hz: p.f_hz,
                    deadline: (p.center + p.half + len as f64 + 0.3 * FS).max(0.0) as u64,
                });
                self.stats.half_ms = (p.half / 48.0) as f32;
                if std::env::var("ION_ACQ_TRACE").is_ok() {
                    eprintln!(
                        "ACQ k {} opts {:?} center {:.3} half {:.1} ms clock {:.3}",
                        self.k,
                        l.opts,
                        p.center / FS,
                        p.half / 48.0,
                        self.clock as f64 / FS
                    );
                }
            }
            let c = self.cur.as_ref().unwrap();
            if !c.searched && c.ofdm.is_some() {
                let ready = c.center + c.half + phy::OVERHEAD_AUDIO as f64 + 960.0;
                if (self.clock as f64) < ready {
                    return;
                }
                self.search(&l);
                continue;
            }
            if self.clock >= c.deadline {
                let c = self.cur.take().unwrap();
                let o = l.opts[0];
                self.stats.periods += 1;
                self.stats.missed += 1;
                if std::env::var("ION_ACQ_TRACE").is_ok() {
                    eprintln!("ACQ missed k {} clock {:.3}", self.k, self.clock as f64 / FS);
                }
                out.push(self.incoming(&l, o, None, f32::NAN));
                let _ = c;
                self.k += 1;
                continue;
            }
            return;
        }
    }

    /// The known-head search for the period in hand; its receiver cued at
    /// the find, or left to its own header (expected) below the mark.
    fn search(&mut self, l: &Listen) {
        let (center, half, f_hz, o) = {
            let c = self.cur.as_ref().unwrap();
            (c.center, c.half, c.f_hz, c.ofdm.as_ref().unwrap().0)
        };
        let user = super::period_user(o, l.session);
        let nsym = super::ofdm_symbols(o);
        let head = self.head(user, nsym);
        let lo = (center - half - PRE as f64).max(0.0) as u64;
        let (a0, audio) = self.since(lo);
        let f0 = locate(&audio, center - a0 as f64, half, f_hz, &head);
        if std::env::var("ION_ACQ_TRACE").is_ok() {
            eprintln!("ACQ search {:?} at {:?}", f0, f0.map(|f| (a0 as f64 + f.at) / FS));
        }
        let found = f0.filter(|f| f.rho >= mark(f.lags));
        let (start, at) = match found {
            Some(f) => {
                let at = a0 as f64 + f.at;
                self.stats.err_ms = ((at - center) / 48.0) as f32;
                ((at as u64).saturating_sub(PRE as u64).max(a0), Some(at))
            }
            None => (a0, None),
        };
        let cue = at.map(|at| self.cue_of(user, o, (at - start as f64).round() as u64, f_hz as f32));
        let (_, feed) = self.since(start);
        let c = self.cur.as_mut().unwrap();
        c.searched = true;
        let (_, rx, s0, a) = c.ofdm.as_mut().unwrap();
        *s0 = start;
        *a = at;
        match cue {
            Some(q) => rx.cue(q),
            None => {
                let opt = tx_options(super::OFDM_LEVEL);
                rx.expect(&[ExpectedHeader {
                    constellation: Constellation::Qpsk,
                    data_symbols: nsym,
                    user,
                    extra: 0,
                    pilots: super::pilots(),
                    mode: DataMode::new(opt.spread, opt.data_gain_db),
                }]);
            }
        }
        let mut got = Vec::new();
        rx.process(&feed, &mut got);
        self.take_ofdm(got, &mut Vec::new());
        // A burst can't complete inside the search's own audio (it starts
        // there and lasts seconds), so nothing is lost by dropping `got`.
    }

    fn take_ofdm(&mut self, got: Vec<RxBurst>, out: &mut Vec<Incoming>) {
        let Some(l) = self.listen.clone() else { return };
        let Some(c) = self.cur.as_ref() else { return };
        let Some((o, _, s0, at)) = c.ofdm.as_ref() else { return };
        let user = super::period_user(*o, l.session);
        let need = super::period_bits(*o);
        let Some(b) = got
            .into_iter()
            .find(|b| !b.header_only && b.user == user && b.llrs.len() >= need)
        else {
            return;
        };
        let (o, s0, at) = (*o, *s0, *at);
        self.stats.periods += 1;
        if at.is_some() {
            self.stats.found += 1;
        } else {
            self.stats.fallback += 1;
        }
        let meas = at.unwrap_or((s0 + b.start_sample) as f64 + 10.0);
        let mut inc = self.incoming(&l, o, Some(Soft::Llrs(b.llrs[..need].to_vec())), b.snr_db);
        inc.chan = b.chan;
        out.push(inc);
        self.pending = Some((
            l.seen + 1,
            Self::sched(self.k),
            meas,
            Some(b.clock_ppm as f64),
            b.freq_offset_hz as f64,
        ));
        self.cur = None;
        self.k += 1;
    }

    /// A burst from the link's MFSK receiver: a period of ours if its
    /// header says so (`start` on our clock). Returns it as the station
    /// takes it, or gives the burst back.
    pub fn take_mfsk(&mut self, b: &phy_mfsk::MfskBurst, start: u64) -> Option<Incoming> {
        let l = self.listen.clone()?;
        if std::env::var("ION_ACQ_TRACE").is_ok() {
            eprintln!(
                "ACQ mfsk burst user {:x} extra {:x} start {:.3}",
                b.user,
                b.extra,
                start as f64 / FS
            );
        }
        if !super::is_period_user(b.user, l.session) || (b.extra >> 8) as u8 != l.id {
            return None;
        }
        let o = (b.extra & 0xFF) as usize;
        let Fam::Mfsk(_, r) = OPTS.get(o)?.fam else { return None };
        // A period later than the one in hand: those between were missed.
        let k = self.k_at(start as f64);
        let frames: Vec<Option<Vec<u8>>> = phy_mfsk::decode_short_frames_at(b, r, &[])
            .into_iter()
            .map(|f| f.map(|bits| crate::link::audio::bits_to_bytes(&bits)))
            .collect();
        let mut inc = self.incoming(&l, o, None, b.snr_db);
        inc.wave = Wave::Mfsk(b.mode);
        inc.frames = frames;
        inc.chan = b.chan;
        self.stats.periods += 1;
        self.stats.found += 1;
        // Its header decoded (user and stream id): that is the check, so
        // it anchors at once.
        self.track.update(
            Self::sched(k),
            start as f64,
            (b.clock_ppm != 0.0).then_some(b.clock_ppm as f64),
            b.freq_offset_hz as f64,
        );
        self.pending = None;
        self.cur = None;
        self.k = k + 1;
        Some(inc)
    }

    /// The period whose predicted start is nearest clock sample `x`.
    fn k_at(&self, x: f64) -> u32 {
        let d = |k: u32| (self.track.predict(Self::sched(k)).center - x).abs();
        (self.k.saturating_sub(2)..self.k + 4)
            .min_by(|&a, &b| d(a).total_cmp(&d(b)))
            .unwrap_or(self.k)
    }

    fn incoming(&self, l: &Listen, o: usize, soft: Option<Soft>, snr_db: f32) -> Incoming {
        let wave = match OPTS[o].fam {
            Fam::Ofdm => Wave::Ofdm(Guard::Normal),
            Fam::Mfsk(m, _) => Wave::Mfsk(m),
        };
        Incoming {
            wave,
            code: STREAM,
            user: super::period_user(o, l.session),
            extra: o as u16 | (l.id as u16) << 8,
            frames: Vec::new(),
            soft,
            snr_db,
            chan: crate::chan::ChannelMeas::of_snr(snr_db),
            slot: 0,
            grid: None,
        }
    }
}
