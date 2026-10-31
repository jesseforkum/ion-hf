//! HARQ with incremental redundancy: the receiver's LLR store on its own,
//! across a level change through the real PHY, and over a whole link.

use std::sync::Arc;

use ion::fec::{self, Rate};
use ion::level::{Guard, LADDER, Wave};
use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::frame::{Frame, Kind};
use ion::link::harq::{self, HarqRx, Soft};
use ion::link::sim::{Medium, ModelAir, ModelMedium, Sim};
use ion::link::station::{Outgoing, user_bits};
use ion::link::{Air, Config, Station};
use ion::phy::PhyRx;
use ion_dsp::channel::{Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

/// Normal-guard QPSK 2/3 and 16QAM 2/3 (`levels_are_the_ones_named`
/// checks).
const QPSK_2_3: usize = 11;
const QAM16_2_3: usize = 13;

#[test]
fn levels_are_the_ones_named() {
    use ion::phy::Constellation;
    let is = |l: usize, c: Constellation, r: Rate| {
        LADDER[l].wave == Wave::Ofdm(Guard::Normal) && LADDER[l].constellation == c && LADDER[l].rate == r
    };
    assert!(is(QPSK_2_3, Constellation::Qpsk, Rate::R2_3));
    assert!(is(QAM16_2_3, Constellation::Qam16, Rate::R2_3));
}

fn frame(off: u32) -> Vec<u8> {
    let mut f = Frame::new(Kind::Data, off);
    f.payload = (0..170).map(|i| (i as u32 * 7 + off) as u8).collect();
    f.encode(fec::K / 8)
}

/// BPSK (bit 0 -> +1) over AWGN at `es_n0_db` per coded bit: LLRs `2y/σ²`.
fn bpsk_awgn(bits: &[u8], es_n0_db: f64, noise: &mut NoiseGen) -> Vec<f32> {
    let sigma2 = 1.0 / (2.0 * 10f64.powf(es_n0_db / 10.0));
    let sigma = sigma2.sqrt() as f32;
    let scale = (2.0 / sigma2) as f32;
    bits.iter()
        .map(|&b| ((if b == 0 { 1.0 } else { -1.0 }) + sigma * noise.gaussian()) * scale)
        .collect()
}

#[test]
fn a_frame_that_fails_at_rate_2_3_decodes_after_one_increment() {
    assert_eq!(LADDER[QPSK_2_3].rate, Rate::R2_3);
    let (n, inc) = (Rate::R2_3.n(), harq::increment(Rate::R2_3.n(), Rate::R2_3.n()));
    assert_eq!(n + inc, Rate::R1_2.n(), "the first increment takes 2/3 to 1/2");
    let mut recovered = 0;
    for seed in 0..6 {
        let f = frame(1000 * seed as u32);
        let info = audio::bytes_to_bits(&f);
        let mut noise = NoiseGen::new(seed);
        // Rate 2/3 needs about 0.5 dB per coded bit, rate 1/2 about -2 dB.
        let es_n0 = -1.0;
        let first = bpsk_awgn(&fec::encode_span(&info, 0, n), es_n0, &mut noise);
        assert_eq!(fec::decode(Rate::R2_3, &first), None, "seed {seed}: 2/3 decoded alone");
        let mut rx = HarqRx::new();
        let r = rx.receive(QPSK_2_3, 0, &Soft::Llrs(first));
        assert_eq!((r.frames.clone(), r.stored), (vec![None], true));
        assert_eq!(rx.held(), vec![n]);

        // The next burst opens with the increment: 720 more bits, and no
        // fresh frames after it.
        let more = bpsk_awgn(&fec::encode_span(&info, n, inc), es_n0, &mut noise);
        let r = rx.receive(QPSK_2_3, harq::check_bits(1), &Soft::Llrs(more));
        assert_eq!(r.bits, vec![inc]);
        assert_eq!(r.parity, 1);
        if r.frames[0].as_deref() == Some(&f[..]) {
            recovered += r.recovered;
            assert!(rx.is_empty(), "a decoded codeword stays stored");
        }
    }
    assert_eq!(recovered, 6, "only {recovered} of 6 recovered at rate 1/2");
}

#[test]
fn increments_go_on_to_chase_combining() {
    // A codeword at -7 dB per coded bit: hopeless at 2/3, and beyond even
    // the whole mother code (rate 1/4 needs about -6 dB): it takes a
    // chase repeat.
    let f = frame(77);
    let info = audio::bytes_to_bits(&f);
    let mut noise = NoiseGen::new(42);
    let slot = Rate::R2_3.n();
    let mut rx = HarqRx::new();
    let r = rx.receive(
        QPSK_2_3,
        0,
        &Soft::Llrs(bpsk_awgn(&fec::encode_span(&info, 0, slot), -7.0, &mut noise)),
    );
    assert_eq!(r.frames, vec![None]);
    let mut sent = slot;
    let mut done = false;
    while !rx.is_empty() {
        let h = rx.held()[0];
        assert_eq!(h, sent);
        let (from, bits) = (harq::start(h), harq::increment(h, slot));
        let llrs = bpsk_awgn(&fec::encode_span(&info, from, bits), -7.0, &mut noise);
        let r = rx.receive(QPSK_2_3, harq::check_bits(1), &Soft::Llrs(llrs));
        sent += bits;
        if r.frames[0].is_some() {
            assert_eq!(r.frames[0].as_deref(), Some(&f[..]));
            done = true;
        }
    }
    assert!(done, "dropped after {sent} bits");
    assert!(sent > fec::N_MOTHER, "decoded at {sent} bits without a chase repeat");
}

#[test]
fn a_check_bit_mismatch_keeps_nothing() {
    let f = frame(5);
    let info = audio::bytes_to_bits(&f);
    let mut noise = NoiseGen::new(1);
    let mut rx = HarqRx::new();
    let n = Rate::R2_3.n();
    rx.receive(
        QPSK_2_3,
        0,
        &Soft::Llrs(bpsk_awgn(&fec::encode_span(&info, 0, n), -3.0, &mut noise)),
    );
    assert_eq!(rx.len(), 1);
    // The sender claims two stored codewords.
    let r = rx.receive(QPSK_2_3, harq::check_bits(2), &Soft::Llrs(vec![1.0; 3000]));
    assert!(!r.stored && r.frames.is_empty() && rx.is_empty());
}

/// One burst through the real OFDM PHY on AWGN at `snr_db`.
fn over_the_air(b: &Outgoing, snr_db: f32, seed: u64) -> ion::link::Incoming {
    let tx = audio::transmit(b);
    let power = tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32;
    let mut a = vec![0.0; 4800];
    a.extend(tx);
    a.extend(vec![0.0; 4800]);
    add_awgn(
        &mut a,
        noise_sigma(power, snr_db, 2400.0, AUDIO_RATE),
        &mut NoiseGen::new(seed),
    );
    // The link's OFDM width, as `audio::Receivers` uses.
    let mut rx = PhyRx::with_numerology(audio::numerology(Guard::Normal), audio::CENTER_HZ);
    let mut bursts = Vec::new();
    rx.process(&a, &mut bursts);
    assert_eq!(bursts.len(), 1, "burst not detected");
    audio::decode(&bursts[0])
}

#[test]
fn llrs_combine_across_a_level_change() {
    // First sent as 16QAM 2/3 at 7 dB (it needs about 10), then the
    // increment to rate 1/2 comes as QPSK at 6 dB.
    let mut recovered = 0;
    let tries = 6;
    for seed in 0..tries {
        let f = frame(300 + seed as u32);
        let first = Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[QAM16_2_3].wave,
            code: QAM16_2_3,
            user: user_bits(QAM16_2_3, 9, 1),
            extra: 0,
            frames: vec![f.clone()],
            spans: vec![(0, Rate::R2_3.n())],
        };
        let inc = harq::increment(Rate::R2_3.n(), LADDER[QPSK_2_3].coded_bits());
        let second = Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[QPSK_2_3].wave,
            code: QPSK_2_3,
            user: user_bits(QPSK_2_3, 9, 2 | harq::check_bits(1) << 2),
            extra: 0,
            frames: vec![f.clone()],
            spans: vec![(Rate::R2_3.n(), inc)],
        };
        let mut rx = HarqRx::new();
        let a = over_the_air(&first, 7.0, 10 + seed);
        let r = rx.receive(a.code, 0, a.soft.as_ref().unwrap());
        assert_eq!(r.frames, vec![None], "seed {seed}: 16QAM 2/3 decoded at 7 dB");
        let b = over_the_air(&second, 6.0, 20 + seed);
        assert_eq!(b.code, QPSK_2_3);
        let hq = ion::link::station::user_hi(b.user) >> 2;
        let r = rx.receive(b.code, hq, b.soft.as_ref().unwrap());
        assert_eq!(r.parity, 1);
        if r.frames[0].is_some() {
            assert_eq!(r.frames[0].as_deref(), Some(&f[..]));
            recovered += 1;
        }
    }
    assert_eq!(recovered, tries, "only {recovered} of {tries} recovered");
}

fn audio_pair(profile: Profile, snr_db: f32, seed: u64, harq: bool) -> Sim {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    // ION_MARGIN: another gear-shift margin.
    let margin = std::env::var("ION_MARGIN").ok().map(|v| v.parse().unwrap());
    let cfg = |call| {
        let c = audio::config(call);
        Config {
            harq,
            margin_db: margin.unwrap_or(c.margin_db),
            ..c
        }
    };
    let a = Station::new(cfg("N0JLF"), air.clone());
    let mut b = Station::new(cfg("W1AW"), air.clone());
    b.listen();
    Sim::new(a, b, Box::new(AudioMedium::new(profile, snr_db, seed)), air)
}

#[test]
fn harq_recovers_frames_over_the_audio_link() {
    // Long enough to climb into 16QAM, where CCIR poor at 14 dB loses
    // frames.
    let n = 20_000;
    let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
    let mut sim = audio_pair(Profile::CcirPoor, 14.0, 4, true);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let ok = sim.run_until(900.0, |s| s.stations[1].stats().bytes_received >= n as u64);
    let (a, b) = (sim.stations[0].stats().clone(), sim.stations[1].stats().clone());
    eprintln!(
        "{:.0} s: {} increments ({} bits), {} recovered, {} failed, {} resent of {} frames; levels {:?}",
        sim.t,
        a.increments_sent,
        a.increment_bits,
        b.harq_recovered,
        b.codewords_failed,
        a.frames_resent,
        a.frames_sent,
        a.levels
    );
    assert!(ok, "{} of {n} bytes", b.bytes_received);
    assert_eq!(sim.stations[1].take_received(), data);
    assert!(a.increments_sent > 0, "no increments sent");
    assert!(b.harq_recovered > 0, "nothing recovered by combining");
    // Every failed codeword was continued, not resent.
    assert!(a.frames_resent < a.increments_sent, "{} resent", a.frames_resent);
}

/// Every data burst sent: (wave, spans).
type Sent = Arc<std::sync::Mutex<Vec<(Wave, Vec<(usize, usize)>)>>>;

/// The model channel, recording every data burst sent.
struct Recording {
    inner: ModelMedium,
    sent: Sent,
}

impl Medium for Recording {
    fn carry(&mut self, from: usize, b: &Outgoing, start: f64, end: f64) -> Option<ion::link::Incoming> {
        if from == 0 && b.code < LADDER.len() {
            self.sent.lock().unwrap().push((b.wave, b.spans.clone()));
        }
        self.inner.carry(from, b, start, end)
    }
}

#[test]
fn stored_codewords_are_not_continued_in_another_guard() {
    // Normal-guard OFDM is useless (long echoes), long guard is fine: the
    // link loses whole normal-guard bursts and falls back to long guard.
    // Their codewords must go out fresh there, not as increments on top
    // of the echo-damaged LLRs.
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let a = Station::new(Config::new("N0ABC"), air.clone());
    let mut b = Station::new(Config::new("W1AW"), air.clone());
    b.listen();
    let mut m = ModelMedium::new(12.0, 21);
    for (l, t) in m.thresholds_db.iter_mut().enumerate() {
        if LADDER[l].wave == Wave::Ofdm(Guard::Normal) {
            *t = 100.0;
        }
    }
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut sim = Sim::new(
        a,
        b,
        Box::new(Recording {
            inner: m,
            sent: sent.clone(),
        }),
        air,
    );
    let n = 20_000;
    let data: Vec<u8> = (0..n).map(|i| (i * 7 % 253) as u8).collect();
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    assert!(sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= n as u64));
    assert_eq!(sim.stations[1].take_received(), data);
    let sent = sent.lock().unwrap();
    assert!(
        sent.iter().any(|(w, _)| *w == Wave::Ofdm(Guard::Normal)),
        "never tried normal guard"
    );
    for w in sent.windows(2) {
        let increments = w[1].1.iter().any(|&(from, _)| from > 0);
        assert!(
            !increments || w[0].0 == w[1].0,
            "increments carried from {:?} into {:?}",
            w[0].0,
            w[1].0
        );
    }
}
