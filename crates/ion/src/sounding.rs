//! An over-the-air test of the whole speed ladder: a numbered sequence of
//! bursts, every waveform and level from the MFSK floor up, between two CW
//! IDs, and the analysis of a recording of it (for instance from a public
//! KiwiSDR or WebSDR somewhere else).
//!
//! The bursts use their own session (`SESSION`), so an Ion station that
//! hears them ignores them. Each data frame carries the sender's call, the
//! burst number, the level and the frame index, and is otherwise filled
//! with a known pattern, so the analysis tells a good copy from a bad one
//! without the sender's file.

use crate::fec::Rate;
use crate::level::{FrameSize, Guard, LADDER, Wave};
use crate::link::audio::{self, Receivers};
use crate::link::frame::{Frame, Kind};
use crate::link::harq::{HarqRx, Soft};
use crate::link::station::{Incoming, Outgoing, TINY, user_bits, user_session};
use crate::phy::Constellation;
use crate::phy_mfsk::MfskMode;

use ion_dsp::Modulator;

/// Session byte of sounding bursts.
pub const SESSION: u8 = 0xA5;
const RATE: f32 = 48_000.0;
/// Silence between bursts (the receivers need a moment to reset).
const GAP_S: f32 = 1.2;
/// Data bursts aim for about this long (one frame at the floor, which is
/// already longer).
const DATA_S: f64 = 3.0;

/// One burst of the sequence.
#[derive(Clone, Debug)]
pub struct Sent {
    pub seq: u16,
    /// Ladder level, or [`TINY`] for a header-only burst.
    pub code: usize,
    pub wave: Wave,
    pub frames: usize,
    /// Start in the sequence's audio, seconds.
    pub start_s: f64,
    pub dur_s: f64,
}

/// A short name for a level or waveform.
pub fn level_name(code: usize, wave: Wave) -> String {
    let w = match wave {
        Wave::Mfsk(MfskMode::R0) => "MFSK R0",
        Wave::Mfsk(MfskMode::R1) => "MFSK R1",
        Wave::Ofdm(Guard::Slice) => "OFDM slice",
        Wave::Ofdm(Guard::Long) => "OFDM long",
        Wave::Ofdm(Guard::Normal) => "OFDM",
    };
    if code == TINY {
        return format!("{w} header-only");
    }
    let Some(l) = LADDER.get(code) else {
        return format!("{w} code {code}");
    };
    if matches!(wave, Wave::Mfsk(_)) {
        // R1 at its faster GF(32) rates: "L4 MFSK R1 2/3".
        return match l.rate {
            Rate::R1_4 => format!("L{code} {w}"),
            Rate::R1_3 => format!("L{code} {w} 1/3"),
            Rate::R1_2 => format!("L{code} {w} 1/2"),
            _ => format!("L{code} {w} 2/3"),
        };
    }
    let c = match l.constellation {
        Constellation::Qpsk => "QPSK",
        Constellation::Psk8 => "8PSK",
        Constellation::Qam16 => "16QAM",
        Constellation::Qam64 => "64QAM",
    };
    let r = match l.rate {
        Rate::R1_4 => "1/4",
        Rate::R1_3 => "1/3",
        Rate::R1_2 => "1/2",
        Rate::R2_3 => "2/3",
        Rate::R3_4 => "3/4",
        Rate::R5_6 => "5/6",
    };
    format!("L{code} {w} {c} {r}")
}

fn payload(call: &str, seq: u16, code: usize, i: usize, max: usize) -> Vec<u8> {
    let mut p = format!("HS {call} {seq:03} L{code:02} F{i}|").into_bytes();
    let mut x: u32 = 0x9E37_79B9 ^ (seq as u32) << 8 ^ i as u32;
    while p.len() < max {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        p.push(b'A' + (x % 26) as u8);
    }
    p.truncate(max);
    p
}

/// `text` in CW at 20 wpm on the Ion centre (48 kHz, at the steady
/// carriers' level): the station ID.
pub fn cw(text: &str) -> Vec<f32> {
    let mut m = ion_dsp::CwMod::new(audio::CENTER_HZ, 20.0);
    m.push_text(text);
    m.finish();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4800];
    loop {
        let n = m.fill(&mut buf);
        out.extend_from_slice(&buf[..n]);
        if n < buf.len() {
            return out;
        }
    }
}

/// The test sequence for `call`: 48 kHz audio and what is in it.
///
/// CW ID; one header-only burst per waveform; every ladder level twice;
/// CW ID. About two and a half minutes.
pub fn sequence(call: &str) -> (Vec<f32>, Vec<Sent>) {
    sequence_with(call, 0.0)
}

/// [`sequence`], with OFDM bursts `ofdm_db` louder ([`audio::ofdm_gain`]);
/// the CW IDs and MFSK bursts stay at the usual level.
pub fn sequence_with(call: &str, ofdm_db: f32) -> (Vec<f32>, Vec<Sent>) {
    let ofdm_gain = audio::ofdm_gain(ofdm_db);
    let call = call.trim().to_ascii_uppercase();
    let air = audio::PhyAir;
    let mut a: Vec<f32> = vec![0.0; (0.5 * RATE) as usize];
    a.extend(cw(&format!("DE {call} ION TEST")));
    a.extend(std::iter::repeat_n(0.0, (GAP_S * RATE) as usize));
    let mut sent = Vec::new();
    let mut seq: u16 = 0;
    let push = |a: &mut Vec<f32>, b: Outgoing, frames: usize, sent: &mut Vec<Sent>, seq: &mut u16| {
        let start_s = a.len() as f64 / RATE as f64;
        let audio = audio::transmit_with(&b, ofdm_gain);
        sent.push(Sent {
            seq: *seq,
            code: b.code,
            wave: b.wave,
            frames,
            start_s,
            dur_s: audio.len() as f64 / RATE as f64,
        });
        a.extend(audio);
        a.extend(std::iter::repeat_n(0.0, (GAP_S * RATE) as usize));
        *seq += 1;
    };
    let waves = [
        Wave::Mfsk(MfskMode::R0),
        Wave::Mfsk(MfskMode::R1),
        Wave::Ofdm(Guard::Slice),
        Wave::Ofdm(Guard::Long),
        Wave::Ofdm(Guard::Normal),
    ];
    for w in waves {
        let b = Outgoing {
            td_ease: false,
            cue: false,
            wave: w,
            code: TINY,
            user: user_bits(TINY, SESSION, 1),
            extra: seq,
            frames: Vec::new(),
            spans: Vec::new(),
        };
        push(&mut a, b, 0, &mut sent, &mut seq);
    }
    for _round in 0..2 {
        for (level, lv) in LADDER.iter().enumerate() {
            let size = lv.frame_bytes();
            let max = Frame::max_payload(size);
            let mut n = 1;
            while n < 8
                && crate::link::Air::duration(&air, lv.wave, level, crate::link::station::frame_bits(level, n + 1))
                    <= DATA_S
            {
                n += 1;
            }
            let frames = (0..n)
                .map(|i| {
                    let mut f = Frame::new(Kind::Data, seq as u32);
                    f.payload = payload(&call, seq, level, i, max);
                    f.encode(size)
                })
                .collect();
            let b = Outgoing {
                td_ease: false,
                cue: false,
                wave: lv.wave,
                code: level,
                user: user_bits(level, SESSION, 0),
                extra: 0,
                frames,
                spans: Vec::new(),
            };
            push(&mut a, b, n, &mut sent, &mut seq);
        }
    }
    a.extend(cw(&format!("DE {call} SK")));
    a.extend(std::iter::repeat_n(0.0, (0.5 * RATE) as usize));
    (a, sent)
}

/// One burst found in a recording.
#[derive(Clone, Debug)]
pub struct Heard {
    /// When the receiver finished it, seconds into the recording.
    pub at_s: f64,
    pub code: usize,
    pub wave: Wave,
    pub snr_db: f32,
    /// Codewords decoded out of those in the burst (0 of 0 for a
    /// header-only burst).
    pub ok: usize,
    pub of: usize,
    /// Sequence number, from the payload (or a header-only burst's extra).
    pub seq: Option<u16>,
    /// Sender's call, from the payload.
    pub call: Option<String>,
}

/// Decodes every sounding burst in 48 kHz audio.
pub fn analyze(audio48: &[f32]) -> Vec<Heard> {
    let mut rx = Receivers::new();
    let mut harq = HarqRx::new();
    let mut out = Vec::new();
    let mut got: Vec<Incoming> = Vec::new();
    let chunk = 480;
    for (k, c) in audio48.chunks(chunk).enumerate() {
        got.clear();
        rx.process(c, &mut got);
        let at_s = ((k + 1) * chunk) as f64 / RATE as f64;
        for inc in got.drain(..) {
            if user_session(inc.user) != SESSION {
                continue;
            }
            let code = inc.code;
            if code == TINY {
                out.push(Heard {
                    at_s,
                    code,
                    wave: inc.wave,
                    snr_db: inc.snr_db,
                    ok: 0,
                    of: 0,
                    seq: Some(inc.extra),
                    call: None,
                });
                continue;
            }
            let frames: Vec<Option<Vec<u8>>> = match &inc.soft {
                Some(soft) if code < LADDER.len() && LADDER[code].frame == FrameSize::Long => {
                    harq.receive(code, 0, soft).frames
                }
                Some(Soft::Llrs(_)) | Some(Soft::Model(_)) | None => inc.frames.clone(),
            };
            let decoded: Vec<Option<Frame>> = frames.iter().map(|f| f.as_deref().and_then(Frame::decode)).collect();
            let ok = decoded.iter().filter(|f| f.is_some()).count();
            let first = decoded.iter().flatten().next();
            let (seq, call) = match first.map(|f| String::from_utf8_lossy(&f.payload).to_string()) {
                Some(s) => {
                    let mut w = s.split(' ');
                    let _ = w.next();
                    let call = w.next().map(str::to_string);
                    let seq = w.next().and_then(|x| x.parse().ok());
                    (seq, call)
                }
                None => (None, None),
            };
            out.push(Heard {
                at_s,
                code,
                wave: inc.wave,
                snr_db: inc.snr_db,
                ok,
                of: decoded.len(),
                seq,
                call,
            });
        }
    }
    out
}

/// Reads a WAV file (PCM 16-bit or float, any channel count, any rate) as
/// mono 48 kHz.
pub fn read_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut channels, mut rate, mut bits) = (0u16, 1usize, 0u32, 16u16);
    let mut data: Option<&[u8]> = None;
    let mut p = 12;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let len = u32::from_le_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as usize;
        let body = &bytes[p + 8..(p + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            fmt = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]).max(1) as usize;
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
            if fmt == 0xFFFE && body.len() >= 26 {
                fmt = u16::from_le_bytes([body[24], body[25]]);
            }
        } else if id == b"data" {
            data = Some(body);
        }
        p += 8 + len + (len & 1);
    }
    let data = data.ok_or("no data chunk")?;
    if rate == 0 {
        return Err("no fmt chunk".into());
    }
    let mono: Vec<f32> = match (fmt, bits) {
        (1, 16) => data
            .chunks_exact(2 * channels)
            .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
            .collect(),
        (3, 32) => data
            .chunks_exact(4 * channels)
            .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]))
            .collect(),
        (1, 24) => data
            .chunks_exact(3 * channels)
            .map(|f| (i32::from_le_bytes([0, f[0], f[1], f[2]]) >> 8) as f32 / 8_388_608.0)
            .collect(),
        _ => return Err(format!("unsupported WAV format {fmt}, {bits} bits")),
    };
    Ok(to_48k(&mono, rate))
}

/// Resamples to 48 kHz: integer factors through a low-pass FIR, anything
/// else by linear interpolation after it.
pub fn to_48k(x: &[f32], rate: u32) -> Vec<f32> {
    if rate == 48_000 {
        return x.to_vec();
    }
    if 48_000 % rate == 0 {
        let k = (48_000 / rate) as usize;
        let taps = ion_dsp::dsp::lowpass_taps(32 * k + 1, 0.45 * rate as f32, 48_000.0);
        let mut up = vec![0.0f32; x.len() * k];
        for (i, &v) in x.iter().enumerate() {
            up[i * k] = v * k as f32;
        }
        let half = taps.len() / 2;
        return (0..up.len())
            .map(|n| {
                let mut s = 0.0;
                for (j, &t) in taps.iter().enumerate() {
                    let m = n + half;
                    if m >= j && m - j < up.len() {
                        s += t * up[m - j];
                    }
                }
                s
            })
            .collect();
    }
    let step = rate as f64 / 48_000.0;
    let n = (x.len() as f64 / step) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 * step;
            let j = t as usize;
            let f = (t - j as f64) as f32;
            let a = x.get(j).copied().unwrap_or(0.0);
            let b = x.get(j + 1).copied().unwrap_or(a);
            a + (b - a) * f
        })
        .collect()
}

/// What became of burst `b` in a recording: found by its number, or (when
/// none of its codewords decoded, so the number is unknown) by its kind
/// and where it should end. `offset_s` is when the sequence started in
/// the recording.
pub fn find<'a>(b: &Sent, heard: &'a [Heard], offset_s: f64) -> Option<&'a Heard> {
    heard.iter().find(|h| h.seq == Some(b.seq)).or_else(|| {
        heard.iter().find(|h| {
            h.seq.is_none()
                && h.code == b.code
                && h.wave == b.wave
                && (h.at_s - offset_s - (b.start_s + b.dur_s)).abs() < 3.0
        })
    })
}

/// When the sequence started in a recording: from the first burst whose
/// number decoded.
pub fn offset_s(sent: &[Sent], heard: &[Heard]) -> f64 {
    heard
        .iter()
        .find_map(|h| {
            let b = sent.iter().find(|b| Some(b.seq) == h.seq)?;
            Some(h.at_s - (b.start_s + b.dur_s))
        })
        .unwrap_or(0.0)
}

/// A plain-text report: every burst sent, and whether it was heard.
pub fn report(sent: &[Sent], heard: &[Heard]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "| # | Burst | Heard | SNR dB | Codewords |");
    let _ = writeln!(s, "|---:|---|---|---:|---:|");
    let off = offset_s(sent, heard);
    for b in sent {
        let h = find(b, heard, off);
        let name = level_name(b.code, b.wave);
        match h {
            Some(h) => {
                let cw = if b.code == TINY {
                    "header".to_string()
                } else {
                    format!("{}/{}", h.ok, h.of)
                };
                let _ = writeln!(s, "| {} | {name} | yes | {:+.1} | {cw} |", b.seq, h.snr_db);
            }
            None => {
                let _ = writeln!(s, "| {} | {name} | **no** | | 0/{} |", b.seq, b.frames);
            }
        }
    }
    s
}

/// The same report from a recording alone (no list of what was sent):
/// every sounding burst found, in order.
pub fn report_heard(heard: &[Heard]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "| t (s) | # | From | Burst | SNR dB | Codewords |");
    let _ = writeln!(s, "|---:|---:|---|---|---:|---:|");
    for h in heard {
        let cw = if h.code == TINY {
            "header".to_string()
        } else {
            format!("{}/{}", h.ok, h.of)
        };
        let _ = writeln!(
            s,
            "| {:.1} | {} | {} | {} | {:+.1} | {cw} |",
            h.at_s,
            h.seq.map_or("?".into(), |v| v.to_string()),
            h.call.clone().unwrap_or_default(),
            level_name(h.code, h.wave),
            h.snr_db
        );
    }
    s
}
