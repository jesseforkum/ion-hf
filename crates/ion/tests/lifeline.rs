//! The link's lifeline (`docs/link.md`, Lifeline): R1/7 bursts that only
//! the lifeline's receiver hears, and a link that holds below the floor.

use std::sync::Arc;

use ion::level::{FLOOR_WAVE, Guard, LADDER, Wave};
use ion::link::audio::{AudioMedium, PhyAir, Receivers, transmit};
use ion::link::frame::{Frame, Kind};
use ion::link::sim::{Medium, ModelAir, ModelMedium, Sim};
use ion::link::station::{CONTROL, CONTROL_BYTES, End, TINY, user_bits};
use ion::link::{Air, Config, Incoming, Node, Outgoing, State, Station};
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
        cue: false,
        td_ease: false,
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
        cue: false,
        td_ease: false,
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

// -- the link holding below the floor ------------------------------------------
//
// The scenarios run on the model channel (`ModelMedium`, quick, many seeds)
// and through the real PHYs (`AudioMedium`, slow); `lifeline_table` and
// `lifeline_audio_table` measure them (`--ignored --nocapture`; `ION_SEEDS`,
// `ION_THREADS`).

/// Each direction its own channel (from station 0, from 1).
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

fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

/// The SNR (dB) each direction sees: `base`, then each step from its time.
#[derive(Clone, Debug)]
struct Paths {
    base: f32,
    ab: Vec<(f64, f32)>,
    ba: Vec<(f64, f32)>,
}

impl Paths {
    /// Both ways `base`, `low` from `from` for `secs` (forever if infinite).
    fn fade(base: f32, low: f32, from: f64, secs: f64) -> Self {
        let mut s = vec![(from, low)];
        if secs.is_finite() {
            s.push((from + secs, base));
        }
        Paths {
            base,
            ab: s.clone(),
            ba: s,
        }
    }

    /// One way only (`ab`: A to B; else B to A).
    fn one_way(base: f32, low: f32, from: f64, secs: f64, ab: bool) -> Self {
        let f = Self::fade(base, low, from, secs);
        if ab {
            Paths { ba: Vec::new(), ..f }
        } else {
            Paths { ab: Vec::new(), ..f }
        }
    }
}

/// The lifeline's timers from the environment, for the tables:
/// `ION_PROBE` / `ION_PROBE_MAX` (s), `ION_KNOCK` (s, 0 for none),
/// `ION_SILENT` (s), `ION_RESUME_DB`.
fn tune(c: &mut Config) {
    if let Some(v) = env("ION_PROBE") {
        c.life_probe_s = v;
    }
    if let Some(v) = env("ION_PROBE_MAX") {
        c.life_probe_max_s = v;
    }
    if let Some(v) = env::<f64>("ION_KNOCK") {
        c.life_knock_s = (v > 0.0).then_some(v);
    }
    if let Some(v) = env("ION_SILENT") {
        c.life_silent_s = v;
    }
    if let Some(v) = env("ION_RESUME_DB") {
        c.life_resume_db = v;
    }
}

#[derive(Clone, Copy, Debug)]
enum Chan {
    Model,
    Audio(Profile),
}

fn link(chan: Chan, lifeline: bool, paths: &Paths, seed: u64) -> Sim {
    let air: Arc<dyn Air> = match chan {
        Chan::Model => Arc::new(ModelAir::default()),
        Chan::Audio(_) => Arc::new(PhyAir),
    };
    let mk = |call: &str| {
        let mut c = Config::new(call);
        c.lifeline = lifeline;
        tune(&mut c);
        c
    };
    let a = Station::new(mk("N0ABC"), air.clone());
    let mut b = Station::new(mk("W1AW"), air.clone());
    b.listen();
    let medium = |sched: &Vec<(f64, f32)>, k: u64| -> Box<dyn Medium> {
        match chan {
            Chan::Model => {
                let mut m = ModelMedium::new(paths.base, seed * 7 + k);
                m.schedule = sched.clone();
                Box::new(m)
            }
            Chan::Audio(p) => {
                let mut m = AudioMedium::new(p, paths.base, seed * 7 + k);
                m.schedule = sched.clone();
                Box::new(m)
            }
        }
    };
    let m = TwoWay([medium(&paths.ab, 1), medium(&paths.ba, 2)]);
    Sim::new(a, b, Box::new(m), air)
}

#[derive(Debug, Default, Clone)]
struct Outcome {
    ends: [Option<End>; 2],
    /// When each station's link ended, from the start of the fade.
    ended_s: [Option<f64>; 2],
    /// Holds A began, and resumed.
    holds: u32,
    resumes: u32,
    /// From the SNR's return to the next byte delivered.
    resume_s: Option<f64>,
    /// Air seconds (both stations) from the start of the fade until data
    /// flows again (or until the end).
    air_s: f64,
    /// B's bytes are the payload's first ones, in order.
    intact: bool,
    delivered: u64,
    /// Bytes of the payload skipped in what B got (today's reconnect
    /// starts again from what was still queued: what was in flight is
    /// gone).
    lost: u64,
    /// Both still connected at the end (or, with `recall`, connected again).
    up: bool,
}

/// A sends a long stream from 0 s; the SNR does what `fade` says from
/// `fade.0`, back at `fade.1` (infinite: never). With `recall`, a station
/// whose link has ended listens again (B) or, once the SNR is back, calls
/// again (A): today's way back.
fn run(sim: &mut Sim, fade: (f64, f64), t_max: f64, recall: bool) -> Outcome {
    let data = payload(4_000_000, 9);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let mut o = Outcome::default();
    let mut got: Vec<u8> = Vec::new();
    let mut before = 0;
    while sim.t < t_max {
        sim.step();
        let t = sim.t;
        got.extend(sim.stations[1].take_received());
        if t < fade.1 {
            before = got.len();
        } else if o.resume_s.is_none() && got.len() > before {
            o.resume_s = Some(t - fade.1);
        }
        for k in 0..2 {
            let s = &sim.stations[k];
            if matches!(s.state(), State::Failed | State::Disconnected) && o.ended_s[k].is_none() {
                o.ended_s[k] = Some(t - fade.0);
                o.ends[k] = s.end();
            }
        }
        if recall {
            if matches!(sim.stations[1].state(), State::Failed | State::Disconnected) {
                sim.stations[1].listen();
            }
            if t >= fade.1 && matches!(sim.stations[0].state(), State::Failed | State::Disconnected) {
                sim.stations[0].connect("W1AW", t);
            }
        }
    }
    let st = sim.stations[0].stats();
    o.holds = st.holds;
    o.resumes = st.resumes;
    let until = o.resume_s.map_or(t_max, |r| fade.1 + r);
    o.air_s = sim
        .log
        .iter()
        .map(|e| (e.end.min(until) - e.start.max(fade.0)).max(0.0))
        .sum();
    o.delivered = got.len() as u64;
    o.intact = data.starts_with(&got);
    o.lost = match got.iter().zip(&data).position(|(a, b)| a != b) {
        Some(i) => {
            let probe = &got[i..(i + 64).min(got.len())];
            (0..data.len() - i)
                .find(|&g| data[i + g..].starts_with(probe))
                .unwrap_or(0) as u64
        }
        None => 0,
    };
    o.up = sim.stations.iter().all(|s| s.state() == State::Connected);
    o
}

fn trace(sim: &Sim, from: f64, to: f64) -> String {
    sim.log
        .iter()
        .filter(|e| e.start >= from && e.start <= to)
        .map(|e| {
            format!(
                "{:.0} {} {:?} c{} {}",
                e.start,
                "AB".as_bytes()[e.from] as char,
                e.wave,
                e.code,
                if e.delivered { "ok" } else { "LOST" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_deep_fade_is_held_and_resumed_without_losing_a_byte() {
    // +10 dB, then -15 dB (under the floor, over the lifeline) for 5 min.
    let fade = (120.0, 420.0);
    let paths = Paths::fade(10.0, -15.0, fade.0, fade.1 - fade.0);
    let mut sim = link(Chan::Model, true, &paths, 1);
    let o = run(&mut sim, fade, 900.0, false);
    let why = || format!("{o:?}\n{}", trace(&sim, fade.0, fade.1 + 120.0));
    assert!(o.up && o.ends == [None, None], "{}", why());
    assert!(o.holds >= 1 && o.resumes >= 1, "{}", why());
    assert!(o.intact && o.lost == 0, "{}", why());
    assert!(o.resume_s.is_some_and(|r| r < 90.0), "{}", why());
    // Today the same fade ends the link at both ends.
    let mut sim = link(Chan::Model, false, &paths, 1);
    let o = run(&mut sim, fade, 900.0, false);
    assert_eq!(o.ends, [Some(End::LinkLost); 2], "{o:?}");
}

#[test]
fn a_path_that_stays_weak_is_closed_by_agreement() {
    let fade = (120.0, f64::INFINITY);
    let paths = Paths::fade(10.0, -15.0, fade.0, f64::INFINITY);
    let mut sim = link(Chan::Model, true, &paths, 2);
    let o = run(&mut sim, fade, 2_400.0, false);
    let weak = Some(End::Weak { confirmed: true });
    assert_eq!(o.ends, [weak, weak], "{o:?}\n{}", trace(&sim, 1_000.0, 1_200.0));
    let cfg = Config::new("X");
    let by = cfg.life_enter_s + cfg.life_max_s + 300.0;
    assert!(o.ended_s.iter().all(|t| t.is_some_and(|t| t < by)), "{o:?}");
}

#[test]
fn nothing_heard_at_all_is_a_lost_link_at_both_ends() {
    let fade = (120.0, f64::INFINITY);
    let paths = Paths::fade(10.0, -40.0, fade.0, f64::INFINITY);
    let mut sim = link(Chan::Model, true, &paths, 3);
    let o = run(&mut sim, fade, 1_500.0, false);
    assert_eq!(o.ends, [Some(End::LinkLost); 2], "{o:?}");
    let cfg = Config::new("X");
    let by = cfg.life_enter_s + cfg.life_silent_s + cfg.life_probe_max_s + 60.0;
    assert!(o.ended_s.iter().all(|t| t.is_some_and(|t| t < by)), "{o:?}");
}

#[test]
fn a_disconnect_in_a_hold_says_goodbye_in_the_lifeline() {
    // Either end's host ends the link mid-hold: the goodbye goes in R1/7
    // and both ends know why.
    for who in [0, 1] {
        let fade = (120.0, f64::INFINITY);
        let paths = Paths::fade(10.0, -15.0, fade.0, f64::INFINITY);
        let mut sim = link(Chan::Model, true, &paths, 4);
        sim.stations[0].connect("W1AW", 0.0);
        sim.stations[0].send(&payload(1_000_000, 1));
        let held = sim.run_until(600.0, |s| s.stations.iter().all(Station::holding));
        assert!(held, "no hold\n{}", trace(&sim, 100.0, 600.0));
        sim.stations[who].disconnect();
        let t0 = sim.t;
        let done = sim.run_until(t0 + 200.0, |s| {
            s.stations.iter().all(|s| s.state() == State::Disconnected)
        });
        let ends = [sim.stations[0].end(), sim.stations[1].end()];
        assert!(done, "{ends:?}\n{}", trace(&sim, t0 - 10.0, sim.t));
        let mut want = [Some(End::Peer); 2];
        want[who] = Some(End::Client { confirmed: true });
        assert_eq!(ends, want);
        assert!(
            sim.log
                .iter()
                .filter(|e| e.start >= t0)
                .all(|e| e.wave == Wave::Lifeline)
        );
    }
}

#[test]
fn a_link_without_the_lifeline_times_out_as_before() {
    let fade = (60.0, f64::INFINITY);
    let paths = Paths::fade(10.0, -40.0, fade.0, f64::INFINITY);
    let mut sim = link(Chan::Model, false, &paths, 5);
    let o = run(&mut sim, fade, 600.0, false);
    assert_eq!(o.ends, [Some(End::LinkLost); 2], "{o:?}");
    assert!(o.ended_s.iter().all(|t| t.is_some_and(|t| t < 200.0)), "{o:?}");
    assert!(sim.log.iter().all(|e| e.wave != Wave::Lifeline));
}

/// Median and range of `v`, `{:.0}`.
fn spread(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "—".into();
    }
    v.sort_by(f64::total_cmp);
    let m = v[v.len() / 2];
    if v[0] == v[v.len() - 1] {
        format!("{m:.0}")
    } else {
        format!("{m:.0} ({:.0}–{:.0})", v[0], v[v.len() - 1])
    }
}

fn end_name(e: Option<End>) -> &'static str {
    match e {
        None => "up",
        Some(End::Client { confirmed: true }) => "client",
        Some(End::Client { confirmed: false }) => "client?",
        Some(End::Peer) => "peer",
        Some(End::NoAnswer) => "no answer",
        Some(End::LinkLost) => "lost",
        Some(End::Weak { confirmed: true }) => "weak",
        Some(End::Weak { confirmed: false }) => "weak?",
    }
}

/// One table row over the seeds' outcomes.
fn row(label: &str, os: &[Outcome]) -> String {
    let n = os.len();
    let up = os.iter().filter(|o| o.up).count();
    let ok = os.iter().filter(|o| o.intact).count();
    let mut resume: Vec<f64> = os.iter().filter_map(|o| o.resume_s).collect();
    let mut air: Vec<f64> = os.iter().map(|o| o.air_s).collect();
    let mut lost: Vec<f64> = os.iter().map(|o| o.lost as f64).collect();
    let mut holds: Vec<f64> = os.iter().map(|o| o.holds as f64).collect();
    let mut end_a: Vec<f64> = os.iter().filter_map(|o| o.ended_s[0]).collect();
    let mut end_b: Vec<f64> = os.iter().filter_map(|o| o.ended_s[1]).collect();
    let mut ends: Vec<String> = os
        .iter()
        .map(|o| format!("{}/{}", end_name(o.ends[0]), end_name(o.ends[1])))
        .collect();
    ends.sort();
    ends.dedup();
    format!(
        "| {label} | {up}/{n} | {ok}/{n} | {} | {} | {} | {} | {} / {} | {} |",
        spread(&mut resume),
        spread(&mut lost),
        spread(&mut air),
        spread(&mut holds),
        spread(&mut end_a),
        spread(&mut end_b),
        ends.join(", ")
    )
}

const HEADER: &str = "| Scenario | up at end | stream intact | resume after SNR back, s | bytes lost | air s (fade → data again) | holds (A) | ended A / B, s after fade | ends A/B |\n|---|---:|---:|---:|---:|---:|---:|---:|---|";

type Scenario = (&'static str, Paths, (f64, f64), f64);

fn scenarios() -> Vec<Scenario> {
    let f = 120.0;
    let mut v: Vec<Scenario> = Vec::new();
    for mins in [2.0, 5.0, 10.0] {
        let secs = mins * 60.0;
        let name: &'static str = Box::leak(format!("fade -15 dB {mins} min").into_boxed_str());
        v.push((name, Paths::fade(10.0, -15.0, f, secs), (f, f + secs), f + secs + 600.0));
    }
    let never = (f, f64::INFINITY);
    v.push((
        "dies at -15 dB",
        Paths::fade(10.0, -15.0, f, f64::INFINITY),
        never,
        1_800.0,
    ));
    v.push((
        "dies at -30 dB",
        Paths::fade(10.0, -30.0, f, f64::INFINITY),
        never,
        1_200.0,
    ));
    v.push((
        "A→B -15 dB 10 min",
        Paths::one_way(15.0, -15.0, f, 600.0, true),
        (f, f + 600.0),
        1_320.0,
    ));
    v.push((
        "B→A -15 dB 10 min",
        Paths::one_way(15.0, -15.0, f, 600.0, false),
        (f, f + 600.0),
        1_320.0,
    ));
    v.push((
        "A→B -15 dB for good",
        Paths::one_way(15.0, -15.0, f, f64::INFINITY, true),
        never,
        1_800.0,
    ));
    v
}

fn table(chan: Chan, seeds: u64, scen: &[Scenario]) {
    let threads: usize = env("ION_THREADS").unwrap_or(4);
    let jobs: Vec<(usize, bool, u64)> = (0..scen.len())
        .flat_map(|s| {
            [false, true]
                .into_iter()
                .flat_map(move |l| (0..seeds).map(move |k| (s, l, k)))
        })
        .collect();
    let out = std::sync::Mutex::new(vec![Vec::new(); scen.len() * 2]);
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(k, lifeline, seed)) = jobs.get(i) else { break };
                    let (name, paths, fade, t_max) = &scen[k];
                    let mut sim = link(chan, lifeline, paths, 100 + seed);
                    let o = run(&mut sim, *fade, *t_max, !lifeline && fade.1.is_finite());
                    eprintln!("{name} lifeline {lifeline} seed {seed}: {o:?}");
                    out.lock().unwrap()[2 * k + lifeline as usize].push(o);
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!("\n{chan:?}, {seeds} seeds; median (range):\n{HEADER}");
    for (k, (name, ..)) in scen.iter().enumerate() {
        println!("{}", row(&format!("{name}, today"), &out[2 * k]));
        println!("{}", row(&format!("{name}, lifeline"), &out[2 * k + 1]));
    }
}

/// The scenarios on the model channel. Today's runs call again once the
/// SNR is back (as an operator or a client that noticed would).
#[test]
#[ignore]
fn lifeline_table() {
    table(Chan::Model, env("ION_SEEDS").unwrap_or(8), &scenarios());
}

/// The same through the real PHYs on CCIR poor (`ION_PROFILE=awgn` for
/// AWGN), fewer seeds (`ION_SEEDS`, default 3), `ION_SCEN` to pick
/// scenarios by number.
#[test]
#[ignore]
fn lifeline_audio_table() {
    let profile = match std::env::var("ION_PROFILE").as_deref() {
        Ok("awgn") => Profile::Awgn,
        _ => Profile::CcirPoor,
    };
    let all = scenarios();
    let pick: Vec<usize> = std::env::var("ION_SCEN")
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|_| (0..all.len()).collect());
    let scen: Vec<Scenario> = pick.iter().map(|&i| all[i].clone()).collect();
    table(Chan::Audio(profile), env("ION_SEEDS").unwrap_or(3), &scen);
}

/// SNR wandering around the floor's threshold: how often the link goes
/// in and out of a hold, and what it moves, against today.
#[test]
#[ignore]
fn flapping_table() {
    let seeds: u64 = env("ION_SEEDS").unwrap_or(8);
    let mins: f64 = env("ION_MINUTES").unwrap_or(30.0);
    println!("\nModel channel, mean SNR wandering (σ, τ 60 s), {mins} min, {seeds} seeds; median (range):");
    println!("| Mean, σ dB | | holds (A) | resumes | bytes delivered | links ended |\n|---|---|---:|---:|---:|---:|");
    for (mean, sigma) in [(-11.0f32, 2.0f32), (-13.0, 2.0), (-14.0, 3.0), (-15.0, 2.0)] {
        for lifeline in [false, true] {
            let mut holds = Vec::new();
            let mut resumes = Vec::new();
            let mut bytes = Vec::new();
            let mut ended = 0;
            for seed in 0..seeds {
                let air: Arc<dyn Air> = Arc::new(ModelAir::default());
                let mk = |call: &str| {
                    let mut c = Config::new(call);
                    c.lifeline = lifeline;
                    tune(&mut c);
                    c
                };
                let a = Station::new(mk("N0ABC"), air.clone());
                let mut b = Station::new(mk("W1AW"), air.clone());
                b.listen();
                // Connect at +10 dB, then wander from 60 s.
                let m = |k: u64| {
                    let mut m = ModelMedium::new(mean, 1000 * seed + k);
                    m.fade_db = sigma;
                    m.fade_tau_s = 60.0;
                    m.schedule = vec![(0.0, 10.0), (60.0, mean)];
                    Box::new(m) as Box<dyn Medium>
                };
                let mut sim = Sim::new(a, b, Box::new(TwoWay([m(1), m(2)])), air);
                let mut got = 0u64;
                let mut ends = 0;
                sim.stations[0].connect("W1AW", 0.0);
                sim.stations[0].send(&payload(2_000_000, 3));
                while sim.t < mins * 60.0 {
                    sim.step();
                    got += sim.stations[1].take_received().len() as u64;
                    // Today: the operator calls again at once.
                    if matches!(sim.stations[1].state(), State::Failed | State::Disconnected) {
                        sim.stations[1].listen();
                        ends += 1;
                    }
                    if matches!(sim.stations[0].state(), State::Failed | State::Disconnected) {
                        sim.stations[0].connect("W1AW", sim.t);
                    }
                }
                holds.push(sim.stations[0].stats().holds as f64);
                resumes.push(sim.stations[0].stats().resumes as f64);
                bytes.push(got as f64);
                ended += ends;
            }
            println!(
                "| {mean}, {sigma} | {} | {} | {} | {} | {ended} |",
                if lifeline { "lifeline" } else { "today" },
                spread(&mut holds),
                spread(&mut resumes),
                spread(&mut bytes),
            );
        }
    }
}

/// Bursts a node sent (true) or heard, as its journal names them, with when.
type Journal = Vec<(f64, bool, String)>;

/// Two [`Node`]s on audio (real PHYs, CW IDs, carrier sense), the noise
/// following `snr_at` (dB against the steady carrier): each one's journal,
/// and the nodes.
fn nodes_through(every_s: f64, secs: f64, snr_at: impl Fn(f64) -> f32) -> (Vec<Journal>, [Node; 2]) {
    use ion_dsp::channel::Channel;
    const CHUNK: usize = 4800;
    let mut nodes = [
        Node::new(Node::config("N0JLF"), 0.2),
        Node::new(Node::config("W1AW"), 0.2),
    ];
    let mut chans = [
        Channel::from_profile(Profile::Awgn, 1),
        Channel::from_profile(Profile::Awgn, 2),
    ];
    let mut outq: [std::collections::VecDeque<f32>; 2] = Default::default();
    let mut noise = NoiseGen::new(5);
    let carrier = ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0;
    for n in &mut nodes {
        n.set_id_every(Some(every_s));
    }
    nodes[1].station_mut().listen();
    nodes[0].station_mut().connect("W1AW", 0.0);
    nodes[0].station_mut().send(&payload(200_000, 4));
    let mut journal = vec![Vec::new(), Vec::new()];
    let mut t = 0.0;
    while t < secs {
        t += CHUNK as f64 / AUDIO_RATE as f64;
        let mut played = [vec![0.0f32; CHUNK], vec![0.0f32; CHUNK]];
        for i in 0..2 {
            if outq[i].is_empty()
                && let Some(a) = nodes[i].poll(t)
            {
                outq[i].extend(a);
            }
            let n = outq[i].len().min(CHUNK);
            for (k, x) in outq[i].drain(..n).enumerate() {
                played[i][k] = x;
            }
            if n > 0 && outq[i].is_empty() {
                nodes[i].tx_done(t);
            }
        }
        let sigma = noise_sigma(carrier, snr_at(t), 2400.0, AUDIO_RATE);
        for i in 0..2 {
            let mut heard = vec![0.0; CHUNK];
            chans[1 - i].process(&played[1 - i], &mut heard);
            for x in &mut heard {
                *x += sigma * noise.gaussian();
            }
            nodes[i].push_audio(&heard, t);
            for b in nodes[i].take_journal() {
                journal[i].push((t, b.tx, b.what));
            }
        }
    }
    (journal, nodes)
}

#[test]
fn cw_ids_stay_on_time_through_a_hold() {
    // An ID every 100 s (the app's is 570 s) and a 6-minute fade under the
    // floor: each side IDs ahead of its first burst after every ID time,
    // and the hold probes right after each, so no ID comes late.
    let every = 100.0;
    let fade = (60.0, 420.0);
    let (journal, nodes) = nodes_through(every, 600.0, |t| if t >= fade.0 && t < fade.1 { -15.0 } else { 10.0 });
    let st = nodes[0].station();
    assert!(
        st.stats().holds >= 1 && st.stats().resumes >= 1,
        "{:?}\n{:?}",
        st.stats(),
        journal[0]
    );
    assert!(nodes.iter().all(|n| n.station().state() == State::Connected));
    let t0 = st.link_t0().expect("ID clock");
    for (k, j) in journal.iter().enumerate() {
        let ids: Vec<f64> = j.iter().filter(|e| e.1 && e.2 == "CW ID").map(|e| e.0).collect();
        eprintln!("side {k}: ID clock from {t0:.1} s, IDs at {ids:.1?}");
        // Every burst after an ID time has had its ID since that time.
        for &(t, tx, ref what) in j {
            if !tx || what == "CW ID" || t < t0 + every {
                continue;
            }
            let due = t0 + ((t - t0) / every).floor() * every;
            assert!(
                ids.iter().any(|&i| i >= due - 1.0 && i <= t),
                "side {k}: {what} at {t:.0} s without the ID due at {due:.0} s; IDs {ids:?}"
            );
        }
        // And in the hold, IDs follow one another at the ID time.
        let in_hold: Vec<&f64> = ids.iter().filter(|&&i| i > fade.0 + 60.0 && i < fade.1).collect();
        assert!(in_hold.len() >= 2, "side {k}: IDs {ids:?}");
        for w in in_hold.windows(2) {
            assert!(w[1] - w[0] <= every + 30.0, "side {k}: IDs {ids:?}");
        }
    }
}

/// What the lifeline's receiver costs: seconds of CPU per second of audio
/// for every receiver, with and without it, on noise and on a link's
/// bursts. `--ignored --nocapture`, on an idle machine.
#[test]
#[ignore]
fn receiver_cpu() {
    let mut g = NoiseGen::new(1);
    let noise: Vec<f32> = (0..60 * AUDIO_RATE as usize).map(|_| 0.05 * g.gaussian()).collect();
    let mut busy = Vec::new();
    for (k, b) in [
        tiny(FLOOR_WAVE, 1, 0),
        data(FLOOR_WAVE, 0, 1),
        data(Wave::Mfsk(ion::phy_mfsk::MfskMode::R1), 3, 6),
        data(Wave::Ofdm(Guard::Normal), 11, 12),
    ]
    .iter()
    .enumerate()
    {
        let a = transmit(b);
        busy.extend(a.iter().map(|x| x + 0.05 * g.gaussian()));
        busy.extend((0..AUDIO_RATE as usize / 2).map(|_| 0.05 * g.gaussian()));
        let _ = k;
    }
    for (name, audio) in [("noise", &noise), ("bursts", &busy)] {
        for with in [false, true] {
            let mut rx = if with {
                Receivers::new()
            } else {
                Receivers::without_lifeline()
            };
            let mut out = Vec::new();
            let t = std::time::Instant::now();
            for c in audio.chunks(4800) {
                rx.process(c, &mut out);
            }
            let cpu = t.elapsed().as_secs_f64() / (audio.len() as f64 / AUDIO_RATE as f64);
            println!(
                "{name}, lifeline {with}: {:.4} s CPU per s of audio, {} bursts",
                cpu,
                out.len()
            );
        }
    }
}
