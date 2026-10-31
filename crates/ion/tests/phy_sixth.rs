// OFDM PHY: six slices side by side in an SSB channel, 400 Hz apart
// (centres 500..2500 Hz), with two candidate numerologies: Half (50 Hz
// spacing, 7 carriers) and Sixth (25 Hz spacing, 13 carriers). Round
// trips, rejection of the five neighbours, false detections, and the
// tables of `docs/phy.md` (Six slices):
//
// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 sixth_table

use std::time::Instant;

use ion::fec::{self, Rate};
use ion::phy::{
    Constellation, Numerology, PhyRx, Pilots, RxBurst, TxOptions, burst_samples_with, capacity_bits_with,
    data_symbols_with, info_for, modulate_full,
};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

const CANDIDATES: [Numerology; 2] = [Numerology::Half, Numerology::Sixth];
/// Six slice centres in a 2.4 kHz SSB channel: carriers 350–2650 Hz.
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

/// `lead` seconds of silence, the burst, `tail` seconds of silence.
fn pad(burst: &[f32], lead: f32, tail: f32) -> Vec<f32> {
    let mut v = vec![0.0; (lead * AUDIO_RATE) as usize];
    v.extend_from_slice(burst);
    v.extend(std::iter::repeat_n(0.0, (tail * AUDIO_RATE) as usize));
    v
}

fn bit_errors(llrs: &[f32], bits: &[u8]) -> usize {
    bits.iter().zip(llrs).filter(|(b, l)| (**l < 0.0) as u8 != **b).count()
}

/// A receiver of `num` at `center` over `audio` in 4800-sample chunks.
fn rx(num: Numerology, center: f32, audio: &[f32]) -> (Vec<RxBurst>, PhyRx) {
    let mut rx = PhyRx::with_numerology(num, center);
    let mut out = Vec::new();
    for c in audio.chunks(4800) {
        rx.process(c, &mut out);
    }
    (out, rx)
}

/// Through `profile` (with a frequency offset) plus AWGN at `snr_db`
/// against `sig_power` in 2.4 kHz.
fn impair(audio: &[f32], profile: Profile, seed: u64, snr_db: Option<f32>, sig_power: f32, offset_hz: f32) -> Vec<f32> {
    let mut ch = Channel::from_profile(profile, seed).with_offset(offset_hz, 0.0);
    let mut out = vec![0.0; audio.len()];
    ch.process(audio, &mut out);
    if let Some(snr) = snr_db {
        let sigma = noise_sigma(sig_power, snr, 2400.0, AUDIO_RATE);
        add_awgn(&mut out, sigma, &mut NoiseGen::new(seed ^ 0xABCD));
    }
    out
}

/// Runs `jobs` on `ION_THREADS` threads (default 2), results in job
/// order.
fn par_map<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..jobs.len()).map(|_| None).collect());
    let threads: usize = std::env::var("ION_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
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

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Numerologies named in `SIXTH_NUMS` (comma-separated: half, sixth,
/// slice), default the two candidates.
fn nums_from_env() -> Vec<Numerology> {
    match std::env::var("SIXTH_NUMS") {
        Ok(v) => v
            .split(',')
            .filter_map(|s| match s.trim() {
                "half" => Some(Numerology::Half),
                "sixth" => Some(Numerology::Sixth),
                "slice" => Some(Numerology::Slice),
                "normal" => Some(Numerology::Normal),
                "long" => Some(Numerology::Long),
                _ => None,
            })
            .collect(),
        Err(_) => CANDIDATES.to_vec(),
    }
}

#[test]
fn sixth_layout() {
    for (num, dc, bw, sym) in [
        (Numerology::Half, (5, 5), 350.0, 0.024),
        (Numerology::Sixth, (10, 10), 325.0, 0.050),
    ] {
        let i = info_for(num);
        assert_eq!((i.data_carriers, i.sparse_data_carriers), dc);
        assert!((i.bandwidth_hz - bw).abs() < 1e-3);
        assert!((i.symbol_s - sym).abs() < 1e-6);
        assert!(num.min_center_hz() <= CENTERS[0]);
        assert!(num.is_slice() && !Numerology::ALL.contains(&num));
        for nbits in [0usize, 1, 2000] {
            let b = random_bits(nbits, 3);
            let a = modulate_full(num, Constellation::Qpsk, 1, 0, &b, 1500.0, &TxOptions::default());
            assert_eq!(
                a.len(),
                burst_samples_with(num, Pilots::Robust, Constellation::Qpsk, nbits)
            );
            if nbits > 0 {
                let secs = i.overhead_s
                    + data_symbols_with(num, Pilots::Robust, Constellation::Qpsk, nbits) as f32 * i.symbol_s;
                assert!((a.len() as f32 / AUDIO_RATE - secs).abs() < 1e-4);
            }
            let peak = a.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!((peak - ion_dsp::TX_LEVEL).abs() < 1e-4);
        }
    }
}

/// The shared 48 kHz filter: flat over the carriers (±150 Hz), ≥ 70 dB
/// down where the neighbouring slice's carriers start (±250 Hz) and
/// beyond.
#[test]
fn sixth_filter_response() {
    let taps = ion::phy::front_end_taps_for(Numerology::Sixth);
    assert_eq!(taps, ion::phy::front_end_taps_for(Numerology::Half));
    for f in (0..=150).step_by(5) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g.abs() < 0.05, "{f} Hz: {g} dB");
    }
    for f in (250..=24_000).step_by(5) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g < -70.0, "{f} Hz: {g} dB");
    }
}

/// Clean round trips at the outer centres, QPSK with the link's clip and
/// 16QAM unclipped (error-free), and header-only.
#[test]
fn sixth_clean_round_trip() {
    let qam_none = TxOptions {
        clip_db: None,
        pilots: Pilots::Robust,
        ..TxOptions::default()
    };
    for num in CANDIDATES {
        for (ci, &center) in [CENTERS[0], CENTERS[5]].iter().enumerate() {
            let cases = [
                (
                    Constellation::Qpsk,
                    TxOptions::for_burst(Constellation::Qpsk, Pilots::Robust),
                ),
                (Constellation::Qam16, qam_none),
            ];
            for (k, (c, o)) in cases.into_iter().enumerate() {
                let bits = random_bits(1500 + 37 * k, 40 + k as u64 + 10 * ci as u64);
                let user = 0x3C00 + (ci * 16 + k) as u16;
                let burst = modulate_full(num, c, user, 0, &bits, center, &o);
                let (got, _) = rx(num, center, &pad(&burst, 0.3, 0.3));
                assert_eq!(got.len(), 1, "{num:?} {center} {c:?}");
                let b = &got[0];
                assert_eq!((b.numerology, b.constellation, b.user), (num, c, user));
                let ns = data_symbols_with(num, Pilots::Robust, c, bits.len());
                assert_eq!(b.llrs.len(), capacity_bits_with(num, Pilots::Robust, c, ns));
                assert_eq!(bit_errors(&b.llrs, &bits), 0, "{num:?} {center} {c:?}");
                let err = b.start_sample as i64 - (0.3 * AUDIO_RATE) as i64;
                assert!(err.abs() <= 96, "{num:?} {center} start error {err}");
            }
            let burst = modulate_full(
                num,
                Constellation::Qpsk,
                0xBEEF,
                0x1234,
                &[],
                center,
                &TxOptions::default(),
            );
            let (got, _) = rx(num, center, &pad(&burst, 0.3, 0.3));
            assert_eq!(got.len(), 1);
            assert!(got[0].header_only);
            assert_eq!((got[0].user, got[0].extra), (0xBEEF, 0x1234));
        }
    }
}

/// Frequency offsets within the ±50 Hz pull-in of A (10 ms period).
#[test]
fn sixth_frequency_offsets() {
    let c = Constellation::Qpsk;
    for num in CANDIDATES {
        for (k, off) in [-40.0f32, -30.0, -12.5, 7.0, 30.0, 40.0].into_iter().enumerate() {
            let bits = random_bits(1000, 70 + k as u64);
            let burst = modulate_full(num, c, 9, 0, &bits, 1300.0, &TxOptions::default());
            let p = power(&burst);
            let a = impair(
                &pad(&burst, 0.3, 0.3),
                Profile::Awgn,
                300 + k as u64,
                Some(10.0),
                p,
                off,
            );
            let (got, _) = rx(num, 1300.0, &a);
            assert_eq!(got.len(), 1, "{num:?} offset {off}");
            let b = &got[0];
            let ber = bit_errors(&b.llrs, &bits) as f32 / bits.len() as f32;
            assert!(
                (b.freq_offset_hz - off).abs() < 0.5,
                "{num:?} {off}: {}",
                b.freq_offset_hz
            );
            assert!(ber < 1e-3, "{num:?} {off}: {ber}");
        }
    }
}

/// One codeword of `rate` (`fec::K` info bits) in one burst of `num` at
/// `center`, constellation `c` with the link's clip, through `profile` at
/// `snr` (2.4 kHz, against the burst's own power): (header right,
/// codeword decoded).
#[allow(clippy::too_many_arguments)]
fn codeword_run(
    num: Numerology,
    c: Constellation,
    rate: Rate,
    profile: Profile,
    snr: f32,
    seed: u64,
    center: f32,
) -> (bool, bool) {
    let info_bits = random_bits(fec::K, seed);
    let coded = fec::encode(rate, &info_bits);
    let user = (seed % 65521) as u16;
    let o = TxOptions::for_burst(c, Pilots::Robust);
    let burst = modulate_full(num, c, user, 0, &coded, center, &o);
    let p = power(&burst);
    let a = impair(&pad(&burst, 0.25, 0.3), profile, seed, Some(snr), p, 0.0);
    let (got, _) = rx(num, center, &a);
    let Some(b) = got
        .iter()
        .find(|b| b.user == user && !b.header_only && b.constellation == c)
    else {
        return (false, false);
    };
    let ok = fec::decode(rate, &b.llrs[..rate.n()]).is_some_and(|d| d == info_bits);
    (true, ok)
}

/// A QPSK rate-1/2 codeword burst of `num` at `center`, clipped at
/// `clip`: (audio, info bits, user).
fn codeword_burst(num: Numerology, center: f32, seed: u64, clip: Option<f32>) -> (Vec<f32>, Vec<u8>, u16) {
    let info = random_bits(fec::K, seed);
    let coded = fec::encode(Rate::R1_2, &info);
    let user = 0x4000 + (seed % 4096) as u16;
    let o = TxOptions {
        clip_db: clip,
        pilots: Pilots::Robust,
        ..TxOptions::default()
    };
    let burst = modulate_full(num, Constellation::Qpsk, user, 0, &coded, center, &o);
    (burst, info, user)
}

/// Adds `x` into `out` starting at `at` seconds.
fn mix_in(out: &mut Vec<f32>, x: &[f32], at: f32, gain: f32) {
    let s = (at * AUDIO_RATE) as usize;
    if out.len() < s + x.len() {
        out.resize(s + x.len(), 0.0);
    }
    for (o, v) in out[s..].iter_mut().zip(x) {
        *o += gain * v;
    }
}

/// The codeword of `user` among `got`, decoded: (found, decoded).
fn find_decode(got: &[RxBurst], user: u16, info: &[u8]) -> (bool, bool) {
    match got.iter().find(|b| b.user == user) {
        Some(b) => {
            let ok = fec::decode(Rate::R1_2, &b.llrs[..Rate::R1_2.n()]).is_some_and(|d| d == info);
            (true, ok)
        }
        None => (false, false),
    }
}

/// How one wanted slice fares among its neighbours.
#[derive(Clone, Copy, Debug, Default)]
struct SixOutcome {
    /// Bursts each receiver delivered that were not its own.
    foreign: usize,
    /// Receivers that did not deliver their own burst.
    missed: usize,
    /// Own codewords decoded.
    decoded: usize,
    /// Own codeword decoded, by centre (false where no receiver ran).
    ok: [bool; 6],
}

/// Six slices of `num` overlapping in time, QPSK 1/2 with the link's clip,
/// each at gain `gains[k]` (amplitude against 1) and frequency offset
/// `offsets[k]` Hz, AWGN at `snr` against one unit-gain slice (`None`: no
/// noise). Runs a receiver at every centre whose gain is 1.
fn six_run(num: Numerology, seed: u64, snr: Option<f32>, gains: [f32; 6], offsets: [f32; 6]) -> SixOutcome {
    let mut mix = Vec::new();
    let mut want = Vec::new();
    let mut p1 = 0.0;
    for (k, &center) in CENTERS.iter().enumerate() {
        let (mut b, info, user) = codeword_burst(num, center, 100 * seed + k as u64, Some(4.5));
        if k == 0 {
            p1 = power(&b);
        }
        if offsets[k] != 0.0 {
            b = impair(&b, Profile::Awgn, seed + k as u64, None, 1.0, offsets[k]);
        }
        // Staggered starts so that every burst overlaps all the others for
        // most of its length.
        mix_in(&mut mix, &b, 0.1 + 0.17 * ((k * 5) % 6) as f32, gains[k]);
        want.push((center, info, user, gains[k] == 1.0));
    }
    mix.extend(std::iter::repeat_n(0.0, 24000));
    if let Some(snr) = snr {
        let sigma = noise_sigma(p1, snr, 2400.0, AUDIO_RATE);
        add_awgn(&mut mix, sigma, &mut NoiseGen::new(77 + seed));
    }
    let mut o = SixOutcome::default();
    for (k, (center, info, user, listen)) in want.iter().enumerate() {
        if !listen {
            continue;
        }
        let (got, _) = rx(num, *center, &mix);
        let (found, ok) = find_decode(&got, *user, info);
        o.foreign += got.iter().filter(|b| b.user != *user).count();
        o.missed += !found as usize;
        o.decoded += ok as usize;
        o.ok[k] = ok;
    }
    o
}

/// Six full-power slices overlapping in time at +4 dB against one slice
/// (QPSK 1/2): every receiver delivers its own codeword and nothing else.
#[test]
fn sixth_six_slices() {
    for num in CANDIDATES {
        let o = six_run(num, 1, Some(4.0), [1.0; 6], [0.0; 6]);
        assert_eq!((o.foreign, o.missed), (0, 0), "{num:?}: {o:?}");
        assert!(o.decoded >= 5, "{num:?}: {o:?}");
    }
}

/// The wanted slice's reported SNR without noise, alone and among
/// neighbours: its own floor, and what the neighbours leak into it.
fn leak_snr(num: Numerology, center: f32, neighbours: &[(f32, f32, f32)]) -> f32 {
    let (w, _, wu) = codeword_burst(num, center, 11, None);
    let mut all = Vec::new();
    mix_in(&mut all, &w, 0.5, 1.0);
    mix_in(&mut all, &[0.0; 48000], 0.5 + w.len() as f32 / AUDIO_RATE, 1.0);
    for (k, &(c, gain, off)) in neighbours.iter().enumerate() {
        let (mut b, _, _) = codeword_burst(num, c, 12 + k as u64, None);
        if off != 0.0 {
            b = impair(&b, Profile::Awgn, 5 + k as u64, None, 1.0, off);
        }
        mix_in(&mut all, &b, 0.1 * k as f32, gain);
    }
    let (got, _) = rx(num, center, &all);
    got.iter().find(|b| b.user == wu).map_or(f32::NAN, |b| b.snr_db)
}

/// Table for `docs/phy.md` (Six slices): the neighbours' leakage as the
/// wanted slice's reported SNR (no noise, unclipped), then six-slice runs
/// at +2 dB with all neighbours at full power, with one neighbour 20 dB
/// stronger, and with that neighbour also 20 Hz off towards the wanted
/// slice. `PHY_RUNS` runs per case (default 10).
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 sixth_neighbours
#[test]
#[ignore]
fn sixth_neighbours() {
    let t0 = Instant::now();
    let runs: u64 = env_or("PHY_RUNS", 10);
    let strong = 10f32; // +20 dB
    for num in nums_from_env() {
        let w = 1300.0;
        let alone = leak_snr(num, w, &[]);
        let six: Vec<(f32, f32, f32)> = CENTERS.iter().filter(|&&c| c != w).map(|&c| (c, 1.0, 0.0)).collect();
        let all = leak_snr(num, w, &six);
        let mut s20 = six.clone();
        s20[2].1 = strong;
        let st = leak_snr(num, w, &s20);
        let mut s20o = s20.clone();
        s20o[2].2 = -20.0;
        let sto = leak_snr(num, w, &s20o);
        let mut s0o = six.clone();
        for (c, _, off) in s0o.iter_mut() {
            *off = if *c < w { 20.0 } else { -20.0 };
        }
        let allo = leak_snr(num, w, &s0o);
        println!(
            "{num:?} clean SNR at {w} Hz: alone {alone:.1} dB, five neighbours {all:.1}, \
             1700 Hz +20 dB {st:.1}, +20 dB and 20 Hz towards {sto:.1}, all neighbours 20 Hz towards {allo:.1}"
        );
        for snr in [0.0f32, 2.0, 4.0] {
            let cases: [(&str, [f32; 6], [f32; 6]); 4] = [
                ("one slice alone", [1.0, 0.0, 0.0, 0.0, 0.0, 0.0], [0.0; 6]),
                ("six, equal", [1.0; 6], [0.0; 6]),
                ("six, 900 Hz +20 dB", [1.0, strong, 1.0, 1.0, 1.0, 1.0], [0.0; 6]),
                (
                    "six, 900 Hz +20 dB and −20 Hz",
                    [1.0, strong, 1.0, 1.0, 1.0, 1.0],
                    [0.0, -20.0, 0.0, 0.0, 0.0, 0.0],
                ),
            ];
            for (name, gains, offs) in cases {
                let jobs: Vec<u64> = (0..runs).collect();
                let res = par_map(&jobs, |&s| six_run(num, 10 + s, Some(snr), gains, offs));
                let rxs = gains.iter().filter(|&&g| g == 1.0).count() * runs as usize;
                let dec: usize = res.iter().map(|r| r.decoded).sum();
                let miss: usize = res.iter().map(|r| r.missed).sum();
                let foreign: usize = res.iter().map(|r| r.foreign).sum();
                // The wanted slice below the strong one.
                let w500 = res.iter().filter(|r| r.ok[0]).count();
                println!(
                    "{num:?} {snr:+} dB {name}: {dec}/{rxs} decoded, {miss} own bursts missed, {foreign} foreign bursts{}",
                    if gains[1] != 1.0 {
                        format!("; 500 Hz (next to the strong one) {w500}/{runs}")
                    } else {
                        String::new()
                    }
                );
            }
        }
    }
    println!("sixth_neighbours: {:.0} s", t0.elapsed().as_secs_f32());
}

/// Table for `docs/phy.md` (Six slices): for each level the lowest SNR on
/// a 2 dB grid (2.4 kHz, against the burst's own power) at which ≥ 90% of
/// bursts deliver the right header, and at which ≥ 90% decode their
/// codeword; `PHY_RUNS` bursts per point (default 20), one codeword per
/// burst, link clip, centre 1300 Hz. Numerologies from `SIXTH_NUMS`,
/// profiles from `SIXTH_PROFILES` (awgn, poor, nvis; default all three).
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 sixth_table
#[test]
#[ignore]
fn sixth_table() {
    let t0 = Instant::now();
    let runs: u64 = env_or("PHY_RUNS", 20);
    let levels = [
        (Constellation::Qpsk, Rate::R1_4, "QPSK 1/4"),
        (Constellation::Qpsk, Rate::R1_2, "QPSK 1/2"),
        (Constellation::Psk8, Rate::R2_3, "8PSK 2/3"),
        (Constellation::Qam16, Rate::R3_4, "16QAM 3/4"),
    ];
    let profiles: Vec<Profile> = std::env::var("SIXTH_PROFILES")
        .unwrap_or_else(|_| "awgn,poor,nvis".into())
        .split(',')
        .filter_map(|s| match s.trim() {
            "awgn" => Some(Profile::Awgn),
            "poor" => Some(Profile::CcirPoor),
            "nvis" => Some(Profile::MidDisturbedNvis),
            _ => None,
        })
        .collect();
    for num in nums_from_env() {
        let mut rows = Vec::new();
        for &profile in &profiles {
            let start = if profile == Profile::Awgn { -16i32 } else { -14 };
            for &(c, rate, name) in &levels {
                let tl = Instant::now();
                let (mut hdr90, mut data90) = (None, None);
                let mut curve = Vec::new();
                let mut snr = start;
                let mut both = 0;
                while snr <= 36 && both < 2 {
                    let jobs: Vec<u64> = (0..runs).collect();
                    let res = par_map(&jobs, |&k| {
                        codeword_run(
                            num,
                            c,
                            rate,
                            profile,
                            snr as f32,
                            7 + 1000 * k + snr.unsigned_abs() as u64,
                            1300.0,
                        )
                    });
                    let h = res.iter().filter(|r| r.0).count() as f32 / runs as f32;
                    let d = res.iter().filter(|r| r.1).count() as f32 / runs as f32;
                    curve.push(format!("{snr:+}: {h:.2}/{d:.2}"));
                    if h >= 0.9 && hdr90.is_none() {
                        hdr90 = Some(snr);
                    }
                    if d >= 0.9 && data90.is_none() {
                        data90 = Some(snr);
                    }
                    both = if h >= 0.9 && d >= 0.9 { both + 1 } else { 0 };
                    snr += if h < 0.05 && d < 0.05 && snr < 10 { 4 } else { 2 };
                }
                let show = |v: Option<i32>| v.map_or("> 36".to_string(), |s| format!("{s:+}"));
                eprintln!(
                    "{num:?} {profile:?} {name}: header ≥ 90% at {} dB, codeword ≥ 90% at {} dB ({:.0} s); header/codeword: {}",
                    show(hdr90),
                    show(data90),
                    tl.elapsed().as_secs_f32(),
                    curve.join(", ")
                );
                rows.push((profile, name, show(hdr90), show(data90)));
            }
        }
        println!("{num:?}:");
        let head: Vec<String> = profiles
            .iter()
            .map(|p| format!("{p:?} header | {p:?} codeword"))
            .collect();
        println!("| Level | {} |", head.join(" | "));
        for &(_, _, name) in &levels {
            let cells: Vec<String> = profiles
                .iter()
                .map(|p| {
                    let r = rows.iter().find(|r| r.0 == *p && r.1 == name).unwrap();
                    format!("{} | {}", r.2, r.3)
                })
                .collect();
            println!("| {name} | {} |", cells.join(" | "));
        }
    }
    println!("sixth_table: {:.0} s", t0.elapsed().as_secs_f32());
}

/// `secs` of white noise into a receiver (one stream, 10 ms chunks):
/// detector triggers, preamble checks passed, bursts, busy chunks, worst
/// whole-preamble correlation.
fn noise_run(num: Numerology, secs: usize, seed: u64, center: f32) -> (ion::phy::RxStats, usize, usize, f32) {
    let mut rx = PhyRx::with_numerology(num, center);
    rx.set_debug(true);
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
    let worst = rx.debug_rho().iter().fold(0f32, |m, x| m.max(*x));
    (rx.stats(), out.len(), busy, worst)
}

/// False detections on 20 s of noise at two centres: no burst comes out.
#[test]
fn sixth_noise_only() {
    for num in CANDIDATES {
        for (k, &center) in [CENTERS[0], CENTERS[3]].iter().enumerate() {
            let (st, bursts, busy, _) = noise_run(num, 20, 99 + k as u64, center);
            assert_eq!(bursts, 0);
            assert!(st.syncs <= 1, "{num:?}: {} false syncs", st.syncs);
            assert!(busy <= 100, "{num:?}: busy in {busy} of 2000 chunks");
        }
        let mut a = vec![0f32; 48000];
        a.extend(std::iter::repeat_n(0.3, 48000));
        a.extend(std::iter::repeat_n(0.0, 48000));
        let (got, _) = rx(num, 1300.0, &a);
        assert!(got.is_empty());
    }
}

/// `SIXTH_NOISE_S` seconds of noise (default 600) at each of the six
/// centres, for each numerology: triggers, syncs, bursts, worst
/// whole-preamble correlation.
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 sixth_noise_long
#[test]
#[ignore]
fn sixth_noise_long() {
    let t0 = Instant::now();
    let secs: usize = env_or("SIXTH_NOISE_S", 600);
    for num in nums_from_env() {
        let jobs: Vec<(usize, u64)> = (0..CENTERS.len())
            .flat_map(|c| (0..secs.div_ceil(60)).map(move |k| (c, 1000 + 37 * c as u64 + k as u64)))
            .collect();
        let res = par_map(&jobs, |&(c, seed)| noise_run(num, 60, seed, CENTERS[c]));
        for (c, &center) in CENTERS.iter().enumerate() {
            let mine: Vec<_> = res
                .iter()
                .zip(&jobs)
                .filter(|(_, j)| j.0 == c)
                .map(|(r, _)| r)
                .collect();
            let cand: u64 = mine.iter().map(|r| r.0.candidates).sum();
            let syncs: u64 = mine.iter().map(|r| r.0.syncs).sum();
            let bursts: usize = mine.iter().map(|r| r.1).sum();
            let worst = mine.iter().map(|r| r.3).fold(0f32, f32::max);
            println!(
                "{num:?} {center} Hz, {} s of noise: {cand} triggers, {syncs} syncs, {bursts} bursts, worst preamble correlation {worst:.3}",
                60 * mine.len()
            );
        }
    }
    println!("sixth_noise_long: {:.0} s", t0.elapsed().as_secs_f32());
}

/// Preamble statistics for the thresholds: whole-preamble correlation of
/// noise-only candidates (120 s) against real bursts at low SNR.
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 diag_sixth_rho
#[test]
#[ignore]
fn diag_sixth_rho() {
    for num in nums_from_env() {
        let jobs: Vec<u64> = (0..4).collect();
        let res = par_map(&jobs, |&k| noise_run(num, 30, 1234 + k, 1300.0));
        let cand: u64 = res.iter().map(|x| x.0.candidates).sum();
        let syncs: u64 = res.iter().map(|x| x.0.syncs).sum();
        let worst = res.iter().map(|x| x.3).fold(0f32, f32::max);
        eprintln!("{num:?} noise 120 s: {cand} candidates, {syncs} syncs, worst rho {worst:.3}");
        let c = Constellation::Qpsk;
        for profile in [Profile::Awgn, Profile::CcirPoor] {
            for snr in [-16.0f32, -14.0, -12.0, -10.0, -8.0, -6.0] {
                let jobs: Vec<u64> = (0..40).collect();
                let res = par_map(&jobs, |&seed| {
                    let bits = random_bits(20 * 40, seed);
                    let burst = modulate_full(num, c, 1, 0, &bits, 1300.0, &TxOptions::default());
                    let p = power(&burst);
                    let a = impair(&pad(&burst, 0.25, 0.25), profile, 900 + seed, Some(snr), p, 0.0);
                    let mut rx = PhyRx::with_numerology(num, 1300.0);
                    rx.set_debug(true);
                    let mut out = Vec::new();
                    rx.process(&a, &mut out);
                    (
                        rx.debug_rho().iter().fold(0f32, |m, x| m.max(*x)),
                        rx.stats().candidates > 0,
                        rx.stats().syncs > 0,
                        out.len() == 1,
                    )
                });
                let mut v: Vec<f32> = res.iter().map(|x| x.0).collect();
                v.sort_by(|a, b| a.total_cmp(b));
                eprintln!(
                    "{num:?} {profile:?} {snr} dB: rho min {:.3}, 10% {:.3}, median {:.3}; detected {}, synced {}, header ok {} / 40",
                    v[0],
                    v[4],
                    v[20],
                    res.iter().filter(|x| x.1).count(),
                    res.iter().filter(|x| x.2).count(),
                    res.iter().filter(|x| x.3).count()
                );
            }
        }
    }
}

/// Receiver CPU, one thread: ms per second of audio for one receiver
/// over 60 s of noise with a burst every 10 s, for Normal, Slice and the
/// candidates.
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 sixth_cpu
#[test]
#[ignore]
fn sixth_cpu() {
    for num in [
        Numerology::Normal,
        Numerology::Slice,
        Numerology::Half,
        Numerology::Sixth,
    ] {
        let center = if num == Numerology::Slice || num == Numerology::Normal {
            1500.0
        } else {
            1300.0
        };
        let mut audio = vec![0f32; 60 * AUDIO_RATE as usize];
        let mut ng = NoiseGen::new(5);
        for v in audio.iter_mut() {
            *v = 0.01 * ng.gaussian();
        }
        let mut sent = 0;
        for k in 0..6 {
            let bits = random_bits(capacity_bits_with(num, Pilots::Robust, Constellation::Qpsk, 40), k);
            let b = modulate_full(num, Constellation::Qpsk, 1, 0, &bits, center, &TxOptions::default());
            let at = 10 * k as usize * AUDIO_RATE as usize + 4800;
            if at + b.len() <= audio.len() {
                for (o, v) in audio[at..].iter_mut().zip(&b) {
                    *o += v;
                }
                sent += 1;
            }
        }
        let t = Instant::now();
        let (got, _) = rx(num, center, &audio);
        let ms = t.elapsed().as_secs_f64() * 1000.0 / 60.0;
        let taps = ion::phy::front_end_taps_for(num).len();
        println!(
            "{num:?}: {ms:.1} ms per second of audio ({taps} taps), {} of {sent} bursts",
            got.len()
        );
    }
}

#[test]
#[ignore]
fn diag_sixth_sync() {
    use ion::phy::Genie;
    let snr: f32 = env_or("SIXTH_SNR", -6.0);
    for num in nums_from_env() {
        let jobs: Vec<u64> = (0..40).collect();
        let res = par_map(&jobs, |&seed| {
            let c = Constellation::Qpsk;
            let bits = random_bits(20 * 40, seed);
            let burst = modulate_full(num, c, 1, 0, &bits, 1300.0, &TxOptions::default());
            let p = power(&burst);
            let a = impair(&pad(&burst, 0.25, 0.25), Profile::Awgn, 900 + seed, Some(snr), p, 0.0);
            let mut r = PhyRx::with_numerology(num, 1300.0);
            let mut out = Vec::new();
            r.process(&a, &mut out);
            let st = r.stats();
            let mut g = PhyRx::with_numerology(num, 1300.0);
            g.set_genie_header(Some(Genie {
                constellation: c,
                data_symbols: data_symbols_with(num, Pilots::Robust, c, bits.len()),
                user: 1,
                start: (0.25 * AUDIO_RATE) as u64,
                pilots: Pilots::Robust,
            }));
            let mut og = Vec::new();
            g.process(&a, &mut og);
            let gi = og.first().map(|b| {
                (
                    b.freq_offset_hz,
                    b.start_sample as i64 - (0.25 * AUDIO_RATE) as i64,
                    b.snr_db,
                    bit_errors(&b.llrs, &bits) as f32 / bits.len() as f32,
                )
            });
            (out.len(), st, gi)
        });
        for (k, r) in res.iter().enumerate() {
            if r.0 != 1 || std::env::var("SIXTH_ALL").is_ok() {
                eprintln!(
                    "{num:?} seed {k}: delivered {}, syncs {} hdr ok {} fail {}; genie {:?}",
                    r.0, r.1.syncs, r.1.header_ok, r.1.header_fail, r.2
                );
            }
        }
        let ok: Vec<_> = res.iter().filter_map(|r| r.2).collect();
        let fo: Vec<String> = ok.iter().map(|g| format!("{:.2}/{}", g.0, g.1)).collect();
        eprintln!("{num:?} genie freq/start: {}", fo.join(" "));
    }
}

/// Per-burst uncoded QPSK BER at high SNR on fading (real header; a
/// missing burst shows as −1), long (one rate-1/4 codeword) and short
/// (48 symbols) bursts: where the codeword-loss floors come from.
///
/// cargo test -p ion --release --test phy_sixth -- --ignored --nocapture --test-threads 1 diag_sixth_floor
#[test]
#[ignore]
fn diag_sixth_floor() {
    let snr: f32 = env_or("SIXTH_SNR", 30.0);
    let c = Constellation::Qpsk;
    for num in nums_from_env() {
        for profile in [Profile::CcirPoor, Profile::MidDisturbedNvis] {
            for long in [true, false] {
                let jobs: Vec<u64> = (0..20).collect();
                let res = par_map(&jobs, |&k| {
                    let seed = 7 + 1000 * k + snr as u64;
                    let nbits = if long {
                        Rate::R1_4.n()
                    } else {
                        capacity_bits_with(num, Pilots::Robust, c, 48)
                    };
                    let bits = random_bits(nbits, seed);
                    let burst = modulate_full(num, c, 1, 0, &bits, 1300.0, &TxOptions::for_burst(c, Pilots::Robust));
                    let p = power(&burst);
                    let a = impair(&pad(&burst, 0.25, 0.3), profile, seed, Some(snr), p, 0.0);
                    let mut r = PhyRx::with_numerology(num, 1300.0);
                    r.set_debug(true);
                    let mut out = Vec::new();
                    r.process(&a, &mut out);
                    match out.first() {
                        Some(b) => (
                            bit_errors(&b.llrs, &bits) as f32 / nbits as f32,
                            b.snr_db,
                            b.freq_offset_hz,
                            b.start_sample as f32 - 0.25 * AUDIO_RATE,
                        ),
                        None => (-1.0, 0.0, 0.0, 0.0),
                    }
                });
                let line: Vec<String> = res
                    .iter()
                    .map(|r| format!("{:.0e}/{:.0}/{:.2}/{:.0}", r.0, r.1, r.2, r.3))
                    .collect();
                eprintln!(
                    "{num:?} {profile:?} {} {snr} dB (BER/SNR/offset/start error): {}",
                    if long { "long" } else { "short" },
                    line.join(" ")
                );
            }
        }
    }
}
