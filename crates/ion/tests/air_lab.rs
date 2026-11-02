//! Air lab: the ladder's OFDM levels through a radio's nonlinear stages,
//! for the on-air NVIS run of 2026-10-31 (QPSK read +20 to +29 dB while
//! every 16QAM/64QAM burst read +2 to +5 dB and failed). Per level: the
//! SNR the receiver reports and the codeword loss.
//!
//! ```text
//! cargo test -p ion --release --test air_lab -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `AIR_LEVELS` (default `9,10,11,12,13,14,15,16`), `AIR_IMPAIR` (default a
//! set of each kind), `AIR_MODE` (IONOS channel, default WGN), `AIR_SNR`
//! (S:N in 3 kHz against a sine at the clean burst's peak, default 30),
//! `AIR_BURSTS` per point (default 8), `AIR_SECS` (default 4),
//! `ION_THREADS` (default 2).
//!
//! Impairments, applied to the analytic signal (an SSB transmitter's RF
//! envelope) or to the received audio:
//! - `none`
//! - `rapp:B`: PA compression (Rapp, p = 2), saturating B dB below the
//!   burst's peak envelope
//! - `alc:T`: an ALC with 0.5 ms attack and 100 ms release holding the
//!   envelope to T dB below the burst's peak
//! - `clip:C`: received audio hard-clipped C dB above its RMS (a sound card
//!   input driven too hot)

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ion::fec::{self, Rate};
use ion::level::{LADDER, Wave};
use ion::link::audio;
use ion::phy::{self, Constellation, Numerology, PhyRx, Pilots};
use ion_dsp::ionos::{Ionos, Mode, Reference, reference_power};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

fn env<T: std::str::FromStr>(k: &str) -> Option<T> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

fn list(k: &str, default: &str) -> Vec<String> {
    std::env::var(k)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn random_bits(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 33) as u8 & 1
        })
        .collect()
}

const LEAD: usize = 4800;

fn frames_for(num: Numerology, p: Pilots, c: Constellation, r: Rate, secs: f64) -> usize {
    let dur = |f: usize| phy::burst_samples_with(num, p, c, f * r.n()) as f64 / 48_000.0;
    let one = dur(1);
    let frame = dur(2) - one;
    (((secs - (one - frame)) / frame.max(1e-3)).floor() as usize).max(1)
}

fn analytic(x: &[f32]) -> Vec<Complex32> {
    let n = x.len();
    let mut p = FftPlanner::<f32>::new();
    let mut a: Vec<Complex32> = x.iter().map(|&v| Complex32::new(v, 0.0)).collect();
    p.plan_fft_forward(n).process(&mut a);
    for (k, v) in a.iter_mut().enumerate() {
        if k == 0 || 2 * k == n {
            continue;
        }
        *v *= if k < n.div_ceil(2) { 2.0 } else { 0.0 };
    }
    p.plan_fft_inverse(n).process(&mut a);
    let s = 1.0 / n as f32;
    a.iter_mut().for_each(|v| *v *= s);
    a
}

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// The transmitter's stage on `x` (the audio the radio gets).
fn tx_impair(x: &[f32], how: &str) -> Vec<f32> {
    let (kind, arg) = how.split_once(':').unwrap_or((how, "0"));
    if kind == "comp" {
        return compress(x, arg);
    }
    let arg: f32 = arg.parse().expect("impairment argument");
    if !matches!(kind, "rapp" | "alc" | "foff" | "drift" | "pn") {
        return x.to_vec();
    }
    let a = analytic(x);
    if matches!(kind, "foff" | "drift" | "pn") {
        // Frequency: an offset of `arg` Hz, a drift of `arg` Hz/s from 0,
        // or a phase random walk of `arg` degrees rms per second.
        let mut s = 0x1234_5678u64;
        let mut walk = 0f64;
        return a
            .iter()
            .enumerate()
            .map(|(n, v)| {
                let t = n as f64 / 48_000.0;
                let ph = match kind {
                    "foff" => std::f64::consts::TAU * arg as f64 * t,
                    "drift" => std::f64::consts::PI * arg as f64 * t * t,
                    _ => {
                        s ^= s << 13;
                        s ^= s >> 7;
                        s ^= s << 17;
                        let u = (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
                        walk += u * (12.0f64).sqrt() * (arg as f64).to_radians() / 48_000f64.sqrt();
                        walk
                    }
                };
                (*v * Complex32::from_polar(1.0, ph as f32)).re
            })
            .collect();
    }
    let peak = a.iter().map(|v| v.norm()).fold(0.0, f32::max);
    let sat = peak / db(arg);
    match kind {
        "rapp" => {
            let p = 2.0f32;
            a.iter()
                .map(|v| {
                    let e = v.norm();
                    v.re / (1.0 + (e / sat).powf(2.0 * p)).powf(1.0 / (2.0 * p))
                })
                .collect()
        }
        _ => {
            let att = (-1.0 / (0.0005 * 48_000.0f32)).exp();
            let rel = (-1.0 / (0.1 * 48_000.0f32)).exp();
            let mut det = 0f32;
            a.iter()
                .map(|v| {
                    let e = v.norm();
                    det = if e > det {
                        att * det + (1.0 - att) * e
                    } else {
                        rel * det + (1.0 - rel) * e
                    };
                    let g = if det > sat { sat / det } else { 1.0 };
                    // The PA still caps what the ALC is too slow for.
                    let y = *v * g;
                    let ey = y.norm();
                    if ey > sat * db(1.0) {
                        y.re * sat * db(1.0) / ey
                    } else {
                        y.re
                    }
                })
                .collect()
        }
    }
}

/// `comp:T:A:R`: a speech processor holding the envelope to T dB below the
/// burst's peak, attack A ms, release R ms (no PA cap: the processor alone).
fn compress(x: &[f32], arg: &str) -> Vec<f32> {
    let v: Vec<f32> = arg.split(':').map(|s| s.parse().expect("comp T:A:R")).collect();
    let a = analytic(x);
    let peak = a.iter().map(|v| v.norm()).fold(0.0, f32::max);
    let thr = peak / db(v[0]);
    let att = (-1.0 / (v[1] * 0.001 * 48_000.0)).exp();
    let rel = (-1.0 / (v[2] * 0.001 * 48_000.0)).exp();
    let mut det = 0f32;
    a.iter()
        .map(|z| {
            let e = z.norm();
            det = if e > det {
                att * det + (1.0 - att) * e
            } else {
                rel * det + (1.0 - rel) * e
            };
            z.re * if det > thr { thr / det } else { 1.0 }
        })
        .collect()
}

/// The receiving radio's stage on its audio.
fn rx_impair(x: &mut [f32], how: &str) {
    if let Some(arg) = how.strip_prefix("ppm:") {
        // The two sound cards' clocks `arg` ppm apart: the audio stretched
        // by 1 + ppm·1e-6 (Catmull-Rom).
        let r = 1.0 + arg.parse::<f64>().expect("ppm") * 1e-6;
        let src = x.to_vec();
        let at = |i: isize| src[i.clamp(0, src.len() as isize - 1) as usize];
        for (n, v) in x.iter_mut().enumerate() {
            let t = n as f64 / r;
            let i = t.floor() as isize;
            let f = (t - i as f64) as f32;
            let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
            *v = p1 + 0.5 * f * (p2 - p0 + f * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + f * (3.0 * (p1 - p2) + p3 - p0)));
        }
        return;
    }
    if let Some(arg) = how.strip_prefix("agc:") {
        // A rig's AGC: 1 ms attack, `arg` ms decay, holding the audio's
        // envelope to a fixed level (no threshold: the signal is strong).
        let dec: f32 = arg.parse().expect("agc decay ms");
        let att = (-1.0 / (0.001 * 48_000.0f32)).exp();
        let rel = (-1.0 / (dec * 0.001 * 48_000.0)).exp();
        let a = analytic(x);
        let mut det = 1e-6f32;
        for (v, z) in x.iter_mut().zip(&a) {
            let e = z.norm();
            det = if e > det {
                att * det + (1.0 - att) * e
            } else {
                rel * det + (1.0 - rel) * e
            };
            *v = z.re / det.max(1e-6);
        }
        return;
    }
    let Some(arg) = how.strip_prefix("clip:") else { return };
    let c: f32 = arg.parse().expect("clip dB");
    let n = x.iter().filter(|v| **v != 0.0).count().max(1);
    let rms = (x.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
    let lim = rms * db(c);
    x.iter_mut().for_each(|v| *v = v.clamp(-lim, lim));
}

struct Out {
    found: bool,
    snr: f32,
    ok: usize,
    all: usize,
}

fn run(l: usize, mode: Mode, snr: f32, seed: u64, secs: f64, how: &str) -> Out {
    let r = LADDER[l].rate;
    let (got, infos) = received(l, mode, snr, seed, secs, how);
    match got {
        Some((snr, llrs)) => Out {
            found: true,
            snr,
            ok: infos
                .iter()
                .zip(llrs.chunks_exact(r.n()))
                .filter(|(info, w)| fec::decode(r, w).as_deref() == Some(&info[..]))
                .count(),
            all: infos.len(),
        },
        None => Out {
            found: false,
            snr: f32::NAN,
            ok: 0,
            all: infos.len(),
        },
    }
}

/// A burst of level `l` as sent and received: the SNR the receiver read
/// and its LLRs (None: not found), and the frames' information bits.
type Received = (Option<(f32, Vec<f32>)>, Vec<Vec<u8>>);

fn received(l: usize, mode: Mode, snr: f32, seed: u64, secs: f64, how: &str) -> Received {
    let lv = LADDER[l];
    let Wave::Ofdm(g) = lv.wave else {
        panic!("level {l} is not OFDM")
    };
    let num = audio::numerology(g);
    let (c, r, pilots) = (lv.constellation, lv.rate, lv.pilots);
    let center = num.center_hz();
    let user = 0x2000 | (seed % 0x1000) as u16;
    let infos: Vec<Vec<u8>> = (0..frames_for(num, pilots, c, r, secs))
        .map(|f| random_bits(fec::K, seed * 1000 + f as u64))
        .collect();
    let coded: Vec<u8> = infos.iter().flat_map(|i| fec::encode(r, i)).collect();
    // `tdtx`/`tdrx`/`tdboth`: the whole-burst clipper (Clipper = Burst) at
    // the sender, the receiver or both (process-wide: ION_THREADS=1).
    phy::set_td_clip(matches!(how, "tdtx" | "tdboth"));
    let mut tx = phy::modulate_full(num, c, user, 0, &coded, center, &audio::tx_options(l));
    let gain =
        (audio::ofdm_gain(audio::OFDM_AIR_GAIN_DB) * db(audio::level_trim_db(l))).min(db(audio::OFDM_AIR_GAIN_MAX_DB));
    for x in &mut tx {
        *x = (*x * gain).clamp(-1.0, 1.0);
    }
    let reference = reference_power(&tx, Reference::Peak);
    let sent = tx_impair(&tx, how);
    let mut ch = Ionos::new(mode, 2, 3000, seed);
    let mut input = vec![0.0; LEAD];
    input.extend_from_slice(&sent);
    input.resize(input.len() + ch.latency() + LEAD, 0.0);
    let mut audio_in = vec![0.0; input.len()];
    ch.process(&input, Some((reference, snr)), &mut audio_in);
    rx_impair(&mut audio_in, how);
    phy::set_td_clip(matches!(how, "tdrx" | "tdboth"));
    let mut rx = PhyRx::with_numerology(num, center);
    let mut got = Vec::new();
    rx.process(&audio_in, &mut got);
    let got = got
        .into_iter()
        .find(|x| x.user == user && !x.header_only)
        .map(|x| (x.snr_db, x.llrs));
    (got, infos)
}

fn parallel<J: Sync, R: Send>(jobs: &[J], f: impl Fn(&J) -> R + Sync) -> Vec<R> {
    let threads: usize = env("ION_THREADS").unwrap_or(2);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..jobs.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(j) = jobs.get(i) else { break };
                    let r = f(j);
                    out.lock().unwrap()[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.unwrap()).collect()
}

#[test]
#[ignore]
fn nonlinear_air() {
    let levels: Vec<usize> = list("AIR_LEVELS", "9,10,11,12,13,14,15,16")
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    let impair = list("AIR_IMPAIR", "none,rapp:0,rapp:3,rapp:6,alc:3,alc:6,clip:6,clip:3");
    let mode_name = std::env::var("AIR_MODE").unwrap_or_else(|_| "WGN".into());
    let mode = *Mode::ALL
        .iter()
        .find(|m| m.name().eq_ignore_ascii_case(&mode_name))
        .expect("mode");
    let snr: f32 = env("AIR_SNR").unwrap_or(30.0);
    let bursts: u64 = env("AIR_BURSTS").unwrap_or(8);
    let secs: f64 = env("AIR_SECS").unwrap_or(4.0);
    let jobs: Vec<(usize, usize, u64)> = (0..impair.len())
        .flat_map(|i| levels.iter().flat_map(move |&l| (0..bursts).map(move |b| (i, l, b))))
        .collect();
    let res = parallel(&jobs, |&(i, l, b)| {
        run(l, mode, snr, 7 + b + 1000 * l as u64, secs, &impair[i])
    });
    println!(
        "\n### {} at {snr} dB, {bursts} bursts of {secs} s: SNR read (median) / codeword loss",
        mode.name()
    );
    println!(
        "| impairment | {} |",
        levels.iter().map(|l| format!("L{l}")).collect::<Vec<_>>().join(" | ")
    );
    println!("|---|{}", "---|".repeat(levels.len()));
    for (i, how) in impair.iter().enumerate() {
        let mut row = format!("| {how} |");
        for &l in &levels {
            let rs: Vec<&Out> = jobs
                .iter()
                .zip(&res)
                .filter(|(j, _)| j.0 == i && j.1 == l)
                .map(|(_, r)| r)
                .collect();
            let mut s: Vec<f32> = rs.iter().filter(|r| r.found).map(|r| r.snr).collect();
            s.sort_by(f32::total_cmp);
            let med = s.get(s.len() / 2).map_or("–".to_string(), |v| format!("{v:+.0}"));
            let ok: usize = rs.iter().map(|r| r.ok).sum();
            let all: usize = rs.iter().map(|r| r.all).sum();
            row += &format!(" {med} / {:.2} |", 1.0 - ok as f32 / all.max(1) as f32);
        }
        println!("{row}");
    }
}

/// HARQ after a wiped-out burst: a codeword first sent at `from` (its LLRs
/// worth nothing: zero, or random signs of a given size, as a scrambled
/// 64QAM burst leaves them) and then continued by increments at `to`
/// whose bits arrive clean (BPSK at `AIR_ESN0` dB per coded bit, default
/// 6: QPSK reading over +20 dB). Bursts until it decodes, against one
/// fresh send at `to`.
#[test]
#[ignore]
fn increments_after_a_wipeout() {
    use ion::link::harq::{self, HarqRx, Soft};
    let es_n0: f64 = env("AIR_ESN0").unwrap_or(6.0);
    let gauss = |s: &mut u64| {
        let mut u = || {
            *s ^= *s << 13;
            *s ^= *s >> 7;
            *s ^= *s << 17;
            ((*s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        };
        let (a, b) = (u(), u());
        ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()) as f32
    };
    let bpsk = |bits: &[u8], s: &mut u64| -> Vec<f32> {
        let sigma2 = 1.0 / (2.0 * 10f64.powf(es_n0 / 10.0));
        let scale = (2.0 / sigma2) as f32;
        bits.iter()
            .map(|&b| ((if b == 0 { 1.0 } else { -1.0 }) + sigma2.sqrt() as f32 * gauss(s)) * scale)
            .collect()
    };
    println!("\n### increments after a wipeout, clean bits at {es_n0} dB Es/N0 per bit");
    println!(
        "| first | to | wiped LLRs | bursts to decode (20 codewords) | increment bits / a fresh codeword's | fresh at `to` |"
    );
    println!("|---|---|---|---|---|");
    for (from, to) in [(16usize, 9usize), (15, 10), (13, 10), (14, 11)] {
        for junk in [0.01f32, 1.0, 4.0] {
            let mut tries = Vec::new();
            let mut fresh_ok = 0;
            for seed in 0..20u64 {
                let mut s = 0x9E37_79B9 ^ ((seed + 1) * 0x1000_0001);
                let info = random_bits(fec::K, 77 + seed);
                let slot = LADDER[from].coded_bits();
                let first: Vec<f32> = (0..slot).map(|_| junk * gauss(&mut s).signum()).collect();
                let mut rx = HarqRx::new();
                rx.receive(from, 0, &Soft::Llrs(first));
                let mut n = 1;
                let mut sent = 0;
                loop {
                    let h = rx.held()[0];
                    let inc = harq::increment(h, LADDER[to].coded_bits());
                    sent += inc;
                    let llrs = bpsk(&fec::encode_span(&info, harq::start(h), inc), &mut s);
                    let r = rx.receive(to, harq::check_bits(1), &Soft::Llrs(llrs));
                    n += 1;
                    if r.frames[0].is_some() || rx.is_empty() || n > 12 {
                        break;
                    }
                }
                tries.push((n, sent));
                let r = LADDER[to].rate;
                fresh_ok += usize::from(fec::decode(r, &bpsk(&fec::encode(r, &info), &mut s)).is_some());
            }
            tries.sort();
            println!(
                "| L{from} | L{to} | {junk} | median {}, max {} | {:.2} | {fresh_ok}/20 in 1 |",
                tries[tries.len() / 2].0,
                tries.last().unwrap().0,
                tries[tries.len() / 2].1 as f64 / LADDER[to].coded_bits() as f64
            );
        }
    }
}

/// Keep or drop what a failed burst left: a burst at `AIR_FROM` (default
/// 13,14,15,16) through `AIR_MODE` at a range of true S:N, received by the
/// PHY and stored; then increments at L10 whose bits arrive clean (BPSK,
/// 6 dB per bit) until every codeword decodes. Per S:N: the SNR read, the
/// codewords that failed, and the increment bits they took against a
/// fresh L10 codeword each (above 1: dropping the store and sending
/// fresh would have been cheaper), and the rounds.
#[test]
#[ignore]
fn wipeout_keep_or_fresh() {
    use ion::link::harq::{self, HarqRx, Soft};
    let froms: Vec<usize> = list("AIR_FROM", "13,14,15,16")
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    let mode_name = std::env::var("AIR_MODE").unwrap_or_else(|_| "WGN".into());
    let mode = *Mode::ALL
        .iter()
        .find(|m| m.name().eq_ignore_ascii_case(&mode_name))
        .expect("mode");
    let bursts: u64 = env("AIR_BURSTS").unwrap_or(4);
    let to = 10usize;
    let slot_to = LADDER[to].coded_bits();
    let es_n0 = 6.0f64;
    let jobs: Vec<(usize, i32, u64)> = froms
        .iter()
        .flat_map(|&f| {
            let th = ion::level::THRESHOLDS_DB[f].round() as i32;
            (0..=8).flat_map(move |k| (0..bursts).map(move |b| (f, th - 2 - 2 * k, b)))
        })
        .collect();
    let res = parallel(&jobs, |&(from, snr, b)| {
        let seed = 31 + b + 1000 * from as u64;
        let (got, infos) = received(from, mode, snr as f32, seed, 4.0, "none");
        let (read, llrs) = got?;
        let mut rx = HarqRx::new();
        let r = rx.receive(from, 0, &Soft::Llrs(llrs));
        let mut pending: Vec<&Vec<u8>> = infos
            .iter()
            .zip(&r.frames)
            .filter(|(_, f)| f.is_none())
            .map(|(i, _)| i)
            .collect();
        let failed = pending.len();
        let (mut bits, mut rounds) = (0usize, 0usize);
        let mut s = seed | 1;
        let mut gauss = || {
            let mut u = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 11) as f64 + 0.5) / (1u64 << 53) as f64
            };
            let (a, c) = (u(), u());
            ((-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * c).cos()) as f32
        };
        let sigma2 = 1.0 / (2.0 * 10f64.powf(es_n0 / 10.0));
        while !pending.is_empty() && rounds < 12 {
            let held = rx.held();
            if held.len() != pending.len() {
                break; // dropped at H_MAX
            }
            let mut llrs = Vec::new();
            for (info, &h) in pending.iter().zip(&held) {
                let inc = harq::increment(h, slot_to);
                bits += inc;
                for &x in &fec::encode_span(info, harq::start(h), inc) {
                    let y = if x == 0 { 1.0 } else { -1.0 } + sigma2.sqrt() as f32 * gauss();
                    llrs.push(y * (2.0 / sigma2) as f32);
                }
            }
            let r = rx.receive(to, harq::check_bits(pending.len()), &Soft::Llrs(llrs));
            pending = pending
                .into_iter()
                .zip(&r.frames)
                .filter(|(_, f)| f.is_none())
                .map(|(i, _)| i)
                .collect();
            rounds += 1;
        }
        Some((read, failed, infos.len(), bits, rounds))
    });
    println!(
        "\n### keep or fresh after a failed burst, {}, increments at L{to}",
        mode.name()
    );
    println!("| from | true S:N | read | failed | increment bits / fresh | rounds |");
    println!("|---|---|---|---|---|---|");
    let mut keys: Vec<(usize, i32)> = jobs.iter().map(|j| (j.0, j.1)).collect();
    keys.dedup();
    for (from, snr) in keys {
        let rs: Vec<_> = jobs
            .iter()
            .zip(&res)
            .filter(|(j, _)| (j.0, j.1) == (from, snr))
            .filter_map(|(_, r)| *r)
            .collect();
        if rs.is_empty() {
            println!("| L{from} | {snr} | not found | | | |");
            continue;
        }
        let mut reads: Vec<f32> = rs.iter().map(|r| r.0).collect();
        reads.sort_by(f32::total_cmp);
        let failed: usize = rs.iter().map(|r| r.1).sum();
        let all: usize = rs.iter().map(|r| r.2).sum();
        let bits: usize = rs.iter().map(|r| r.3).sum();
        let rounds = rs.iter().map(|r| r.4).max().unwrap_or(0);
        println!(
            "| L{from} | {snr} | {:+.1} | {failed}/{all} | {} | {rounds} |",
            reads[reads.len() / 2],
            if failed > 0 {
                format!("{:.2}", bits as f64 / (failed * slot_to) as f64)
            } else {
                "–".into()
            }
        );
    }
}

/// IONOS (peak-referenced, as on the air), with plain QAM bursts arriving
/// `qam_db` weaker than everything else: a link where 16QAM and 64QAM fail
/// as they did on the NVIS run.
struct QamHostile {
    inner: ion::link::audio::AudioMedium,
    qam_db: f32,
}

impl ion::link::sim::Medium for QamHostile {
    fn carry(&mut self, from: usize, b: &ion::link::Outgoing, start: f64, end: f64) -> Option<ion::link::Incoming> {
        let qam = b.code < LADDER.len()
            && matches!(
                LADDER[b.code].constellation,
                Constellation::Qam16 | Constellation::Qam64
            );
        let keep = self.inner.dir_db[from];
        if qam {
            self.inner.dir_db[from] -= self.qam_db;
        }
        let out = self.inner.carry(from, b, start, end);
        self.inner.dir_db[from] = keep;
        out
    }

    fn expect(&mut self, to: usize, e: &[ion::link::Expected]) {
        self.inner.expect(to, e)
    }

    fn stream(&mut self, to: usize, l: Option<ion::link::stream::Listen>) {
        self.inner.stream(to, l)
    }
}

/// A transfer of `AIR_BYTES` (default 20000) at `AIR_SNR` (default 25) on
/// `AIR_MODE`, plain QAM `AIR_QAM_DB` (default 22) weaker, with and
/// without `harq_drop_wiped`, over `AIR_SEEDS` (default 4): seconds to
/// deliver, increments sent, stores dropped.
#[test]
#[ignore]
fn drop_wiped_on_a_qam_hostile_link() {
    use ion::link::sim::Sim;
    use ion::link::{Air, Station};
    use std::sync::Arc;
    let n: usize = env("AIR_BYTES").unwrap_or(20_000);
    let snr: f32 = env("AIR_SNR").unwrap_or(25.0);
    let qam_db: f32 = env("AIR_QAM_DB").unwrap_or(22.0);
    let seeds: u64 = env("AIR_SEEDS").unwrap_or(4);
    let mode_name = std::env::var("AIR_MODE").unwrap_or_else(|_| "WGN".into());
    let mode = *Mode::ALL
        .iter()
        .find(|m| m.name().eq_ignore_ascii_case(&mode_name))
        .expect("mode");
    let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
    // `AIR_MARGINS`: `off`, or `harq_wiped_margin_db` values.
    let arms: Vec<Option<f32>> = list("AIR_MARGINS", "off,0").iter().map(|m| m.parse().ok()).collect();
    let jobs: Vec<(Option<f32>, u64)> = arms.iter().flat_map(|&d| (0..seeds).map(move |s| (d, s))).collect();
    let res = parallel(&jobs, |&(drop, seed)| {
        let air: Arc<dyn Air> = Arc::new(ion::link::audio::PhyAir);
        let cfg = |call| {
            let mut c = audio::config(call);
            c.harq_drop_wiped = drop.is_some();
            c.harq_wiped_margin_db = drop.unwrap_or(0.0);
            c
        };
        let a = Station::new(cfg("N0JLF"), air.clone());
        let mut b = Station::new(cfg("W1AW"), air.clone());
        b.listen();
        let medium = QamHostile {
            inner: ion::link::audio::AudioMedium::ionos(mode, 2, Reference::Peak, snr, 100 + seed),
            qam_db,
        };
        let mut sim = Sim::new(a, b, Box::new(medium), air);
        sim.stations[0].connect("W1AW", 0.0);
        sim.stations[0].send(&data);
        let ok = sim.run_until(1800.0, |s| s.stations[1].stats().bytes_received >= n as u64);
        let (sa, sb) = (sim.stations[0].stats(), sim.stations[1].stats());
        let qam = sa
            .levels
            .iter()
            .filter(|&&l| matches!(LADDER[l].constellation, Constellation::Qam16 | Constellation::Qam64))
            .count();
        (ok, sim.t, sa.increments_sent, sb.harq_dropped, qam, sa.levels.len())
    });
    println!(
        "\n### {n} B on {} at {snr} dB, plain QAM {qam_db} dB weaker",
        mode.name()
    );
    println!("| drop wiped | seed | done | s | increments | dropped | QAM bursts / data bursts |");
    println!("|---|---|---|---|---|---|---|");
    for ((d, s), r) in jobs.iter().zip(&res) {
        println!(
            "| {d:?} | {s} | {} | {:.0} | {} | {} | {}/{} |",
            r.0, r.1, r.2, r.3, r.4, r.5
        );
    }
    for d in &arms {
        let t: Vec<f64> = jobs
            .iter()
            .zip(&res)
            .filter(|(j, _)| j.0 == *d)
            .map(|(_, r)| r.1)
            .collect();
        let geo = (t.iter().map(|x| x.ln()).sum::<f64>() / t.len() as f64).exp();
        println!("drop wiped {d:?}: geometric mean {geo:.0} s");
    }
}
