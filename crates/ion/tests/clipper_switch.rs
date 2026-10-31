//! The whole-burst clipper switched while a link runs (`phy::set_td_clip`
//! is process-wide, so this file is its own test binary).

use std::sync::Arc;

use ion::level::LADDER;
use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::sim::Sim;
use ion::link::{Air, Station};
use ion::phy;
use ion_dsp::channel::Profile;

fn mean_power(code: usize) -> f32 {
    let lv = LADDER[code];
    let num = audio::numerology(match lv.wave {
        ion::level::Wave::Ofdm(g) => g,
        _ => unreachable!(),
    });
    let bits = vec![1u8; phy::capacity_bits_with(num, lv.pilots, lv.constellation, 20)];
    let a = phy::modulate_full(
        num,
        lv.constellation,
        0,
        0,
        &bits,
        num.center_hz(),
        &audio::tx_options(code),
    );
    a.iter().map(|x| x * x).sum::<f32>() / a.len() as f32
}

#[test]
fn the_clipper_switches_mid_link_and_the_transfer_completes() {
    phy::set_td_clip(false);
    let classic = mean_power(10);
    phy::set_td_clip(true);
    let burst = mean_power(10);
    let rise = 10.0 * (burst / classic).log10();
    assert!((0.5..3.0).contains(&rise), "QPSK 1/2 mean power {rise:+.2} dB");

    phy::set_td_clip(false);
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let a = Station::new(audio::config("N0JLF"), air.clone());
    let mut b = Station::new(audio::config("W1AW"), air.clone());
    b.listen();
    let medium = AudioMedium::new(Profile::Awgn, 12.0, 3).peak_referenced(true);
    let mut sim = Sim::new(a, b, Box::new(medium), air);
    let data: Vec<u8> = (0..12_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let n = data.len() as u64;
    assert!(sim.run_until(600.0, |s| s.stations[1].stats().bytes_received >= n / 3));
    phy::set_td_clip(true);
    assert!(sim.run_until(sim.t + 600.0, |s| s.stations[1].stats().bytes_received >= 2 * n / 3));
    phy::set_td_clip(false);
    assert!(sim.run_until(sim.t + 600.0, |s| s.stations[1].stats().bytes_received >= n));
    assert_eq!(sim.stations[1].take_received(), data);
}
