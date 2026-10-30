//! Low-PAPR data modes (DFT spreading and a data gain over the header,
//! `phy::DataMode`): every numerology and constellation round-trips, the
//! header names the mode, and plain bursts are as before.

use ion::phy::{self, Constellation, DataMode, Numerology, PhyRx, Pilots, TxOptions};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

fn random_bits(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 33) as u8 & 1
        })
        .collect()
}

fn receive(num: Numerology, a: &[f32], snr: Option<f32>, profile: Profile, seed: u64) -> Vec<phy::RxBurst> {
    let mut x = vec![0.0f32; 4800];
    x.extend_from_slice(a);
    x.extend(vec![0.0f32; 9600]);
    let mut y = x.clone();
    if profile != Profile::Awgn {
        let mut ch = Channel::from_profile(profile, seed);
        y.resize(y.len() + ch.latency(), 0.0);
        let inp = y.clone();
        ch.process(&inp, &mut y);
    }
    if let Some(snr) = snr {
        let p = a.iter().map(|v| v * v).sum::<f32>() / a.len() as f32;
        add_awgn(
            &mut y,
            noise_sigma(p, snr, 2400.0, AUDIO_RATE),
            &mut NoiseGen::new(seed ^ 9),
        );
    }
    let mut rx = PhyRx::with_numerology(num, num.center_hz());
    let mut out = Vec::new();
    rx.process(&y, &mut out);
    out
}

const NUMS: [Numerology; 5] = [
    Numerology::Normal,
    Numerology::NormalWide,
    Numerology::Long,
    Numerology::LongWide,
    Numerology::Slice,
];

#[test]
fn every_mode_round_trips_clean() {
    for num in NUMS {
        for c in Constellation::ALL {
            // (spread, gain, pilots, clipped): unclipped bursts must come
            // back exact; clipped ones carry the clip's distortion, which
            // leaves raw errors even without noise.
            for (spread, gain, pilots, clipped) in [
                (true, 0.0, Pilots::Robust, false),
                (true, 1.5, Pilots::Robust, false),
                (true, 2.25, Pilots::Sparse, false),
                (false, 1.5, Pilots::Robust, false),
                (true, phy::low_papr_gain_db(c), Pilots::Robust, true),
                (false, 1.5, Pilots::Robust, true),
            ] {
                let base = if clipped && spread {
                    TxOptions::low_papr(c, pilots)
                } else {
                    TxOptions::for_burst(c, pilots)
                };
                let opt = TxOptions {
                    spread,
                    data_gain_db: gain,
                    pilots,
                    clip_db: if clipped { base.clip_db } else { None },
                    ..base
                };
                let bits = random_bits(phy::capacity_bits_with(num, pilots, c, 9), 7);
                let a = phy::modulate_full(num, c, 0x1234, 0, &bits, num.center_hz(), &opt);
                let out = receive(num, &a, None, Profile::Awgn, 1);
                assert_eq!(out.len(), 1, "{num:?} {c:?} {spread} {gain}");
                let b = &out[0];
                assert_eq!(b.mode, DataMode::new(spread, gain), "{num:?} {c:?}");
                assert_eq!((b.user, b.constellation, b.pilots), (0x1234, c, pilots));
                let errs = bits
                    .iter()
                    .zip(&b.llrs)
                    .filter(|(x, l)| (**l < 0.0) != (**x == 1))
                    .count();
                let allowed = if clipped { (bits.len() / 50).max(4) } else { 0 };
                assert!(
                    errs <= allowed,
                    "{num:?} {c:?} spread {spread} gain {gain} clipped {clipped}: {errs} bit errors"
                );
            }
        }
    }
}

#[test]
fn plain_bursts_keep_mode_zero() {
    let c = Constellation::Qpsk;
    let bits = random_bits(phy::capacity_bits_with(Numerology::NormalWide, Pilots::Robust, c, 5), 3);
    let a = phy::modulate_full(
        Numerology::NormalWide,
        c,
        7,
        0,
        &bits,
        1600.0,
        &TxOptions::for_burst(c, Pilots::Robust),
    );
    let out = receive(Numerology::NormalWide, &a, None, Profile::Awgn, 1);
    assert_eq!(out[0].mode, DataMode::PLAIN);
}

/// Raw bit errors through noise and fading: the spread receiver is not
/// worse than a plain one by more than a little at equal mean SNR.
#[test]
fn spread_uncoded_ber_is_sane() {
    for (num, c, snr, prof) in [
        (Numerology::NormalWide, Constellation::Qpsk, 6.0, Profile::Awgn),
        (Numerology::NormalWide, Constellation::Qam16, 16.0, Profile::Awgn),
        (Numerology::NormalWide, Constellation::Qpsk, 10.0, Profile::CcirPoor),
        (Numerology::Slice, Constellation::Qpsk, 0.0, Profile::Awgn),
    ] {
        let mut ber = [0f64; 2];
        for (k, spread) in [false, true].into_iter().enumerate() {
            let (mut e, mut n) = (0usize, 0usize);
            for seed in 0..6u64 {
                let opt = TxOptions {
                    spread,
                    ..TxOptions::for_burst(c, Pilots::Robust)
                };
                let bits = random_bits(phy::capacity_bits_with(num, Pilots::Robust, c, 40), seed);
                let a = phy::modulate_full(num, c, 1, 0, &bits, num.center_hz(), &opt);
                let out = receive(num, &a, Some(snr), prof, 100 + seed);
                let Some(b) = out.first() else {
                    n += bits.len();
                    e += bits.len() / 2;
                    continue;
                };
                e += bits
                    .iter()
                    .zip(&b.llrs)
                    .filter(|(x, l)| (**l < 0.0) != (**x == 1))
                    .count();
                n += bits.len();
            }
            ber[k] = e as f64 / n as f64;
        }
        assert!(ber[1] < 2.0 * ber[0] + 1e-3, "{num:?} {c:?} {prof:?}: {ber:?}");
    }
}

/// The SNR report of low-PAPR bursts follows the channel (AWGN, against
/// the burst's mean power), as the gear shift needs.
#[test]
fn low_papr_snr_report_tracks_the_channel() {
    for (num, c) in [
        (Numerology::NormalWide, Constellation::Qpsk),
        (Numerology::Slice, Constellation::Qpsk),
        (Numerology::LongWide, Constellation::Qpsk),
        (Numerology::NormalWide, Constellation::Psk8),
    ] {
        for snr in [0.0f32, 10.0, 20.0] {
            let opt = TxOptions::low_papr(c, Pilots::Robust);
            let mut got = Vec::new();
            for seed in 0..3u64 {
                let bits = random_bits(phy::capacity_bits_with(num, Pilots::Robust, c, 40), seed);
                let a = phy::modulate_full(num, c, 1, 0, &bits, num.center_hz(), &opt);
                if let Some(b) = receive(num, &a, Some(snr), Profile::Awgn, 50 + seed).first() {
                    got.push(b.snr_db);
                }
            }
            let m = got.iter().sum::<f32>() / got.len() as f32;
            // Within 1 dB: the sender's own clipping is left out.
            assert!((m - snr).abs() < 1.0, "{num:?} {c:?} {snr}: {got:?}");
        }
    }
}

/// The delay span (`chan::ChannelMeas`) of plain and low-PAPR bursts on
/// AWGN, one change at a time, blanker on and off: a single path should
/// read 0–3 ms whatever the data mode.
#[test]
#[ignore]
fn diag_delay_by_mode() {
    let c = Constellation::Qpsk;
    for num in [Numerology::Slice, Numerology::LongWide, Numerology::NormalWide] {
        for snr in [2.0f32, 20.0] {
            for (name, opt) in [
                ("plain", TxOptions::for_burst(c, Pilots::Robust)),
                (
                    "plain+gain",
                    TxOptions {
                        data_gain_db: 2.25,
                        ..TxOptions::for_burst(c, Pilots::Robust)
                    },
                ),
                (
                    "spread",
                    TxOptions {
                        spread: true,
                        ..TxOptions::for_burst(c, Pilots::Robust)
                    },
                ),
                (
                    "spread noclip",
                    TxOptions {
                        spread: true,
                        clip_db: None,
                        ..TxOptions::for_burst(c, Pilots::Robust)
                    },
                ),
                ("lowpapr", TxOptions::low_papr(c, Pilots::Robust)),
            ] {
                for blank in [true, false] {
                    let mut d = Vec::new();
                    for seed in 0..4u64 {
                        let bits = random_bits(phy::capacity_bits_with(num, Pilots::Robust, c, 20), seed);
                        let a = phy::modulate_full(num, c, 1, 0, &bits, num.center_hz(), &opt);
                        let mut x = vec![0.0f32; 4800];
                        x.extend_from_slice(&a);
                        x.extend(vec![0.0f32; 9600]);
                        let p = a.iter().map(|v| v * v).sum::<f32>() / a.len() as f32;
                        add_awgn(
                            &mut x,
                            noise_sigma(p, snr, 2400.0, AUDIO_RATE),
                            &mut NoiseGen::new(seed ^ 9),
                        );
                        let mut rx = PhyRx::with_numerology(num, num.center_hz());
                        rx.set_blanker(blank);
                        let mut out = Vec::new();
                        rx.process(&x, &mut out);
                        if let Some(b) = out.first() {
                            d.push((b.chan.delay_ms, b.chan.raw[2]));
                        }
                    }
                    println!("{num:?} {snr:+} dB {name:<14} blank {blank:<5}: {d:.1?}");
                }
            }
        }
    }
}

#[test]
#[ignore]
fn diag_clean_errors() {
    for num in NUMS {
        for c in Constellation::ALL {
            for (name, opt) in [
                ("lowpapr", TxOptions::low_papr(c, Pilots::Robust)),
                (
                    "spread0",
                    TxOptions {
                        spread: true,
                        ..TxOptions::for_burst(c, Pilots::Robust)
                    },
                ),
                (
                    "spread0-noclip",
                    TxOptions {
                        spread: true,
                        clip_db: None,
                        ..TxOptions::for_burst(c, Pilots::Robust)
                    },
                ),
            ] {
                let mut errs = 0;
                let mut n = 0;
                for seed in 0..4 {
                    let bits = random_bits(phy::capacity_bits_with(num, Pilots::Robust, c, 20), seed);
                    let a = phy::modulate_full(num, c, 1, 0, &bits, num.center_hz(), &opt);
                    let out = receive(num, &a, None, Profile::Awgn, 1);
                    errs += bits
                        .iter()
                        .zip(&out[0].llrs)
                        .filter(|(x, l)| (**l < 0.0) != (**x == 1))
                        .count();
                    n += bits.len();
                }
                println!("{num:?} {c:?} {name}: {errs}/{n}");
            }
        }
    }
}

/// Spread 16QAM has no error floor: at high SNR the clip's distortion, which
/// falls on the outer points' despread symbols, must not leave confident
/// wrong LLRs that the code cannot correct (it lost about 7% of codewords at
/// 30–40 dB before).
#[test]
fn spread_16qam_decodes_at_high_snr() {
    let (num, c, r) = (Numerology::NormalWide, Constellation::Qam16, ion::fec::Rate::R2_3);
    let per_burst = 60 * phy::info_for(num).data_carriers * c.bits_per_symbol() / r.n();
    let (mut bad, mut total) = (0, 0);
    for seed in 0..30u64 {
        let infos: Vec<Vec<u8>> = (0..per_burst)
            .map(|k| random_bits(ion::fec::K, seed * 1000 + k as u64))
            .collect();
        let bits: Vec<u8> = infos.iter().flat_map(|i| ion::fec::encode(r, i)).collect();
        let opt = TxOptions::low_papr(c, Pilots::Robust);
        let a = phy::modulate_full(num, c, 1, 0, &bits, num.center_hz(), &opt);
        let out = receive(num, &a, Some(35.0), Profile::Awgn, 200 + seed);
        assert_eq!(out.len(), 1, "seed {seed}");
        for (info, llrs) in infos.iter().zip(out[0].llrs.chunks_exact(r.n())) {
            total += 1;
            if ion::fec::decode(r, llrs).as_ref() != Some(info) {
                bad += 1;
            }
        }
    }
    assert_eq!(bad, 0, "{bad} of {total} codewords lost");
}
