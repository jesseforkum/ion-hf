// OFDM PHY: round trips, acquisition, streaming and channel tests. The
// results tables in `docs/phy.md` come from the ignored `tables` and
// `numerology_tables` tests:
//
// cargo test -p ion --release --test phy -- --ignored --nocapture tables

use ion::phy::{
    Constellation, Genie, Numerology, PhyRx, Pilots, RxBurst, TxOptions, burst_samples, burst_samples_for,
    burst_samples_with, capacity_bits, capacity_bits_for, capacity_bits_with, data_symbols, data_symbols_for,
    data_symbols_with, info, info_for, modulate, modulate_full, modulate_with,
};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, Complex, Fir, Nco, NoiseGen, lowpass_taps, noise_sigma};

const CENTER: f32 = 1500.0;

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

fn rx_all(audio: &[f32], chunk: usize) -> Vec<RxBurst> {
    let mut rx = PhyRx::new(CENTER);
    let mut out = Vec::new();
    for c in audio.chunks(chunk.max(1)) {
        rx.process(c, &mut out);
    }
    out
}

/// `lead` seconds of silence, the burst, `tail` seconds of silence.
fn pad(burst: &[f32], lead: f32, tail: f32) -> Vec<f32> {
    let mut v = vec![0.0; (lead * AUDIO_RATE) as usize];
    v.extend_from_slice(burst);
    v.extend(std::iter::repeat_n(0.0, (tail * AUDIO_RATE) as usize));
    v
}

/// Through a channel profile (with offset/drift) plus AWGN at `snr_db`
/// against `sig_power` in 2.4 kHz.
#[allow(clippy::too_many_arguments)]
fn impair(
    audio: &[f32],
    profile: Profile,
    seed: u64,
    snr_db: Option<f32>,
    sig_power: f32,
    offset_hz: f32,
    drift: f32,
) -> Vec<f32> {
    let mut ch = Channel::from_profile(profile, seed).with_offset(offset_hz, drift);
    let mut out = vec![0.0; audio.len()];
    ch.process(audio, &mut out);
    if let Some(snr) = snr_db {
        let sigma = noise_sigma(sig_power, snr, 2400.0, AUDIO_RATE);
        add_awgn(&mut out, sigma, &mut NoiseGen::new(seed ^ 0xABCD));
    }
    out
}

fn genie(c: Constellation, nbits: usize, user: u16, lead_s: f32) -> Genie {
    Genie {
        constellation: c,
        data_symbols: data_symbols(c, nbits),
        user,
        // The channel simulator delays everything by its Hilbert filter.
        start: (lead_s * AUDIO_RATE) as u64 + Channel::from_profile(Profile::Awgn, 0).latency() as u64,
        pilots: Pilots::Robust,
    }
}

fn bit_errors(llrs: &[f32], bits: &[u8]) -> usize {
    bits.iter().zip(llrs).filter(|(b, l)| (**l < 0.0) as u8 != **b).count()
}

#[test]
fn numerology() {
    let i = info();
    assert_eq!(i.data_carriers, 44);
    assert!((i.symbol_s - 0.024).abs() < 1e-6);
    assert!((i.bandwidth_hz - 2450.0).abs() < 1e-3);
    for c in Constellation::ALL {
        for nbits in [1usize, 500, 5000] {
            let b = random_bits(nbits, 1);
            let a = modulate(c, 7, &b, CENTER);
            assert_eq!(a.len(), burst_samples(c, nbits));
            let secs = i.overhead_s + data_symbols(c, nbits) as f32 * i.symbol_s;
            assert!((a.len() as f32 / AUDIO_RATE - secs).abs() < 1e-4);
            let peak = a.iter().fold(0f32, |m, v| m.max(v.abs()));
            assert!((peak - ion_dsp::TX_LEVEL).abs() < 1e-4);
        }
    }
}

fn rx_num(num: Numerology, audio: &[f32], chunk: usize) -> (Vec<RxBurst>, PhyRx) {
    let mut rx = PhyRx::with_numerology(num, num.center_hz());
    let mut out = Vec::new();
    for c in audio.chunks(chunk.max(1)) {
        rx.process(c, &mut out);
    }
    (out, rx)
}

fn other(num: Numerology) -> Numerology {
    Numerology::of(!num.is_long(), num.is_wide())
}

#[test]
fn numerology_long_and_header_only_lengths() {
    let n = info_for(Numerology::Normal);
    let i = info_for(Numerology::Long);
    assert_eq!(i.data_carriers, 88);
    assert!((i.symbol_s - 0.050).abs() < 1e-6);
    assert!((i.bandwidth_hz - 2425.0).abs() < 1e-3);
    assert!((i.overhead_s - 0.242).abs() < 1e-6, "{}", i.overhead_s);
    assert_eq!(n.overhead_s, info().overhead_s);
    for num in Numerology::ALL {
        let inf = info_for(num);
        for c in Constellation::ALL {
            for nbits in [0usize, 1, 500, 5000] {
                let b = random_bits(nbits, 1);
                let extra = if nbits == 0 { 0xBEEF } else { 0 };
                let a = modulate_full(num, c, 7, extra, &b, num.center_hz(), &TxOptions::default());
                assert_eq!(a.len(), burst_samples_for(num, c, nbits), "{num:?} {c:?} {nbits}");
                let ns = data_symbols_for(num, c, nbits);
                assert_eq!(ns == 0, nbits == 0);
                assert!(capacity_bits_for(num, c, ns) >= nbits);
                // A header-only burst sends its header twice.
                let hdr_syms = if num.is_long() { 2 } else { 4 };
                let sent = if ns == 0 { hdr_syms } else { ns };
                let secs = inf.overhead_s + sent as f32 * inf.symbol_s;
                assert!((a.len() as f32 / AUDIO_RATE - secs).abs() < 1e-4);
                let peak = a.iter().fold(0f32, |m, v| m.max(v.abs()));
                assert!((peak - ion_dsp::TX_LEVEL).abs() < 1e-4);
            }
        }
        // Normal through modulate_full is modulate_with.
        if num == Numerology::Normal {
            let b = random_bits(700, 2);
            let o = TxOptions::default();
            assert_eq!(
                modulate_full(num, Constellation::Psk8, 9, 0, &b, num.center_hz(), &o),
                modulate_with(Constellation::Psk8, 9, &b, CENTER, &o)
            );
        }
    }
    // modulate keeps its old meaning: no bits is still one data symbol.
    assert_eq!(
        modulate(Constellation::Qpsk, 1, &[], CENTER).len(),
        burst_samples(Constellation::Qpsk, 0)
    );
    assert_eq!(
        burst_samples(Constellation::Qpsk, 0),
        burst_samples_for(Numerology::Normal, Constellation::Qpsk, 1)
    );
}

#[test]
fn long_clean_round_trip_all_constellations() {
    let num = Numerology::Long;
    for (k, c) in Constellation::ALL.into_iter().enumerate() {
        let bits = random_bits(5000 + 77 * k, 10 + k as u64);
        let burst = modulate_full(num, c, 0xA5C3, 0, &bits, num.center_hz(), &TxOptions::default());
        let audio = pad(&burst, 0.3, 0.3);
        let (got, _) = rx_num(num, &audio, 4800);
        assert_eq!(got.len(), 1, "{c:?}");
        let b = &got[0];
        assert_eq!(b.numerology, num);
        assert!(!b.header_only);
        assert_eq!(b.extra, 0);
        assert_eq!(b.constellation, c);
        assert_eq!(b.user, 0xA5C3);
        assert_eq!(
            b.llrs.len(),
            capacity_bits_for(num, c, data_symbols_for(num, c, bits.len()))
        );
        assert_eq!(bit_errors(&b.llrs, &bits), 0, "{c:?}");
        assert!(b.llrs[bits.len()..].iter().all(|&l| l > 0.0));
        let err = b.start_sample as i64 - (0.3 * AUDIO_RATE) as i64;
        assert!(err.abs() <= 48);
    }
}

#[test]
fn header_only_round_trip() {
    for num in Numerology::ALL {
        for (k, (user, extra)) in [(0u16, 0u16), (0xFFFF, 0xFFFF), (0x1234, 0xA5C3), (1, 0x8000)]
            .into_iter()
            .enumerate()
        {
            let burst = modulate_full(
                num,
                Constellation::Qam16,
                user,
                extra,
                &[],
                num.center_hz(),
                &TxOptions::default(),
            );
            let p = power(&burst);
            let a = impair(
                &pad(&burst, 0.2, 0.3),
                Profile::Awgn,
                40 + k as u64,
                Some(10.0),
                p,
                3.0,
                0.0,
            );
            let (got, rx) = rx_num(num, &a, 480);
            assert_eq!(got.len(), 1, "{num:?}");
            let b = &got[0];
            assert!(b.header_only);
            assert_eq!(b.numerology, num);
            assert_eq!((b.user, b.extra), (user, extra));
            assert!(b.llrs.is_empty());
            assert_eq!(b.constellation, Constellation::Qpsk);
            assert!((b.freq_offset_hz - 3.0).abs() < 1.0, "{}", b.freq_offset_hz);
            assert!((b.snr_db - 10.0).abs() < 3.0, "{num:?} snr {}", b.snr_db);
            let err = b.start_sample as i64 - (0.2 * AUDIO_RATE) as i64 - 200;
            assert!(err.abs() <= 48, "{num:?} start error {err}");
            assert!(!rx.busy());
        }
        // The first header copy blanked out: the receiver waits for the
        // second and decodes both together.
        let mut burst = modulate_full(
            num,
            Constellation::Qpsk,
            0x4242,
            0x1717,
            &[],
            num.center_hz(),
            &TxOptions::default(),
        );
        let (pre_bb, hdr_bb) = if num.is_long() {
            (640 + 400, 2 * 400)
        } else {
            (352 + 192, 4 * 192)
        };
        let blank = 6 * pre_bb + 240..6 * (pre_bb + hdr_bb) + 240;
        burst[blank].fill(0.0);
        let p = power(&burst);
        let a = impair(&pad(&burst, 0.2, 0.3), Profile::Awgn, 9, Some(10.0), p, 0.0, 0.0);
        let (got, rx) = rx_num(num, &a, 480);
        assert_eq!(got.len(), 1, "{num:?}: {:?}", rx.stats());
        assert!(got[0].header_only && got[0].user == 0x4242 && got[0].extra == 0x1717);
        // A header-only burst followed at once by a data burst: both arrive.
        let bits = random_bits(3000, 3);
        let mut audio = vec![0.0; 9600];
        let o = TxOptions::default();
        audio.extend(modulate_full(num, Constellation::Qpsk, 5, 77, &[], num.center_hz(), &o));
        audio.extend(modulate_full(
            num,
            Constellation::Psk8,
            6,
            0,
            &bits,
            num.center_hz(),
            &o,
        ));
        audio.extend(modulate_full(num, Constellation::Qpsk, 7, 78, &[], num.center_hz(), &o));
        audio.extend(vec![0.0; 9600]);
        let (got, _) = rx_num(num, &audio, 1000);
        assert_eq!(got.len(), 3, "{num:?}");
        assert!(got[0].header_only && got[0].user == 5 && got[0].extra == 77);
        assert!(!got[1].header_only && got[1].user == 6);
        assert_eq!(bit_errors(&got[1].llrs, &bits), 0);
        assert!(got[2].header_only && got[2].user == 7 && got[2].extra == 78);
    }
}

#[test]
fn long_survives_nvis_echo() {
    // ITU mid-latitude disturbed NVIS (7 ms, 1 Hz) at 20 dB: the Long
    // numerology's CP and pilot grid cover the echo, so the uncoded BER is
    // that of a fading channel at this SNR, not an ISI floor.
    let num = Numerology::Long;
    let c = Constellation::Qpsk;
    let (mut errs, mut total, mut found) = (0usize, 0usize, 0);
    for seed in 0..6u64 {
        let bits = random_bits(capacity_bits_for(num, c, 20), 80 + seed);
        let burst = modulate_full(num, c, 0x0E0E, 0, &bits, num.center_hz(), &TxOptions::default());
        let p = power(&burst);
        let a = impair(
            &pad(&burst, 0.2, 0.3),
            Profile::MidDisturbedNvis,
            300 + seed,
            Some(20.0),
            p,
            0.0,
            0.0,
        );
        let (got, _) = rx_num(num, &a, 4800);
        if let Some(b) = got.first() {
            found += 1;
            errs += bit_errors(&b.llrs, &bits);
            total += bits.len();
        }
    }
    let ber = errs as f64 / total.max(1) as f64;
    assert!(found >= 5);
    assert!(ber < 1e-2, "BER {ber}");
}

#[test]
fn long_frequency_offsets() {
    let num = Numerology::Long;
    let c = Constellation::Qpsk;
    let bits = random_bits(capacity_bits_for(num, c, 20), 9);
    let burst = modulate_full(num, c, 77, 0, &bits, num.center_hz(), &TxOptions::default());
    let p = power(&burst);
    let audio = pad(&burst, 0.3, 0.3);
    for (k, off) in [-50.0f32, -23.0, 0.4, 31.0, 50.0].into_iter().enumerate() {
        let a = impair(&audio, Profile::Awgn, 100 + k as u64, Some(12.0), p, off, 0.0);
        let (got, _) = rx_num(num, &a, 4800);
        assert_eq!(got.len(), 1, "offset {off}");
        let b = &got[0];
        let ber = bit_errors(&b.llrs, &bits) as f32 / bits.len() as f32;
        assert!((b.freq_offset_hz - off).abs() < 0.5);
        assert!(ber < 2e-3, "offset {off}: BER {ber}");
    }
}

/// Each numerology's bursts (data and header-only) through the other
/// numerology's receiver: nothing is delivered, and the preamble check
/// passes nothing.
#[test]
fn numerologies_do_not_cross_trigger() {
    for num in Numerology::ALL {
        let mut audio = Vec::new();
        let mut sent = 0;
        let mut ng = NoiseGen::new(17);
        for (k, (prof, off)) in [
            (Profile::Awgn, 0.0f32),
            (Profile::Awgn, 37.0),
            (Profile::Awgn, -45.0),
            (Profile::MidDisturbedNvis, 5.0),
            (Profile::CcirPoor, -12.0),
        ]
        .into_iter()
        .enumerate()
        {
            for c in Constellation::ALL {
                let bits = random_bits(2000, k as u64);
                let o = TxOptions::default();
                for b in [
                    modulate_full(num, c, 1, 0, &bits, num.center_hz(), &o),
                    modulate_full(num, c, 2, 3, &[], num.center_hz(), &o),
                ] {
                    let p = power(&b);
                    let mut x = pad(&b, 0.1, 0.1);
                    x = impair(&x, prof, 900 + k as u64, Some(20.0), p, off, 0.0);
                    audio.extend(x);
                    audio.extend((0..2400).map(|_| 0.001 * ng.gaussian()));
                    sent += 1;
                }
            }
        }
        let (own, rx_own) = rx_num(num, &audio, 960);
        let mut rx = PhyRx::with_numerology(other(num), other(num).center_hz());
        let mut got = Vec::new();
        for c in audio.chunks(960) {
            rx.process(c, &mut got);
        }
        assert!(
            own.len() + 2 >= sent,
            "own receiver: {} ({:?})",
            own.len(),
            rx_own.stats()
        );
        assert!(got.is_empty());
        assert_eq!(rx.stats().syncs, 0);
    }
}

/// Stress version of `numerologies_do_not_cross_trigger`: 240 bursts of
/// each numerology over several channels, SNRs and offsets into the other
/// receiver; reports the worst whole-preamble correlation it saw.
#[test]
#[ignore]
fn cross_trigger_long() {
    for num in Numerology::ALL {
        let jobs: Vec<u64> = (0..240).collect();
        let res = par_map(&jobs, |&k| {
            let prof = [
                Profile::Awgn,
                Profile::CcirPoor,
                Profile::MidDisturbedNvis,
                Profile::CcirGood,
            ][(k % 4) as usize];
            let snr = [0.0f32, 10.0, 20.0, 30.0][(k / 4 % 4) as usize];
            let off = ((k * 37) % 101) as f32 - 50.0;
            let c = Constellation::ALL[(k % 3) as usize];
            let bits = if k % 5 == 0 {
                Vec::new()
            } else {
                random_bits(1000 + 37 * k as usize, k)
            };
            let b = modulate_full(num, c, k as u16, 0, &bits, num.center_hz(), &TxOptions::default());
            let p = power(&b);
            let a = impair(&pad(&b, 0.2, 0.2), prof, 4000 + k, Some(snr), p, off, 0.0);
            let mut rx = PhyRx::with_numerology(other(num), other(num).center_hz());
            rx.set_debug(true);
            let mut out = Vec::new();
            rx.process(&a, &mut out);
            let worst = rx.debug_rho().iter().fold(0f32, |m, x| m.max(*x));
            (out.len(), rx.stats().syncs, worst, rx.debug_rho().len())
        });
        let checks: usize = res.iter().map(|r| r.3).sum();
        let bursts: usize = res.iter().map(|r| r.0).sum();
        let syncs: u64 = res.iter().map(|r| r.1).sum();
        let worst = res.iter().map(|r| r.2).fold(0f32, f32::max);
        println!(
            "240 {num:?} bursts into the {:?} receiver: {checks} preamble checks, {syncs} syncs, {bursts} bursts, worst preamble correlation {worst:.3}",
            other(num)
        );
        assert_eq!(bursts, 0);
    }
}

/// The link's case: one stream, both receivers, Normal and Long bursts
/// interleaved back to back (no gaps), data and header-only.
#[test]
fn both_receivers_on_one_stream() {
    for wide in [false, true] {
        both_receivers_at(wide);
    }
}

fn both_receivers_at(wide: bool) {
    let nums = [Numerology::of(false, wide), Numerology::of(true, wide)];
    let center = nums[0].center_hz();
    let mut audio = vec![0.0f32; 4800];
    let mut want = Vec::new();
    for k in 0..8u16 {
        let num = nums[(k % 2) as usize];
        let c = Constellation::ALL[(k % 3) as usize];
        // Normal bursts alternate the pilot patterns. The default clip:
        // the link's harder 16QAM clip leaves a few uncoded bit errors at
        // 25 dB, and this test is about the numerologies, not the clip.
        let p = if k % 4 == 2 { Pilots::Sparse } else { Pilots::Robust };
        let o = TxOptions {
            pilots: p,
            ..TxOptions::default()
        };
        let bits = if k % 3 == 2 {
            Vec::new()
        } else {
            random_bits(1500 + 300 * k as usize, k as u64)
        };
        let extra = if bits.is_empty() { 0x100 + k } else { 0 };
        want.push((num, audio.len(), k, extra, bits.clone()));
        audio.extend(modulate_full(num, c, k, extra, &bits, center, &o));
    }
    audio.extend(vec![0.0; 9600]);
    let p = power(&audio);
    let audio = impair(&audio, Profile::Awgn, 5, Some(25.0), p, 0.0, 0.0);
    for num in nums {
        let (got, _) = rx_num(num, &audio, 480);
        let mine: Vec<_> = want.iter().filter(|w| w.0 == num).collect();
        assert_eq!(got.len(), mine.len(), "{num:?}");
        for (g, (_, start, user, extra, bits)) in got.iter().zip(mine) {
            assert_eq!(g.numerology, num);
            assert_eq!(g.user, *user);
            assert_eq!(g.extra, *extra);
            assert_eq!(g.header_only, bits.is_empty());
            assert!((g.start_sample as i64 - *start as i64 - 200).abs() <= 48);
            assert_eq!(bit_errors(&g.llrs, bits), 0);
        }
    }
}

#[test]
fn front_end_filter_response() {
    // The PHY's 48 kHz filter (TX interpolation and RX decimation):
    // lowpass_taps(481, 1600 Hz). Flat over the carriers plus a 50 Hz
    // offset (±1275 Hz); nothing aliases into ±1.3 kHz after decimation.
    let taps = ion::phy::front_end_taps();
    for f in (0..=1300).step_by(25) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g.abs() < 0.05, "{f} Hz: {g} dB");
    }
    for f in (1900..=24_000).step_by(25) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g < -70.0, "{f} Hz: {g} dB");
    }
}

#[test]
fn clean_round_trip_all_constellations() {
    for (k, c) in Constellation::ALL.into_iter().enumerate() {
        let bits = random_bits(3000 + 77 * k, 10 + k as u64);
        let o = TxOptions::for_burst(c, Pilots::Robust);
        let audio = pad(&modulate_with(c, 0xA5C3, &bits, CENTER, &o), 0.3, 0.3);
        let got = rx_all(&audio, 4800);
        assert_eq!(got.len(), 1, "{c:?}");
        let b = &got[0];
        assert_eq!(b.constellation, c);
        assert_eq!(b.user, 0xA5C3);
        assert_eq!(b.pilots, Pilots::Robust);
        assert_eq!(b.llrs.len(), capacity_bits(c, data_symbols(c, bits.len())));
        assert_eq!(bit_errors(&b.llrs, &bits), 0, "{c:?}");
        // Padding is zeros.
        assert!(b.llrs[bits.len()..].iter().all(|&l| l > 0.0));
    }
}

/// Every numerology, constellation and pilot pattern, clean: the header
/// names the pattern and the bits come back.
#[test]
fn clean_round_trip_numerologies_and_pilots() {
    for num in Numerology::ALL {
        for p in [Pilots::Robust, Pilots::Sparse] {
            for (k, c) in Constellation::ALL.into_iter().enumerate() {
                let bits = random_bits(4000 + 131 * k, 30 + k as u64);
                let o = TxOptions::for_burst(c, p);
                let burst = modulate_full(num, c, 0x5A00 + k as u16, 0, &bits, num.center_hz(), &o);
                assert_eq!(burst.len(), burst_samples_with(num, p, c, bits.len()));
                let (got, _) = rx_num(num, &pad(&burst, 0.3, 0.3), 4800);
                assert_eq!(got.len(), 1, "{num:?} {p:?} {c:?}");
                let b = &got[0];
                assert_eq!((b.numerology, b.constellation, b.pilots), (num, c, p));
                let ns = data_symbols_with(num, p, c, bits.len());
                assert_eq!(b.llrs.len(), capacity_bits_with(num, p, c, ns));
                assert_eq!(bit_errors(&b.llrs, &bits), 0, "{num:?} {p:?} {c:?}");
                assert!(b.llrs[bits.len()..].iter().all(|&l| l > 0.0));
                let err = b.start_sample as i64 - (0.3 * AUDIO_RATE) as i64;
                assert!(err.abs() <= 48, "{num:?} start error {err}");
            }
        }
        let i = info_for(num);
        let want = match num {
            Numerology::Normal => (44, 46, 2450.0),
            Numerology::Long => (88, 88, 2425.0),
            Numerology::NormalWide => (51, 52, 2750.0),
            Numerology::LongWide => (99, 99, 2725.0),
            Numerology::Slice => (10, 11, 650.0),
            Numerology::Slim => (22, 22, 666.667),
            Numerology::Half => (5, 5, 350.0),
            Numerology::Quarter => (3, 3, 250.0),
            Numerology::Sixth => (10, 10, 325.0),
        };
        assert_eq!((i.data_carriers, i.sparse_data_carriers), (want.0, want.1));
        assert!((i.bandwidth_hz - want.2).abs() < 1e-3);
    }
}

#[test]
fn wide_filter_response() {
    // The wide numerologies' filter (cutoff 1720 Hz): flat over ±1375 Hz of
    // carriers plus a 50 Hz offset, nothing aliasing back after decimation.
    let taps = ion::phy::front_end_taps_for(Numerology::NormalWide);
    for f in (0..=1440).step_by(20) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g.abs() < 0.05, "{f} Hz: {g} dB");
    }
    for f in (2050..=24_000).step_by(25) {
        let g = 20.0 * ion_dsp::dsp::fir_response(&taps, f as f32, AUDIO_RATE).log10();
        assert!(g < -70.0, "{f} Hz: {g} dB");
    }
}

#[test]
fn header_fields_round_trip() {
    for (c, user, nbits) in [
        (Constellation::Qpsk, 0u16, 1usize),
        (Constellation::Psk8, 0xFFFF, 132),
        (Constellation::Qam16, 0x1234, 176 * 100),
        (Constellation::Qpsk, 0x8001, 88 * 300),
    ] {
        let bits = random_bits(nbits, user as u64 + 3);
        let audio = pad(&modulate(c, user, &bits, CENTER), 0.2, 0.2);
        let got = rx_all(&audio, 1000);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].constellation, c);
        assert_eq!(got[0].user, user);
        assert_eq!(got[0].llrs.len(), capacity_bits(c, data_symbols(c, nbits)));
        assert_eq!(bit_errors(&got[0].llrs, &bits), 0);
    }
}

#[test]
fn start_sample_is_accurate() {
    let bits = random_bits(2000, 5);
    let burst = modulate(Constellation::Qpsk, 1, &bits, CENTER);
    for lead in [0.1f32, 0.37, 1.3] {
        let audio = pad(&burst, lead, 0.2);
        let got = rx_all(&audio, 333);
        let want = (lead * AUDIO_RATE) as i64;
        let err = got[0].start_sample as i64 - want;
        assert!(
            err.abs() <= 48,
            "lead {lead}: start {} want {want}",
            got[0].start_sample
        );
    }
}

#[test]
fn frequency_offsets_and_drift() {
    let c = Constellation::Qpsk;
    let bits = random_bits(capacity_bits(c, 40), 9);
    let burst = modulate(c, 77, &bits, CENTER);
    let p = power(&burst);
    let audio = pad(&burst, 0.3, 0.3);
    for (k, off) in [-50.0f32, -31.3, -7.0, 0.4, 12.5, 49.0, 50.0].into_iter().enumerate() {
        let a = impair(&audio, Profile::Awgn, 100 + k as u64, Some(12.0), p, off, 0.0);
        let got = rx_all(&a, 4800);
        assert_eq!(got.len(), 1, "offset {off}");
        let b = &got[0];
        let ber = bit_errors(&b.llrs, &bits) as f32 / bits.len() as f32;
        assert!(
            (b.freq_offset_hz - off).abs() < 0.5,
            "offset {off}: measured {}",
            b.freq_offset_hz
        );
        assert!(ber < 2e-3, "offset {off}: BER {ber}");
    }
    // Slow drift across a 4.8 s burst: 35 Hz drifting 0.5 Hz/s.
    let bits = random_bits(capacity_bits(c, 200), 11);
    let burst = modulate(c, 78, &bits, CENTER);
    let p = power(&burst);
    let a = impair(&pad(&burst, 0.3, 0.3), Profile::Awgn, 7, Some(12.0), p, 35.0, 0.5);
    let got = rx_all(&a, 4800);
    assert_eq!(got.len(), 1);
    let ber = bit_errors(&got[0].llrs, &bits) as f32 / bits.len() as f32;
    // The reported offset is the mean over the burst: 35 + 0.5 x (0.3 s of
    // lead-in + half of the 5.3 s burst) = 36.4 Hz.
    assert!((got[0].freq_offset_hz - 36.4).abs() < 0.5, "{}", got[0].freq_offset_hz);
    assert!(ber < 2e-3, "drift: BER {ber}");
}

#[test]
fn other_centre_frequency() {
    // Nothing assumes 1500 Hz: 1700 Hz centre (carriers 475-2925 Hz),
    // 20 Hz offset, AWGN 20 dB (64QAM: 28 dB), all constellations.
    for (k, c) in Constellation::ALL.into_iter().enumerate() {
        let bits = random_bits(capacity_bits(c, 30), 90 + k as u64);
        let o = TxOptions::for_burst(c, Pilots::Robust);
        let burst = modulate_with(c, 0x1700, &bits, 1700.0, &o);
        let p = power(&burst);
        let snr = if c == Constellation::Qam64 { 28.0 } else { 20.0 };
        let a = impair(
            &pad(&burst, 0.2, 0.3),
            Profile::Awgn,
            5 + k as u64,
            Some(snr),
            p,
            20.0,
            0.0,
        );
        let mut rx = PhyRx::new(1700.0);
        let mut out = Vec::new();
        rx.process(&a, &mut out);
        assert_eq!(out.len(), 1, "{c:?}");
        assert_eq!(out[0].user, 0x1700);
        assert!((out[0].freq_offset_hz - 20.0).abs() < 0.3);
        // 16QAM's link clip leaves an uncoded distortion floor of a few
        // 1e-3 at 20 dB.
        let ber = bit_errors(&out[0].llrs, &bits) as f32 / bits.len() as f32;
        let limit = if c == Constellation::Qam16 { 5e-3 } else { 2e-3 };
        assert!(ber < limit, "{c:?}: BER {ber}");
    }
}

#[test]
fn arbitrary_chunking_gives_identical_output() {
    let mut audio = Vec::new();
    for (k, c) in Constellation::ALL.into_iter().enumerate() {
        let bits = random_bits(1500, 20 + k as u64);
        audio.extend(pad(&modulate(c, k as u16, &bits, CENTER), 0.15, 0.0));
    }
    audio.extend(std::iter::repeat_n(0.0, 4800));
    let p = power(&audio);
    let audio = impair(&audio, Profile::CcirModerate, 3, Some(18.0), p, 3.0, 0.0);
    let reference = rx_all(&audio, audio.len());
    assert_eq!(reference.len(), Constellation::ALL.len());
    for chunk in [1usize, 7, 480, 4801] {
        let got = rx_all(&audio, chunk);
        assert_eq!(got.len(), reference.len(), "chunk {chunk}");
        for (a, b) in got.iter().zip(&reference) {
            assert_eq!(a.start_sample, b.start_sample);
            assert_eq!(a.user, b.user);
            assert_eq!(a.llrs, b.llrs, "chunk {chunk}");
            assert_eq!(a.snr_db, b.snr_db);
        }
    }
    // Random chunk sizes, including empty ones.
    let mut rx = PhyRx::new(CENTER);
    let mut out = Vec::new();
    let mut i = 0;
    let mut s = 12345u32;
    while i < audio.len() {
        s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let n = ((s >> 16) as usize % 3000).min(audio.len() - i);
        rx.process(&audio[i..i + n], &mut out);
        i += n;
    }
    assert_eq!(out.len(), reference.len());
    assert_eq!(out[2].llrs, reference[2].llrs);
}

#[test]
fn back_to_back_bursts() {
    // Gaps of silence, of noise, and none at all.
    let mut audio = Vec::new();
    let mut want = Vec::new();
    let mut ng = NoiseGen::new(4);
    let gaps = [0.2f32, 0.05, 0.0, 0.5, 0.01];
    for (k, &gap) in gaps.iter().enumerate() {
        let c = Constellation::ALL[k % 3];
        let bits = random_bits(1000 + 400 * k, 40 + k as u64);
        let n = (gap * AUDIO_RATE) as usize;
        if k % 2 == 0 {
            audio.extend(std::iter::repeat_n(0.0, n));
        } else {
            audio.extend((0..n).map(|_| 0.01 * ng.gaussian()));
        }
        want.push((audio.len(), c, k as u16, bits.clone()));
        audio.extend(modulate(c, k as u16, &bits, CENTER));
    }
    audio.extend(std::iter::repeat_n(0.0, 9600));
    let got = rx_all(&audio, 960);
    assert_eq!(got.len(), gaps.len());
    for (g, (start, c, user, bits)) in got.iter().zip(&want) {
        assert_eq!(g.constellation, *c);
        assert_eq!(g.user, *user);
        assert!((g.start_sample as i64 - *start as i64).abs() <= 48);
        assert_eq!(bit_errors(&g.llrs, bits), 0);
    }
    // The whole stream (much longer than the 2 s ring buffer) in one call.
    assert!(audio.len() > 3 * AUDIO_RATE as usize);
    let one = rx_all(&audio, audio.len());
    assert_eq!(one.len(), got.len());
    for (a, b) in one.iter().zip(&got) {
        assert_eq!(a.llrs, b.llrs);
    }
}

#[test]
fn busy_during_a_burst_only() {
    let bits = random_bits(88 * 40, 8);
    let burst = modulate(Constellation::Qpsk, 1, &bits, CENTER);
    let lead = 24000;
    let audio = pad(&burst, 0.5, 0.5);
    let mut rx = PhyRx::new(CENTER);
    let mut out = Vec::new();
    let mut busy_at = Vec::new();
    for (k, c) in audio.chunks(480).enumerate() {
        rx.process(c, &mut out);
        busy_at.push((k * 480 + c.len(), rx.busy(), out.len()));
    }
    // Idle in the lead-in silence and after the burst is delivered.
    assert!(busy_at.iter().filter(|b| b.0 < lead).all(|b| !b.1));
    assert!(!busy_at.last().unwrap().1);
    // Busy through most of the burst: from shortly after the preamble
    // until the burst is delivered.
    let mid = lead + burst.len() / 2;
    assert!(busy_at.iter().any(|b| b.0 > mid && b.0 < mid + 960 && b.1));
    let done = busy_at.iter().find(|b| b.2 == 1).unwrap().0;
    assert!(done >= lead + burst.len() - 4800 && done <= lead + burst.len() + 9600);
}

#[test]
fn noise_only_gives_no_bursts() {
    for num in Numerology::ALL {
        let mut rx = PhyRx::with_numerology(num, num.center_hz());
        let mut out = Vec::new();
        let mut ng = NoiseGen::new(99);
        let mut buf = vec![0f32; 480];
        let mut busy = 0;
        for _ in 0..6000 {
            for v in buf.iter_mut() {
                *v = 0.1 * ng.gaussian();
            }
            rx.process(&buf, &mut out);
            busy += rx.busy() as usize;
        }
        assert!(out.is_empty());
        // A half-duplex station must not see the channel as busy on noise.
        assert!(busy <= 30, "busy in {busy} of 6000 10 ms chunks");
        // Silence and a hard step must not trigger either.
        let mut rx = PhyRx::with_numerology(num, num.center_hz());
        let mut a = vec![0f32; 48000];
        a.extend(std::iter::repeat_n(0.3, 48000));
        a.extend(std::iter::repeat_n(0.0, 48000));
        rx.process(&a, &mut out);
        assert!(out.is_empty());
    }
}

#[test]
fn ccir_poor_moderate_snr() {
    let c = Constellation::Qpsk;
    let mut decoded = 0;
    let mut errs = 0usize;
    let mut total = 0usize;
    let mut llr = Vec::new();
    let mut ref_bits = Vec::new();
    let runs = 10;
    for seed in 0..runs {
        let bits = random_bits(capacity_bits(c, 42), 60 + seed);
        let burst = modulate(c, 0x55AA, &bits, CENTER);
        let p = power(&burst);
        let a = impair(
            &pad(&burst, 0.2, 0.3),
            Profile::CcirPoor,
            500 + seed,
            Some(12.0),
            p,
            0.0,
            0.0,
        );
        let got = rx_all(&a, 4800);
        if let Some(b) = got.first() {
            assert_eq!(b.user, 0x55AA);
            decoded += 1;
            errs += bit_errors(&b.llrs, &bits);
            total += bits.len();
            assert!(b.llrs.iter().all(|l| l.is_finite()));
            assert!((b.snr_db - 12.0).abs() < 6.0, "snr {}", b.snr_db);
            llr.extend_from_slice(&b.llrs[..bits.len()]);
            ref_bits.extend_from_slice(&bits);
        }
    }
    let ber = errs as f32 / total.max(1) as f32;
    assert!(decoded >= runs as usize - 1);
    assert!(ber < 0.05);
    check_calibration(&llr, &ref_bits, "CCIR poor 12 dB QPSK");
}

/// Bins LLR magnitudes and compares the measured bit error rate with the
/// one the LLRs claim, 1/(1+e^|L|): (lo, hi, measured, predicted, count).
fn calibration(llr: &[f32], bits: &[u8]) -> Vec<(f32, f32, f64, f64, usize)> {
    let edges = [0.0f32, 0.5, 1.0, 2.0, 3.0, 4.5, 6.0, 9.0, 1e9];
    let mut rows = Vec::new();
    for w in edges.windows(2) {
        let (mut n, mut e, mut pred) = (0usize, 0usize, 0f64);
        for (&l, &b) in llr.iter().zip(bits) {
            let m = l.abs();
            if m >= w[0] && m < w[1] {
                n += 1;
                e += ((l < 0.0) as u8 != b) as usize;
                pred += 1.0 / (1.0 + (m as f64).exp());
            }
        }
        if n > 0 {
            rows.push((w[0], w[1], e as f64 / n as f64, pred / n as f64, n));
        }
    }
    rows
}

fn check_calibration(llr: &[f32], bits: &[u8], what: &str) {
    for (lo, hi, meas, pred, n) in calibration(llr, bits) {
        // Only bins with enough expected errors are judged.
        if pred * n as f64 >= 30.0 {
            let ratio = meas / pred;
            assert!(
                (0.6..1.6).contains(&ratio),
                "{what}: |L| in {lo}..{hi}: measured {meas:.4} vs predicted {pred:.4}"
            );
        }
    }
}

#[test]
fn llrs_are_calibrated_on_awgn() {
    for (c, snr) in [
        (Constellation::Qpsk, 3.0f32),
        (Constellation::Psk8, 9.0),
        (Constellation::Qam16, 13.0),
    ] {
        let mut llr = Vec::new();
        let mut ref_bits = Vec::new();
        for seed in 0..6 {
            let bits = random_bits(capacity_bits(c, 42), 70 + seed);
            let burst = modulate(c, 1, &bits, CENTER);
            let p = power(&burst);
            let a = impair(
                &pad(&burst, 0.2, 0.3),
                Profile::Awgn,
                700 + seed,
                Some(snr),
                p,
                0.0,
                0.0,
            );
            let mut rx = PhyRx::new(CENTER);
            rx.set_genie_header(Some(genie(c, bits.len(), 1, 0.2)));
            let mut out = Vec::new();
            rx.process(&a, &mut out);
            assert_eq!(out.len(), 1);
            llr.extend_from_slice(&out[0].llrs[..bits.len()]);
            ref_bits.extend_from_slice(&bits);
        }
        check_calibration(&llr, &ref_bits, &format!("AWGN {snr} dB {c:?}"));
    }
}

/// Uncoded QPSK BER of both numerologies on two-path channels (delay ms,
/// spread Hz) at 20 dB, plus the channel model each burst picked.
#[test]
#[ignore]
fn diag_long() {
    use ion_dsp::channel::ChannelSpec;
    let c = Constellation::Qpsk;
    let snr: f32 = std::env::var("DIAG_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    for (d, s) in [
        (0.0f32, 0.0f32),
        (2.0, 1.0),
        (7.0, 0.0),
        (7.0, 0.3),
        (7.0, 1.0),
        (7.0, 2.0),
        (4.0, 1.0),
    ] {
        for num in Numerology::ALL {
            let (mut e, mut t, mut models) = (0, 0, Vec::new());
            for seed in 0..8u64 {
                let bits = random_bits(capacity_bits_for(num, c, (1.0 / info_for(num).symbol_s) as usize), seed);
                let burst = modulate_full(num, c, 1, 0, &bits, num.center_hz(), &TxOptions::default());
                let p = power(&burst);
                let spec = ChannelSpec::two_path("x", d, s);
                let mut ch = Channel::new(&spec, 50 + seed);
                let x = pad(&burst, 0.2, 0.3);
                let mut a = vec![0.0; x.len()];
                ch.process(&x, &mut a);
                let sigma = noise_sigma(p, snr, 2400.0, AUDIO_RATE);
                add_awgn(&mut a, sigma, &mut NoiseGen::new(seed ^ 0xABCD));
                let mut rx = PhyRx::with_numerology(num, num.center_hz());
                rx.set_genie_header(Some(Genie {
                    constellation: c,
                    data_symbols: data_symbols_for(num, c, bits.len()),
                    user: 1,
                    start: (0.2 * AUDIO_RATE) as u64 + 200,
                    pilots: Pilots::Robust,
                }));
                let mut out = Vec::new();
                rx.process(&a, &mut out);
                if let Some(b) = out.first() {
                    e += bit_errors(&b.llrs, &bits);
                    t += bits.len();
                    let m = rx.debug_model();
                    models.push((m.0, m.1, m.2, bit_errors(&b.llrs, &bits)));
                }
            }
            eprintln!(
                "{d} ms {s} Hz {num:?}: BER {:.2e} ({t} bits), models {:?}",
                e as f64 / t.max(1) as f64,
                models
                    .iter()
                    .map(|m| (m.0, m.1, (m.2 * 10.0).round() / 10.0, m.3))
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// Codeword success of Long on a 7 ms two-path channel with a 2 Hz spread
/// (twice the ITU NVIS profile's), 20 bursts per point.
#[test]
#[ignore]
fn diag_long_2hz() {
    use ion::fec::{self, Rate};
    use ion_dsp::channel::ChannelSpec;
    let spec = ChannelSpec::two_path("7 ms, 2 Hz", 7.0, 2.0);
    for rate in [Rate::R1_4, Rate::R1_3, Rate::R1_2] {
        let mut line = String::new();
        for snr in [4.0f32, 8.0, 12.0, 16.0, 20.0] {
            let jobs: Vec<u64> = (0..20).collect();
            let res = par_map(&jobs, |&r| {
                let seed = 7000 + 100 * r + snr as u64;
                let info_bits = random_bits(fec::K, seed);
                let coded = fec::encode(rate, &info_bits);
                let num = Numerology::Long;
                let b = modulate_full(num, Constellation::Qpsk, 3, 0, &coded, CENTER, &TxOptions::default());
                let p = power(&b);
                let x = pad(&b, 0.25, 0.3);
                let mut a = vec![0.0; x.len()];
                Channel::new(&spec, seed).process(&x, &mut a);
                add_awgn(
                    &mut a,
                    noise_sigma(p, snr, 2400.0, AUDIO_RATE),
                    &mut NoiseGen::new(seed ^ 0xABCD),
                );
                let (got, _) = rx_num(num, &a, 4800);
                got.first()
                    .is_some_and(|g| fec::decode(rate, &g.llrs[..rate.n()]).is_some_and(|d| d == info_bits))
            });
            line += &format!(" {snr:+}: {:.2}", res.iter().filter(|&&b| b).count() as f32 / 20.0);
        }
        eprintln!("Long, 7 ms / 2 Hz, {rate:?}:{line}");
    }
}

/// Breaks the cell error down into noise and channel-estimation error on
/// AWGN (where the true channel is constant per carrier).
#[test]
#[ignore]
fn diag_cells() {
    use ion::phy::debug_tx_cells;
    let c = Constellation::Qpsk;
    for snr in [0.0f32, 3.0, 10.0, 20.0] {
        let bits = random_bits(capacity_bits(c, 42), 5);
        let burst = modulate(c, 1, &bits, CENTER);
        let p = power(&burst);
        let a = impair(&pad(&burst, 0.2, 0.3), Profile::Awgn, 9, Some(snr), p, 0.0, 0.0);
        let mut rx = PhyRx::new(CENTER);
        rx.set_genie_header(Some(genie(c, bits.len(), 1, 0.2)));
        rx.set_debug(true);
        let mut out = Vec::new();
        rx.process(&a, &mut out);
        let x = debug_tx_cells(c, &bits);
        let cells = rx.debug_cells();
        // True channel per carrier: average of y·x*.
        let mut ht = [Complex::ZERO; 49];
        let mut n = [0f32; 49];
        for (d, &xv) in cells.iter().zip(&x) {
            ht[d.c] += d.y * xv.conj();
            n[d.c] += 1.0;
        }
        for k in 0..49 {
            if n[k] > 0.0 {
                ht[k] = ht[k].scale(1.0 / n[k]);
            }
        }
        let (mut tot, mut noise, mut est, mut pred_n, mut pred_e, mut hp) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut by_g = vec![(0f64, 0f64); 60];
        for (d, &xv) in cells.iter().zip(&x) {
            let t = (d.y - d.h * xv).norm_sqr() as f64;
            tot += t;
            noise += (d.y - ht[d.c] * xv).norm_sqr() as f64;
            est += (d.h - ht[d.c]).norm_sqr() as f64;
            pred_n += d.sigma2 as f64;
            pred_e += d.mse as f64;
            hp += ht[d.c].norm_sqr() as f64;
            by_g[d.g].0 += (d.h - ht[d.c]).norm_sqr() as f64;
            by_g[d.g].1 += d.mse as f64;
        }
        let m = cells.len() as f64;
        eprintln!("  model {:?}", rx.debug_model());
        let ber = bit_errors(&out[0].llrs, &bits) as f64 / bits.len() as f64;
        let implied: f64 = out[0].llrs[..bits.len()]
            .iter()
            .map(|l| 1.0 / (1.0 + (l.abs() as f64).exp()))
            .sum::<f64>()
            / bits.len() as f64;
        // Hard decisions straight from the cells.
        let mut cell_err = 0usize;
        for (d, &xv) in cells.iter().zip(&x) {
            let z = d.y * d.h.conj();
            if (z.re < 0.0) != (xv.re < 0.0) {
                cell_err += 1;
            }
            if (z.im < 0.0) != (xv.im < 0.0) {
                cell_err += 1;
            }
        }
        eprintln!(
            "  BER {ber:.4}, LLR-implied {implied:.4}, cell-level BER {:.4}",
            cell_err as f64 / (2.0 * m)
        );
        eprintln!(
            "AWGN {snr} dB: |H|² {:.3e}; total err {:.3e}; noise {:.3e} (pred {:.3e}); est err {:.3e} (pred {:.3e}); snr_db {:.2}",
            hp / m,
            tot / m,
            noise / m,
            pred_n / m,
            est / m,
            pred_e / m,
            out[0].snr_db
        );
        let line: Vec<String> = by_g[5..47]
            .iter()
            .map(|(a, b)| format!("{:.2}", a / b.max(1e-30)))
            .collect();
        eprintln!("  est err / pred by symbol: {}", line.join(" "));
    }
}

/// A sound card running `ppm` slow against the sender's: the burst is
/// stretched by 1 + ppm·10⁻⁶ (16-tap windowed-sinc interpolation; the
/// signal sits far below Nyquist, so its error is under −80 dB).
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

/// One burst of `nsym` data symbols, resampled by `ppm`, through `profile`
/// at `snr` dB: (uncoded BER, tracked ppm), or None if it was not found.
#[allow(clippy::too_many_arguments)]
fn drift_run(
    num: Numerology,
    c: Constellation,
    nsym: usize,
    ppm: f64,
    profile: Profile,
    snr: f32,
    seed: u64,
    track: bool,
) -> Option<(f64, f32)> {
    let bits = random_bits(capacity_bits_for(num, c, nsym), seed);
    let burst = modulate_full(num, c, 9, 0, &bits, num.center_hz(), &TxOptions::default());
    let x = resample(&pad(&burst, 0.2, 0.3), ppm);
    let p = power(&burst);
    let a = impair(&x, profile, seed ^ 0x77, Some(snr), p, 0.0, 0.0);
    let mut rx = PhyRx::with_numerology(num, num.center_hz());
    rx.set_drift_tracking(track);
    let mut out = Vec::new();
    rx.process(&a, &mut out);
    let b = out.iter().find(|b| b.user == 9)?;
    let ber = bit_errors(&b.llrs, &bits) as f64 / bits.len() as f64;
    Some((ber, b.clock_ppm))
}

/// Long bursts survive ±100 ppm of sample-clock offset: the longest
/// Normal burst (1023 symbols, 24.6 s) drifts 2.5 ms, the CP's margin is
/// ~0.5 ms.
#[test]
fn sample_clock_drift_100ppm() {
    let jobs: Vec<(Numerology, f64)> = [Numerology::Normal, Numerology::NormalWide, Numerology::LongWide]
        .into_iter()
        .flat_map(|n| [-100.0, 100.0].map(move |p| (n, p)))
        .collect();
    let res = par_map(&jobs, |&(num, ppm)| {
        let nsym = if num.is_long() { 400 } else { 1023 };
        drift_run(num, Constellation::Qpsk, nsym, ppm, Profile::Awgn, 15.0, 5, true)
    });
    for (&(num, ppm), r) in jobs.iter().zip(&res) {
        let (ber, est) = r.expect("burst found");
        assert!(ber < 2e-3, "{num:?} {ppm} ppm: BER {ber}");
        assert!((est as f64 - ppm).abs() < 20.0, "{num:?} {ppm} ppm: tracked {est}");
    }
    // No offset: the estimate stays near zero and changes nothing.
    let (ber, est) = drift_run(
        Numerology::Normal,
        Constellation::Qam16,
        300,
        0.0,
        Profile::Awgn,
        20.0,
        6,
        true,
    )
    .unwrap();
    assert!(est.abs() < 10.0, "tracked {est} ppm on a clean clock");
    assert!(ber < 2e-3, "BER {ber}");
}

/// Uncoded BER with and without drift tracking against clock offset, on
/// AWGN and CCIR poor (the table in `docs/phy.md`).
#[test]
#[ignore]
fn diag_clock_drift() {
    let ppms = [0.0f64, 30.0, -60.0, 100.0, -100.0, 200.0];
    for (profile, snr) in [(Profile::Awgn, 15.0f32), (Profile::CcirPoor, 20.0)] {
        for num in [Numerology::NormalWide, Numerology::LongWide] {
            let nsym = if num.is_long() { 400 } else { 1023 };
            let jobs: Vec<(f64, bool, u64)> = ppms
                .iter()
                .flat_map(|&p| [true, false].map(move |t| (p, t)))
                .flat_map(|(p, t)| (0..4u64).map(move |s| (p, t, s)))
                .collect();
            let res = par_map(&jobs, |&(p, t, s)| {
                drift_run(num, Constellation::Qpsk, nsym, p, profile, snr, 40 + s, t)
            });
            for &ppm in &ppms {
                for track in [true, false] {
                    let rs: Vec<_> = jobs
                        .iter()
                        .zip(&res)
                        .filter(|(j, _)| j.0 == ppm && j.1 == track)
                        .filter_map(|(_, r)| *r)
                        .collect();
                    let ber = rs.iter().map(|r| r.0).sum::<f64>() / rs.len().max(1) as f64;
                    let est: Vec<String> = rs.iter().map(|r| format!("{:+.0}", r.1)).collect();
                    println!(
                        "| {} | {num:?} | {ppm:+} | {} | {}/4 | {ber:.1e} | {} |",
                        profile_label(profile),
                        if track { "on" } else { "off" },
                        rs.len(),
                        est.join(" ")
                    );
                }
            }
        }
    }
}

#[test]
fn snr_estimate_tracks_awgn() {
    // Unclipped: the estimate tracks the thermal SNR.
    let c = Constellation::Qpsk;
    let bits = random_bits(capacity_bits(c, 42), 3);
    let burst = modulate_with(
        c,
        1,
        &bits,
        CENTER,
        &TxOptions {
            clip_db: None,
            ..TxOptions::default()
        },
    );
    let p = power(&burst);
    let audio = pad(&burst, 0.2, 0.3);
    for snr in [0.0f32, 6.0, 12.0, 18.0, 24.0] {
        let a = impair(&audio, Profile::Awgn, 11, Some(snr), p, 0.0, 0.0);
        let got = rx_all(&a, 4800);
        assert_eq!(got.len(), 1);
        assert!(
            (got[0].snr_db - snr).abs() < 1.0,
            "applied {snr}, estimated {}",
            got[0].snr_db
        );
    }
    // Clipped bursts: the receiver takes the sender's known clipping
    // distortion out of the report, so it follows the channel up to the
    // estimator's own floor (about 34 dB).
    let burst = modulate(c, 1, &bits, CENTER);
    let p = power(&burst);
    let audio = pad(&burst, 0.2, 0.3);
    for snr in [6.0f32, 12.0, 24.0, 30.0] {
        let a = impair(&audio, Profile::Awgn, 11, Some(snr), p, 0.0, 0.0);
        let got = rx_all(&a, 4800);
        let floor = 34.0f32;
        let eff = -10.0 * (10f32.powf(-snr / 10.0) + 10f32.powf(-floor / 10.0)).log10();
        assert!((got[0].snr_db - eff).abs() < 1.5, "clipped {snr}: {}", got[0].snr_db);
    }
    // The link's harder clips are taken out the same way.
    for c in [Constellation::Qpsk, Constellation::Psk8, Constellation::Qam16] {
        let bits = random_bits(capacity_bits(c, 42), 4);
        let burst = modulate_with(c, 1, &bits, CENTER, &TxOptions::for_burst(c, Pilots::Robust));
        let p = power(&burst);
        let audio = pad(&burst, 0.2, 0.3);
        for snr in [12.0f32, 24.0] {
            let a = impair(&audio, Profile::Awgn, 11, Some(snr), p, 0.0, 0.0);
            let got = rx_all(&a, 4800);
            let eff = -10.0 * (10f32.powf(-snr / 10.0) + 10f32.powf(-34.0 / 10.0)).log10();
            assert!(
                (got[0].snr_db - eff).abs() < 2.0,
                "{c:?} link clip {snr}: {}",
                got[0].snr_db
            );
        }
    }
}

/// In-band clip distortion per clip level: clean bursts (60 dB AWGN)
/// clipped at each level, the reported SNR, and the distortion it implies
/// with 26 dB taken out (none for 64QAM) and a ~34 dB estimation floor.
#[test]
#[ignore]
fn diag_clip_distortion() {
    for c in [
        Constellation::Qpsk,
        Constellation::Psk8,
        Constellation::Qam16,
        Constellation::Qam64,
    ] {
        let assumed = if c == Constellation::Qam64 {
            0.0
        } else {
            10f32.powf(-2.6)
        };
        for clip in [None, Some(8.0f32), Some(6.0), Some(5.0), Some(4.5), Some(4.0)] {
            let mut rs = Vec::new();
            for seed in 0..4u64 {
                let bits = random_bits(capacity_bits(c, 42), 3 + seed);
                let opt = TxOptions {
                    clip_db: clip,
                    ..TxOptions::default()
                };
                let burst = modulate_with(c, 1, &bits, CENTER, &opt);
                let p = power(&burst);
                let a = impair(
                    &pad(&burst, 0.2, 0.3),
                    Profile::Awgn,
                    11 + seed,
                    Some(60.0),
                    p,
                    0.0,
                    0.0,
                );
                if let Some(b) = rx_all(&a, 4800).first() {
                    rs.push(b.snr_db);
                }
            }
            let r = rs.iter().sum::<f32>() / rs.len().max(1) as f32;
            let d = -10.0 * (10f32.powf(-r / 10.0) + assumed - 10f32.powf(-3.4)).max(1e-6).log10();
            println!("{c:?} clip {clip:?}: reported {r:.1} dB, distortion ~{d:.1} dB below the signal");
        }
    }
}

/// Reported SNR against the channel's, for bursts at the link's clips
/// (`TxOptions::for_burst`), 4 bursts per point on AWGN.
#[test]
#[ignore]
fn diag_link_clip_snr() {
    for c in Constellation::ALL {
        let mut line = format!("{c:?}:");
        for snr in (12..=36).step_by(3) {
            let mut rs = Vec::new();
            for seed in 0..4u64 {
                let bits = random_bits(capacity_bits(c, 42), 50 + seed);
                let burst = modulate_with(c, 1, &bits, CENTER, &TxOptions::for_burst(c, Pilots::Robust));
                let p = power(&burst);
                let a = impair(
                    &pad(&burst, 0.2, 0.3),
                    Profile::Awgn,
                    21 + seed,
                    Some(snr as f32),
                    p,
                    0.0,
                    0.0,
                );
                if let Some(b) = rx_all(&a, 4800).first() {
                    rs.push(b.snr_db);
                }
            }
            line += &format!(" {snr}:{:.1}", rs.iter().sum::<f32>() / rs.len().max(1) as f32);
        }
        println!("{line}");
    }
}

/// Whole-preamble correlation of noise-only candidates against real
/// bursts at low SNR (for `Layout::rho_full_min`).
#[test]
#[ignore]
fn diag_rho() {
    for num in Numerology::ALL {
        let mut rx = PhyRx::with_numerology(num, num.center_hz());
        rx.set_debug(true);
        let mut out = Vec::new();
        let mut ng = NoiseGen::new(1234);
        let mut buf = vec![0f32; 4800];
        for _ in 0..1200 {
            for v in buf.iter_mut() {
                *v = 0.1 * ng.gaussian();
            }
            rx.process(&buf, &mut out);
        }
        let mut r = rx.debug_rho().to_vec();
        r.sort_by(|a, b| b.total_cmp(a));
        eprintln!(
            "{num:?} noise 120 s: {:?}, top rho {:?}",
            rx.stats(),
            &r[..r.len().min(12)]
        );
        let c = Constellation::Qpsk;
        for profile in [Profile::Awgn, Profile::CcirPoor, Profile::MidDisturbedNvis] {
            for snr in [-8.0f32, -6.0, -4.0, -2.0, 0.0] {
                let mut v = Vec::new();
                for seed in 0..40 {
                    let bits = random_bits(88 * 4, seed);
                    let burst = modulate_full(num, c, 1, 0, &bits, num.center_hz(), &TxOptions::default());
                    let p = power(&burst);
                    let a = impair(&pad(&burst, 0.25, 0.25), profile, 900 + seed, Some(snr), p, 0.0, 0.0);
                    let mut rx = PhyRx::with_numerology(num, num.center_hz());
                    rx.set_debug(true);
                    let mut out = Vec::new();
                    rx.process(&a, &mut out);
                    v.push(rx.debug_rho().iter().fold(0f32, |m, x| m.max(*x)));
                }
                v.sort_by(|a, b| a.total_cmp(b));
                eprintln!(
                    "{num:?} {profile:?} {snr} dB: rho min {:.3}, 10% {:.3}, median {:.3}",
                    v[0], v[4], v[20]
                );
            }
        }
    }
}

/// False-alarm statistics over 10 minutes of white noise (20 streams of
/// 30 s in parallel).
#[test]
#[ignore]
fn noise_long() {
    for num in Numerology::ALL {
        let jobs: Vec<u64> = (0..20).collect();
        let res = par_map(&jobs, |&k| {
            let mut rx = PhyRx::with_numerology(num, num.center_hz());
            rx.set_debug(true);
            let mut out = Vec::new();
            let mut ng = NoiseGen::new(1000 + k);
            let mut buf = vec![0f32; 4800];
            for _ in 0..300 {
                for v in buf.iter_mut() {
                    *v = 0.05 * ng.gaussian();
                }
                rx.process(&buf, &mut out);
            }
            let worst = rx.debug_rho().iter().fold(0f32, |m, x| m.max(*x));
            (rx.stats(), out.len(), worst)
        });
        let cand: u64 = res.iter().map(|r| r.0.candidates).sum();
        let syncs: u64 = res.iter().map(|r| r.0.syncs).sum();
        let bursts: usize = res.iter().map(|r| r.1).sum();
        let worst = res.iter().map(|r| r.2).fold(0f32, f32::max);
        println!(
            "{num:?}, 600 s of noise: {cand} detector triggers, {syncs} syncs (header attempts), {bursts} bursts, worst preamble correlation {worst:.3}"
        );
        assert_eq!(bursts, 0);
    }
}

/// Where header failures come from: detection, sync or decoding.
#[test]
#[ignore]
fn diag_header() {
    for profile in [Profile::Awgn, Profile::CcirPoor] {
        for snr in [-4.0f32, -2.0, 0.0, 2.0] {
            let jobs: Vec<u64> = (0..100).collect();
            let res = par_map(&jobs, |&r| {
                let bits = random_bits(88 * 4, r + 5);
                let burst = modulate(Constellation::Qpsk, 9, &bits, CENTER);
                let p = power(&burst);
                let a = impair(&pad(&burst, 0.25, 0.3), profile, 77 + 1000 * r, Some(snr), p, 0.0, 0.0);
                let mut rx = PhyRx::new(CENTER);
                let mut out = Vec::new();
                rx.process(&a, &mut out);
                (rx.stats(), out.len())
            });
            let cand = res.iter().filter(|r| r.0.candidates > 0).count();
            let sync = res.iter().filter(|r| r.0.syncs > 0).count();
            let ok = res.iter().filter(|r| r.1 == 1).count();
            eprintln!("{profile:?} {snr:+} dB: candidate {cand}, sync {sync}, header ok {ok} / 100");
        }
    }
}

// ---------------------------------------------------------------------------
// Results tables for docs/phy.md
// ---------------------------------------------------------------------------

const PROFILES: [Profile; 4] = [
    Profile::Awgn,
    Profile::CcirGood,
    Profile::CcirModerate,
    Profile::CcirPoor,
];

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

struct RunResult {
    /// Burst found (genie header accepted it).
    found: bool,
    errors: usize,
    bits: usize,
    snr_est: f32,
}

/// One 1 s burst (42 data symbols) through `profile` at `snr`, genie header.
fn ber_run(c: Constellation, profile: Profile, snr: f32, seed: u64, opt: &TxOptions) -> RunResult {
    let bits = random_bits(capacity_bits(c, 42), seed);
    let burst = modulate_with(c, 1, &bits, CENTER, opt);
    let p = power(&burst);
    let a = impair(&pad(&burst, 0.25, 0.3), profile, seed, Some(snr), p, 0.0, 0.0);
    let mut rx = PhyRx::new(CENTER);
    rx.set_genie_header(Some(genie(c, bits.len(), 1, 0.25)));
    let mut out = Vec::new();
    rx.process(&a, &mut out);
    match out.first() {
        Some(b) => RunResult {
            found: true,
            errors: bit_errors(&b.llrs, &bits),
            bits: bits.len(),
            snr_est: b.snr_db,
        },
        None => RunResult {
            found: false,
            errors: 0,
            bits: 0,
            snr_est: 0.0,
        },
    }
}

fn fmt_ber(errors: usize, bits: usize, missed: usize) -> String {
    let mut s = if bits == 0 {
        "-".to_string()
    } else if errors == 0 {
        format!("<{:.0e}", 1.0 / bits as f64)
    } else {
        format!("{:.1e}", errors as f64 / bits as f64)
    };
    if missed > 0 {
        s.push_str(&format!(" ({missed}m)"));
    }
    s
}

fn header_row(snrs: &[i32]) {
    print!("| Channel |");
    for s in snrs {
        print!(" {s:+} |");
    }
    println!("\n|---|{}", "---:|".repeat(snrs.len()));
}

#[test]
#[ignore]
fn tables() {
    let runs: u64 = std::env::var("PHY_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let only: String = std::env::var("PHY_TABLES").unwrap_or_else(|_| "bshpcrl".into());
    let snrs: Vec<i32> = (-4..=24).step_by(2).collect();

    // Uncoded BER.
    let mut snr_rows = Vec::new();
    if only.contains('b') {
        println!("### Uncoded BER, 1 s bursts (42 data symbols), {runs} bursts per point, genie header\n");
        println!("`(Nm)`: bursts not found by the detector (excluded from the BER).\n");
        for c in Constellation::ALL {
            println!("#### {c:?}\n");
            header_row(&snrs);
            for profile in PROFILES {
                let jobs: Vec<(i32, u64)> = snrs.iter().flat_map(|&s| (0..runs).map(move |r| (s, r))).collect();
                let res = par_map(&jobs, |&(s, r)| {
                    ber_run(c, profile, s as f32, 1000 * r + 17, &TxOptions::default())
                });
                print!("| {} |", profile.spec().name);
                let mut est_row = Vec::new();
                for i in 0..snrs.len() {
                    let rs = &res[i * runs as usize..(i + 1) * runs as usize];
                    let e: usize = rs.iter().map(|r| r.errors).sum();
                    let b: usize = rs.iter().map(|r| r.bits).sum();
                    let m = rs.iter().filter(|r| !r.found).count();
                    print!(" {} |", fmt_ber(e, b, m));
                    let found: Vec<f32> = rs.iter().filter(|r| r.found).map(|r| r.snr_est).collect();
                    est_row.push(found.iter().sum::<f32>() / found.len().max(1) as f32);
                }
                println!();
                if c == Constellation::Qpsk {
                    snr_rows.push((profile, est_row));
                }
            }
            println!();
        }
    }

    // SNR estimate.
    if only.contains('s') && !snr_rows.is_empty() {
        println!("### Mean reported `snr_db` (the QPSK bursts above)\n");
        header_row(&snrs);
        for (profile, row) in &snr_rows {
            print!("| {} |", profile.spec().name);
            for v in row {
                print!(" {v:.1} |");
            }
            println!();
        }
        println!();
    }

    // Header decode probability (no genie), 1 s QPSK bursts.
    if only.contains('h') {
        let hsnrs: Vec<i32> = (-10..=8).step_by(2).collect();
        let hruns = 4 * runs;
        println!("### Header decode probability, 1 s QPSK bursts, {hruns} bursts per point (no genie)\n");
        header_row(&hsnrs);
        for profile in PROFILES {
            let jobs: Vec<(i32, u64)> = hsnrs.iter().flat_map(|&s| (0..hruns).map(move |r| (s, r))).collect();
            let res = par_map(&jobs, |&(s, r)| {
                let user = (r * 7919 % 65536) as u16;
                let bits = random_bits(capacity_bits(Constellation::Qpsk, 42), r + 5);
                let burst = modulate(Constellation::Qpsk, user, &bits, CENTER);
                let p = power(&burst);
                let a = impair(
                    &pad(&burst, 0.25, 0.3),
                    profile,
                    77 + 1000 * r,
                    Some(s as f32),
                    p,
                    0.0,
                    0.0,
                );
                let got = rx_all(&a, 4800);
                got.len() == 1 && got[0].user == user && got[0].constellation == Constellation::Qpsk
            });
            print!("| {} |", profile.spec().name);
            for i in 0..hsnrs.len() {
                let ok = res[i * hruns as usize..(i + 1) * hruns as usize]
                    .iter()
                    .filter(|&&b| b)
                    .count();
                print!(" {:.2} |", ok as f64 / hruns as f64);
            }
            println!();
        }
        println!();
    }

    // PAPR.
    if only.contains('p') {
        println!("### PAPR\n");
        println!(
            "Envelope = |complex baseband| (what the SSB PA sees); audio crest = peak/RMS of the 48 kHz audio. Mean power is relative to the fixed peak `TX_LEVEL` (higher is better).\n"
        );
        println!(
            "| Constellation | Clip | Envelope PAPR (mean of burst peaks) | 99.9% CCDF | Audio crest | Mean power vs peak |"
        );
        println!("|---|---|---:|---:|---:|---:|");
        for c in Constellation::ALL {
            for clip in [
                None,
                Some(8.0f32),
                Some(7.0),
                Some(6.0),
                Some(5.0),
                Some(4.5),
                Some(4.0),
            ] {
                let opt = TxOptions {
                    clip_db: clip,
                    ..TxOptions::default()
                };
                let res = par_map(&(0..40u64).collect::<Vec<_>>(), |&r| {
                    let bits = random_bits(capacity_bits(c, 42), 300 + r);
                    let a = modulate_with(c, 1, &bits, CENTER, &opt);
                    papr_stats(&a)
                });
                let n = res.len() as f64;
                let peak = res.iter().map(|r| r.0).sum::<f64>() / n;
                let ccdf = res.iter().map(|r| r.1).sum::<f64>() / n;
                let crest = res.iter().map(|r| r.2).sum::<f64>() / n;
                let pw = res.iter().map(|r| r.3).sum::<f64>() / n;
                println!(
                    "| {c:?} | {} | {peak:.1} dB | {ccdf:.1} dB | {crest:.1} dB | {pw:.1} dB |",
                    clip.map_or("none".to_string(), |v| format!("{v} dB"))
                );
            }
        }
        println!();
    }

    // Clipping: BER at equal mean SNR.
    if only.contains('c') {
        println!("### Clipping cost: uncoded BER at equal mean SNR\n");
        println!("| Constellation | Channel | SNR | none | 8 dB | 7 dB | 6 dB | 5 dB |");
        println!("|---|---|---:|---:|---:|---:|---:|---:|");
        for (c, snr) in [
            (Constellation::Qpsk, 4.0f32),
            (Constellation::Psk8, 10.0),
            (Constellation::Qam16, 14.0),
            (Constellation::Qam16, 20.0),
            (Constellation::Qam16, 24.0),
        ] {
            for profile in [Profile::Awgn, Profile::CcirModerate] {
                print!("| {c:?} | {} | {snr:+} |", profile.spec().name);
                for clip in [
                    None,
                    Some(8.0f32),
                    Some(7.0),
                    Some(6.0),
                    Some(5.0),
                    Some(4.5),
                    Some(4.0),
                ] {
                    let opt = TxOptions {
                        clip_db: clip,
                        ..TxOptions::default()
                    };
                    let res = par_map(&(0..runs).collect::<Vec<_>>(), |&r| {
                        ber_run(c, profile, snr, 1000 * r + 17, &opt)
                    });
                    let e: usize = res.iter().map(|r| r.errors).sum();
                    let b: usize = res.iter().map(|r| r.bits).sum();
                    let m = res.iter().filter(|r| !r.found).count();
                    print!(" {} |", fmt_ber(e, b, m));
                }
                println!();
            }
        }
        println!();
    }

    // Throughput.
    if only.contains('r') {
        println!("### Payload bit rate (uncoded, before FEC)\n");
        let i = info();
        println!(
            "Overhead (preamble + header + ramp + filter tails) {:.0} ms, symbol {:.0} ms, {} data carriers.\n",
            i.overhead_s * 1e3,
            i.symbol_s * 1e3,
            i.data_carriers
        );
        println!(
            "| Constellation | Raw (continuous) | 1 s burst (42 sym) | 3 s burst (125 sym) | 10 s burst (417 sym) |"
        );
        println!("|---|---:|---:|---:|---:|");
        for c in Constellation::ALL {
            let raw = (i.data_carriers * c.bits_per_symbol()) as f32 / i.symbol_s;
            let rate = |n: usize| capacity_bits(c, n) as f32 / (i.overhead_s + n as f32 * i.symbol_s);
            println!(
                "| {c:?} | {raw:.0} | {:.0} | {:.0} | {:.0} |",
                rate(42),
                rate(125),
                rate(417)
            );
        }
        println!();
    }

    // LLR calibration.
    if only.contains('l') {
        println!("### LLR calibration (genie header, {runs} bursts)\n");
        println!("| Case | abs(L) bin | bits | measured BER | 1/(1+e^abs(L)) |");
        println!("|---|---|---:|---:|---:|");
        for (c, profile, snr) in [
            (Constellation::Qpsk, Profile::Awgn, 2.0f32),
            (Constellation::Qpsk, Profile::CcirPoor, 8.0),
            (Constellation::Psk8, Profile::CcirModerate, 14.0),
            (Constellation::Qam16, Profile::CcirGood, 18.0),
        ] {
            let res = par_map(&(0..runs).collect::<Vec<_>>(), |&r| {
                let bits = random_bits(capacity_bits(c, 42), 4000 + r);
                let burst = modulate(c, 1, &bits, CENTER);
                let p = power(&burst);
                let a = impair(&pad(&burst, 0.25, 0.3), profile, 4000 + r, Some(snr), p, 0.0, 0.0);
                let mut rx = PhyRx::new(CENTER);
                rx.set_genie_header(Some(genie(c, bits.len(), 1, 0.25)));
                let mut out = Vec::new();
                rx.process(&a, &mut out);
                out.first().map(|b| (b.llrs[..bits.len()].to_vec(), bits.clone()))
            });
            let mut llr = Vec::new();
            let mut bits = Vec::new();
            for (l, b) in res.into_iter().flatten() {
                llr.extend(l);
                bits.extend(b);
            }
            for (lo, hi, meas, pred, n) in calibration(&llr, &bits) {
                let hi = if hi > 1e6 { "inf".to_string() } else { hi.to_string() };
                println!(
                    "| {c:?} {} {snr:+} dB | {lo}-{hi} | {n} | {meas:.4} | {pred:.4} |",
                    profile.spec().name
                );
            }
        }
        println!();
    }
}

const NUM_PROFILES: [Profile; 4] = [
    Profile::Awgn,
    Profile::CcirPoor,
    Profile::MidDisturbedNvis,
    Profile::LowDisturbed,
];

fn profile_label(p: Profile) -> String {
    let s = p.spec();
    let d = s.paths.iter().map(|q| q.delay_ms).fold(0f32, f32::max);
    let f = s.paths.iter().map(|q| q.spread_hz).fold(0f32, f32::max);
    if d == 0.0 && f == 0.0 {
        s.name.to_string()
    } else {
        format!("{} ({d} ms, {f} Hz)", s.name)
    }
}

/// One QPSK codeword of `rate` in one burst, real header (no genie):
/// (burst delivered with the right header, codeword decoded).
fn codeword_run(num: Numerology, rate: ion::fec::Rate, profile: Profile, snr: f32, seed: u64) -> (bool, bool) {
    use ion::fec;
    let info_bits = random_bits(fec::K, seed);
    let coded = fec::encode(rate, &info_bits);
    let user = (seed % 65521) as u16;
    let c = Constellation::Qpsk;
    let burst = modulate_full(num, c, user, 0, &coded, num.center_hz(), &TxOptions::default());
    let p = power(&burst);
    let a = impair(&pad(&burst, 0.25, 0.3), profile, seed, Some(snr), p, 0.0, 0.0);
    let (got, _) = rx_num(num, &a, 4800);
    let Some(b) = got.iter().find(|b| b.user == user && !b.header_only) else {
        return (false, false);
    };
    let ok = fec::decode(rate, &b.llrs[..rate.n()]).is_some_and(|d| d == info_bits);
    (true, ok)
}

/// One header-only burst: delivered with the right `user` and `extra`.
fn header_only_run(num: Numerology, profile: Profile, snr: f32, seed: u64) -> bool {
    let user = (seed.wrapping_mul(7919) % 65536) as u16;
    let extra = (seed.wrapping_mul(104_729) % 65536) as u16;
    let burst = modulate_full(
        num,
        Constellation::Qpsk,
        user,
        extra,
        &[],
        num.center_hz(),
        &TxOptions::default(),
    );
    let p = power(&burst);
    let a = impair(
        &pad(&burst, 0.25, 0.3),
        profile,
        77 + 1000 * seed,
        Some(snr),
        p,
        0.0,
        0.0,
    );
    let (got, _) = rx_num(num, &a, 4800);
    got.len() == 1 && got[0].header_only && got[0].user == user && got[0].extra == extra
}

/// Tables for the numerology sections of `docs/phy.md`:
///
/// cargo test -j 2 -p ion --release --test phy -- --ignored --nocapture
/// --test-threads 2 numerology_tables
///
/// `PHY_RUNS` bursts per point (default 40), `PHY_NUM_TABLES` picks
/// c(odeword), h(eader-only), s(izes); 2 threads (`ION_THREADS`).
#[test]
#[ignore]
fn numerology_tables() {
    use ion::fec::Rate;
    let runs: u64 = std::env::var("PHY_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let only: String = std::env::var("PHY_NUM_TABLES").unwrap_or_else(|_| "shc".into());
    let rate_sel: String = std::env::var("PHY_RATES").unwrap_or_else(|_| "432".into());
    let rates: Vec<Rate> = [(Rate::R1_4, '4'), (Rate::R1_3, '3'), (Rate::R1_2, '2')]
        .into_iter()
        .filter(|r| rate_sel.contains(r.1))
        .map(|r| r.0)
        .collect();
    let nums: Vec<Numerology> = match std::env::var("PHY_NUMS").as_deref() {
        Ok("long") => vec![Numerology::Long],
        Ok("normal") => vec![Numerology::Normal],
        Ok("narrow") => vec![Numerology::Normal, Numerology::Long],
        Ok("wide") => vec![Numerology::NormalWide, Numerology::LongWide],
        _ => Numerology::ALL.to_vec(),
    };
    let prof_sel: String = std::env::var("PHY_PROFILES").unwrap_or_else(|_| "apnl".into());
    let profiles: Vec<Profile> = NUM_PROFILES
        .into_iter()
        .zip("apnl".chars())
        .filter(|p| prof_sel.contains(p.1))
        .map(|p| p.0)
        .collect();

    if only.contains('s') {
        println!("### Burst lengths (QPSK, one codeword per burst)\n");
        print!("|");
        for num in &nums {
            print!(" | {num:?}");
        }
        println!(" |\n|---|{}", "---:|".repeat(nums.len()));
        let ms = |num, nbits| 1000.0 * burst_samples_for(num, Constellation::Qpsk, nbits) as f32 / AUDIO_RATE;
        print!("| Header-only |");
        for &num in &nums {
            print!(" {:.0} ms |", ms(num, 0));
        }
        println!();
        for &r in &rates {
            let n = r.n();
            print!("| {r:?} ({n} bits) |");
            for &num in &nums {
                print!(
                    " {} sym, {:.0} ms |",
                    data_symbols_for(num, Constellation::Qpsk, n),
                    ms(num, n)
                );
            }
            println!();
        }
        println!();
    }

    if only.contains('h') {
        let snrs: Vec<i32> = (-10..=8).step_by(2).collect();
        let hruns = 3 * runs;
        println!("### Header-only decode probability, {hruns} bursts per point\n");
        print!("| Channel | Burst |");
        for s in &snrs {
            print!(" {s:+} |");
        }
        println!("\n|---|---|{}", "---:|".repeat(snrs.len()));
        for &profile in &profiles {
            for &num in &nums {
                let jobs: Vec<(i32, u64)> = snrs.iter().flat_map(|&s| (0..hruns).map(move |r| (s, r))).collect();
                let res = par_map(&jobs, |&(s, r)| header_only_run(num, profile, s as f32, r));
                print!("| {} | {num:?} header-only |", profile_label(profile));
                for i in 0..snrs.len() {
                    let ok = res[i * hruns as usize..(i + 1) * hruns as usize]
                        .iter()
                        .filter(|&&b| b)
                        .count();
                    print!(" {:.2} |", ok as f64 / hruns as f64);
                }
                println!();
            }
            // Reference: the header of a 1 s Normal QPSK data burst.
            let jobs: Vec<(i32, u64)> = snrs.iter().flat_map(|&s| (0..hruns).map(move |r| (s, r))).collect();
            let res = par_map(&jobs, |&(s, r)| {
                let user = (r * 7919 % 65536) as u16;
                let bits = random_bits(capacity_bits(Constellation::Qpsk, 42), r + 5);
                let burst = modulate(Constellation::Qpsk, user, &bits, CENTER);
                let p = power(&burst);
                let a = impair(
                    &pad(&burst, 0.25, 0.3),
                    profile,
                    77 + 1000 * r,
                    Some(s as f32),
                    p,
                    0.0,
                    0.0,
                );
                let got = rx_all(&a, 4800);
                got.len() == 1 && got[0].user == user
            });
            print!("| {} | Normal, header of a 1 s data burst |", profile_label(profile));
            for i in 0..snrs.len() {
                let ok = res[i * hruns as usize..(i + 1) * hruns as usize]
                    .iter()
                    .filter(|&&b| b)
                    .count();
                print!(" {:.2} |", ok as f64 / hruns as f64);
            }
            println!();
        }
        println!();
    }

    if only.contains('c') {
        let snrs: Vec<i32> = (-4..=16).step_by(2).collect();
        println!(
            "### Codeword success, QPSK, one codeword per burst, {runs} bursts per point (real header, no genie)\n"
        );
        for rate in rates {
            println!("#### {rate:?}\n");
            print!("| Channel | Numerology |");
            for s in &snrs {
                print!(" {s:+} |");
            }
            println!("\n|---|---|{}", "---:|".repeat(snrs.len()));
            for &profile in &profiles {
                for &num in &nums {
                    let jobs: Vec<(i32, u64)> = snrs.iter().flat_map(|&s| (0..runs).map(move |r| (s, r))).collect();
                    let res = par_map(&jobs, |&(s, r)| {
                        codeword_run(num, rate, profile, s as f32, 5000 + 1000 * r + (s + 50) as u64)
                    });
                    print!("| {} | {num:?} |", profile_label(profile));
                    for i in 0..snrs.len() {
                        let rs = &res[i * runs as usize..(i + 1) * runs as usize];
                        let c = rs.iter().filter(|r| r.1).count();
                        print!(" {:.2} |", c as f64 / runs as f64);
                    }
                    println!();
                }
            }
            println!();
        }
    }
}

/// (mean-normalised envelope peak dB, 99.9% envelope CCDF dB, audio crest
/// dB, mean audio power relative to TX_LEVEL² dB) of one burst.
fn papr_stats(a: &[f32]) -> (f64, f64, f64, f64) {
    let mut nco = Nco::new(CENTER, AUDIO_RATE);
    let mut fir = Fir::new(lowpass_taps(481, 1600.0, AUDIO_RATE));
    let delay = 240;
    let mut env = Vec::with_capacity(a.len());
    for (i, &x) in a.iter().chain(std::iter::repeat_n(&0.0, delay)).enumerate() {
        let z = fir.filter(nco.mix_down(x));
        if i >= delay {
            env.push(z.norm_sqr() as f64);
        }
    }
    // Skip the filter tails and ramps at both ends.
    let body = &env[600..env.len() - 600];
    let mean = body.iter().sum::<f64>() / body.len() as f64;
    let peak = body.iter().fold(0f64, |m, v| m.max(*v));
    let mut s = body.to_vec();
    s.sort_by(|a, b| b.total_cmp(a));
    let p999 = s[s.len() / 1000];
    let ap = a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / a.len() as f64;
    let apk = a.iter().fold(0f64, |m, v| m.max(v.abs() as f64));
    let lvl = ion_dsp::TX_LEVEL as f64;
    (
        10.0 * (peak / mean).log10(),
        10.0 * (p999 / mean).log10(),
        10.0 * (apk * apk / ap).log10(),
        10.0 * (ap / (lvl * lvl)).log10(),
    )
}

#[test]
#[ignore]
fn cpu_cost() {
    // About 60 s of back-to-back 3 s bursts, CCIR moderate at 10 dB.
    let mut audio = Vec::new();
    let mut k = 0u64;
    while audio.len() < 60 * AUDIO_RATE as usize {
        let c = Constellation::ALL[k as usize % 3];
        let bits = random_bits(capacity_bits(c, 125), k);
        audio.extend(modulate(c, k as u16, &bits, CENTER));
        audio.extend(std::iter::repeat_n(0.0, 4800));
        k += 1;
    }
    let p = power(&audio);
    let a = impair(&audio, Profile::CcirModerate, 1, Some(10.0), p, 0.0, 0.0);
    let secs = a.len() as f64 / AUDIO_RATE as f64;
    let t = std::time::Instant::now();
    let mut rx = PhyRx::new(CENTER);
    let mut out = Vec::new();
    let mut worst = 0f64;
    for c in a.chunks(480) {
        let t1 = std::time::Instant::now();
        rx.process(c, &mut out);
        worst = worst.max(t1.elapsed().as_secs_f64());
    }
    let rx_s = t.elapsed().as_secs_f64();
    println!(
        "RX: slowest 10 ms chunk took {:.2} ms (the end of a 3 s burst)",
        worst * 1e3
    );
    let bits = random_bits(capacity_bits(Constellation::Qam16, 125), 1);
    let n_tx = 20;
    let t = std::time::Instant::now();
    let plain = TxOptions {
        clip_db: None,
        ..TxOptions::default()
    };
    for _ in 0..n_tx {
        std::hint::black_box(modulate_with(Constellation::Qam16, 1, &bits, CENTER, &plain));
    }
    let tx_s = t.elapsed().as_secs_f64() / n_tx as f64;
    let opt = TxOptions::default();
    let t = std::time::Instant::now();
    for _ in 0..n_tx {
        std::hint::black_box(modulate_with(Constellation::Qam16, 1, &bits, CENTER, &opt));
    }
    let txc_s = t.elapsed().as_secs_f64() / n_tx as f64;
    let burst_s = burst_samples(Constellation::Qam16, bits.len()) as f64 / AUDIO_RATE as f64;
    println!(
        "RX: {secs:.1} s of audio ({k} bursts sent, {} decoded) in {rx_s:.3} s: {:.1} ms per second of audio",
        out.len(),
        1e3 * rx_s / secs
    );
    println!(
        "TX: {burst_s:.2} s burst in {:.1} ms ({:.1} ms/s); with clipping {:.1} ms ({:.1} ms/s)",
        tx_s * 1e3,
        tx_s * 1e3 / burst_s,
        txc_s * 1e3,
        txc_s * 1e3 / burst_s
    );
}

/// 64QAM and the clip: uncoded BER and reported SNR against the clip
/// level on AWGN (wide, sparse pilots), genie header.
#[test]
#[ignore]
fn diag_clip64() {
    let num = Numerology::NormalWide;
    let clips = [None, Some(9.0f32), Some(8.0), Some(7.0), Some(6.0)];
    for c in [Constellation::Qam16, Constellation::Qam64] {
        for snr in [20.0f32, 24.0, 28.0, 32.0, 50.0] {
            let jobs: Vec<(Option<f32>, u64)> = clips.iter().flat_map(|&cl| (0..4u64).map(move |s| (cl, s))).collect();
            let res = par_map(&jobs, |&(clip, s)| {
                let o = TxOptions {
                    clip_db: clip,
                    pilots: Pilots::Sparse,
                    ..TxOptions::default()
                };
                let bits = random_bits(capacity_bits_with(num, Pilots::Sparse, c, 42), 900 + s);
                let burst = modulate_full(num, c, 1, 0, &bits, num.center_hz(), &o);
                let p = power(&burst);
                let a = impair(&pad(&burst, 0.2, 0.3), Profile::Awgn, 1900 + s, Some(snr), p, 0.0, 0.0);
                let mut rx = PhyRx::with_numerology(num, num.center_hz());
                rx.set_genie_header(Some(Genie {
                    constellation: c,
                    data_symbols: data_symbols_with(num, Pilots::Sparse, c, bits.len()),
                    user: 1,
                    start: (0.2 * AUDIO_RATE) as u64 + 200,
                    pilots: Pilots::Sparse,
                }));
                let mut out = Vec::new();
                rx.process(&a, &mut out);
                let b = &out[0];
                (bit_errors(&b.llrs, &bits), bits.len(), b.snr_db)
            });
            let mut line = format!("| {c:?} | {snr:+} |");
            for &cl in &clips {
                let rs: Vec<_> = jobs
                    .iter()
                    .zip(&res)
                    .filter(|(j, _)| j.0 == cl)
                    .map(|(_, r)| *r)
                    .collect();
                let e: usize = rs.iter().map(|r| r.0).sum();
                let n: usize = rs.iter().map(|r| r.1).sum();
                let s = rs.iter().map(|r| r.2).sum::<f32>() / rs.len() as f32;
                line += &format!(" {:.1e} / {s:.1} |", e as f64 / n as f64);
            }
            println!("{line}");
        }
    }
}

/// Drift tracking near the floor: link-sized bursts (8 s) at low SNR with
/// no offset, tracking on and off (it must not hurt).
#[test]
#[ignore]
fn diag_clock_low_snr() {
    for (profile, snr) in [(Profile::Awgn, -2.0f32), (Profile::Awgn, 0.0), (Profile::CcirPoor, 2.0)] {
        for num in [Numerology::NormalWide, Numerology::LongWide] {
            let nsym = if num.is_long() { 160 } else { 333 };
            for track in [true, false] {
                let jobs: Vec<u64> = (0..16).collect();
                let rs: Vec<(f64, f32)> = par_map(&jobs, |&s| {
                    drift_run(num, Constellation::Qpsk, nsym, 0.0, profile, snr, 500 + s, track)
                })
                .into_iter()
                .flatten()
                .collect();
                let ber = rs.iter().map(|r| r.0).sum::<f64>() / rs.len().max(1) as f64;
                let worst = rs.iter().map(|r| r.1.abs()).fold(0f32, f32::max);
                println!(
                    "| {} {snr:+} | {num:?} | {track} | {}/16 | {ber:.3e} | {worst:.0} |",
                    profile_label(profile),
                    rs.len()
                );
            }
        }
    }
}
