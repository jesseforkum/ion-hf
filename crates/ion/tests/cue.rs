//! MFSK cues on the link: agreed at connect (`frame::caps::CUE`), sent only
//! to a peer that agreed. A station with `Config::cue` off stands in for
//! Ion HF 0.1.0-alpha.4, which neither offers nor accepts them.

use std::sync::{Arc, Mutex};

use ion::level::{Guard, LADDER, Wave};
use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::frame::caps;
use ion::link::sim::{Medium, Sim};
use ion::link::{Air, Incoming, Outgoing, Station};
use ion_dsp::ionos::{Mode, Reference};

/// Data bursts as carried: (from, full-band OFDM, cue, extra), and the
/// cues each side's receivers heard.
#[derive(Default)]
struct Seen {
    bursts: Vec<(usize, bool, bool, u16)>,
    cues_heard: [u64; 2],
}

struct Spy {
    inner: AudioMedium,
    seen: Arc<Mutex<Seen>>,
}

impl Medium for Spy {
    fn carry(&mut self, from: usize, b: &Outgoing, start: f64, end: f64) -> Option<Incoming> {
        let inc = self.inner.carry(from, b, start, end);
        let mut s = self.seen.lock().unwrap();
        if b.code < LADDER.len() {
            let full = matches!(b.wave, Wave::Ofdm(Guard::Normal | Guard::Long));
            s.bursts.push((from, full, b.cue, b.extra));
        }
        s.cues_heard[1 - from] = self.inner.receivers(1 - from).cues_heard();
        inc
    }
}

fn payload(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(151).wrapping_add(seed)).collect()
}

/// A calls B on IONOS WGN at `snr_db` (low enough for cues); each sends the
/// other data. Returns the sim and what the medium saw.
fn link(cue_a: bool, cue_b: bool, snr_db: f32) -> (Sim, Arc<Mutex<Seen>>) {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let cfg = |call: &str, cue: bool| {
        let mut c = audio::config(call);
        c.cue = cue;
        // Cues alone: the stream gear has its own tests (`tests/stream.rs`).
        c.stream = false;
        c
    };
    let a = Station::new(cfg("N0JLF", cue_a), air.clone());
    let mut b = Station::new(cfg("N0CALL", cue_b), air.clone());
    b.listen();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let medium = Spy {
        inner: AudioMedium::ionos(Mode::Wgn, 2, Reference::Peak, snr_db, 7),
        seen: seen.clone(),
    };
    let mut sim = Sim::new(a, b, Box::new(medium), air);
    sim.stations[0].connect("N0CALL", 0.0);
    sim.stations[0].send(&payload(1_500, 1));
    sim.stations[1].send(&payload(500, 2));
    let done = sim.run_until(1_800.0, |s| {
        s.stations[1].stats().bytes_received >= 1_500 && s.stations[0].stats().bytes_received >= 500
    });
    assert!(
        done,
        "a got {} of 500, b {} of 1500 in {:.0} s",
        sim.stations[0].stats().bytes_received,
        sim.stations[1].stats().bytes_received,
        sim.t
    );
    (sim, seen)
}

#[test]
fn a_station_without_cues_gets_none_and_the_transfer_completes() {
    // Either end without cues: the caller, then the called station.
    for (cue_a, cue_b) in [(true, false), (false, true)] {
        let (sim, seen) = link(cue_a, cue_b, 0.0);
        assert_eq!(sim.stations[0].caps(), 0);
        assert_eq!(sim.stations[1].caps(), 0);
        let s = seen.lock().unwrap();
        assert!(s.bursts.iter().any(|b| b.0 == 0) && s.bursts.iter().any(|b| b.0 == 1));
        assert!(sim.log.iter().all(|e| !e.cue));
        // No cue, and no HARQ parity field (it rides only in a cue).
        for &(from, full, cue, extra) in &s.bursts {
            assert!(!cue, "a cue from side {from}");
            assert!(!full || extra == 0, "extra {extra} from side {from}");
        }
        assert_eq!(s.cues_heard, [0, 0]);
    }
}

#[test]
fn two_stations_with_cues_use_them() {
    let (sim, seen) = link(true, true, 0.0);
    assert_eq!(sim.stations[0].caps(), caps::CUE);
    assert_eq!(sim.stations[1].caps(), caps::CUE);
    let s = seen.lock().unwrap();
    let cued = s.bursts.iter().filter(|b| b.2).count();
    assert!(cued > 0, "no cued bursts: {:?}", s.bursts);
    assert!(
        s.bursts.iter().all(|b| !b.2 || b.1),
        "a cue on a burst that is not full-band OFDM"
    );
    assert!(s.cues_heard[0] + s.cues_heard[1] > 0);
    assert_eq!(sim.log.iter().filter(|e| e.cue).count(), cued);
}
