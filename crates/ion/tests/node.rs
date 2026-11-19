//! Two [`Node`]s talking through audio, as an app and its peer would: chunks
//! of 48 kHz audio, half duplex, real PHY + FEC + link.

use std::collections::VecDeque;

use ion::link::{Node, State};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::NoiseGen;

const CHUNK: usize = 4800; // 0.1 s

struct Side {
    node: Node,
    /// Audio still to play from our current burst.
    out: VecDeque<f32>,
    channel: Channel,
}

struct Air {
    sides: [Side; 2],
    noise: NoiseGen,
    sigma: f32,
    t: f64,
}

impl Air {
    fn new(profile: Profile, sigma: f32, a: &str, b: &str, lead_s: f64) -> Self {
        Self::seeded(profile, sigma, a, b, lead_s, 0)
    }

    /// `new` with other channel and noise realisations (`seed` 0 is
    /// `new`'s).
    fn seeded(profile: Profile, sigma: f32, a: &str, b: &str, lead_s: f64, seed: u64) -> Self {
        let side = |call: &str, ch: u64| Side {
            node: Node::new(Node::config(call), lead_s),
            out: VecDeque::new(),
            channel: Channel::from_profile(profile, ch + 10 * seed),
        };
        Self {
            sides: [side(a, 1), side(b, 2)],
            noise: NoiseGen::new(9 + seed),
            sigma,
            t: 0.0,
        }
    }

    /// One 0.1 s step: whoever has audio plays it, each side hears the
    /// other through its channel plus noise.
    fn step(&mut self) {
        self.t += CHUNK as f64 / 48_000.0;
        let t = self.t;
        let mut played = [vec![0.0f32; CHUNK], vec![0.0f32; CHUNK]];
        for (i, s) in self.sides.iter_mut().enumerate() {
            if s.out.is_empty()
                && let Some(audio) = s.node.poll(t)
            {
                s.out.extend(audio);
            }
            let n = s.out.len().min(CHUNK);
            for (k, x) in s.out.drain(..n).enumerate() {
                played[i][k] = x;
            }
            if n > 0 && s.out.is_empty() {
                s.node.tx_done(t);
            }
        }
        for i in 0..2 {
            let other = 1 - i;
            let mut heard = vec![0.0; CHUNK];
            self.sides[other].channel.process(&played[other], &mut heard);
            add_awgn(&mut heard, self.sigma, &mut self.noise);
            self.sides[i].node.push_audio(&heard, t);
        }
    }

    fn run_until(&mut self, secs: f64, mut done: impl FnMut(&Air) -> bool) -> bool {
        let end = self.t + secs;
        while self.t < end {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }
}

#[test]
fn two_nodes_connect_chat_both_ways_and_disconnect() {
    let mut air = Air::new(Profile::Awgn, 0.01, "N0JLF", "W1AW", 0.2);
    air.sides[1].node.station_mut().listen();
    air.sides[0].node.station_mut().connect("W1AW", 0.0);
    air.sides[0].node.station_mut().send(b"hello from ion\n");
    let ok = air.run_until(60.0, |a| a.sides[1].node.station().stats().bytes_received >= 15);
    assert!(
        ok,
        "A {:?} B {:?} after {:.1} s",
        air.sides[0].node.station().state(),
        air.sides[1].node.station().state(),
        air.t
    );
    assert_eq!(air.sides[1].node.station_mut().take_received(), b"hello from ion\n");
    assert!(air.sides[1].node.last_snr_db.unwrap() > 15.0);

    // B answers; the link turns around.
    air.sides[1].node.station_mut().send(b"copy, 73\n");
    let ok = air.run_until(90.0, |a| a.sides[0].node.station().stats().bytes_received >= 9);
    assert!(ok, "reply never arrived");
    assert_eq!(air.sides[0].node.station_mut().take_received(), b"copy, 73\n");

    air.sides[0].node.station_mut().disconnect();
    let ok = air.run_until(90.0, |a| {
        a.sides.iter().all(|s| s.node.station().state() == State::Disconnected)
    });
    assert!(
        ok,
        "A {:?} B {:?}",
        air.sides[0].node.station().state(),
        air.sides[1].node.station().state()
    );
}

#[test]
fn transfer_on_ccir_moderate() {
    // About 12 dB in 2.4 kHz against a typical burst.
    let mut air = Air::new(Profile::CcirModerate, 0.03, "N0JLF", "W1AW", 0.2);
    air.sides[1].node.station_mut().listen();
    air.sides[0].node.station_mut().connect("W1AW", 0.0);
    let text: Vec<u8> = (0..3000).map(|i| b'a' + (i % 26) as u8).collect();
    air.sides[0].node.station_mut().send(&text);
    let ok = air.run_until(240.0, |a| a.sides[1].node.station().stats().bytes_received >= 3000);
    assert!(
        ok,
        "{} bytes after {:.0} s",
        air.sides[1].node.station().stats().bytes_received,
        air.t
    );
    assert_eq!(air.sides[1].node.station_mut().take_received(), text);
}

#[test]
fn harq_increments_recover_frames_between_nodes() {
    // About 8 dB (as the receiver measures it) on CCIR poor: the link
    // climbs into the OFDM levels and loses frames in the fades, which
    // HARQ increments have to recover.
    // One transfer is one realisation of a chaotic system (any changed
    // decode reroutes the gear shifting, and with it whether long frames
    // fail where increments can help), so this runs four: every one must
    // deliver its 20 kB intact, and increments must recover frames in at
    // least one of them.
    let text: Vec<u8> = (0..20_000).map(|i| (i * 13 % 251) as u8).collect();
    let mut recovered = 0;
    for seed in 0..4 {
        let mut air = Air::seeded(Profile::CcirPoor, 0.2, "N0JLF", "W1AW", 0.2, seed);
        air.sides[1].node.station_mut().listen();
        air.sides[0].node.station_mut().connect("W1AW", 0.0);
        air.sides[0].node.station_mut().send(&text);
        let ok = air.run_until(600.0, |a| a.sides[1].node.station().stats().bytes_received >= 20_000);
        let (tx, rx) = (air.sides[0].node.station().stats(), air.sides[1].node.station().stats());
        eprintln!(
            "seed {seed}, {:.0} s: {} increments, {} recovered, {} resent; SNR {:?}; levels {:?}",
            air.t, tx.increments_sent, rx.harq_recovered, tx.frames_resent, air.sides[1].node.last_snr_db, tx.levels
        );
        assert!(ok, "seed {seed}: {} bytes after {:.0} s", rx.bytes_received, air.t);
        recovered += rx.harq_recovered;
        assert_eq!(air.sides[1].node.station_mut().take_received(), text);
    }
    assert!(recovered > 0, "nothing recovered by combining in four transfers");
}

/// A link with a CW ID every 20 s from each side (the app's is 9.5 min):
/// bytes delivered, polls the sender made, and IDs each side sent.
fn id_run(sigma: f32, every: Option<f64>) -> (u64, u32, [usize; 2]) {
    id_run_of(sigma, every, [true; 2])
}

/// [`id_run`], each side keying its own ID or not.
fn id_run_of(sigma: f32, every: Option<f64>, own: [bool; 2]) -> (u64, u32, [usize; 2]) {
    let mut air = Air::new(Profile::Awgn, sigma, "N0JLF", "N0JLF/P", 0.2);
    for (s, own) in air.sides.iter_mut().zip(own) {
        s.node.set_id_every(every);
        s.node.set_own_id(own);
    }
    air.sides[1].node.station_mut().listen();
    air.sides[0].node.station_mut().connect("N0JLF/P", 0.0);
    let text: Vec<u8> = (0..200_000).map(|i| (i * 13 % 251) as u8).collect();
    air.sides[0].node.station_mut().send(&text);
    let mut ids = [0; 2];
    for _ in 0..15 {
        air.run_until(10.0, |_| false);
        for (k, s) in air.sides.iter_mut().enumerate() {
            ids[k] += s
                .node
                .take_journal()
                .iter()
                .filter(|n| n.tx && n.what == "CW ID")
                .count();
        }
    }
    let st = air.sides[0].node.station().stats();
    (air.sides[1].node.station().stats().bytes_received, st.polls, ids)
}

#[test]
fn cw_id_breaks_do_not_cost_polls() {
    // Each side's ID goes ahead of its next burst; the other hears it and
    // waits instead of polling into it.
    for sigma in [0.01, 0.06] {
        let (base, base_polls, _) = id_run(sigma, None);
        let (got, polls, ids) = id_run(sigma, Some(20.0));
        eprintln!("σ {sigma}: {got} B with IDs {ids:?} ({polls} polls), {base} B without ({base_polls} polls)");
        assert!(ids.iter().all(|&n| n >= 3), "IDs sent {ids:?}");
        assert!(polls <= base_polls + 1, "{polls} polls with IDs, {base_polls} without");
        // Each ID costs its own air time and little more (the 20 s here
        // makes it a third of the link; at the app's 9.5 min, about 2%).
        let cw_s = ion::sounding::cw("DE N0JLF/P").len() as f64 / 48_000.0;
        let lost_s = (base - got) as f64 / (base as f64 / 150.0);
        let per_id = lost_s / (ids[0] + ids[1]) as f64;
        eprintln!("  each ID {cw_s:.1} s of CW, costs {per_id:.1} s");
        assert!(per_id < cw_s + 3.0, "each ID costs {per_id:.1} s, {cw_s:.1} s of it CW");
    }
}

#[test]
fn a_side_without_its_cw_id_costs_the_other_nothing() {
    // The side that keys none still waits out the other's; the other's
    // allowance for an ID that never comes costs it no poll.
    let (both, _, _) = id_run(0.01, Some(20.0));
    for own in [[false, true], [true, false]] {
        let (got, polls, ids) = id_run_of(0.01, Some(20.0), own);
        eprintln!("own IDs {own:?}: {got} B, IDs {ids:?}, {polls} polls ({both} B with both)");
        for k in 0..2 {
            assert_eq!(ids[k] >= 3, own[k], "{own:?}: IDs sent {ids:?}");
            assert!(own[k] || ids[k] == 0, "{own:?}: IDs sent {ids:?}");
        }
        assert!(polls <= 1, "{own:?}: {polls} polls");
        assert!(got >= both, "{own:?}: {got} B, {both} B with both IDs");
    }
}

/// Noise for `secs` through all the receivers: how often
/// `Receivers::control_in` came up.
fn control_in_on_noise(sigma: f32, secs: usize, seed: u64) -> usize {
    let mut rx = ion::link::audio::Receivers::new();
    let mut g = NoiseGen::new(seed);
    let mut out = Vec::new();
    let mut ups = 0;
    let mut was = false;
    for _ in 0..secs * 10 {
        let chunk: Vec<f32> = (0..CHUNK).map(|_| sigma * g.gaussian()).collect();
        rx.process(&chunk, &mut out);
        let now = rx.control_in();
        ups += usize::from(now && !was);
        was = now;
    }
    ups
}

/// `Node::call_in`, what a host's PENDING goes by: not on noise; up while
/// a call's burst comes in, whoever it is for, and down at its end, the
/// link up by then if it was for us.
#[test]
fn a_call_coming_in_shows_before_it_is_read() {
    assert_eq!(control_in_on_noise(0.06, 60, 3), 0);
    for (to, state) in [("K9ZZZ", State::Listening), ("W1AW", State::Connected)] {
        let mut air = Air::new(Profile::Awgn, 0.06, "N0JLF", "W1AW", 0.2);
        air.sides[1].node.station_mut().listen();
        assert!(!air.run_until(5.0, |a| a.sides[1].node.call_in()), "before any call");
        air.sides[0].node.station_mut().connect(to, air.t);
        assert!(air.run_until(30.0, |a| a.sides[1].node.call_in()), "{to}: never up");
        let up = air.t;
        assert!(air.run_until(30.0, |a| !a.sides[1].node.call_in()), "{to}: never down");
        eprintln!("{to}: up at {up:.1} s, down at {:.1} s", air.t);
        assert_eq!(air.sides[1].node.station().state(), state, "{to}");
        assert!(!air.sides[0].node.call_in(), "the caller is not listening");
    }
}

/// Twenty minutes of noise at each of three levels.
#[test]
#[ignore]
fn no_call_shows_on_noise() {
    for (sigma, seed) in [(0.01, 1), (0.06, 2), (0.3, 3)] {
        let ups = control_in_on_noise(sigma, 1200, seed);
        eprintln!("sigma {sigma}: {ups} in 20 min");
        assert_eq!(ups, 0, "sigma {sigma}");
    }
}
