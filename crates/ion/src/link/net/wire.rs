//! What goes on the air: frame types, control subtypes, header-only
//! message kinds, and how calls, SNRs and offsets are packed.

use super::*;

/// Slot number meaning "everyone" (bulletins) or "nobody yet".
pub const ALL: u8 = 63;

/// Short-code control frames.
pub(super) const CTRL_CODE: usize = 0;

/// Frame types (top two bits of the frame's flags; the low six are a slot).
pub(super) const T_UP: u8 = 0;
pub(super) const T_DOWN: u8 = 1;
pub(super) const T_CTRL: u8 = 2;

/// Control frame subtypes (first payload byte).
pub(super) const C_BEACON: u8 = 1;
pub(super) const C_CHECKIN: u8 = 2;
pub(super) const C_REPORT: u8 = 3;
pub(super) const C_OVERHEARD: u8 = 4;
pub(super) const C_ACKS: u8 = 5;
pub(super) const C_RXLIST: u8 = 6;
/// "Looking for a net" from a station that hears no beacon.
pub(super) const C_SEEK: u8 = 7;
/// NCS: (leaf, relay) pairs; relay [`ALL`] means "polled directly".
pub(super) const C_TREE: u8 = 8;
/// A relay to its leaf: your slot, I am your relay (with its call: its ID).
pub(super) const C_CHILD: u8 = 9;
/// A relay to NCS: which of its leaves answered, and how well it hears them.
pub(super) const C_KIDS: u8 = 10;
/// A member's check-in line, in one or two parts (relayed like a report).
pub(super) const C_LINE: u8 = 11;
/// NCS (or a relay, to its leaves): the objects on offer.
pub(super) const C_OFFER: u8 = 12;

/// Header-only message kinds (the burst's `user` high nibble).
pub(super) const K_POLL: u8 = 1;
pub(super) const K_POLLACK: u8 = 2;
pub(super) const K_ACK: u8 = 3;
pub(super) const K_IDLE: u8 = 4;
pub(super) const K_RELAY: u8 = 5;
pub(super) const K_CLOSE: u8 = 6;
/// Times NCS sends Close. A Close's `extra` says how many more follow from
/// its sender (low 2 bits), and this bit that a relay sent it.
pub(super) const CLOSES: u32 = 3;
pub(super) const CLOSE_RELAYED: u16 = 4;
/// A relay polls one of its leaves, and acknowledges its answer (as
/// [`K_POLL`] and [`K_ACK`]).
pub(super) const K_SUBPOLL: u8 = 7;
pub(super) const K_SUBACK: u8 = 8;

/// `user` high-nibble bit of a data or control burst that does not answer
/// NCS's poll: a check-in, or traffic between a relay and its leaves.
pub(super) const ASIDE: u8 = 8;

/// Protocol version, sent at check-in.
pub const PROTOCOL: u8 = 3;

/// What a station can do, announced at check-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Caps {
    /// Has rig control: NCS may send it to another frequency.
    pub qsy: bool,
    /// Can use a 2.75 kHz channel (otherwise 2.4 kHz).
    pub wide: bool,
    /// Willing to relay for others.
    pub relay: bool,
    /// An operator is at the station.
    pub attended: bool,
    /// Checks in by chorus when NCS offers chorus slots: every station at
    /// once, each in its own 400 Hz sub-band.
    pub chorus: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            qsy: false,
            wide: false,
            relay: true,
            attended: true,
            chorus: false,
        }
    }
}

impl Caps {
    pub fn to_byte(self) -> u8 {
        self.qsy as u8
            | (self.wide as u8) << 1
            | (self.relay as u8) << 2
            | (self.attended as u8) << 3
            | (self.chorus as u8) << 4
    }

    /// Bit 4 is chorus check-ins; bits 5–6 are reserved (QSY sub-nets); bit 7
    /// is [`CAPS_COMPRESS`].
    pub fn from_byte(b: u8) -> Self {
        Self {
            qsy: b & 1 != 0,
            wide: b & 2 != 0,
            relay: b & 4 != 0,
            attended: b & 8 != 0,
            chorus: b & 16 != 0,
        }
    }

    pub fn bandwidth_hz(self) -> u32 {
        if self.wide { 2750 } else { 2400 }
    }
}

/// How urgent a station's most urgent traffic is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Precedence {
    #[default]
    Routine,
    Welfare,
    Priority,
    Emergency,
}

/// The traffic a station lists, sent at check-in and in its reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Traffic {
    /// Items listed (7 means 7 or more).
    pub items: u8,
    /// Of the most urgent.
    pub precedence: Precedence,
    /// Short time: in and out.
    pub short: bool,
}

impl Traffic {
    /// Bits 0–2 items, 3–4 precedence, 5 short time.
    pub fn to_byte(self) -> u8 {
        self.items.min(7) | (self.precedence as u8) << 3 | (self.short as u8) << 5
    }

    pub fn from_byte(b: u8) -> Self {
        Self {
            items: b & 7,
            precedence: match b >> 3 & 3 {
                0 => Precedence::Routine,
                1 => Precedence::Welfare,
                2 => Precedence::Priority,
                _ => Precedence::Emergency,
            },
            short: b & 32 != 0,
        }
    }

    /// Where NCS polls it in a round: Emergency, Priority, short time, the rest.
    pub(super) fn rank(self) -> u8 {
        match self.precedence {
            Precedence::Emergency => 0,
            Precedence::Priority => 1,
            _ if self.short => 2,
            _ => 3,
        }
    }
}

/// Longest check-in line, bytes.
pub const MAX_LINE: usize = 40;
/// Text bytes in one `C_LINE` frame.
pub(super) const LINE_PART: usize = 20;

/// `line` cut to [`MAX_LINE`] bytes on a character boundary.
pub(super) fn clip_line(line: &str) -> String {
    let mut n = line.len().min(MAX_LINE);
    while !line.is_char_boundary(n) {
        n -= 1;
    }
    line[..n].to_string()
}

/// One bit per `C_LINE` frame `line` takes.
pub(super) fn line_mask(line: &str) -> u8 {
    if line.len() > LINE_PART { 3 } else { 1 }
}

/// The `C_LINE` payloads of `slot`'s line: subtype, then the slot with the
/// part number in bit 6 and "two parts" in bit 7, then up to 20 bytes.
pub(super) fn line_payloads(slot: u8, line: &str) -> Vec<Vec<u8>> {
    let b = line.as_bytes();
    let parts = if b.len() > LINE_PART { 2 } else { 1 };
    (0..parts)
        .map(|i| {
            let mut p = vec![C_LINE, slot & 63 | (i as u8) << 6 | (parts as u8 - 1) << 7];
            p.extend(&b[(i * LINE_PART).min(b.len())..((i + 1) * LINE_PART).min(b.len())]);
            p
        })
        .collect()
}

/// Check-in caps byte, bit 7 (of the reserved 4–7): the member can take
/// compressed direct messages ([`NetConfig::compress`]). Not part of
/// [`Caps`]: relayed check-ins drop it, and NCS then sends plain.
pub const CAPS_COMPRESS: u8 = 0x80;
/// Direct messages to such a member: bit 15 of the length prefix marks a
/// [`crate::link::compress::pack`]ed message (so they are at most 32 767 bytes).
pub(super) const LEN_PACKED: u16 = 0x8000;

/// This software's version as sent at check-in: major in the high
/// nibble, minor in the low.
pub fn software_version() -> u8 {
    let mut v = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|x| x.parse::<u8>().unwrap_or(0));
    let major = v.next().unwrap_or(0).min(15);
    let minor = v.next().unwrap_or(0).min(15);
    major << 4 | minor
}

/// "1.2" from a version byte.
pub fn version_string(v: u8) -> String {
    format!("{}.{}", v >> 4, v & 15)
}

const CALL_CHARS: &[u8] = b" 0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ/";
/// Longest callsign that packs into 5 bytes (38⁷ < 2⁴⁰).
pub const MAX_CALL: usize = 7;

/// Packs a callsign (up to 7 of A–Z, 0–9, /) into 5 bytes.
pub fn pack_call(call: &str) -> [u8; 5] {
    let mut v: u64 = 0;
    let up = call.to_ascii_uppercase();
    let chars: Vec<u8> = up.bytes().take(MAX_CALL).collect();
    for i in 0..MAX_CALL {
        let c = chars.get(i).copied().unwrap_or(b' ');
        let d = CALL_CHARS.iter().position(|&x| x == c).unwrap_or(0) as u64;
        v = v * CALL_CHARS.len() as u64 + d;
    }
    let b = v.to_le_bytes();
    [b[0], b[1], b[2], b[3], b[4]]
}

pub fn unpack_call(b: &[u8]) -> String {
    let mut v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], 0, 0, 0]);
    let n = CALL_CHARS.len() as u64;
    let mut out = vec![b' '; MAX_CALL];
    for i in (0..MAX_CALL).rev() {
        out[i] = CALL_CHARS[(v % n) as usize];
        v /= n;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// The 8-bit id a net name goes by on the air.
pub fn net_id(name: &str) -> u8 {
    let x = crc32(name.trim().to_ascii_uppercase().as_bytes());
    (x ^ x >> 8 ^ x >> 16 ^ x >> 24) as u8
}

/// A control frame to `slot` ([`ALL`]: everyone).
pub(super) fn ctrl_frame(slot: u8, payload: Vec<u8>) -> Frame {
    let mut f = Frame::new(Kind::Data, 0);
    f.flags = T_CTRL << 6 | (slot & 63);
    f.payload = payload;
    f
}

/// A check-in's control payload: subtype, packed call, how well we hear
/// NCS, capabilities, protocol and software version, traffic (11 bytes).
pub(super) fn checkin_payload(call: &str, snr_ncs_db: f32, caps: Caps, version: u8, traffic: Traffic) -> Vec<u8> {
    let mut p = vec![C_CHECKIN];
    p.extend(pack_call(call));
    p.push(snr_q(snr_ncs_db));
    p.extend([caps.to_byte(), PROTOCOL, version, traffic.to_byte()]);
    p
}

/// The whole check-in frame as it goes on the air (one short-code
/// codeword's information bytes), with no traffic listed.
pub fn checkin_frame_bytes(call: &str, snr_ncs_db: f32, caps: Caps, version: u8) -> Vec<u8> {
    ctrl_frame(
        ALL,
        checkin_payload(call, snr_ncs_db, caps, version, Traffic::default()),
    )
    .encode(LADDER[CTRL_CODE].frame_bytes())
}

/// A template of member `slot`'s report frame listing `hears` stations
/// heard, with every field NCS cannot know zero (for priors).
pub fn report_frame_bytes(slot: u8, hears: usize) -> Vec<u8> {
    let mut p = vec![C_REPORT, slot];
    p.resize(REPORT_LEN + 2 * hears, 0);
    ctrl_frame(slot, p).encode(LADDER[CTRL_CODE].frame_bytes())
}

/// A report's fixed part: subtype, slot, how well it hears NCS, bytes
/// queued (u24), bulletin block (u16) and shortfall, direct bytes held
/// (u24), traffic, objects held. The stations it hears follow, 2 bytes each.
pub const REPORT_LEN: usize = 14;

/// PHY header (`user`, `extra`) of member `slot`'s Idle in net `id`,
/// hearing NCS at `snr_db`.
pub fn idle_header(id: u8, slot: u8, snr_db: f32) -> (u16, u16) {
    (
        user_bits(TINY, id, K_IDLE),
        slot as u16 & 63 | (snr_q(snr_db) as u16) << 6,
    )
}

/// The headers a primed NCS expects of member `slot`'s answer to a poll
/// at the floor (R0): an Idle, whose SNR byte it cannot know, or one short
/// frame, whose "more" flag it cannot know, nor the burst number unless
/// the poll named it (`burst`).
pub fn floor_reply_headers(id: u8, slot: u8, burst: Option<u8>) -> (ExpectedHeader, ExpectedHeader) {
    let Wave::Mfsk(mode) = LADDER[FLOOR].wave else {
        unreachable!("the floor is MFSK")
    };
    let (user, extra) = idle_header(id, slot, 0.0);
    let idle = ExpectedHeader {
        extra_mask: 63 << 6,
        ..ExpectedHeader::exact(mode, user, extra & 63, 0)
    };
    let data = ExpectedHeader {
        user_mask: if burst.is_some() { 4 << 12 } else { 7 << 12 },
        ..ExpectedHeader::exact(
            mode,
            user_bits(FLOOR, id, burst.unwrap_or(0) & 3),
            0,
            frame_bits(FLOOR, 1),
        )
    };
    (idle, data)
}

/// The check-in's call, if `f` is a check-in frame.
pub fn checkin_call(f: &Frame) -> Option<String> {
    let p = &f.payload;
    (f.flags >> 6 == T_CTRL && p.first() == Some(&C_CHECKIN) && p.len() >= 7).then(|| unpack_call(&p[1..6]))
}

/// `user` of the PHY header of a check-in burst in net `id`.
pub fn checkin_user(id: u8) -> u16 {
    user_bits(CTRL_CODE, id, ASIDE)
}

/// An SNR on the air: dB + 24, 0 to 63.
pub(super) fn snr_q(snr: f32) -> u8 {
    (snr.round() + 24.0).clamp(0.0, 63.0) as u8
}

pub(super) fn snr_from_q(q: u8) -> f32 {
    (q & 63) as f32 - 24.0
}

pub(super) fn u24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

pub(super) fn put_u24(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes()[..3]);
}

/// A 24-bit offset from the air, made full near `near`.
pub(super) fn widen(off24: u32, near: u32) -> u32 {
    let d = off24.wrapping_sub(near) & 0xFF_FFFF;
    if d < 0x80_0000 {
        near.wrapping_add(d)
    } else {
        near.wrapping_sub(0x100_0000 - d)
    }
}

/// An 8-bit message number from the air, made full near `near` (`None`:
/// from before the start).
pub(super) fn widen8(s: u8, near: u32) -> Option<u32> {
    let d = s.wrapping_sub(near as u8);
    if d < 0x80 {
        Some(near + d as u32)
    } else {
        near.checked_sub(0x100 - d as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_round_trip() {
        for c in ["K1ABC", "N0CALL", "W1AW", "W1AW/7", "K", "VE3ABC/"] {
            assert_eq!(unpack_call(&pack_call(c)), c);
        }
        assert_eq!(unpack_call(&pack_call("k1abc")), "K1ABC");
        assert_eq!(unpack_call(&pack_call("DL1ABCD/P")), "DL1ABCD");
    }

    #[test]
    fn traffic_and_lines_pack() {
        for b in 0..64u8 {
            assert_eq!(Traffic::from_byte(b).to_byte(), b);
        }
        let t = Traffic {
            items: 9,
            precedence: Precedence::Priority,
            short: true,
        };
        assert_eq!(Traffic::from_byte(t.to_byte()).items, 7);
        let line = "Trousdale County ARES, EOC net control!!";
        assert_eq!(line.len(), MAX_LINE);
        let p = line_payloads(5, line);
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|x| x.len() <= Frame::max_payload(32)));
        assert_eq!((p[0][1], p[1][1]), (5 | 0x80, 5 | 0xC0));
        assert_eq!(line_payloads(5, "")[0], vec![C_LINE, 5]);
        assert_eq!(clip_line(&"é".repeat(30)).len(), 40);
        assert_eq!(clip_line(&format!("a{}", "é".repeat(30))).len(), 39);
    }

    #[test]
    fn caps_round_trip() {
        for b in 0..32u8 {
            assert_eq!(Caps::from_byte(b).to_byte(), b);
        }
        let c = Caps {
            qsy: true,
            wide: true,
            relay: false,
            attended: false,
            chorus: true,
        };
        assert_eq!(Caps::from_byte(c.to_byte()), c);
        assert_eq!(c.bandwidth_hz(), 2750);
        assert_eq!(version_string(0x12), "1.2");
    }
}
