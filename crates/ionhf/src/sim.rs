//! Headless stations on one simulated band, for testing host programs
//! (Ragtime, Pat) against Ion without a radio. Each station has its own
//! host ports; what one transmits reaches each other station through its
//! own IONOS channel, with noise.
//!
//! ionhf --sim [--stations 2] [--ionos MPG] [--paths 2] [--snr 10[,DB...]]
//!   [--port 8300] [--calls N0AAA,N0BBB] [--seed 1] [--gear goodput|ladder]
//!
//! Station `i` listens on `port + 10·i` (data one above, KISS 200 below).
//! `--snr` is what each station hears, in station order (the last value
//! repeats), against a sine at full transmit level in 3 kHz, as the IONOS
//! charts. The log goes where Ion's log goes: run it from a portable folder.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ion_dsp::TX_LEVEL;
use ion_dsp::ionos::{Ionos, Mode, SIM_RATE, StreamResampler};
use rtrb::{Consumer, Producer};

use crate::engine::{Engine, Msg, SAMPLE_RATE, Shared, UiEvent, discard, pop_slice, push_slice};
use crate::ion_engine::{self, IonEvent};
use crate::radio::{LoopCmd, RadioCtl};

const BLOCK: usize = SAMPLE_RATE / 100;
/// Covers both resamplers' filters, and their uneven block sizes.
const DELAY: usize = SAMPLE_RATE / 20;

struct Opts {
    stations: usize,
    mode: Mode,
    paths: usize,
    snr_db: Vec<f32>,
    port: u16,
    calls: Vec<String>,
    seed: u64,
    gear: crate::config::Gear,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        stations: 2,
        mode: Mode::Mpg,
        paths: 2,
        snr_db: vec![10.0],
        port: 8300,
        calls: Vec::new(),
        seed: 1,
        gear: crate::config::Gear::default(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--sim" => {}
            "--stations" => o.stations = val()?.parse().map_err(|_| "--stations")?,
            "--ionos" => {
                let v = val()?.to_ascii_uppercase();
                o.mode = *Mode::ALL
                    .iter()
                    .find(|m| m.name() == v)
                    .ok_or(format!("no IONOS mode {v}"))?;
            }
            "--paths" => o.paths = val()?.parse().map_err(|_| "--paths")?,
            "--snr" => {
                o.snr_db = val()?
                    .split(',')
                    .map(|s| s.trim().parse())
                    .collect::<Result<_, _>>()
                    .map_err(|_| "--snr")?;
            }
            "--port" => o.port = val()?.parse().map_err(|_| "--port")?,
            "--calls" => o.calls = val()?.split(',').map(|s| s.trim().to_ascii_uppercase()).collect(),
            "--seed" => o.seed = val()?.parse().map_err(|_| "--seed")?,
            "--gear" => o.gear = crate::config::Gear::parse(val()?).ok_or("--gear: goodput or ladder")?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if o.stations < 2 {
        return Err("--stations: at least 2".into());
    }
    Ok(o)
}

/// One direction between two stations: 48 kHz → the box at 44.1 kHz → back.
struct Dir {
    up: StreamResampler,
    box_: Ionos,
    down: StreamResampler,
    out: VecDeque<f32>,
    noise: Option<(f32, f32)>,
}

impl Dir {
    fn new(mode: Mode, paths: usize, seed: u64, noise: Option<(f32, f32)>) -> Self {
        Dir {
            up: StreamResampler::new(SAMPLE_RATE as u32, SIM_RATE as u32),
            box_: Ionos::new(mode, paths, 3000, seed),
            down: StreamResampler::new(SIM_RATE as u32, SAMPLE_RATE as u32),
            out: std::iter::repeat_n(0.0, DELAY).collect(),
            noise,
        }
    }

    fn add(&mut self, x: &[f32], y: &mut [f32]) {
        let sim = self.box_.process_sim(&self.up.push(x), self.noise);
        self.out.extend(self.down.push(&sim));
        for v in y.iter_mut() {
            *v += self.out.pop_front().unwrap_or(0.0);
        }
    }
}

struct Air {
    rx: Producer<f32>,
    tx: Consumer<f32>,
    shared: Arc<Shared>,
    cmds: Receiver<LoopCmd>,
}

pub fn run(args: &[String]) {
    let o = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            log::error!("sim: {e}");
            return;
        }
    };
    log::info!(
        "sim: {} stations, IONOS {} {} paths, S/N {:?} dB, ports from {}",
        o.stations,
        o.mode.name(),
        o.paths,
        o.snr_db,
        o.port
    );

    let mut air = Vec::new();
    let mut keep = Vec::new();
    for i in 0..o.stations {
        let (rx_prod, rx_cons) = rtrb::RingBuffer::<f32>::new(2 * SAMPLE_RATE);
        let (tx_prod, tx_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE);
        let shared = Arc::new(Shared::default());
        let (ui_tx, ui_rx) = crossbeam_channel::bounded(16_384);
        let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
        let (_spec_tx, spec_rx) = crossbeam_channel::bounded(32);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let call = o.calls.get(i).cloned().unwrap_or_default();
        let port = o.port + 10 * i as u16;

        let ion = ion_engine::spawn(call, msg_tx.clone(), ui_tx.clone(), Box::new(|| {}));
        let _ = ion.cmds.send(ion_engine::IonCmd::SetGear(o.gear.link()));
        let engine = Engine::new(
            rx_cons,
            tx_prod,
            shared.clone(),
            RadioCtl::Loopback(cmd_tx),
            ui_tx.clone(),
            msg_rx,
            spec_rx,
            Box::new(|| {}),
        );
        let engine = std::thread::Builder::new()
            .name(format!("dsp{i}"))
            .spawn(move || engine.run())
            .expect("spawn dsp thread");
        let _ = msg_tx.send(Msg::Ion(Some(ion.audio.clone())));
        let host = match crate::host::spawn(port, false, ion.cmds.clone(), ui_tx.clone()) {
            Ok(h) => h,
            Err(e) => {
                log::error!("sim: station {i} port {port}: {e}");
                return;
            }
        };
        std::thread::Builder::new()
            .name(format!("log{i}"))
            .spawn(move || report(i, ui_rx))
            .expect("spawn log thread");
        log::info!("sim: station {i} on {port} (data {}, KISS {})", port + 1, port - 200);
        air.push(Air {
            rx: rx_prod,
            tx: tx_cons,
            shared,
            cmds: cmd_rx,
        });
        keep.push((ion, host, engine, msg_tx, _spec_tx, ui_tx));
    }

    let ref_power = TX_LEVEL * TX_LEVEL / 2.0;
    let snr = |j: usize| *o.snr_db.get(j).or(o.snr_db.last()).unwrap_or(&10.0);
    // dirs[i][j]: i → j. Each receiver's noise comes in on one of its paths.
    let mut dirs: Vec<Vec<Option<Dir>>> = (0..o.stations)
        .map(|i| {
            (0..o.stations)
                .map(|j| {
                    let noisy = i == if j == 0 { 1 } else { 0 };
                    let seed = o.seed.wrapping_mul(1_000_003) ^ ((i as u64) << 16 | j as u64);
                    (j != i).then(|| Dir::new(o.mode, o.paths, seed, noisy.then(|| (ref_power, snr(j)))))
                })
                .collect()
        })
        .collect();

    let mut tx = vec![[0.0f32; BLOCK]; o.stations];
    let mut rx = vec![[0.0f32; BLOCK]; o.stations];
    let mut keyed = vec![false; o.stations];
    let mut underruns = vec![0u64; o.stations];
    let mut next = Instant::now();
    loop {
        for (i, a) in air.iter_mut().enumerate() {
            while let Ok(c) = a.cmds.try_recv() {
                if let LoopCmd::Ptt(on) = c
                    && on != keyed[i]
                {
                    keyed[i] = on;
                    log::info!("sim{i}: PTT {}", if on { "on" } else { "off" });
                    let u = a.shared.underruns.load(Ordering::Relaxed);
                    if !on && u > underruns[i] {
                        log::warn!("sim{i}: {} TX underruns in that burst", u - underruns[i]);
                        underruns[i] = u;
                    }
                }
            }
            tx[i].fill(0.0);
            if a.shared.keyed.load(Ordering::Acquire) {
                let n = pop_slice(&mut a.tx, &mut tx[i]);
                if n < BLOCK && !a.shared.tx_done.load(Ordering::Acquire) {
                    a.shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                discard(&mut a.tx);
            }
        }
        for r in rx.iter_mut() {
            r.fill(0.0);
        }
        for (i, row) in dirs.iter_mut().enumerate() {
            for (j, d) in row.iter_mut().enumerate() {
                if let Some(d) = d {
                    d.add(&tx[i], &mut rx[j]);
                }
            }
        }
        for (a, r) in air.iter_mut().zip(&rx) {
            push_slice(&mut a.rx, r);
        }

        next += Duration::from_millis(10);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > Duration::from_millis(200) {
            log::warn!("sim: {} ms behind real time; resync", (now - next).as_millis());
            next = now;
        }
    }
}

/// Logs what each station does, and keeps its UI channel from filling.
fn report(i: usize, ui: Receiver<UiEvent>) {
    let mut last = None;
    for ev in ui {
        match ev {
            UiEvent::Ion(IonEvent::Note(n)) => log::info!("sim{i}: {n}"),
            UiEvent::Ion(IonEvent::Received(b)) => log::info!("sim{i}: received {} bytes", b.len()),
            UiEvent::Ion(IonEvent::Status(s)) => {
                let now = (s.state, s.peer.clone(), s.level);
                if last.as_ref() != Some(&now) {
                    log::info!(
                        "sim{i}: {:?} {} level {} snr {:?} acked {} received {} pending {}",
                        now.0,
                        now.1,
                        now.2,
                        s.snr_db,
                        s.bytes_acked,
                        s.bytes_received,
                        s.pending
                    );
                    last = Some(now);
                }
            }
            UiEvent::Host(n) => log::info!("sim{i}: host {n}"),
            UiEvent::Warning(w) => log::warn!("sim{i}: {w}"),
            _ => {}
        }
    }
}
