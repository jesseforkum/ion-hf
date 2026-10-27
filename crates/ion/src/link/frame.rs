//! What goes inside bursts.
//!
//! **Frames** fill one FEC codeword each (32 bytes at the floor, 180 above):
//!
//! ```text
//! kind:u8  flags:u8  field:u24le  len:u8  payload[len]  zero pad  crc32:u32le
//! ```
//!
//! `field` is the byte offset of the payload in the sender's stream for
//! data frames, and the session for control frames. Offsets (not frame
//! numbers) let the sender re-cut unacknowledged data into smaller frames
//! when it drops to a slower level. The CRC covers everything before it.
//!
//! **Tiny messages** ride in a header-only burst: 4 bits of kind in the
//! burst's `user` word plus a 16-bit `extra`, for ACKs and the like that
//! must get through at the floor in a fraction of a second.
//!
//! ACKs also carry the channel report (`super::report`): two class bits in
//! a tiny ACK's kind or a ConnAck's `extra`, and four bytes more in a full
//! ACK.

use super::report::{ChanClass, ChanReport};

pub const HEADER: usize = 6;
pub const CRC: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Payload: "FROMCALL TOCALL" and the caller's highest level.
    ConnReq,
    Data,
    /// Payload: see [`AckInfo`]. (Small bursts are answered by a tiny ACK.)
    Ack,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::ConnReq => 1,
            Kind::Data => 3,
            Kind::Ack => 4,
        }
    }

    fn from_code(c: u8) -> Option<Kind> {
        Some(match c {
            1 => Kind::ConnReq,
            3 => Kind::Data,
            4 => Kind::Ack,
            _ => return None,
        })
    }
}

/// Flag bits (ACKs).
pub mod flags {
    /// The receiving side has data and wants the turn.
    pub const WANT_TURN: u8 = 1;
    /// The receiving side wants to disconnect.
    pub const WANT_DISC: u8 = 2;
    /// The frames the bitmap misses are stored for HARQ (see `link::harq`).
    pub const STORED: u8 = 4;
    /// ConnReq: the caller can compress its stream (`link::compress`,
    /// record type CM1) and asks the called station to do the same.
    pub const COMPRESS: u8 = 8;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub flags: u8,
    /// Stream offset (data) or session (control), 24 bits.
    pub field: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: Kind, field: u32) -> Self {
        Self {
            kind,
            flags: 0,
            field: field & 0xFF_FFFF,
            payload: Vec::new(),
        }
    }

    pub fn max_payload(size: usize) -> usize {
        (size - HEADER - CRC).min(255)
    }

    /// Exactly `size` bytes. Panics if the payload does not fit.
    pub fn encode(&self, size: usize) -> Vec<u8> {
        assert!(
            self.payload.len() <= Self::max_payload(size),
            "payload {} > {}",
            self.payload.len(),
            Self::max_payload(size)
        );
        let mut b = Vec::with_capacity(size);
        b.push(self.kind.code());
        b.push(self.flags);
        b.extend_from_slice(&self.field.to_le_bytes()[..3]);
        b.push(self.payload.len() as u8);
        b.extend_from_slice(&self.payload);
        b.resize(size - CRC, 0);
        let crc = crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// `None` if the CRC or the header is bad.
    pub fn decode(b: &[u8]) -> Option<Frame> {
        if b.len() < HEADER + CRC {
            return None;
        }
        let (body, tail) = b.split_at(b.len() - CRC);
        if crc32(body) != u32::from_le_bytes(tail.try_into().ok()?) {
            return None;
        }
        let len = body[5] as usize;
        if HEADER + len > body.len() {
            return None;
        }
        Some(Frame {
            kind: Kind::from_code(body[0])?,
            flags: body[1],
            field: u32::from_le_bytes([body[2], body[3], body[4], 0]),
            payload: body[HEADER..HEADER + len].to_vec(),
        })
    }
}

/// A full ACK (for bursts too big for a tiny one).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AckInfo {
    /// Which data burst this answers (mod 4).
    pub burst: u8,
    /// Bit i set: frame i of that burst arrived.
    pub bitmap: u64,
    pub snr_db: f32,
    /// Everything before this stream offset has been delivered (24 bits).
    pub delivered: u32,
    /// The channel report, if the ACK carries one.
    pub report: Option<ChanReport>,
}

impl AckInfo {
    pub fn to_bytes(self) -> Vec<u8> {
        let mut b = Vec::with_capacity(13 + ChanReport::BYTES);
        b.push(self.burst);
        b.extend_from_slice(&self.bitmap.to_le_bytes());
        b.push(snr_q(self.snr_db, 8) as u8);
        b.extend_from_slice(&self.delivered.to_le_bytes()[..3]);
        if let Some(r) = self.report {
            b.extend_from_slice(&r.to_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<AckInfo> {
        if b.len() < 13 {
            return None;
        }
        Some(AckInfo {
            burst: b[0] & 3,
            bitmap: u64::from_le_bytes(b[1..9].try_into().ok()?),
            snr_db: snr_from_q(b[9] as u32),
            delivered: u32::from_le_bytes([b[10], b[11], b[12], 0]),
            report: ChanReport::from_bytes(&b[13..]),
        })
    }
}

/// SNR on the air: `bits` wide, 1 dB steps from -24 dB (6 bits: -24..+39).
pub(crate) fn snr_q(snr_db: f32, bits: u32) -> u32 {
    let max = (1u32 << bits) - 1;
    (snr_db.round() + 24.0).clamp(0.0, max as f32) as u32
}

pub(crate) fn snr_from_q(q: u32) -> f32 {
    q as f32 - 24.0
}

/// Header-only control messages.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tiny {
    /// Frames of data burst `burst` (mod 4) that arrived: bit i = frame i.
    Ack {
        burst: u8,
        bitmap: u8,
        snr_db: f32,
        want_turn: bool,
        want_disc: bool,
        /// The missing frames are stored for HARQ.
        stored: bool,
        /// Channel class (see `super::report`).
        class: ChanClass,
        /// The third of the passband the receiver wants slice bursts in
        /// (`crate::slots`: 0 the centre, 1 below, 2 above); always 0 with
        /// a class (no slice then).
        slot: u8,
    },
    ConnAck {
        snr_db: f32,
        /// Both directions of the link carry compressed records (the
        /// caller asked with [`flags::COMPRESS`]). Bit 8 of `extra`, which
        /// a station without compression leaves clear and ignores.
        compress: bool,
        /// The channel as the called station measured it (bits 6–7).
        class: ChanClass,
        /// Its slot (bits 9–10; see [`Tiny::Ack`]).
        slot: u8,
    },
    /// "Repeat your last ACK."
    Poll,
    /// "Your turn to send." With how well the giver hears the taker: the
    /// taker's own direction, which on a lopsided link differs from the way
    /// back (`None`: not reported).
    Turn {
        snr_db: Option<f32>,
    },
    /// "Still here, nothing to send."
    Idle,
    DiscReq,
    DiscAck,
}

/// Frames a tiny ACK can account for.
pub const TINY_ACK_FRAMES: usize = 6;

/// Lifeline messages (`docs/link.md`, Lifeline): header-only bursts in
/// `Wave::Lifeline`, whose 4-bit kinds are their own (0 and 5–15 unused).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Life {
    /// The holding sender's probe, with how it last heard the peer's
    /// Report (`None`: none yet in this hold).
    Probe {
        snr_db: Option<f32>,
    },
    /// The answer: how the probe was heard.
    Report {
        snr_db: f32,
    },
    /// Goodbye: `weak`, the hold ran out; else a host ended the link.
    Bye {
        weak: bool,
    },
    ByeAck,
}

impl Life {
    /// (kind, extra). SNRs as everywhere: 6 bits, 1 dB from -24 dB.
    pub fn encode(self) -> (u8, u16) {
        match self {
            Life::Probe { snr_db } => (1, snr_db.map_or(0, |s| 1 << 6 | snr_q(s, 6) as u16)),
            Life::Report { snr_db } => (2, snr_q(snr_db, 6) as u16),
            Life::Bye { weak } => (3, weak as u16),
            Life::ByeAck => (4, 0),
        }
    }

    pub fn decode(kind: u8, extra: u16) -> Option<Life> {
        Some(match kind {
            1 => Life::Probe {
                snr_db: (extra >> 6 & 1 == 1).then(|| snr_from_q((extra & 0x3F) as u32)),
            },
            2 => Life::Report {
                snr_db: snr_from_q((extra & 0x3F) as u32),
            },
            3 => Life::Bye { weak: extra & 1 == 1 },
            4 => Life::ByeAck,
            _ => return None,
        })
    }
}

impl Tiny {
    pub const CONN_ACK_KIND: u8 = 2;

    /// (kind, extra): 4 + 16 bits. A tiny ACK's kind carries the class,
    /// the HARQ flag and the slot, as plain / HARQ stored: 1 / 8 clear at
    /// the centre slot, 9 / 12 echo, 10 / 13 Doppler (echo and Doppler
    /// together go as Doppler: MFSK only either way), 11 / 14 clear with
    /// the slot below, 0 / 15 clear with the slot above.
    pub fn encode(self) -> (u8, u16) {
        match self {
            Tiny::Ack {
                burst,
                bitmap,
                snr_db,
                want_turn,
                want_disc,
                stored,
                class,
                slot,
            } => (
                match (class.doppler, class.echo, slot, stored) {
                    (true, _, _, false) => 10,
                    (true, _, _, true) => 13,
                    (false, true, _, false) => 9,
                    (false, true, _, true) => 12,
                    (false, false, 1, false) => 11,
                    (false, false, 1, true) => 14,
                    (false, false, 2, false) => 0,
                    (false, false, 2, true) => 15,
                    (false, false, _, false) => 1,
                    (false, false, _, true) => 8,
                },
                (burst as u16 & 3)
                    | (bitmap as u16 & 0x3F) << 2
                    | (snr_q(snr_db, 6) as u16) << 8
                    | (want_turn as u16) << 14
                    | (want_disc as u16) << 15,
            ),
            Tiny::ConnAck {
                snr_db,
                compress,
                class,
                slot,
            } => (
                2,
                snr_q(snr_db, 6) as u16
                    | (class.bits() as u16) << 6
                    | (compress as u16) << 8
                    | ((slot as u16).min(2) & 3) << 9,
            ),
            Tiny::Poll => (3, 0),
            Tiny::Turn { snr_db } => (4, snr_db.map_or(0, |s| 1 << 6 | snr_q(s, 6) as u16)),
            Tiny::Idle => (5, 0),
            Tiny::DiscReq => (6, 0),
            Tiny::DiscAck => (7, 0),
        }
    }

    pub fn decode(kind: u8, extra: u16) -> Option<Tiny> {
        Some(match kind {
            0 | 1 | 8..=15 => Tiny::Ack {
                burst: (extra & 3) as u8,
                bitmap: (extra >> 2 & 0x3F) as u8,
                snr_db: snr_from_q((extra >> 8 & 0x3F) as u32),
                want_turn: extra >> 14 & 1 == 1,
                want_disc: extra >> 15 & 1 == 1,
                stored: matches!(kind, 8 | 12..=15),
                class: ChanClass {
                    echo: matches!(kind, 9 | 12),
                    doppler: matches!(kind, 10 | 13),
                },
                slot: match kind {
                    11 | 14 => 1,
                    0 | 15 => 2,
                    _ => 0,
                },
            },
            2 => Tiny::ConnAck {
                snr_db: snr_from_q((extra & 0x3F) as u32),
                compress: extra >> 8 & 1 == 1,
                class: ChanClass::from_bits((extra >> 6 & 3) as u8),
                slot: match extra >> 9 & 3 {
                    3 => 0,
                    s => s as u8,
                },
            },
            3 => Tiny::Poll,
            4 => Tiny::Turn {
                snr_db: (extra >> 6 & 1 == 1).then(|| snr_from_q((extra & 0x3F) as u32)),
            },
            5 => Tiny::Idle,
            6 => Tiny::DiscReq,
            7 => Tiny::DiscAck,
            _ => return None,
        })
    }
}

/// CRC-32 (IEEE 802.3, reflected, init and xorout all ones).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn frames_round_trip_and_reject_damage() {
        let mut f = Frame::new(Kind::Data, 0x12_3456);
        f.flags = flags::WANT_TURN;
        f.payload = b"hello ion".to_vec();
        let b = f.encode(32);
        assert_eq!(b.len(), 32);
        assert_eq!(Frame::decode(&b), Some(f));
        for i in [0, 3, 20, 31] {
            let mut bad = b.clone();
            bad[i] ^= 0x10;
            assert_eq!(Frame::decode(&bad), None, "byte {i}");
        }
        assert_eq!(Frame::decode(&[0u8; 32]), None);
        assert_eq!(Frame::max_payload(32), 22);
        assert_eq!(Frame::max_payload(180), 170);
    }

    #[test]
    fn acks_round_trip() {
        let a = AckInfo {
            burst: 2,
            bitmap: 0x8000_0000_0000_0005,
            snr_db: -3.0,
            delivered: 0xAB_CDEF,
            report: None,
        };
        assert_eq!(AckInfo::from_bytes(&a.to_bytes()), Some(a));
        let r = AckInfo {
            report: Some(ChanReport {
                class: ChanClass {
                    echo: true,
                    doppler: false,
                },
                eff_snr_db: -7.0,
                delay_ms: 6.75,
                doppler_hz: 0.5,
                slot: 2,
            }),
            ..a
        };
        assert_eq!(r.to_bytes().len(), 17);
        assert_eq!(AckInfo::from_bytes(&r.to_bytes()), Some(r));
        for t in [
            Tiny::Ack {
                burst: 3,
                bitmap: 0b10_1101,
                snr_db: 12.0,
                want_turn: true,
                want_disc: false,
                stored: false,
                class: ChanClass::default(),
                slot: 0,
            },
            Tiny::Ack {
                burst: 1,
                bitmap: 0b00_0110,
                snr_db: -2.0,
                want_turn: false,
                want_disc: true,
                stored: true,
                class: ChanClass::default(),
                slot: 2,
            },
            Tiny::ConnAck {
                snr_db: -9.0,
                compress: false,
                class: ChanClass::default(),
                slot: 0,
            },
            Tiny::ConnAck {
                snr_db: 3.0,
                compress: true,
                class: ChanClass {
                    echo: true,
                    doppler: true,
                },
                slot: 1,
            },
            Tiny::ConnAck {
                snr_db: 3.0,
                compress: true,
                class: ChanClass::default(),
                slot: 2,
            },
            Tiny::Poll,
            Tiny::Turn { snr_db: None },
            Tiny::Turn { snr_db: Some(-7.0) },
            Tiny::Turn { snr_db: Some(21.0) },
            Tiny::Idle,
            Tiny::DiscReq,
            Tiny::DiscAck,
        ] {
            let (k, e) = t.encode();
            assert!(k < 16);
            assert_eq!(Tiny::decode(k, e), Some(t));
        }
        // Every class and slot with and without HARQ storage. Echo with
        // Doppler goes as Doppler, and a class sends no slot.
        let ack = |class, slot, stored| Tiny::Ack {
            burst: 2,
            bitmap: 0b11_0011,
            snr_db: 5.0,
            want_turn: false,
            want_disc: false,
            stored,
            class,
            slot,
        };
        let mut kinds = Vec::new();
        for (c, slot) in [(0, 0), (1, 0), (2, 0), (0, 1), (0, 2)] {
            for stored in [false, true] {
                let t = ack(ChanClass::from_bits(c), slot, stored);
                let (k, e) = t.encode();
                assert_eq!(Tiny::decode(k, e), Some(t));
                kinds.push(k);
            }
        }
        assert_eq!(kinds, [1, 8, 9, 12, 10, 13, 11, 14, 0, 15]);
        for m in [
            Life::Probe { snr_db: None },
            Life::Probe { snr_db: Some(-17.0) },
            Life::Report { snr_db: -24.0 },
            Life::Report { snr_db: 12.0 },
            Life::Bye { weak: true },
            Life::Bye { weak: false },
            Life::ByeAck,
        ] {
            let (k, e) = m.encode();
            assert!(k < 16);
            assert_eq!(Life::decode(k, e), Some(m));
        }
        assert_eq!(Life::decode(0, 0), None);
        let (k, e) = ack(ChanClass::from_bits(3), 2, true).encode();
        assert_eq!(Tiny::decode(k, e), Some(ack(ChanClass::from_bits(2), 0, true)));
        // SNR saturates instead of wrapping.
        let (k, e) = Tiny::ConnAck {
            snr_db: 80.0,
            compress: false,
            class: ChanClass::default(),
            slot: 0,
        }
        .encode();
        assert_eq!(
            Tiny::decode(k, e),
            Some(Tiny::ConnAck {
                snr_db: 39.0,
                compress: false,
                class: ChanClass::default(),
                slot: 0
            })
        );
    }
}
