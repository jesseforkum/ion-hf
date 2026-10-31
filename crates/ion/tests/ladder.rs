//! Ion end to end in simulation: LDPC + OFDM + Watterson channel + ARQ.
//!
//! The ignored tests print the measurement tables:
//! `cargo test -p ion --release --test ladder -- --ignored --nocapture`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ion::fec::{self, Rate};
use ion::level::{Guard, LADDER, Wave};
use ion::link::audio::{self, AudioMedium, PhyAir};
use ion::link::sim::{Airtime, Sim, level_name};
use ion::link::station::{CONTROL, Outgoing, TINY, user_bits};
use ion::link::{Air, State, Station};
use ion::phy::{self, Constellation, Numerology, PhyRx, Pilots, TxOptions};
use ion_dsp::channel::{Channel, Profile, add_awgn};
use ion_dsp::dsp::{AUDIO_RATE, NoiseGen, noise_sigma};

/// Worker threads for the tables: `ION_THREADS`, default 2.
fn threads() -> usize {
    std::env::var("ION_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

fn payload(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).max(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Gear-shift knobs from the environment, for tuning runs.
fn gear_knobs(c: &mut ion::link::Config) {
    let env = |k: &str| std::env::var(k).ok().map(|v| v.parse::<f64>().unwrap());
    if let Some(v) = env("ION_MARGIN") {
        c.margin_db = v as f32;
    }
    if let Some(v) = env("ION_LOSS_HOLD") {
        c.loss_hold_s = v;
    }
    if let Some(v) = env("ION_SKIP_FROM") {
        c.skip_slices_from = v as usize;
    }
    // ION_PICK_FAMILY=0 leaves the waveform to the hold logic alone,
    // ION_EFF_SNR=0 aims by the mean SNR, ION_EFF_MARGIN moves the
    // effective-SNR targets.
    if let Some(v) = env("ION_PICK_FAMILY") {
        c.pick_family = v != 0.0;
    }
    if let Some(v) = env("ION_EFF_SNR") {
        c.eff_snr = v != 0.0;
    }
    if let Some(v) = env("ION_EFF_MARGIN") {
        c.eff_margin_db = v as f32;
    }
}

/// Overhead knobs from the environment (A/B runs): `ION_OVERHEAD_OFF=1`
/// turns every one off; `ION_ADAPT`,
/// `ION_TINY_ALL` = 0/1 one at a time; `ION_BURST_LONG` (s),
/// `ION_BURST_OVERHEAD` (share), `ION_BURST_STEADY` and
/// `ION_BURST_CLIMB` (dB) tune `burst_adapt`; `ION_CHEAP_ACK` = margin dB or `off`.
fn overhead_knobs(c: &mut ion::link::Config) {
    let env = |k: &str| std::env::var(k).ok();
    let flag = |k: &str| env(k).map(|v| v != "0");
    if flag("ION_OVERHEAD_OFF") == Some(true) {
        c.burst_adapt = false;
        c.tiny_ack_all = false;
        c.cheap_ack_margin_db = None;
    }
    if let Some(v) = flag("ION_ADAPT") {
        c.burst_adapt = v;
    }
    if let Some(v) = flag("ION_TINY_ALL") {
        c.tiny_ack_all = v;
    }
    if let Some(v) = env("ION_BURST_LONG") {
        c.max_burst_long_s = v.parse().unwrap();
    }
    if let Some(v) = env("ION_BURST_OVERHEAD") {
        c.burst_overhead = v.parse().unwrap();
    }
    if let Some(v) = env("ION_BURST_STEADY") {
        c.burst_steady_db = v.parse().unwrap();
    }
    // ION_BURST_CLIMB = dB, or "off".
    if let Some(v) = env("ION_BURST_CLIMB") {
        c.burst_climb_db = if v == "off" { None } else { Some(v.parse().unwrap()) };
    }
    // ION_CHEAP_ACK = margin in dB, or "off".
    if let Some(v) = env("ION_CHEAP_ACK") {
        c.cheap_ack_margin_db = if v == "off" { None } else { Some(v.parse().unwrap()) };
    }
}

fn audio_pair(profile: Profile, snr_db: f32, seed: u64) -> Sim {
    audio_pair_with(profile, snr_db, seed, |_| {})
}

/// [`audio_pair`] with both stations' configuration edited.
fn audio_pair_with(profile: Profile, snr_db: f32, seed: u64, edit: impl Fn(&mut ion::link::Config)) -> Sim {
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let tune = |mut c: ion::link::Config| {
        edit(&mut c);
        // Tuning knobs for the throughput table.
        if let Ok(v) = std::env::var("ION_BURST") {
            c.max_burst_s = v.parse().unwrap();
        }
        // ION_HARQ=0: whole-frame resends only, with a 1 dB gear-shift
        // margin.
        if let Ok(v) = std::env::var("ION_HARQ") {
            c.harq = v != "0";
            if !c.harq {
                c.margin_db = 1.0;
            }
        }
        gear_knobs(&mut c);
        overhead_knobs(&mut c);
        c
    };
    let a = Station::new(tune(audio::config("N0JLF")), air.clone());
    let mut b = Station::new(tune(audio::config("W1AW")), air.clone());
    b.listen();
    // ION_PEP=1: SNR against the bursts' common peak, not each
    // burst's mean power (`AudioMedium::peak_referenced`).
    let pep = std::env::var("ION_PEP").is_ok_and(|v| v == "1");
    let medium = AudioMedium::new(profile, snr_db, seed).peak_referenced(pep);
    Sim::new(a, b, Box::new(medium), air)
}

/// Sends `n` bytes A → B; returns the seconds it took, or None.
fn transfer(sim: &mut Sim, n: usize, t_max: f64) -> Option<f64> {
    let data = payload(n, 11);
    sim.stations[0].connect("W1AW", 0.0);
    sim.stations[0].send(&data);
    let ok = sim.run_until(t_max, |s| s.stations[1].stats().bytes_received >= n as u64);
    if !ok {
        return None;
    }
    assert_eq!(sim.stations[1].take_received(), data, "delivered bytes differ");
    Some(sim.t)
}

#[test]
fn burst_codes_survive_the_air() {
    // Every level, TINY and CONTROL come back from their 4-bit air code in
    // the waveform they are sent in; net control bursts (code 0) too.
    use ion::link::station::{air_code, user_code};
    use ion::phy_mfsk::MfskMode;
    for (l, lv) in LADDER.iter().enumerate() {
        assert!(air_code(l) < 14, "level {l}");
        assert_eq!(user_code(lv.wave, user_bits(l, 9, 3)), l, "level {l}");
    }
    for w in [
        Wave::Mfsk(MfskMode::R0),
        Wave::Ofdm(Guard::Slice),
        Wave::Ofdm(Guard::Normal),
    ] {
        assert_eq!(user_code(w, user_bits(TINY, 9, 3)), TINY);
        assert_eq!(user_code(w, user_bits(CONTROL, 9, 0)), CONTROL);
        assert_eq!(user_code(w, user_bits(0, 9, 0)), 0);
    }
}

#[test]
fn a_slice_burst_is_heard_once_in_its_slot() {
    // `audio::transmit` puts a slice data burst in the third its `extra`
    // names; the receivers listen in all three and hear it exactly once.
    // One nearly empty frame: a hard case for the decoder's capped check
    // messages (`fec::decoder::PHI_MIN`).
    use ion::link::audio::Receivers;
    let level = LADDER.iter().position(|l| l.wave == Wave::Ofdm(Guard::Slice)).unwrap();
    let mut f = ion::link::frame::Frame::new(ion::link::frame::Kind::Data, 9);
    f.payload = b"slot test".to_vec();
    let frames = vec![f.encode(LADDER[level].frame_bytes())];
    for slot in 0..3u16 {
        let out = Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, 9, 1),
            extra: slot,
            frames: frames.clone(),
            spans: Vec::new(),
        };
        assert_eq!(
            audio::center_of(&out),
            audio::CENTER_HZ + ion::slots::OFFSETS_HZ[slot as usize]
        );
        let mut x = vec![0.0; 4800];
        x.extend(audio::transmit(&out));
        x.extend(vec![0.0; 4800]);
        // 30 dB: perfectly clean audio would leave the receiver no noise to
        // measure.
        let p = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
        add_awgn(
            &mut x,
            noise_sigma(p, 30.0, 2400.0, AUDIO_RATE),
            &mut NoiseGen::new(slot as u64 + 1),
        );
        let mut rx = Receivers::new();
        let mut heard = Vec::new();
        rx.process(&x, &mut heard);
        assert_eq!(heard.len(), 1, "slot {slot}: {} bursts", heard.len());
        assert_eq!(heard[0].code, level, "slot {slot}");
        let got: Vec<Option<Vec<u8>>> = audio::decode_alone(&heard[0]);
        assert_eq!(got, frames.iter().cloned().map(Some).collect::<Vec<_>>(), "slot {slot}");
    }
}

#[test]
fn control_tiny_and_data_bursts_survive_the_ofdm_path() {
    let mut f = ion::link::frame::Frame::new(ion::link::frame::Kind::Data, 9);
    f.payload = b"ion over audio".to_vec();
    // A control frame and a tiny message (normal guard), and OFDM levels
    // each on its own guard.
    let first = |w: Wave| LADDER.iter().position(|l| l.wave == w).unwrap();
    let (slice, long, normal) = (
        first(Wave::Ofdm(Guard::Slice)),
        first(Wave::Ofdm(Guard::Long)),
        first(Wave::Ofdm(Guard::Normal)),
    );
    for code in [CONTROL, TINY, slice, long, normal, normal + 5] {
        let guard = match code {
            TINY | CONTROL => Guard::Normal,
            l => match LADDER[l].wave {
                Wave::Ofdm(g) => g,
                w => panic!("{w:?}"),
            },
        };
        let (frames, extra) = match code {
            TINY => (Vec::new(), 0xBEEF),
            CONTROL => (vec![f.encode(fec::K_SHORT / 8)], 0),
            l => (vec![f.encode(LADDER[l].frame_bytes()); 3], 0),
        };
        let out = Outgoing {
            td_ease: false,
            cue: false,
            wave: Wave::Ofdm(guard),
            code,
            user: user_bits(code, 9, if code == TINY { 3 } else { 1 }),
            extra,
            frames: frames.clone(),
            spans: Vec::new(),
        };
        let audio = audio::transmit(&out);
        let mut padded = vec![0.0; 4800];
        padded.extend(audio);
        padded.extend(vec![0.0; 4800]);
        let mut rx = PhyRx::with_numerology(audio::numerology(guard), audio::CENTER_HZ);
        let mut bursts = Vec::new();
        rx.process(&padded, &mut bursts);
        assert_eq!(bursts.len(), 1, "code {code}");
        let inc = audio::decode(&bursts[0]);
        assert_eq!((inc.code, inc.user, inc.extra), (code, out.user, extra), "code {code}");
        let got: Vec<Vec<u8>> = audio::decode_alone(&inc)
            .into_iter()
            .map(|x| x.unwrap_or_else(|| panic!("code {code}: a frame did not decode")))
            .collect();
        assert_eq!(got, frames, "code {code}");
    }
}

#[test]
fn adapted_bursts_fit_the_phys() {
    // The longest burst `burst_adapt` can make at each level stays within
    // what the PHYs can send (OFDM: 1023 data symbols; MFSK: 4095), and
    // `Air::max_bits` says so for any longer cap.
    use ion::level::FrameSize;
    use ion::link::frame::TINY_ACK_FRAMES;
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let st = Station::new(audio::config("N0ABC"), air.clone());
    for (l, lv) in LADDER.iter().enumerate() {
        let max_s = st.clean_burst_s(l);
        let slot = lv.coded_bits();
        let cap = if lv.frame == FrameSize::Short {
            TINY_ACK_FRAMES
        } else {
            64
        };
        let mut n = 1;
        while n < cap && air.duration(lv.wave, l, (n + 1) * slot) <= max_s {
            n += 1;
        }
        let bits = n * slot;
        match lv.wave {
            Wave::Ofdm(g) => {
                let syms = phy::data_symbols_with(audio::numerology(g), lv.pilots, lv.constellation, bits);
                assert!(syms <= phy::MAX_DATA_SYMBOLS, "level {l}: {n} frames, {syms} symbols");
            }
            Wave::Mfsk(m) => assert!(bits <= ion::phy_mfsk::max_bits(m), "level {l}: {n} frames"),
            Wave::Lifeline => unreachable!("not a level"),
        }
        assert!(bits <= air.max_bits(lv.wave, l), "level {l}");
        // A 32 s burst at OFDM-normal would pass the PHY's limit: the
        // station stops at it.
        if lv.wave == Wave::Ofdm(Guard::Normal) {
            let limit = air.max_bits(lv.wave, l);
            let syms = phy::data_symbols_with(audio::numerology(Guard::Normal), lv.pilots, lv.constellation, limit);
            assert_eq!(syms, phy::MAX_DATA_SYMBOLS, "level {l}");
        }
    }
}

#[test]
fn audio_link_transfers_on_awgn() {
    let mut sim = audio_pair(Profile::Awgn, 15.0, 1);
    let secs = transfer(&mut sim, 6_000, 600.0).expect("transfer finished");
    assert!(secs < 120.0, "{secs} s");
    sim.stations[0].disconnect();
    assert!(sim.run_until(sim.t + 60.0, |s| s.stations[1].state() == State::Disconnected));
}

#[test]
fn audio_link_transfers_on_ccir_poor() {
    let mut sim = audio_pair(Profile::CcirPoor, 12.0, 2);
    transfer(&mut sim, 4_000, 900.0).expect("transfer finished");
}

// -- tables -------------------------------------------------------------------

const PROFILES: [Profile; 4] = [
    Profile::Awgn,
    Profile::CcirGood,
    Profile::CcirModerate,
    Profile::CcirPoor,
];

fn rate_name(r: Rate) -> &'static str {
    match r {
        Rate::R1_4 => "1/4",
        Rate::R1_3 => "1/3",
        Rate::R1_2 => "1/2",
        Rate::R2_3 => "2/3",
        Rate::R3_4 => "3/4",
        Rate::R5_6 => "5/6",
    }
}

/// What `ladder_table` measures, from the environment:
///
/// - `ION_NUM` = `narrow` / `wide` / `longwide` / `slice` (default: the
///   link's OFDM-normal numerology, [`audio::numerology`]);
/// - `ION_PILOTS` = `robust` (default) / `sparse`;
/// - `ION_CONS`: constellations, e.g. `qpsk,8psk,16qam,64qam` (default
///   all); `ION_RATES`: rates, e.g. `3/4,5/6` (default all);
/// - `ION_CLIP`: clip in dB for every burst, or `none` (default: the
///   link's, [`TxOptions::for_burst`]);
/// - `ION_PEP=1`: SNR against peak power instead of each burst's mean;
/// - `ION_SPREAD=1`: DFT-spread data symbols; `ION_GAIN`: data gain
///   over the header, dB; `ION_HDR_CLIP`: the header's clip, dB
///   (`phy::TxOptions`);
/// - `ION_LOWPAPR=1`: every burst in the link's low-PAPR mode
///   (`TxOptions::low_papr`); `ladder`: as `LADDER` says, level by level
///   (`threshold_table`).
#[derive(Clone, Copy, Debug)]
struct Setup {
    num: Numerology,
    pilots: Pilots,
    clip: Option<Option<f32>>,
    /// `ION_PEP=1`: SNR referenced to peak power.
    pep: bool,
    spread: bool,
    gain: f32,
    hdr_clip: Option<f32>,
    low_papr: bool,
    /// `ION_LOWPAPR=ladder`.
    ladder_papr: bool,
}

impl Setup {
    fn from_env() -> Self {
        let num = match std::env::var("ION_NUM").as_deref() {
            Ok("narrow") => Numerology::Normal,
            Ok("wide") => Numerology::NormalWide,
            Ok("longwide") => Numerology::LongWide,
            Ok("slice") => Numerology::Slice,
            _ => audio::numerology(Guard::Normal),
        };
        let pilots = match std::env::var("ION_PILOTS").as_deref() {
            Ok("sparse") => Pilots::Sparse,
            _ => Pilots::Robust,
        };
        let clip = std::env::var("ION_CLIP")
            .ok()
            .map(|v| if v == "none" { None } else { Some(v.parse().unwrap()) });
        let pep = std::env::var("ION_PEP").is_ok_and(|v| v == "1");
        let spread = std::env::var("ION_SPREAD").is_ok_and(|v| v == "1");
        let gain = std::env::var("ION_GAIN").ok().map_or(0.0, |v| v.parse().unwrap());
        let hdr_clip = std::env::var("ION_HDR_CLIP").ok().map(|v| v.parse().unwrap());
        let lp = std::env::var("ION_LOWPAPR").unwrap_or_default();
        Self {
            num,
            pilots,
            clip,
            pep,
            spread,
            gain,
            hdr_clip,
            low_papr: lp == "1",
            ladder_papr: lp == "ladder",
        }
    }

    fn tx(&self, c: Constellation) -> TxOptions {
        if self.low_papr {
            return TxOptions::low_papr(c, self.pilots);
        }
        let mut o = TxOptions::for_burst(c, self.pilots);
        if let Some(clip) = self.clip {
            o.clip_db = clip;
        }
        if self.spread || self.gain > 0.0 {
            o.spread = self.spread;
            o.data_gain_db = self.gain;
            o.hdr_clip_db = self.hdr_clip;
        }
        o
    }

    fn data_carriers(&self) -> usize {
        let i = phy::info_for(self.num);
        match self.pilots {
            Pilots::Robust => i.data_carriers,
            Pilots::Sparse => i.sparse_data_carriers,
        }
    }
}

/// Codeword error rate of one (constellation, rate) at one SNR, over bursts
/// of about 1.5 s on `profile`.
fn cer(set: &Setup, c: Constellation, r: Rate, profile: Profile, snr_db: f32, bursts: usize, seed: u64) -> f32 {
    cer_with(set, &set.tx(c), c, r, profile, snr_db, bursts, seed)
}

/// [`cer`] with explicit transmit options.
#[allow(clippy::too_many_arguments)]
fn cer_with(
    set: &Setup,
    opt: &TxOptions,
    c: Constellation,
    r: Rate,
    profile: Profile,
    snr_db: f32,
    bursts: usize,
    seed: u64,
) -> f32 {
    let per_symbol = set.data_carriers() * c.bits_per_symbol();
    let frames = (60 * per_symbol / r.n()).max(1);
    let center = set.num.center_hz();
    let mut ch = Channel::from_profile(profile, seed);
    let mut noise = NoiseGen::new(seed ^ 77);
    let mut rx = PhyRx::with_numerology(set.num, center);
    let mut found = Vec::new();
    let (mut bad, mut total) = (0, 0);
    for b in 0..bursts {
        let infos: Vec<Vec<u8>> = (0..frames)
            .map(|i| payload(fec::K / 8, (b * 31 + i) as u32 + seed as u32))
            .collect();
        let mut bits = Vec::new();
        for info in &infos {
            bits.extend(fec::encode(r, &audio::bytes_to_bits(info)));
        }
        let tx = phy::modulate_full(set.num, c, 0, 0, &bits, center, opt);
        // `pep`: SNR against the mean power of a constant-envelope tone at
        // the burst's peak (a PEP-limited transmitter; every burst peaks at
        // `TX_LEVEL`), not the burst's own mean power.
        let power = if set.pep {
            ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0
        } else {
            tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32
        };
        let mut input = vec![0.0; 4800];
        input.extend(&tx);
        input.resize(input.len() + ch.latency() + 4800, 0.0);
        let mut out = vec![0.0; input.len()];
        ch.process(&input, &mut out);
        add_awgn(&mut out, noise_sigma(power, snr_db, 2400.0, AUDIO_RATE), &mut noise);
        found.clear();
        rx.process(&out, &mut found);
        total += frames;
        let Some(burst) = found.first() else {
            bad += frames;
            continue;
        };
        for (i, llrs) in burst.llrs.chunks_exact(r.n()).take(frames).enumerate() {
            let ok = fec::decode(r, llrs).is_some_and(|d| audio::bits_to_bytes(&d) == infos[i]);
            if !ok {
                bad += 1;
            }
        }
    }
    bad as f32 / total as f32
}

/// For every constellation × rate and channel: the lowest SNR (2 dB steps)
/// from which at most 10% of codewords are lost. See [`Setup`] for the
/// knobs.
#[test]
#[ignore]
fn ladder_table() {
    let set = Setup::from_env();
    // ION_SNR_MIN: lowest SNR tried (default −4 dB).
    let lo: i32 = std::env::var("ION_SNR_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(-4);
    let snrs: Vec<f32> = (lo..=36).step_by(2).map(|s| s as f32).collect();
    let cons: Vec<Constellation> = match std::env::var("ION_CONS") {
        Ok(v) => v
            .split(',')
            .map(|s| match s {
                "qpsk" => Constellation::Qpsk,
                "8psk" => Constellation::Psk8,
                "16qam" => Constellation::Qam16,
                "64qam" => Constellation::Qam64,
                _ => panic!("ION_CONS: {s}"),
            })
            .collect(),
        Err(_) => Constellation::ALL.to_vec(),
    };
    let rates: Vec<Rate> = match std::env::var("ION_RATES") {
        Ok(v) => Rate::ALL
            .into_iter()
            .filter(|&r| v.split(',').any(|s| s == rate_name(r)))
            .collect(),
        Err(_) => Rate::ALL.to_vec(),
    };
    let combos: Vec<(Constellation, Rate)> = cons.iter().flat_map(|&c| rates.iter().map(move |&r| (c, r))).collect();
    // One job per (combo, channel), on `ION_THREADS` workers.
    let jobs: Vec<(usize, usize)> = (0..combos.len())
        .flat_map(|k| (0..PROFILES.len()).map(move |p| (k, p)))
        .collect();
    let cells = Mutex::new(vec![vec![None; PROFILES.len()]; combos.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(k, p)) = jobs.get(i) else { break };
                    let (c, r) = combos[k];
                    let t = snrs
                        .iter()
                        .copied()
                        .find(|&snr| cer(&set, c, r, PROFILES[p], snr, 20, 1000 + snr as u64) <= 0.10);
                    cells.lock().unwrap()[k][p] = t;
                }
            });
        }
    });
    let results: Vec<Vec<Option<f32>>> = cells.into_inner().unwrap();
    println!("{set:?}\n");
    println!("| Constellation | Rate | User bit/s | AWGN | CCIR good | CCIR moderate | CCIR poor |");
    println!("|---|---|---:|---:|---:|---:|---:|");
    for ((c, r), row) in combos.iter().zip(&results) {
        let bps = set.data_carriers() as f64 * c.bits_per_symbol() as f64 / phy::info_for(set.num).symbol_s as f64
            * (fec::K as f64 / r.n() as f64);
        let cells: Vec<String> = row
            .iter()
            .map(|t| t.map_or("—".to_string(), |v| format!("{v:+.0}")))
            .collect();
        println!("| {c:?} | {} | {bps:.0} | {} |", rate_name(*r), cells.join(" | "));
    }
}

/// Clip level against PEP-limited performance: for the OFDM levels of
/// `LADDER` (`ION_LEVELS`, comma-separated indices; default all), on
/// AWGN and CCIR poor, codeword loss at five SNRs 1 dB apart around the
/// level's threshold, with the SNR referenced to **peak** power, for each
/// clip in `ION_CLIPS` (default 6,5,4.5,4; 64QAM
/// levels add their own 8 dB), `ION_BURSTS` bursts per point (default
/// 40). Prints the loss and the interpolated SNR of 10% loss per clip.
///
/// `ION_CONFIGS` replaces the clips by whole transmit setups,
/// `clip:spread:gain;...` (`spread` 0/1, data gain in dB; the header is
/// clipped at `ION_HDR_CLIP`, default 3.5 dB, when spread or gain is
/// set). `ION_SHIFT` moves the five SNRs (dB), `ION_SPAN` widens
/// them (default 2: centre ± 2 dB).
#[test]
#[ignore]
fn clip_table() {
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let levels: Vec<usize> = std::env::var("ION_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| {
            (0..LADDER.len())
                .filter(|&l| matches!(LADDER[l].wave, Wave::Ofdm(_)))
                .collect()
        });
    let clips: Vec<f32> = std::env::var("ION_CLIPS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![6.0, 5.0, 4.5, 4.0]);
    // (clip, spread, gain)
    let configs: Option<Vec<(f32, bool, f32)>> = std::env::var("ION_CONFIGS").ok().map(|v| {
        v.split(';')
            .map(|r| {
                let f: Vec<&str> = r.split(':').collect();
                (f[0].parse().unwrap(), f[1] == "1", f[2].parse().unwrap())
            })
            .collect()
    });
    let hdr_clip: f32 = std::env::var("ION_HDR_CLIP").ok().map_or(3.5, |v| v.parse().unwrap());
    let shift: i32 = std::env::var("ION_SHIFT").ok().map_or(0, |v| v.parse().unwrap());
    let span: i32 = std::env::var("ION_SPAN").ok().map_or(2, |v| v.parse().unwrap());
    let profiles = [Profile::Awgn, Profile::CcirPoor];
    let mut jobs = Vec::new();
    for &l in &levels {
        let cl: Vec<(f32, bool, f32)> = match &configs {
            Some(c) => c.clone(),
            None => {
                let mut cl = clips.clone();
                if LADDER[l].constellation == Constellation::Qam64 && !cl.contains(&8.0) {
                    cl.insert(0, 8.0);
                }
                cl.into_iter().map(|c| (c, false, 0.0)).collect()
            }
        };
        for (pi, &p) in profiles.iter().enumerate() {
            for &c in &cl {
                jobs.push((l, pi, p, c));
            }
        }
    }
    let out = Mutex::new(vec![(Vec::new(), None::<f32>); jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(l, pi, prof, (clip, spread, gain))) = jobs.get(i) else {
                        break;
                    };
                    let lv = LADDER[l];
                    let Wave::Ofdm(g) = lv.wave else { break };
                    let set = Setup {
                        num: audio::numerology(g),
                        pilots: lv.pilots,
                        clip: Some(Some(clip)),
                        pep: true,
                        spread,
                        gain,
                        hdr_clip: Some(hdr_clip),
                        low_papr: false,
                        ladder_papr: false,
                    };
                    // Mean-power threshold (`level::THRESHOLDS_DB` ± half the
                    // AWGN–poor spread) plus the 6 dB clip's 6.7 dB from the
                    // burst's mean power up to the reference (a tone at the
                    // same peak), rounded.
                    let th = ion::level::THRESHOLDS_DB[l] + if pi == 0 { -2.5 } else { 2.5 } + 6.7;
                    let c0 = th.round() as i32 + shift;
                    let pts: Vec<(i32, f32)> = (c0 - span..=c0 + span)
                        .map(|snr| {
                            (
                                snr,
                                cer(
                                    &set,
                                    lv.constellation,
                                    lv.rate,
                                    prof,
                                    snr as f32,
                                    bursts,
                                    7000 + (snr + 100) as u64,
                                ),
                            )
                        })
                        .collect();
                    // Interpolated 10% point (log loss, linear in dB).
                    let mut t = None;
                    for w in pts.windows(2) {
                        let (a, b) = (w[0], w[1]);
                        if a.1 > 0.10 && b.1 <= 0.10 {
                            let la = a.1.max(1e-3).ln();
                            let lb = b.1.max(1e-3).ln();
                            let f = (la - 0.1f32.ln()) / (la - lb);
                            t = Some(a.0 as f32 + f);
                        }
                    }
                    if pts[0].1 <= 0.10 {
                        t = Some(f32::NEG_INFINITY);
                    }
                    out.lock().unwrap()[i] = (pts, t);
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!("{bursts} bursts per point; SNR against peak power (PEP − 3 dB)\n");
    println!("| Level | Channel | Clip | Shape | Gain | loss at each SNR | 10% loss at (dB) |");
    println!("|---:|---|---:|---|---:|---|---:|");
    for ((l, pi, _, (clip, spread, gain)), (pts, t)) in jobs.iter().zip(&out) {
        let lv = LADDER[*l];
        let row: Vec<String> = pts.iter().map(|(s, c)| format!("{s:+}:{c:.2}")).collect();
        println!(
            "| {l} {:?} {} | {} | {clip} | {} | {gain} | {} | {} |",
            lv.constellation,
            rate_name(lv.rate),
            ["AWGN", "CCIR poor"][*pi],
            if *spread { "DFT-s" } else { "plain" },
            row.join(" "),
            t.map_or("—".into(), |v| if v.is_finite() {
                format!("{v:+.1}")
            } else {
                "below".into()
            })
        );
    }
}

/// An `ION_TD` config for level `l`: `link` (the link's options) or
/// `clip:spread:gain:headroom:k1/k2/..` (`clip`, `headroom`: `none`).
fn td_opt(cfg: &str, l: usize) -> TxOptions {
    if cfg == "link" {
        return audio::tx_options(l);
    }
    let f: Vec<&str> = cfg.split(':').collect();
    let lv = LADDER[l];
    let opt_none = |v: &str| {
        if v == "none" {
            None
        } else {
            Some(v.parse::<f32>().unwrap())
        }
    };
    TxOptions {
        clip_db: opt_none(f[0]),
        spread: f[1] == "1",
        data_gain_db: f[2].parse().unwrap(),
        hdr_clip_db: None,
        td_clip: opt_none(f[3]).map(|h| phy::TdClip {
            headroom_db: h,
            overshoot: Box::leak(
                f[4].split('/')
                    .map(|k| k.parse().unwrap())
                    .collect::<Vec<f32>>()
                    .into_boxed_slice(),
            ),
        }),
        ..TxOptions::for_burst(lv.constellation, lv.pilots)
    }
}

/// Data2G's whole-burst clipper against the link's: per OFDM level,
/// channel and config, the PEP-referenced SNR of 10% codeword loss,
/// bracketed on a 1 dB grid from the level's threshold and interpolated.
/// `ION_TD` holds `;`-separated configs, each `link` (the link's options)
/// or `clip:spread:gain:headroom:k1/k2/..` (`clip`, `headroom`: `none`).
#[test]
#[ignore]
fn td_clip_table() {
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let levels: Vec<usize> = std::env::var("ION_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| {
            (0..LADDER.len())
                .filter(|&l| matches!(LADDER[l].wave, Wave::Ofdm(_)))
                .collect()
        });
    let profiles: Vec<Profile> = match std::env::var("ION_PROFILES").as_deref() {
        Ok("awgn") => vec![Profile::Awgn],
        Ok("poor") => vec![Profile::CcirPoor],
        _ => vec![Profile::Awgn, Profile::CcirPoor],
    };
    let configs: Vec<String> = std::env::var("ION_TD")
        .unwrap_or_else(|_| "link".into())
        .split(';')
        .map(str::to_owned)
        .collect();
    let mut jobs = Vec::new();
    for &l in &levels {
        for (pi, &p) in profiles.iter().enumerate() {
            for cfg in &configs {
                jobs.push((l, pi, p, cfg.clone()));
            }
        }
    }
    let out = Mutex::new(vec![(Vec::new(), None::<f32>); jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((l, _, prof, cfg)) = jobs.get(i) else {
                        break;
                    };
                    let (l, prof) = (*l, *prof);
                    let lv = LADDER[l];
                    let Wave::Ofdm(g) = lv.wave else { break };
                    let set = Setup {
                        num: audio::numerology(g),
                        pilots: lv.pilots,
                        clip: None,
                        pep: true,
                        spread: false,
                        gain: 0.0,
                        hdr_clip: None,
                        low_papr: false,
                        ladder_papr: false,
                    };
                    let opt = td_opt(cfg, l);
                    let at = |snr: i32| {
                        cer_with(
                            &set,
                            &opt,
                            lv.constellation,
                            lv.rate,
                            prof,
                            snr as f32,
                            bursts,
                            7000 + (snr + 100) as u64,
                        )
                    };
                    // From the mean-power threshold plus ~4 dB of PAPR, walk
                    // to the first 1 dB step that crosses 10%.
                    let off = if prof == Profile::Awgn { -2.5 } else { 2.5 };
                    let mut snr = (ion::level::THRESHOLDS_DB[l] + off + 4.0).round() as i32;
                    let mut pts = vec![(snr, at(snr))];
                    let up = pts[0].1 > 0.10;
                    for _ in 0..12 {
                        snr += if up { 1 } else { -1 };
                        let c = at(snr);
                        pts.push((snr, c));
                        if (c > 0.10) != up {
                            break;
                        }
                    }
                    pts.sort_by_key(|p| p.0);
                    let mut t = None;
                    for w in pts.windows(2) {
                        let (a, b) = (w[0], w[1]);
                        if a.1 > 0.10 && b.1 <= 0.10 {
                            let la = a.1.max(1e-3).ln();
                            let lb = b.1.max(1e-3).ln();
                            t = Some(a.0 as f32 + (la - 0.1f32.ln()) / (la - lb));
                        }
                    }
                    out.lock().unwrap()[i] = (pts, t);
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!("{bursts} bursts per point; SNR against peak power (PEP − 3 dB)\n");
    println!("| Level | Channel | Config | loss at each SNR | 10% loss at (dB) |");
    println!("|---|---|---|---|---:|");
    for ((l, _, p, cfg), (pts, t)) in jobs.iter().zip(&out) {
        let lv = LADDER[*l];
        let row: Vec<String> = pts.iter().map(|(s, c)| format!("{s:+}:{c:.2}")).collect();
        println!(
            "| {l} {:?} {} | {} | {cfg} | {} | {} |",
            lv.constellation,
            rate_name(lv.rate),
            if *p == Profile::Awgn { "AWGN" } else { "CCIR poor" },
            row.join(" "),
            t.map_or("—".into(), |v| format!("{v:+.2}"))
        );
    }
}

/// Gear-shift thresholds of the OFDM-normal levels of `LADDER`, finer than
/// `ladder_table`: the lowest SNR on a 1 dB grid with at most 10% of
/// codewords lost over `ION_BURSTS` bursts (default 40), on AWGN and
/// CCIR poor, and their midpoint (`level::THRESHOLDS_DB`).
#[test]
#[ignore]
fn threshold_table() {
    let set = Setup::from_env();
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    // ION_GUARD = slice / long: those levels instead of OFDM-normal.
    let guard = match std::env::var("ION_GUARD").as_deref() {
        Ok("slice") => Guard::Slice,
        Ok("long") => Guard::Long,
        _ => Guard::Normal,
    };
    let set = Setup {
        num: audio::numerology(guard),
        ..set
    };
    // ION_LEVELS: only these levels (comma-separated indices).
    let only: Option<Vec<usize>> = std::env::var("ION_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect());
    let levels: Vec<usize> = (0..LADDER.len())
        .filter(|&i| LADDER[i].wave == Wave::Ofdm(guard))
        .filter(|i| only.as_ref().is_none_or(|o| o.contains(i)))
        .collect();
    // Search upward from a little below the 2 dB table.
    let start = |c: Constellation, prof: Profile| -> i32 {
        let base = match c {
            Constellation::Qpsk if guard == Guard::Slice => -9,
            Constellation::Qpsk => -2,
            Constellation::Psk8 => 3,
            Constellation::Qam16 => 8,
            Constellation::Qam64 => 14,
        };
        if prof == Profile::Awgn { base } else { base + 4 }
    };
    let jobs: Vec<(usize, Profile)> = levels
        .iter()
        .flat_map(|&l| [Profile::Awgn, Profile::CcirPoor].map(move |p| (l, p)))
        .collect();
    let out = Mutex::new(vec![None; jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(l, prof)) = jobs.get(i) else { break };
                    let lv = LADDER[l];
                    let low_papr = set.low_papr || (set.ladder_papr && lv.low_papr);
                    let set = Setup {
                        pilots: lv.pilots,
                        low_papr,
                        ..set
                    };
                    let s0 = start(lv.constellation, prof);
                    let t = (s0..=s0 + 20).find(|&snr| {
                        cer(
                            &set,
                            lv.constellation,
                            lv.rate,
                            prof,
                            snr as f32,
                            bursts,
                            5000 + snr as u64,
                        ) <= 0.10
                    });
                    out.lock().unwrap()[i] = t;
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!("{set:?}, {bursts} bursts per point\n");
    println!("| Level | Level | AWGN | CCIR poor | Midpoint |");
    println!("|---:|---|---:|---:|---:|");
    for (k, &l) in levels.iter().enumerate() {
        let (a, p) = (out[2 * k], out[2 * k + 1]);
        let mid = match (a, p) {
            (Some(a), Some(p)) => format!("{:.1}", (a + p) as f32 / 2.0),
            _ => "—".into(),
        };
        let f = |t: Option<i32>| t.map_or("—".to_string(), |v| format!("{v:+}"));
        println!(
            "| {l} | {:?} {} | {} | {} | {mid} |",
            LADDER[l].constellation,
            rate_name(LADDER[l].rate),
            f(a),
            f(p)
        );
    }
}

/// Robust against sparse pilots, finer than `ladder_table`'s 2 dB grid:
/// codeword loss of the upper levels at fixed SNRs around their
/// thresholds, `ION_BURSTS` bursts (default 40) per point, the link's
/// numerology (or `ION_NUM`).
#[test]
#[ignore]
fn pilots_table() {
    let base = Setup::from_env();
    let bursts: usize = std::env::var("ION_BURSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    // (constellation, rate, AWGN SNRs, CCIR poor SNRs)
    let points: [(Constellation, Rate, [f32; 3], [f32; 3]); 8] = [
        (Constellation::Qpsk, Rate::R1_2, [0.0, 1.0, 2.0], [4.0, 6.0, 8.0]),
        (Constellation::Psk8, Rate::R2_3, [6.0, 7.0, 8.0], [10.0, 12.0, 14.0]),
        (Constellation::Qam16, Rate::R2_3, [8.0, 9.0, 10.0], [12.0, 14.0, 16.0]),
        (Constellation::Qam16, Rate::R3_4, [10.0, 11.0, 12.0], [14.0, 16.0, 18.0]),
        (Constellation::Qam16, Rate::R5_6, [12.0, 13.0, 14.0], [16.0, 18.0, 20.0]),
        (Constellation::Qam64, Rate::R2_3, [14.0, 15.0, 16.0], [18.0, 20.0, 22.0]),
        (Constellation::Qam64, Rate::R3_4, [16.0, 17.0, 18.0], [20.0, 22.0, 24.0]),
        (Constellation::Qam64, Rate::R5_6, [18.0, 19.0, 20.0], [22.0, 24.0, 26.0]),
    ];
    let mut jobs = Vec::new();
    for (k, &(c, r, aw, po)) in points.iter().enumerate() {
        for (pi, (prof, snrs)) in [(Profile::Awgn, aw), (Profile::CcirPoor, po)].into_iter().enumerate() {
            for (si, &snr) in snrs.iter().enumerate() {
                for pil in [Pilots::Robust, Pilots::Sparse] {
                    jobs.push((k, pi, si, pil, c, r, prof, snr));
                }
            }
        }
    }
    let out = Mutex::new(vec![0f32; jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(_, _, _, pil, c, r, prof, snr)) = jobs.get(i) else {
                        break;
                    };
                    let set = Setup { pilots: pil, ..base };
                    let v = cer(&set, c, r, prof, snr, bursts, 3000 + snr as u64);
                    out.lock().unwrap()[i] = v;
                }
            });
        }
    });
    let out = out.into_inner().unwrap();
    println!("{base:?}, {bursts} bursts per point; codeword loss robust / sparse\n");
    println!("| Level | Channel | SNR | Robust | Sparse |");
    println!("|---|---|---:|---:|---:|");
    for w in jobs.chunks(2).zip(out.chunks(2)) {
        let (j, v) = w;
        let (_, pi, _, _, c, r, _, snr) = j[0];
        println!(
            "| {c:?} {} | {} | {snr:+} | {:.3} | {:.3} |",
            rate_name(r),
            ["AWGN", "CCIR poor"][pi],
            v[0],
            v[1]
        );
    }
}

/// Link throughput (payload bytes delivered / time, ARQ and turnarounds
/// included) against SNR per channel, on `ION_THREADS` workers.
/// `ION_SNRS` (e.g. `22,26,30`) picks the rows (default −2 to +30 dB),
/// `ION_BYTES` the transfer size (default 20 000).
/// `ION_PEP=1`: noise against the bursts' common peak, not each burst's
/// mean power (`AudioMedium::peak_referenced`; also for `horrid_table`).
#[test]
#[ignore]
fn link_throughput_table() {
    // ION_SNR_MIN skips the low rows; ION_SEEDS averages several
    // transfers per cell.
    let bytes: usize = std::env::var("ION_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let snr_min: i32 = std::env::var("ION_SNR_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(-2);
    let seeds: u64 = std::env::var("ION_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let snrs: Vec<i32> = match std::env::var("ION_SNRS") {
        Ok(v) => v.split(',').map(|s| s.parse().unwrap()).collect(),
        Err(_) => (-2..=30).step_by(4).filter(|&s| s >= snr_min).collect(),
    };
    let jobs: Vec<(usize, usize, u64)> = (0..snrs.len())
        .flat_map(|s| (0..PROFILES.len()).flat_map(move |p| (0..seeds).map(move |k| (s, p, k))))
        .collect();
    // Per cell, per seed: (bit/s, finished).
    let runs = Mutex::new(vec![vec![Vec::new(); PROFILES.len()]; snrs.len()]);
    // Per cell, per seed: (failed data bursts, data bursts).
    let fails = Mutex::new(vec![vec![Vec::new(); PROFILES.len()]; snrs.len()]);
    // Where the air time went, per SNR row (all channels and seeds).
    let air = Mutex::new(vec![Airtime::default(); snrs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(k, p, seed)) = jobs.get(i) else { break };
                    let snr = snrs[k];
                    let mut sim = audio_pair(PROFILES[p], snr as f32, 500 + snr as u64 + 1000 * seed);
                    let n = bytes;
                    let run = match transfer(&mut sim, n, 1800.0) {
                        Some(t) => (n as f64 * 8.0 / t, true),
                        None => (sim.stations[1].stats().bytes_received as f64 * 8.0 / sim.t, false),
                    };
                    let st = sim.stations[0].stats();
                    eprintln!(
                        "{snr:+} {:?} seed {seed}: {:.0} bit/s, failed {}/{} ({} OFDM)",
                        PROFILES[p], run.0, st.failed_bursts, st.data_bursts, st.failed_ofdm
                    );
                    runs.lock().unwrap()[k][p].push(run);
                    fails.lock().unwrap()[k][p].push((st.failed_bursts, st.data_bursts));
                    air.lock().unwrap()[k].add(&sim.airtime());
                }
            });
        }
    });
    let rows: Vec<Vec<String>> = runs
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|r| {
                    let mean = r.iter().map(|x| x.0).sum::<f64>() / r.len() as f64;
                    let star = if r.iter().all(|x| x.1) { "" } else { "*" };
                    if r.len() == 1 {
                        return format!("{mean:.0}{star}");
                    }
                    let lo = r.iter().map(|x| x.0).fold(f64::MAX, f64::min);
                    let hi = r.iter().map(|x| x.0).fold(0.0, f64::max);
                    format!("{mean:.0}{star} ({lo:.0}–{hi:.0})")
                })
                .collect()
        })
        .collect();
    println!("| SNR dB | AWGN | CCIR good | CCIR moderate | CCIR poor |");
    println!("|---:|---:|---:|---:|---:|");
    for (snr, row) in snrs.iter().zip(rows) {
        println!("| {snr:+} | {} |", row.join(" | "));
    }
    println!("(* = {bytes} bytes not delivered within 30 min; bit/s so far)");
    println!();
    println!("| SNR dB | failed bursts: AWGN | CCIR good | CCIR moderate | CCIR poor |");
    println!("|---:|---:|---:|---:|---:|");
    for (snr, row) in snrs.iter().zip(fails.into_inner().unwrap()) {
        let cells: Vec<String> = row
            .iter()
            .map(|r| {
                let f: u32 = r.iter().map(|x| x.0).sum();
                let d: u32 = r.iter().map(|x| x.1).sum();
                format!("{f}/{d}")
            })
            .collect();
        println!("| {snr:+} | {} |", cells.join(" | "));
    }
    println!("(data bursts of which nothing got through / data bursts, summed over seeds)");
    for (snr, a) in snrs.iter().zip(air.into_inner().unwrap()) {
        println!("\nAir time at {snr:+} dB, all channels and seeds, by the level of each cycle's data burst:\n");
        println!("{}", a.table());
    }
}

/// Bad and very bad HF: does the link get through at all, and how fast?
#[test]
#[ignore]
fn horrid_table() {
    let profiles = [
        (Profile::Awgn, "AWGN"),
        (Profile::CcirPoor, "CCIR poor 2 ms/1 Hz"),
        (Profile::CcirFlutter, "flutter 0.5 ms/10 Hz"),
        (Profile::LowDisturbed, "low-lat disturbed 6 ms/10 Hz"),
        (Profile::MidDisturbedNvis, "NVIS disturbed 7 ms/1 Hz"),
        (Profile::HighDisturbed, "high-lat disturbed 7 ms/30 Hz"),
    ];
    // ION_SNRS: a subset of the columns (default −14..+10 in 2 dB).
    let snrs: Vec<i32> = std::env::var("ION_SNRS")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| (-14..=10).step_by(2).collect());
    // ION_SEED: another set of channel seeds.
    let seed: u64 = std::env::var("ION_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    // ION_SNR_MIN / ION_SNR_MAX and ION_PROFILES (indices into
    // the list above) run part of the table; every cell keeps its seed.
    let env_i32 = |k: &str, d: i32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (lo, hi) = (env_i32("ION_SNR_MIN", -14), env_i32("ION_SNR_MAX", 10));
    let sel: Option<Vec<usize>> = std::env::var("ION_PROFILES")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect());
    // (profile, SNR index, the cell's index in the full table: its seed).
    let jobs: Vec<(usize, usize, usize)> = (0..profiles.len())
        .flat_map(|p| (0..snrs.len()).map(move |s| (p, s)))
        .enumerate()
        .map(|(i, (p, s))| (p, s, i))
        .filter(|&(p, s, _)| (lo..=hi).contains(&snrs[s]) && sel.as_ref().is_none_or(|v| v.contains(&p)))
        .collect();
    let cells = Mutex::new(vec![vec![String::new(); snrs.len()]; profiles.len()]);
    // Failed data bursts (nothing got through) / data bursts, per cell.
    let failed = Mutex::new(vec![vec![String::new(); snrs.len()]; profiles.len()]);
    let air = Mutex::new(vec![Airtime::default(); profiles.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| {
                loop {
                    let j = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(p, k, i)) = jobs.get(j) else { break };
                    let snr = snrs[k];
                    let mut sim = audio_pair(profiles[p].0, snr as f32, 900 + i as u64 + 1000 * seed);
                    let n = 3_000;
                    let cell = match transfer(&mut sim, n, 900.0) {
                        Some(t) => format!("{:.0}", n as f64 * 8.0 / t),
                        None => {
                            let got = sim.stations[1].stats().bytes_received;
                            if got == 0 {
                                "—".to_string()
                            } else {
                                format!("{:.0}*", got as f64 * 8.0 / sim.t)
                            }
                        }
                    };
                    let st = sim.stations[0].stats();
                    let fails = format!("{}/{}", st.failed_bursts, st.data_bursts);
                    eprintln!(
                        "{} {snr:+}: {cell} bit/s, failed {fails} ({} OFDM)",
                        profiles[p].1, st.failed_ofdm
                    );
                    cells.lock().unwrap()[p][k] = cell;
                    failed.lock().unwrap()[p][k] = fails;
                    air.lock().unwrap()[p].add(&sim.airtime());
                }
            });
        }
    });
    let cells = cells.into_inner().unwrap();
    println!(
        "| Channel | {} |",
        snrs.iter().map(|s| format!("{s:+} dB")).collect::<Vec<_>>().join(" | ")
    );
    println!("|---|{}", "---:|".repeat(snrs.len()));
    for (p, row) in cells.iter().enumerate() {
        if sel.as_ref().is_some_and(|v| !v.contains(&p)) {
            continue;
        }
        println!("| {} | {} |", profiles[p].1, row.join(" | "));
    }
    println!("(bit/s for 3 kB incl. connect; * = unfinished after 15 min, rate so far; — = nothing got through)");
    let failed = failed.into_inner().unwrap();
    println!();
    println!(
        "| Failed bursts | {} |",
        snrs.iter().map(|s| format!("{s:+} dB")).collect::<Vec<_>>().join(" | ")
    );
    println!("|---|{}", "---:|".repeat(snrs.len()));
    for (p, row) in failed.iter().enumerate() {
        println!("| {} | {} |", profiles[p].1, row.join(" | "));
    }
    println!("(data bursts of which nothing got through / data bursts sent)");
    for (p, a) in air.into_inner().unwrap().iter().enumerate() {
        println!("\nAir time on {}, all SNRs:\n", profiles[p].1);
        println!("{}", a.table());
    }
}

/// One transfer burst by burst (`ION_PROFILE` = awgn / poor / flutter /
/// nvis / low / high, `ION_SNR`, `ION_SEED`, `ION_BYTES`): each
/// data burst's level, frames, whether it got through, the channel class
/// and target the sender had, and what the receiver measured.
#[test]
#[ignore]
fn diag_link_trace() {
    let profile = match std::env::var("ION_PROFILE").as_deref() {
        Ok("poor") => Profile::CcirPoor,
        Ok("good") => Profile::CcirGood,
        Ok("flutter") => Profile::CcirFlutter,
        Ok("nvis") => Profile::MidDisturbedNvis,
        Ok("low") => Profile::LowDisturbed,
        Ok("high") => Profile::HighDisturbed,
        _ => Profile::Awgn,
    };
    let snr: f32 = std::env::var("ION_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6.0);
    let seed: u64 = std::env::var("ION_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let n: usize = std::env::var("ION_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);
    let mut sim = audio_pair(profile, snr, seed);
    let t = transfer(&mut sim, n, 1800.0);
    println!("{profile:?} {snr:+} dB seed {seed}: {n} bytes in {t:?} s");
    let a = sim.stations[0].stats();
    let b = sim.stations[1].stats();
    let data: Vec<_> = sim
        .log
        .iter()
        .filter(|e| e.from == 0 && e.code < LADDER.len())
        .collect();
    for (i, l) in a.levels.iter().enumerate() {
        let (c, target) = a.classes[i];
        let ev = data.get(i);
        println!(
            "burst {i:2}: L{l:<2} target {target:2} class echo {} dopp {} | {}",
            c.echo as u8,
            c.doppler as u8,
            ev.map_or("?".into(), |e| format!(
                "t {:6.1} frames {} heard {}",
                e.start, e.frames, e.delivered
            ))
        );
    }
    for (l, m, c) in &b.heard {
        println!(
            "heard L{l:<2}: delay {:5.1} ms doppler {:5.1} Hz eff {:+5.1} mean {:+5.1} -> echo {} dopp {}",
            m[0], m[1], m[2], m[3], c.echo as u8, c.doppler as u8
        );
    }
    println!("failed {}/{}", a.failed_bursts, a.data_bursts);
    if std::env::var("ION_LOG").is_ok() {
        for e in &sim.log {
            println!(
                "  {:6.1}-{:6.1} from {} {:?} code {} frames {} delivered {}",
                e.start, e.end, e.from, e.wave, e.code, e.frames, e.delivered
            );
        }
    }
    println!(
        "A: frames sent {} resent {} increments {}; B: codewords failed {} HARQ recovered {}",
        a.frames_sent, a.frames_resent, a.increments_sent, b.codewords_failed, b.harq_recovered
    );
}

/// The levels one transfer climbs through (`ION_SNR`, default 30 dB, AWGN).
#[test]
#[ignore]
fn diag_link_levels() {
    let snr: f32 = std::env::var("ION_SNR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30.0);
    let mut sim = audio_pair(Profile::Awgn, snr, 77);
    let n: usize = std::env::var("ION_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let t = transfer(&mut sim, n, 1800.0);
    let st = sim.stations[0].stats();
    println!("{n} bytes: {t:?} s, levels {:?}", st.levels);
    println!("B: {:?}", sim.stations[1].stats());
}

// -- compression ----------------------------------------------------------------

/// Held-out chat and net text (`crates/ion/data`), whole lines up to
/// about `n` bytes, one `send` per line as typed.
fn text_lines(n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut total = 0;
    for l in include_str!("../data/chat_heldout.txt").lines() {
        let l = l.trim_end_matches('\r');
        if l.is_empty() {
            continue;
        }
        if total >= n {
            break;
        }
        let line = format!("{l}\n").into_bytes();
        total += line.len();
        out.push(line);
    }
    out
}

/// Connects A to B and sends `lines` A → B; the seconds until B has all
/// of the text back (decompressed), or None.
fn transfer_text(sim: &mut Sim, lines: &[Vec<u8>], t_max: f64) -> Option<f64> {
    let want = lines.concat();
    sim.stations[0].connect("W1AW", 0.0);
    for l in lines {
        sim.stations[0].send(l);
    }
    let n = want.len() as u64;
    if !sim.run_until(t_max, |s| s.stations[1].stats().text_received >= n) {
        return None;
    }
    assert_eq!(sim.stations[1].take_received(), want, "delivered text differs");
    Some(sim.t)
}

/// Delivery time of ~2 kB of held-out chat/net text, uncompressed and
/// compressed, over the audio link on the same channel seeds.
///
/// `ION_SNRS` (default `-6,-2,2`), `ION_SEEDS` (3),
/// `ION_PROFILES` (`awgn,moderate`; also `good`, `poor`),
/// `ION_TEXT_BYTES` (2000), `ION_MAX_LEVEL` (cap the ladder, e.g. 3
/// to stay on MFSK and the slices).
#[test]
#[ignore]
fn compression_table() {
    let env = |k: &str| std::env::var(k).ok();
    let snrs: Vec<i32> = env("ION_SNRS")
        .unwrap_or("-6,-2,2".into())
        .split(',')
        .map(|s| s.parse().unwrap())
        .collect();
    let seeds: u64 = env("ION_SEEDS").and_then(|v| v.parse().ok()).unwrap_or(3);
    let bytes: usize = env("ION_TEXT_BYTES").and_then(|v| v.parse().ok()).unwrap_or(2000);
    let max_level: Option<usize> = env("ION_MAX_LEVEL").and_then(|v| v.parse().ok());
    let profiles: Vec<(Profile, &str)> = env("ION_PROFILES")
        .unwrap_or("awgn,moderate".into())
        .split(',')
        .map(|p| match p {
            "awgn" => (Profile::Awgn, "AWGN"),
            "good" => (Profile::CcirGood, "CCIR good"),
            "moderate" => (Profile::CcirModerate, "CCIR moderate"),
            "poor" => (Profile::CcirPoor, "CCIR poor"),
            x => panic!("profile {x}"),
        })
        .collect();
    let lines = text_lines(bytes);
    let text: usize = lines.iter().map(Vec::len).sum();
    // (profile, snr, seed, compress)
    let jobs: Vec<(usize, usize, u64, bool)> = (0..profiles.len())
        .flat_map(|p| {
            (0..snrs.len()).flat_map(move |s| (0..seeds).flat_map(move |k| [(p, s, k, false), (p, s, k, true)]))
        })
        .collect();
    // Per job: (seconds or None, stream bytes, data bursts per waveform: MFSK, slice, long, normal).
    let results = Mutex::new(vec![None; jobs.len()]);
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads() {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&(p, k, seed, on)) = jobs.get(i) else { break };
                let snr = snrs[k];
                let mut sim = audio_pair_with(profiles[p].0, snr as f32, 700 + (snr + 50) as u64 + 1000 * seed, |c| {
                    c.compress = on;
                    if let Some(m) = max_level {
                        c.max_level = m;
                    }
                });
                let t = transfer_text(&mut sim, &lines, 1800.0);
                let mut waves = [0u32; 4];
                for &l in &sim.stations[0].stats().levels {
                    waves[match LADDER[l].wave {
                        Wave::Mfsk(_) | Wave::Lifeline => 0,
                        Wave::Ofdm(Guard::Slice) => 1,
                        Wave::Ofdm(Guard::Long) => 2,
                        Wave::Ofdm(_) => 3,
                    }] += 1;
                }
                let stream = sim.stations[1].stats().bytes_received;
                eprintln!(
                    "{} {snr:+} seed {seed} compress {on}: {t:?} s, {stream} stream bytes, bursts M/S/L/N {waves:?}",
                    profiles[p].1
                );
                results.lock().unwrap()[i] = Some((t, stream, waves));
            });
        }
    });
    let results: Vec<_> = results.into_inner().unwrap().into_iter().map(Option::unwrap).collect();
    println!(
        "{text} bytes of held-out text in {} lines{}",
        lines.len(),
        max_level.map_or(String::new(), |m| format!(", ladder capped at level {m}"))
    );
    println!(
        "| Channel | SNR dB | Plain: s (each seed) | Compressed: s (each seed) | Time saved | Stream bytes | Data bursts M/S/L/N plain → compressed |"
    );
    println!("|---|---:|---|---|---:|---:|---|");
    for (p, (_, pname)) in profiles.iter().enumerate() {
        for (k, snr) in snrs.iter().enumerate() {
            let cell = |on: bool| {
                jobs.iter()
                    .zip(&results)
                    .filter(|((jp, jk, _, jon), _)| *jp == p && *jk == k && *jon == on)
                    .map(|(_, r)| r)
                    .collect::<Vec<_>>()
            };
            let (off, on) = (cell(false), cell(true));
            let times = |c: &[&(Option<f64>, u64, [u32; 4])]| {
                c.iter()
                    .map(|r| r.0.map_or("—".to_string(), |t| format!("{t:.0}")))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let mean = |c: &[&(Option<f64>, u64, [u32; 4])]| {
                let v: Vec<f64> = c.iter().filter_map(|r| r.0).collect();
                (v.len() == c.len()).then(|| v.iter().sum::<f64>() / v.len() as f64)
            };
            let saved = match (mean(&off), mean(&on)) {
                (Some(a), Some(b)) => format!("{:.0}%", 100.0 * (1.0 - b / a)),
                _ => "—".into(),
            };
            let bursts = |c: &[&(Option<f64>, u64, [u32; 4])]| {
                let mut w = [0u32; 4];
                for r in c {
                    for (wi, x) in w.iter_mut().zip(r.2) {
                        *wi += x;
                    }
                }
                w.map(|x| x.to_string()).join("/")
            };
            let stream = on.iter().map(|r| r.1).max().unwrap_or(0);
            println!(
                "| {pname} | {snr:+} | {} | {} | {saved} | {} → {stream} | {} → {} |",
                times(&off),
                times(&on),
                off.iter().map(|r| r.1).max().unwrap_or(0),
                bursts(&off),
                bursts(&on)
            );
        }
    }
    println!("(— = not delivered within 30 min of simulated time)");
}

/// One transfer, in detail: `ION_PROFILE` (awgn, good, moderate, poor,
/// flutter, lowlat, nvis, highlat; default poor), `ION_SNR` (default
/// 4), `ION_SEED` (the channel seed itself, default 1), `ION_BYTES`
/// (default 3000). Prints every burst, the levels and the air time. The
/// overhead knobs apply.
#[test]
#[ignore]
fn diag_transfer() {
    let env = |k: &str| std::env::var(k).ok();
    let profile = match env("ION_PROFILE").as_deref() {
        Some("awgn") => Profile::Awgn,
        Some("good") => Profile::CcirGood,
        Some("moderate") => Profile::CcirModerate,
        Some("flutter") => Profile::CcirFlutter,
        Some("lowlat") => Profile::LowDisturbed,
        Some("nvis") => Profile::MidDisturbedNvis,
        Some("highlat") => Profile::HighDisturbed,
        _ => Profile::CcirPoor,
    };
    let snr: f32 = env("ION_SNR").and_then(|v| v.parse().ok()).unwrap_or(4.0);
    let seed: u64 = env("ION_SEED").and_then(|v| v.parse().ok()).unwrap_or(1);
    let n: usize = env("ION_BYTES").and_then(|v| v.parse().ok()).unwrap_or(3_000);
    let mut sim = audio_pair(profile, snr, seed);
    let t = transfer(&mut sim, n, 1800.0);
    for e in &sim.log {
        println!(
            "{:8.2} {:8.2} from {} {:?} code {} frames {} heard {}",
            e.start, e.end, e.from, e.wave, e.code, e.frames, e.delivered
        );
    }
    println!("{n} bytes: {t:?} s, levels {:?}", sim.stations[0].stats().levels);
    println!("{}", sim.airtime().table());
}

/// Air-time arithmetic of one round trip per level, from the PHYs: a
/// data burst of as many frames as fit, two turnarounds and the ACK it
/// gets, with no losses; at `max_burst_s` and, with `burst_adapt`, at the
/// length it reaches on a clean channel ("clean"). Instant. The overhead
/// knobs (`overhead_knobs`) apply.
#[test]
#[ignore]
fn cycle_table() {
    use ion::link::frame::{Frame, TINY_ACK_FRAMES};
    use ion::link::station::frame_bits;
    let air: Arc<dyn Air> = Arc::new(PhyAir);
    let mut cfg = audio::config("N0ABC");
    overhead_knobs(&mut cfg);
    let st = Station::new(cfg.clone(), air.clone());
    let turn = 2.0 * cfg.turnaround_s;
    println!(
        "| Level | burst s | frames | preamble+header s | ACK s | cycle s | data | preamble | ACK | turnarounds | payload bit/s |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (l, lv) in LADDER.iter().enumerate() {
        let w = lv.wave;
        let slot = lv.coded_bits();
        let short = lv.frame == ion::level::FrameSize::Short;
        let cap = if short { TINY_ACK_FRAMES } else { 64 };
        let lens = if cfg.burst_adapt {
            vec![cfg.max_burst_s, st.clean_burst_s(l)]
        } else {
            vec![cfg.max_burst_s]
        };
        for (k, max_s) in lens.into_iter().enumerate() {
            let mut n = 0;
            while n < cap && air.duration(w, l, (n + 1) * slot) <= max_s {
                n += 1;
            }
            let n = n.max(1);
            let d = air.duration(w, l, n * slot);
            let pre = air.duration(w, l, 1);
            let tiny_ack = n <= TINY_ACK_FRAMES || cfg.tiny_ack_all;
            let ack = if tiny_ack {
                air.duration(w, TINY, 0)
            } else {
                air.duration(w, CONTROL, frame_bits(CONTROL, 1))
            };
            let cycle = d + ack + turn;
            let payload = n * Frame::max_payload(lv.frame_bytes());
            let pc = |x: f64| format!("{:.0}%", 100.0 * x / cycle);
            println!(
                "| {l} {}{} | {d:.2} | {n} | {pre:.2} | {ack:.2} | {cycle:.2} | {} | {} | {} | {} | {:.0} |",
                level_name(l),
                if k == 1 { " (clean)" } else { "" },
                pc(d - pre),
                pc(pre),
                pc(ack),
                pc(turn),
                payload as f64 * 8.0 / cycle
            );
        }
    }
}

/// On-air mean (against a steady carrier at `TX_LEVEL`) and peak (against
/// `TX_LEVEL`) of level `l`, dB, over an ~8 s burst (preamble, header,
/// data), with the link's OFDM air gain (`transmit_with`).
/// Averaged over 8 payloads: one burst's mean moves ±0.3 dB with its data
/// (the low-PAPR clipping).
fn air_power_db(l: usize) -> (f32, f32, f32) {
    let runs: Vec<(f32, f32, f32)> = (1..=8).map(|s| air_power_db_seed(l, s)).collect();
    let mean = 10.0 * (runs.iter().map(|r| 10f32.powf(r.0 / 10.0)).sum::<f32>() / runs.len() as f32).log10();
    let peak = runs.iter().fold(f32::NEG_INFINITY, |m, r| m.max(r.1));
    (mean, peak, runs[0].2)
}

fn air_power_db_seed(l: usize, seed: u32) -> (f32, f32, f32) {
    let lv = &LADDER[l];
    let carrier = ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0;
    let mut f = ion::link::frame::Frame::new(ion::link::frame::Kind::Data, 9);
    f.payload = payload(lv.frame_bytes().saturating_sub(16), seed);
    let burst = |n: usize| Outgoing {
        td_ease: false,
        cue: false,
        wave: lv.wave,
        code: l,
        user: user_bits(l, 9, 1),
        extra: 0,
        frames: vec![f.encode(lv.frame_bytes()); n],
        spans: Vec::new(),
    };
    let one = audio::transmit(&burst(1)).len() as f32 / AUDIO_RATE;
    let a = audio::transmit_with(
        &burst(((8.0 / one).floor() as usize).max(1)),
        audio::ofdm_gain(audio::OFDM_AIR_GAIN_DB),
    );
    let mean = a.iter().map(|x| x * x).sum::<f32>() / a.len() as f32;
    let peak = a.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    (
        10.0 * (mean / carrier).log10(),
        20.0 * (peak / ion_dsp::TX_LEVEL).log10(),
        a.len() as f32 / AUDIO_RATE,
    )
}

/// Every OFDM level at the drive full-width OFDM uses: the slice levels
/// average within 0.3 dB of full-width QPSK, and no OFDM burst peaks more
/// than the 3 dB cap over the steady carriers.
#[test]
fn slice_levels_average_full_ofdm_power() {
    let (full, ..) = air_power_db(LADDER.iter().position(|l| l.wave == Wave::Ofdm(Guard::Normal)).unwrap());
    for (l, lv) in LADDER.iter().enumerate() {
        let (mean, peak, _) = air_power_db(l);
        if lv.wave == Wave::Ofdm(Guard::Slice) {
            assert!(
                (mean - full).abs() < 0.3,
                "L{l}: {mean:+.2} dB against full width {full:+.2}"
            );
        }
        // The gear shift's table of the same (`level::AIR_MEAN_DB`).
        let table = if phy::td_clip_on() {
            ion::level::AIR_MEAN_TD_DB[l]
        } else {
            ion::level::AIR_MEAN_DB[l]
        };
        assert!(
            (mean - table).abs() < 0.3,
            "L{l}: {mean:+.2} dB on the air, {table:+.1} in AIR_MEAN_DB"
        );
        if matches!(lv.wave, Wave::Ofdm(_)) {
            assert!(peak <= audio::OFDM_AIR_GAIN_MAX_DB + 0.01, "L{l} peaks {peak:+.2} dB");
        } else {
            assert!(
                peak.abs() < 0.01 && mean.abs() < 0.01,
                "MFSK L{l} changed: {mean:+.2} / {peak:+.2}"
            );
        }
    }
}

/// On-air power of every level (`air_power_db`).
#[test]
#[ignore]
fn air_power_table() {
    println!("| Level | mean vs carrier | peak vs TX_LEVEL | burst s |");
    println!("|---|---:|---:|---:|");
    for l in 0..LADDER.len() {
        let (mean, peak, secs) = air_power_db(l);
        println!(
            "| L{l} {} | {mean:+.2} dB | {peak:+.2} dB | {secs:.1} |",
            ion::link::sim::level_name(l)
        );
    }
}

/// Occupied bandwidth of every level as transmitted (`transmit_with`, ~8 s
/// bursts, preamble and header included): the 99% power bandwidth and the
/// −26 dB bandwidth (the outermost frequencies within 26 dB of the
/// spectrum's peak), from a Welch PSD in 10 Hz bins. Against the US HF
/// data limit (2.8 kHz) and the 500 Hz limit for automatically controlled
/// stations outside their sub-bands (§97.221(c)).
#[test]
#[ignore]
fn occupied_bandwidth_table() {
    use rustfft::FftPlanner;
    use rustfft::num_complex::Complex32;
    let n = 4800; // 10 Hz bins at 48 kHz
    let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())
        .collect();
    println!(
        "Width: {} (center {} Hz)",
        if audio::WIDE { "2.75 kHz" } else { "2.45 kHz" },
        audio::CENTER_HZ
    );
    println!("| Level | 99% band | 99% width | §97.3(a)(8) band | §97.3(a)(8) width | −26 dB-from-peak width |");
    println!("|---|---|---:|---|---:|---:|");
    for (l, lv) in LADDER.iter().enumerate() {
        let mut f = ion::link::frame::Frame::new(ion::link::frame::Kind::Data, 9);
        f.payload = payload(lv.frame_bytes().saturating_sub(16), l as u32 + 1);
        let burst = |k: usize| Outgoing {
            td_ease: false,
            cue: false,
            wave: lv.wave,
            code: l,
            user: user_bits(l, 9, 1),
            extra: 0,
            frames: vec![f.encode(lv.frame_bytes()); k],
            spans: Vec::new(),
        };
        let one = audio::transmit(&burst(1)).len() as f32 / AUDIO_RATE;
        let a = audio::transmit_with(
            &burst(((8.0 / one).floor() as usize).max(1)),
            audio::ofdm_gain(audio::OFDM_AIR_GAIN_DB),
        );
        // Welch: Hann windows, half overlap.
        let mut psd = vec![0f64; n / 2];
        let mut buf = vec![Complex32::new(0.0, 0.0); n];
        for seg in (0..a.len().saturating_sub(n)).step_by(n / 2) {
            for (b, (&x, &w)) in buf.iter_mut().zip(a[seg..seg + n].iter().zip(&win)) {
                *b = Complex32::new(x * w, 0.0);
            }
            fft.process(&mut buf);
            for (p, b) in psd.iter_mut().zip(&buf) {
                *p += b.norm_sqr() as f64;
            }
        }
        let hz = |k: usize| k as f64 * 48_000.0 / n as f64;
        let total: f64 = psd.iter().sum();
        let mut acc = 0.0;
        let (mut lo99, mut hi99) = (0, 0);
        for (k, &p) in psd.iter().enumerate() {
            let before = acc;
            acc += p;
            if before < 0.005 * total && acc >= 0.005 * total {
                lo99 = k;
            }
            if before < 0.995 * total && acc >= 0.995 * total {
                hi99 = k;
            }
        }
        // §97.3(a)(8): the band outside which the mean power is at least
        // 26 dB below that inside it: the smallest band holding
        // 1/(1 + 10^−2.6) of the power, split evenly between the two tails.
        let tail = (1.0 - 1.0 / (1.0 + 10f64.powf(-2.6))) / 2.0;
        let mut acc2 = 0.0;
        let (mut lo_f, mut hi_f) = (0, psd.len() - 1);
        for (k, &p) in psd.iter().enumerate() {
            let before = acc2;
            acc2 += p;
            if before < tail * total && acc2 >= tail * total {
                lo_f = k;
            }
            if before < (1.0 - tail) * total && acc2 >= (1.0 - tail) * total {
                hi_f = k;
            }
        }
        let peak = psd.iter().cloned().fold(0.0, f64::max);
        let floor = peak * 10f64.powf(-26.0 / 10.0);
        let lo26 = psd.iter().position(|&p| p >= floor).unwrap_or(0);
        let hi26 = psd.len() - 1 - psd.iter().rev().position(|&p| p >= floor).unwrap_or(0);
        println!(
            "| L{l} {} | {:.0}–{:.0} Hz | {:.0} Hz | {:.0}–{:.0} Hz | {:.0} Hz | {:.0} Hz |",
            ion::link::sim::level_name(l),
            hz(lo99),
            hz(hi99),
            hz(hi99) - hz(lo99),
            hz(lo_f),
            hz(hi_f),
            hz(hi_f) - hz(lo_f),
            hz(hi26) - hz(lo26)
        );
    }
}

/// Static side of [`td_clip_table`]: per level and `ION_TD` config, the
/// mean power under the peak (dB against a tone at `TX_LEVEL`) and the
/// signal-to-distortion ratio against the same burst sent unclipped
/// (least-squares gain, the preamble included).
#[test]
#[ignore]
fn td_clip_papr() {
    let configs: Vec<String> = std::env::var("ION_TD")
        .unwrap_or_else(|_| "link".into())
        .split(';')
        .map(str::to_owned)
        .collect();
    let levels: Vec<usize> = std::env::var("ION_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| {
            (0..LADDER.len())
                .filter(|&l| matches!(LADDER[l].wave, Wave::Ofdm(_)))
                .collect()
        });
    println!("| Level | Config | Mean vs peak (dB) | SDR (dB) |");
    println!("|---|---|---:|---:|");
    for &l in &levels {
        let lv = LADDER[l];
        let Wave::Ofdm(g) = lv.wave else { continue };
        let num = audio::numerology(g);
        let bits: Vec<u8> = payload(phy::capacity_bits_with(num, lv.pilots, lv.constellation, 40) / 8, 9)
            .iter()
            .flat_map(|b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
        let send = |opt: &TxOptions| phy::modulate_full(num, lv.constellation, 0, 0, &bits, num.center_hz(), opt);
        for cfg in &configs {
            let opt = td_opt(cfg, l);
            let a = send(&TxOptions {
                clip_db: None,
                td_clip: None,
                ..opt
            });
            let b = send(&opt);
            if let Ok(dir) = std::env::var("ION_DUMP") {
                let bytes: Vec<u8> = b.iter().flat_map(|x| x.to_le_bytes()).collect();
                std::fs::write(format!("{dir}/{l}_{}.f32", cfg.replace(['/', ':'], "_")), bytes).unwrap();
            }
            let mean = b.iter().map(|x| x * x).sum::<f32>() / b.len() as f32;
            let pk = ion_dsp::TX_LEVEL * ion_dsp::TX_LEVEL / 2.0;
            let ab = a.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>();
            let aa = a.iter().map(|x| x * x).sum::<f32>();
            let k = ab / aa;
            let e = a.iter().zip(&b).map(|(x, y)| (y - k * x).powi(2)).sum::<f32>();
            println!(
                "| {l} {:?} {} | {cfg} | {:+.2} | {:.1} |",
                lv.constellation,
                rate_name(lv.rate),
                10.0 * (mean / pk).log10(),
                10.0 * (k * k * aa / e).log10()
            );
        }
    }
}

/// The whole-burst clipper raises mean power under the same peak, and its
/// bursts still decode clean.
#[test]
fn td_clip_raises_mean_power_and_decodes() {
    let l = 10;
    let set = Setup {
        num: audio::numerology(Guard::Normal),
        pilots: LADDER[l].pilots,
        clip: None,
        pep: true,
        spread: false,
        gain: 0.0,
        hdr_clip: None,
        low_papr: false,
        ladder_papr: false,
    };
    let mean = |opt: &TxOptions| {
        let a = phy::modulate_full(
            set.num,
            LADDER[l].constellation,
            0,
            0,
            &[1; 4000],
            set.num.center_hz(),
            opt,
        );
        a.iter().map(|x| x * x).sum::<f32>() / a.len() as f32
    };
    let td = td_opt("none:1:0:-2:1/2/3", l);
    assert!(mean(&td) > 1.2 * mean(&audio::tx_options(l)));
    let lv = LADDER[l];
    assert_eq!(
        cer_with(&set, &td, lv.constellation, lv.rate, Profile::Awgn, 15.0, 2, 1),
        0.0
    );
}

/// In-band distortion each OFDM level's link options leave on the data
/// cells, as the receiver's SNR estimate sees it: AWGN at 60 dB against the
/// burst's mean, 4 bursts. Run with `ION_RX_DIST=none` so the receiver
/// takes none out; `ION_TDCLIP=1` for the whole-burst clipper's picks.
#[test]
#[ignore]
fn td_clip_distortion() {
    for (l, lv) in LADDER.iter().enumerate() {
        let Wave::Ofdm(g) = lv.wave else { continue };
        let num = audio::numerology(g);
        let center = num.center_hz();
        let opt = audio::tx_options(l);
        let mut rs = Vec::new();
        for seed in 0..4u64 {
            let bits: Vec<u8> = payload(
                phy::capacity_bits_with(num, lv.pilots, lv.constellation, 40) / 8,
                70 + seed as u32,
            )
            .iter()
            .flat_map(|b| (0..8).map(move |i| (b >> i) & 1))
            .collect();
            let tx = phy::modulate_full(num, lv.constellation, 0, 0, &bits, center, &opt);
            let power = tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32;
            let mut out = vec![0.0; 4800];
            out.extend(&tx);
            out.resize(out.len() + 4800, 0.0);
            let mut noise = NoiseGen::new(seed ^ 91);
            add_awgn(&mut out, noise_sigma(power, 60.0, 2400.0, AUDIO_RATE), &mut noise);
            let mut rx = PhyRx::with_numerology(num, center);
            let mut found = Vec::new();
            rx.process(&out, &mut found);
            if let Some(b) = found.first() {
                rs.push(b.snr_db);
            }
        }
        let r = rs.iter().sum::<f32>() / rs.len().max(1) as f32;
        let d = -10.0 * (10f32.powf(-r / 10.0) - 10f32.powf(-3.4)).max(1e-6).log10();
        println!(
            "| {l} {:?} {} | {:?} | {} | {r:.1} | {d:.1} |",
            lv.constellation,
            rate_name(lv.rate),
            g,
            rs.len()
        );
    }
}

/// The channel report's delay and Doppler spread under the link's options
/// (`ION_TDCLIP=1`: the whole-burst clipper's): 12 bursts per point,
/// SNR against each burst's mean.
#[test]
#[ignore]
fn td_clip_chan() {
    println!("| Level | Channel | SNR | heard | delay ms | Doppler Hz | echo | doppler |");
    for l in std::env::var("ION_LEVELS")
        .ok()
        .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect::<Vec<usize>>())
        .unwrap_or(vec![9, 10, 11, 13, 14])
    {
        let lv = LADDER[l];
        let Wave::Ofdm(g) = lv.wave else { continue };
        let num = audio::numerology(g);
        let center = num.center_hz();
        let opt = audio::tx_options(l);
        for profile in [Profile::Awgn, Profile::CcirPoor, Profile::MidDisturbedNvis] {
            for snr in [6.0f32, 12.0, 25.0] {
                let mut ch = Channel::from_profile(profile, 5);
                let mut noise = NoiseGen::new(17);
                let mut rx = PhyRx::with_numerology(num, center);
                let (mut d, mut f, mut n, mut echo, mut dop) = (Vec::new(), Vec::new(), 0, 0, 0);
                let (mut ex, mut sp) = (Vec::new(), Vec::new());
                for b in 0..12u32 {
                    let bits: Vec<u8> = payload(
                        phy::capacity_bits_with(num, lv.pilots, lv.constellation, 40) / 8,
                        300 + b,
                    )
                    .iter()
                    .flat_map(|x| (0..8).map(move |i| (x >> i) & 1))
                    .collect();
                    let tx = phy::modulate_full(num, lv.constellation, 0, 0, &bits, center, &opt);
                    let power = tx.iter().map(|x| x * x).sum::<f32>() / tx.len() as f32;
                    let mut input = vec![0.0; 4800];
                    input.extend(&tx);
                    input.resize(input.len() + ch.latency() + 4800, 0.0);
                    let mut out = vec![0.0; input.len()];
                    ch.process(&input, &mut out);
                    add_awgn(&mut out, noise_sigma(power, snr, 2400.0, AUDIO_RATE), &mut noise);
                    let mut found = Vec::new();
                    rx.process(&out, &mut found);
                    if let Some(r) = found.first() {
                        n += 1;
                        sp.push(r.chan.raw[2]);
                        ex.push(r.chan.raw[3]);
                        if r.chan.delay_ms.is_finite() {
                            d.push(r.chan.delay_ms);
                            echo += (r.chan.delay_ms > ion::chan::ECHO_MS) as usize;
                        }
                        if r.chan.doppler_hz.is_finite() {
                            f.push(r.chan.doppler_hz);
                            dop += (r.chan.doppler_hz > ion::chan::DOPPLER_HZ) as usize;
                        }
                    }
                }
                let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
                println!(
                    "| {l} | {profile:?} | {snr} | {n} | {:.2} | {:.2} | {echo} | {dop} | span {:.2} excess {:.1} |",
                    mean(&d),
                    mean(&f),
                    mean(&sp),
                    mean(&ex)
                );
            }
        }
    }
}
