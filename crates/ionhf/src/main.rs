//! Ion HF: an HF data modem for host programs (Ragtime, Pat, Winlink
//! Express) on VARA-style host ports and KISS.
//!
//! Usage: ionhf [--loopback] [--tci ws://host:port] [--connect CALL]
//! [--setup] [--log] [--shots DIR] [--quit S] [--loop-snr DB]
//! [--gear goodput|ladder] [--stream on|off] [--size WxH] [--shot-set NAME --shot-at S]
//! ionhf --sim ...: headless stations on a simulated band (see `sim`)

// Release builds are GUI-subsystem exes: no console window on launch.
#![cfg_attr(all(not(debug_assertions), not(test)), windows_subsystem = "windows")]

mod app;
mod config;
mod engine;
mod history;
mod host;
mod ion_engine;
mod logbook;
mod panel;
#[cfg(test)]
mod pipeline_tests;
mod plot;
mod radio;
mod resample;
mod rig;
mod sim;
mod soundcard;
mod theme;
mod views;
mod waterfall;

use std::sync::Arc;

use eframe::egui;

use crate::config::{Backend, Config};
use crate::engine::{Engine, SAMPLE_RATE, Shared};

struct Args {
    loopback: bool,
    tci: Option<String>,
    /// Call this station at start (a test: host programs do this).
    connect: Option<String>,
    /// Open on Setup.
    setup: bool,
    /// Save a screenshot to this folder every 15 s; quit after `quit` s.
    shots: Option<String>,
    quit: Option<f64>,
    /// With `shots`: seconds between screenshots (15).
    shot_every: Option<f64>,
    /// Loopback only: the simulated channel's SNR, dB.
    loop_snr: Option<f32>,
    /// The link's gear for this run, over the setting.
    gear: Option<config::Gear>,
    /// The stream gear for this run, over the setting.
    stream: Option<bool>,
    /// Open on the log.
    log: bool,
    /// Window size in pixels (screenshots); the saved size is left alone.
    size: Option<[f32; 2]>,
    /// With `shots`: one set of screenshots `shot_at` seconds in, at
    /// 1280x720 and 1920x1080, named after `shot_set`.
    shot_set: Option<String>,
    shot_at: Option<f64>,
}

fn parse_args() -> Args {
    let mut args = Args {
        loopback: false,
        tci: None,
        connect: None,
        setup: false,
        shots: None,
        quit: None,
        shot_every: None,
        loop_snr: None,
        gear: None,
        stream: None,
        log: false,
        size: None,
        shot_set: None,
        shot_at: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--loopback" => args.loopback = true,
            "--tci" => args.tci = it.next(),
            "--connect" => args.connect = it.next(),
            "--setup" => args.setup = true,
            "--shots" => args.shots = it.next(),
            "--quit" => args.quit = it.next().and_then(|v| v.parse().ok()),
            "--shot-every" => args.shot_every = it.next().and_then(|v| v.parse().ok()),
            "--loop-snr" => args.loop_snr = it.next().and_then(|v| v.parse().ok()),
            "--gear" => match it.next().as_deref().and_then(config::Gear::parse) {
                Some(g) => args.gear = Some(g),
                None => eprintln!("--gear: goodput or ladder"),
            },
            "--stream" => match it.next().as_deref().and_then(config::parse_on_off) {
                Some(on) => args.stream = Some(on),
                None => eprintln!("--stream: on or off"),
            },
            "--log" => args.log = true,
            "--size" => {
                args.size = it.next().and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some([w.parse().ok()?, h.parse().ok()?])
                })
            }
            "--shot-set" => args.shot_set = it.next(),
            "--shot-at" => args.shot_at = it.next().and_then(|v| v.parse().ok()),
            "-h" | "--help" => {
                println!("ionhf [--loopback] [--tci ws://127.0.0.1:50001] [--connect CALL] [--setup]");
                std::process::exit(0);
            }
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    args
}

/// To stderr, or to `ion.log` in [`Config::dir`] for a release build
/// started without a terminal (from Explorer, Finder or a desktop menu).
fn init_logging() {
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    #[cfg(not(debug_assertions))]
    {
        use std::io::IsTerminal;
        if !std::io::stderr().is_terminal() {
            let dir = Config::dir();
            let file = std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(dir.join("ion.log")));
            if let Ok(file) = file {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
                let default_hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    log::error!("{info}");
                    default_hook(info);
                }));
            }
        }
    }
    builder.init();
}

fn main() -> eframe::Result {
    init_logging();
    log::info!("ion {} starting", env!("CARGO_PKG_VERSION"));
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--sim") {
        sim::run(&argv);
        return Ok(());
    }
    let args = parse_args();
    let mut cfg = Config::load();
    let _ = std::fs::create_dir_all(Config::dir());
    if let Some(url) = args.tci {
        cfg.tci_url = url;
        cfg.radio.backend = Backend::Tci;
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(if args.loopback { "Ion HF (loopback)" } else { "Ion HF" })
        .with_inner_size(args.size.unwrap_or(app::SIZE))
        .with_min_inner_size(app::MIN_SIZE);
    match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(e) => log::warn!("window icon: {e}"),
    }
    let options = eframe::NativeOptions {
        viewport,
        // Beside the settings file, so a portable install keeps it too.
        persistence_path: Some(Config::dir().join("window.ron")),
        persist_window: args.size.is_none(),
        ..Default::default()
    };

    eframe::run_native(
        "Ion HF",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let repaint = move || ctx.request_repaint();
            let s = &cfg;

            let (rx_prod, rx_cons) = rtrb::RingBuffer::<f32>::new(2 * SAMPLE_RATE);
            let (tx_prod, tx_cons) = rtrb::RingBuffer::<f32>::new(SAMPLE_RATE);
            let shared = Arc::new(Shared::default());
            let (ui_tx, ui_rx) = crossbeam_channel::bounded(16_384);
            let (msg_tx, msg_rx) = crossbeam_channel::unbounded();
            let (spec_tx, spec_rx) = crossbeam_channel::bounded(32);

            let radio = if args.loopback {
                radio::spawn_loopback(
                    s.loopback_snr_db,
                    rx_prod,
                    tx_cons,
                    shared.clone(),
                    ui_tx.clone(),
                    Box::new(repaint.clone()),
                )
            } else if s.radio.backend == Backend::Tci {
                radio::spawn_tci(
                    s.tci_url.clone(),
                    s.tci_rate(),
                    rx_prod,
                    tx_cons,
                    shared.clone(),
                    ui_tx.clone(),
                    Box::new(repaint.clone()),
                )
            } else {
                rig::thread::spawn(
                    s.radio.clone(),
                    rx_prod,
                    tx_cons,
                    shared.clone(),
                    ui_tx.clone(),
                    Box::new(repaint.clone()),
                )
            };

            let ion = ion_engine::spawn(
                s.call.to_ascii_uppercase(),
                msg_tx.clone(),
                ui_tx.clone(),
                Box::new(repaint.clone()),
            );

            let engine = Engine::new(
                rx_cons,
                tx_prod,
                shared,
                radio.ctl.clone(),
                ui_tx.clone(),
                msg_rx,
                spec_rx,
                Box::new(repaint),
            );
            let engine = std::thread::Builder::new()
                .name("dsp".into())
                .spawn(move || engine.run())
                .expect("spawn dsp thread");

            let mut app = app::App::new(
                cfg,
                args.loopback,
                msg_tx,
                ui_rx,
                spec_tx,
                app::Threads {
                    engine: Some(engine),
                    radio,
                    ion,
                    ui_tx,
                },
            );
            if let Some(db) = args.loop_snr
                && let Some(lb) = app.threads.radio.ctl.loopback()
            {
                let _ = lb.send(crate::radio::LoopCmd::SnrDb(db));
            }
            if let Some(g) = args.gear {
                let _ = app.threads.ion.cmds.send(ion_engine::IonCmd::SetGear(g.link()));
            }
            if let Some(on) = args.stream {
                let _ = app.threads.ion.cmds.send(ion_engine::IonCmd::SetStream(on));
            }
            if let Some(call) = &args.connect {
                app.connect(call);
            }
            app.setup |= args.setup;
            app.log_view |= args.log;
            app.shots = args.shots.as_ref().map(std::path::PathBuf::from);
            app.shot_set = args.shot_set.zip(args.shot_at);
            app.pixel_size = args.size;
            app.quit_after = args.quit;
            if let Some(e) = args.shot_every {
                app.shot_every = e.max(0.5);
            }
            Ok(Box::new(app))
        }),
    )
}
