//! The channel measurement each receiver makes over a burst (delay and
//! Doppler spread, effective SNR; `ion::chan`), on the Watterson
//! profiles, and how the effective SNR predicts decoding.
//!
//! `cargo test -p ion --release --test chan_meas -- --ignored --nocapture`
//! (`ION_THREADS` workers, default 2).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::level::LADDER;
use ion::link::Air;
use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::frame::{Frame, Kind};
use ion::link::sim::Medium;
use ion::link::station::{Outgoing, user_bits};
use ion_dsp::channel::Profile;

fn threads() -> usize {
    std::env::var("ION_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// A data burst at `level` of about `secs` seconds (at least one frame).
fn burst(level: usize, secs: f64, seed: u32) -> Outgoing {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let lv = LADDER[level];
    let size = lv.frame_bytes();
    let mut n = 1;
    while n < 64 && air.duration(lv.wave, level, (n + 1) * lv.coded_bits()) <= secs {
        n += 1;
    }
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    let frames = (0..n)
        .map(|i| {
            let mut f = Frame::new(Kind::Data, (i * 100) as u32);
            f.payload = (0..Frame::max_payload(size))
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            f.encode(size)
        })
        .collect();
    Outgoing {
        td_ease: false,
        cue: false,
        wave: lv.wave,
        code: level,
        user: user_bits(level, 7, 1),
        extra: 0,
        frames,
        spans: Vec::new(),
    }
}

const PROFILES: [(Profile, &str); 7] = [
    (Profile::Awgn, "AWGN"),
    (Profile::CcirGood, "CCIR good 0.5/0.1"),
    (Profile::CcirPoor, "CCIR poor 2/1"),
    (Profile::CcirFlutter, "flutter 0.5/10"),
    (Profile::LowDisturbed, "low-lat dist 6/10"),
    (Profile::MidDisturbedNvis, "NVIS dist 7/1"),
    (Profile::HighDisturbed, "high-lat dist 7/30"),
];

/// Does the effective SNR predict decoding the same way on every channel?
/// Per level (`ION_LEVELS`, default 2,4,7,8,10), bursts of ~4 s on
/// AWGN, CCIR good and CCIR poor over a range of SNRs around the level's
/// thresholds, `ION_BURSTS` per point (default 6); each burst's
/// codewords lost, binned by the burst's effective SNR (2 dB bins) and,
/// for comparison, by its mean SNR. EESM works if the loss in an
/// effective-SNR bin is about the same on every channel.
#[test]
#[ignore]
fn eff_snr_calibration() {
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let levels: Vec<usize> = std::env::var("ION_LEVELS")
        .map(|v| v.split(',').map(|s| s.parse().unwrap()).collect())
        .unwrap_or(vec![2, 4, 7, 8, 10]);
    let profiles = [
        (Profile::Awgn, "AWGN"),
        (Profile::CcirGood, "CCIR good"),
        (Profile::CcirPoor, "CCIR poor"),
    ];
    let th = ion::level::EFF_THRESHOLDS_DB;
    // (level index, profile, snr offset from the AWGN threshold)
    let offs: Vec<i32> = (-3..=9).step_by(2).collect();
    let (nl, no) = (levels.len(), offs.len());
    let jobs: Vec<(usize, usize, usize)> = (0..nl)
        .flat_map(|l| (0..profiles.len()).flat_map(move |p| (0..no).map(move |o| (l, p, o))))
        .collect();
    // Per level, profile: (eff, mean, lost, total) per burst.
    let res = Mutex::new(vec![vec![Vec::<(f32, f32, usize, usize)>::new(); profiles.len()]; nl]);
    let next = AtomicUsize::new(0);
    let levels = &levels;
    let offs = &offs;
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(li, p, o)) = jobs.get(i) else { break };
                    let level = levels[li];
                    let snr = th[level] + offs[o] as f32;
                    let mut m = AudioMedium::new(profiles[p].0, snr, 7000 + i as u64);
                    let mut v = Vec::new();
                    for b in 0..bursts {
                        let out = burst(level, 4.0, (i * 17 + b) as u32);
                        let n = out.frames.len();
                        let Some(inc) = m.carry(0, &out, 0.0, 0.0) else {
                            continue;
                        };
                        let got = audio::decode_alone(&inc);
                        let ok = got
                            .iter()
                            .zip(&out.frames)
                            .filter(|(g, f)| g.as_ref() == Some(*f))
                            .count();
                        v.push((inc.chan.eff_snr_db, inc.snr_db, n - ok, n));
                    }
                    eprintln!("L{level} {} {snr:+.1}: {v:?}", profiles[p].1);
                    res.lock().unwrap()[li][p].extend(v);
                }
            });
        }
    });
    let res = res.into_inner().unwrap();
    for (li, &level) in levels.iter().enumerate() {
        println!(
            "\nL{level} (AWGN threshold {:+.1} dB): codewords lost by SNR bin (bursts)",
            th[level]
        );
        for (which, name) in [(0, "effective"), (1, "mean")] {
            println!(
                "| {name} SNR | {} |",
                profiles.iter().map(|p| p.1).collect::<Vec<_>>().join(" | ")
            );
            println!("|---:|{}", "---:|".repeat(profiles.len()));
            for bin in (-14..=30).step_by(2) {
                let lo = th[level] + bin as f32 - 1.0;
                let mut cells = Vec::new();
                let mut any = false;
                for r in &res[li] {
                    let (mut lost, mut tot, mut nb) = (0, 0, 0);
                    for x in r {
                        let v = if which == 0 { x.0 } else { x.1 };
                        if v >= lo && v < lo + 2.0 {
                            lost += x.2;
                            tot += x.3;
                            nb += 1;
                        }
                    }
                    any |= nb > 0;
                    cells.push(if nb == 0 {
                        "".to_string()
                    } else {
                        format!("{:.0}% ({nb})", 100.0 * lost as f32 / tot as f32)
                    });
                }
                if any {
                    println!("| {:+} | {} |", th[level] + bin as f32, cells.join(" | "));
                }
            }
        }
    }
}

fn med(v: &mut [f32]) -> f32 {
    let mut w: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if w.is_empty() {
        return f32::NAN;
    }
    w.sort_by(f32::total_cmp);
    w[w.len() / 2]
}

/// Per profile, SNR and waveform (levels `ION_LEVELS`, default
/// 0,1,2,4,7): medians over `ION_BURSTS` bursts (default 6) of the
/// delay span, Doppler spread, effective minus mean SNR and the raw
/// statistics, and the share of codewords decoded.
#[test]
#[ignore]
fn diag_channel_measure() {
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let levels: Vec<usize> = std::env::var("ION_LEVELS")
        .map(|v| v.split(',').map(|s| s.parse().unwrap()).collect())
        .unwrap_or(vec![0, 1, 2, 4, 7]);
    let snrs: Vec<f32> = std::env::var("ION_SNRS")
        .map(|v| v.split(',').map(|s| s.parse().unwrap()).collect())
        .unwrap_or(vec![-4.0, 2.0, 8.0, 14.0]);
    let secs: f64 = std::env::var("ION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4.0);
    let (ns, nl) = (snrs.len(), levels.len());
    let jobs: Vec<(usize, usize, usize)> = (0..PROFILES.len())
        .flat_map(|p| (0..ns).flat_map(move |s| (0..nl).map(move |l| (p, s, l))))
        .collect();
    let levels = &levels;
    let snrs = &snrs;
    let lines = Mutex::new(vec![String::new(); jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(p, k, li)) = jobs.get(i) else { break };
                let level = levels[li];
                let snr = snrs[k];
                let mut m = AudioMedium::new(PROFILES[p].0, snr, 4000 + i as u64);
                let (mut d, mut f, mut e, mut sn) = (vec![], vec![], vec![], vec![]);
                let mut raw = [vec![], vec![], vec![], vec![]];
                let (mut ok, mut tot, mut heard) = (0, 0, 0);
                let (mut echo, mut dopp) = (0, 0);
                for b in 0..bursts {
                    let out = burst(level, secs, (i * 31 + b) as u32);
                    tot += out.frames.len();
                    let Some(inc) = m.carry(0, &out, 0.0, 0.0) else { continue };
                    heard += 1;
                    let got = audio::decode_alone(&inc);
                    ok += got.iter().zip(&out.frames).filter(|(g, f)| g.as_ref() == Some(*f)).count();
                    echo += (inc.chan.delay_ms > ion::chan::ECHO_MS) as usize;
                    dopp += (inc.chan.doppler_hz > ion::chan::DOPPLER_HZ) as usize;
                    d.push(inc.chan.delay_ms);
                    f.push(inc.chan.doppler_hz);
                    e.push(inc.chan.eff_snr_db - inc.snr_db);
                    sn.push(inc.snr_db);
                    for (r, v) in raw.iter_mut().zip(inc.chan.raw) {
                        r.push(v);
                    }
                }
                let line = format!(
                    "{:<20} {:+5.0} L{:<2} heard {heard}/{bursts} cw {:>3.0}% | snr {:+5.1} eff-snr {:+5.1} | delay {:4.1} ms doppler {:5.1} Hz | echo {echo} dopp {dopp} | raw {:6.3} {:6.3} {:6.3} {:6.3}",
                    PROFILES[p].1,
                    snr,
                    level,
                    100.0 * ok as f32 / tot.max(1) as f32,
                    med(&mut sn),
                    med(&mut e),
                    med(&mut d),
                    med(&mut f),
                    med(&mut raw[0]),
                    med(&mut raw[1]),
                    med(&mut raw[2]),
                    med(&mut raw[3]),
                );
                eprintln!("{line}");
                lines.lock().unwrap()[i] = line;
            });
        }
    });
    for l in lines.into_inner().unwrap() {
        println!("{l}");
    }
}
