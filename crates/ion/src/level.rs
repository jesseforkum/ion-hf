//! The speed ladder: which waveform, constellation, code rate and frame
//! size each level uses, from the robust MFSK floor up to 64QAM OFDM.

use crate::fec::{self, Rate};
use crate::phy::{Constellation, Pilots};
use crate::phy_mfsk::{MfskLayout, MfskMode};

/// OFDM shape: guard interval and width. Normal (4 ms) and long (10 ms,
/// NVIS-type echoes) fill the band; slice is Normal's spacing and guard on
/// 650 Hz, the same power on a quarter of the carriers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Guard {
    Normal,
    Long,
    Slice,
}

/// Which waveform a burst is sent in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wave {
    Mfsk(MfskMode),
    Ofdm(Guard),
    /// [`LIFELINE_MODE`] in [`LIFELINE_LAYOUT`] on the link's centre: not a
    /// level, the link's lifeline below the floor (`docs/link.md`,
    /// Lifeline).
    Lifeline,
}

/// The lifeline's waveform: MFSK R1 stretched 7 times, 32 tones 10.2 Hz
/// apart in 337 Hz, about 6 dB under R0 (`docs/mfsk.md`, Below the floor).
pub const LIFELINE_LAYOUT: MfskLayout = MfskLayout::scaled(7);
pub const LIFELINE_MODE: MfskMode = MfskMode::R1;

/// Codeword size of a level's data frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameSize {
    /// [`fec::K_SHORT`] bits at rate 1/4: small frames for the slow floor.
    Short,
    /// [`fec::K`] bits at the level's rate.
    Long,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub wave: Wave,
    /// OFDM only.
    pub constellation: Constellation,
    /// Code rate: the main code's (long frames) or the GF(32) code's (short
    /// frames in MFSK, `fec::nb`; control frames are always 1/4).
    pub rate: Rate,
    pub frame: FrameSize,
    /// OFDM only: pilot pattern of the data symbols.
    pub pilots: Pilots,
    /// OFDM only: data symbols in the low-PAPR mode
    /// ([`crate::phy::TxOptions::low_papr`]), more mean power at the same
    /// peak.
    pub low_papr: bool,
}

impl Level {
    pub fn frame_bytes(&self) -> usize {
        match self.frame {
            FrameSize::Short => fec::K_SHORT / 8,
            FrameSize::Long => fec::K / 8,
        }
    }

    /// Coded bits each frame occupies on the air.
    pub fn coded_bits(&self) -> usize {
        match self.frame {
            // At 1/4 the binary short code's length; above it, five bits a
            // GF(32) symbol.
            FrameSize::Short if self.rate == Rate::R1_4 => fec::n_short(),
            FrameSize::Short => 5 * fec::nb::n_for(self.rate),
            FrameSize::Long => self.rate.n(),
        }
    }
}

const fn mfsk(m: MfskMode) -> Level {
    mfsk_at(m, Rate::R1_4)
}

/// MFSK with GF(32) short frames at `rate`.
const fn mfsk_at(m: MfskMode, rate: Rate) -> Level {
    Level {
        wave: Wave::Mfsk(m),
        constellation: Constellation::Qpsk,
        rate,
        frame: FrameSize::Short,
        pilots: Pilots::Robust,
        low_papr: false,
    }
}

const fn ofdm(g: Guard, c: Constellation, r: Rate) -> Level {
    Level {
        wave: Wave::Ofdm(g),
        constellation: c,
        rate: r,
        frame: FrameSize::Long,
        pilots: Pilots::Robust,
        low_papr: false,
    }
}

/// `l` in the low-PAPR mode.
const fn lp(l: Level) -> Level {
    Level { low_papr: true, ..l }
}

/// Most robust first. The QPSK and 8PSK OFDM levels use the low-PAPR mode;
/// 16QAM and 64QAM stay plain, where the spread receiver loses on fading.
pub const LADDER: [Level; 17] = [
    mfsk(MfskMode::R0),
    mfsk(MfskMode::R1),
    // R1 at the faster GF(32) rates bridges the gap up to the slice.
    mfsk_at(MfskMode::R1, Rate::R1_3),
    mfsk_at(MfskMode::R1, Rate::R1_2),
    mfsk_at(MfskMode::R1, Rate::R2_3),
    lp(ofdm(Guard::Slice, Constellation::Qpsk, Rate::R1_3)),
    lp(ofdm(Guard::Slice, Constellation::Qpsk, Rate::R1_2)),
    lp(ofdm(Guard::Long, Constellation::Qpsk, Rate::R1_4)),
    lp(ofdm(Guard::Long, Constellation::Qpsk, Rate::R1_2)),
    lp(ofdm(Guard::Normal, Constellation::Qpsk, Rate::R1_3)),
    lp(ofdm(Guard::Normal, Constellation::Qpsk, Rate::R1_2)),
    lp(ofdm(Guard::Normal, Constellation::Qpsk, Rate::R2_3)),
    lp(ofdm(Guard::Normal, Constellation::Psk8, Rate::R2_3)),
    ofdm(Guard::Normal, Constellation::Qam16, Rate::R2_3),
    ofdm(Guard::Normal, Constellation::Qam16, Rate::R3_4),
    ofdm(Guard::Normal, Constellation::Qam64, Rate::R3_4),
    ofdm(Guard::Normal, Constellation::Qam64, Rate::R5_6),
];

/// Where every link starts.
pub const FLOOR: usize = 0;
pub const FLOOR_WAVE: Wave = Wave::Mfsk(MfskMode::R0);

/// Gear-shift targets: SNR (dB in 2.4 kHz) at which each level loses about
/// 10% of frames, midway between AWGN and CCIR poor. Used when
/// `Config::eff_snr` is off, and by the model channel.
pub const THRESHOLDS_DB: [f32; 17] = [
    -12.0, -9.0, -8.0, -6.5, -4.5, -3.0, -0.5, 2.0, 6.5, 2.5, 5.5, 8.0, 12.0, 13.5, 15.5, 20.5, 23.5,
];

/// Gear-shift targets against the effective SNR (EESM, `crate::chan`),
/// which already holds what fading costs: each level's AWGN threshold.
pub const EFF_THRESHOLDS_DB: [f32; 17] = [
    -13.0, -11.0, -10.3, -8.9, -7.4, -5.0, -2.5, -0.5, 3.8, 0.0, 3.0, 5.0, 9.5, 11.0, 12.0, 17.0, 19.0,
];

/// Each level's mean power on the air against a steady carrier at
/// `ion_dsp::TX_LEVEL`, dB, as `link::audio::transmit_with` sends it at the
/// link's OFDM air gain (checked by
/// `tests/ladder.rs::slice_levels_average_full_ofdm_power`). Used by
/// `Config::power_aware` to move an SNR report from one level to another.
pub const AIR_MEAN_DB: [f32; 17] = [
    0.0, 0.0, 0.0, 0.0, 0.0, -2.7, -2.5, -2.4, -2.4, -2.5, -2.5, -2.5, -3.4, -5.2, -5.1, -7.3, -7.3,
];

pub fn thresholds_db() -> Vec<f32> {
    THRESHOLDS_DB.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_gets_faster_within_a_waveform() {
        let info_per_coded = |l: &Level| {
            let k = match l.frame {
                FrameSize::Short => fec::K_SHORT,
                FrameSize::Long => fec::K,
            };
            k as f32 / l.coded_bits() as f32 * l.constellation.bits_per_symbol() as f32
        };
        for w in LADDER.windows(2) {
            if w[0].wave == w[1].wave {
                assert!(
                    info_per_coded(&w[1]) > info_per_coded(&w[0]),
                    "{:?} -> {:?}",
                    w[0],
                    w[1]
                );
            }
        }
        assert_eq!(LADDER[FLOOR].wave, FLOOR_WAVE);
        assert_eq!(THRESHOLDS_DB.len(), LADDER.len());
    }
}
