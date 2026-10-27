//! Preamble detector: a sliding Costas-energy statistic over a 25 Hz
//! spectrogram, whose peaks become sync candidates.

use ion_dsp::dsp::Complex;

use super::{Cand, MASK, MfskRx, Q};
use crate::phy_mfsk::frame::{self, PRE_LEN};
use crate::phy_mfsk::{SPACING_HZ, SYM};

/// Detector window (the 20 ms analysis window).
pub(super) const DWIN: usize = 160;
/// Detector spectrum: 25 Hz bins k = K0..K0 + NK.
pub(super) const NK: usize = 121;
const K0: i32 = -60;
/// Detector columns kept.
pub(super) const NCOL: usize = 128;
/// Detector threshold on the normalised Costas energy (mean 1 on noise).
const Z_DET: f32 = 1.9;
/// Cap on one preamble symbol's contribution to the detector statistic,
/// so a strong signal that matches one or two Costas tones by chance (MFSK
/// data at high SNR) cannot reach the threshold.
const Z_CLAMP: f32 = 10.0;
/// Detector threshold inside an expected slot ([`MfskRx::expect`]; see
/// also `Z_ACC_SLOT`). Only a few hundred milliseconds are searched
/// there instead of the whole stream, so far weaker preambles may become
/// candidates; the header (decoded, or tested against the expected one)
/// still has to pass.
const Z_DET_SLOT: f32 = 1.2;
/// Half the tone spacing: the detector's frequency step.
const SPACING_HALF: f32 = SPACING_HZ / 2.0;

impl MfskRx {
    pub(super) fn detect(&mut self) {
        let end = self.n_bb as i64;
        let mut win = [Complex::ZERO; DWIN];
        for (n, w) in win.iter_mut().enumerate() {
            *w = self.ring[(end as usize - DWIN + n) & MASK];
        }
        let slot = (self.n_col as usize) % NCOL;
        {
            let col = &mut self.cols[slot * NK..(slot + 1) * NK];
            self.fft.energies(&win, K0, col);
            let band = &col[(-49 - K0) as usize..=(49 - K0) as usize];
            self.col_mean[slot] = band.iter().sum::<f32>() / band.len() as f32;
        }
        let span = 4 * (PRE_LEN as u64 - 1);
        if self.n_col >= span {
            // Per frequency hypothesis: the clamped statistic decides,
            // the raw one locates the peak (the clamp flattens it at high
            // SNR).
            let mut best = (f32::MIN, f32::MIN, 0i32);
            let hmax = self.layout.hmax;
            for h in -hmax..=hmax {
                let (mut z, mut zr) = (0.0, 0.0);
                for i in 0..PRE_LEN {
                    let c = (self.n_col - 4 * (PRE_LEN as u64 - 1 - i as u64)) as usize % NCOL;
                    let k = (2 * frame::costas_bin(i) as i32 - 47 + h - K0) as usize;
                    let den = self.bavg[k].max(self.col_mean[c]).max(1e-30);
                    let v = self.cols[c * NK + k] / den;
                    z += v.min(Z_CLAMP);
                    zr += v;
                }
                z /= PRE_LEN as f32;
                zr /= PRE_LEN as f32;
                if (z, zr) > (best.0, best.1) {
                    best = (z, zr, h);
                }
            }
            self.zhist.push_back((end, best.0, best.1, best.2));
            if self.zhist.len() > 9 {
                self.zhist.pop_front();
            }
            if self.zhist.len() == 9 {
                let (ce, cz, czr, ch) = self.zhist[4];
                let before = self.zhist.iter().take(4).all(|v| v.2 < czr);
                let after = self.zhist.iter().skip(5).all(|v| v.2 <= czr);
                let t0c = ce + 28 - (PRE_LEN * SYM) as i64;
                let z_det = if self.in_slot(t0c) { Z_DET_SLOT } else { Z_DET };
                if cz >= z_det && before && after {
                    self.new_candidate(ce, cz, ch);
                }
            }
        }
        // Long-term per-bin level (≈ 0.9 s), after use.
        let alpha = 1.0 / (self.n_col.min(127) as f32 + 1.0);
        for k in 0..NK {
            let v = self.cols[slot * NK + k];
            self.bavg[k] += alpha * (v - self.bavg[k]);
        }
        self.n_col += 1;
    }

    fn new_candidate(&mut self, col_end: i64, z: f32, h: i32) {
        self.stats.detections += 1;
        // The window of the last preamble symbol fits anywhere in a 7 ms
        // plateau before the symbol end; take its middle.
        let t0c = col_end + 28 - (PRE_LEN * SYM) as i64;
        let f0 = h as f32 * SPACING_HALF;
        if self.spans.iter().any(|s| t0c >= s.0 && t0c <= s.1) {
            return;
        }
        if let Some(c) = self
            .cands
            .iter_mut()
            .find(|c| (c.t0c - t0c).abs() <= 2 * Q as i64 && (c.f0 - f0).abs() <= 30.0)
        {
            if !c.synced && z > c.z {
                c.t0c = t0c;
                c.f0 = f0;
                c.z = z;
            }
            return;
        }
        self.cands.push(Cand {
            t0c,
            f0,
            z,
            synced: false,
            zfine: 0.0,
            f: f0,
            d: [0; 3],
            e: Default::default(),
            e1: Default::default(),
            tried: [false; 4],
            slot: self.in_slot(t0c),
        });
    }
}
