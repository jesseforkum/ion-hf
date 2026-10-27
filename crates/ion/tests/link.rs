//! The link protocol over the statistical channel model (no audio): connect,
//! transfer, gear shifting, turn-over, disconnect, and giving up.

use std::sync::Arc;

use ion::level::{Guard, LADDER, Wave};
use ion::link::sim::{ModelAir, ModelMedium, Sim};
use ion::link::{Air, Config, Role, State, Station};

fn pair(snr_db: f32, seed: u64, medium: impl FnOnce(&mut ModelMedium)) -> Sim {
    pair_with(snr_db, seed, true, medium)
}

/// Gear-shift knobs from the environment, for tuning runs: `ION_MARGIN`
/// and so on for both arms, `ION_HARQ_MARGIN` / `ION_ARQ_MARGIN` for
/// HARQ on / off only.
fn gear_knobs(c: &mut Config) {
    let env = |k: &str| {
        let arm = if c.harq { "HARQ" } else { "ARQ" };
        std::env::var(format!("ION_{arm}_{k}"))
            .or_else(|_| std::env::var(format!("ION_{k}")))
            .ok()
            .map(|v| v.parse::<f64>().unwrap())
    };
    if let Some(v) = env("MARGIN") {
        c.margin_db = v as f32;
    }
    if let Some(v) = env("LOSS_HOLD") {
        c.loss_hold_s = v;
    }
}

/// Overhead knobs from the environment (A/B runs): `ION_OVERHEAD_OFF=1`
/// turns every one off; `ION_ADAPT`,
/// `ION_TINY_ALL` = 0/1 one at a time; `ION_BURST_LONG` (s),
/// `ION_BURST_OVERHEAD` (share), `ION_BURST_STEADY` and
/// `ION_BURST_CLIMB` (dB) tune `burst_adapt`; `ION_CHEAP_ACK` = margin dB or `off`;
/// `ION_LIFELINE=0` turns the lifeline off.
fn overhead_knobs(c: &mut ion::link::Config) {
    let env = |k: &str| std::env::var(k).ok();
    let flag = |k: &str| env(k).map(|v| v != "0");
    if flag("ION_OVERHEAD_OFF") == Some(true) {
        c.burst_adapt = false;
        c.tiny_ack_all = false;
        c.cheap_ack_margin_db = None;
    }
    if let Some(v) = flag("ION_ADAPT") {
        c.burst_adapt = v;
    }
    if let Some(v) = flag("ION_TINY_ALL") {
        c.tiny_ack_all = v;
    }
    if let Some(v) = env("ION_BURST_LONG") {
        c.max_burst_long_s = v.parse().unwrap();
    }
    if let Some(v) = env("ION_BURST_OVERHEAD") {
        c.burst_overhead = v.parse().unwrap();
    }
    if let Some(v) = env("ION_BURST_STEADY") {
        c.burst_steady_db = v.parse().unwrap();
    }
    // ION_BURST_CLIMB = dB, or "off".
    if let Some(v) = env("ION_BURST_CLIMB") {
        c.burst_climb_db = if v == "off" { None } else { Some(v.parse().unwrap()) };
    }
    // ION_CHEAP_ACK = margin in dB, or "off".
    if let Some(v) = env("ION_CHEAP_ACK") {
        c.cheap_ack_margin_db = if v == "off" { None } else { Some(v.parse().unwrap()) };
    }
    if env("ION_LIFELINE").is_some_and(|v| v == "0") {
        c.lifeline = false;
    }
}

fn pair_with(snr_db: f32, seed: u64, harq: bool, medium: impl FnOnce(&mut ModelMedium)) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let cfg = |call| {
        // HARQ off goes with a 1 dB gear-shift margin.
        let mut c = Config {
            harq,
            margin_db: if harq { Config::new(call).margin_db } else { 1.0 },
            ..Config::new(call)
        };
        gear_knobs(&mut c);
        overhead_knobs(&mut c);
        c
    };
    let a = Station::new(cfg("N0ABC"), air.clone());
    let mut b = Station::new(cfg("W1AW"), air.clone());
    b.listen();
    let mut m = ModelMedium::new(snr_db, seed);
    medium(&mut m);
    Sim::new(a, b, Box::new(m), air)
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

/// Connects A to B, sends `n` bytes A → B, returns (sim, seconds taken).
fn transfer(sim: &mut Sim, n: usize, t_max: f64) -> f64 {
    let data = payload(n, 7);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let ok = sim.run_until(t_max, |s| s.stations[1].stats().bytes_received >= n as u64);
    assert!(
        ok,
        "only {} of {n} bytes after {t_max} s; A {:?}, B {:?}",
        sim.stations[1].stats().bytes_received,
        sim.stations[0].state(),
        sim.stations[1].state()
    );
    assert_eq!(sim.stations[1].take_received(), data, "delivered bytes differ");
    sim.t
}

fn bps(n: usize, secs: f64) -> f64 {
    n as f64 * 8.0 / secs
}

/// A pair with the default config changed by `tune` (no environment
/// knobs), on the model channel, with the real PHYs' burst lengths
/// (`PhyAir`: the overhead options size bursts from them).
fn pair_tuned(snr_db: f32, seed: u64, tune: impl Fn(&mut Config), medium: impl FnOnce(&mut ModelMedium)) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ion::link::audio::PhyAir);
    let cfg = |call| {
        let mut c = Config::new(call);
        tune(&mut c);
        c
    };
    let a = Station::new(cfg("N0ABC"), air.clone());
    let mut b = Station::new(cfg("W1AW"), air.clone());
    b.listen();
    let mut m = ModelMedium::new(snr_db, seed);
    medium(&mut m);
    Sim::new(a, b, Box::new(m), air)
}

#[test]
fn bursts_grow_on_a_steady_floor_and_it_pays() {
    // −9.5 dB, steady: the SNR report allows only MFSK R0 (R1 wants
    // −8.5), which loses next to nothing. Its 8 s bursts hold one 22-byte
    // frame; on a clean run they grow to several.
    let n = 600;
    let run = |adapt: bool| {
        let mut sim = pair_tuned(-9.5, 21, |c| c.burst_adapt = adapt, |_| {});
        let secs = transfer(&mut sim, n, 3_600.0);
        let most = sim
            .log
            .iter()
            .filter(|e| e.from == 0 && e.code == 0)
            .map(|e| e.frames)
            .max()
            .unwrap();
        (secs, most)
    };
    let (t_off, most_off) = run(false);
    let (t_on, most_on) = run(true);
    assert_eq!(most_off, 1, "8 s bursts at the floor hold one frame");
    assert!(most_on >= 3, "bursts never grew: {most_on} frames");
    assert!(t_on < 0.85 * t_off, "adapted {t_on:.0} s, fixed {t_off:.0} s");
}

#[test]
fn a_loss_takes_the_burst_back_to_its_base_length() {
    // At the floor with slow deep fades: bursts grow between fades, and the
    // data burst after one that was not even detected is a base-length
    // one (one frame at MFSK R0).
    let mut sim = pair_tuned(
        -9.5,
        22,
        |c| c.max_level = 0,
        |m| {
            m.fade_db = 4.0;
            m.fade_tau_s = 60.0;
        },
    );
    transfer(&mut sim, 3_000, 20_000.0);
    let data: Vec<_> = sim.log.iter().filter(|e| e.from == 0 && e.code == 0).collect();
    assert!(data.iter().any(|e| e.frames >= 3), "never grew");
    let after_loss: Vec<usize> = data
        .windows(2)
        .filter(|w| !w[0].delivered)
        .map(|w| w[1].frames)
        .collect();
    assert!(!after_loss.is_empty(), "no burst was lost");
    assert!(after_loss.iter().all(|&f| f == 1), "{after_loss:?}");
}

#[test]
fn a_big_clean_burst_gets_a_tiny_ack() {
    use ion::link::station::{CONTROL, TINY};
    // Clean: the climb reaches OFDM-normal levels with 7+ frames a burst.
    let acks = |tiny_all: bool| {
        let mut sim = pair_tuned(32.0, 23, |c| c.tiny_ack_all = tiny_all, |_| {});
        transfer(&mut sim, 30_000, 600.0);
        let log = &sim.log;
        let mut codes = Vec::new();
        for w in log.windows(2) {
            if w[0].from == 0 && w[0].code < ion::level::LADDER.len() && w[0].frames > 6 && w[1].from == 1 {
                codes.push(w[1].code);
            }
        }
        codes
    };
    let on = acks(true);
    assert!(!on.is_empty());
    assert!(on.iter().all(|&c| c == TINY), "{on:?}");
    let off = acks(false);
    assert!(off.iter().all(|&c| c == CONTROL), "{off:?}");
}

#[test]
fn the_first_floor_burst_is_acked_in_the_quicker_mfsk_mode_when_the_snr_allows() {
    use ion::level::Wave;
    use ion::phy_mfsk::MfskMode;
    let first_ack_wave = |cheap: Option<f32>| {
        // The floor start (`start_margin_db`), so the first burst is R0.
        let mut sim = pair_tuned(
            20.0,
            24,
            |c| {
                c.cheap_ack_margin_db = cheap;
                c.start_margin_db = None;
            },
            |_| {},
        );
        transfer(&mut sim, 500, 300.0);
        let i = sim.log.iter().position(|e| e.from == 0 && e.code == 0).unwrap();
        let ack = &sim.log[i + 1];
        assert_eq!(ack.from, 1);
        ack.wave
    };
    assert_eq!(first_ack_wave(Some(0.0)), Wave::Mfsk(MfskMode::R1));
    assert_eq!(first_ack_wave(None), Wave::Mfsk(MfskMode::R0));
}

#[test]
fn overhead_options_off_still_deliver_and_disconnect() {
    // Every overhead option off and on: the same bytes arrive, the link
    // closes, on a fading channel that loses bursts.
    for on in [false, true] {
        let mut sim = pair_tuned(
            4.0,
            25,
            |c| {
                c.burst_adapt = on;
                c.tiny_ack_all = on;
                c.cheap_ack_margin_db = on.then_some(0.0);
            },
            |m| {
                m.fade_db = 8.0;
                m.fade_tau_s = 15.0;
            },
        );
        transfer(&mut sim, 8_000, 3_600.0);
        sim.stations[0].disconnect();
        assert!(
            sim.run_until(sim.t + 300.0, |s| {
                s.stations[0].state() == State::Disconnected && s.stations[1].state() == State::Disconnected
            }),
            "on = {on}"
        );
    }
}

#[test]
fn transfers_and_climbs_to_the_top_level_on_a_clean_channel() {
    // Clean for every level: the top one (64QAM 5/6) wants about 25 dB.
    let mut sim = pair(32.0, 1, |_| {});
    let n = 40_000;
    let secs = transfer(&mut sim, n, 600.0);
    let a = &sim.stations[0];
    assert_eq!(a.state(), State::Connected);
    assert_eq!(
        *a.stats().levels.last().unwrap(),
        ion::level::LADDER.len() - 1,
        "{:?}",
        a.stats().levels
    );
    let rate = bps(n, secs);
    assert!(rate > 3000.0, "{rate:.0} bps");
    // Then a clean disconnect on both sides.
    sim.stations[0].disconnect();
    assert!(sim.run_until(sim.t + 60.0, |s| {
        s.stations[0].state() == State::Disconnected && s.stations[1].state() == State::Disconnected
    }));
}

#[test]
fn low_snr_still_delivers_at_the_bottom_level() {
    let mut sim = pair(1.5, 2, |_| {});
    transfer(&mut sim, 5_000, 900.0);
    let levels = &sim.stations[0].stats().levels;
    // 1.5 dB allows the MFSK floor, the slice levels and the long-guard
    // levels (the gear shifter may try the second and come back down).
    let long = ion::level::LADDER
        .iter()
        .rposition(|l| l.wave == ion::level::Wave::Ofdm(ion::level::Guard::Long))
        .unwrap();
    assert!(levels.iter().all(|&l| l <= long), "{levels:?}");
}

#[test]
fn shifts_gears_through_fading() {
    let mut sim = pair(11.0, 3, |m| {
        m.fade_db = 6.0;
        m.fade_tau_s = 20.0;
    });
    transfer(&mut sim, 60_000, 1800.0);
    let levels = &sim.stations[0].stats().levels;
    let (lo, hi) = (levels.iter().min().unwrap(), levels.iter().max().unwrap());
    assert!(hi - lo >= 3, "levels only {lo}..{hi}");
    // Frames were lost in the fades and made good: resent whole, or (with
    // HARQ) continued with parity increments.
    let st = sim.stations[0].stats();
    assert!(st.frames_resent + st.increments_sent > 0, "no losses to handle");
}

#[test]
fn both_directions_with_turn_over() {
    let mut sim = pair(12.0, 4, |_| {});
    let back = payload(9_000, 99);
    sim.stations[1].send(&back);
    transfer(&mut sim, 12_000, 900.0);
    let ok = sim.run_until(sim.t + 900.0, |s| s.stations[0].stats().bytes_received >= 9_000);
    assert!(ok, "return traffic: {} bytes", sim.stations[0].stats().bytes_received);
    assert_eq!(sim.stations[0].take_received(), back);
    assert_eq!(sim.stations[1].role(), Role::Sender);
}

#[test]
fn lost_acks_are_recovered_without_duplicates() {
    // ACKs are far more robust than data, so only deep fades lose them:
    // fade hard around a mid SNR.
    let mut sim = pair(6.0, 5, |m| {
        m.fade_db = 12.0;
        m.fade_tau_s = 8.0;
    });
    transfer(&mut sim, 20_000, 6_000.0);
    let a = sim.stations[0].stats();
    assert!(a.polls > 0, "no lost ACK was polled for");
}

#[test]
fn calling_nobody_gives_up() {
    let mut sim = pair(20.0, 6, |m| m.deaf = Some(1));
    sim.stations[0].connect("W1AW", 0.0);
    let ok = sim.run_until(300.0, |s| s.stations[0].state() == State::Failed);
    assert!(ok, "still {:?} after 300 s", sim.stations[0].state());
    assert_eq!(sim.stations[1].state(), State::Listening);
}

#[test]
fn receiver_can_ask_to_disconnect() {
    let mut sim = pair(15.0, 7, |_| {});
    transfer(&mut sim, 3_000, 300.0);
    sim.stations[1].disconnect();
    let ok = sim.run_until(sim.t + 120.0, |s| {
        s.stations[0].state() == State::Disconnected && s.stations[1].state() == State::Disconnected
    });
    assert!(ok, "A {:?} B {:?}", sim.stations[0].state(), sim.stations[1].state());
}

#[test]
fn a_silent_peer_times_the_receiver_out() {
    // A vanishes (radio off): B must not hang forever. It holds in the
    // lifeline first, as for a deep fade (`docs/link.md`, Lifeline);
    // without it, it gives up after `link_timeout_s`.
    for lifeline in [true, false] {
        let mut sim = pair(20.0, 8, |_| {});
        sim.stations[1] = Station::new(
            Config {
                lifeline,
                ..Config::new("W1AW")
            },
            sim.air.clone(),
        );
        sim.stations[1].listen();
        transfer(&mut sim, 2_000, 300.0);
        let t0 = sim.t;
        sim.stations[0] = Station::new(Config::new("N0ABC"), sim.air.clone());
        let c = Config::new("W1AW");
        let within = if lifeline {
            c.life_enter_s + c.life_silent_s + c.life_probe_max_s + 10.0
        } else {
            300.0
        };
        let ok = sim.run_until(t0 + within, |s| s.stations[1].state() == State::Failed);
        assert!(ok, "B still {:?}", sim.stations[1].state());
        assert_eq!(sim.stations[1].end(), Some(ion::link::station::End::LinkLost));
    }
}

/// The first OFDM level (the MFSK levels come first).
fn first_ofdm() -> usize {
    ion::level::LADDER
        .iter()
        .position(|l| matches!(l.wave, ion::level::Wave::Ofdm(_)))
        .unwrap()
}

#[test]
fn starts_where_the_report_allows_and_climbs_quickly() {
    // 32 dB: the first data burst already in 64QAM, and the top within two.
    let mut sim = pair(32.0, 9, |_| {});
    transfer(&mut sim, 20_000, 600.0);
    let levels = sim.stations[0].stats().levels.clone();
    let top = ion::level::LADDER.len() - 1;
    assert!(levels[0] >= top - 1, "{levels:?}");
    let reached = levels.iter().position(|&l| l == top).expect("never reached the top");
    assert!(reached <= 1, "took {reached} bursts to climb: {levels:?}");
    assert!(
        levels.windows(2).all(|w| w[1] >= w[0]),
        "dropped on a clean channel: {levels:?}"
    );
}

#[test]
fn a_dying_band_drops_to_the_floor_and_still_delivers() {
    // 20 dB, then the band collapses to -6 dB mid-transfer: data in flight
    // in 180-byte frames has to be re-cut into 32-byte floor frames. (The
    // link starts near the top, so the drop comes after two bursts.)
    let mut sim = pair(20.0, 10, |m| m.drop_at = Some((30.0, -6.0)));
    let n = 40_000;
    transfer(&mut sim, n, 20_000.0);
    let levels = &sim.stations[0].stats().levels;
    // Normal-guard QPSK 2/3 or faster.
    let qpsk_2_3 = ion::level::LADDER
        .iter()
        .position(|l| l.wave == ion::level::Wave::Ofdm(ion::level::Guard::Normal) && l.rate == ion::fec::Rate::R2_3)
        .unwrap();
    assert!(levels.iter().any(|&l| l >= qpsk_2_3), "never got fast: {levels:?}");
    // It ends on the MFSK levels. (Not "the last burst": the slice levels
    // are probed now and then, and the transfer may end on a probe that
    // got through.)
    let mfsk = levels
        .iter()
        .rev()
        .take(10)
        .filter(|&&l| matches!(ion::level::LADDER[l].wave, ion::level::Wave::Mfsk(_)))
        .count();
    assert!(mfsk >= 8, "{levels:?}");
}

#[test]
fn a_dead_waveform_is_not_retried_over_and_over() {
    const LADDER_LEN: usize = ion::level::LADDER.len();
    let ofdm = first_ofdm();
    // Fast fading: plenty of SNR, but every OFDM level dies (its pilots
    // cannot follow the Doppler). The SNR report keeps inviting a climb;
    // the link must learn to stay on MFSK and only probe now and then.
    let mut sim = pair(12.0, 12, |m| {
        for t in &mut m.thresholds_db[ofdm..] {
            *t = 100.0;
        }
    });
    let n = 3_000;
    let secs = transfer(&mut sim, n, 3_600.0);
    let levels = &sim.stations[0].stats().levels;
    // Air time, not bursts: the probes are spaced in time (the hold-offs).
    // A short transfer may still hold the first few probes (each one the
    // dead level and the one it may step down through), the hold doubling
    // each time.
    let dead: Vec<&_> = sim
        .log
        .iter()
        .filter(|e| e.from == 0 && (ofdm..LADDER_LEN).contains(&e.code))
        .collect();
    let dead_s: f64 = dead.iter().map(|e| e.end - e.start).sum();
    assert!(
        dead_s * 8.0 <= secs || dead.len() <= 6,
        "{dead_s:.0} of {secs:.0} s at dead levels ({} bursts): {levels:?}",
        dead.len()
    );
    // MFSK R1 alone would do it in well under 15 minutes.
    assert!(secs < 900.0, "{secs:.0} s: {levels:?}");
}

#[test]
fn a_doppler_report_keeps_the_link_on_mfsk() {
    // The fast-fading channel above, but the receiver measures the
    // Doppler (15 Hz) and says so in its ACKs: the sender never tries an
    // OFDM level at all, instead of learning by losing bursts.
    let ofdm = first_ofdm();
    let mut sim = pair(12.0, 12, |m| {
        for t in &mut m.thresholds_db[ofdm..] {
            *t = 100.0;
        }
        m.spreads = Some((0.5, 15.0));
    });
    let secs = transfer(&mut sim, 3_000, 3_600.0);
    let st = sim.stations[0].stats();
    assert!(st.levels.iter().all(|&l| l < ofdm), "OFDM tried: {:?}", st.levels);
    assert_eq!(st.failed_bursts, 0, "{:?}", st.levels);
    assert!(secs < 600.0, "{secs:.0} s: {:?}", st.levels);
}

#[test]
fn an_echo_report_skips_the_4ms_guard() {
    // 7 ms echoes: the slice and normal-guard levels die, the long guard
    // works. With the echo in the report the sender goes from the MFSK
    // floor straight to the long guard and never loses a burst to it.
    let mut sim = pair(12.0, 13, |m| {
        for (l, t) in m.thresholds_db.iter_mut().enumerate() {
            if matches!(LADDER[l].wave, Wave::Ofdm(Guard::Normal | Guard::Slice)) {
                *t = 100.0;
            }
        }
        m.spreads = Some((7.0, 1.0));
    });
    transfer(&mut sim, 20_000, 3_600.0);
    let st = sim.stations[0].stats();
    assert!(
        st.levels
            .iter()
            .all(|&l| !matches!(LADDER[l].wave, Wave::Ofdm(Guard::Normal | Guard::Slice))),
        "4 ms guard tried: {:?}",
        st.levels
    );
    let long_1_2 = LADDER
        .iter()
        .position(|l| l.wave == Wave::Ofdm(Guard::Long) && l.rate == ion::fec::Rate::R1_2)
        .unwrap();
    assert!(
        st.levels.contains(&long_1_2),
        "never reached long-guard QPSK 1/2: {:?}",
        st.levels
    );
    assert_eq!(st.failed_bursts, 0, "{:?}", st.levels);
}

/// Throughput against SNR on the model channel, HARQ off and on, each
/// the mean over `ION_SEEDS` seeds (default 8). Run with --ignored
/// --nocapture.
#[test]
#[ignore]
fn throughput_table() {
    let seeds: u64 = std::env::var("ION_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    println!("| SNR dB | steady, ARQ | steady, HARQ | fading 6 dB, ARQ | fading 6 dB, HARQ |");
    println!("|---:|---:|---:|---:|---:|");
    for snr in (-2..=24).step_by(2) {
        let mut row = Vec::new();
        for fade in [0.0, 6.0] {
            for harq in [false, true] {
                let mut sum = 0.0;
                let mut unfinished = 0;
                for k in 0..seeds {
                    let seed = 11 + snr as u64 + 1000 * k;
                    let mut sim = pair_with(snr as f32, seed, harq, |m| m.fade_db = fade);
                    let n = 30_000;
                    let data = payload(n, 3);
                    sim.stations[0].connect("W1AW", 0.0);
                    sim.stations[0].send(&data);
                    let done = sim.run_until(3600.0, |s| s.stations[1].stats().bytes_received >= n as u64);
                    unfinished += !done as u32;
                    sum += bps(sim.stations[1].stats().bytes_received as usize, sim.t);
                }
                let mark = if unfinished > 0 {
                    format!(" ({unfinished} unfinished)")
                } else {
                    String::new()
                };
                row.push(format!("{:.0}{mark}", sum / seeds as f64));
            }
        }
        println!("| {snr:+} | {} |", row.join(" | "));
    }
}

// -- compression (`Config::compress`, `link::compress`) ---------------------------

const HELDOUT: &str = include_str!("../data/chat_heldout.txt");

fn text_pair(a_on: bool, b_on: bool, snr_db: f32, seed: u64) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let cfg = |call, on| Config {
        compress: on,
        ..Config::new(call)
    };
    let a = Station::new(cfg("N0ABC", a_on), air.clone());
    let mut b = Station::new(cfg("W1AW", b_on), air.clone());
    b.listen();
    Sim::new(a, b, Box::new(ModelMedium::new(snr_db, seed)), air)
}

/// Held-out lines, one `send` each, as typed.
fn typed_lines(n: usize) -> Vec<Vec<u8>> {
    HELDOUT
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .take(n)
        .map(|l| format!("{l}\n").into_bytes())
        .collect()
}

#[test]
fn compressed_text_both_ways_with_turn_over() {
    let mut sim = text_pair(true, true, 6.0, 5);
    let lines = typed_lines(40);
    let want: Vec<u8> = lines.concat();
    // B's reply is queued before the link exists: it waits for the
    // agreement, then goes compressed.
    let back = typed_lines(60)[40..].concat();
    sim.stations[1].send(&back);
    sim.stations[0].connect("W1AW", 0.0);
    for l in &lines {
        sim.stations[0].send(l);
    }
    assert!(sim.run_until(1800.0, |s| s.stations[1].stats().text_received >= want.len() as u64));
    assert!(sim.stations[0].compressing() && sim.stations[1].compressing());
    assert_eq!(sim.stations[1].take_received(), want);
    let a = sim.stations[0].stats().clone();
    let b = sim.stations[1].stats().clone();
    assert_eq!(a.text_sent, want.len() as u64);
    assert_eq!(b.codec_errors, 0);
    // Stream bytes: about a third of the text.
    assert!(
        b.bytes_received * 2 < want.len() as u64,
        "{} stream bytes for {}",
        b.bytes_received,
        want.len()
    );
    assert!(sim.run_until(sim.t + 1800.0, |s| s.stations[0].stats().text_received
        >= back.len() as u64));
    assert_eq!(sim.stations[0].take_received(), back);
    assert_eq!(sim.stations[0].stats().codec_errors, 0);
}

#[test]
fn compression_needs_both_ends() {
    for (a_on, b_on) in [(true, false), (false, true), (false, false)] {
        let mut sim = text_pair(a_on, b_on, 10.0, 6);
        let data = typed_lines(10).concat();
        sim.stations[0].connect("W1AW", 0.0);
        sim.stations[0].send(&data);
        assert!(sim.run_until(900.0, |s| s.stations[1].stats().text_received >= data.len() as u64));
        assert!(!sim.stations[0].compressing() && !sim.stations[1].compressing());
        assert_eq!(sim.stations[1].stats().bytes_received, data.len() as u64);
        assert_eq!(sim.stations[1].take_received(), data);
    }
}

#[test]
fn incompressible_data_over_a_compressed_link() {
    let mut sim = text_pair(true, true, 12.0, 8);
    let data = payload(5_000, 3);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    assert!(sim.run_until(900.0, |s| s.stations[1].stats().text_received >= data.len() as u64));
    assert!(sim.stations[0].compressing());
    assert_eq!(sim.stations[1].take_received(), data);
    // Five raw records of up to 1 kB, three header bytes each.
    assert_eq!(sim.stations[1].stats().bytes_received, 5_000 + 15);
}

/// Where the air time goes on the model channel, by the level of each
/// cycle's data burst: 30 kB at `ION_SNRS` (default -2,2,6,14,22),
/// steady and 6 dB fading, HARQ on, `ION_SEEDS` seeds (default 8).
/// The overhead knobs apply. Run with --ignored --nocapture.
#[test]
#[ignore]
fn overhead_table() {
    let seeds: u64 = std::env::var("ION_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let snrs: Vec<i32> = std::env::var("ION_SNRS")
        .map(|v| v.split(',').map(|s| s.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![-2, 2, 6, 14, 22]);
    for snr in snrs {
        for fade in [0.0, 6.0] {
            let mut air = ion::link::sim::Airtime::default();
            let mut rate = 0.0;
            for k in 0..seeds {
                let seed = 11 + snr as u64 + 1000 * k;
                let mut sim = pair_with(snr as f32, seed, true, |m| m.fade_db = fade);
                let n = 30_000;
                sim.stations[0].connect("W1AW", 0.0);
                sim.stations[0].send(&payload(n, 3));
                sim.run_until(3600.0, |s| s.stations[1].stats().bytes_received >= n as u64);
                rate += bps(sim.stations[1].stats().bytes_received as usize, sim.t) / seeds as f64;
                air.add(&sim.airtime());
            }
            println!("\n{snr:+} dB, fade {fade} dB: {rate:.0} bit/s\n\n{}", air.table());
        }
    }
}
