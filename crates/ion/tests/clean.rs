//! A host's CLEANTXBUFFER on the link ([`Station::clean`]) and its idle
//! close ([`Config::idle_close_s`]), on the model channel.

use std::sync::Arc;

use ion::link::sim::{Medium, ModelAir, ModelMedium, Sim};
use ion::link::station::End;
use ion::link::{Air, Clean, Config, Incoming, Outgoing, Settled, State, Station};

/// Whether to lose a burst: (from, burst, time).
type Rule = Box<dyn FnMut(usize, &Outgoing, f64) -> bool>;

/// The model channel, losing whatever its rule says.
struct Lossy {
    inner: ModelMedium,
    lose: Rule,
}

impl Medium for Lossy {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        let inc = self.inner.carry(from, burst, start, end);
        if (self.lose)(from, burst, start) { None } else { inc }
    }
}

fn pair(snr_db: f32, seed: u64, lose: impl FnMut(usize, &Outgoing, f64) -> bool + 'static) -> Sim {
    pair_with(true, snr_db, seed, lose)
}

/// `stream`: both ends offer the stream gear.
fn pair_with(stream: bool, snr_db: f32, seed: u64, lose: impl FnMut(usize, &Outgoing, f64) -> bool + 'static) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let cfg = |call: &str| Config {
        stream,
        ..Config::new(call)
    };
    let a = Station::new(cfg("N0ABC"), air.clone());
    let mut b = Station::new(cfg("W1AW"), air.clone());
    b.listen();
    let medium = Lossy {
        inner: ModelMedium::new(snr_db, seed),
        lose: Box::new(lose),
    };
    Sim::new(a, b, Box::new(medium), air)
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

/// Steps until `done` or `t_max`, gathering what B receives and A's
/// settled limbos.
fn run(
    sim: &mut Sim,
    got: &mut Vec<u8>,
    settled: &mut Vec<Settled>,
    t_max: f64,
    mut done: impl FnMut(&Sim, &[u8], &[Settled]) -> bool,
) -> bool {
    while sim.t < t_max {
        got.extend(sim.stations[1].take_received());
        settled.extend(sim.stations[0].take_settled());
        if done(sim, got, settled) {
            return true;
        }
        sim.step();
    }
    false
}

fn connect(sim: &mut Sim) {
    sim.stations[0].connect("W1AW", 0.0);
    assert!(sim.run_until(120.0, |s| s.stations[0].state() == State::Connected));
}

fn on_air(sim: &Sim) -> bool {
    sim.stations[0].text_progress().1 > 0
}

/// Data2G's mid-burst scenario (its test_cleantxbuffer_mid_burst): 20 kB
/// queued, the first data burst on air. `CLEANTXBUFFER 0` is late; one
/// that keeps the last 1000 bytes drops the rest never sent, the burst on
/// air is the limbo, it settles delivered, and the peer gets the bytes
/// before the limbo's end and the kept tail, nothing else.
#[test]
fn clean_mid_burst_keeps_the_tail_and_settles_the_limbo() {
    for (snr, seed) in [(15.0, 1), (6.0, 2), (2.0, 3), (-2.0, 4)] {
        let mut sim = pair(snr, seed, |_, _, _| false);
        connect(&mut sim);
        let up = payload(20_000, seed as u32);
        sim.stations[0].send(&up);
        let (mut got, mut settled) = (Vec::new(), Vec::new());
        assert!(run(&mut sim, &mut got, &mut settled, 300.0, |s, _, _| on_air(s)));
        let a = &mut sim.stations[0];
        assert_eq!(a.clean(Some(0), false), Clean::Late);
        let Clean::Done { drop, limbo } = a.clean(Some(19_000), false) else {
            panic!("no clean at {snr} dB");
        };
        assert_eq!(drop.1, 19_000);
        assert!(limbo.0 < limbo.1 && limbo.1 == drop.0, "{drop:?} {limbo:?}");
        assert!(a.limbo_bytes() > 0);
        assert_eq!(a.limbo().map(|l| (l.0, l.2)), Some(limbo));
        let want: Vec<u8> = [&up[..limbo.1 as usize], &up[19_000..]].concat();
        let ok = run(&mut sim, &mut got, &mut settled, 3_000.0, |sim, g, s| {
            g.len() >= want.len() && !s.is_empty() && sim.stations[0].pending() == 0
        });
        assert!(
            ok,
            "{snr} dB: got {} of {} B, settled {settled:?}",
            got.len(),
            want.len()
        );
        assert_eq!(got, want, "{snr} dB");
        assert_eq!(
            settled,
            [Settled {
                from: limbo.0,
                split: limbo.1,
                to: limbo.1,
                unknown: false
            }]
        );
        assert_eq!(sim.stations[0].limbo(), None);
        assert_eq!(sim.stations[0].pending(), 0);
    }
}

/// Cleans at random moments of a lossy transfer, each keeping a random
/// tail, then more writes: the peer always gets each limbo's bytes and
/// every byte kept or written later, in order, and nothing dropped.
/// Without the stream gear and with it (a clean with a stream block out
/// fails and drops nothing).
#[test]
fn random_cleans_on_a_lossy_link_never_tear_the_stream() {
    for stream in [false, true] {
        let (cleans, limbos, failed) = random_cleans(stream);
        if stream {
            assert!(cleans > 15 && failed > 0, "{cleans} cleans, {failed} failed");
        } else {
            assert!(cleans > 30 && limbos > 15, "{cleans} cleans, {limbos} with a limbo");
        }
    }
}

/// Cleans done, those with a limbo, and those that failed for a stream.
fn random_cleans(stream: bool) -> (u32, u32, u32) {
    let (mut cleans, mut limbos, mut failed) = (0, 0, 0);
    for seed in 1..=24u64 {
        let mut r = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut rnd = move |n: u64| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r % n
        };
        let snr = [12.0, 5.0, 0.0, -3.0][seed as usize % 4];
        let loss = rnd(3);
        let mut k = seed;
        let mut sim = pair_with(stream, snr, seed, move |_, _, _| {
            k = k.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (k >> 33) % 10 < loss
        });
        connect(&mut sim);
        let (mut got, mut settled) = (Vec::new(), Vec::new());
        // The host's bytes, by host offset, and which of them must arrive.
        let mut host: Vec<u8> = Vec::new();
        let mut keep: Vec<bool> = Vec::new();
        for round in 0..3 {
            let chunk = payload(3_000 + rnd(9_000) as usize, (seed * 10 + round) as u32);
            sim.stations[0].send(&chunk);
            host.extend(&chunk);
            keep.resize(host.len(), true);
            let wait = sim.t + 2.0 + rnd(60) as f64;
            run(&mut sim, &mut got, &mut settled, wait, |_, _, _| false);
            let written = host.len() as u64;
            let upto = (rnd(2) == 0).then(|| written - rnd(written / 2));
            match sim.stations[0].clean(upto, rnd(2) == 0) {
                Clean::Done { drop, limbo } => {
                    assert!(drop.0 <= drop.1 && limbo.1 <= drop.0, "{drop:?} {limbo:?}");
                    cleans += 1;
                    limbos += (limbo.0 < limbo.1) as u32;
                    for b in &mut keep[drop.0 as usize..drop.1 as usize] {
                        *b = false;
                    }
                }
                Clean::Late => assert!(upto.is_some()),
                Clean::Failed if stream => failed += 1,
                c => assert_eq!(c, Clean::Empty, "seed {seed}"),
            }
        }
        let want: Vec<u8> = host.iter().zip(&keep).filter(|p| *p.1).map(|p| *p.0).collect();
        let ok = run(&mut sim, &mut got, &mut settled, 20_000.0, |s, g, _| {
            g.len() >= want.len() && s.stations[0].limbo().is_none()
        });
        assert!(ok, "seed {seed}: got {} of {} B", got.len(), want.len());
        assert_eq!(got, want, "seed {seed}");
        for s in &settled {
            assert!(!s.unknown && s.split == s.to && s.from < s.to, "{s:?}");
        }
        assert_eq!(sim.stations[0].state(), State::Connected);
    }
    (cleans, limbos, failed)
}

/// Nothing to clean, no link, a VARA client's count (the limbo already
/// gone), and a second clean while the limbo is unsettled (it repeats it).
#[test]
fn clean_replies_as_vara_and_data2g() {
    let mut sim = pair(10.0, 5, |_, _, _| false);
    assert_eq!(sim.stations[0].clean(None, true), Clean::Empty);
    sim.stations[0].send(b"x");
    assert_eq!(sim.stations[0].clean(None, true), Clean::Failed);
    connect(&mut sim);
    sim.stations[0].send(&payload(10_000, 9));
    let (mut got, mut settled) = (Vec::new(), Vec::new());
    assert!(run(&mut sim, &mut got, &mut settled, 300.0, |s, _, _| on_air(s)));
    let a = &mut sim.stations[0];
    let Clean::Done { drop, limbo } = a.clean(None, true) else {
        panic!()
    };
    assert_eq!(drop.1, 10_001);
    assert_eq!(a.pending(), a.limbo_bytes());
    assert_eq!(a.clean(None, true), Clean::Empty);
    assert_eq!(
        a.clean(None, false),
        Clean::Done {
            drop: (drop.1, drop.1),
            limbo
        }
    );
}

/// The link ends with the limbo unsettled: it goes as unknown, split at
/// the first byte never acknowledged.
#[test]
fn a_limbo_unsettled_at_the_end_is_unknown() {
    let cut = std::rc::Rc::new(std::cell::Cell::new(f64::INFINITY));
    let c = cut.clone();
    let mut sim = pair(10.0, 6, move |_, _, t| t >= c.get());
    connect(&mut sim);
    sim.stations[0].send(&payload(20_000, 3));
    let (mut got, mut settled) = (Vec::new(), Vec::new());
    assert!(run(&mut sim, &mut got, &mut settled, 300.0, |s, _, _| on_air(s)));
    cut.set(sim.t);
    let Clean::Done { limbo, .. } = sim.stations[0].clean(None, false) else {
        panic!()
    };
    assert!(limbo.0 < limbo.1);
    sim.stations[0].disconnect();
    let s = sim.stations[0].take_settled().expect("settled at the goodbye");
    assert!(s.unknown && s.from == limbo.0 && s.to == limbo.1, "{s:?}");
    assert!(s.split >= s.from && s.split <= s.to);
    assert_eq!(sim.stations[0].take_settled(), None);
}

fn quiet_pair(idle: [Option<f64>; 2]) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let mut cfg = [Config::new("N0ABC"), Config::new("W1AW")];
    for (c, s) in cfg.iter_mut().zip(idle) {
        c.idle_close_s = s;
    }
    let [ca, cb] = cfg;
    let a = Station::new(ca, air.clone());
    let mut b = Station::new(cb, air.clone());
    b.listen();
    Sim::new(a, b, Box::new(ModelMedium::new(10.0, 7)), air)
}

/// Without an idle close a quiet link stays up: its check-ins are answered.
#[test]
fn a_quiet_link_stays_up_by_default() {
    let mut sim = quiet_pair([None, None]);
    connect(&mut sim);
    sim.run_until(600.0, |_| false);
    assert_eq!(sim.stations.each_ref().map(Station::state), [State::Connected; 2]);
}

/// An idle close on either end, whichever side holds the turn: that end
/// closes it (IDLE), the other hears the goodbye (PEER), once nothing has
/// moved for that long.
#[test]
fn an_idle_link_closes_after_the_idle_time() {
    for (who, answer) in [(0, false), (1, false), (0, true), (1, true)] {
        let mut idle = [None; 2];
        idle[who] = Some(20.0);
        let mut sim = quiet_pair(idle);
        if answer {
            // The turn changes hands: B answers.
            sim.stations[1].send(b"hello back");
        }
        connect(&mut sim);
        sim.stations[0].send(b"hello");
        let n = if answer { 10 } else { 0 };
        assert!(sim.run_until(300.0, |s| s.stations[0].stats().bytes_received >= n
            && s.stations[1].stats().bytes_received >= 5));
        let t0 = sim.t;
        assert!(sim.run_until(t0 + 120.0, |s| {
            s.stations.iter().all(|x| x.state() == State::Disconnected)
        }));
        let mut want = [Some(End::Peer); 2];
        want[who] = Some(End::Idle { confirmed: true });
        assert_eq!(sim.stations.each_ref().map(Station::end), want, "{who} {answer}");
        assert!(sim.t - t0 > 15.0 && sim.t - t0 < 50.0, "closed at {:.0} s", sim.t - t0);
    }
}

/// Data still moving holds the idle close off, however short.
#[test]
fn data_moving_is_not_idle() {
    let mut sim = quiet_pair([Some(5.0), Some(5.0)]);
    connect(&mut sim);
    let up = payload(6_000, 4);
    sim.stations[0].send(&up);
    let ok = sim.run_until(600.0, |s| s.stations[1].stats().bytes_received >= up.len() as u64);
    assert!(ok, "states {:?}", sim.stations.each_ref().map(Station::state));
    assert!(sim.stations.iter().all(|s| s.end().is_none()));
}

/// The band dies on a quiet link that holds in the lifeline: a short idle
/// close says goodbye in the lifeline (unanswered: unconfirmed); a long
/// one changes nothing, the hold's own rules end it (nothing heard:
/// lost).
#[test]
fn an_idle_close_in_a_hold() {
    for (idle, want) in [(200.0, End::Idle { confirmed: false }), (1e9, End::LinkLost)] {
        let air: Arc<dyn Air> = Arc::new(ModelAir::default());
        let mut cfg = Config::new("N0ABC");
        cfg.idle_close_s = Some(idle);
        let a = Station::new(cfg, air.clone());
        let mut b = Station::new(Config::new("W1AW"), air.clone());
        b.listen();
        let mut m = ModelMedium::new(10.0, 8);
        m.drop_at = Some((60.0, -40.0));
        let mut sim = Sim::new(a, b, Box::new(m), air);
        connect(&mut sim);
        let mut held = false;
        let ended = sim.run_until(2_000.0, |s| {
            held |= s.stations[0].holding();
            s.stations[0].end().is_some()
        });
        assert!(ended && held, "{idle}: {:?}", sim.stations[0].state());
        assert_eq!(sim.stations[0].end(), Some(want), "{idle}");
        if idle < 1e3 {
            assert!(sim.t < 60.0 + idle + 60.0, "ended at {:.0} s", sim.t);
        }
    }
}

/// A and B on a path where a big transfer goes by the stream gear (as
/// `tests/stream.rs`), each with this idle close.
fn stream_pair(idle: Option<f64>) -> Sim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let cfg = |call: &str| Config {
        idle_close_s: idle,
        ..Config::new(call)
    };
    let a = Station::new(cfg("N0ABC"), air.clone());
    let mut b = Station::new(cfg("W1AW"), air.clone());
    b.listen();
    Sim::new(a, b, Box::new(ModelMedium::new(-6.0, 3)), air)
}

#[test]
fn a_stream_is_not_idle() {
    let data = payload(30_000, 1);
    let mut sim = stream_pair(Some(60.0));
    connect(&mut sim);
    sim.stations[0].send(&data);
    let mut got = Vec::new();
    let mut settled = Vec::new();
    run(&mut sim, &mut got, &mut settled, 3_600.0, |s, got, _| {
        got.len() >= data.len() || s.stations.iter().any(|x| x.state() != State::Connected)
    });
    assert_eq!(
        got.len(),
        data.len(),
        "ended at {:.0} s: {:?}",
        sim.t,
        sim.stations[0].end()
    );
    assert_eq!(got, data);
    assert!(sim.stations[1].stats().stream_blocks >= 2, "it streamed");
    // Then it is idle, and closes.
    let by = sim.t + 600.0;
    assert!(sim.run_until(by, |s| s.stations.iter().all(|x| x.state() == State::Disconnected)));
    let ends = [sim.stations[0].end(), sim.stations[1].end()];
    assert!(ends.contains(&Some(End::Idle { confirmed: true })), "{ends:?}");
}

#[test]
fn a_clean_with_a_stream_block_out_fails_and_drops_nothing() {
    let data = payload(30_000, 1);
    let mut sim = stream_pair(None);
    connect(&mut sim);
    sim.stations[0].send(&data);
    assert!(sim.run_until(3_600.0, |s| s.stations[0].streaming()), "it streams");
    for vara in [true, false] {
        assert_eq!(sim.stations[0].clean(None, vara), Clean::Failed);
        assert_eq!(sim.stations[0].clean(Some(0), vara), Clean::Failed);
    }
    assert_eq!(sim.stations[0].limbo(), None);
    let mut got = Vec::new();
    let mut settled = Vec::new();
    assert!(run(&mut sim, &mut got, &mut settled, 3_600.0, |_, got, _| got.len()
        >= data.len()));
    assert_eq!(got, data);
    assert!(settled.is_empty(), "{settled:?}");
}
