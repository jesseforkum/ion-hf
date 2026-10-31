//! Decode plots: on clean audio the equalised OFDM symbols sit on the
//! constellation's points at unit power, and the MFSK tones are clear
//! winners.

use ion::level::LADDER;
use ion::link::audio::{self, PLOT_MAX, Receivers, RxPlot};
use ion::link::frame::{Frame, Kind};
use ion::link::station::{Outgoing, user_bits};
use ion_dsp::channel::add_awgn;
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

fn heard(level: usize, plot: bool) -> Vec<Option<RxPlot>> {
    let mut f = Frame::new(Kind::Data, 9);
    f.payload = b"plot test".to_vec();
    let out = Outgoing {
        td_ease: false,
        wave: LADDER[level].wave,
        code: level,
        user: user_bits(level, 9, 1),
        extra: 0,
        frames: vec![f.encode(LADDER[level].frame_bytes()); 3],
        spans: Vec::new(),
        cue: false,
    };
    let mut x = vec![0.0; 4800];
    x.extend(audio::transmit(&out));
    x.extend(vec![0.0; 4800]);
    let p = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    add_awgn(&mut x, noise_sigma(p, 30.0, 2400.0, AUDIO_RATE), &mut NoiseGen::new(7));
    let mut rx = Receivers::new();
    rx.set_plot(plot);
    let mut bursts = Vec::new();
    rx.process(&x, &mut bursts);
    assert_eq!(bursts.len(), 1, "L{level}");
    rx.take_plots()
}

#[test]
fn equalised_symbols_sit_on_the_points() {
    // A spread (low-PAPR) level, 8PSK, and plain 64QAM.
    for level in [10, 12, 16] {
        let plots = heard(level, true);
        let Some(RxPlot::Iq { constellation, symbols }) = &plots[0] else {
            panic!("L{level}: {:?}", plots[0]);
        };
        assert!(!symbols.is_empty() && symbols.len() <= PLOT_MAX, "L{level}");
        let pts = constellation.unit_points();
        let evm = symbols
            .iter()
            .map(|z| pts.iter().map(|p| (*z - *p).norm_sqr()).fold(f32::MAX, f32::min))
            .sum::<f32>()
            / symbols.len() as f32;
        let power = symbols.iter().map(|z| z.norm_sqr()).sum::<f32>() / symbols.len() as f32;
        assert!((power - 1.0).abs() < 0.1, "L{level}: mean power {power}");
        // The low-PAPR levels' clipping costs them about 12 dB of EVM.
        assert!(evm < 0.1, "L{level}: mean squared error {evm}");
    }
}

#[test]
fn mfsk_tones_are_clear() {
    for level in [0, 4] {
        let plots = heard(level, true);
        let Some(RxPlot::Tones(t)) = &plots[0] else {
            panic!("L{level}: {:?}", plots[0]);
        };
        assert!(!t.is_empty() && t.len() <= PLOT_MAX, "L{level}");
        let top = t.iter().map(|[_, y]| y).sum::<f32>() / t.len() as f32;
        assert!(top > 0.9, "L{level}: mean top share {top}");
    }
}

#[test]
fn no_plots_unless_asked() {
    assert!(heard(10, false).is_empty());
}
