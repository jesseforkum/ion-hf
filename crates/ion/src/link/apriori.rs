//! A priori decoding of net check-ins: what the receiver already knows of a
//! frame goes to the LDPC decoder as prior LLRs, so it decodes at a lower
//! SNR. Every pass still ends in the frame's CRC-32, so a wrong guess cannot
//! invent a check-in.
//!
//! A check-in is one short-code control frame of 32 bytes (256 bits):
//!
//! ```text
//! byte  0      kind (Data)              format
//!       1      flags (control, to all)  format
//!       2..5   field (0)                format
//!       5      payload length (11)      format
//!       6      subtype (check-in)       format
//!       7..12  packed callsign          roster
//!       12     how well it hears NCS    unknown
//!       13     capabilities             unknown
//!       14     protocol version         format
//!       15     software version         unknown
//!       16     traffic                  unknown
//!       17..28 zero padding             format
//!       28..32 CRC-32                   unknown
//! ```

use std::ops::Range;

use super::audio::{bits_to_bytes, bytes_to_bits};
use super::frame::Frame;
use super::net::{Caps, REPORT_LEN, checkin_call, checkin_frame_bytes, report_frame_bytes};
use crate::fec;
use crate::phy_mfsk::{MfskBurst, decode_short_frames_prior};

/// Prior LLR of a bit the receiver is sure of. Large enough to pin the
/// bit, below the decoder's clip (30) so the code can still overrule it.
pub const SURE: f32 = 20.0;

/// Bytes of the packed callsign.
const CALL: Range<usize> = 7..12;
/// Bytes no assumption fixes: SNR, capabilities, software version,
/// traffic, CRC.
const UNKNOWN: [Range<usize>; 4] = [12..13, 13..14, 15..17, 28..32];

/// What the receiver assumes about a check-in before decoding it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assume<'a> {
    /// Nothing: plain decoding.
    Nothing,
    /// The frame is a check-in: its fixed fields are known.
    Format,
    /// A check-in from this call.
    Call(&'a str),
}

/// True when byte `b` of a check-in frame is fixed by `assume`.
fn known_byte(b: usize, assume: Assume) -> bool {
    match assume {
        Assume::Nothing => false,
        Assume::Format => !UNKNOWN.iter().any(|r| r.contains(&b)) && !CALL.contains(&b),
        Assume::Call(_) => !UNKNOWN.iter().any(|r| r.contains(&b)),
    }
}

/// A priori LLRs of a check-in frame's [`fec::K_SHORT`] information bits
/// under `assume`: ±[`SURE`] where the bit is known, 0 elsewhere. Empty
/// for [`Assume::Nothing`].
pub fn checkin_prior(assume: Assume) -> Vec<f32> {
    let call = match assume {
        Assume::Nothing => return Vec::new(),
        Assume::Format => "",
        Assume::Call(c) => c,
    };
    // A template frame: the unknown bytes are wrong in it, but they get
    // no prior.
    let bits = bytes_to_bits(&checkin_frame_bytes(call, 0.0, Caps::default(), 0));
    debug_assert_eq!(bits.len(), fec::K_SHORT);
    bits.iter()
        .enumerate()
        .map(|(i, &b)| match (known_byte(i / 8, assume), b) {
            (false, _) => 0.0,
            (true, 0) => SURE,
            (true, _) => -SURE,
        })
        .collect()
}

/// A priori LLRs of member `slot`'s report frame listing `hears`
/// stations (0..=4), as NCS expects it when it polls that member at the
/// floor. Known: the frame header (kind, flags with the slot, field 0,
/// length), the subtype and slot bytes, and the zero padding (up to 128
/// of the 256 bits with nothing heard). The values in it are unknown, and
/// so is the CRC.
pub fn report_prior(slot: u8, hears: usize) -> Vec<f32> {
    let bits = bytes_to_bits(&report_frame_bytes(slot, hears));
    let len = REPORT_LEN + 2 * hears;
    // Header (6), subtype and slot (2), then padding up to the CRC.
    let known = |b: usize| b < 8 || (6 + len..fec::K_SHORT / 8 - 4).contains(&b);
    bits.iter()
        .enumerate()
        .map(|(i, &b)| match (known(i / 8), b) {
            (false, _) => 0.0,
            (true, 0) => SURE,
            (true, _) => -SURE,
        })
        .collect()
}

/// Which pass of [`decode_checkin`] found the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    Blind,
    Format,
    /// The roster entry whose call was assumed.
    Roster(usize),
}

/// A decoded check-in.
#[derive(Clone, Debug)]
pub struct Checkin {
    pub frame: Frame,
    pub call: String,
    pub pass: Pass,
}

/// One decoding pass over a burst carrying one check-in codeword: the
/// frame and its call if it decodes, passes its CRC and is a check-in.
pub fn try_checkin(burst: &MfskBurst, assume: Assume) -> Option<(Frame, String)> {
    let prior = checkin_prior(assume);
    let info = decode_short_frames_prior(burst, &prior).into_iter().next()??;
    let frame = Frame::decode(&bits_to_bytes(&info))?;
    let call = checkin_call(&frame)?;
    Some((frame, call))
}

/// Decodes a check-in burst with everything NCS knows, cheapest first:
/// blind, then the format, then each call of `roster` in turn, each pass
/// only when the earlier ones failed.
pub fn decode_checkin(burst: &MfskBurst, roster: &[&str]) -> Option<Checkin> {
    let passes = [(Assume::Nothing, Pass::Blind), (Assume::Format, Pass::Format)]
        .into_iter()
        .chain(
            roster
                .iter()
                .enumerate()
                .map(|(i, c)| (Assume::Call(c), Pass::Roster(i))),
        );
    for (assume, pass) in passes {
        if let Some((frame, call)) = try_checkin(burst, assume) {
            return Some(Checkin { frame, call, pass });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prior_marks_exactly_the_fixed_bytes() {
        // Bytes that change when the variable fields change must carry no
        // prior under Format; the call's bytes are known under Call.
        let a = checkin_frame_bytes("K1ABC", -3.0, Caps::default(), 0x01);
        let b = checkin_frame_bytes(
            "W1AW",
            7.0,
            Caps {
                qsy: true,
                ..Caps::default()
            },
            0x23,
        );
        let f = checkin_prior(Assume::Format);
        let c = checkin_prior(Assume::Call("K1ABC"));
        for byte in 0..a.len() {
            let fixed = (0..8).all(|k| f[8 * byte + k] != 0.0);
            let free = (0..8).all(|k| f[8 * byte + k] == 0.0);
            assert!(fixed || free, "byte {byte} half known");
            if a[byte] != b[byte] {
                assert!(free, "byte {byte} varies but is marked known");
            }
            if CALL.contains(&byte) {
                assert!(free);
                for k in 0..8 {
                    let bit = a[byte] >> (7 - k) & 1;
                    assert_eq!(c[8 * byte + k], if bit == 0 { SURE } else { -SURE });
                }
            }
        }
        // A report: the prior agrees with a real one whatever its values.
        for h in 0..=4 {
            let mut fr = Frame::decode(&report_frame_bytes(9, h)).unwrap();
            for (i, b) in fr.payload.iter_mut().enumerate().skip(2) {
                *b = 0xA5 ^ i as u8;
            }
            let bits = bytes_to_bits(&fr.encode(32));
            let p = report_prior(9, h);
            assert!(bits.iter().zip(&p).all(|(&b, &l)| l == 0.0 || (l > 0.0) == (b == 0)));
            assert_eq!(
                p.iter().filter(|&&l| l != 0.0).count(),
                8 * (8 + 22 - REPORT_LEN - 2 * h)
            );
        }
        let known_f = f.iter().filter(|&&l| l != 0.0).count();
        let known_c = c.iter().filter(|&&l| l != 0.0).count();
        assert_eq!((known_f, known_c), (152, 192));
    }
}
