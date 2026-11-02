//! The radio thread for rig-control backends: owns the [`Rig`] and the PTT
//! line, reconnects when the link drops, reads the dial and mode every
//! `poll_ms`, and publishes [`RadioInfo`] like the TCI thread does. Audio
//! runs beside it on the sound-card thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use rtrb::{Consumer, Producer};

use super::cat::{self, Cat};
use super::civ::Civ;
use super::rigctld::Rigctld;
use super::{Line, LinePtt, NoRig, Rig, RigError, RigResult, SerialPort};
use crate::config::{Backend, PttMethod, RadioSettings};
use crate::engine::{Shared, UiEvent};
use crate::radio::{RadioCtl, RadioInfo, RadioThread};
use crate::soundcard::{AudioConfig, AudioLevels, SoundCard};

const RETRY: Duration = Duration::from_secs(3);
/// Longest wait for the sound card to play out the end of an over.
const TAIL_MAX: Duration = Duration::from_millis(300);
/// Consecutive missed polls before the link counts as down.
const MAX_MISSES: u32 = 3;

pub enum RigCmd {
    Ptt(bool),
    Drive(u32),
}

pub type Opener = Box<dyn FnMut() -> RigResult<Box<dyn Rig>> + Send>;
pub type LineOpener = Box<dyn FnMut() -> RigResult<LinePtt> + Send>;

/// Opens the rig the settings describe.
pub fn open_rig(cfg: &RadioSettings) -> RigResult<Box<dyn Rig>> {
    let need_port = || {
        if cfg.port.trim().is_empty() {
            Err(RigError::Link("choose the radio's COM port in Setup".into()))
        } else {
            Ok(cfg.port.trim().to_string())
        }
    };
    Ok(match cfg.backend {
        Backend::Rigctld => Box::new(Rigctld::connect(cfg.rigctld_addr.trim())?.with_tx_audio(cfg.tx_audio)),
        Backend::Icom => {
            let name = need_port()?;
            let port = SerialPort::open(&name, cfg.baud, 1)?;
            Box::new(Civ::new(Box::new(port), &name, &cfg.icom_model, cfg.civ_addr)?)
        }
        Backend::Cat => {
            let name = need_port()?;
            let model = cat::model(&cfg.cat_model)
                .ok_or_else(|| RigError::Link(format!("unknown CAT model {:?}", cfg.cat_model)))?;
            let port = SerialPort::open(&name, cfg.baud, model.stop_bits())?;
            Box::new(Cat::new(Box::new(port), &name, model)?.with_tx_audio(cfg.tx_audio))
        }
        Backend::None => Box::new(NoRig),
        Backend::Tci => return Err(RigError::Link("TCI is not a rig-control backend".into())),
        Backend::Loopback => return Err(RigError::Link("loopback has no radio".into())),
    })
}

/// RTS/DTR PTT on a port of its own, when the settings ask for one.
fn line_opener(cfg: &RadioSettings) -> Option<LineOpener> {
    let line = match cfg.ptt {
        PttMethod::Rts => Line::Rts,
        PttMethod::Dtr => Line::Dtr,
        _ => return None,
    };
    let mut name = cfg.ptt_port.trim().to_string();
    // A phone has one USB port: the interface's serial bridge on it.
    if cfg!(target_os = "android") && name.is_empty() && cfg.backend == Backend::None {
        name = "usb".into();
    }
    if cfg.backend == Backend::None && name.is_empty() {
        return Some(Box::new(|| {
            Err(RigError::Link("choose the PTT COM port in Setup".into()))
        }));
    }
    // No port of its own: the line on the CAT port.
    if name.is_empty() || (name == cfg.port.trim() && cfg.backend != Backend::None) {
        return None;
    }
    Some(Box::new(move || {
        let port = SerialPort::open(&name, 9600, 1)?;
        Ok(LinePtt::new(Box::new(port), line))
    }))
}

/// Starts the sound card and the rig thread.
pub fn spawn(
    cfg: RadioSettings,
    rx: Producer<f32>,
    tx: Consumer<f32>,
    shared: Arc<Shared>,
    ui: Sender<UiEvent>,
    repaint: Box<dyn Fn() + Send>,
) -> RadioThread {
    let levels = Arc::new(AudioLevels::new(cfg.tx_level));
    let sound = SoundCard::start(
        AudioConfig {
            input: cfg.audio_in.clone(),
            output: cfg.audio_out.clone(),
            ptt_lead_ms: cfg.ptt_lead_ms,
        },
        levels.clone(),
        rx,
        tx,
        shared,
        ui.clone(),
    );
    let c2 = cfg.clone();
    let opener: Opener = Box::new(move || open_rig(&c2));
    let lines = line_opener(&cfg);
    spawn_with(cfg, opener, lines, levels, ui, repaint).with_sound(sound)
}

/// The rig thread alone, with the rig and PTT port supplied (tests pass
/// emulators here).
pub fn spawn_with(
    cfg: RadioSettings,
    opener: Opener,
    lines: Option<LineOpener>,
    levels: Arc<AudioLevels>,
    ui: Sender<UiEvent>,
    repaint: Box<dyn Fn() + Send>,
) -> RadioThread {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = std::thread::Builder::new()
        .name("rig".into())
        .spawn(move || {
            let mut st = Station {
                info: RadioInfo {
                    message: "connecting".into(),
                    ..RadioInfo::default()
                },
                cfg,
                opener,
                lines,
                levels,
                ui,
                repaint,
                rig: None,
                line: None,
                retry_at: Instant::now(),
                line_retry_at: Instant::now(),
                next_poll: Instant::now(),
                misses: 0,
                ptt: false,
                warned_ptt: false,
            };
            st.run(&cmd_rx, &stop2);
        })
        .expect("spawn rig thread");
    RadioThread::new(RadioCtl::Rig(cmd_tx), stop, handle)
}

struct Station {
    cfg: RadioSettings,
    opener: Opener,
    lines: Option<LineOpener>,
    levels: Arc<AudioLevels>,
    ui: Sender<UiEvent>,
    repaint: Box<dyn Fn() + Send>,
    info: RadioInfo,
    rig: Option<Box<dyn Rig>>,
    line: Option<LinePtt>,
    retry_at: Instant,
    line_retry_at: Instant,
    next_poll: Instant,
    misses: u32,
    ptt: bool,
    warned_ptt: bool,
}

impl Station {
    fn run(&mut self, cmds: &Receiver<RigCmd>, stop: &AtomicBool) {
        self.publish();
        while !stop.load(Ordering::Relaxed) {
            self.connect_if_due();
            match cmds.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => self.command(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= self.next_poll {
                self.next_poll = Instant::now() + Duration::from_millis(self.cfg.poll_ms.max(200) as u64);
                self.poll();
            }
        }
        // Never leave the radio keyed.
        if self.ptt {
            self.set_ptt(false);
        }
        self.rig = None;
        self.line = None;
    }

    fn publish(&self) {
        let _ = self.ui.try_send(UiEvent::Radio(self.info.clone()));
        (self.repaint)();
    }

    fn warn(&self, w: String) {
        let _ = self.ui.try_send(UiEvent::Warning(w));
        (self.repaint)();
    }

    fn connect_if_due(&mut self) {
        let now = Instant::now();
        if self.rig.is_none() && now >= self.retry_at {
            match (self.opener)() {
                Ok(mut rig) => {
                    log::info!("rig: connected to {}", rig.describe());
                    self.info.connected = true;
                    self.info.device = Some(rig.describe());
                    self.info.message = format!("connected: {}", rig.describe());
                    match rig.power() {
                        Ok(p) => {
                            self.info.drive = Some(p);
                            self.info.drive_unsupported = false;
                        }
                        Err(e) => {
                            log::info!("rig: no power readout ({e})");
                            self.info.drive = None;
                            self.info.drive_unsupported = true;
                        }
                    }
                    self.rig = Some(rig);
                    self.misses = 0;
                    self.next_poll = now;
                    self.publish();
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != self.info.message || self.info.connected {
                        log::warn!("rig: {msg}");
                        self.info.connected = false;
                        self.info.problem = None;
                        self.info.message = msg;
                        self.publish();
                    }
                    self.retry_at = now + RETRY;
                }
            }
        }
        if self.line.is_none()
            && let Some(open) = self.lines.as_mut()
            && now >= self.line_retry_at
        {
            match open() {
                Ok(l) => self.line = Some(l),
                Err(e) => {
                    if !self.warned_ptt {
                        self.warn(format!("PTT port: {e}"));
                        self.warned_ptt = true;
                    }
                    self.line_retry_at = now + RETRY;
                }
            }
        }
    }

    /// A link failure drops the rig; the next pass reconnects.
    fn failed(&mut self, e: &RigError) {
        if let RigError::Link(_) = e {
            self.drop_link(e.to_string());
        }
    }

    fn drop_link(&mut self, why: String) {
        log::warn!("rig: {why}");
        self.rig = None;
        self.info.connected = false;
        self.info.problem = None;
        self.info.message = why;
        self.retry_at = Instant::now() + RETRY;
        self.publish();
    }

    fn poll(&mut self) {
        let Some(rig) = self.rig.as_mut() else { return };
        let freq = rig.freq();
        // The mode does not change mid-over; spare the bus while keyed,
        // and after a missed reply.
        let mode = if self.ptt || matches!(freq, Err(ref e) if *e != RigError::Unsupported) {
            None
        } else {
            Some(rig.mode())
        };
        let mut changed = false;
        match freq {
            Ok(hz) => {
                self.misses = 0;
                if self.info.dial_hz != Some(hz) {
                    self.info.dial_hz = Some(hz);
                    changed = true;
                }
            }
            Err(RigError::Unsupported) => {}
            Err(RigError::Link(e)) => return self.drop_link(e),
            Err(e) => {
                self.misses += 1;
                if self.misses >= MAX_MISSES {
                    return self.drop_link(format!("lost the radio: {e}"));
                }
            }
        }
        if let Some(Ok(m)) = mode {
            let p = m.problem();
            if p != self.info.problem {
                self.info.problem = p;
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    fn command(&mut self, cmd: RigCmd) {
        match cmd {
            RigCmd::Ptt(on) => self.set_ptt(on),
            RigCmd::Drive(pct) => {
                let Some(rig) = self.rig.as_mut() else { return };
                match rig.set_power(pct) {
                    Ok(()) => {
                        self.info.drive = Some(pct);
                        self.info.drive_unsupported = false;
                    }
                    Err(RigError::Unsupported | RigError::Rejected(_)) => {
                        self.info.drive_unsupported = true;
                        self.info.drive = None;
                    }
                    Err(e) => {
                        self.warn(format!("drive: {e}"));
                        self.failed(&e);
                    }
                }
                self.publish();
            }
        }
    }

    fn set_ptt(&mut self, on: bool) {
        if !on {
            if self.ptt {
                // Let the sound card play the end of the over first.
                let tail = self.levels.backlog().min(TAIL_MAX);
                if !tail.is_zero() {
                    std::thread::sleep(tail + Duration::from_millis(10));
                }
            }
            self.levels.ptt_ready.store(false, Ordering::Release);
        }
        let line = match self.cfg.ptt {
            PttMethod::Rts => Some(Line::Rts),
            PttMethod::Dtr => Some(Line::Dtr),
            _ => None,
        };
        let r: RigResult<()> = match (self.cfg.ptt, line) {
            (PttMethod::Vox, _) => Ok(()),
            (_, Some(_)) if self.lines.is_some() => match self.line.as_mut() {
                Some(p) => p.set(on),
                None => Err(RigError::Link("PTT port not open".into())),
            },
            (_, Some(l)) => match self.rig.as_mut() {
                Some(rig) => rig.set_line(l, on),
                None => Err(RigError::Link("radio not connected".into())),
            },
            _ => match self.rig.as_mut() {
                Some(rig) => rig.set_ptt(on),
                None => Err(RigError::Link("radio not connected".into())),
            },
        };
        // TX audio may flow now: the radio has the command. Released on a
        // failure too, so the audio never waits forever.
        if on {
            self.levels.ptt_ready.store(true, Ordering::Release);
        }
        match r {
            Ok(()) => {}
            Err(RigError::Unsupported) => {
                self.warn("this radio has no CAT PTT: choose RTS, DTR or VOX in Setup".into())
            }
            Err(e) => {
                self.warn(format!("PTT {}: {e}", if on { "on" } else { "off" }));
                if !on {
                    log::error!("rig: PTT off failed: {e}");
                }
                self.failed(&e);
            }
        }
        self.ptt = on;
        self.info.ptt = on;
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rig::civ::Civ;
    use crate::rig::emu::{CatEmu, CivEmu, EmuPort, LineEmu};

    struct Harness {
        thread: RadioThread,
        events: Receiver<UiEvent>,
        levels: Arc<AudioLevels>,
    }

    impl Harness {
        /// Waits for a RadioInfo matching `f`.
        fn wait_info(&self, f: impl Fn(&RadioInfo) -> bool) -> RadioInfo {
            self.wait_info_for(Duration::from_secs(5), f)
        }
        fn wait_info_for(&self, d: Duration, f: impl Fn(&RadioInfo) -> bool) -> RadioInfo {
            let until = Instant::now() + d;
            while Instant::now() < until {
                if let Ok(UiEvent::Radio(i)) = self.events.recv_timeout(Duration::from_millis(50))
                    && f(&i)
                {
                    return i;
                }
            }
            panic!("no matching RadioInfo");
        }
        fn wait_warning(&self, needle: &str) -> String {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if let Ok(UiEvent::Warning(w)) = self.events.recv_timeout(Duration::from_millis(50))
                    && w.contains(needle)
                {
                    return w;
                }
            }
            panic!("no warning containing {needle:?}");
        }
    }

    fn harness(cfg: RadioSettings, opener: Opener, lines: Option<LineOpener>) -> Harness {
        let (ui, events) = crossbeam_channel::unbounded();
        let levels = Arc::new(AudioLevels::new(0.5));
        let thread = spawn_with(cfg, opener, lines, levels.clone(), ui, Box::new(|| {}));
        Harness { thread, events, levels }
    }

    fn civ_opener(port: EmuPort<CivEmu>) -> Opener {
        Box::new(move || Ok(Box::new(Civ::new(Box::new(port.clone()), "EMU", "IC-7300", 0x94)?) as Box<dyn Rig>))
    }

    fn icom() -> RadioSettings {
        RadioSettings {
            backend: Backend::Icom,
            poll_ms: 200,
            ..RadioSettings::default()
        }
    }

    #[test]
    fn icom_drive_and_cat_ptt_through_the_thread() {
        let port = EmuPort::new(CivEmu::new(0x94));
        let mut h = harness(icom(), civ_opener(port.clone()), None);
        let i = h.wait_info(|i| i.connected && i.dial_hz.is_some());
        assert_eq!(i.dial_hz, Some(14_074_000));
        assert_eq!(i.drive, Some(50));

        h.thread.ctl.drive(25);
        h.wait_info(|i| i.drive == Some(25));
        assert_eq!(port.with(|e| e.power), 64);

        assert!(!h.levels.ptt_ready.load(Ordering::Acquire));
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        assert!(port.with(|e| e.ptt));
        assert!(
            h.levels.ptt_ready.load(Ordering::Acquire),
            "audio released after the CI-V PTT"
        );
        h.thread.ctl.ptt(false);
        h.wait_info(|i| !i.ptt);
        assert!(!port.with(|e| e.ptt));
        assert!(!h.levels.ptt_ready.load(Ordering::Acquire));

        // The radio is turned to the dial elsewhere: the poll sees it.
        port.with(|e| e.freq = 21_074_000);
        h.wait_info(|i| i.dial_hz == Some(21_074_000));
        h.thread.stop();
    }

    #[test]
    fn stopping_while_keyed_unkeys() {
        let port = EmuPort::new(CivEmu::new(0x94));
        let mut h = harness(icom(), civ_opener(port.clone()), None);
        h.wait_info(|i| i.connected);
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        h.thread.stop();
        assert!(!port.with(|e| e.ptt));
    }

    #[test]
    fn mode_problem_is_published() {
        let port = EmuPort::new(CivEmu::new(0x94));
        port.with(|e| e.mode = 0x00);
        let mut h = harness(icom(), civ_opener(port.clone()), None);
        let i = h.wait_info(|i| i.problem.is_some());
        assert!(i.problem.unwrap().contains("LSB"));
        port.with(|e| e.mode = 0x01);
        h.wait_info(|i| i.connected && i.problem.is_none());
        h.thread.stop();
    }

    #[test]
    fn reconnects_after_the_radio_goes_away() {
        let port = EmuPort::new(CivEmu::new(0x94));
        let mut cfg = icom();
        cfg.poll_ms = 200;
        let mut h = harness(cfg, civ_opener(port.clone()), None);
        h.wait_info(|i| i.connected);
        // Radio switched off: no replies at all.
        port.with(|e| e.addr = 0x00);
        // Three missed polls of two 300 ms timeouts each, under load.
        let i = h.wait_info_for(Duration::from_secs(15), |i| !i.connected);
        assert!(
            i.message.contains("CI-V") || i.message.contains("lost"),
            "{}",
            i.message
        );
        port.with(|e| e.addr = 0x94);
        h.wait_info_for(Duration::from_secs(15), |i| i.connected);
        h.thread.stop();
    }

    #[test]
    fn not_running_rigctld_is_reported_and_retried() {
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let cfg = RadioSettings {
            backend: Backend::Rigctld,
            rigctld_addr: addr.clone(),
            ..RadioSettings::default()
        };
        let c2 = cfg.clone();
        let mut h = harness(cfg, Box::new(move || open_rig(&c2)), None);
        let i = h.wait_info(|i| i.message.contains("rigctld -m"));
        assert!(!i.connected);
        h.thread.stop();
    }

    #[test]
    fn rigctld_end_to_end() {
        let fake = crate::rig::emu::FakeRigctld::start();
        let cfg = RadioSettings {
            backend: Backend::Rigctld,
            rigctld_addr: fake.addr(),
            poll_ms: 200,
            ..RadioSettings::default()
        };
        let c2 = cfg.clone();
        let mut h = harness(cfg, Box::new(move || open_rig(&c2)), None);
        h.wait_info(|i| i.connected && i.dial_hz == Some(14_074_000));
        fake.state_mut(|s| s.freq = 14_080_000);
        h.wait_info(|i| i.dial_hz == Some(14_080_000));
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        assert!(fake.state().ptt);
        h.thread.ctl.ptt(false);
        h.wait_info(|i| !i.ptt);
        assert!(!fake.state().ptt);
        h.thread.stop();
    }

    #[test]
    fn rts_ptt_on_its_own_port_with_no_rig_control() {
        let line = EmuPort::new(LineEmu::default());
        let l2 = line.clone();
        let cfg = RadioSettings {
            backend: Backend::None,
            ptt: PttMethod::Rts,
            ptt_port: "EMU".into(),
            ..RadioSettings::default()
        };
        let lines: LineOpener = Box::new(move || Ok(LinePtt::new(Box::new(l2.clone()), Line::Rts)));
        let mut h = harness(cfg, Box::new(|| Ok(Box::new(NoRig) as Box<dyn Rig>)), Some(lines));
        let i = h.wait_info(|i| i.connected);
        assert!(i.drive_unsupported);
        assert_eq!(i.dial_hz, None);
        // Give the line opener a pass.
        std::thread::sleep(Duration::from_millis(150));
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        assert!(line.with(|e| e.rts) && !line.with(|e| e.dtr));
        h.thread.ctl.ptt(false);
        h.wait_info(|i| !i.ptt);
        assert!(!line.with(|e| e.rts));
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        h.thread.stop();
        assert!(!line.with(|e| e.rts), "dropping the port drops RTS");
    }

    #[test]
    fn dtr_ptt_on_the_cat_port() {
        let port = EmuPort::new(CivEmu::new(0x94));
        let cfg = RadioSettings {
            ptt: PttMethod::Dtr,
            ..icom()
        };
        let mut h = harness(cfg, civ_opener(port.clone()), None);
        h.wait_info(|i| i.connected);
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        assert!(port.with(|e| e.dtr) && !port.with(|e| e.ptt), "keyed by DTR, not CI-V");
        h.thread.ctl.ptt(false);
        h.wait_info(|i| !i.ptt);
        assert!(!port.with(|e| e.dtr));
        h.thread.stop();
    }

    #[test]
    fn vox_sends_nothing() {
        let port = EmuPort::new(CivEmu::new(0x94));
        let cfg = RadioSettings {
            ptt: PttMethod::Vox,
            ..icom()
        };
        let mut h = harness(cfg, civ_opener(port.clone()), None);
        h.wait_info(|i| i.connected);
        h.thread.ctl.ptt(true);
        h.wait_info(|i| i.ptt);
        assert!(!port.with(|e| e.ptt || e.rts || e.dtr));
        h.thread.stop();
    }

    #[test]
    fn cat_radio_without_power_control_greys_out_drive() {
        let port = EmuPort::new(CatEmu::new(cat::Brand::Yaesu));
        port.with(|e| e.no_power = true);
        let p2 = port.clone();
        let opener: Opener = Box::new(move || {
            Ok(Box::new(Cat::new(Box::new(p2.clone()), "EMU", cat::model("FT-891").unwrap())?) as Box<dyn Rig>)
        });
        let cfg = RadioSettings {
            backend: Backend::Cat,
            ..RadioSettings::default()
        };
        let mut h = harness(cfg, opener, None);
        let i = h.wait_info(|i| i.connected);
        assert!(i.drive_unsupported);
        h.thread.ctl.drive(40);
        h.wait_info(|i| i.drive_unsupported && i.drive.is_none());
        h.thread.stop();
    }

    #[test]
    fn cat_ptt_without_rig_control_explains_itself() {
        let cfg = RadioSettings {
            backend: Backend::None,
            ptt: PttMethod::Cat,
            ..RadioSettings::default()
        };
        let mut h = harness(cfg, Box::new(|| Ok(Box::new(NoRig) as Box<dyn Rig>)), None);
        h.wait_info(|i| i.connected);
        h.thread.ctl.ptt(true);
        h.wait_warning("RTS, DTR or VOX");
        h.thread.ctl.ptt(false);
        h.thread.stop();
    }

    #[test]
    fn rts_ptt_with_no_port_named_asks_for_one() {
        let cfg = RadioSettings {
            backend: Backend::None,
            ptt: PttMethod::Rts,
            ..RadioSettings::default()
        };
        let lines = line_opener(&cfg);
        let mut h = harness(cfg, Box::new(|| Ok(Box::new(NoRig) as Box<dyn Rig>)), lines);
        h.wait_warning("PTT COM port");
        h.thread.stop();
    }

    #[test]
    fn no_com_port_chosen_says_so() {
        let cfg = RadioSettings {
            backend: Backend::Icom,
            ..RadioSettings::default()
        };
        let e = open_rig(&cfg).err().unwrap().to_string();
        assert!(e.contains("COM port"), "{e}");
    }
}
