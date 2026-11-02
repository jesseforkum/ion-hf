//! Rig control for radios without TCI: one [`Rig`] trait and a backend per
//! control method, each written from a published protocol reference:
//!
//! - [`rigctld`]: Hamlib's `rigctld` daemon over TCP (any rig Hamlib knows).
//! - [`civ`]: Icom CI-V over a COM port.
//! - [`cat`]: the Kenwood-style ASCII CAT family (Kenwood, Elecraft, Yaesu).
//! - [`NoRig`]: no control at all (PTT by RTS/DTR or VOX only).
//!
//! None is derived from Hamlib, flrig or wfview code. The tests run them
//! against the in-process emulators in [`emu`].

use std::fmt;
use std::io::{Read, Write};

pub mod cat;
pub mod civ;
#[cfg(test)]
pub mod emu;
pub mod rigctld;
#[cfg(not(target_os = "android"))]
mod serial;
pub mod thread;
#[cfg(target_os = "android")]
mod usb_serial;

#[cfg(not(target_os = "android"))]
pub use serial::SerialPort;
#[cfg(target_os = "android")]
pub use usb_serial::SerialPort;

#[derive(Clone, Debug, PartialEq)]
pub enum RigError {
    /// The radio (or this backend) has no such control.
    Unsupported,
    /// No reply in time.
    Timeout,
    /// The radio refused the command (CI-V NG, CAT `?;`, rigctld RPRT < 0).
    Rejected(String),
    /// A reply we could not make sense of.
    Protocol(String),
    /// The link itself failed (port gone, daemon not running, ...).
    Link(String),
}

impl fmt::Display for RigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RigError::Unsupported => write!(f, "not supported by this radio"),
            RigError::Timeout => write!(f, "no reply from the radio"),
            RigError::Rejected(s) => write!(f, "radio refused: {s}"),
            RigError::Protocol(s) => write!(f, "bad reply: {s}"),
            RigError::Link(s) => write!(f, "{s}"),
        }
    }
}

impl From<std::io::Error> for RigError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => RigError::Timeout,
            _ => RigError::Link(e.to_string()),
        }
    }
}

pub type RigResult<T> = Result<T, RigError>;

/// Narrowest RX filter that does not count as a problem, Hz.
const MIN_FILTER_HZ: u32 = 2700;

/// The radio's operating mode, as far as Ion cares.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeInfo {
    /// As the radio names it, e.g. "USB-D", "PKTUSB", "DATA-U".
    pub name: String,
    /// Upper sideband (plain or data), which Ion needs.
    pub usb: bool,
    /// RX filter width, where the radio reports it.
    pub width_hz: Option<u32>,
}

impl ModeInfo {
    /// A radio setting that will stop Ion working: wrong mode or a filter
    /// narrower than Ion needs.
    pub fn problem(&self) -> Option<String> {
        if !self.usb {
            return Some(format!("radio in {}; Ion expects USB-D (or USB)", self.name));
        }
        match self.width_hz {
            Some(w) if w > 0 && w < MIN_FILTER_HZ => {
                Some(format!("RX filter {w} Hz wide; set 2.8-3.0 kHz on the radio"))
            }
            _ => None,
        }
    }
}

/// A serial control line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Rts,
    Dtr,
}

/// One radio, under whatever control method. All calls block for at most a
/// timeout or two; the radio thread is the only caller.
pub trait Rig: Send {
    /// E.g. "IC-7300 (CI-V 94h, COM3)".
    fn describe(&self) -> String;
    /// VFO A / main dial frequency.
    fn freq(&mut self) -> RigResult<i64>;
    /// PTT by CAT command.
    fn set_ptt(&mut self, on: bool) -> RigResult<()>;
    /// RF power as 0-100 % of the radio's range.
    fn power(&mut self) -> RigResult<u32>;
    fn set_power(&mut self, pct: u32) -> RigResult<()>;
    fn mode(&mut self) -> RigResult<ModeInfo>;
    /// RTS/DTR on the CAT port, for "PTT on RTS" with one cable.
    fn set_line(&mut self, _line: Line, _on: bool) -> RigResult<()> {
        Err(RigError::Unsupported)
    }
}

/// No rig control: the dial is whatever the radio is on. PTT by RTS/DTR on
/// a port of its own, or VOX.
pub struct NoRig;

impl Rig for NoRig {
    fn describe(&self) -> String {
        "no rig control".into()
    }
    fn freq(&mut self) -> RigResult<i64> {
        Err(RigError::Unsupported)
    }
    fn set_ptt(&mut self, _on: bool) -> RigResult<()> {
        Err(RigError::Unsupported)
    }
    fn power(&mut self) -> RigResult<u32> {
        Err(RigError::Unsupported)
    }
    fn set_power(&mut self, _pct: u32) -> RigResult<()> {
        Err(RigError::Unsupported)
    }
    fn mode(&mut self) -> RigResult<ModeInfo> {
        Err(RigError::Unsupported)
    }
}

/// A byte link to a radio: a COM port, or an emulator in tests.
pub trait Port: Read + Write + Send {
    fn set_line(&mut self, line: Line, on: bool) -> std::io::Result<()>;
    /// Drops whatever is waiting to be read.
    fn clear_input(&mut self) -> std::io::Result<()>;
}

/// PTT on RTS or DTR of a port that carries nothing else.
pub struct LinePtt {
    port: Box<dyn Port>,
    line: Line,
}

impl LinePtt {
    pub fn new(port: Box<dyn Port>, line: Line) -> Self {
        Self { port, line }
    }

    pub fn set(&mut self, on: bool) -> RigResult<()> {
        self.port.set_line(self.line, on).map_err(RigError::from)
    }
}

impl Drop for LinePtt {
    fn drop(&mut self) {
        let _ = self.port.set_line(self.line, false);
    }
}

/// Percent (0-100) to a 0..=`max` scale, rounded.
pub fn pct_to_scale(pct: u32, max: u32) -> u32 {
    (pct.min(100) * max + 50) / 100
}

/// A 0..=`max` value back to percent.
pub fn scale_to_pct(v: u32, max: u32) -> u32 {
    if max == 0 {
        return 0;
    }
    ((v.min(max) * 100 + max / 2) / max).min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_scales_round_trip() {
        for pct in 0..=100 {
            assert_eq!(scale_to_pct(pct_to_scale(pct, 255), 255), pct);
            assert_eq!(scale_to_pct(pct_to_scale(pct, 100), 100), pct);
        }
        assert_eq!(pct_to_scale(50, 255), 128);
        assert_eq!(pct_to_scale(150, 255), 255);
    }

    #[test]
    fn mode_problems() {
        let m = |name: &str, usb, w| ModeInfo {
            name: name.into(),
            usb,
            width_hz: w,
        };
        assert_eq!(m("USB-D", true, Some(3000)).problem(), None);
        assert_eq!(m("PKTUSB", true, None).problem(), None);
        assert!(m("LSB", false, None).problem().unwrap().contains("LSB"));
        assert!(m("USB", true, Some(2400)).problem().unwrap().contains("2400"));
        // 0 means "the radio's default" in rigctld.
        assert_eq!(m("USB", true, Some(0)).problem(), None);
    }
}
