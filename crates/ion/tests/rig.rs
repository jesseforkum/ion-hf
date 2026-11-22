//! A link through two radios, not two sound cards: an SSB filter each
//! end, a receiver's AGC, a transmitter driven into its ALC. The session
//! of 2026-11-21 (`docs/link.md`, Not moved by one burst) lost 16QAM
//! bursts at +25 dB reported and read echoes on a path that had none; the
//! tables on a bare channel show neither.

use std::sync::{Arc, Mutex};

use ion::level::LADDER;
use ion::link::audio::{AudioMedium, PhyAir};
use ion::link::sim::{Medium, Sim};
use ion::link::{Air, Config, Incoming, Outgoing, Station};
use ion_dsp::channel::Profile;
use ion_dsp::dsp::AUDIO_RATE;

/// (start, SNR read) of each burst station 0 sent that was heard.
type Heard = Arc<Mutex<Vec<(f64, f32)>>>;

struct TwoWay([AudioMedium; 2], Heard);

impl Medium for TwoWay {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        let inc = self.0[from].carry(from, burst, start, end);
        if let (0, Some(i)) = (from, &inc) {
            self.1.lock().unwrap().push((start, i.snr_db));
        }
        inc
    }
}

fn env<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn payload(n: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9u32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// A Butterworth section (RBJ), high-pass or low-pass at `hz`.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}

impl Biquad {
    fn new(high: bool, hz: f32, q: f32) -> Self {
        let w = std::f32::consts::TAU * hz / AUDIO_RATE;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = if high {
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0]
        } else {
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0]
        };
        Self {
            b: b.map(|v| v / a0),
            a: [-2.0 * c / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// An SSB filter: Butterworth of order `2 * sections` at each edge, with
/// the group delay such a filter has there.
fn ssb_filter(lo: f32, hi: f32, sections: usize) -> impl FnMut(&mut [f32]) {
    let n = 2 * sections;
    let qs: Vec<f32> = (0..sections)
        .map(|k| 1.0 / (2.0 * (std::f32::consts::PI * (2 * k + 1) as f32 / (2 * n) as f32).cos()))
        .collect();
    let mut bq: Vec<Biquad> = qs
        .iter()
        .flat_map(|&q| [Biquad::new(true, lo, q), Biquad::new(false, hi, q)])
        .collect();
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            for s in bq.iter_mut() {
                *v = s.run(*v);
            }
        }
    }
}

/// A receiver's AGC: the gain follows the envelope's peaks, up in
/// `attack_ms`, back in `decay_ms`.
fn agc(attack_ms: f32, decay_ms: f32) -> impl FnMut(&mut [f32]) {
    let up = (-1.0 / (attack_ms * 1e-3 * AUDIO_RATE)).exp();
    let down = (-1.0 / (decay_ms * 1e-3 * AUDIO_RATE)).exp();
    let mut env = 1e-4f32;
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            let m = v.abs();
            env = if m > env {
                m + (env - m) * up
            } else {
                (env * down).max(1e-4)
            };
            *v *= 0.25 / env.max(1e-4);
        }
    }
}

/// A transmitter into its ALC: peaks over `db` under the burst's highest
/// are pressed down (tanh).
fn alc(db: f32) -> impl FnMut(&mut [f32]) {
    move |x: &mut [f32]| {
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        let knee = peak * 10f32.powf(-db / 20.0);
        if knee > 0.0 {
            for v in x.iter_mut() {
                *v = knee * (*v / knee).tanh();
            }
        }
    }
}

/// Steady carriers in the receiver's passband (birdies, a neighbour's
/// carrier), each `db` against a tone at the bursts' peak.
fn birdies(hz: &'static [f32], db: f32) -> impl FnMut(&mut [f32]) {
    let amp = ion_dsp::TX_LEVEL * 10f32.powf(db / 20.0);
    let mut n = 0u64;
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            let t = n as f32 / AUDIO_RATE;
            *v += hz
                .iter()
                .map(|f| amp * (std::f32::consts::TAU * f * t).sin())
                .sum::<f32>();
            n = (n + 1) % (AUDIO_RATE as u64 * 60);
        }
    }
}

/// A sound card that drops `ms` of audio every `every_s` or so.
fn slips(every_s: f32, ms: f32) -> impl FnMut(&mut [f32]) {
    let cut = (ms * 1e-3 * AUDIO_RATE) as usize;
    let period = (every_s * AUDIO_RATE) as usize;
    let mut left = period / 2;
    move |x: &mut [f32]| {
        let mut at = 0;
        while at + left + cut < x.len() {
            at += left;
            x.copy_within(at + cut.., at);
            let n = x.len();
            x[n - cut..].fill(0.0);
            left = period;
        }
        left -= (x.len() - at).min(left);
        if left == 0 {
            left = period;
        }
    }
}

/// A transmitter whose first `ms` of each burst are `db` down (PTT or a
/// power ramp late).
fn head(ms: f32, db: f32) -> impl FnMut(&mut [f32]) {
    let n = (ms * 1e-3 * AUDIO_RATE) as usize;
    let g = 10f32.powf(-db / 20.0);
    move |x: &mut [f32]| {
        let n = n.min(x.len());
        for v in &mut x[..n] {
            *v *= g;
        }
    }
}

/// A receiver overloaded: third-order distortion `db` under the signal
/// at the bursts' peak.
fn overload(db: f32) -> impl FnMut(&mut [f32]) {
    let k = 10f32.powf(-db / 20.0) / (ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL);
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            *v -= k * *v * *v * *v;
        }
    }
}

/// A receiver whose AGC has run its gain up to the noise between bursts
/// (it was muted while its own transmitter was keyed): the gain comes
/// down over the first of each burst.
fn agc_recovered(attack_ms: f32, decay_ms: f32) -> impl FnMut(&mut [f32]) {
    let up = (-1.0 / (attack_ms * 1e-3 * AUDIO_RATE)).exp();
    let down = (-1.0 / (decay_ms * 1e-3 * AUDIO_RATE)).exp();
    move |x: &mut [f32]| {
        let mut env = 1e-4f32;
        for v in x.iter_mut() {
            let m = v.abs();
            env = if m > env {
                m + (env - m) * up
            } else {
                (env * down).max(1e-4)
            };
            *v = (*v * 0.25 / env.max(1e-4)).clamp(-0.5, 0.5);
        }
    }
}

/// Receive audio into a sound card set too hot: clipped `db` under the
/// bursts' peak.
fn rx_clip(db: f32) -> impl FnMut(&mut [f32]) {
    let at = ion_dsp::TX_LEVEL * 10f32.powf(-db / 20.0);
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            *v = v.clamp(-at, at);
        }
    }
}

/// A receiver's gain `db` down when the burst's audio starts, coming back
/// in `tau_ms` (a slow AGC hit by the T/R change).
fn pump(db: f32, tau_ms: f32) -> impl FnMut(&mut [f32]) {
    let k = (-1.0 / (tau_ms * 1e-3 * AUDIO_RATE)).exp();
    move |x: &mut [f32]| {
        let mut down = db;
        for v in x.iter_mut() {
            *v *= 10f32.powf(-down / 20.0);
            down *= k;
        }
    }
}

/// A transmitter `hz` off at key-up, settling in `tau_ms`.
fn settle(hz: f32, tau_ms: f32) -> impl FnMut(&mut [f32]) {
    const HALF: usize = 127;
    let taps: Vec<f32> = (0..=2 * HALF)
        .map(|i| {
            let n = i as isize - HALF as isize;
            if n % 2 == 0 {
                0.0
            } else {
                let w = 0.54 + 0.46 * (std::f32::consts::PI * n as f32 / HALF as f32).cos();
                2.0 / (std::f32::consts::PI * n as f32) * w
            }
        })
        .collect();
    move |x: &mut [f32]| {
        let src = x.to_vec();
        let mut phase = 0f64;
        for (i, v) in x.iter_mut().enumerate() {
            let mut q = 0.0;
            for (k, t) in taps.iter().enumerate() {
                let j = i as isize + HALF as isize - k as isize;
                if *t != 0.0 && j >= 0 && (j as usize) < src.len() {
                    q += t * src[j as usize];
                }
            }
            let f = hz as f64 * (-(i as f64) / (tau_ms as f64 * 1e-3 * AUDIO_RATE as f64)).exp();
            phase += std::f64::consts::TAU * f / AUDIO_RATE as f64;
            let (sn, cs) = phase.sin_cos();
            *v = src[i] * cs as f32 - q * sn as f32;
        }
    }
}

/// A sound card whose clock is `ppm` off the sender's.
fn skew(ppm: f32) -> impl FnMut(&mut [f32]) {
    move |x: &mut [f32]| {
        let src = x.to_vec();
        let at = |i: isize| src[i.clamp(0, src.len() as isize - 1) as usize];
        for (n, v) in x.iter_mut().enumerate() {
            let t = n as f64 * (1.0 + ppm as f64 * 1e-6);
            let i = t.floor() as isize;
            let u = (t - i as f64) as f32;
            let (a, b, c, d) = (at(i - 1), at(i), at(i + 1), at(i + 2));
            *v = b + 0.5 * u * (c - a + u * (2.0 * a - 5.0 * b + 4.0 * c - d + u * (3.0 * (b - c) + d - a)));
        }
    }
}

/// Mains noise: a spike `ms` long every 1/120 s, at the bursts' peak.
fn impulses(ms: f32) -> impl FnMut(&mut [f32]) {
    let period = (AUDIO_RATE / 120.0) as usize;
    let len = ((ms * 1e-3 * AUDIO_RATE) as usize).max(1);
    let mut n = 0usize;
    let mut x32 = 0x2545_F491u32;
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            if n % period < len {
                x32 ^= x32 << 13;
                x32 ^= x32 >> 17;
                x32 ^= x32 << 5;
                *v += ion_dsp::TX_LEVEL * (x32 as f32 / u32::MAX as f32 * 2.0 - 1.0);
            }
            n += 1;
        }
    }
}

/// A sound card that goes silent for `ms` every `every_s` or so, without
/// losing its place.
fn dropouts(every_s: f32, ms: f32) -> impl FnMut(&mut [f32]) {
    let cut = (ms * 1e-3 * AUDIO_RATE) as usize;
    let period = (every_s * AUDIO_RATE) as usize;
    let mut n = period / 2;
    move |x: &mut [f32]| {
        for v in x.iter_mut() {
            if n % period < cut {
                *v = 0.0;
            }
            n += 1;
        }
    }
}

type RigFn = Box<dyn FnMut(&mut [f32])>;

fn chain(mut parts: Vec<RigFn>) -> Option<RigFn> {
    if parts.is_empty() {
        return None;
    }
    Some(Box::new(move |x: &mut [f32]| {
        for p in parts.iter_mut() {
            p(x);
        }
    }))
}

/// `what`: `+`-joined from `ssb` (300–2700 Hz, each end), `ssb8` (the
/// same, steeper), `agcf` (fast AGC), `agcs` (slow), `alc3`, `alc6`.
fn rig(what: &str, tx: bool) -> Option<RigFn> {
    let mut parts: Vec<RigFn> = Vec::new();
    for w in what.split('+') {
        match (w, tx) {
            ("ssb", _) => parts.push(Box::new(ssb_filter(300.0, 2700.0, 2))),
            ("ssb8", _) => parts.push(Box::new(ssb_filter(300.0, 2700.0, 4))),
            ("wide", _) => parts.push(Box::new(ssb_filter(200.0, 2900.0, 4))),
            ("ssbn", _) => parts.push(Box::new(ssb_filter(400.0, 2600.0, 4))),
            ("ssbh", _) => parts.push(Box::new(ssb_filter(500.0, 2900.0, 4))),
            ("ssbl", _) => parts.push(Box::new(ssb_filter(100.0, 2500.0, 4))),
            ("bird10", false) => parts.push(Box::new(birdies(&[700.0, 1310.0, 2105.0], -10.0))),
            ("bird20", false) => parts.push(Box::new(birdies(&[700.0, 1310.0, 2105.0], -20.0))),
            ("bird30", false) => parts.push(Box::new(birdies(&[700.0, 1310.0, 2105.0], -30.0))),
            ("slip1", false) => parts.push(Box::new(slips(3.0, 1.0))),
            ("slip02", false) => parts.push(Box::new(slips(3.0, 0.2))),
            ("head50", true) => parts.push(Box::new(head(50.0, 20.0))),
            ("head150", true) => parts.push(Box::new(head(150.0, 20.0))),
            ("over20", false) => parts.push(Box::new(overload(20.0))),
            ("over10", false) => parts.push(Box::new(overload(10.0))),
            ("agcx", false) => parts.push(Box::new(agc(0.5, 20.0))),
            ("agcr", false) => parts.push(Box::new(agc_recovered(2.0, 500.0))),
            ("agcr10", false) => parts.push(Box::new(agc_recovered(10.0, 500.0))),
            ("rclip6", false) => parts.push(Box::new(rx_clip(6.0))),
            ("rclip10", false) => parts.push(Box::new(rx_clip(10.0))),
            ("pump20", false) => parts.push(Box::new(pump(20.0, 300.0))),
            ("pump40", false) => parts.push(Box::new(pump(40.0, 300.0))),
            ("fset10", true) => parts.push(Box::new(settle(10.0, 100.0))),
            ("fset30", true) => parts.push(Box::new(settle(30.0, 100.0))),
            ("fset2", true) => parts.push(Box::new(settle(2.0, 100.0))),
            ("fset5", true) => parts.push(Box::new(settle(5.0, 100.0))),
            ("fslow5", true) => parts.push(Box::new(settle(5.0, 1000.0))),
            ("foff10", true) => parts.push(Box::new(settle(10.0, 1e9))),
            ("skew100", false) => parts.push(Box::new(skew(100.0))),
            ("skew300", false) => parts.push(Box::new(skew(300.0))),
            ("imp", false) => parts.push(Box::new(impulses(0.2))),
            ("drop", false) => parts.push(Box::new(dropouts(2.0, 20.0))),
            ("alc10", true) => parts.push(Box::new(alc(10.0))),
            ("alc15", true) => parts.push(Box::new(alc(15.0))),
            ("agcf", false) => parts.push(Box::new(agc(2.0, 100.0))),
            ("agcs", false) => parts.push(Box::new(agc(2.0, 2000.0))),
            ("alc3", true) => parts.push(Box::new(alc(3.0))),
            ("alc6", true) => parts.push(Box::new(alc(6.0))),
            (
                "agcf" | "agcs" | "agcx" | "alc3" | "alc6" | "alc10" | "alc15" | "bare" | "bird10" | "bird20"
                | "bird30" | "slip1" | "slip02" | "head50" | "head150" | "over20" | "over10" | "agcr" | "agcr10"
                | "rclip6" | "rclip10" | "pump20" | "pump40" | "fset10" | "fset30" | "fset2" | "fset5" | "fslow5"
                | "foff10" | "skew100" | "skew300" | "imp" | "drop",
                _,
            ) => {}
            _ => panic!("rig: {w}"),
        }
    }
    chain(parts)
}

struct Run {
    secs: f64,
    got: usize,
    levels: Vec<usize>,
    failed: u32,
    bursts: u32,
    flagged: usize,
    echo: usize,
    /// Data bursts under and over a second: sent, not heard, and the sum
    /// of the SNR read from those heard.
    by_length: [(u32, u32, f32); 2],
}

fn run(what: &str, snr: f32, bytes: usize, seed: u64) -> Run {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let mk = |call: &str| {
        let mut c = Config::new(call);
        if std::env::var("ION_NEWGEAR").is_ok_and(|v| v == "0") {
            c.doubt_db = None;
            c.aim_fall_db = None;
            c.loss_step_db = None;
            c.class_soft = false;
        }
        Station::new(c, air.clone())
    };
    let a = mk("N0ABC");
    let mut b = mk("W1AW");
    b.listen();
    let medium = |k: u64| {
        let mut m = AudioMedium::new(Profile::Awgn, snr, seed * 2 + k);
        for side in 0..2 {
            m.tx_rig[side] = rig(what, true);
            m.rx_rig[side] = rig(what, false);
        }
        m
    };
    let heard: Heard = Arc::default();
    let mut sim = Sim::new(a, b, Box::new(TwoWay([medium(3), medium(4)], heard.clone())), air);
    let data = payload(bytes);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let mut got = Vec::new();
    let limit = env("RIG_SECS", 300.0);
    while sim.t < limit && got.len() < data.len() {
        sim.step();
        got.extend(sim.stations[1].take_received());
    }
    assert!(data.starts_with(&got));
    let st = sim.stations[0].stats();
    if std::env::var("RIG_TRACE").is_ok() {
        let rx = sim.stations[1].stats();
        eprintln!(
            "{what}: polls {} search {} narrowings {} control {} resent {}/{} incr {}",
            st.polls,
            st.search_polls,
            rx.narrowings,
            st.control_bursts,
            st.frames_resent,
            st.frames_sent,
            st.increments_sent
        );
        for e in sim.log.iter().filter(|e| e.from == 0 && e.frames > 0) {
            eprintln!(
                "  {:6.1} L{} ×{} {:.1} s {} {:?}",
                e.start,
                e.code,
                e.frames,
                e.end - e.start,
                if e.delivered { "" } else { "LOST" },
                e.slot
            );
        }
        for (l, m, c) in &rx.heard {
            eprintln!(
                "  heard L{l} delay {:.1} ms doppler {:.1} Hz eff {:+.0} mean {:+.0} {}{}",
                m[0],
                m[1],
                m[2],
                m[3],
                if c.echo { "ECHO " } else { "" },
                if c.doppler { "DOPPLER" } else { "" }
            );
        }
    }
    let heard = heard.lock().unwrap();
    let mut by_length = [(0u32, 0u32, 0f32); 2];
    for e in sim
        .log
        .iter()
        .filter(|e| e.from == 0 && e.code < LADDER.len() && e.frames > 0)
    {
        let k = &mut by_length[(e.end - e.start >= 1.0) as usize];
        k.0 += 1;
        match heard.iter().find(|(t, _)| (t - e.start).abs() < 1e-3) {
            Some((_, snr)) => k.2 += snr,
            None => k.1 += 1,
        }
    }
    Run {
        by_length,
        secs: sim.t,
        got: got.len(),
        levels: sim
            .log
            .iter()
            .filter(|e| e.from == 0 && e.code < LADDER.len() && e.frames > 0)
            .map(|e| e.code)
            .collect(),
        failed: st.failed_bursts,
        bursts: st.data_bursts,
        flagged: st.classes.iter().filter(|(c, _)| c.doppler).count(),
        echo: st.classes.iter().filter(|(c, _)| c.echo).count(),
    }
}

/// `RIG=bare,ssb,ssb+agcf,…` `RIG_SNRS=25` `RIG_BYTES=52646`: time to move
/// the file, bursts lost, class flags, and the levels used.
#[test]
#[ignore]
fn rig_table() {
    let rigs: String = env("RIG", "bare,ssb,ssb8,agcf,agcs,alc3,alc6,ssb+agcf+alc3".to_string());
    let snrs: String = env("RIG_SNRS", "25".to_string());
    let bytes = env("RIG_BYTES", 52_646usize);
    let seeds = env("ION_SEEDS", 1u64);
    let threads = env("ION_THREADS", 8usize);
    let cases: Vec<(String, f32, u64)> = snrs
        .split(',')
        .flat_map(|s| rigs.split(',').map(move |r| (r.to_string(), s.parse::<f32>().unwrap())))
        .flat_map(|(r, s)| (1..=seeds).map(move |k| (r.clone(), s, k)))
        .collect();
    let mut runs = Vec::new();
    for chunk in cases.chunks(threads) {
        std::thread::scope(|sc| {
            let hs: Vec<_> = chunk
                .iter()
                .map(|(r, snr, seed)| sc.spawn(move || run(r, *snr, bytes, *seed)))
                .collect();
            runs.extend(hs.into_iter().map(|h| h.join().unwrap()));
        });
    }
    println!("| Rig | SNR dB | bit/s | each seed | lost | echo, Doppler | short: unheard, read | long | levels |");
    println!("|---|---:|---:|---|---:|---:|---|---|---|");
    for (case, xs) in cases.chunks(seeds as usize).zip(runs.chunks(seeds as usize)) {
        let (r, snr, _) = &case[0];
        let rate = |x: &Run| x.got as f64 * 8.0 / x.secs;
        let mut hist = vec![0usize; LADDER.len()];
        for &l in xs.iter().flat_map(|x| &x.levels) {
            hist[l] += 1;
        }
        let used: Vec<String> = hist
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .map(|(l, n)| format!("L{l}×{n}"))
            .collect();
        let each: Vec<String> = xs.iter().map(|x| format!("{:.0}", rate(x))).collect();
        let length = |k: usize| {
            let (n, lost, sum) = xs.iter().fold((0, 0, 0.0), |a, x| {
                let b = x.by_length[k];
                (a.0 + b.0, a.1 + b.1, a.2 + b.2)
            });
            format!("{lost}/{n} {:+.0}", sum / (n - lost).max(1) as f32)
        };
        println!(
            "| {r} | {snr:+.0} | {:.0} | {} | {}/{} | {} {} | {} | {} | {} |",
            xs.iter().map(rate).sum::<f64>() / xs.len() as f64,
            each.join(" "),
            xs.iter().map(|x| x.failed).sum::<u32>(),
            xs.iter().map(|x| x.bursts).sum::<u32>(),
            xs.iter().map(|x| x.echo).sum::<usize>(),
            xs.iter().map(|x| x.flagged).sum::<usize>(),
            length(0),
            length(1),
            used.join(" ")
        );
    }
}
