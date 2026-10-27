//! The link's lifeline (`docs/link.md`, Lifeline): R1/7 bursts that only
//! the lifeline's receiver hears, and a link that holds below the floor.

use ion::level::{FLOOR_WAVE, Guard, LADDER, Wave};
use ion::link::audio::{AudioMedium, Receivers, transmit};
use ion::link::frame::{Frame, Kind};
use ion::link::sim::Medium;
use ion::link::station::{CONTROL, CONTROL_BYTES, TINY, user_bits};
use ion::link::{Incoming, Outgoing};
use ion_dsp::channel::Profile;
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

fn tiny(wave: Wave, kind: u8, extra: u16) -> Outgoing {
    Outgoing {
        wave,
        code: TINY,
        user: user_bits(TINY, 0x5A, kind),
        extra,
        frames: Vec::new(),
        spans: Vec::new(),
    }
}

fn data(wave: Wave, code: usize, n: usize) -> Outgoing {
    let size = match code {
        CONTROL => CONTROL_BYTES,
        l => LADDER[l].frame_bytes(),
    };
    let frames = (0..n)
        .map(|i| {
            let mut f = Frame::new(Kind::Data, 100 * i as u32);
            f.payload = vec![i as u8; 20];
            f.encode(size)
        })
        .collect();
    Outgoing {
        wave,
        code,
        user: user_bits(code, 0x5A, 1),
        extra: 0,
        frames,
        spans: Vec::new(),
    }
}

/// `b` with noise at `snr_db` (2.4 kHz, against its mean power), 3.5 s of
/// noise ahead and 4.2 s after (the lifeline's receiver runs 7 times
/// slower), through `rx`: every burst heard.
fn hear(rx: &mut Receivers, b: &Outgoing, snr_db: f32, seed: u64) -> Vec<Incoming> {
    let tx = transmit(b);
    let p = tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32;
    let mut a = vec![0.0; (3.5 * AUDIO_RATE) as usize];
    a.extend_from_slice(&tx);
    a.resize(a.len() + (4.2 * AUDIO_RATE) as usize, 0.0);
    let sigma = noise_sigma(p, snr_db, 2400.0, AUDIO_RATE);
    let mut g = NoiseGen::new(seed);
    for x in &mut a {
        *x += sigma * g.gaussian();
    }
    let mut out = Vec::new();
    for chunk in a.chunks(4800) {
        rx.process(chunk, &mut out);
    }
    out
}

#[test]
fn lifeline_bursts_are_heard_only_by_the_lifeline_receiver() {
    let b = tiny(Wave::Lifeline, 2, 0xBEEF);
    let got = hear(&mut Receivers::new(), &b, -14.0, 1);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        (got[0].wave, got[0].code, got[0].user, got[0].extra),
        (Wave::Lifeline, TINY, b.user, b.extra)
    );
    // A control frame in the lifeline too.
    let c = data(Wave::Lifeline, CONTROL, 1);
    let got = hear(&mut Receivers::new(), &c, -14.0, 2);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].wave, Wave::Lifeline);
    assert_eq!(got[0].frames[0].as_deref(), Some(&c.frames[0][..]));
    // The full-band receivers hear nothing of either, even loud.
    for (k, b) in [b, c].iter().enumerate() {
        let got = hear(&mut Receivers::without_lifeline(), b, 10.0, 3 + k as u64);
        assert!(got.is_empty(), "{got:?}");
    }
}

#[test]
fn full_band_bursts_never_come_out_of_the_lifeline() {
    let mut rx = Receivers::new();
    let slice = LADDER.iter().position(|l| l.wave == Wave::Ofdm(Guard::Slice)).unwrap();
    let ofdm = LADDER.iter().position(|l| l.wave == Wave::Ofdm(Guard::Normal)).unwrap();
    let bursts = [
        tiny(FLOOR_WAVE, 1, 0x1234),
        tiny(Wave::Mfsk(ion::phy_mfsk::MfskMode::R1), 3, 0),
        data(FLOOR_WAVE, 0, 1),
        data(Wave::Mfsk(ion::phy_mfsk::MfskMode::R1), 3, 4),
        data(Wave::Ofdm(Guard::Slice), slice, 2),
        tiny(Wave::Ofdm(Guard::Normal), 5, 0),
        data(Wave::Ofdm(Guard::Normal), ofdm, 6),
    ];
    for (k, b) in bursts.iter().enumerate() {
        for snr in [-6.0, 20.0] {
            let got = hear(&mut rx, b, snr, 10 + k as u64);
            assert!(
                got.iter().all(|i| i.wave != Wave::Lifeline),
                "{:?} at {snr} dB: {got:?}",
                b.wave
            );
            assert!(snr < 0.0 || got.iter().any(|i| i.wave == b.wave), "{:?} missed", b.wave);
        }
    }
}

#[test]
fn the_lifeline_receiver_follows_the_offset_it_hears() {
    // 50 Hz off: beyond R1/7's pull-in (±35 Hz) but well inside R0's. An
    // R0 burst heard first moves the lifeline's receiver there.
    let life = tiny(Wave::Lifeline, 1, 0x0042);
    let mut m = AudioMedium::new(Profile::Awgn, -12.0, 5).with_offset(50.0);
    assert!(m.carry(0, &life, 0.0, 8.0).is_none(), "heard 50 Hz off untuned");
    let r0 = tiny(FLOOR_WAVE, 1, 7);
    assert_eq!(m.carry(0, &r0, 10.0, 12.0).map(|i| i.wave), Some(FLOOR_WAVE));
    let got = m.carry(0, &life, 20.0, 28.0);
    assert_eq!(got.map(|i| (i.wave, i.extra)), Some((Wave::Lifeline, 0x0042)));
}
