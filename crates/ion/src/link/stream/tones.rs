//! The receiver's answers in a stream pause: DONE, SLOWER or FASTER, each
//! two steady tones in sequence at frequencies drawn for the session, and
//! the sender's detector (`docs/phy.md`, Stream tones).

use ion_dsp::TX_LEVEL;
use ion_dsp::dsp::AUDIO_RATE;

/// Tone grid: 400–2600 Hz in 50 Hz steps.
const GRID_LO_HZ: f32 = 400.0;
const GRID_STEP_HZ: f32 = 50.0;
pub const GRID_N: usize = 45;
/// Ramp at each end of an answer, audio samples (5 ms).
const RAMP: usize = 240;
/// The detector's time step, audio samples (10 ms).
const BLOCK: usize = 480;
/// Left out at each end of a tone, blocks (timing slack and the ramps).
const SLACK_BLOCKS: usize = 2;

/// Each tone of an answer, seconds.
pub const TONE_S: f64 = 0.2;
/// Detection mark on a pair's score (the weaker tone's energy over the
/// unused bins' mean) with the start searched: noise alone tops it in
/// well under 1 pause in 1000 (`noise_seldom_passes_the_mark`).
pub const MARK: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    Done,
    Slower,
    Faster,
}

impl Msg {
    pub const ALL: [Msg; 3] = [Msg::Done, Msg::Slower, Msg::Faster];

    pub fn code(self) -> u16 {
        match self {
            Msg::Done => 0,
            Msg::Slower => 1,
            Msg::Faster => 2,
        }
    }

    pub fn from_code(c: u16) -> Option<Msg> {
        Msg::ALL.get(c as usize).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Msg::Done => "DONE",
            Msg::Slower => "SLOWER",
            Msg::Faster => "FASTER",
        }
    }
}

/// One session's tone pairs and the grid bins left for the noise
/// reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// (message, first bin, second bin).
    pub pairs: Vec<(Msg, usize, usize)>,
    pub reference: Vec<usize>,
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn bin_hz(b: usize) -> f32 {
    GRID_LO_HZ + GRID_STEP_HZ * b as f32
}

impl Plan {
    /// The pairs of session `seed`: every bin used once, at least 100 Hz
    /// from every other bin in use, and the two tones of a pair at least
    /// 600 Hz apart (some frequency diversity on a fading path).
    pub fn new(seed: u64) -> Plan {
        let mut used: Vec<usize> = Vec::new();
        let mut x = mix(seed ^ 0x5EED);
        let mut pick = |used: &mut Vec<usize>, away_from: Option<usize>| loop {
            x = mix(x);
            let b = (x % GRID_N as u64) as usize;
            let clear = used.iter().all(|&u| u.abs_diff(b) >= 2);
            let apart = away_from.is_none_or(|a| a.abs_diff(b) >= 12);
            if clear && apart {
                used.push(b);
                return b;
            }
        };
        let pairs = [Msg::Slower, Msg::Faster, Msg::Done]
            .into_iter()
            .map(|m| {
                let b1 = pick(&mut used, None);
                let b2 = pick(&mut used, Some(b1));
                (m, b1, b2)
            })
            .collect();
        let reference = (0..GRID_N).filter(|b| !used.contains(b)).collect();
        Plan { pairs, reference }
    }

    pub fn pair(&self, m: Msg) -> (usize, usize) {
        self.pairs.iter().find(|p| p.0 == m).map(|p| (p.1, p.2)).expect("pair")
    }
}

/// Audio samples of one answer.
pub fn answer_samples() -> usize {
    2 * (TONE_S * AUDIO_RATE as f64) as usize
}

/// Audio of answer `m`, `off_hz` off the grid (pre-corrected against the
/// carrier offset the receiver tracked): a steady sine at `TX_LEVEL`, as
/// loud as every MFSK burst and never louder, phase-continuous from the
/// first tone to the second, 5 ms ramps at the ends.
pub fn synth(plan: &Plan, m: Msg, off_hz: f32) -> Vec<f32> {
    let (b1, b2) = plan.pair(m);
    let n = answer_samples() / 2;
    let mut out = Vec::with_capacity(2 * n);
    let mut ph = 0.0f64;
    for (k, b) in [b1, b2].into_iter().enumerate() {
        let w = core::f64::consts::TAU * (bin_hz(b) + off_hz) as f64 / AUDIO_RATE as f64;
        for i in 0..n {
            let j = k * n + i;
            let ramp = if j < RAMP {
                0.5 - 0.5 * (core::f32::consts::PI * (j as f32 + 0.5) / RAMP as f32).cos()
            } else if 2 * n - j <= RAMP {
                0.5 - 0.5 * (core::f32::consts::PI * ((2 * n - j) as f32 - 0.5) / RAMP as f32).cos()
            } else {
                1.0
            };
            out.push(TX_LEVEL * ramp * ph.sin() as f32);
            ph += w;
        }
    }
    out
}

/// What the detector found: the best pair over the mark, its score, and
/// where its first tone starts in the audio searched (samples).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Heard {
    pub msg: Option<Msg>,
    pub score: f32,
    pub at: usize,
}

/// The sender's detector. `audio` is what it heard in the pause; the
/// answer's first tone may start anywhere from `lo` to `hi` samples in.
/// For each start (10 ms steps) every pair is scored by its weaker tone:
/// the energy in that tone's DFT bin and its two neighbours over the mean
/// of the same over the unused grid bins (CFAR). The best score over
/// `mark` is the answer.
pub fn detect(plan: &Plan, audio: &[f32], lo: usize, hi: usize, mark: f32) -> Heard {
    let tone_blocks = (TONE_S * AUDIO_RATE as f64) as usize / BLOCK;
    let slot_blocks = tone_blocks - 2 * SLACK_BLOCKS;
    let df = AUDIO_RATE / (slot_blocks * BLOCK) as f32;
    let nb = audio.len() / BLOCK;
    let none = Heard {
        msg: None,
        score: 0.0,
        at: 0,
    };
    if nb < 2 * tone_blocks {
        return none;
    }
    let mut bins: Vec<usize> = plan.pairs.iter().flat_map(|p| [p.1, p.2]).collect();
    bins.extend(&plan.reference);
    // Per bin and neighbour, prefix sums over blocks of the block DFTs
    // (phases from the absolute sample index, so windows add coherently).
    let nf = 3 * GRID_N;
    let mut pre = vec![(0f64, 0f64); (nb + 1) * nf];
    for &g in &bins {
        for (j, k) in [-1.0f32, 0.0, 1.0].iter().enumerate() {
            let w = core::f64::consts::TAU * (bin_hz(g) + k * df) as f64 / AUDIO_RATE as f64;
            let (c, s) = (w.cos(), w.sin());
            let (mut pr, mut pi) = (1.0f64, 0.0f64);
            let col = 3 * g + j;
            for b in 0..nb {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for &v in &audio[b * BLOCK..(b + 1) * BLOCK] {
                    re += v as f64 * pr;
                    im -= v as f64 * pi;
                    let t = pr * c - pi * s;
                    pi = pr * s + pi * c;
                    pr = t;
                }
                let n = (pr * pr + pi * pi).sqrt();
                pr /= n;
                pi /= n;
                let p = pre[b * nf + col];
                pre[(b + 1) * nf + col] = (p.0 + re, p.1 + im);
            }
        }
    }
    let energy3 = |g: usize, a: usize| -> f32 {
        (0..3)
            .map(|j| {
                let (x0, x1) = (pre[a * nf + 3 * g + j], pre[(a + slot_blocks) * nf + 3 * g + j]);
                let (re, im) = (x1.0 - x0.0, x1.1 - x0.1);
                (re * re + im * im) as f32
            })
            .sum()
    };
    let noise = |a: usize| -> f32 {
        let e: f32 = plan.reference.iter().map(|&g| energy3(g, a)).sum();
        (e / plan.reference.len() as f32).max(1e-30)
    };
    let mut best = none;
    let t_lo = lo.div_ceil(BLOCK);
    let t_hi = (hi / BLOCK).min(nb - 2 * tone_blocks);
    for t in t_lo..=t_hi {
        let (a1, a2) = (t + SLACK_BLOCKS, t + tone_blocks + SLACK_BLOCKS);
        let (n1, n2) = (noise(a1), noise(a2));
        for &(m, b1, b2) in &plan.pairs {
            let score = (energy3(b1, a1) / n1).min(energy3(b2, a2) / n2);
            if score > best.score {
                best = Heard {
                    msg: (score >= mark).then_some(m),
                    score,
                    at: t * BLOCK,
                };
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use ion_dsp::dsp::{NoiseGen, noise_sigma};

    fn noisy(len: usize, sigma: f32, seed: u64) -> Vec<f32> {
        let mut g = NoiseGen::new(seed);
        (0..len).map(|_| sigma * g.gaussian()).collect()
    }

    #[test]
    fn pairs_are_apart_and_differ_by_session() {
        for seed in 0..200 {
            let p = Plan::new(seed);
            let bins: Vec<usize> = p.pairs.iter().flat_map(|q| [q.1, q.2]).collect();
            for (i, a) in bins.iter().enumerate() {
                for b in &bins[i + 1..] {
                    assert!(a.abs_diff(*b) >= 2);
                }
            }
            for q in &p.pairs {
                assert!(q.1.abs_diff(q.2) >= 12);
            }
            assert_eq!(p.reference.len(), GRID_N - 6);
        }
        assert_ne!(Plan::new(1), Plan::new(2));
    }

    #[test]
    fn tones_are_no_louder_than_mfsk() {
        let p = Plan::new(9);
        for m in Msg::ALL {
            for off in [0.0, 20.0, -20.0] {
                let a = synth(&p, m, off);
                assert_eq!(a.len(), answer_samples());
                let peak = a.iter().fold(0f32, |x, v| x.max(v.abs()));
                assert!(peak <= TX_LEVEL * 1.0001, "{peak}");
                let mfsk = crate::phy_mfsk::modulate(crate::phy_mfsk::MfskMode::R0, 1, 2, &[], 1500.0);
                let mfsk_peak = mfsk.iter().fold(0f32, |x, v| x.max(v.abs()));
                assert!(peak <= mfsk_peak * 1.0001, "{peak} > MFSK {mfsk_peak}");
                // Steady: the mean power of a sine at TX_LEVEL.
                let mid = &a[RAMP..a.len() - RAMP];
                let pw = mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32;
                assert!((pw - TX_LEVEL * TX_LEVEL / 2.0).abs() < 0.01 * pw);
            }
        }
    }

    #[test]
    fn an_answer_is_heard_wherever_it_starts_and_at_low_snr() {
        let p = Plan::new(77);
        let sigma = noise_sigma(TX_LEVEL * TX_LEVEL / 2.0, -10.0, 2400.0, AUDIO_RATE);
        for (k, m) in Msg::ALL.into_iter().enumerate() {
            for at in [4_800usize, 9_000, 20_123] {
                let mut a = noisy(60_000, sigma, 5 + k as u64 + at as u64);
                for (x, t) in a[at..].iter_mut().zip(synth(&p, m, 0.0)) {
                    *x += t;
                }
                let h = detect(&p, &a, 0, 30_000, MARK);
                assert_eq!(h.msg, Some(m), "{m:?} at {at}: {h:?}");
                assert!(h.at.abs_diff(at) <= 4 * BLOCK, "{h:?}");
            }
        }
    }

    #[test]
    fn noise_seldom_passes_the_mark() {
        let p = Plan::new(3);
        let n = 300;
        let mut over = 0;
        let mut top = 0f32;
        for s in 0..n {
            let a = noisy(36_000, 0.01, 1_000 + s);
            let h = detect(&p, &a, 0, 30_000, MARK);
            top = top.max(h.score);
            over += h.msg.is_some() as usize;
        }
        assert!(over == 0, "{over} of {n} pauses of noise over the mark (top {top:.2})");
    }
}
