//! Ion through ham QRM on IONOS SIM: another station over part of our
//! passband at the receiving end, the way 7.101 goes on a busy afternoon.
//! A → B for `ION_MINUTES` (default 5) per run.
//!
//! `cargo test --release -p ion --test qrm -- --ignored --nocapture`
//!
//! `ION_IONOS_MODES` (default `WGN,MPP`), `ION_SNRS` (default `16`),
//! `ION_QRM_DB` (QRM over a steady tone at our peak, default `-20,-10,0`),
//! `ION_QRM` (`lowmid` 300–1850 Hz, the default; `low` 300–1050;
//! `mid` 1150–1850; `none`, the receivers hearing the silences; `skip`,
//! the medium skipping them), `ION_QRM_KIND` (`steady` or `voice`),
//! `ION_QRM_KEY` (`on`, the default, or `period:on` in seconds, from
//! `ION_QRM_START`), `ION_CLEAR_S` (`Config::qrm_clear_s`),
//! `ION_NARROW_BURST` (`Config::narrow_burst_s`),
//! `ION_SLOT` (B's third for slice bursts, `Config::slot_pick`),
//! `ION_QRM=move` (the QRM moves every `ION_QRM_HOP` s, default 120: low
//! and middle thirds, middle and high, low and high), `ION_QRM_WAV` (a
//! recording of real signals as the QRM, looped, at `ION_QRM_DB` while it
//! sounds),
//! `ION_QRM_START` (seconds: the QRM comes on then, and the run's minutes
//! and bytes count from then), `ION_QRM_STOP` (seconds: and goes off),
//! `ION_VARIANTS` (`gear`; `old` (without narrowing for QRM); `slice`
//! (level 6 pinned, B asking for the clean third above); `meter` (level 6
//! pinned, the meter's pick); `narrow` (B narrowed by hand); `oldtx` (A
//! without narrowing, as an older station); `oldrx` (B without it);
//! `stale` (`gear` without `Config::harq_fresh_on_move`); `quick` (`gear`
//! with `Config::qrm_quick`); `trial` (with `Config::qrm_trial`); `all` (the
//! two); default
//! `gear,slice,meter`), `ION_SEEDS` (from seed number `ION_SEED0`),
//! `ION_THREADS` (default 2),
//! `ION_TRACE=1` (each run's levels), `ION_QRM_JITTER` (seconds: each
//! seed's QRM starts up to this much after `ION_QRM_START`, so that it
//! lands anywhere in a burst and its ACK), `ION_LABEL` (a name on each
//! run's `RUN` line), `ION_QRM_AT` (`b`, the default; `a`, the sending
//! end; `both`), `ION_TWO_WAY=1` (B sends as much to A), `ION_HEAR_ALL=1`
//! (an end without QRM hears the silences too; the medium skips them
//! otherwise).
//!
//! Each run also prints a `RUN` line: the longest time nothing was
//! delivered, the sender's lifeline holds, both ends' states, and for each
//! change of the QRM how long until the data bursts moved (`det`), from
//! then until a frame was delivered (`stall`), and whether the burst the
//! change caught delivered anything; `back`, the bytes B sent A; `narB`
//! and `narA`, when each end narrowed (`+`) and widened (`-`); `nsec` and
//! `nbytes`, how long B was narrowed and what it took in meanwhile.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::link::audio::{AudioMedium, PhyAir};
use ion::link::sim::Sim;
use ion::link::{Air, Station};
use ion_dsp::ionos::{Mode, Reference};
use ion_dsp::qrm::{Qrm, QrmKind};

fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

fn payload(n: usize, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as u8
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct Run {
    mode: Mode,
    snr_db: f32,
    qrm_db: f32,
    variant: &'static str,
    seed: u64,
}

/// The QRM band, Hz, and dB added to `ION_QRM_DB`.
fn band() -> Option<(f32, f32, f32)> {
    match std::env::var("ION_QRM").as_deref() {
        // No QRM, but the receivers still hear the silences, as on the
        // air (the medium skips them otherwise).
        Ok("none") => Some((300.0, 1850.0, -200.0)),
        // No QRM, and the medium skips the silences between bursts.
        Ok("skip") => None,
        Ok("low") => Some((300.0, 1050.0, 0.0)),
        Ok("mid") => Some((1150.0, 1850.0, 0.0)),
        _ => Some((300.0, 1850.0, 0.0)),
    }
}

/// This seed's share of `ION_QRM_JITTER`, s.
fn jitter(seed: u64) -> f64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    env::<f64>("ION_QRM_JITTER").unwrap_or(0.0) * ((z ^ (z >> 31)) % 10_000) as f64 / 10_000.0
}

/// When the QRM comes on: `ION_QRM_START` and this seed's jitter.
fn qrm_start(seed: u64) -> Option<f64> {
    env::<f64>("ION_QRM_START").map(|t| t + jitter(seed))
}

fn session(r: Run, secs: f64) -> (u64, String, String) {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let cfg = |call: &str, rx: bool| {
        let mut c = ion::link::Config::new(call);
        if matches!(r.variant, "slice" | "meter") {
            c.pin_level = Some(6);
            c.stream = false;
        }
        if rx && r.variant == "slice" {
            c.slot_pick = Some(2);
        }
        if rx && r.variant == "narrow" {
            c.narrow = true;
        }
        if rx && let Some(k) = env::<u8>("ION_SLOT") {
            c.slot_pick = Some(k);
        }
        if r.variant == "old" {
            c.qrm_narrow = false;
        }
        if r.variant == "stale" {
            c.harq_fresh_on_move = false;
        }
        if matches!(r.variant, "quick" | "all") {
            c.qrm_quick = true;
        }
        if matches!(r.variant, "trial" | "all") {
            c.qrm_trial = true;
        }
        if let Some(s) = env("ION_CLEAR_S") {
            c.qrm_clear_s = s;
        }
        if let Some(s) = env("ION_NARROW_BURST") {
            c.narrow_burst_s = Some(s);
        }
        // A sender without narrowing (an older station) to a receiver with it.
        if !rx && r.variant == "oldtx" {
            c.qrm_narrow = false;
        }
        if rx && r.variant == "oldrx" {
            c.qrm_narrow = false;
        }
        c
    };
    let a = Station::new(cfg("N0JLF", false), air.clone());
    let mut b = Station::new(cfg("N0CALL", true), air.clone());
    b.listen();
    let mut medium = AudioMedium::ionos(r.mode, 2, Reference::Peak, r.snr_db, r.seed);
    let sides: &[usize] = match std::env::var("ION_QRM_AT").as_deref() {
        Ok("a") => &[0],
        Ok("both") => &[1, 0],
        _ => &[1],
    };
    let start = qrm_start(r.seed).unwrap_or(0.0);
    // When the QRM changes: comes on, moves, goes off.
    let mut changes = Vec::new();
    if std::env::var("ION_QRM").as_deref() == Ok("move") {
        // Low and middle thirds, middle and high, low and high, in turn,
        // each for `ION_QRM_HOP` s, at one power per Hz.
        let hop = env::<f64>("ION_QRM_HOP").unwrap_or(120.0);
        let phases: [&[(f32, f32)]; 3] = [
            &[(300.0, 1850.0)],
            &[(1150.0, 2700.0)],
            &[(300.0, 1050.0), (1950.0, 2700.0)],
        ];
        let mut t = start;
        let mut i = 0;
        while t < start + secs {
            for &(lo, hi) in phases[i % 3] {
                for &k in sides {
                    let seed = r.seed ^ 0x0BAD ^ i as u64 ^ (1 - k as u64) << 20;
                    let q = Qrm::new(lo, hi, QrmKind::Steady, seed).keyed(1e6, hop, 1e6 - t);
                    medium = medium.with_qrm(k, q, r.qrm_db + 10.0 * ((hi - lo) / 1550.0).log10());
                }
            }
            changes.push(t);
            t += hop;
            i += 1;
        }
    } else if let Ok(path) = std::env::var("ION_QRM_WAV") {
        // A recording of real signals, looped, from `ION_QRM_START`.
        let x = ion::sounding::read_wav(&std::fs::read(&path).expect("ION_QRM_WAV")).expect("WAV");
        changes.push(start);
        for &k in sides {
            let q = Qrm::recording(x.clone()).keyed(1e6, 1e6 - start, 1e6 - start);
            medium = medium.with_qrm(k, q, r.qrm_db);
        }
    } else if let Some((lo, hi, extra_db)) = band() {
        let kind = match std::env::var("ION_QRM_KIND").as_deref() {
            Ok("voice") => QrmKind::Voice,
            _ => QrmKind::Steady,
        };
        for &side in sides {
            let mut q = Qrm::new(lo, hi, kind, r.seed ^ 0x0BAD ^ (1 - side as u64) << 20);
            // Keyed (`period:on`), from `ION_QRM_START` if given.
            if let Ok(k) = std::env::var("ION_QRM_KEY")
                && let Some((p, on)) = k.split_once(':')
            {
                let (p, on): (f64, f64) = (p.parse().unwrap(), on.parse().unwrap());
                q = q.keyed(p, on, (p - start % p) % p).from(start);
            }
            if let Some(t0) = qrm_start(r.seed).filter(|_| std::env::var("ION_QRM_KEY").is_err()) {
                // On at t0, and off again at `ION_QRM_STOP`.
                let on = env::<f64>("ION_QRM_STOP").map_or(1e6 - t0, |t1| t1 - t0);
                q = q.keyed(1e6, on, 1e6 - t0);
                if extra_db > -100.0 && side == sides[0] {
                    changes.push(t0);
                    changes.extend(env::<f64>("ION_QRM_STOP"));
                }
            }
            medium = medium.with_qrm(side, q, r.qrm_db + extra_db);
        }
    }
    // The ends without QRM hear the silences too, as on the air.
    if std::env::var("ION_HEAR_ALL").is_ok_and(|v| v == "1") {
        for k in (0..2).filter(|k| !sides.contains(k)) {
            medium = medium.with_qrm(k, Qrm::new(300.0, 1850.0, QrmKind::Steady, r.seed ^ 0x51), -200.0);
        }
    }
    let mut sim = Sim::new(a, b, Box::new(medium), air);
    sim.stations[0].connect("N0CALL", 0.0);
    sim.stations[0].send(&payload((secs * 1_500.0) as usize + 10_000, r.seed as u32));
    if std::env::var("ION_TWO_WAY").is_ok_and(|v| v == "1") {
        sim.stations[1].send(&payload((secs * 1_500.0) as usize + 10_000, r.seed as u32 ^ 0x55));
    }
    // With a start time, only what moves once the QRM is on counts.
    let t0 = start.min(secs);
    sim.run_until(t0, |_| false);
    let before = sim.stations[1].stats().bytes_received;
    let back_before = sim.stations[0].stats().bytes_received;
    let mut nar: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    let mut was = [false; 2];
    let (mut nsec, mut nbytes) = (0f64, 0u64);
    // B's bytes every half second.
    let mut line = vec![(t0, before)];
    let mut t = t0;
    while t < t0 + secs {
        let dt = (t + 0.5).min(t0 + secs) - t;
        t += dt;
        sim.run_until(t, |_| false);
        let got = sim.stations[1].stats().bytes_received;
        if was[1] {
            nsec += dt;
            nbytes += got - line.last().map_or(before, |s| s.1);
        }
        line.push((t, got));
        for k in 0..2 {
            let on = sim.stations[k].narrow_state().0;
            if on != was[k] {
                nar[k].push(format!("{}{t:.0}", if on { '+' } else { '-' }));
                was[k] = on;
            }
        }
    }
    if std::env::var("ION_EVENTS").is_ok_and(|v| v == "1") {
        for e in &sim.log {
            eprintln!(
                "  {:7.1}-{:7.1} {} {:?}{} code {} frames {} {}",
                e.start,
                e.end,
                if e.from == 0 { "A>" } else { "<B" },
                e.wave,
                e.slot.map_or(String::new(), |k| format!(" slot {k}")),
                e.code,
                e.frames,
                if e.delivered { "ok" } else { "LOST" }
            );
        }
        eprintln!(
            "  slots A {:?} B {:?}; narrow A {:?} B {:?}",
            sim.stations[0].slots(),
            sim.stations[1].slots(),
            sim.stations[0].narrow_state(),
            sim.stations[1].narrow_state()
        );
    }
    let st = sim.stations[0].stats();
    let mut levels = std::collections::BTreeMap::new();
    for l in &st.levels {
        *levels.entry(*l).or_insert(0) += 1;
    }
    let trace = format!(
        "{} | narrowed {} search polls {}",
        levels
            .iter()
            .map(|(l, n)| format!("L{l}×{n}"))
            .collect::<Vec<_>>()
            .join(" "),
        sim.stations[1].stats().narrowings,
        st.search_polls
    );
    let bytes_at = |t: f64| line.iter().rev().find(|s| s.0 <= t).map_or(before, |s| s.1);
    let next_delivery = |t: f64| {
        let b = bytes_at(t);
        line.iter().find(|s| s.0 > t && s.1 > b).map(|s| s.0)
    };
    let mut gap = 0f64;
    let mut last = t0;
    for w in line.windows(2) {
        if w[1].1 > w[0].1 {
            gap = gap.max(w[1].0 - last);
            last = w[1].0;
        }
    }
    gap = gap.max(t0 + secs - last);
    let data: Vec<_> = sim
        .log
        .iter()
        .filter(|e| e.from == 0 && e.code < ion::level::LADDER.len())
        .collect();
    let num = |x: Option<f64>| x.map_or("-".to_string(), |v| format!("{v:.1}"));
    let hops: Vec<String> = changes
        .iter()
        .filter(|&&c| c >= t0 && c < t0 + secs)
        .map(|&c| {
            let was = data.iter().rev().find(|e| e.start < c);
            let caught = was.filter(|e| e.end > c).map(|e| bytes_at(e.end + 1.0) > bytes_at(c));
            let moved = data
                .iter()
                .find(|e| e.start >= c && was.is_some_and(|w| (w.wave, w.slot) != (e.wave, e.slot)))
                .map(|e| e.start);
            let stall = moved.and_then(|m| next_delivery(m).map(|d| d - m));
            format!(
                "{}/{}/{}/{}",
                num(moved.map(|m| m - c)),
                num(stall),
                num(next_delivery(was.map_or(c, |e| e.end.max(c)) + 1.0).map(|d| d - c)),
                caught.map_or("-", |ok| if ok { "y" } else { "n" })
            )
        })
        .collect();
    let run = format!(
        "narrowed={} polls={} maxgap={gap:.1} holds={} hold_s={:.0} dropped={} state={:?}/{:?} hops={} back={} narB={} narA={} nsec={nsec:.0} nbytes={nbytes}",
        sim.stations[1].stats().narrowings,
        st.search_polls,
        st.holds,
        st.hold_s,
        sim.stations[1].stats().harq_dropped,
        sim.stations[0].state(),
        sim.stations[1].state(),
        if hops.is_empty() { "-".into() } else { hops.join(";") },
        sim.stations[0].stats().bytes_received - back_before,
        if nar[1].is_empty() {
            "-".into()
        } else {
            nar[1].join(",")
        },
        if nar[0].is_empty() {
            "-".into()
        } else {
            nar[0].join(",")
        }
    );
    (sim.stations[1].stats().bytes_received - before, trace, run)
}

#[test]
#[ignore]
fn qrm_table() {
    let list = |k: &str, d: &str| -> Vec<String> {
        std::env::var(k)
            .unwrap_or_else(|_| d.into())
            .split(',')
            .map(|s| s.trim().to_string())
            .collect()
    };
    let modes: Vec<Mode> = list("ION_IONOS_MODES", "WGN,MPP")
        .iter()
        .map(|m| {
            *Mode::ALL
                .iter()
                .find(|x| x.name().eq_ignore_ascii_case(m))
                .expect("mode")
        })
        .collect();
    let snrs: Vec<f32> = list("ION_SNRS", "16").iter().map(|s| s.parse().unwrap()).collect();
    let qrms: Vec<f32> = list("ION_QRM_DB", "-20,-10,0")
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    let variants: Vec<&'static str> = list("ION_VARIANTS", "gear,slice,meter")
        .iter()
        .map(|v| match v.as_str() {
            "gear" => "gear",
            "slice" => "slice",
            "meter" => "meter",
            "narrow" => "narrow",
            "old" => "old",
            "stale" => "stale",
            "oldtx" => "oldtx",
            "oldrx" => "oldrx",
            "quick" => "quick",
            "trial" => "trial",
            "all" => "all",
            _ => panic!("variant {v}"),
        })
        .collect();
    let minutes: f64 = env("ION_MINUTES").unwrap_or(5.0);
    let seeds: u64 = env("ION_SEEDS").unwrap_or(1);
    let seed0: u64 = env("ION_SEED0").unwrap_or(0);
    let threads: usize = env("ION_THREADS").unwrap_or(2);
    let trace = std::env::var("ION_TRACE").is_ok_and(|v| v == "1");
    let mut jobs = Vec::new();
    for &mode in &modes {
        for &snr_db in &snrs {
            for &qrm_db in &qrms {
                for &variant in &variants {
                    for k in seed0..seed0 + seeds {
                        jobs.push(Run {
                            mode,
                            snr_db,
                            qrm_db,
                            variant,
                            seed: 700 + 1000 * k + snr_db as u64,
                        });
                    }
                }
            }
        }
    }
    let out = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&r) = jobs.get(i) else { break };
                    let (bytes, levels, run) = session(r, minutes * 60.0);
                    let bps = bytes as f64 * 8.0 / (minutes * 60.0);
                    eprintln!(
                        "{} {:+} dB QRM {:+} {}: {bytes} B, {bps:.0} bit/s{}",
                        r.mode.name(),
                        r.snr_db,
                        r.qrm_db,
                        r.variant,
                        if trace { format!(" [{levels}]") } else { String::new() }
                    );
                    eprintln!(
                        "RUN label={} mode={} snr={} qrm={} variant={} seed={} bytes={bytes} bps={bps:.1} {run}",
                        std::env::var("ION_LABEL").unwrap_or_else(|_| "-".into()),
                        r.mode.name(),
                        r.snr_db,
                        r.qrm_db,
                        r.variant,
                        r.seed
                    );
                    out.lock().unwrap().push((r, bps));
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!(
        "\nIon through QRM {:?} at B, IONOS SIM, {minutes} min per run, bit/s (mean of {seeds} seeds):",
        band()
    );
    println!("| mode | S:N | QRM dB | {} |", variants.join(" | "));
    println!("|---|---:|---:|{}", "---:|".repeat(variants.len()));
    for &mode in &modes {
        for &snr in &snrs {
            for &q in &qrms {
                let cell = |v: &str| {
                    let x: Vec<f64> = out
                        .iter()
                        .filter(|(r, _)| r.mode == mode && r.snr_db == snr && r.qrm_db == q && r.variant == v)
                        .map(|(_, b)| *b)
                        .collect();
                    format!("{:.0}", x.iter().sum::<f64>() / x.len().max(1) as f64)
                };
                let row: Vec<String> = variants.iter().map(|v| cell(v)).collect();
                println!("| {} | {snr:+} | {q:+} | {} |", mode.name(), row.join(" | "));
            }
        }
    }
}

/// One QPSK 1/2 slice codeword in the third above (2300 Hz) at S:N +16 dB
/// (2.4 kHz, against the burst), through QRM over 300–1850 Hz at each of
/// `ION_QRM_DB` over a steady tone at our peak: header and codeword, with
/// the blanker on and off.
#[test]
#[ignore]
fn slice_burst_through_qrm() {
    use ion::fec::{self, Rate};
    use ion::phy::{Constellation, Numerology, PhyRx, Pilots, TxOptions, modulate_full};
    use ion_dsp::channel::add_awgn;
    use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};
    let qrms: Vec<f32> = std::env::var("ION_QRM_DB")
        .unwrap_or_else(|_| "-20,-10,0,10,20".into())
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    for q_db in qrms {
        for blanker in [true, false] {
            let mut hdr = 0;
            let mut ok = 0;
            for seed in 0..6u64 {
                let info: Vec<u8> = payload(fec::K, seed as u32 + 1).iter().map(|b| b & 1).collect();
                let coded = fec::encode(Rate::R1_2, &info);
                let user = 0x4000 + seed as u16;
                let o = TxOptions {
                    clip_db: Some(4.5),
                    pilots: Pilots::Robust,
                    ..TxOptions::default()
                };
                let burst = modulate_full(Numerology::Slice, Constellation::Qpsk, user, 0, &coded, 2300.0, &o);
                let p = burst.iter().map(|v| v * v).sum::<f32>() / burst.len() as f32;
                let mut a = vec![0f32; 24_000];
                a.extend_from_slice(&burst);
                a.extend(std::iter::repeat_n(0.0, 24_000));
                add_awgn(
                    &mut a,
                    noise_sigma(p, 16.0, 2400.0, AUDIO_RATE),
                    &mut NoiseGen::new(seed + 9),
                );
                let peak = ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0;
                Qrm::new(300.0, 1850.0, QrmKind::Steady, seed + 99).add(&mut a, 0, peak * 10f32.powf(q_db / 10.0));
                let mut rx = PhyRx::with_numerology(Numerology::Slice, 2300.0);
                rx.set_blanker(blanker);
                let mut out = Vec::new();
                for c in a.chunks(4800) {
                    rx.process(c, &mut out);
                }
                if let Some(b) = out.iter().find(|b| b.user == user) {
                    hdr += 1;
                    ok += fec::decode(Rate::R1_2, &b.llrs[..Rate::R1_2.n()]).is_some_and(|d| d == info) as usize;
                }
            }
            println!(
                "QRM {q_db:+} dB, blanker {}: headers {hdr}/6, codewords {ok}/6",
                if blanker { "on" } else { "off" }
            );
        }
    }
}
