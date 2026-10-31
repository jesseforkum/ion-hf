//! The over-the-air test sequence, through a simulated HF channel and a
//! KiwiSDR-style 12 kHz recording, decodes again.

use ion::level::LADDER;
use ion::link::station::TINY;
use ion::phy::Constellation;
use ion::sounding::{self, read_wav, to_48k};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, lowpass_taps, noise_sigma};

/// 48 kHz to 12 kHz, as a KiwiSDR records.
fn to_12k(x: &[f32]) -> Vec<f32> {
    let taps = lowpass_taps(129, 5000.0, 48_000.0);
    (0..x.len() / 4)
        .map(|n| {
            let m = n * 4;
            taps.iter()
                .enumerate()
                .filter(|(j, _)| m >= *j)
                .map(|(j, t)| t * x[m - j])
                .sum()
        })
        .collect()
}

fn wav16(x: &[f32], rate: u32) -> Vec<u8> {
    let mut b = Vec::new();
    let len = (x.len() * 2) as u32;
    b.extend(b"RIFF");
    b.extend((36 + len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * 2).to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(len.to_le_bytes());
    for &s in x {
        b.extend(((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    b
}

fn on_air(a: &[f32], profile: Profile, snr_db: f32, seed: u64) -> Vec<f32> {
    let mut ch = Channel::from_profile(profile, seed);
    let mut x = vec![0.0f32; 4800];
    x.extend_from_slice(a);
    x.resize(x.len() + ch.latency() + 4800, 0.0);
    let mut y = vec![0.0f32; x.len()];
    ch.process(&x, &mut y);
    // SNR against the bursts' mean power (silences left out).
    let p: f32 =
        a.iter().map(|v| v * v).filter(|&v| v > 0.0).sum::<f32>() / a.iter().filter(|v| **v != 0.0).count() as f32;
    add_awgn(
        &mut y,
        noise_sigma(p, snr_db, 2400.0, AUDIO_RATE),
        &mut NoiseGen::new(seed ^ 77),
    );
    y
}

#[test]
fn the_sequence_decodes_through_a_kiwisdr_style_recording() {
    let (a, sent) = sounding::sequence("N0JLF");
    let secs = a.len() as f64 / 48_000.0;
    assert!((100.0..240.0).contains(&secs), "{secs:.0} s");
    let rx = on_air(&a, Profile::CcirGood, 25.0, 1);
    // Recorded at 12 kHz, saved as a WAV, read back at 48 kHz.
    let audio = read_wav(&wav16(&to_12k(&rx), 12_000)).unwrap();
    let heard = sounding::analyze(&audio);
    println!("{}", sounding::report(&sent, &heard));
    let off = sounding::offset_s(&sent, &heard);
    for b in &sent {
        // Every burst is detected and its header read.
        let h = sounding::find(b, &heard, off);
        assert!(
            h.is_some(),
            "burst {} ({}) not heard",
            b.seq,
            sounding::level_name(b.code, b.wave)
        );
        let h = h.unwrap();
        // Up to 16QAM 3/4 the data decodes too (64QAM needs more SNR than
        // this fading channel leaves).
        if b.code != TINY && LADDER[b.code].constellation != Constellation::Qam64 {
            assert_eq!(h.call.as_deref(), Some("N0JLF"));
            assert!(h.ok * 2 >= h.of, "burst {}: {}/{}", b.seq, h.ok, h.of);
        }
    }
}

/// On the air only OFDM gets louder, and by at most 3 dB: the CW IDs and
/// the MFSK carriers (steady key-down) keep the keyboard modes' peak. An
/// OFDM burst peaks at most the operator's OFDM level plus its level's trim
/// (the slice levels raised to full OFDM's average), never past the cap.
#[test]
fn only_ofdm_is_raised_and_never_past_the_cap() {
    let peak = |a: &[f32]| a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    let tx = ion_dsp::TX_LEVEL;
    let cap = ion::link::audio::OFDM_AIR_GAIN_MAX_DB;
    for (db, want) in [(0.0f32, 0.0f32), (1.2, 1.2), (20.0, 3.0)] {
        let (a, sent) = sounding::sequence_with("N0JLF", db);
        for b in &sent {
            let i = (b.start_s * 48_000.0) as usize;
            let j = ((b.start_s + b.dur_s) * 48_000.0) as usize;
            let p = peak(&a[i..j.min(a.len())]);
            let limit = match b.wave {
                ion::level::Wave::Mfsk(_) | ion::level::Wave::Lifeline => tx,
                ion::level::Wave::Ofdm(_) => {
                    let trim = ion::link::audio::level_trim_db(b.code);
                    tx * 10f32.powf((want + trim).min(cap) / 20.0)
                }
            };
            assert!(p <= limit * 1.001, "{db} dB: burst {} peaks {p:.3} > {limit:.3}", b.seq);
        }
        // The CW IDs, before the first burst and after the last.
        let first = (sent[0].start_s * 48_000.0) as usize;
        assert!(peak(&a[..first]) <= tx * 1.001);
        let last = sent.last().unwrap();
        assert!(peak(&a[((last.start_s + last.dur_s) * 48_000.0) as usize..]) <= tx * 1.001);
    }
}

#[test]
fn weak_signals_keep_only_the_robust_levels() {
    let (a, sent) = sounding::sequence("N0JLF");
    let heard = sounding::analyze(&to_48k(&to_12k(&on_air(&a, Profile::CcirPoor, -4.0, 2)), 12_000));
    let ok = |b: &sounding::Sent| {
        heard
            .iter()
            .any(|h| h.seq == Some(b.seq) && (h.code == TINY || h.ok > 0))
    };
    // The MFSK floor gets through; 16QAM and up do not.
    assert!(
        sent.iter().filter(|b| b.code == 0).all(ok),
        "{}",
        sounding::report(&sent, &heard)
    );
    let qam = |b: &&sounding::Sent| b.code != TINY && LADDER[b.code].constellation.bits_per_symbol() >= 4;
    assert!(!sent.iter().filter(qam).any(ok), "{}", sounding::report(&sent, &heard));
}

/// Even at the 3 dB cap, no OFDM burst averages more power than the steady
/// MFSK carriers: the low-PAPR levels raise OFDM's mean power at the same
/// peak, and the transmitter's continuous rating must still hold.
#[test]
fn ofdm_mean_power_stays_below_the_steady_carriers() {
    let (a, sent) = sounding::sequence_with("N0JLF", 20.0);
    let mean = |b: &sounding::Sent| {
        let i = (b.start_s * 48_000.0) as usize;
        let j = (((b.start_s + b.dur_s) * 48_000.0) as usize).min(a.len());
        a[i..j].iter().map(|x| x * x).sum::<f32>() / (j - i) as f32
    };
    let steady = sent
        .iter()
        .filter(|b| matches!(b.wave, ion::level::Wave::Mfsk(_)))
        .map(mean)
        .fold(0.0f32, f32::max);
    for b in sent.iter().filter(|b| matches!(b.wave, ion::level::Wave::Ofdm(_))) {
        let m = mean(b);
        println!(
            "burst {} ({}): {:+.1} dB against MFSK",
            b.seq,
            sounding::level_name(b.code, b.wave),
            10.0 * (m / steady).log10()
        );
        assert!(
            m <= steady,
            "burst {}: OFDM mean {m:.4} above the MFSK's {steady:.4}",
            b.seq
        );
    }
}
