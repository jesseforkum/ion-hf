//! Small helpers: hashing, CRC, interleaver, ln I0 and the detector FFT.

use core::f64::consts::TAU;

use ion_dsp::dsp::Complex;

/// SplitMix64 (Steele, Lea & Flood 2014): a fixed hash for the hop
/// sequence and the scrambler.
#[inline]
pub fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// CRC-16-CCITT (polynomial 0x1021) over bits, MSB-first, from `init`.
pub fn crc16(bits: &[u8], init: u16) -> u16 {
    let mut crc = init;
    for &b in bits {
        let top = ((crc >> 15) as u8 ^ (b & 1)) & 1;
        crc <<= 1;
        if top != 0 {
            crc ^= 0x1021;
        }
    }
    crc
}

/// Pushes `v` as `n` bits, MSB first.
pub fn push_bits(out: &mut Vec<u8>, v: u32, n: usize) {
    for i in (0..n).rev() {
        out.push(((v >> i) & 1) as u8);
    }
}

pub fn read_bits(bits: &[u8], n: usize) -> u32 {
    bits[..n].iter().fold(0u32, |a, &b| (a << 1) | b as u32)
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Multiplier of the interleaver `i → a·i mod n`: the integer nearest
/// n/φ that is coprime with n, so neighbours land far apart.
pub fn interleave_mult(n: usize) -> usize {
    if n <= 2 {
        return 1;
    }
    let target = (n as f64 * 0.618_033_988_75).round() as usize;
    for d in 0..n {
        for a in [target + d, target.wrapping_sub(d)] {
            if a > 0 && a < n && gcd(a, n) == 1 {
                return a;
            }
        }
    }
    1
}

/// ln I0(x) for x ≥ 0 (Abramowitz & Stegun 9.8.1 and 9.8.2).
pub fn ln_i0(x: f32) -> f32 {
    let x = x as f64;
    let r = if x < 3.75 {
        let t = (x / 3.75) * (x / 3.75);
        (1.0 + t
            * (3.515_622_9
                + t * (3.089_942_4 + t * (1.206_749_2 + t * (0.265_973_2 + t * (0.036_076_8 + t * 0.004_581_3))))))
            .ln()
    } else {
        let t = 3.75 / x;
        let p = 0.398_942_28
            + t * (0.013_285_92
                + t * (0.002_253_19
                    + t * (-0.001_575_65
                        + t * (0.009_162_81
                            + t * (-0.020_577_06 + t * (0.026_355_37 + t * (-0.016_476_33 + t * 0.003_923_77)))))));
        x - 0.5 * x.ln() + p.ln()
    };
    r as f32
}

/// ln(e^a + e^b).
#[inline]
pub fn lse(a: f32, b: f32) -> f32 {
    if a == f32::NEG_INFINITY {
        return b;
    }
    if b == f32::NEG_INFINITY {
        return a;
    }
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    hi + (lo - hi).exp().ln_1p()
}

/// A 320-point DFT (5 × 64, Good–Thomas-free two-stage Cooley–Tukey) of a
/// zero-padded 160-sample window, evaluated only at the outputs the
/// detector needs.
pub struct Fft320 {
    tw64: Vec<Complex>,
    tw320: Vec<Complex>,
    rev: [u8; 64],
}

impl Fft320 {
    pub fn new() -> Self {
        let tw64 = (0..32)
            .map(|k| {
                let a = -TAU * k as f64 / 64.0;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let tw320 = (0..320)
            .map(|k| {
                let a = -TAU * k as f64 / 320.0;
                Complex::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        let mut rev = [0u8; 64];
        for (i, r) in rev.iter_mut().enumerate() {
            *r = ((i as u8).reverse_bits() >> 2) & 63;
        }
        Self { tw64, tw320, rev }
    }

    fn fft64(&self, x: &mut [Complex; 64]) {
        for (i, &j) in self.rev.iter().enumerate() {
            let j = j as usize;
            if j > i {
                x.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= 64 {
            let half = len / 2;
            let step = 64 / len;
            for start in (0..64).step_by(len) {
                for k in 0..half {
                    let w = self.tw64[k * step];
                    let a = x[start + k];
                    let b = x[start + k + half] * w;
                    x[start + k] = a + b;
                    x[start + k + half] = a - b;
                }
            }
            len *= 2;
        }
    }

    /// `x`: 160 samples (the rest of the 320 is zero). Writes |X[k]|² for
    /// k = k0, k0+1, ..., k0 + out.len() − 1 (k may be negative).
    pub fn energies(&self, x: &[Complex], k0: i32, out: &mut [f32]) {
        debug_assert_eq!(x.len(), 160);
        let mut buf = [[Complex::ZERO; 64]; 5];
        for (r, b) in buf.iter_mut().enumerate() {
            for (n, v) in b.iter_mut().enumerate() {
                let i = 5 * n + r;
                *v = if i < 160 { x[i] } else { Complex::ZERO };
            }
            self.fft64(b);
        }
        for (o, e) in out.iter_mut().enumerate() {
            let k = (k0 + o as i32).rem_euclid(320) as usize;
            let mut acc = Complex::ZERO;
            for (r, b) in buf.iter().enumerate() {
                acc += b[k & 63] * self.tw320[(r * k) % 320];
            }
            *e = acc.norm_sqr();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft320_matches_dft() {
        let x: Vec<Complex> = (0..160)
            .map(|n| Complex::new(((n * 7) % 13) as f32 - 6.0, ((n * 3) % 5) as f32 - 2.0))
            .collect();
        let f = Fft320::new();
        let mut out = vec![0f32; 21];
        f.energies(&x, -10, &mut out);
        for (o, &e) in out.iter().enumerate() {
            let k = -10 + o as i32;
            let mut acc = (0.0f64, 0.0f64);
            for (n, v) in x.iter().enumerate() {
                let a = -TAU * k as f64 * n as f64 / 320.0;
                acc.0 += v.re as f64 * a.cos() - v.im as f64 * a.sin();
                acc.1 += v.re as f64 * a.sin() + v.im as f64 * a.cos();
            }
            let want = (acc.0 * acc.0 + acc.1 * acc.1) as f32;
            assert!((e - want).abs() <= 1e-3 * want.max(1.0), "{k}: {e} vs {want}");
        }
    }

    #[test]
    fn ln_i0_values() {
        for (x, want) in [(0.0f32, 0.0f32), (1.0, 0.235_914), (5.0, 3.304_7), (20.0, 17.589_7)] {
            assert!((ln_i0(x) - want).abs() < 2e-3, "{x}: {}", ln_i0(x));
        }
    }

    #[test]
    fn crc_check_value() {
        // CRC-16/CCITT-FALSE of "123456789" is 0x29B1.
        let mut bits = Vec::new();
        for c in b"123456789" {
            push_bits(&mut bits, *c as u32, 8);
        }
        assert_eq!(crc16(&bits, 0xFFFF), 0x29B1);
    }
}
