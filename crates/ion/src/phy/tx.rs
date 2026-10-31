//! Transmitter: burst synthesis, clip-and-filter PAPR reduction and the
//! 48 kHz upconversion.

use ion_dsp::TX_LEVEL;
use ion_dsp::dsp::{AUDIO_RATE, Complex};
use ion_dsp::fft::Fft;
use std::sync::atomic::{AtomicU8, Ordering};

use super::frame::*;
use super::{
    Constellation, HO_COPIES, Numerology, PREAMBLE_BOOST_DB, check_center, data_symbols, data_symbols_with,
    front_end_taps_for,
};

/// Transmitter options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TxOptions {
    /// Clip-and-filter PAPR reduction: clip the oversampled envelope of
    /// each header/data symbol at this many dB above its RMS, then restore
    /// the out-of-band bins (zero) and the pilots, over a few passes.
    /// `None` sends the symbols unclipped.
    pub clip_db: Option<f32>,
    /// Pilot pattern of the data symbols (named in the header).
    pub pilots: Pilots,
    /// DFT-spread the data cells of each data symbol (lower PAPR; named in
    /// the header).
    pub spread: bool,
    /// Data symbols (pilots included) this many dB above the preamble and
    /// header's reference level (named in the header).
    pub data_gain_db: f32,
    /// Clip level of the header symbols, when not `clip_db` (only with
    /// `clip_db` set).
    pub hdr_clip_db: Option<f32>,
    /// Whole-burst clip-and-filter after upconversion (Data2G's
    /// `tx_condition`), on top of or instead of `clip_db`.
    pub td_clip: Option<TdClip>,
}

/// Data2G's clipper: clip the 48 kHz envelope at `headroom_db` over the
/// burst's RMS envelope, the scale raised to `overshoot[i]` on pass `i`
/// (one pass per factor), then low-pass with the front-end filter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TdClip {
    pub headroom_db: f32,
    pub overshoot: &'static [f32],
}

/// Clip level of [`TxOptions::default`] (and so [`modulate`]), dB over the
/// symbol RMS. The link clips per constellation ([`TxOptions::for_burst`]).
pub const DEFAULT_CLIP_DB: f32 = 6.0;

/// Clip levels the link uses, dB over the symbol RMS: harder clips buy
/// mean power at the same peak until their distortion floor costs more.
pub const QPSK_CLIP_DB: f32 = 4.5;
pub const PSK8_CLIP_DB: f32 = 4.5;
pub const QAM16_CLIP_DB: f32 = 5.0;
/// 64QAM is clipped lightly: a 6 dB clip's distortion floor would cost it
/// about 1 dB.
pub const QAM64_CLIP_DB: f32 = 8.0;

impl Default for TxOptions {
    fn default() -> Self {
        Self {
            clip_db: Some(DEFAULT_CLIP_DB),
            pilots: Pilots::Robust,
            spread: false,
            data_gain_db: 0.0,
            hdr_clip_db: None,
            td_clip: None,
        }
    }
}

impl TxOptions {
    /// The options the link uses for constellation `c` with pilots `p`:
    /// [`QPSK_CLIP_DB`], [`PSK8_CLIP_DB`], [`QAM16_CLIP_DB`] or
    /// [`QAM64_CLIP_DB`].
    pub fn for_burst(c: Constellation, p: Pilots) -> Self {
        Self {
            clip_db: Some(clip_db_for(c)),
            pilots: p,
            spread: false,
            data_gain_db: 0.0,
            hdr_clip_db: None,
            td_clip: None,
        }
    }
}

/// Header clip of low-PAPR bursts, dB: keeps the QPSK header's peaks below
/// the preamble's (the header is not raised).
pub const LOW_PAPR_HDR_CLIP_DB: f32 = 3.5;

impl TxOptions {
    /// The link's low-PAPR options for constellation `c` with pilots `p`:
    /// DFT-spread data symbols, clipped at [`low_papr_clip_db`] and raised
    /// by [`low_papr_gain_db`] over the preamble and header, so that the
    /// data's peaks come up to the preamble's.
    pub fn low_papr(c: Constellation, p: Pilots) -> Self {
        Self {
            clip_db: Some(low_papr_clip_db(c)),
            pilots: p,
            spread: true,
            data_gain_db: low_papr_gain_db(c),
            hdr_clip_db: Some(LOW_PAPR_HDR_CLIP_DB),
            td_clip: None,
        }
    }
}

/// Clip of low-PAPR data symbols, dB over their RMS.
pub fn low_papr_clip_db(c: Constellation) -> f32 {
    match c {
        Constellation::Qpsk => 2.0,
        Constellation::Psk8 => 3.5,
        Constellation::Qam16 => 4.5,
        Constellation::Qam64 => 6.0,
    }
}

/// Data gain of low-PAPR bursts over the preamble and header, dB (a
/// multiple of [`GAIN_STEP_DB`]).
pub fn low_papr_gain_db(c: Constellation) -> f32 {
    match c {
        Constellation::Qpsk => 2.25,
        Constellation::Psk8 => 1.5,
        Constellation::Qam16 => 0.75,
        Constellation::Qam64 => 0.75,
    }
}

/// Measured in-band distortion of the low-PAPR clip of `c` as the receiver
/// sees it, dB below the signal (NormalWide).
fn low_papr_distortion_db(c: Constellation) -> f32 {
    match c {
        Constellation::Qpsk => 11.5,
        Constellation::Psk8 => 17.0,
        Constellation::Qam16 => 19.5,
        Constellation::Qam64 => 27.5,
    }
}

fn clip_db_for(c: Constellation) -> f32 {
    match c {
        Constellation::Qpsk => QPSK_CLIP_DB,
        Constellation::Psk8 => PSK8_CLIP_DB,
        Constellation::Qam16 => QAM16_CLIP_DB,
        Constellation::Qam64 => QAM64_CLIP_DB,
    }
}

/// In-band distortion of a clip-and-filter at `clip_db`, dB below the
/// signal, as the SNR report should take it out: measured on clean bursts
/// (`diag_clip_distortion`), piecewise linear in between. Above 7 dB it is
/// below the receiver's own estimation floor.
fn clip_distortion_db(clip_db: f32) -> f32 {
    const PTS: [(f32, f32); 4] = [(4.0, 17.3), (4.5, 19.1), (5.0, 21.0), (6.0, 26.0)];
    if clip_db >= 7.0 {
        return f32::INFINITY;
    }
    if clip_db <= PTS[0].0 {
        return PTS[0].1;
    }
    for w in PTS.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if clip_db <= x1 {
            return y0 + (y1 - y0) * (clip_db - x0) / (x1 - x0);
        }
    }
    // 6–7 dB: towards the 8 dB clip's negligible distortion.
    26.0 + 8.0 * (clip_db - 6.0)
}

/// 0: not set yet (`ION_TDCLIP` decides), 1: off, 2: on.
static TD_CLIP: AtomicU8 = AtomicU8::new(0);

/// Whether the link's data bursts go through [`TdClip`] instead of the
/// per-symbol clip and the low-PAPR mode: [`set_td_clip`], else
/// `ION_TDCLIP=1`. Read for every burst, so a change applies to the next.
pub fn td_clip_on() -> bool {
    match TD_CLIP.load(Ordering::Relaxed) {
        0 => {
            let on = std::env::var("ION_TDCLIP").is_ok_and(|v| v == "1");
            let _ = TD_CLIP.compare_exchange(0, 1 + on as u8, Ordering::Relaxed, Ordering::Relaxed);
            TD_CLIP.load(Ordering::Relaxed) == 2
        }
        v => v == 2,
    }
}

/// Switches [`TdClip`] on or off for the whole process.
pub fn set_td_clip(on: bool) {
    TD_CLIP.store(1 + on as u8, Ordering::Relaxed);
}

/// Whether a burst of `c` in `mode` was sent through [`TdClip`]: spread with
/// no data gain is only ever the clipper's (low-PAPR bursts always have a
/// gain); plain QAM looks the same either way and follows our own setting.
pub(super) fn td_clipped(c: Constellation, mode: DataMode) -> bool {
    match (mode.spread, mode.gain_code, c) {
        (true, 0, _) => true,
        (false, 0, Constellation::Qam16 | Constellation::Qam64) => td_clip_on(),
        _ => false,
    }
}

/// In-band distortion of [`TdClip`] at the link's picks, dB below the
/// signal, for the header's modes it alone sends: spread with no data gain
/// (low-PAPR bursts always have one) and plain QAM. Measured with
/// `td_clip_distortion` (tests/ladder.rs), the most clipped rate of each.
fn td_clip_distortion_db(c: Constellation, mode: DataMode, num: Numerology) -> Option<f32> {
    match (mode.spread, mode.gain_code, c) {
        (true, 0, _) => Some(match (c, num) {
            (Constellation::Qpsk, Numerology::Slice) => TD_DIST_DB[0],
            (Constellation::Qpsk, Numerology::Long | Numerology::LongWide) => TD_DIST_DB[1],
            (Constellation::Qpsk, _) => TD_DIST_DB[2],
            _ => TD_DIST_DB[3],
        }),
        (false, 0, Constellation::Qam16) => Some(TD_DIST_DB[4]),
        (false, 0, Constellation::Qam64) => Some(TD_DIST_DB[5]),
        _ => None,
    }
}

/// Slice QPSK, long QPSK, full-band QPSK, 8PSK, 16QAM, 64QAM.
const TD_DIST_DB: [f32; 6] = [8.3, 9.3, 9.5, 14.0, 17.6, 27.0];

/// In-band distortion of the link's clip for `c`, dB below the signal.
/// The receiver leaves it out of its SNR report; taking out a little too
/// much only falls back to the preamble's noise estimate.
pub(super) fn tx_distortion_db(c: Constellation, mode: DataMode, num: Numerology) -> f32 {
    if std::env::var("ION_RX_DIST").is_ok_and(|v| v == "none") {
        return f32::INFINITY;
    }
    if td_clipped(c, mode)
        && let Some(d) = td_clip_distortion_db(c, mode, num)
    {
        return d;
    }
    if mode.spread {
        // The slice's few data carriers clip worse.
        low_papr_distortion_db(c) - if num.is_slice() { 1.3 } else { 0.0 }
    } else if mode == DataMode::PLAIN {
        clip_distortion_db(clip_db_for(c))
    } else {
        // Plain cells with a data gain: the clip is not known here.
        clip_distortion_db(4.0)
    }
}

/// Builds one Normal burst: preamble, header (constellation, length,
/// `user`), data. `bits` is padded to whole OFDM symbols (at least one).
/// 48 kHz audio centred on `center_hz`, peak about `ion_dsp::TX_LEVEL`, with
/// ramps at both ends.
///
/// `center_hz` must be at least [`MIN_CENTER_HZ`](super::MIN_CENTER_HZ);
/// 1500 Hz puts the carriers at 275–2725 Hz. Panics above
/// [`MAX_DATA_SYMBOLS`](super::MAX_DATA_SYMBOLS) data symbols (size bursts
/// with [`capacity_bits`](super::capacity_bits)). Uses
/// [`TxOptions::default`].
pub fn modulate(c: Constellation, user: u16, bits: &[u8], center_hz: f32) -> Vec<f32> {
    modulate_with(c, user, bits, center_hz, &TxOptions::default())
}

/// [`modulate`] with explicit options.
pub fn modulate_with(c: Constellation, user: u16, bits: &[u8], center_hz: f32, opt: &TxOptions) -> Vec<f32> {
    let num = Numerology::Normal;
    check_center(num, center_hz);
    let nsym = data_symbols_with(num, opt.pilots, c, bits.len()).max(1);
    let bb = baseband(&NORMAL, c, user, 0, bits, nsym, opt);
    upconvert(num, &bb, center_hz, opt.td_clip)
}

/// Builds one burst of any numerology.
///
/// With data (`bits` not empty) it is [`modulate_with`] for `num`: the
/// header names `c`, the number of data symbols, the pilot pattern and
/// `user`; `extra` must be 0 (it is not sent). With empty `bits` it is a
/// **header-only** burst: the preamble and the header sent twice, carrying
/// `user` and `extra` (32 bits); `c` and `opt.pilots` are ignored and the
/// receiver reports `header_only` with `llrs` empty. The first copy
/// decodes on its own; when it fails, the receiver combines both (+3 dB).
/// Its length is [`burst_samples_for`](super::burst_samples_for)`(num, c, 0)`.
pub fn modulate_full(
    num: Numerology,
    c: Constellation,
    user: u16,
    extra: u16,
    bits: &[u8],
    center_hz: f32,
    opt: &TxOptions,
) -> Vec<f32> {
    check_center(num, center_hz);
    debug_assert!(
        extra == 0 || bits.is_empty(),
        "ion::phy::modulate_full: extra is only sent in header-only bursts"
    );
    let nsym = data_symbols_with(num, opt.pilots, c, bits.len());
    let bb = baseband(num.layout(), c, user, extra, bits, nsym, opt);
    upconvert(num, &bb, center_hz, opt.td_clip)
}

/// e^{-j2πm/n}, m = 0..n.
pub(super) fn twiddles(n: usize) -> Vec<Complex> {
    (0..n)
        .map(|m| {
            let a = -core::f64::consts::TAU * m as f64 / n as f64;
            Complex::new(a.cos() as f32, a.sin() as f32)
        })
        .collect()
}

/// Inverse DFT of the `nc` carriers in `x` into one useful symbol `u`
/// (`n` samples).
fn idft(l: &Layout, x: &[Complex], tw: &[Complex], u: &mut [Complex]) {
    let n = l.n;
    u.fill(Complex::ZERO);
    for (c, &xc) in x.iter().enumerate() {
        if xc == Complex::ZERO {
            continue;
        }
        let k = l.bin_mod(c, n);
        let mut idx = 0;
        for v in u.iter_mut() {
            *v += xc * tw[idx].conj();
            idx += k;
            if idx >= n {
                idx -= n;
            }
        }
    }
}

#[inline]
fn ramp(n: usize) -> f32 {
    0.5 - 0.5 * (core::f32::consts::PI * (n as f32 + 0.5) / RAMP as f32).cos()
}

pub(super) fn preamble_boost() -> f32 {
    10f32.powf(PREAMBLE_BOOST_DB / 20.0)
}

/// Time-domain period of preamble A.
fn a_period(l: &Layout) -> Vec<Complex> {
    let n = l.n;
    let tw = twiddles(n);
    let boost = preamble_boost();
    let mut a = vec![Complex::ZERO; l.a_period()];
    for (t, v) in a.iter_mut().enumerate() {
        for m in 0..l.a_carriers() {
            let k = l.bin_mod(l.a_carrier(m), n);
            *v += l.a_value(m, boost) * tw[(k * t) % n].conj();
        }
    }
    a
}

/// Useful part of symbol B (boost 1).
pub(super) fn b_time(l: &Layout) -> Vec<Complex> {
    let tw = twiddles(l.n);
    let x: Vec<Complex> = (0..l.nc).map(|c| l.b_value(c, 1.0)).collect();
    let mut u = vec![Complex::ZERO; l.n];
    idft(l, &x, &tw, &mut u);
    u
}

/// The receiver's template for B's matched filter: B, or only its carriers
/// that A leaves empty ([`Layout::mf_a_free`]).
pub(super) fn b_sync(l: &Layout) -> Vec<Complex> {
    let tw = twiddles(l.n);
    let x: Vec<Complex> = (0..l.nc)
        .map(|c| {
            if l.mf_a_free && !l.a_empty(c) {
                Complex::ZERO
            } else {
                l.b_value(c, 1.0)
            }
        })
        .collect();
    let mut u = vec![Complex::ZERO; l.n];
    idft(l, &x, &tw, &mut u);
    u
}

/// A (with its ramp) followed by B (with its CP), as transmitted.
pub(super) fn preamble_template(l: &Layout) -> Vec<Complex> {
    let a = a_period(l);
    let b = b_time(l);
    let boost = preamble_boost();
    let mut v = Vec::with_capacity(l.a_to_b() + l.n);
    for n in 0..l.a_len {
        let x = a[n % a.len()];
        v.push(if n < RAMP { x.scale(ramp(n)) } else { x });
    }
    v.extend(b[l.n - l.cp..].iter().map(|x| x.scale(boost)));
    v.extend(b.iter().map(|x| x.scale(boost)));
    v
}

/// Clip-and-filter (Armstrong 2002) on a 4x oversampled grid.
struct Clipper {
    map: Map,
    /// Clip level of header and data symbols (their pilot counts differ).
    level_hdr: f32,
    level_data: f32,
    /// Clip-and-filter passes.
    iters: usize,
    tw: Vec<Complex>,
    buf: Vec<Complex>,
}

const CLIP_ITER: usize = 3;
/// Clip-and-filter passes of bursts with a [`DataMode`] other than plain:
/// more passes leave lower peaks after the last filtering, at the cost of
/// more distortion.
const CLIP_ITER_LOW_PAPR: usize = 6;

impl Clipper {
    /// `data_gain`: amplitude of the data symbols against the header's.
    /// `hdr_db`: the header's clip level.
    fn new(map: Map, db: f32, hdr_db: f32, data_gain: f32, iters: usize) -> Self {
        let l = map.l;
        let os = 4 * l.n;
        let k = 10f32.powf(db / 20.0);
        Self {
            map,
            level_hdr: 10f32.powf(hdr_db / 20.0) * l.p_sym().sqrt(),
            level_data: k * data_gain * l.p_sym_of(map.pilots).sqrt(),
            iters,
            tw: twiddles(os),
            buf: vec![Complex::ZERO; os],
        }
    }

    /// Clips symbol `g` (carriers `x`, `nc` long) in place.
    fn apply(&mut self, x: &mut [Complex], g: usize) {
        let map = self.map;
        let l = map.l;
        let os = self.buf.len();
        let level = if g < map.hdr_end {
            self.level_hdr
        } else {
            self.level_data
        };
        let mut orig = [Complex::ZERO; NC_MAX];
        orig[..x.len()].copy_from_slice(x);
        for _ in 0..self.iters {
            self.buf.fill(Complex::ZERO);
            for (c, &xc) in x.iter().enumerate() {
                let k = l.bin_mod(c, os);
                let mut idx = 0;
                for v in self.buf.iter_mut() {
                    *v += xc * self.tw[idx].conj();
                    idx += k;
                    if idx >= os {
                        idx -= os;
                    }
                }
            }
            let mut clipped = false;
            for v in self.buf.iter_mut() {
                let m = v.norm();
                if m > level {
                    *v = v.scale(level / m);
                    clipped = true;
                }
            }
            if !clipped {
                break;
            }
            let inv = 1.0 / os as f32;
            for (c, xc) in x.iter_mut().enumerate() {
                let k = l.bin_mod(c, os);
                let mut idx = 0;
                let mut acc = Complex::ZERO;
                for v in self.buf.iter() {
                    acc += *v * self.tw[idx];
                    idx += k;
                    if idx >= os {
                        idx -= os;
                    }
                }
                *xc = acc.scale(inv);
            }
            // Pilots go out clean; data cells are rescaled by the
            // Bussgang gain so the receiver sees no amplitude bias.
            let (mut num, mut den) = (0f32, 0f32);
            for c in 0..x.len() {
                if map.is_pilot(c, g) {
                    x[c] = orig[c];
                } else {
                    num += (x[c] * orig[c].conj()).re;
                    den += orig[c].norm_sqr();
                }
            }
            let alpha = (num / den.max(1e-30)).max(0.1);
            for (c, v) in x.iter_mut().enumerate() {
                if !map.is_pilot(c, g) {
                    *v = v.scale(1.0 / alpha);
                }
            }
        }
    }
}

/// QPSK slots of `copies` header copies: copy `k` holds coded-sequence
/// indices `j = k·S ..` (`S` = [`Layout::hdr_slots`]), interleaved within
/// the copy.
fn header_slots(h: &Header, l: &Layout, copies: usize) -> Vec<u8> {
    let coded = conv_encode(&header_bits(h, l));
    let hs = l.hdr_slots();
    let a = interleave_step(hs);
    let mut slots = vec![0u8; copies * hs];
    for j in 0..copies * hs {
        let (k, i) = (j / hs, j % hs);
        slots[k * hs + (a * i) % hs] = coded[j % HDR_CODED] ^ scramble(SEED_HDR, j);
    }
    slots
}

/// Scrambled, interleaved data slots for `nsym` symbols of `dc` data
/// carriers.
fn data_slots(dc: usize, bps: usize, bits: &[u8], nsym: usize) -> Vec<u8> {
    let nbits = nsym * dc * bps;
    let mut dslots = vec![0u8; nbits];
    let ad = interleave_step(nbits) as u64;
    for i in 0..nbits {
        let b = bits.get(i).map_or(0, |b| b & 1);
        dslots[((ad * i as u64) % nbits as u64) as usize] = b ^ scramble(SEED_DATA, i);
    }
    dslots
}

/// Complex baseband of one burst with `nsym` data symbols (0: header-only,
/// carrying `extra`).
fn baseband(
    l: &'static Layout,
    c: Constellation,
    user: u16,
    extra: u16,
    bits: &[u8],
    nsym: usize,
    opt: &TxOptions,
) -> Vec<Complex> {
    let bps = c.bits_per_symbol();
    assert!(
        nsym <= MAX_DATA_SYMS,
        "ion::phy::modulate: {} bits need {nsym} symbols, at most {MAX_DATA_SYMS}",
        bits.len()
    );
    let (n, nc, cp, pre) = (l.n, l.nc, l.cp, l.pre_syms());
    let tw = twiddles(n);
    let boost = preamble_boost();
    let mut bb = Vec::with_capacity(l.overhead_bb() + l.sym() * nsym.max(l.hdr_syms));

    // A: periodic, raised-cosine ramp up.
    let a = a_period(l);
    for t in 0..l.a_len {
        let v = a[t % a.len()];
        bb.push(if t < RAMP { v.scale(ramp(t)) } else { v });
    }

    // WOLA: each symbol's first `w` samples cross-fade with the tapered
    // cyclic continuation of what came before (`tail`).
    let w = l.wola;
    let rc = |i: usize| 0.5 - 0.5 * (core::f32::consts::PI * (i as f32 + 0.5) / w.max(1) as f32).cos();
    let mut tail: Vec<Complex> = (0..w).map(|i| a[(l.a_len + i) % a.len()].scale(1.0 - rc(i))).collect();
    let push_symbol = |bb: &mut Vec<Complex>, u: &[Complex], tail: &mut Vec<Complex>| {
        let start = bb.len();
        bb.extend_from_slice(&u[n - cp..]);
        bb.extend_from_slice(u);
        for i in 0..w {
            bb[start + i] = bb[start + i].scale(rc(i)) + tail[i];
            tail[i] = u[i].scale(1.0 - rc(i));
        }
    };

    // B.
    let mut xa = [Complex::ZERO; NC_MAX];
    let x = &mut xa[..nc];
    for (cc, v) in x.iter_mut().enumerate() {
        *v = l.b_value(cc, boost);
    }
    let mut u = vec![Complex::ZERO; n];
    idft(l, x, &tw, &mut u);
    push_symbol(&mut bb, &u, &mut tail);

    // Header and data cells.
    let pilots = if nsym == 0 { Pilots::Robust } else { opt.pilots };
    let dmode = if nsym == 0 {
        DataMode::PLAIN
    } else {
        DataMode::new(opt.spread, opt.data_gain_db)
    };
    let hdr = if nsym == 0 {
        Header {
            constellation: 0,
            nsym: 0,
            user,
            extra,
            sparse: false,
            mode: DataMode::PLAIN,
        }
    } else {
        Header {
            constellation: c.code(),
            nsym: nsym as u16,
            user,
            extra: 0,
            sparse: pilots == Pilots::Sparse,
            mode: dmode,
        }
    };
    let copies = if nsym == 0 { HO_COPIES } else { 1 };
    let hdr_end = pre + (copies - 1) * l.hdr_syms;
    let map = Map::new(l, hdr_end, pilots);
    let (dc_hdr, dc_data) = (l.data_carriers(Pilots::Robust), l.data_carriers(pilots));
    let hslots = header_slots(&hdr, l, copies);
    let dslots = data_slots(dc_data, bps, bits, nsym);
    let pq = qpsk();
    let pd = c.points();
    let gain = dmode.gain();
    let mut clip = opt.clip_db.map(|db| {
        let iters = if dmode == DataMode::PLAIN {
            CLIP_ITER
        } else {
            CLIP_ITER_LOW_PAPR
        };
        Clipper::new(map, db, opt.hdr_clip_db.unwrap_or(db), gain, iters)
    });
    let mut spreader = Spreader::new(dc_data);
    let mut dcar = [0u8; NC_MAX];
    for g in 1..hdr_end + nsym {
        let (slots, p, base): (&[u8], &Points, usize) = if g < hdr_end {
            (&hslots, &pq, (g - 1) * dc_hdr)
        } else {
            (&dslots, &pd, (g - hdr_end) * dc_data)
        };
        for (cc, v) in x.iter_mut().enumerate() {
            if map.is_pilot(cc, g) {
                *v = l.pilot_value(cc, g);
            }
        }
        let dcg = map.data_carriers(g);
        map.data_carriers_of(g, &mut dcar[..dcg]);
        for (j, &cc) in dcar[..dcg].iter().enumerate() {
            let cell = base + j;
            let mut label = 0;
            for b in 0..p.bps {
                label = (label << 1) | slots[cell * p.bps + b] as usize;
            }
            x[cc as usize] = p.pts[label];
        }
        if g >= hdr_end {
            if dmode.spread {
                spreader.spread_cells(x, &dcar[..dcg]);
            }
            if gain != 1.0 {
                for v in x.iter_mut() {
                    *v = v.scale(gain);
                }
            }
        }
        if let Some(cl) = clip.as_mut() {
            cl.apply(x, g);
        }
        idft(l, x, &tw, &mut u);
        push_symbol(&mut bb, &u, &mut tail);
    }

    // Ramp down over a cyclic continuation of the last symbol.
    for (t, &v) in u.iter().take(RAMP).enumerate() {
        bb.push(v.scale(ramp(RAMP - 1 - t)));
    }
    bb
}

/// Interpolates 8 kHz complex baseband to 48 kHz, shifts it to
/// `center_hz` and takes the real part, peak-normalised to `TX_LEVEL`.
fn upconvert(num: Numerology, bb: &[Complex], center_hz: f32, td: Option<TdClip>) -> Vec<f32> {
    let taps = front_end_taps_for(num);
    let flen = taps.len();
    let len = bb.len() * DEC + flen - 1;
    let mut z = vec![Complex::ZERO; len];
    for (t, o) in z.iter_mut().enumerate() {
        let m_hi = (t / DEC).min(bb.len() - 1);
        let m_lo = if t >= flen - 1 {
            (t - (flen - 1)).div_ceil(DEC)
        } else {
            0
        };
        let mut acc = Complex::ZERO;
        for (m, &v) in bb.iter().enumerate().take(m_hi + 1).skip(m_lo) {
            acc += v.scale(taps[t - m * DEC]);
        }
        *o = acc;
    }
    if let Some(td) = td {
        td_clip(&mut z, &taps, td);
    }
    let w = core::f64::consts::TAU * center_hz as f64 / AUDIO_RATE as f64;
    let mut out = vec![0f32; len];
    for (t, (o, acc)) in out.iter_mut().zip(&z).enumerate() {
        let (s, co) = ((w * t as f64) % core::f64::consts::TAU).sin_cos();
        *o = acc.re * co as f32 - acc.im * s as f32;
    }
    let peak = out.iter().fold(0f32, |m, v| m.max(v.abs())).max(1e-30);
    let k = TX_LEVEL / peak;
    for o in &mut out {
        *o *= k;
    }
    out
}

/// [`TdClip`] on 48 kHz complex baseband `z` (its envelope is the
/// passband's analytic envelope; the low-pass is the passband's band-pass).
/// The interpolation taps have DC gain `DEC`, so they are normalised here.
fn td_clip(z: &mut [Complex], taps: &[f32], td: TdClip) {
    let power = z.iter().map(|v| v.norm_sqr()).sum::<f32>() / z.len() as f32;
    let thresh = power.sqrt() * 10f32.powf(td.headroom_db / 20.0);
    let filter = TdFilter::new(taps);
    for &k in td.overshoot {
        for v in z.iter_mut() {
            let m = v.norm();
            if m > thresh {
                *v = v.scale((thresh / m).powf(k));
            }
        }
        filter.apply(z);
    }
}

/// The front-end low-pass at unit DC gain, centred (taps are symmetric),
/// by overlap-add FFT convolution.
struct TdFilter {
    fft: Fft,
    spectrum: Vec<Complex>,
    taps: usize,
}

impl TdFilter {
    fn new(taps: &[f32]) -> Self {
        let n = (4 * taps.len()).next_power_of_two();
        let g = 1.0 / (taps.iter().sum::<f32>() * n as f32);
        let mut t = vec![Complex::ZERO; n];
        for (o, &v) in t.iter_mut().zip(taps) {
            *o = Complex::new(v * g, 0.0);
        }
        let fft = Fft::new(n);
        let mut spectrum = vec![Complex::ZERO; n];
        fft.forward(&t, &mut spectrum);
        Self {
            fft,
            spectrum,
            taps: taps.len(),
        }
    }

    fn apply(&self, z: &mut [Complex]) {
        let n = self.spectrum.len();
        let step = n - self.taps + 1;
        let mut y = vec![Complex::ZERO; z.len() + self.taps - 1];
        let (mut a, mut b) = (vec![Complex::ZERO; n], vec![Complex::ZERO; n]);
        for start in (0..z.len()).step_by(step) {
            let blk = &z[start..(start + step).min(z.len())];
            a.fill(Complex::ZERO);
            a[..blk.len()].copy_from_slice(blk);
            self.fft.forward(&a, &mut b);
            for (x, h) in b.iter_mut().zip(&self.spectrum) {
                *x = *x * *h;
            }
            self.fft.inverse(&b, &mut a);
            let end = (start + n).min(y.len());
            for (o, v) in y[start..end].iter_mut().zip(&a) {
                *o += *v;
            }
        }
        let h = self.taps / 2;
        z.copy_from_slice(&y[h..h + z.len()]);
    }
}

/// Data cell values `modulate` sends for `bits`, in cell order (debug
/// hook; Normal, robust pilots, unclipped).
#[doc(hidden)]
pub fn debug_tx_cells(c: Constellation, bits: &[u8]) -> Vec<Complex> {
    let l = &NORMAL;
    let bps = c.bits_per_symbol();
    let dc = l.data_carriers(Pilots::Robust);
    let nsym = data_symbols(c, bits.len());
    let dslots = data_slots(dc, bps, bits, nsym);
    let p = c.points();
    (0..nsym * dc)
        .map(|cell| {
            let mut label = 0;
            for b in 0..bps {
                label = (label << 1) | dslots[cell * bps + b] as usize;
            }
            p.pts[label]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_filter_matches_direct_convolution() {
        for num in [Numerology::NormalWide, Numerology::Slice, Numerology::LongWide] {
            let taps = front_end_taps_for(num);
            let z: Vec<Complex> = (0..20_011)
                .map(|i| Complex::new((i as f32 * 0.37).sin(), (i as f32 * 0.011).cos() * (i % 7) as f32))
                .collect();
            let g = 1.0 / taps.iter().sum::<f32>();
            let h = taps.len() / 2;
            let direct: Vec<Complex> = (0..z.len())
                .map(|t| {
                    let mut acc = Complex::ZERO;
                    for (j, v) in z
                        .iter()
                        .enumerate()
                        .take((t + h).min(z.len() - 1) + 1)
                        .skip(t.saturating_sub(h))
                    {
                        acc += v.scale(taps[j + h - t]);
                    }
                    acc.scale(g)
                })
                .collect();
            let mut fast = z.clone();
            TdFilter::new(&taps).apply(&mut fast);
            let err = direct
                .iter()
                .zip(&fast)
                .map(|(a, b)| (*a - *b).norm())
                .fold(0f32, f32::max);
            assert!(err < 1e-3, "{num:?}: {err}");
        }
    }
}
