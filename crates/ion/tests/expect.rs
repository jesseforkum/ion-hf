//! What a station expects to hear next (`Station::expected`): the peer's
//! bursts must be among it as a link runs, the set must stay small, and
//! nothing is expected outside a link.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ion::level::{EFF_THRESHOLDS_DB, Guard, LADDER, Wave};
use ion::link::sim::{Medium, ModelAir, ModelMedium, Sim};
use ion::link::station::TINY;
use ion::link::{Air, Config, Expected, Incoming, Outgoing, State, Station};

/// Slow, short data bursts sent while their receiver expected data, and
/// how many of them it expected; the same for tiny bursts; the largest set one
/// receiver (one guard) gets.
#[derive(Default)]
struct Tally {
    data: (usize, usize),
    tiny: (usize, usize),
    cued: usize,
    largest: usize,
}

/// The model channel, checking each OFDM burst against what its receiver
/// expected just before it.
struct Checking {
    inner: ModelMedium,
    air: ModelAir,
    expected: [Vec<Expected>; 2],
    tally: Rc<RefCell<Tally>>,
}

impl Medium for Checking {
    fn expect(&mut self, to: usize, e: &[Expected]) {
        let mut t = self.tally.borrow_mut();
        for g in [Guard::Normal, Guard::Long, Guard::Slice] {
            t.largest = t.largest.max(e.iter().filter(|x| x.wave == Wave::Ofdm(g)).count());
        }
        self.expected[to] = e.to_vec();
    }

    fn carry(&mut self, from: usize, b: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        let e = &self.expected[1 - from];
        if matches!(b.wave, Wave::Ofdm(_)) {
            let found = e.iter().any(|x| {
                x.wave == b.wave
                    && x.code == b.code
                    && x.user == b.user
                    && x.bits == b.coded_bits()
                    && (b.code != TINY || x.extra == b.extra)
            });
            let mut t = self.tally.borrow_mut();
            // Data bursts the receiver can use a header guess for: slow
            // levels, bursts no longer than `max_burst_s`. A cued burst's
            // header comes from its cue, and a partial one (its count
            // only in the cue) is useless without it, so none is listed.
            let slow = b.code < LADDER.len() && EFF_THRESHOLDS_DB[b.code] <= 5.0;
            let short = self.air.duration(b.wave, b.code, b.coded_bits()) <= Config::new("X").max_burst_s;
            if b.cue {
                t.cued += 1;
            } else if slow && short && e.iter().any(|x| x.code < LADDER.len()) {
                t.data.0 += 1;
                t.data.1 += found as usize;
            } else if b.code == TINY && e.iter().any(|x| x.code == TINY && x.user == b.user) {
                t.tiny.0 += 1;
                t.tiny.1 += found as usize;
            }
        }
        self.inner.carry(from, b, start, end)
    }
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn the_peers_next_burst_is_expected() {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let tally = Rc::new(RefCell::new(Tally::default()));
    // Without cues (a peer that has none) every data burst needs its
    // header; with them, those that go out uncued.
    for (cue, seed, snr) in [
        (false, 1, 3.0),
        (false, 2, 8.0),
        (false, 3, 14.0),
        (true, 1, 3.0),
        (true, 2, 8.0),
    ] {
        // The link's own bursts: the stream gear has its own receiver.
        let cfg = |call: &str| Config {
            cue,
            stream: false,
            ..Config::new(call)
        };
        let a = Station::new(cfg("N0ABC"), air.clone());
        let mut b = Station::new(cfg("W1AW"), air.clone());
        b.listen();
        assert!(b.expected().is_empty(), "nothing is expected while listening");
        let mut m = ModelMedium::new(snr, seed);
        m.fade_db = 4.0;
        let medium = Checking {
            inner: m,
            air: ModelAir::default(),
            expected: [Vec::new(), Vec::new()],
            tally: tally.clone(),
        };
        let mut sim = Sim::new(a, b, Box::new(medium), air.clone());
        sim.stations[0].connect("W1AW", 0.0);
        sim.stations[0].send(&payload(200_000));
        sim.stations[1].send(&payload(20_000));
        sim.run_until(900.0, |_| false);
        sim.stations[0].disconnect();
        sim.run_until(1000.0, |s| s.stations[0].state() == State::Disconnected);
        assert_eq!(sim.stations[0].state(), State::Disconnected);
        assert!(
            sim.stations[0].expected().is_empty(),
            "nothing is expected after the link"
        );
    }
    let t = tally.borrow();
    println!(
        "data bursts expected: {} of {}; tiny bursts: {} of {}; cued bursts {}; largest set {}",
        t.data.1, t.data.0, t.tiny.1, t.tiny.0, t.cued, t.largest
    );
    assert!(
        t.data.0 > 50 && t.data.1 * 20 >= t.data.0 * 19,
        "data bursts not expected"
    );
    assert!(t.tiny.1 == t.tiny.0, "fixed tiny bursts not expected");
    assert!(t.largest <= 300, "expected set too large");
}
