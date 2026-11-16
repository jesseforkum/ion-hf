//! The channel report: what the receiving side measured of the channel
//! (`crate::chan`), boiled down to what the sending side needs to pick
//! its waveform and level, and carried back in every ACK.
//!
//! **On the air.** Two class bits and an SNR:
//!
//! - *echo*: the echoes reach past the 4 ms guard of the slice and
//!   OFDM-normal levels (smoothed delay span over [`ECHO_MS`]); only the
//!   MFSK floor and the long-guard levels are used;
//! - *doppler*: the channel fades faster than the OFDM pilots can follow
//!   (smoothed Doppler spread over [`DOPPLER_HZ`]); only the MFSK floor is
//!   used;
//! - the SNR field of the ACK carries the **effective** SNR (EESM) instead
//!   of the mean when [`super::Config::eff_snr`] is on.
//!
//! A tiny ACK (16-bit `extra`, already full) carries the class, and the
//! slice slot (`crate::slots`), in its kind nibble (`frame::Tiny::encode`).
//! A ConnAck carries the class in bits 6–7 of `extra` (bits 0–5 are its
//! SNR) and the slot in 9–10. A full ACK (`AckInfo`) carries four more
//! bytes: class (and slot), effective SNR, delay span (0.25 ms steps) and
//! Doppler spread (0.25 Hz steps).

use crate::chan::ChannelMeas;

pub use crate::chan::{DOPPLER_HZ, ECHO_MS};
/// Weight of the newest burst in the receiver's running channel average.
const SMOOTH: f32 = 0.5;

/// Bursts in a row without a Doppler measurement after which the Doppler
/// spread is forgotten. The fast-fading class keeps the link on MFSK, which
/// measures none, so without this one bad OFDM estimate would hold the
/// link there for good; forgotten, the next OFDM burst measures again.
const DOPPLER_MEMORY: u32 = 4;

/// The two class bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChanClass {
    /// Echoes beyond the 4 ms guard.
    pub echo: bool,
    /// Fading too fast for OFDM.
    pub doppler: bool,
}

impl ChanClass {
    pub fn bits(self) -> u8 {
        self.echo as u8 | (self.doppler as u8) << 1
    }

    pub fn from_bits(b: u8) -> Self {
        Self {
            echo: b & 1 == 1,
            doppler: b & 2 == 2,
        }
    }
}

/// The receiving side's running view of the channel over a link: delay
/// and Doppler averaged over the data bursts heard (newest weighted
/// `SMOOTH`), so one odd burst does not flip the class.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChanTrack {
    delay_ms: Option<f32>,
    doppler_hz: Option<f32>,
    eff_snr_db: Option<f32>,
    since_doppler: u32,
}

impl ChanTrack {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Adds one burst's measurement.
    pub fn add(&mut self, m: &ChannelMeas) {
        let mix = |old: Option<f32>, new: f32| -> Option<f32> {
            if !new.is_finite() {
                return old;
            }
            Some(old.map_or(new, |o| o + SMOOTH * (new - o)))
        };
        self.delay_ms = mix(self.delay_ms, m.delay_ms);
        self.doppler_hz = mix(self.doppler_hz, m.doppler_hz);
        if m.doppler_hz.is_finite() {
            self.since_doppler = 0;
        } else {
            self.since_doppler += 1;
            if self.since_doppler >= DOPPLER_MEMORY {
                self.doppler_hz = None;
            }
        }
        if m.eff_snr_db.is_finite() {
            self.eff_snr_db = Some(m.eff_snr_db);
        }
    }

    pub fn class(&self) -> ChanClass {
        ChanClass {
            echo: self.delay_ms.is_some_and(|d| d > ECHO_MS),
            doppler: self.doppler_hz.is_some_and(|d| d > DOPPLER_HZ),
        }
    }

    /// Smoothed delay span (ms) and Doppler spread (Hz), NaN if none yet.
    pub fn spreads(&self) -> (f32, f32) {
        (self.delay_ms.unwrap_or(f32::NAN), self.doppler_hz.unwrap_or(f32::NAN))
    }

    /// The last burst's effective SNR, if it had one.
    pub fn eff_snr_db(&self) -> Option<f32> {
        self.eff_snr_db
    }
}

/// What a full ACK carries beyond the tiny one's fields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChanReport {
    pub class: ChanClass,
    pub eff_snr_db: f32,
    /// Smoothed delay span, ms (0.25 ms steps, at most 63.75).
    pub delay_ms: f32,
    /// Smoothed Doppler spread, Hz (0.25 Hz steps, at most 63.75).
    pub doppler_hz: f32,
    /// The slot the receiver wants slice bursts in (`crate::slots`; bits
    /// 2–3 of the class byte).
    pub slot: u8,
    /// The receiver asks for the slice levels only (QRM over the rest of
    /// the passband; bit 4 of the class byte, which older stations leave
    /// clear and ignore).
    pub narrow: bool,
}

impl ChanReport {
    pub const BYTES: usize = 4;

    pub fn to_bytes(self) -> [u8; 4] {
        let q = |v: f32| {
            if v.is_finite() {
                (v * 4.0).round().clamp(0.0, 254.0) as u8
            } else {
                255
            }
        };
        [
            self.class.bits() | (self.slot.min(2) & 3) << 2 | (self.narrow as u8) << 4,
            super::frame::snr_q(self.eff_snr_db, 8) as u8,
            q(self.delay_ms),
            q(self.doppler_hz),
        ]
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b: &[u8; 4] = b.get(..4)?.try_into().ok()?;
        let v = |x: u8| if x == 255 { f32::NAN } else { x as f32 / 4.0 };
        Some(Self {
            class: ChanClass::from_bits(b[0] & 3),
            eff_snr_db: super::frame::snr_from_q(b[1] as u32),
            delay_ms: v(b[2]),
            doppler_hz: v(b[3]),
            slot: match b[0] >> 2 & 3 {
                3 => 0,
                s => s,
            },
            narrow: b[0] >> 4 & 1 == 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_fading_class_clears_after_bursts_without_doppler() {
        let mut t = ChanTrack::default();
        t.add(&ChannelMeas {
            doppler_hz: 32.0,
            ..ChannelMeas::default()
        });
        assert!(t.class().doppler);
        for _ in 1..DOPPLER_MEMORY {
            t.add(&ChannelMeas::default());
            assert!(t.class().doppler);
        }
        t.add(&ChannelMeas::default());
        assert!(!t.class().doppler);
    }
}
