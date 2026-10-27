//! Ion on IONOS SIM's channels (`ion_dsp::ionos`), measured the way the
//! Winlink comparison was (QEX March/April 2022, figures 11–13): net
//! throughput in bytes per minute over a long session at S:N 0 to 30 dB,
//! S:N in 3 kHz against a sine at each burst's peak (the box is levelled
//! on the modem's peaks and calibrated with a sine). Winlink compresses
//! its messages, so the payload here is incompressible and Ion's own
//! compression stays off.
//!
//! ```text
//! cargo test -p ion --release --test ionos -- --ignored --nocapture
//! ```
//!
//! `ION_IONOS_MODES` (default `WGN,MPG,MPP`), `ION_SNRS` (default
//! `0,5,...,30`), `ION_PATHS` (2 or 4, default 2), `ION_MINUTES` per
//! run (default 10), `ION_IONOS_REF=mean` (S:N against each burst's
//! mean power instead), `ION_SEEDS` (runs averaged per cell, default
//! 1), `ION_THREADS` (default 2), `ION_SMOOTH=0` / `ION_HOLD=0`
//! (without `Config::smooth_target` / `hold_by_goodput`),
//! `ION_POWER=1` (with `Config::power_aware`), `ION_START=floor`
//! (the first data burst at the floor, `Config::start_margin_db`),
//! `ION_LIFELINE=0` (without `Config::lifeline`), `ION_TRACE=1` (every
//! data burst's level).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::sim::{Airtime, Sim};
use ion::link::{Air, Station};
use ion_dsp::ionos::{Mode, Reference};

fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
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

/// Bytes delivered A → B in `secs` of a session (connect included), and
/// where the air time went.
fn session(mode: Mode, paths: usize, reference: Reference, snr_db: f32, secs: f64, seed: u64) -> (u64, Airtime) {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let cfg = |call: &str| {
        let mut c = audio::config(call);
        if std::env::var("ION_SMOOTH").is_ok_and(|v| v == "0") {
            c.smooth_target = false;
        }
        if std::env::var("ION_HOLD").is_ok_and(|v| v == "0") {
            c.hold_by_goodput = false;
        }
        if let Ok(v) = std::env::var("ION_POWER") {
            c.power_aware = v != "0";
        }
        if std::env::var("ION_START").is_ok_and(|v| v == "floor") {
            c.start_margin_db = None;
        }
        if std::env::var("ION_LIFELINE").is_ok_and(|v| v == "0") {
            c.lifeline = false;
        }
        c
    };
    let a = Station::new(cfg("N0JLF"), air.clone());
    let mut b = Station::new(cfg("N0CALL"), air.clone());
    b.listen();
    let medium = AudioMedium::ionos(mode, paths, reference, snr_db, seed);
    let mut sim = Sim::new(a, b, Box::new(medium), air);
    sim.stations[0].connect("N0CALL", 0.0);
    // More than the top level could move in the time.
    sim.stations[0].send(&payload((secs * 1_500.0) as usize + 10_000, seed as u32));
    sim.run_until(secs, |_| false);
    // Every data burst, its level (target) and frames delivered / sent.
    if std::env::var("ION_TRACE").is_ok_and(|v| v == "1") {
        let st = sim.stations[0].stats();
        let data: Vec<_> = sim
            .log
            .iter()
            .filter(|e| e.from == 0 && e.code < ion::level::LADDER.len())
            .collect();
        let line: Vec<String> = st
            .levels
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let t = st.classes.get(i).map_or(0, |c| c.1);
                let e = data
                    .get(i)
                    .map_or("?".to_string(), |e| format!("{}/{}", e.delivered, e.frames));
                format!("L{l}(t{t}) {e}")
            })
            .collect();
        eprintln!("{} {snr_db:+} dB seed {seed}: {}", mode.name(), line.join(", "));
    }
    let holds = sim.stations[0].stats().holds + sim.stations[1].stats().holds;
    if holds > 0 {
        eprintln!("{} {snr_db:+} dB seed {seed}: {holds} holds", mode.name());
    }
    (sim.stations[1].stats().bytes_received, sim.airtime())
}

#[test]
#[ignore]
fn ionos_table() {
    let modes: Vec<Mode> = std::env::var("ION_IONOS_MODES")
        .unwrap_or_else(|_| "WGN,MPG,MPP".into())
        .split(',')
        .map(|m| {
            *Mode::ALL
                .iter()
                .find(|x| x.name().eq_ignore_ascii_case(m.trim()))
                .expect("mode")
        })
        .collect();
    let snrs: Vec<i32> = std::env::var("ION_SNRS")
        .map(|v| v.split(',').map(|s| s.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| (0..=30).step_by(5).collect());
    let paths: usize = env("ION_PATHS").unwrap_or(2);
    let minutes: f64 = env("ION_MINUTES").unwrap_or(10.0);
    let seeds: u64 = env("ION_SEEDS").unwrap_or(1);
    let threads: usize = env("ION_THREADS").unwrap_or(2);
    let reference = match std::env::var("ION_IONOS_REF").as_deref() {
        Ok("mean") => Reference::Mean,
        _ => Reference::Peak,
    };
    let jobs: Vec<(usize, usize, u64)> = (0..snrs.len())
        .flat_map(|s| (0..modes.len()).flat_map(move |m| (0..seeds).map(move |k| (s, m, k))))
        .collect();
    let cells = Mutex::new(vec![vec![Vec::new(); modes.len()]; snrs.len()]);
    // Air time per (S:N, mode), for the breakdown at low S:N.
    let air = Mutex::new(vec![vec![Airtime::default(); modes.len()]; snrs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(k, m, seed)) = jobs.get(i) else { break };
                    let snr = snrs[k];
                    let (bytes, at) = session(
                        modes[m],
                        paths,
                        reference,
                        snr as f32,
                        minutes * 60.0,
                        900 + snr as u64 + 1000 * seed,
                    );
                    air.lock().unwrap()[k][m].add(&at);
                    let bpm = bytes as f64 / minutes;
                    eprintln!("{} {snr:+} dB seed {seed}: {bytes} B, {bpm:.0} B/min", modes[m].name());
                    cells.lock().unwrap()[k][m].push(bpm);
                }
            });
        }
    });
    let cells = cells.into_inner().unwrap();
    println!(
        "\nIon on IONOS SIM, {paths} paths, S:N in 3 kHz against {}; {minutes} min per run, bytes/min (bit/s):",
        if reference == Reference::Peak {
            "a sine at each burst's peak"
        } else {
            "each burst's mean power"
        }
    );
    println!(
        "| S:N dB | {} |",
        modes.iter().map(|m| m.name()).collect::<Vec<_>>().join(" | ")
    );
    println!("|---:|{}", "---:|".repeat(modes.len()));
    for (k, snr) in snrs.iter().enumerate().rev() {
        let row: Vec<String> = cells[k]
            .iter()
            .map(|v| {
                let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
                format!("{mean:.0} ({:.0})", mean * 8.0 / 60.0)
            })
            .collect();
        println!("| {snr} | {} |", row.join(" | "));
    }
    // Where the air time went at the low end (ION_AIR_MAX, default 5 dB).
    let air_max: i32 = env("ION_AIR_MAX").unwrap_or(5);
    let air = air.into_inner().unwrap();
    for (k, snr) in snrs.iter().enumerate().filter(|(_, s)| **s <= air_max) {
        for (m, mode) in modes.iter().enumerate() {
            println!("\nAir time, {} {snr} dB:\n{}", mode.name(), air[k][m].table());
        }
    }
}
