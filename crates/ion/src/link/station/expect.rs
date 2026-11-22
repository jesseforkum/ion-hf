//! The bursts a station expects next from its peer, for receivers that
//! can decode a weak header against them (`phy::PhyRx::expect`).

use super::{CONTROL, MAX_FRAMES, Role, State, Station, TINY, frame_bits, user_bits};
use crate::level::{FrameSize, Guard, LADDER, Wave};
use crate::link::frame::Tiny;
use crate::link::harq;

/// One burst header the station expects: what an [`super::Outgoing`]
/// with these fields would carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    pub wave: Wave,
    /// Ladder level, [`CONTROL`] or [`TINY`].
    pub code: usize,
    pub user: u16,
    pub extra: u16,
    /// Coded bits of the data part (0 for a header-only burst).
    pub bits: usize,
}

/// Data bursts are expected only at levels whose effective-SNR threshold
/// is at most this, dB: headers fail below about +1 dB (WGN) to +5 dB
/// (fading), and the data of a faster level is lost there anyway.
const EXPECT_MAX_DB: f32 = 5.0;

const OFDM_GUARDS: [Guard; 3] = [Guard::Normal, Guard::Long, Guard::Slice];

impl Station {
    /// The OFDM bursts the peer may send next, while a link is up (empty
    /// otherwise: a call comes in MFSK, and outside a link nothing is
    /// expected). Receiving side: the peer's data bursts at every OFDM
    /// level up to `EXPECT_MAX_DB` (a sender may jump several levels at
    /// once), with and without the increments for what we store, every
    /// frame count up to `Config::max_burst_s` (longer bursts come only
    /// while every frame gets through, far above where a header fails),
    /// the next two burst numbers; its poll and idle, and its turn while
    /// we ask for it. Sending side: a full ACK frame in the waveforms ACKs
    /// come in. Saying goodbye: the goodbyes.
    pub fn expected(&self) -> Vec<Expected> {
        let mut out = Vec::new();
        if !self.cfg.expect_headers || !matches!(self.state, State::Connected | State::Disconnecting) {
            return out;
        }
        let tiny = |out: &mut Vec<Expected>, (kind, extra): (u8, u16)| {
            for g in OFDM_GUARDS {
                out.push(Expected {
                    wave: Wave::Ofdm(g),
                    code: TINY,
                    user: user_bits(TINY, self.session, kind),
                    extra,
                    bits: 0,
                });
            }
        };
        // A goodbye or a turn taken from a guess would end the link or
        // hand it over: expected only once our own side has asked for it.
        if self.state == State::Disconnecting {
            tiny(&mut out, Tiny::DiscReq.encode());
            tiny(&mut out, Tiny::DiscAck.encode());
            return out;
        }
        match self.role {
            Role::Receiver => {
                tiny(&mut out, Tiny::Poll.encode());
                tiny(&mut out, Tiny::Idle.encode());
                // Our ACKs ask for the turn while we have data; it carries
                // how well the peer hears us: its 6-bit report, or none.
                if self.has_data() {
                    let (kind, _) = Tiny::Turn { snr_db: None }.encode();
                    for extra in std::iter::once(0).chain((0..64).map(|q| 1 << 6 | q)) {
                        tiny(&mut out, (kind, extra));
                    }
                }
                self.expected_data(&mut out);
            }
            Role::Sender => {
                let mut waves = vec![self.cur_wave()];
                waves.extend(self.peer_ack_wave);
                for wave in waves {
                    if matches!(wave, Wave::Ofdm(_)) && !out.iter().any(|e| e.code == CONTROL && e.wave == wave) {
                        out.push(Expected {
                            wave,
                            code: CONTROL,
                            user: user_bits(CONTROL, self.session, 0),
                            extra: 0,
                            bits: frame_bits(CONTROL, 1),
                        });
                    }
                }
            }
        }
        out
    }

    /// The peer's next data burst, as `Station::data_burst` would build it.
    fn expected_data(&self, out: &mut Vec<Expected>) {
        let last = self.last_ack.0;
        let ids = [(last + 1) & 3, (last + 2) & 3];
        let held = self.harq.held();
        let stored_wave = LADDER[self.rx_level].wave;
        for (level, &lv) in LADDER.iter().enumerate() {
            if lv.frame != FrameSize::Long
                || crate::level::is_narrow(level) != self.link500
                || !matches!(lv.wave, Wave::Ofdm(_))
                || self.cfg.eff_thresholds_db.get(level).is_none_or(|&t| t > EXPECT_MAX_DB)
            {
                continue;
            }
            let slot = lv.coded_bits();
            let max_bits = self.air.max_bits(lv.wave, level);
            let fits =
                |bits: usize| bits <= max_bits && self.air.duration(lv.wave, level, bits) <= self.cfg.max_burst_s;
            let mut layouts = vec![(0, 0usize)];
            if !held.is_empty() && lv.wave == stored_wave {
                let parity: usize = held.iter().map(|&h| harq::increment(h, slot)).sum();
                layouts.push((held.len(), parity));
            }
            for (np, parity) in layouts {
                let hq = harq::check_bits(np);
                // Increments alone when the fresh frames do not fit.
                let f0 = usize::from(np == 0);
                for f in f0..=MAX_FRAMES.saturating_sub(np) {
                    let bits = parity + f * slot;
                    if f > f0 && !fits(bits) {
                        break;
                    }
                    for &id in &ids {
                        out.push(Expected {
                            wave: lv.wave,
                            code: level,
                            user: user_bits(level, self.session, id | hq << 2),
                            extra: 0,
                            bits,
                        });
                    }
                }
            }
        }
    }
}
