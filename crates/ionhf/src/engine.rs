//! The DSP thread: received audio to Ion and the waterfall, Ion's bursts to
//! the transmitter. Lock-free: rings, atomics and a command channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use rtrb::{Consumer, Producer};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::ion_engine::{IonEvent, IonIn};
use crate::radio::RadioCtl;

pub const SAMPLE_RATE: usize = 48_000;
/// 2.93 Hz bins; the first 1024 cover 0–3000 Hz.
pub const FFT_LEN: usize = 16_384;
pub const SPECTRUM_BINS: usize = 1024;
pub const SPECTRUM_HZ: f32 = SPECTRUM_BINS as f32 * SAMPLE_RATE as f32 / FFT_LEN as f32;
const SPECTRUM_HOP: usize = SAMPLE_RATE / 20;
const TICK: Duration = Duration::from_millis(10);
const TX_AHEAD: usize = SAMPLE_RATE / 4;
/// A burst longer than its own length plus this is cut off.
const WATCHDOG_SLACK: Duration = Duration::from_secs(15);
/// The radio side (TCI's audio requests, the sound card) must start taking
/// a burst's audio this soon after PTT, a sound card's PTT lead included...
const FIRST_PULL: Duration = Duration::from_secs(2);
/// ...and keep taking it: this long with audio waiting and none taken ends
/// the over (the radio is keyed but sends nothing).
const PULL_STALL: Duration = Duration::from_millis(1500);
/// Our own tail can still echo back for a moment after unkeying.
const QUIET_AFTER_TX: Duration = Duration::from_millis(150);
const UNKEY_HOLD: Duration = Duration::from_millis(60);

#[derive(Clone, Debug)]
pub enum Msg {
    Abort,
    /// Where received audio goes (`None`: nowhere).
    Ion(Option<Sender<IonIn>>),
    IonTx(Vec<f32>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    pub keyed: bool,
}

pub enum UiEvent {
    Spectrum(Vec<f32>),
    Status(Status),
    Warning(String),
    Radio(crate::radio::RadioInfo),
    Ion(IonEvent),
    /// A note from the Winlink host interface.
    Host(String),
}

#[derive(Default)]
pub struct Shared {
    /// TX audio should flow; the radio side discards the TX ring otherwise.
    pub keyed: AtomicBool,
    /// The last sample is queued: a short TX ring from here on is the end
    /// of the burst, not an underrun.
    pub tx_done: AtomicBool,
    pub underruns: AtomicU64,
}

pub struct Engine {
    burst: Vec<f32>,
    played: usize,
    keyed: bool,
    tx_started: Instant,
    watchdog: Duration,
    unkey_at: Option<Instant>,
    /// Samples of this burst put in the TX ring, and how many of them the
    /// radio side had taken at `last_pull`.
    pushed: usize,
    taken: usize,
    last_pull: Option<Instant>,
    rx: Consumer<f32>,
    tx: Producer<f32>,
    tx_capacity: usize,
    shared: Arc<Shared>,
    radio: RadioCtl,
    ui: Sender<UiEvent>,
    msgs: Receiver<Msg>,
    spectrum_return: Receiver<Vec<f32>>,
    spectrum: Spectrum,
    repaint: Box<dyn Fn() + Send>,
    ion: Option<Sender<IonIn>>,
    quiet_until: Instant,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rx: Consumer<f32>,
        tx: Producer<f32>,
        shared: Arc<Shared>,
        radio: RadioCtl,
        ui: Sender<UiEvent>,
        msgs: Receiver<Msg>,
        spectrum_return: Receiver<Vec<f32>>,
        repaint: Box<dyn Fn() + Send>,
    ) -> Self {
        let tx_capacity = tx.buffer().capacity();
        Self {
            burst: Vec::new(),
            played: 0,
            keyed: false,
            tx_started: Instant::now(),
            watchdog: WATCHDOG_SLACK,
            unkey_at: None,
            pushed: 0,
            taken: 0,
            last_pull: None,
            rx,
            tx,
            tx_capacity,
            shared,
            radio,
            ui,
            msgs,
            spectrum_return,
            spectrum: Spectrum::new(),
            repaint,
            ion: None,
            quiet_until: Instant::now(),
        }
    }

    pub fn run(mut self) {
        let mut next = Instant::now();
        let mut last_status = Instant::now();
        loop {
            while let Ok(msg) = self.msgs.try_recv() {
                if matches!(msg, Msg::Shutdown) {
                    self.unkey();
                    return;
                }
                self.handle(msg);
            }
            let mut dirty = self.drain_rx();
            self.service_tx();
            if last_status.elapsed() >= Duration::from_millis(50) {
                last_status = Instant::now();
                self.send(UiEvent::Status(Status { keyed: self.keyed }));
                dirty = true;
            }
            if dirty {
                (self.repaint)();
            }
            next += TICK;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }

    fn send(&self, ev: UiEvent) {
        if let Err(TrySendError::Full(_)) = self.ui.try_send(ev) {
            log::debug!("UI channel full; dropping event");
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Abort => self.abort(),
            Msg::Ion(h) => {
                self.abort();
                self.ion = h;
            }
            Msg::IonTx(audio) => {
                if self.ion.is_none() {
                    return;
                }
                self.abort();
                self.watchdog = Duration::from_secs_f32(audio.len() as f32 / SAMPLE_RATE as f32) + WATCHDOG_SLACK;
                self.burst = audio;
                self.played = 0;
                self.pushed = 0;
                self.taken = 0;
                self.last_pull = None;
                self.key();
            }
            Msg::Shutdown => {}
        }
    }

    fn key(&mut self) {
        if self.keyed {
            return;
        }
        self.keyed = true;
        self.unkey_at = None;
        self.tx_started = Instant::now();
        self.shared.tx_done.store(false, Ordering::Release);
        // `shared.keyed` goes up in `service_tx`, once audio is queued.
        self.radio.ptt(true);
    }

    fn unkey(&mut self) {
        if !self.keyed {
            return;
        }
        self.keyed = false;
        self.quiet_until = Instant::now() + QUIET_AFTER_TX;
        if let Some(h) = &self.ion {
            let _ = h.send(IonIn::TxDone);
        }
        self.shared.keyed.store(false, Ordering::Release);
        self.radio.ptt(false);
    }

    fn abort(&mut self) {
        self.burst.clear();
        self.played = 0;
        self.unkey();
    }

    fn drain_rx(&mut self) -> bool {
        let n = self.rx.slots();
        if n == 0 {
            return false;
        }
        let Ok(chunk) = self.rx.read_chunk(n) else {
            return false;
        };
        let (a, b) = chunk.as_slices();
        if let Some(h) = self.ion.as_ref()
            && !self.keyed
            && Instant::now() >= self.quiet_until
        {
            let mut v = Vec::with_capacity(a.len() + b.len());
            v.extend_from_slice(a);
            v.extend_from_slice(b);
            let _ = h.send(IonIn::Audio(v));
        }
        self.spectrum.push(a);
        self.spectrum.push(b);
        chunk.commit_all();

        let mut sent_any = false;
        while self.spectrum.ready() {
            let mut row = self
                .spectrum_return
                .try_recv()
                .unwrap_or_else(|_| vec![0.0; SPECTRUM_BINS]);
            row.resize(SPECTRUM_BINS, 0.0);
            self.spectrum.compute(&mut row);
            self.send(UiEvent::Spectrum(row));
            sent_any = true;
        }
        sent_any
    }

    fn service_tx(&mut self) {
        if !self.keyed {
            return;
        }
        if self.tx_started.elapsed() > self.watchdog {
            self.abort();
            self.send(UiEvent::Warning(format!(
                "TX watchdog: unkeyed after {} s",
                self.watchdog.as_secs()
            )));
            return;
        }
        while self.played < self.burst.len() {
            let queued = self.tx_capacity - self.tx.slots();
            if queued >= TX_AHEAD {
                break;
            }
            let end = (self.played + TX_AHEAD - queued).min(self.burst.len());
            let n = push_slice(&mut self.tx, &self.burst[self.played..end]);
            self.played += n;
            self.pushed += n;
            if n == 0 {
                break;
            }
        }
        self.shared.keyed.store(true, Ordering::Release);
        let queued = self.tx_capacity - self.tx.slots();
        let taken = self.pushed - queued.min(self.pushed);
        let now = Instant::now();
        if taken > self.taken {
            self.taken = taken;
            self.last_pull = Some(now);
        }
        let limit = match self.last_pull {
            None => self.tx_started + FIRST_PULL,
            Some(t) => t + PULL_STALL,
        };
        if queued > 0 && now > limit {
            let what = if self.last_pull.is_none() {
                "none of the audio"
            } else {
                "no more audio"
            };
            let secs = (now - self.last_pull.unwrap_or(self.tx_started)).as_secs_f32();
            let left_ms = (self.burst.len() - self.played + queued) * 1000 / SAMPLE_RATE;
            self.abort();
            self.send(UiEvent::Warning(format!(
                "TX stalled: the radio took {what} for {secs:.1} s ({left_ms} ms of the burst left); unkeyed"
            )));
            return;
        }
        let done = self.played >= self.burst.len();
        if done {
            self.shared.tx_done.store(true, Ordering::Release);
        }
        if done && self.tx.slots() == self.tx_capacity {
            let at = *self.unkey_at.get_or_insert_with(|| Instant::now() + UNKEY_HOLD);
            if Instant::now() >= at {
                self.unkey();
            }
        }
    }
}

pub fn push_slice(p: &mut Producer<f32>, data: &[f32]) -> usize {
    let n = data.len().min(p.slots());
    if n == 0 {
        return 0;
    }
    let mut chunk = p.write_chunk(n).expect("slots checked");
    let (a, b) = chunk.as_mut_slices();
    let k = a.len();
    a.copy_from_slice(&data[..k]);
    b.copy_from_slice(&data[k..n]);
    chunk.commit_all();
    n
}

pub fn pop_slice(c: &mut Consumer<f32>, out: &mut [f32]) -> usize {
    let n = out.len().min(c.slots());
    if n == 0 {
        return 0;
    }
    let chunk = c.read_chunk(n).expect("slots checked");
    let (a, b) = chunk.as_slices();
    out[..a.len()].copy_from_slice(a);
    out[a.len()..n].copy_from_slice(b);
    chunk.commit_all();
    n
}

pub fn discard(c: &mut Consumer<f32>) {
    let n = c.slots();
    if n > 0
        && let Ok(chunk) = c.read_chunk(n)
    {
        chunk.commit_all();
    }
}

/// Sliding FFT of the received audio for the waterfall.
struct Spectrum {
    history: Vec<f32>,
    pos: usize,
    since: usize,
    window: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    fft: Arc<dyn Fft<f32>>,
}

impl Spectrum {
    fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        let window = (0..FFT_LEN)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_LEN as f32).cos())
            .collect();
        Self {
            history: vec![0.0; FFT_LEN],
            pos: 0,
            since: 0,
            window,
            buf: vec![Complex32::default(); FFT_LEN],
            scratch,
            fft,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        for &x in samples {
            self.history[self.pos] = x;
            self.pos = (self.pos + 1) % FFT_LEN;
        }
        self.since += samples.len();
    }

    /// A row is due; if we fell behind, only the latest matters.
    fn ready(&mut self) -> bool {
        if self.since < SPECTRUM_HOP {
            return false;
        }
        self.since = if self.since >= 2 * SPECTRUM_HOP {
            0
        } else {
            self.since - SPECTRUM_HOP
        };
        true
    }

    /// Power in dB for bins 0..SPECTRUM_BINS.
    fn compute(&mut self, out: &mut [f32]) {
        for i in 0..FFT_LEN {
            let x = self.history[(self.pos + i) % FFT_LEN] * self.window[i];
            self.buf[i] = Complex32::new(x, 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        let norm = 1.0 / (FFT_LEN as f32 * FFT_LEN as f32);
        for (o, c) in out.iter_mut().zip(&self.buf) {
            *o = 10.0 * (c.norm_sqr() * norm + 1e-20).log10();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radio::LoopCmd;

    /// The DSP thread with a radio side that takes TX audio at `pull` (a
    /// fraction of real time) until `stop_after` samples, then none.
    fn over(burst_s: f32, pull: f32, stop_after: usize) -> (bool, Option<String>, Duration) {
        let (_rx_prod, rx_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE);
        let (tx_prod, mut tx_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE);
        let shared = Arc::new(Shared::default());
        let (ui_tx, ui_rx) = crossbeam_channel::unbounded();
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (_spec_tx, spec_rx) = crossbeam_channel::bounded(1);
        let (loop_tx, loop_rx) = crossbeam_channel::unbounded::<LoopCmd>();
        let (ion_tx, ion_rx) = crossbeam_channel::unbounded();
        let engine = Engine::new(
            rx_cons,
            tx_prod,
            shared.clone(),
            RadioCtl::Loopback(loop_tx),
            ui_tx,
            msg_rx,
            spec_rx,
            Box::new(|| {}),
        );
        let dsp = std::thread::spawn(move || engine.run());
        msg_tx.send(Msg::Ion(Some(ion_tx))).unwrap();
        msg_tx
            .send(Msg::IonTx(vec![0.1; (burst_s * SAMPLE_RATE as f32) as usize]))
            .unwrap();
        let start = Instant::now();
        let (mut taken, mut warning, mut done) = (0usize, None, false);
        let mut buf = vec![0f32; SAMPLE_RATE / 100];
        while start.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(10));
            if shared.keyed.load(Ordering::Acquire) && taken < stop_after {
                let want = ((buf.len() as f32 * pull) as usize).min(stop_after - taken);
                taken += pop_slice(&mut tx_cons, &mut buf[..want]);
            }
            while let Ok(ev) = ui_rx.try_recv() {
                if let UiEvent::Warning(w) = ev {
                    warning = Some(w);
                }
            }
            if matches!(ion_rx.try_recv(), Ok(IonIn::TxDone)) {
                done = true;
                break;
            }
        }
        let took = start.elapsed();
        msg_tx.send(Msg::Shutdown).unwrap();
        dsp.join().unwrap();
        for ev in ui_rx.try_iter() {
            if let UiEvent::Warning(w) = ev {
                warning = Some(w);
            }
        }
        let unkeyed = loop_rx.try_iter().filter(|c| matches!(c, LoopCmd::Ptt(false))).count() > 0;
        (done && unkeyed, warning, took)
    }

    #[test]
    fn a_radio_that_takes_the_audio_plays_the_whole_burst() {
        let (done, warning, took) = over(2.0, 1.0, usize::MAX);
        assert!(done && warning.is_none(), "{warning:?}");
        assert!(took >= Duration::from_millis(1900), "{took:?}");
    }

    #[test]
    fn a_radio_that_takes_no_audio_is_unkeyed_in_two_seconds() {
        let (done, warning, took) = over(2.0, 1.0, 0);
        assert!(done, "still keyed");
        assert!(warning.is_some_and(|w| w.starts_with("TX stalled")));
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn a_radio_that_stops_taking_audio_is_unkeyed_soon_after() {
        let (done, warning, took) = over(6.0, 1.0, SAMPLE_RATE);
        assert!(done, "still keyed");
        assert!(warning.is_some_and(|w| w.starts_with("TX stalled")));
        assert!(took < Duration::from_secs(4), "{took:?}");
    }
}
