//! [`NetStation`] on audio.

use super::*;
use crate::link::audio;
use crate::{phy_chorus, phy_mfsk};

const FS: f64 = 48_000.0;

/// A net station driven by audio: [`NetStation`] plus every receiver, as
/// [`crate::link::Node`] is for a link.
pub struct NetNode {
    ofdm_gain: f32,
    station: NetStation,
    rx: audio::Receivers,
    bursts: Vec<audio::Heard>,
    lead: usize,
    on_air: bool,
    /// Audio samples given to the receivers so far (none while we
    /// transmit): their sample count, against which bursts are placed.
    fed: u64,
    /// NCS: the chorus slots being recorded (plan, our clock at the first
    /// sample, the audio), and the last plan received.
    chorus_rec: Option<(ChorusPlan, f64, Vec<f32>)>,
    chorus_done: Option<f64>,
}

impl NetNode {
    pub fn new(cfg: NetConfig, lead_s: f64) -> Self {
        let air: Arc<dyn Air> = Arc::new(crate::link::node::NodeAir { lead_s });
        let mut station = NetStation::new(cfg, air);
        station.set_tx_lead(lead_s);
        Self {
            ofdm_gain: 1.0,
            station,
            rx: audio::Receivers::without_lifeline(),
            bursts: Vec::new(),
            lead: (lead_s * FS) as usize,
            on_air: false,
            fed: 0,
            chorus_rec: None,
            chorus_done: None,
        }
    }

    #[doc(hidden)]
    pub fn mfsk_stats(&self) -> phy_mfsk::debug::RxStats {
        self.rx.mfsk_stats()
    }

    pub fn station(&self) -> &NetStation {
        &self.station
    }

    pub fn station_mut(&mut self) -> &mut NetStation {
        &mut self.station
    }

    /// Received 48 kHz audio ending at `now` (ignored while we transmit).
    pub fn push_audio(&mut self, samples: &[f32], now: f64) {
        if self.on_air {
            return;
        }
        // Our clock at the first of these samples, and its sample number.
        let t0 = now - samples.len() as f64 / FS;
        let at = |t: f64| (self.fed as f64 + (t - t0) * FS).round().max(0.0) as u64;
        let expect = self.station.expectation(now).map(|e| phy_mfsk::Expect {
            start_sample: at(e.start_s),
            tol_samples: (e.tol_s * FS) as u64,
            header: e.header,
            alt: e.alt,
            min_score: phy_mfsk::EXPECT_MIN_SCORE,
            min_margin: phy_mfsk::EXPECT_MIN_MARGIN,
        });
        self.rx.expect_mfsk(expect);
        self.bursts.clear();
        self.rx.process_heard(samples, &mut self.bursts);
        for mut h in std::mem::take(&mut self.bursts) {
            // A short frame that did not decode: again, with what NCS
            // knows of it.
            if let Some(b) = &h.mfsk
                && h.inc.frames.len() == 1
                && h.inc.frames[0].is_none()
            {
                for prior in self.station.priors_for(h.inc.user) {
                    let got = phy_mfsk::decode_short_frames_prior(b, &prior)
                        .into_iter()
                        .next()
                        .flatten();
                    let bytes = got.map(|bits| audio::bits_to_bytes(&bits));
                    if let Some(bytes) = bytes.filter(|x| Frame::decode(x).is_some()) {
                        self.station
                            .note_prior_decode(h.inc.user == checkin_user(self.station.id));
                        h.inc.frames[0] = Some(bytes);
                        break;
                    }
                }
            }
            let t = |s: u64| t0 + (s as f64 - self.fed as f64) / FS;
            let air = (t(h.start_sample), t(h.end_sample));
            self.station.on_rx_at(&h.inc, now, Some(air));
        }
        self.fed += samples.len() as u64;
        self.station.set_busy(self.rx.busy());
        self.record_chorus(samples, t0, now);
    }

    /// NCS: records its chorus slots, and receives them once they are over.
    fn record_chorus(&mut self, samples: &[f32], t0: f64, now: f64) {
        // Noise before the first slot (for telling an empty slot from a
        // busy one) and the slots themselves.
        const PRE_S: f64 = 0.3;
        // A slot is busy when its band power is this far above that noise
        // (about 1 dB).
        const BUSY_OVER_NOISE: f32 = 1.25;
        if self.chorus_rec.is_none()
            && let Some(p) = self.station.chorus_plan()
            && self.chorus_done != Some(p.first_s)
            && now >= p.first_s - p.tol_s - PRE_S
        {
            self.chorus_rec = Some((p, t0, Vec::new()));
        }
        let Some((p, r0, buf)) = self.chorus_rec.as_mut() else {
            return;
        };
        buf.extend_from_slice(samples);
        let end = p.first_s + p.count as f64 * p.len_s + p.tol_s;
        if now < end {
            return;
        }
        let (p, r0) = (*p, *r0);
        let buf = std::mem::take(buf);
        self.chorus_rec = None;
        self.chorus_done = Some(p.first_s);
        let center = audio::CENTER_HZ;
        let at = |t: f64| ((t - r0) * FS).round().max(0.0) as u64;
        let mut heard = Vec::new();
        let mut busy = false;
        let power = |a: &[f32]| phy_chorus::band_power(a, center);
        let noise = power(&buf[..(at(p.first_s - p.tol_s) as usize).min(buf.len())]);
        for j in 0..p.count {
            let start = p.first_s + j as f64 * p.len_s;
            heard.extend(phy_chorus::receive_in(
                p.fmt,
                &buf,
                at(start),
                (p.tol_s * FS) as u64,
                p.net,
                center,
            ));
            let a = (at(start) as usize).min(buf.len());
            let b = (at(start) as usize + p.fmt.burst_samples()).min(buf.len());
            busy |= noise > 0.0 && power(&buf[a..b]) > BUSY_OVER_NOISE * noise;
        }
        self.station.on_chorus(&heard, busy, now);
    }

    /// Audio to play now, if any; call [`NetNode::tx_done`] after its last
    /// sample.
    pub fn poll(&mut self, now: f64) -> Option<Vec<f32>> {
        if self.on_air {
            return None;
        }
        // A chorus check-in goes at its slot, busy channel or not: the
        // slot is everyone's.
        if let Some((sub, msg)) = self.station.poll_chorus(now) {
            let mut out = vec![0.0; self.lead];
            out.extend(phy_chorus::modulate_in(
                CHORUS_FMT,
                sub,
                &msg,
                self.station.id,
                audio::CENTER_HZ,
            ));
            self.on_air = true;
            return Some(out);
        }
        if self.rx.busy() {
            return None;
        }
        let b = self.station.poll_tx(now)?;
        let mut out = vec![0.0; self.lead];
        out.extend(audio::transmit_with(&b, self.ofdm_gain));
        self.on_air = true;
        Some(out)
    }

    pub fn tx_done(&mut self, now: f64) {
        if self.on_air {
            self.on_air = false;
            self.station.on_tx_done(now);
        }
    }

    pub fn on_air(&self) -> bool {
        self.on_air
    }

    /// See [`crate::link::Node::set_ofdm_gain_db`].
    pub fn set_ofdm_gain_db(&mut self, db: f32) {
        self.ofdm_gain = audio::ofdm_gain(db);
    }
}
