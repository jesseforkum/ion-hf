//! The stream gear on the link (`docs/link.md`, Stream gear): agreed at
//! connect (`frame::caps::STREAM`), used for big transfers at low S:N,
//! back to ARQ after each block; a station without it (Ion HF 0.1.0-alpha.5
//! and before) gets plain ARQ.

use std::sync::Arc;

use ion::link::audio::{AudioMedium, PhyAir};
use ion::link::frame::{Frame, Kind, caps, flags};
use ion::link::sim::{ModelAir, ModelMedium, Sim};
use ion::link::stream::{STREAM, TONES};
use ion::link::{Air, Config, Outgoing, State, Station};
use ion_dsp::ionos::{Mode, Reference};

fn payload(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn model(snr_db: f32, stream_a: bool, stream_b: bool, seed: u64) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let cfg = |call: &str, on: bool| Config {
        stream: on,
        ..Config::new(call)
    };
    let a = Station::new(cfg("N0JLF", stream_a), air.clone());
    let mut b = Station::new(cfg("N0CALL", stream_b), air.clone());
    b.listen();
    let mut sim = Sim::new(a, b, Box::new(ModelMedium::new(snr_db, seed)), air);
    sim.stations[0].connect("N0CALL", 0.0);
    sim
}

#[allow(dead_code)]
fn dump(sim: &Sim) {
    for e in &sim.log {
        eprintln!(
            "{:8.2} {:8.2} {} code {} {:?} frames {} {}",
            e.start, e.end, e.from, e.code, e.wave, e.frames, e.delivered
        );
    }
}

fn periods(sim: &Sim) -> usize {
    sim.log.iter().filter(|e| e.code == STREAM).count()
}

#[test]
fn a_big_transfer_streams_and_arrives_whole() {
    let data = payload(20_000, 1);
    let mut sim = model(0.0, true, true, 3);
    sim.stations[0].send(&data);
    let done = sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= data.len() as u64);
    assert!(
        done,
        "{} of {} in {:.0} s",
        sim.stations[1].stats().bytes_received,
        data.len(),
        sim.t
    );
    assert_eq!(sim.stations[0].caps() & caps::STREAM, caps::STREAM);
    assert_eq!(sim.stations[1].take_received(), data);
    assert!(periods(&sim) > 0);
    assert!(
        sim.stations[1].stats().stream_blocks >= 2,
        "{:?}",
        sim.stations[1].stats().stream_blocks
    );
    // Both sides back on the link, connected.
    let more = sim.t + 300.0;
    sim.run_until(more, |s| s.stations[0].stats().bytes_acked >= data.len() as u64);
    assert_eq!(sim.stations[0].state(), State::Connected);
    assert_eq!(sim.stations[0].stats().bytes_acked, data.len() as u64);
}

#[test]
fn a_station_without_the_stream_gets_plain_arq() {
    for (a, b) in [(true, false), (false, true)] {
        let data = payload(8_000, 2);
        let mut sim = model(0.0, a, b, 5);
        sim.stations[0].send(&data);
        let done = sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= data.len() as u64);
        assert!(done);
        assert_eq!(sim.stations[0].caps() & caps::STREAM, 0);
        assert_eq!(sim.stations[1].caps() & caps::STREAM, 0);
        assert_eq!(periods(&sim), 0);
        assert!(sim.log.iter().all(|e| e.code != TONES));
        assert_eq!(sim.stations[1].take_received(), data);
    }
}

#[test]
fn small_transfers_and_high_snr_stay_on_arq() {
    let mut sim = model(0.0, true, true, 6);
    sim.stations[0].send(&payload(3_000, 3));
    assert!(sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= 3_000));
    assert_eq!(periods(&sim), 0);
    let mut sim = model(25.0, true, true, 6);
    sim.stations[0].send(&payload(30_000, 3));
    assert!(sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= 30_000));
    assert_eq!(periods(&sim), 0);
}

/// The ConnReq's caps with the gear off are what alpha.5 sent (CUE only),
/// and with it on one bit more.
#[test]
fn the_conn_req_without_the_stream_is_alpha5s() {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let req = |on: bool| -> Outgoing {
        let mut s = Station::new(
            Config {
                stream: on,
                ..Config::new("N0JLF")
            },
            air.clone(),
        );
        s.connect("N0CALL", 0.0);
        s.poll_tx(0.0).unwrap()
    };
    let flags_of = |b: &Outgoing| Frame::decode(&b.frames[0]).unwrap();
    let (off, on) = (req(false), req(true));
    let (f_off, f_on) = (flags_of(&off), flags_of(&on));
    assert_eq!(f_off.kind, Kind::ConnReq);
    assert_eq!(f_off.flags, caps::CUE << flags::CAPS_SHIFT);
    assert_eq!(f_on.flags, f_off.flags | caps::STREAM << flags::CAPS_SHIFT);
    assert_eq!(f_on.payload, f_off.payload);
    assert_eq!((off.wave, off.code, off.user), (on.wave, on.code, on.user));
}

#[test]
fn the_receiver_with_data_gets_the_turn_after_the_block() {
    let data = payload(12_000, 4);
    let mut sim = model(0.0, true, true, 8);
    sim.stations[0].send(&data);
    sim.stations[1].send(b"reply from the other side\n");
    let done = sim.run_until(3_600.0, |s| {
        s.stations[1].stats().bytes_received >= data.len() as u64 && s.stations[0].stats().bytes_received >= 26
    });
    if !done {
        dump(&sim);
    }
    assert!(
        done,
        "a got {}, b got {}",
        sim.stations[0].stats().bytes_received,
        sim.stations[1].stats().bytes_received
    );
    assert_eq!(sim.stations[1].take_received(), data);
}

#[test]
fn a_cw_id_ends_the_block_and_it_goes_on_after() {
    let data = payload(16_000, 9);
    let mut sim = model(-3.0, true, true, 12);
    for s in &mut sim.stations {
        s.set_ids(120.0, 3.0);
    }
    sim.stations[0].send(&data);
    let done = sim.run_until(7_200.0, |s| s.stations[1].stats().bytes_received >= data.len() as u64);
    assert!(done, "{} of {}", sim.stations[1].stats().bytes_received, data.len());
    assert_eq!(sim.stations[1].take_received(), data);
    assert!(periods(&sim) > 20);
    // No period or tone answer runs across an ID time.
    let t0 = sim.stations[0].link_t0().unwrap();
    for e in sim.log.iter().filter(|e| e.code == STREAM || e.code == TONES) {
        let n = ((e.end - t0) / 120.0).floor();
        let due = t0 + n * 120.0;
        assert!(n < 1.0 || due <= e.start || due >= e.end, "{e:?} across {due}");
    }
}

/// Real PHYs on IONOS SIM: the timekeeping finds the periods, the tones
/// are heard, the block arrives whole.
fn ionos(mode: Mode, snr_db: f32, n: usize, seed: u64) -> Sim {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    // ION_STREAM=0: the same link without the gear (alpha.5's ARQ).
    let on = std::env::var("ION_STREAM").map_or(true, |v| v != "0");
    let cfg = |call: &str| Config {
        stream: on,
        ..Config::new(call)
    };
    let a = Station::new(cfg("N0JLF"), air.clone());
    let mut b = Station::new(cfg("N0CALL"), air.clone());
    b.listen();
    let medium = AudioMedium::ionos(mode, 2, Reference::Peak, snr_db, seed);
    let mut sim = Sim::new(a, b, Box::new(medium), air);
    sim.stations[0].connect("N0CALL", 0.0);
    sim.stations[0].send(&payload(n, seed as u32));
    sim
}

#[test]
fn on_ionos_at_0_db_the_stream_delivers() {
    let n = 6_000;
    let mut sim = ionos(Mode::Wgn, 0.0, n, 11);
    let done = sim.run_until(1_200.0, |s| s.stations[1].stats().bytes_received >= n as u64);
    if std::env::var("ION_DUMP").is_ok() {
        dump(&sim);
        for p in &sim.stations[1].stats().stream_log {
            eprintln!(
                "{:<9} {}/{} {:+.1} dB -> {}",
                ion::link::stream::OPTS[p.0 as usize].name,
                p.1,
                p.2,
                p.3,
                p.4
            );
        }
    }
    assert!(
        done,
        "{} of {n} in {:.0} s",
        sim.stations[1].stats().bytes_received,
        sim.t
    );
    assert_eq!(sim.stations[1].take_received(), payload(n, 11));
    assert!(periods(&sim) > 3);
    assert!(sim.stations[1].stats().stream_blocks >= 1);
}

/// Two `Node`s on one continuous audio timeline (as the app and its
/// loopback peer): the sender's tone watch, the receiver's clock across
/// its own answers, the timekeeping without a simulator's help.
#[test]
fn two_nodes_stream_over_continuous_audio() {
    use ion::link::Node;
    use ion_dsp::dsp::{NoiseGen, noise_sigma};
    const CHUNK: usize = 960;
    let n = 9_000;
    let data = payload(n, 21);
    let mut nodes = [
        Node::new(Node::config("N0JLF"), 0.2),
        Node::new(Node::config("N0CALL"), 0.2),
    ];
    nodes[1].station_mut().listen();
    nodes[0].station_mut().connect("N0CALL", 0.0);
    nodes[0].station_mut().send(&data);
    let mut out: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let mut noise = NoiseGen::new(5);
    // About 0 dB in 2.4 kHz against a steady carrier at TX_LEVEL.
    let sigma = noise_sigma(ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0, 0.0, 2400.0, 48_000.0);
    let mut t = 0.0;
    let mut tones = 0;
    while t < 1_500.0 && nodes[1].station().stats().bytes_received < n as u64 {
        t += CHUNK as f64 / 48_000.0;
        let mut played = [vec![0.0f32; CHUNK], vec![0.0f32; CHUNK]];
        for (i, node) in nodes.iter_mut().enumerate() {
            if out[i].is_empty()
                && let Some(a) = node.poll(t)
            {
                out[i] = a;
            }
            let k = out[i].len().min(CHUNK);
            played[i][..k].copy_from_slice(&out[i][..k]);
            out[i].drain(..k);
            if k > 0 && out[i].is_empty() {
                node.tx_done(t);
            }
        }
        for i in 0..2 {
            let heard: Vec<f32> = played[1 - i].iter().map(|x| x + sigma * noise.gaussian()).collect();
            nodes[i].push_audio(&heard, t);
        }
        for (i, node) in nodes.iter_mut().enumerate() {
            for j in node.take_journal() {
                if std::env::var("ION_DUMP").is_ok() {
                    eprintln!(
                        "{t:7.2} {i} {} {} {:?}",
                        if j.tx { "tx" } else { "rx" },
                        j.what,
                        j.snr_db
                    );
                }
                tones += (["DONE", "SLOWER", "FASTER"].contains(&j.what.as_str()) && !j.tx) as usize;
            }
        }
    }
    let acq = nodes[1].stream_acq();
    let got = nodes[1].station_mut().take_received();
    assert!(got == data, "{} of {n} B in {t:.0} s, {acq:?}", got.len());
    assert!(nodes[1].station().stats().stream_blocks >= 1, "{acq:?}");
    assert!(acq.found >= 3 && acq.missed <= acq.periods / 4, "{acq:?}");
    assert!(tones >= 1, "the sender heard no answer");
}

#[test]
fn after_the_turn_the_other_side_streams_too() {
    let data = payload(10_000, 31);
    let back = payload(10_000, 32);
    let mut sim = model(0.0, true, true, 14);
    sim.stations[0].send(&data);
    let done = sim.run_until(3_600.0, |s| s.stations[1].stats().bytes_received >= data.len() as u64);
    assert!(done);
    sim.stations[1].send(&back);
    let done = sim.run_until(sim.t + 3_600.0, |s| {
        s.stations[0].stats().bytes_received >= back.len() as u64
    });
    assert!(done);
    assert!(
        sim.stations[1].stats().stream_blocks >= 1,
        "{:?}",
        sim.stations[1].stats().stream_blocks
    );
    assert!(
        sim.log.iter().any(|e| e.from == 1 && e.code == STREAM),
        "the side that took the turn did not stream"
    );
}

/// A traced session on IONOS SIM: `ION_MODE` (WGN), `ION_SNR` (0),
/// `ION_SECS` (300), `ION_SEED` (900 + S:N, as `tests/ionos.rs` seed 0),
/// `ION_DUMP=1` for every burst. Prints where the air time went, when each
/// block arrived, and what the receiver's timekeeping did.
#[test]
#[ignore]
fn stream_trace() {
    use ion::level::LADDER;
    let env = |k: &str| std::env::var(k).ok();
    let mode = env("ION_MODE")
        .and_then(|m| Mode::ALL.iter().copied().find(|x| x.name().eq_ignore_ascii_case(&m)))
        .unwrap_or(Mode::Wgn);
    let snr: f32 = env("ION_SNR").and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let secs: f64 = env("ION_SECS").and_then(|v| v.parse().ok()).unwrap_or(300.0);
    let seed: u64 = env("ION_SEED")
        .and_then(|v| v.parse().ok())
        .unwrap_or((900 + snr as i64) as u64);
    let mut sim = ionos(mode, snr, 400_000, seed);
    let mut arrivals = Vec::new();
    let mut have = 0;
    // When the receiver first held 10 kB and 50 kB (the lab's measure).
    let mut at = [None; 2];
    let mut aims = Vec::new();
    while sim.t < secs {
        let t = sim.t + 1.0;
        sim.run_until(t, |_| false);
        if sim.t as u64 % 30 == 0
            && let Some(a) = sim.stations[0].aim_db()
        {
            aims.push(format!("{a:.1}"));
        }
        let got = sim.stations[1].stats().bytes_received;
        if got > have {
            arrivals.push(format!("{:.0}s:{}", sim.t, got));
            have = got;
        }
        for (a, size) in at.iter_mut().zip([10_000u64, 50_000]) {
            if a.is_none() && got >= size {
                *a = Some(sim.t);
            }
        }
    }
    let bpm = |k: usize, size: f64| at[k].map_or("-".to_string(), |t: f64| format!("{:.0}", size * 60.0 / t));
    if env("ION_DUMP").is_some() {
        dump(&sim);
        for p in &sim.stations[1].stats().stream_log {
            eprintln!(
                "{:<9} {}/{} {:+.1} dB -> {}",
                ion::link::stream::OPTS[p.0 as usize].name,
                p.1,
                p.2,
                p.3,
                p.4
            );
        }
    }
    // Air time: each period owns its slot up to the next burst (its pause
    // and the answer in it); data bursts and control the same up to the
    // next burst; before the first burst nothing.
    let (mut period, mut data, mut ctl, mut first) = (0.0, 0.0, 0.0, None);
    for (k, e) in sim.log.iter().enumerate() {
        let next = sim.log.get(k + 1).map_or(sim.t, |n| n.start);
        let span = next - e.start;
        match e.code {
            STREAM => {
                period += span;
                first.get_or_insert(e.start);
            }
            TONES => period += span,
            c if c < LADDER.len() => data += span,
            _ => ctl += span,
        }
    }
    let total = sim.t;
    let log = &sim.stations[1].stats().stream_log;
    let mut by_opt = std::collections::BTreeMap::new();
    for p in log {
        let e = by_opt
            .entry(ion::link::stream::OPTS[p.0 as usize].name)
            .or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += p.1 as u32;
        e.2 += p.2 as u32;
    }
    let acq = sim.medium.stream_stats(1);
    eprintln!(
        "TRACE {} {snr:+} dB seed {seed} {secs:.0} s: {} B ({:.0} B/min); first period {:?} s; air: periods+pauses {:.0}% ARQ data {:.0}% control {:.0}%; blocks {}; arrivals [{}]",
        mode.name(),
        have,
        have as f64 * 60.0 / total,
        first.map(|f: f64| f.round()),
        100.0 * period / total,
        100.0 * data / total,
        100.0 * ctl / total,
        sim.stations[1].stats().stream_blocks,
        arrivals.join(" ")
    );
    eprintln!(
        "TRACE   10 kB at {:?} s ({} B/min), 50 kB at {:?} s ({} B/min)",
        at[0].map(|t: f64| t.round()),
        bpm(0, 10_000.0),
        at[1].map(|t: f64| t.round()),
        bpm(1, 50_000.0)
    );
    eprintln!("TRACE   options (periods ok/tried frames): {by_opt:?}");
    eprintln!("TRACE   aim every 30 s: {}", aims.join(" "));
    eprintln!("TRACE   acquisition: {acq:?}");
}
