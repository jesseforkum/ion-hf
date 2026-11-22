//! The 500 Hz link (`docs/link.md`, 500 Hz links): a call in the narrow
//! OFDM wave makes a link whose every burst is that wave or the lifeline.

use std::sync::Arc;

use ion::level::{LADDER, NARROW_MFSK_WAVE, NARROW_OFDM, NARROW_WAVE, Wave, is_narrow};
use ion::link::audio::{AudioMedium, PhyAir};
use ion::link::sim::{Medium, Sim};
use ion::link::station::{CONTROL, TINY};
use ion::link::{Air, Config, Incoming, Outgoing, State, Station};
use ion_dsp::channel::Profile;

struct TwoWay([Box<dyn Medium>; 2]);

impl Medium for TwoWay {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        self.0[from].carry(from, burst, start, end)
    }
}

fn payload(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// A (N0ABC) and B (W1AW, listening) over `profile` at `snr` both ways,
/// with its steps `(from, snr)`; each a 500 Hz station or not.
fn link(profile: Profile, snr: f32, steps: &[(f64, f32)], bw500: [bool; 2], seed: u64) -> Sim {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let mk = |call: &str, narrow: bool| {
        let mut c = Config::new(call);
        c.bw500 = narrow;
        // ION_NEWGEAR=0: without the gear rules of 2026-11-21, to compare.
        if std::env::var("ION_NEWGEAR").is_ok_and(|v| v == "0") {
            c.doubt_db = None;
            c.aim_fall_db = None;
            c.loss_step_db = None;
            c.class_soft = false;
        }
        Station::new(c, air.clone())
    };
    let a = mk("N0ABC", bw500[0]);
    let mut b = mk("W1AW", bw500[1]);
    b.listen();
    // Each medium hears for the other station: A's bursts with B's
    // receivers, and back.
    let medium = |k: usize| -> Box<dyn Medium> {
        // `ION_PEP=1`: noise against the bursts' common peak, as a
        // transmitter limited by its peak power sends them.
        let mut m = AudioMedium::new(profile, snr, seed * 7 + k as u64).peak_referenced(env("ION_PEP", 0) == 1);
        m.schedule = steps.to_vec();
        m.set_bw500(bw500[1 - k]);
        Box::new(m)
    };
    Sim::new(a, b, Box::new(TwoWay([medium(0), medium(1)])), air)
}

/// Every burst on the air is the narrow OFDM wave or the lifeline, and of
/// the narrow ladder.
fn assert_narrow(sim: &Sim) {
    for e in &sim.log {
        assert!(
            e.wave == NARROW_WAVE || e.wave == NARROW_MFSK_WAVE || e.wave == Wave::Lifeline,
            "{:?} code {} at {:.1} s",
            e.wave,
            e.code,
            e.start
        );
        assert!(
            e.code == TINY || e.code == CONTROL || is_narrow(e.code),
            "code {} at {:.1} s",
            e.code,
            e.start
        );
        assert!(!e.cue && e.slot.is_none());
    }
}

/// A sends `n` bytes; runs until B has them all or `t_max`: (seconds,
/// bytes B got, intact).
fn transfer(sim: &mut Sim, n: usize, t_max: f64) -> (f64, usize, bool) {
    let data = payload(n, 5);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let mut got = Vec::new();
    while sim.t < t_max && got.len() < n {
        sim.step();
        got.extend(sim.stations[1].take_received());
    }
    (sim.t, got.len(), data.starts_with(&got))
}

fn levels(sim: &Sim) -> String {
    let mut n = vec![0usize; LADDER.len()];
    for e in &sim.log {
        if e.code < LADDER.len() {
            n[e.code] += 1;
        }
    }
    (0..LADDER.len())
        .filter(|&l| n[l] > 0)
        .map(|l| format!("{}×{}", ion::link::sim::level_name(l), n[l]))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn a_500_hz_link_connects_and_moves_data_in_500_hz() {
    let mut sim = link(Profile::Awgn, 12.0, &[], [true, true], 1);
    let (t, got, intact) = transfer(&mut sim, 6000, 300.0);
    let st = sim.stations[0].stats();
    eprintln!(
        "{got} bytes in {t:.0} s; {}; fixed {} dropped {}",
        levels(&sim),
        st.width_fixed,
        st.width_dropped
    );
    assert!(sim.stations.iter().all(|s| s.link500()));
    assert!(intact && got == 6000, "{got} bytes, intact {intact}");
    assert!(
        sim.log.iter().any(|e| e.code > NARROW_OFDM && is_narrow(e.code)),
        "never climbed"
    );
    assert_narrow(&sim);
    assert_eq!(sim.stations.iter().map(|s| s.stats().width_dropped).sum::<u32>(), 0);
}

#[test]
fn a_500_hz_station_answers_no_wide_call_and_a_wide_one_hears_no_narrow_call() {
    for (bw, name) in [([false, true], "wide calls 500"), ([true, false], "500 calls wide")] {
        let mut sim = link(Profile::Awgn, 15.0, &[], bw, 2);
        sim.stations[0].connect("W1AW", 0.0);
        sim.run_until(600.0, |s| s.stations[0].state() == State::Failed);
        assert_eq!(sim.stations[0].state(), State::Failed, "{name}");
        assert_eq!(sim.stations[1].state(), State::Listening, "{name}");
        assert!(sim.log.iter().all(|e| e.from == 0), "{name}: the called station sent");
        if bw[0] {
            assert_narrow(&sim);
        }
    }
}

/// A fade below the narrow floor: the link holds in the lifeline and
/// comes back, never wider than 500 Hz.
#[test]
fn a_500_hz_link_holds_in_the_lifeline_through_a_fade() {
    let mut sim = link(Profile::Awgn, 10.0, &[(40.0, -14.0), (160.0, 10.0)], [true, true], 3);
    let (t, got, intact) = transfer(&mut sim, 40_000, 420.0);
    let st = sim.stations[0].stats();
    eprintln!(
        "{got} bytes in {t:.0} s; holds {} resumes {}; {}; fixed {} dropped {}",
        st.holds,
        st.resumes,
        levels(&sim),
        st.width_fixed,
        st.width_dropped
    );
    assert_narrow(&sim);
    assert!(intact);
    assert!(
        st.holds >= 1 && st.resumes >= 1,
        "holds {} resumes {}",
        st.holds,
        st.resumes
    );
    assert!(sim.log.iter().any(|e| e.wave == Wave::Lifeline));
    assert!(
        sim.log.iter().any(|e| e.start > 170.0 && is_narrow(e.code)),
        "no data after the fade"
    );
}

/// Throughput of the 500 Hz link: bytes delivered in `ION_SECS` (300) at
/// each SNR, AWGN and CCIR poor, `ION_SEEDS` (2) seeds. `ION_WIDE=1`: the
/// 2.45 kHz link in the same harness, to compare.
#[test]
#[ignore]
fn narrow_throughput_table() {
    let secs: f64 = env("ION_SECS", 300.0);
    let seeds: u64 = env("ION_SEEDS", 2);
    let narrow = env("ION_WIDE", 0) == 0;
    println!("| SNR | AWGN bit/s | CCIR poor bit/s | levels (first seed, poor) |");
    println!("|---:|---:|---:|---|");
    // `ION_SNRS` (e.g. `-6,-3,0`) picks the rows.
    let snrs: Vec<f32> = match std::env::var("ION_SNRS") {
        Ok(v) => v.split(',').filter_map(|x| x.trim().parse().ok()).collect(),
        Err(_) => vec![
            -18.0, -15.0, -12.0, -9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 10.0, 15.0, 20.0, 25.0,
        ],
    };
    for snr in snrs {
        let mut cells = Vec::new();
        let mut lv = String::new();
        for profile in [Profile::Awgn, Profile::CcirPoor] {
            let mut bps = Vec::new();
            for seed in 0..seeds {
                let mut sim = link(profile, snr, &[], [narrow, narrow], 10 + seed);
                let (_, got, intact) = transfer(&mut sim, 2_000_000, secs);
                assert!(intact);
                if narrow {
                    assert_narrow(&sim);
                }
                for st in &sim.stations {
                    assert_eq!((st.stats().width_fixed, st.stats().width_dropped), (0, 0));
                }
                bps.push(got as f64 * 8.0 / secs);
                if seed == 0 && profile == Profile::CcirPoor {
                    lv = levels(&sim);
                }
            }
            cells.push(format!("{:.0}", bps.iter().sum::<f64>() / bps.len() as f64));
        }
        println!("| {snr:+} | {} | {} | {lv} |", cells[0], cells[1]);
    }
}

/// Data both ways and a disconnect: the turn-over, the replies and the
/// goodbye all stay in 500 Hz.
#[test]
fn a_500_hz_link_turns_over_and_disconnects_in_500_hz() {
    let mut sim = link(Profile::Awgn, 10.0, &[], [true, true], 4);
    let (a, b) = (payload(3000, 1), payload(2000, 2));
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&a);
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    let mut replied = false;
    let mut asked = false;
    while sim.t < 400.0 && !sim.stations.iter().all(|s| s.state() == State::Disconnected) {
        sim.step();
        got_b.extend(sim.stations[1].take_received());
        got_a.extend(sim.stations[0].take_received());
        if !replied && got_b.len() == a.len() {
            sim.stations[1].send(&b);
            replied = true;
        }
        if !asked && got_a.len() == b.len() {
            sim.stations[0].disconnect();
            asked = true;
        }
    }
    eprintln!("{:.0} s; {}", sim.t, levels(&sim));
    assert_eq!((got_b.len(), got_a.len()), (a.len(), b.len()));
    assert!(got_b == a && got_a == b);
    assert!(sim.stations.iter().all(|s| s.state() == State::Disconnected));
    assert_narrow(&sim);
    for s in &sim.stations {
        assert_eq!((s.stats().width_fixed, s.stats().width_dropped), (0, 0));
    }
}

/// For scanners: when the tries of an unanswered 500 Hz call go out, and
/// how often noise alone makes the narrow receiver look like a call
/// coming in (`Receivers::control_in`).
#[test]
#[ignore]
fn narrow_call_timing_and_noise() {
    use ion::link::audio::Receivers;
    use ion_dsp::dsp::{AUDIO_RATE, NoiseGen};
    let mut sim = link(Profile::Awgn, 15.0, &[], [true, false], 2);
    sim.stations[0].connect("W1AW", 0.0);
    sim.run_until(900.0, |s| s.stations[0].state() == State::Failed);
    let tries: Vec<String> = sim
        .log
        .iter()
        .map(|e| format!("{:.1}–{:.1} {:?}", e.start, e.end, e.wave))
        .collect();
    println!("call tries (s): {}; gave up at {:.1} s", tries.join(", "), sim.t);
    let mut rx = Receivers::new();
    rx.set_bw500(true);
    let mut noise = NoiseGen::new(77);
    let secs = env("ION_SECS", 600usize);
    let (mut on, mut rises, mut was) = (0usize, 0usize, false);
    let mut out = Vec::new();
    let block = AUDIO_RATE as usize / 10;
    for _ in 0..secs * 10 {
        let mut a = vec![0.0f32; block];
        ion_dsp::channel::add_awgn(&mut a, 0.05, &mut noise);
        out.clear();
        rx.process(&a, &mut out);
        let c = rx.control_in();
        on += c as usize;
        rises += (c && !was) as usize;
        was = c;
    }
    println!(
        "noise {secs} s: control_in true {:.1}% of the time, {rises} times; bursts {}",
        on as f32 / (secs * 10) as f32 * 100.0,
        out.len()
    );
}

/// Below its OFDM and on fast fading the 500 Hz link has its MFSK: data
/// moves at −12 dB, and on CCIR flutter, where no OFDM level works.
#[test]
fn a_500_hz_link_moves_data_in_its_mfsk() {
    for (profile, snr, name) in [
        (Profile::Awgn, -12.0, "AWGN −12 dB"),
        (Profile::CcirFlutter, 6.0, "flutter +6 dB"),
    ] {
        let mut sim = link(profile, snr, &[], [true, true], 6);
        let (t, got, intact) = transfer(&mut sim, 300, 600.0);
        eprintln!("{name}: {got} bytes in {t:.0} s; {}", levels(&sim));
        assert!(intact && got == 300, "{name}: {got} bytes");
        assert!(
            sim.log
                .iter()
                .any(|e| e.wave == NARROW_MFSK_WAVE && e.frames > 0 && e.delivered)
        );
        assert_narrow(&sim);
    }
}
