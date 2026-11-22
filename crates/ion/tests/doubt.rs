//! A data burst that arrives wiped (cut short, or into a receiver not yet
//! back from its own transmission) reads far below the path and may look
//! like fast fading. One such burst does not send the link to the floor
//! (`Config::doubt_db`).

use std::sync::Arc;

use ion::level::{LADDER, Wave};
use ion::link::audio::{AudioMedium, PhyAir};
use ion::link::sim::{Medium, Sim};
use ion::link::{Air, Config, Incoming, Outgoing, State, Station};
use ion_dsp::channel::Profile;

/// The audio path, with A's data bursts numbered `wipe` (from 0) arriving
/// with their header right and nothing else: no frame, −24 dB, 30 Hz of
/// Doppler.
struct Wiping {
    inner: [AudioMedium; 2],
    wipe: Vec<usize>,
    /// Lost with the reading the path has (no wild one).
    lose: Vec<usize>,
    /// A's data bursts from this one on read a 6 ms echo.
    echo: Option<usize>,
    seen: usize,
}

impl Medium for Wiping {
    fn carry(&mut self, from: usize, burst: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        let mut inc = self.inner[from].carry(from, burst, start, end)?;
        if from == 0 && self.echo.is_some_and(|n| self.seen >= n) {
            inc.chan.delay_ms = 6.0;
        }
        if from == 0 && burst.code < LADDER.len() && !burst.frames.is_empty() {
            let k = self.seen;
            self.seen += 1;
            if self.lose.contains(&k) {
                inc.frames.fill(None);
                inc.soft = None;
                inc.grid = None;
            }
            if self.wipe.contains(&k) {
                inc.frames.fill(None);
                inc.soft = None;
                inc.grid = None;
                inc.snr_db = -24.0;
                inc.chan.eff_snr_db = -24.0;
                inc.chan.doppler_hz = 30.0;
            }
        }
        Some(inc)
    }
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

/// A sends 60 kB at 26 dB on AWGN with the bursts `wipe` wiped: the levels
/// of A's data bursts in order, how many B passed over, and what B got.
fn run(wipe: &[usize]) -> (Vec<usize>, u32, usize) {
    run_with(wipe, &[], None)
}

fn run_with(wipe: &[usize], lose: &[usize], echo: Option<usize>) -> (Vec<usize>, u32, usize) {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let a = Station::new(Config::new("N0ABC"), air.clone());
    let mut b = Station::new(Config::new("W1AW"), air.clone());
    b.listen();
    let m = Wiping {
        inner: [
            AudioMedium::new(Profile::Awgn, 26.0, 3),
            AudioMedium::new(Profile::Awgn, 26.0, 4),
        ],
        wipe: wipe.to_vec(),
        lose: lose.to_vec(),
        echo,
        seen: 0,
    };
    let mut sim = Sim::new(a, b, Box::new(m), air);
    let data = payload(60_000);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let mut got = Vec::new();
    while sim.t < 300.0 && got.len() < data.len() {
        sim.step();
        got.extend(sim.stations[1].take_received());
    }
    assert!(data.starts_with(&got));
    assert!(sim.stations.iter().all(|s| s.state() == State::Connected));
    let levels = sim
        .log
        .iter()
        .filter(|e| e.from == 0 && e.code < LADDER.len() && e.frames > 0)
        .map(|e| e.code)
        .collect();
    (levels, sim.stations[1].stats().doubted, got.len())
}

#[test]
fn one_wiped_burst_does_not_send_the_link_to_the_floor() {
    let (clean, doubted, got) = run(&[]);
    assert_eq!((doubted, got), (0, 60_000));
    let top = *clean.iter().max().unwrap();
    let k = clean.iter().position(|&l| l == top).unwrap() + 2;
    let (levels, doubted, got) = run(&[k]);
    eprintln!("clean {clean:?}\nwiped burst {k}: {levels:?}");
    assert_eq!((doubted, got), (1, 60_000));
    // The burst after the wiped one is still an OFDM level, not far down.
    let after = levels[k + 1];
    assert!(
        matches!(LADDER[after].wave, Wave::Ofdm(_)),
        "level {after} after the wiped burst"
    );
    assert!(after + 4 >= top, "level {after} after the wiped burst, top {top}");
    assert!(levels[k + 1..].iter().all(|&l| matches!(LADDER[l].wave, Wave::Ofdm(_))));
}

/// Two in a row: the second is believed, and costs a step, not the ladder
/// (`Config::aim_fall_db`, `loss_step_db`).
#[test]
fn two_wiped_bursts_in_a_row_step_down_but_not_to_the_floor() {
    let (clean, ..) = run(&[]);
    let top = *clean.iter().max().unwrap();
    let k = clean.iter().position(|&l| l == top).unwrap() + 2;
    let (levels, doubted, got) = run(&[k, k + 1]);
    eprintln!("wiped bursts {k}, {}: {levels:?}", k + 1);
    assert_eq!((doubted, got), (1, 60_000));
    let after = levels[k + 2];
    assert!(after < top, "level {after} after two wiped bursts");
    assert!(
        matches!(LADDER[after].wave, Wave::Ofdm(_)) && after + 4 >= top,
        "level {after}"
    );
}

/// Data2G's rule: one burst lost at a level the report still allows is
/// sent again at that level; a second loss in a row steps down.
#[test]
fn one_lost_burst_is_sent_again_at_its_level() {
    let (clean, ..) = run(&[]);
    let top = *clean.iter().max().unwrap();
    let k = clean.iter().position(|&l| l == top).unwrap() + 2;
    let (once, _, got) = run_with(&[], &[k], None);
    eprintln!("lost burst {k}: {once:?}");
    assert_eq!(got, 60_000);
    assert_eq!((once[k], once[k + 1]), (top, top));
    let (twice, _, got) = run_with(&[], &[k, k + 1], None);
    eprintln!("lost bursts {k}, {}: {twice:?}", k + 1);
    assert_eq!(got, 60_000);
    assert_eq!((twice[k], twice[k + 1]), (top, top));
    assert!(twice[k + 2] < top, "level {} after two losses", twice[k + 2]);
    assert!(matches!(LADDER[twice[k + 2]].wave, Wave::Ofdm(_)));
}

/// An echo the receiver starts to report (a filter's edge reads as one)
/// on a link whose bursts go on coming through whole in the normal guard
/// does not move it to the long one, nor does one burst lost; two lost in
/// a row do. An echo there from the call on is believed from the start.
#[test]
fn an_echo_report_does_not_move_a_link_whose_bursts_come_through() {
    let normal = |l: &usize| LADDER[*l].wave == Wave::Ofdm(ion::level::Guard::Normal);
    let (clean, ..) = run(&[]);
    let top = *clean.iter().max().unwrap();
    let e = clean.iter().position(|&l| l == top).unwrap() + 1;
    let (levels, _, got) = run_with(&[], &[], Some(e));
    eprintln!("echo reported from burst {e}: {levels:?}");
    assert_eq!(got, 60_000);
    assert!(levels.iter().all(normal), "{levels:?}");
    let k = e + 3;
    let (levels, _, got) = run_with(&[], &[k], Some(e));
    eprintln!("echo reported, burst {k} lost: {levels:?}");
    assert_eq!(got, 60_000);
    assert!(levels.iter().all(normal), "{levels:?}");
    let (levels, _, got) = run_with(&[], &[k, k + 1], Some(e));
    eprintln!("echo reported, bursts {k} and {} lost: {levels:?}", k + 1);
    assert_eq!(got, 60_000);
    assert!(levels[k + 2..].iter().all(|l| !normal(l)), "{levels:?}");
    let (levels, ..) = run_with(&[], &[], Some(0));
    eprintln!("echo reported from the first burst: {levels:?}");
    assert!(levels[2..].iter().all(|l| !normal(l)), "{levels:?}");
}
