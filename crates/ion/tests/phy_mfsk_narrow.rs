// Weak-signal options below the MFSK floor: R0 and R1 stretched in time
// (`MfskLayout::scaled`; R1/7 fits a 400 Hz slot, R0/2 half the band) and
// R0 with every code symbol sent twice. Round trips here; the tables of
// `docs/mfsk.md` (Below the floor) come from the `#[ignore]`d generators:
//
// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_table

use std::time::Instant;

use ion::fec::{self, Rate, nb};
use ion::phy::{Constellation, Numerology, PhyRx, Pilots, TxOptions, modulate_full};
use ion::phy_mfsk::{
    EXPECT_MIN_MARGIN, EXPECT_MIN_SCORE, Expect, ExpectedHeader, MfskBurst, MfskLayout, MfskMode, MfskRx,
    burst_samples_with, header_only_s_with, info_with, modulate, modulate_symbols, modulate_symbols_with,
    modulate_with,
};
use ion_dsp::TX_LEVEL;
use ion_dsp::channel::{Channel, ChannelSpec, PathSpec, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};
use ion_dsp::ionos::{self, Ionos};

/// Slot centres of six 400 Hz slots in an SSB channel (`docs/phy.md`, Six
/// slices).
const CENTERS: [f32; 6] = [500.0, 900.0, 1300.0, 1700.0, 2100.0, 2500.0];

fn random_bits(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 33) as u8 & 1
        })
        .collect()
}

fn power(x: &[f32]) -> f32 {
    x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32
}

fn pad(burst: &[f32], lead: f32, tail: f32) -> Vec<f32> {
    let mut v = vec![0.0; (lead * AUDIO_RATE) as usize];
    v.extend_from_slice(burst);
    v.extend(std::iter::repeat_n(0.0, (tail * AUDIO_RATE) as usize));
    v
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The channels of the tables.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Chan {
    Watterson(Profile),
    /// One Rayleigh path with this two-sigma Doppler spread, Hz.
    Rayleigh(f32),
    /// IONOS SIM multipath disturbed (2 Hz, 4 ms, two paths).
    IonosMpd,
}

impl Chan {
    fn name(self) -> String {
        match self {
            Chan::Watterson(p) => p.spec().name.to_string(),
            Chan::Rayleigh(s) => format!("Rayleigh {s} Hz"),
            Chan::IonosMpd => "IONOS MPD".into(),
        }
    }
}

/// Through `chan` with a frequency offset (Hz) and drift (Hz/s), then AWGN
/// at `snr_db` against `sig_power` in 2.4 kHz (`None`: no noise). IONOS
/// gets its own noise off and the same AWGN, so every channel has the same
/// SNR convention.
fn impair(audio: &[f32], chan: Chan, seed: u64, snr_db: Option<f32>, sig_power: f32, off: (f32, f32)) -> Vec<f32> {
    let mut out = vec![0.0; audio.len()];
    match chan {
        Chan::Watterson(p) => Channel::from_profile(p, seed)
            .with_offset(off.0, off.1)
            .process(audio, &mut out),
        Chan::Rayleigh(s) => {
            let spec = ChannelSpec {
                name: "Rayleigh",
                paths: vec![PathSpec {
                    delay_ms: 0.0,
                    spread_hz: s,
                }],
            };
            Channel::new(&spec, seed)
                .with_offset(off.0, off.1)
                .process(audio, &mut out)
        }
        Chan::IonosMpd => {
            assert!(off == (0.0, 0.0), "no offset through IONOS");
            Ionos::new(ionos::Mode::Mpd, 2, 3000, seed).process(audio, None, &mut out);
        }
    }
    if let Some(snr) = snr_db {
        let sigma = noise_sigma(sig_power, snr, 2400.0, AUDIO_RATE);
        add_awgn(&mut out, sigma, &mut NoiseGen::new(seed ^ 0xABCD));
    }
    out
}

fn rx_mfsk(layout: MfskLayout, center: f32, audio: &[f32], expect: Option<Expect>) -> (Vec<MfskBurst>, MfskRx) {
    let mut rx = MfskRx::with_layout(layout, center);
    rx.expect(expect);
    let mut out = Vec::new();
    for c in audio.chunks(4800) {
        rx.process(c, &mut out);
    }
    (out, rx)
}

/// GF(32) metrics of `copies` repetitions of one codeword, summed.
fn combine(metrics: &[f32], copies: usize) -> Vec<f32> {
    let n = nb::N * nb::Q;
    let mut sum = metrics[..n].to_vec();
    for c in 1..copies {
        for (s, m) in sum.iter_mut().zip(&metrics[c * n..(c + 1) * n]) {
            *s += m;
        }
    }
    sum
}

/// The candidates of the tables.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wave {
    /// `mode` in `layout`, one GF(32) short frame sent `rep` times in one
    /// burst (0: a header-only burst); with `expect`, the receiver knows when the burst starts
    /// (±60 ms) and what its header holds.
    Mfsk {
        layout: MfskLayout,
        mode: MfskMode,
        rep: usize,
        expect: bool,
        center: f32,
    },
    /// Sixth QPSK 1/4, one K = 1440 codeword, link clip.
    Sixth,
}

fn mfsk(k: usize, mode: MfskMode, rep: usize, expect: bool, center: f32) -> Wave {
    Wave::Mfsk {
        layout: MfskLayout::scaled(k),
        mode,
        rep,
        expect,
        center,
    }
}

fn wave_by_name(name: &str) -> Option<Wave> {
    use MfskMode::{R0, R1};
    Some(match name {
        "r0" => mfsk(1, R0, 1, false, 1500.0),
        "r1" => mfsk(1, R1, 1, false, 1500.0),
        "sixth" => Wave::Sixth,
        "r1k6" => mfsk(6, R1, 1, false, 1300.0),
        "r1k7" => mfsk(7, R1, 1, false, 1300.0),
        "r1k8" => mfsk(8, R1, 1, false, 1300.0),
        "r0k8" => mfsk(8, R0, 1, false, 1300.0),
        "r0k2" => mfsk(2, R0, 1, false, 1500.0),
        "r1k2" => mfsk(2, R1, 1, false, 1500.0),
        "r0e" => mfsk(1, R0, 1, true, 1500.0),
        "r0x2" => mfsk(1, R0, 2, false, 1500.0),
        "r0x2e" => mfsk(1, R0, 2, true, 1500.0),
        "r0x4e" => mfsk(1, R0, 4, true, 1500.0),
        "r1k7e" => mfsk(7, R1, 1, true, 1300.0),
        "r0h" => mfsk(1, R0, 0, false, 1500.0),
        "r1h" => mfsk(1, R1, 0, false, 1500.0),
        "r1k7h" => mfsk(7, R1, 0, false, 1300.0),
        "r0k2h" => mfsk(2, R0, 0, false, 1500.0),
        "r1k2h" => mfsk(2, R1, 0, false, 1500.0),
        "r1k7s2" => Wave::Mfsk {
            layout: MfskLayout {
                sweep: 2 * MfskLayout::scaled(7).sweep,
                ..MfskLayout::scaled(7)
            },
            mode: R1,
            rep: 1,
            expect: false,
            center: 1300.0,
        },
        _ => return None,
    })
}

/// One burst of `wave` through `chan` at `snr` (2.4 kHz, against the
/// burst's own mean power): (header right, frame decoded; both the
/// header for a header-only burst).
fn run(wave: Wave, chan: Chan, snr: f32, seed: u64, off: (f32, f32), ppm: f64) -> (bool, bool) {
    let user = (seed as u16).wrapping_mul(7919) | 1;
    let extra = (seed as u16) ^ 0x5A5A;
    match wave {
        Wave::Mfsk {
            layout,
            mode,
            rep,
            expect,
            center,
        } => {
            let info = random_bits(nb::K_BITS, seed);
            let cw = nb::encode_bits(&info);
            let syms: Vec<u8> = (0..rep).flat_map(|_| cw.iter().copied()).collect();
            let burst = modulate_symbols_with(layout, mode, user, extra, &syms, center);
            let p = power(&burst);
            let k = layout.k as f32;
            let lead = 0.5 * k;
            let mut audio = pad(&burst, lead, 0.6 * k);
            if ppm != 0.0 {
                audio = resample(&audio, ppm);
            }
            let a = impair(&audio, chan, seed, Some(snr), p, off);
            let exp = expect.then(|| {
                let delay = match chan {
                    Chan::IonosMpd => Ionos::new(ionos::Mode::Mpd, 2, 3000, 1).latency(),
                    _ => Channel::from_profile(Profile::Awgn, 1).latency(),
                };
                Expect {
                    start_sample: (lead * AUDIO_RATE) as u64 + delay as u64,
                    tol_samples: (0.06 * AUDIO_RATE) as u64,
                    header: Some(ExpectedHeader {
                        symbols: true,
                        ..ExpectedHeader::exact(mode, user, extra, 5 * syms.len())
                    }),
                    alt: None,
                    min_score: EXPECT_MIN_SCORE,
                    min_margin: EXPECT_MIN_MARGIN,
                }
            });
            let (got, _) = rx_mfsk(layout, center, &a, exp);
            let Some(b) = got
                .iter()
                .find(|b| b.mode == mode && b.user == user && b.extra == extra)
            else {
                return (false, false);
            };
            if rep == 0 {
                return (b.llrs.is_empty(), b.llrs.is_empty());
            }
            if b.metrics.len() != syms.len() * nb::Q {
                return (true, false);
            }
            let ok = nb::decode_bits_prior(&combine(&b.metrics, rep), &[]).is_some_and(|d| d == info);
            (true, ok)
        }
        Wave::Sixth => {
            assert!(off == (0.0, 0.0) && ppm == 0.0);
            let info = random_bits(fec::K, seed);
            let coded = fec::encode(Rate::R1_4, &info);
            let c = Constellation::Qpsk;
            let o = TxOptions::for_burst(c, Pilots::Robust);
            let burst = modulate_full(Numerology::Sixth, c, user, 0, &coded, 1300.0, &o);
            let p = power(&burst);
            let a = impair(&pad(&burst, 0.25, 0.3), chan, seed, Some(snr), p, off);
            let mut rx = PhyRx::with_numerology(Numerology::Sixth, 1300.0);
            let mut got = Vec::new();
            for ch in a.chunks(4800) {
                rx.process(ch, &mut got);
            }
            let Some(b) = got.iter().find(|b| b.user == user && !b.header_only) else {
                return (false, false);
            };
            let ok = fec::decode(Rate::R1_4, &b.llrs[..Rate::R1_4.n()]).is_some_and(|d| d == info);
            (true, ok)
        }
    }
}

/// A sound card running `ppm` slow against the sender's: the audio is
/// stretched by 1 + ppm·10⁻⁶ (16-tap windowed-sinc interpolation).
fn resample(x: &[f32], ppm: f64) -> Vec<f32> {
    const HALF: i64 = 8;
    let r = 1.0 + ppm * 1e-6;
    let n_out = (x.len() as f64 * r) as usize;
    (0..n_out)
        .map(|n| {
            let t = n as f64 / r;
            let i0 = t.floor() as i64;
            let mut acc = 0f64;
            for i in i0 - HALF + 1..=i0 + HALF {
                if i < 0 || i >= x.len() as i64 {
                    continue;
                }
                let d = t - i as f64;
                let s = if d.abs() < 1e-12 {
                    1.0
                } else {
                    (core::f64::consts::PI * d).sin() / (core::f64::consts::PI * d)
                };
                let w = 0.5 + 0.5 * (core::f64::consts::PI * d / HALF as f64).cos();
                acc += x[i as usize] as f64 * s * w;
            }
            acc as f32
        })
        .collect()
}

/// Runs `jobs` on `ION_THREADS` threads (default 2), results in job
/// order.
fn par_map<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..jobs.len()).map(|_| None).collect());
    let threads: usize = env_or("ION_THREADS", 2);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= jobs.len() {
                        break;
                    }
                    let r = f(&jobs[i]);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect()
}

/// Header and frame success fractions of `runs` bursts.
fn point(wave: Wave, chan: Chan, snr: f32, runs: u64, off: (f32, f32), ppm: f64) -> (f32, f32) {
    let jobs: Vec<u64> = (0..runs).collect();
    let res = par_map(&jobs, |&k| {
        run(wave, chan, snr, 7 + 1000 * k + (snr + 50.0) as u64, off, ppm)
    });
    let h = res.iter().filter(|r| r.0).count() as f32 / runs as f32;
    let d = res.iter().filter(|r| r.1).count() as f32 / runs as f32;
    (h, d)
}

/// Lowest SNR on a 1 dB grid from `start` with ≥ 90% right headers, and
/// with ≥ 90% decoded frames (stops after two points with both), and the
/// curve as "snr: header/frame".
fn threshold(wave: Wave, chan: Chan, start: i32, stop: i32, runs: u64) -> (Option<i32>, Option<i32>, String) {
    let (mut h90, mut d90) = (None, None);
    let mut curve = Vec::new();
    let mut snr = start;
    let mut both = 0;
    while snr <= stop && both < 2 {
        let (h, d) = point(wave, chan, snr as f32, runs, (0.0, 0.0), 0.0);
        curve.push(format!("{snr:+}: {h:.2}/{d:.2}"));
        if h >= 0.9 && h90.is_none() {
            h90 = Some(snr);
        }
        if d >= 0.9 && d90.is_none() {
            d90 = Some(snr);
        }
        both = if h >= 0.9 && d >= 0.9 { both + 1 } else { 0 };
        snr += if h < 0.05 && d < 0.05 { 3 } else { 1 };
    }
    (h90, d90, curve.join(", "))
}

fn chans_from_env(default: &str) -> Vec<Chan> {
    std::env::var("NARROW_CHANS")
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter_map(|s| match s.trim() {
            "awgn" => Some(Chan::Watterson(Profile::Awgn)),
            "moderate" => Some(Chan::Watterson(Profile::CcirModerate)),
            "poor" => Some(Chan::Watterson(Profile::CcirPoor)),
            "flutter" => Some(Chan::Watterson(Profile::CcirFlutter)),
            "nvis" => Some(Chan::Watterson(Profile::MidDisturbedNvis)),
            "low" => Some(Chan::Watterson(Profile::LowDisturbed)),
            "high" => Some(Chan::Watterson(Profile::HighDisturbed)),
            "mpd" => Some(Chan::IonosMpd),
            _ => None,
        })
        .collect()
}

fn waves_from_env(default: &str) -> Vec<(String, Wave)> {
    std::env::var("NARROW_WAVES")
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter_map(|s| wave_by_name(s.trim()).map(|w| (s.trim().to_string(), w)))
        .collect()
}

fn show(v: Option<i32>) -> String {
    v.map_or("—".into(), |s| format!("{s:+}"))
}

#[test]
fn scaled_layout_numbers() {
    for (k, mode) in [(2, MfskMode::R0), (7, MfskMode::R1)] {
        let l = MfskLayout::scaled(k);
        let full = info_with(MfskLayout::FULL, mode);
        let i = info_with(l, mode);
        assert!((i.raw_bps * k as f32 - full.raw_bps).abs() < 1e-3);
        assert!((i.bandwidth_hz * k as f32 - full.bandwidth_hz).abs() < 1e-3);
        for nbits in [0usize, 5, 1025] {
            let b = modulate_with(l, mode, 1, 2, &random_bits(nbits, 1), 1300.0);
            assert_eq!(b.len(), burst_samples_with(l, mode, nbits));
            let peak = b.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!(peak <= TX_LEVEL * 1.001 && peak > 0.9 * TX_LEVEL);
            // Constant envelope: the full band's mean power, never more.
            let f = modulate(mode, 1, 2, &random_bits(nbits, 1), 1300.0);
            assert!((power(&b) / power(&f) - 1.0).abs() < 0.01, "{k} {nbits}");
        }
        let h = header_only_s_with(l, mode);
        assert!((h - burst_samples_with(l, mode, 0) as f32 / AUDIO_RATE).abs() < 1e-6);
    }
}

/// R1/7 at the outer slot centres (clean; the lowest also at a low SNR)
/// and R0/2 (clean and at a low SNR), symbol-mapped and header-only;
/// start and offset in real units.
#[test]
fn scaled_round_trip() {
    for (k, mode, center, snr, off) in [
        (7, MfskMode::R1, CENTERS[0], -14.0f32, 12.0f32),
        (7, MfskMode::R1, CENTERS[5], f32::NAN, 0.0),
        (2, MfskMode::R0, 1500.0, -12.0, 30.0),
    ] {
        let l = MfskLayout::scaled(k);
        let info = random_bits(nb::K_BITS, k as u64);
        let syms = nb::encode_bits(&info);
        let burst = modulate_symbols_with(l, mode, 0x1234, 0x0042, &syms, center);
        for noisy in [false, true].into_iter().filter(|&n| !n || !snr.is_nan()) {
            let a = pad(&burst, 0.4 * k as f32, 0.5 * k as f32);
            let a = if noisy {
                impair(
                    &a,
                    Chan::Watterson(Profile::Awgn),
                    3,
                    Some(snr),
                    power(&burst),
                    (off, 0.0),
                )
            } else {
                a
            };
            let (got, _) = rx_mfsk(l, center, &a, None);
            assert_eq!(got.len(), 1, "k {k} {center} noisy {noisy}");
            let b = &got[0];
            assert!(b.symbols && (b.user, b.extra) == (0x1234, 0x0042));
            assert_eq!(nb::decode_bits(&b.metrics[..nb::N * nb::Q]), Some(info.clone()));
            let want_off = if noisy { off } else { 0.0 };
            assert!((b.freq_offset_hz - want_off).abs() < 0.5, "{}", b.freq_offset_hz);
            if !noisy {
                let err = b.start_sample as i64 - (0.4 * k as f32 * AUDIO_RATE) as i64;
                assert!(err.abs() <= 12 * k as i64, "start error {err}");
                // The model caps a tone's window SNR at 30 dB.
                assert!(b.snr_db > 5.0, "{}", b.snr_db);
            } else {
                assert!((b.snr_db - snr).abs() < 2.0, "snr {} at {snr}", b.snr_db);
            }
        }
        let h = modulate_with(l, mode, 0xBEEF, 0x0001, &[], center);
        let (got, _) = rx_mfsk(l, center, &pad(&h, 0.4 * k as f32, 0.5 * k as f32), None);
        assert_eq!(got.len(), 1);
        assert!(got[0].llrs.is_empty() && (got[0].user, got[0].extra) == (0xBEEF, 0x0001));
    }
}

/// R0 with the codeword sent twice: the summed metrics decode where one
/// copy's do not.
#[test]
fn repetition_round_trip() {
    let info = random_bits(nb::K_BITS, 9);
    let cw = nb::encode_bits(&info);
    let two: Vec<u8> = cw.iter().chain(&cw).copied().collect();
    let burst = modulate_symbols(MfskMode::R0, 5, 6, &two, 1500.0);
    let p = power(&burst);
    let mut both = 0;
    for seed in 0..4 {
        let a = impair(
            &pad(&burst, 0.5, 0.6),
            Chan::Watterson(Profile::Awgn),
            40 + seed,
            Some(-13.5),
            p,
            (0.0, 0.0),
        );
        let (got, _) = rx_mfsk(MfskLayout::FULL, 1500.0, &a, None);
        let Some(b) = got.first() else { continue };
        assert_eq!(b.metrics.len(), 2 * nb::N * nb::Q);
        let one = nb::decode_bits(&b.metrics[..nb::N * nb::Q]) == Some(info.clone());
        let sum = nb::decode_bits(&combine(&b.metrics, 2)) == Some(info.clone());
        assert!(sum || !one, "a copy decodes but the sum does not");
        both += sum as usize;
    }
    assert!(both >= 3, "{both}/4");
}

/// Hashes of R0 and R1 bursts and of what the receiver makes of them
/// through CCIR poor with an offset: the same before and after a change
/// that must leave the full-band modes alone.
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --ignored --nocapture diag_full_band_hash
#[test]
#[ignore]
fn diag_full_band_hash() {
    let fnv = |h: &mut u64, x: u32| {
        for b in x.to_le_bytes() {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x0100_0000_01B3);
        }
    };
    for mode in MfskMode::ALL {
        let mut ht = 0xCBF2_9CE4_8422_2325u64;
        let mut hr = ht;
        let syms: Vec<u8> = random_bits(5 * 205, 3)
            .chunks(5)
            .map(|c| c.iter().fold(0, |a, &b| a << 1 | b))
            .collect();
        let bursts = [
            modulate_symbols(mode, 0x1234, 0x5678, &syms, 1500.0),
            modulate(mode, 0x0F0F, 0x00FF, &random_bits(1000, 4), 1500.0),
            modulate(mode, 0xBEEF, 0x0001, &[], 1500.0),
        ];
        for (k, b) in bursts.iter().enumerate() {
            b.iter().for_each(|x| fnv(&mut ht, x.to_bits()));
            let a = impair(
                &pad(b, 0.4, 0.5),
                Chan::Watterson(Profile::CcirPoor),
                20 + k as u64,
                Some(-4.0),
                power(b),
                (7.0, 0.0),
            );
            let mut rx = MfskRx::new(1500.0);
            let mut got = Vec::new();
            for c in a.chunks(4800) {
                rx.process(c, &mut got);
            }
            for b in &got {
                for x in b
                    .llrs
                    .iter()
                    .chain(&b.metrics)
                    .chain(&[b.snr_db, b.freq_offset_hz, b.clock_ppm])
                {
                    fnv(&mut hr, x.to_bits());
                }
                fnv(&mut hr, b.start_sample as u32);
                fnv(&mut hr, b.user as u32 ^ (b.extra as u32) << 16);
            }
        }
        println!("{mode:?}: tx {ht:016x} rx {hr:016x}");
    }
}

/// Burst numbers of every candidate: raw and net rate, one 32-byte frame,
/// header-only burst, occupied bandwidth, mean power against a steady
/// carrier at `TX_LEVEL`.
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --ignored --nocapture narrow_numbers
#[test]
#[ignore]
fn narrow_numbers() {
    let carrier = TX_LEVEL * TX_LEVEL / 2.0;
    for name in [
        "r0", "r1", "r1k6", "r1k7", "r1k8", "r0k8", "r0k2", "r1k2", "r0x2", "r0x4e",
    ] {
        let Some(Wave::Mfsk { layout, mode, rep, .. }) = wave_by_name(name) else {
            continue;
        };
        let i = info_with(layout, mode);
        let n = 5 * nb::N * rep;
        let one = burst_samples_with(layout, mode, n) as f32 / AUDIO_RATE;
        let b = modulate_symbols_with(layout, mode, 1, 1, &vec![3u8; nb::N * rep], 1300.0);
        println!(
            "{name}: raw {:.1} bit/s, bandwidth {:.0} Hz, overhead {:.2} s, header-only {:.2} s, \
             one frame {:.2} s ({:.1} bit/s net), mean power {:+.2} dB re carrier",
            i.raw_bps,
            i.bandwidth_hz,
            i.overhead_s,
            header_only_s_with(layout, mode),
            one,
            256.0 / one,
            10.0 * (power(&b) / carrier).log10()
        );
    }
    let c = Constellation::Qpsk;
    let o = TxOptions::for_burst(c, Pilots::Robust);
    let coded = fec::encode(Rate::R1_4, &random_bits(fec::K, 1));
    let b = modulate_full(Numerology::Sixth, c, 1, 0, &coded, 1300.0, &o);
    let h = modulate_full(Numerology::Sixth, c, 1, 0, &[], 1300.0, &TxOptions::default());
    let sixth_i = ion::phy::info_for(Numerology::Sixth);
    println!(
        "sixth QPSK 1/4: one K = 1440 codeword {:.2} s ({:.1} bit/s net), header-only {:.2} s, \
         overhead {:.2} s, mean power {:+.2} dB re carrier",
        b.len() as f32 / AUDIO_RATE,
        1440.0 / (b.len() as f32 / AUDIO_RATE),
        h.len() as f32 / AUDIO_RATE,
        sixth_i.overhead_s,
        10.0 * (power(&b) / carrier).log10()
    );
}

/// Threshold table: for each wave (`NARROW_WAVES`) and channel
/// (`NARROW_CHANS`), the lowest SNR (1 dB grid, 2.4 kHz, against the
/// burst's own mean power) with ≥ 90% right headers and with ≥ 90%
/// decoded frames, `NARROW_RUNS` bursts per point (default 40).
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_table
#[test]
#[ignore]
fn narrow_table() {
    let t0 = Instant::now();
    let runs: u64 = env_or("NARROW_RUNS", 40);
    let stop: i32 = env_or("NARROW_STOP", 14);
    let waves = waves_from_env("r0,r1,sixth,r1k6,r1k7,r1k8,r0k2,r0x2,r0x2e");
    let chans = chans_from_env("awgn,moderate,poor,flutter,nvis,mpd");
    let mut rows = Vec::new();
    for (name, wave) in &waves {
        let start = match name.as_str() {
            "r0" | "r0e" | "r0h" => -17,
            "r1" | "r1h" => -15,
            "sixth" => -13,
            "r0k2" | "r1k2" | "r0x2" | "r0x2e" | "r0k2h" | "r1k2h" => -19,
            "r0x4e" => -21,
            _ => -24,
        };
        for &chan in &chans {
            let tl = Instant::now();
            let (h, d, curve) = threshold(*wave, chan, start, stop, runs);
            eprintln!(
                "{name} {}: header {} frame {} ({:.0} s): {curve}",
                chan.name(),
                show(h),
                show(d),
                tl.elapsed().as_secs_f32()
            );
            rows.push((name.clone(), chan, h, d));
        }
    }
    println!(
        "| Wave | {} |",
        chans
            .iter()
            .map(|c| format!("{} header / frame", c.name()))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for (name, _) in &waves {
        let cells: Vec<String> = chans
            .iter()
            .map(|c| {
                let r = rows.iter().find(|r| &r.0 == name && r.1 == *c).unwrap();
                format!("{} / {}", show(r.2), show(r.3))
            })
            .collect();
        println!("| {name} | {} |", cells.join(" | "));
    }
    println!("narrow_table: {:.0} s", t0.elapsed().as_secs_f32());
}

/// Doppler tolerance: frame threshold on one Rayleigh path with spread
/// 0.5–8 Hz (`NARROW_SPREADS`), `NARROW_RUNS` bursts per point.
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_doppler
#[test]
#[ignore]
fn narrow_doppler() {
    let runs: u64 = env_or("NARROW_RUNS", 40);
    let spreads: Vec<f32> = std::env::var("NARROW_SPREADS")
        .unwrap_or_else(|_| "0.5,1,2,4,8".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    for (name, wave) in waves_from_env("r1,r1k7,r0k2") {
        let start = match name.as_str() {
            "r1" => -14,
            "r0k2" => -17,
            _ => -22,
        };
        for &s in &spreads {
            let (h, d, curve) = threshold(wave, Chan::Rayleigh(s), start, 14, runs);
            println!("{name} Rayleigh {s} Hz: header {} frame {}: {curve}", show(h), show(d));
        }
    }
}

/// Frequency offset, frequency drift and sample-clock offset at a fixed
/// AWGN SNR (`NARROW_SNR`, default −16 dB for the narrow waves):
/// header/frame success, `NARROW_RUNS` bursts per point (default 20).
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_offsets
#[test]
#[ignore]
fn narrow_offsets() {
    let runs: u64 = env_or("NARROW_RUNS", 20);
    for (name, wave) in waves_from_env("r1k7") {
        let snr: f32 = env_or("NARROW_SNR", -16.0);
        let awgn = Chan::Watterson(Profile::Awgn);
        let mut line = Vec::new();
        for off in [
            -50.0f32, -40.0, -35.0, -30.0, -20.0, -10.0, 0.0, 10.0, 20.0, 30.0, 35.0, 40.0, 50.0,
        ] {
            let (h, d) = point(wave, awgn, snr, runs, (off, 0.0), 0.0);
            line.push(format!("{off:+}: {h:.2}/{d:.2}"));
        }
        println!("{name} {snr} dB offset Hz: {}", line.join(", "));
        let mut line = Vec::new();
        for drift in [-0.5f32, -0.2, -0.1, 0.1, 0.2, 0.5] {
            let (h, d) = point(wave, awgn, snr, runs, (0.0, drift), 0.0);
            line.push(format!("{drift:+}: {h:.2}/{d:.2}"));
        }
        println!("{name} {snr} dB drift Hz/s from 0: {}", line.join(", "));
        let mut line = Vec::new();
        for ppm in [-200.0f64, -100.0, 100.0, 200.0] {
            let (h, d) = point(wave, awgn, snr, runs, (0.0, 0.0), ppm);
            line.push(format!("{ppm:+}: {h:.2}/{d:.2}"));
        }
        println!("{name} {snr} dB clock ppm: {}", line.join(", "));
    }
}

/// Six bursts of `wave`'s layout and mode, one per slot centre, each with
/// its own frame, at amplitude `gains[i]` and offset `offs[i]` Hz,
/// staggered so that all overlap for most of their length; AWGN at `snr`
/// against one unit-gain burst. A receiver at every unit-gain centre:
/// (frames decoded, own bursts missed, foreign bursts delivered,
/// receivers).
fn six_run(wave: Wave, seed: u64, snr: f32, gains: [f32; 6], offs: [f32; 6]) -> (usize, usize, usize, usize) {
    let Wave::Mfsk { layout, mode, .. } = wave else {
        panic!("six_run: MFSK only")
    };
    let k = layout.k as f32;
    let mut mix: Vec<f32> = Vec::new();
    let mut want = Vec::new();
    let mut p1 = 0.0;
    for (i, &center) in CENTERS.iter().enumerate() {
        let info = random_bits(nb::K_BITS, 100 * seed + i as u64);
        let user = 0x4000 + (100 * seed + i as u64) as u16;
        let mut b = modulate_symbols_with(layout, mode, user, 7, &nb::encode_bits(&info), center);
        if i == 0 {
            p1 = power(&b);
        }
        if offs[i] != 0.0 {
            b = impair(
                &b,
                Chan::Watterson(Profile::Awgn),
                seed + i as u64,
                None,
                1.0,
                (offs[i], 0.0),
            );
        }
        let at = ((0.5 + 0.4 * ((i * 5) % 6) as f32) * k * AUDIO_RATE) as usize;
        if mix.len() < at + b.len() {
            mix.resize(at + b.len(), 0.0);
        }
        for (o, v) in mix[at..].iter_mut().zip(&b) {
            *o += gains[i] * v;
        }
        want.push((center, info, user, gains[i] == 1.0));
    }
    mix.extend(std::iter::repeat_n(0.0, (0.6 * k * AUDIO_RATE) as usize));
    let sigma = noise_sigma(p1, snr, 2400.0, AUDIO_RATE);
    add_awgn(&mut mix, sigma, &mut NoiseGen::new(77 + seed));
    let (mut dec, mut missed, mut foreign, mut rxs) = (0, 0, 0, 0);
    for (center, info, user, listen) in &want {
        if !listen {
            continue;
        }
        rxs += 1;
        let (got, _) = rx_mfsk(layout, *center, &mix, None);
        foreign += got.iter().filter(|b| b.user != *user).count();
        match got.iter().find(|b| b.user == *user) {
            Some(b) => dec += (nb::decode_bits(&b.metrics[..nb::N * nb::Q]).as_ref() == Some(info)) as usize,
            None => missed += 1,
        }
    }
    (dec, missed, foreign, rxs)
}

/// Six slots side by side (`NARROW_WAVES`, default R1/7 with its 7 ms
/// sweep and with 14 ms): all equal, and the 900 Hz slot `NARROW_STRONG_DB`
/// stronger (default 20; on its centre and `NARROW_TOWARDS` Hz towards
/// 500 Hz, default 10), at `NARROW_SNRS` (default −15, −10) against one
/// slot; `NARROW_RUNS` runs per case (default 10).
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_neighbours
#[test]
#[ignore]
fn narrow_neighbours() {
    let runs: u64 = env_or("NARROW_RUNS", 10);
    let snrs: Vec<f32> = std::env::var("NARROW_SNRS")
        .unwrap_or_else(|_| "-15,-10".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let strong_db: f32 = env_or("NARROW_STRONG_DB", 20.0);
    let strong = 10f32.powf(strong_db / 20.0);
    let towards: f32 = env_or("NARROW_TOWARDS", 10.0);
    for (name, wave) in waves_from_env("r1k7,r1k7s2") {
        for &snr in &snrs {
            let cases: [(&str, [f32; 6], [f32; 6]); 4] = [
                ("one slot alone", [1.0, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 6]),
                ("six, equal", [1.0; 6], [0.0; 6]),
                ("six, 900 Hz strong", [1.0, strong, 1.0, 1.0, 1.0, 1.0], [0.0; 6]),
                (
                    "six, 900 Hz strong and moved down",
                    [1.0, strong, 1.0, 1.0, 1.0, 1.0],
                    [0.0, -towards, 0.0, 0.0, 0.0, 0.0],
                ),
            ];
            for (case, gains, offs) in cases {
                let jobs: Vec<u64> = (0..runs).collect();
                let res = par_map(&jobs, |&s| six_run(wave, 10 + s, snr, gains, offs));
                let sum = res
                    .iter()
                    .fold((0, 0, 0, 0), |a, r| (a.0 + r.0, a.1 + r.1, a.2 + r.2, a.3 + r.3));
                println!(
                    "{name} {snr:+} dB {case} ({strong_db} dB, {towards} Hz): {}/{} decoded, {} own missed, {} foreign",
                    sum.0, sum.3, sum.1, sum.2
                );
            }
        }
    }
}

/// Averaged spectrum of a long burst around its slot: level of 10 Hz
/// bands at offsets above the centre against the strongest 10 Hz band,
/// and the power beyond ±200 Hz (in the neighbouring slots).
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --ignored --nocapture narrow_spectrum
#[test]
#[ignore]
fn narrow_spectrum() {
    use ion_dsp::dsp::Complex;
    use ion_dsp::fft::Fft;
    const N: usize = 16_384;
    let fft = Fft::new(N);
    for (name, wave) in waves_from_env("r1k6,r1k7,r1k7s2,r1k8") {
        let Wave::Mfsk { layout, mode, .. } = wave else {
            continue;
        };
        let center = 1300.0f64;
        let syms: Vec<u8> = (0..6)
            .flat_map(|i| nb::encode_bits(&random_bits(nb::K_BITS, i)))
            .collect();
        let b = modulate_symbols_with(layout, mode, 1, 1, &syms, center as f32);
        let mut psd = vec![0f64; N];
        let mut buf = vec![Complex::ZERO; N];
        let mut out = vec![Complex::ZERO; N];
        let mut segs = 0;
        let mut start = 0;
        while start + N <= b.len() {
            for (n, v) in buf.iter_mut().enumerate() {
                let w = 0.5 - 0.5 * (core::f64::consts::TAU * n as f64 / N as f64).cos();
                *v = Complex::new(b[start + n] * w as f32, 0.0);
            }
            fft.forward(&buf, &mut out);
            for (p, o) in psd.iter_mut().zip(&out) {
                *p += o.norm_sqr() as f64;
            }
            segs += 1;
            start += N / 2;
        }
        let df = AUDIO_RATE as f64 / N as f64;
        let band = |lo: f64, hi: f64| -> f64 {
            ((lo / df).ceil() as usize..=(hi / df).floor() as usize)
                .map(|i| psd[i])
                .sum::<f64>()
        };
        let total = band(0.0, 24_000.0 - df);
        let peak = (0..60)
            .map(|i| band(center - 300.0 + 10.0 * i as f64, center - 290.0 + 10.0 * i as f64))
            .fold(0.0, f64::max);
        let rel: Vec<String> = [150.0, 175.0, 200.0, 225.0, 250.0, 300.0, 400.0, 600.0]
            .iter()
            .map(|&o| {
                let up = band(center + o - 5.0, center + o + 5.0);
                format!("{o}: {:.0}", 10.0 * (up / peak).log10())
            })
            .collect();
        let outside = total - band(center - 200.0, center + 200.0);
        println!(
            "{name} ({segs} segments): dB per 10 Hz against the strongest, above the centre: {}; power beyond ±200 Hz {:.1} dB",
            rel.join(", "),
            10.0 * (outside / total).log10()
        );
    }
}

/// `secs` of white noise into one receiver, 10 ms chunks: detector
/// triggers, syncs, headers, bursts, chunks busy.
fn noise_run(layout: MfskLayout, center: f32, secs: usize, seed: u64) -> (u64, u64, u64, usize, usize) {
    let mut rx = MfskRx::with_layout(layout, center);
    let mut out = Vec::new();
    let mut ng = NoiseGen::new(seed);
    let mut buf = vec![0f32; 480];
    let mut busy = 0;
    for _ in 0..secs * 100 {
        for v in buf.iter_mut() {
            *v = 0.1 * ng.gaussian();
        }
        rx.process(&buf, &mut out);
        busy += rx.busy() as usize;
    }
    let st = rx.stats();
    (st.detections, st.accepted, st.headers, out.len(), busy)
}

/// False triggers on `NARROW_NOISE_S` seconds of white noise (default
/// 600) per receiver, in 60 s pieces.
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_noise
#[test]
#[ignore]
fn narrow_noise() {
    let secs: usize = env_or("NARROW_NOISE_S", 600);
    for (name, wave) in waves_from_env("r1,r1k7,r0k2") {
        let Wave::Mfsk { layout, center, .. } = wave else {
            continue;
        };
        let jobs: Vec<u64> = (0..secs.div_ceil(60) as u64).collect();
        let res = par_map(&jobs, |&s| noise_run(layout, center, 60, 1000 + s));
        let sum = res.iter().fold((0, 0, 0, 0, 0), |a, r| {
            (a.0 + r.0, a.1 + r.1, a.2 + r.2, a.3 + r.3, a.4 + r.4)
        });
        println!(
            "{name}, {} s of noise: {} triggers, {} synced, {} headers, {} bursts, busy {:.2}%",
            60 * jobs.len(),
            sum.0,
            sum.1,
            sum.2,
            sum.3,
            100.0 * sum.4 as f32 / (6000 * jobs.len()) as f32
        );
    }
}

/// Receiver CPU, one thread: 120 s of noise with one-frame bursts 40 s
/// apart, received in 10 ms chunks; and the transmitter.
///
/// cargo test -j 2 -p ion --release --test phy_mfsk_narrow -- --test-threads 1 --ignored --nocapture narrow_cpu
#[test]
#[ignore]
fn narrow_cpu() {
    for (name, wave) in waves_from_env("r0,r1,r1k7,r0k2") {
        let Wave::Mfsk {
            layout, mode, center, ..
        } = wave
        else {
            continue;
        };
        let secs = 120usize;
        let mut audio = vec![0f32; secs * AUDIO_RATE as usize];
        let mut ng = NoiseGen::new(5);
        for v in audio.iter_mut() {
            *v = 0.05 * ng.gaussian();
        }
        let syms = nb::encode_bits(&random_bits(nb::K_BITS, 3));
        let t = Instant::now();
        let b = modulate_symbols_with(layout, mode, 1, 1, &syms, center);
        let tx_ms = t.elapsed().as_secs_f64() * 1000.0 / (b.len() as f64 / AUDIO_RATE as f64);
        let mut sent = 0;
        let mut at = AUDIO_RATE as usize;
        while at + b.len() < audio.len() {
            for (o, v) in audio[at..].iter_mut().zip(&b) {
                *o += v;
            }
            sent += 1;
            at += b.len() + 40 * AUDIO_RATE as usize;
        }
        let mut rx = MfskRx::with_layout(layout, center);
        let mut out = Vec::new();
        let mut worst = 0f64;
        let t = Instant::now();
        for c in audio.chunks(480) {
            let tc = Instant::now();
            rx.process(c, &mut out);
            worst = worst.max(tc.elapsed().as_secs_f64() * 1000.0);
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / secs as f64;
        println!(
            "{name}: RX {ms:.1} ms per second of audio, worst 10 ms chunk {worst:.0} ms, {} of {sent} bursts; TX {tx_ms:.1} ms per second",
            out.len()
        );
    }
}
