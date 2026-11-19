//! The signal processing Ion builds on: oscillators, filters, the FFT, the
//! CW sender for the station ID, and the HF channel models it is tested
//! through (Watterson, a port of the IONOS SIM box, and ham QRM).

pub mod channel;
pub mod cw;
pub mod dsp;
pub mod fft;
pub mod ionos;
pub mod qrm;

pub use cw::CwMod;

/// Peak amplitude of modulator output (full scale is 1.0).
pub const TX_LEVEL: f32 = 0.5;

pub trait Modulator: Send {
    fn set_center_hz(&mut self, hz: f32);
    fn push_text(&mut self, text: &str);
    /// Fills `out` with 48 kHz samples; fewer than `out.len()` only when the
    /// transmission ends, 0 when idle.
    fn fill(&mut self, out: &mut [f32]) -> usize;
    fn pending(&self) -> usize;
    fn start(&mut self);
    fn finish(&mut self);
    fn abort(&mut self);
    fn is_active(&self) -> bool;
    fn chars_sent(&self) -> usize;
}
