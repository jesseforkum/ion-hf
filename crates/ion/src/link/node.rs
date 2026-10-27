//! One complete Ion station driven by audio: the link [`Station`], every
//! streaming receiver and the codecs. The host feeds it received 48 kHz
//! audio and plays the bursts it hands out.

use std::sync::Arc;

use super::audio::{self, PhyAir, Receivers};
use super::frame::{AckInfo, Frame, Kind, Life, Tiny};
use super::station::{Air, CONTROL, Config, Incoming, Station, TINY, user_hi, user_session};
use crate::level::{LADDER, Wave};

/// One burst sent or heard, for the operator's log.
#[derive(Clone, Debug)]
pub struct BurstNote {
    /// We sent it.
    pub tx: bool,
    /// Ladder level, for a data burst.
    pub level: Option<usize>,
    /// "DATA L6 MFSK R0 ×7", "ACK 2 111111 +6 dB", "CONREQ A>B", ...
    pub what: String,
    /// Heard: its SNR, and its codewords that failed.
    pub snr_db: Option<f32>,
    pub lost: u32,
    /// Sent: seconds on the air, lead-in included.
    pub secs: f64,
    /// A connect request: (from, to).
    pub calls: Option<(String, String)>,
    /// An ACK: the SNR it reports (how its sender hears the other side).
    pub reported_db: Option<f32>,
}

/// A burst's log line, the calls of a connect request and the SNR an ACK
/// reports.
type Described = (String, Option<(String, String)>, Option<f32>);

/// What a burst is, from its header and (control bursts) first frame.
fn describe(wave: Wave, code: usize, user: u16, extra: u16, first: Option<&[u8]>, frames: usize) -> Described {
    let mut reported = None;
    if wave == Wave::Lifeline && code == TINY {
        let what = match Life::decode(user_hi(user), extra) {
            Some(Life::Probe { snr_db }) => {
                reported = snr_db;
                snr_db.map_or("PROBE".into(), |s| format!("PROBE {s:+.0} dB"))
            }
            Some(Life::Report { snr_db }) => {
                reported = Some(snr_db);
                format!("REPORT {snr_db:+.0} dB")
            }
            Some(Life::Bye { weak }) => if weak { "BYE WEAK" } else { "BYE" }.into(),
            Some(Life::ByeAck) => "BYEACK".into(),
            None => "LIFELINE".into(),
        };
        return (what, None, reported);
    }
    let what = match code {
        TINY => match Tiny::decode(user_hi(user), extra) {
            Some(Tiny::Ack {
                burst,
                bitmap,
                snr_db,
                want_turn,
                want_disc,
                stored,
                ..
            }) => {
                reported = Some(snr_db);
                format!(
                    "ACK {burst} {bitmap:06b} {snr_db:+.0} dB{}{}{}",
                    if stored { " HARQ" } else { "" },
                    if want_turn { " TURN?" } else { "" },
                    if want_disc { " DISC?" } else { "" },
                )
            }
            Some(Tiny::ConnAck { snr_db, .. }) => {
                reported = Some(snr_db);
                format!("CONACK {snr_db:+.0} dB")
            }
            Some(Tiny::Poll) => "POLL".into(),
            Some(Tiny::Turn { .. }) => "TURN".into(),
            Some(Tiny::Idle) => "IDLE".into(),
            Some(Tiny::DiscReq) => "DISC".into(),
            Some(Tiny::DiscAck) => "DISCACK".into(),
            None => "TINY".into(),
        },
        CONTROL => match first.and_then(Frame::decode) {
            Some(f) if f.kind == Kind::ConnReq => {
                let text: String = f
                    .payload
                    .iter()
                    .map(|&c| if c.is_ascii_graphic() { c as char } else { ' ' })
                    .collect();
                let mut it = text.split_whitespace();
                let (from, to) = (
                    it.next().unwrap_or("?").to_string(),
                    it.next().unwrap_or("?").to_string(),
                );
                return (format!("CONREQ {from}>{to}"), Some((from, to)), None);
            }
            Some(f) if f.kind == Kind::Ack => match AckInfo::from_bytes(&f.payload) {
                Some(a) => {
                    reported = Some(a.snr_db);
                    format!("ACK {} {} ok {:+.0} dB", a.burst, a.bitmap.count_ones(), a.snr_db)
                }
                None => "ACK".into(),
            },
            _ => "CTRL".into(),
        },
        // Received long-frame bursts arrive as soft bits: no count yet.
        l if frames == 0 => format!("DATA {}", crate::sounding::level_name(l, LADDER[l].wave)),
        l => format!("DATA {} ×{frames}", crate::sounding::level_name(l, LADDER[l].wave)),
    };
    (what, None, reported)
}

/// Default silence before each burst: covers the radio's PTT-to-RF delay so
/// the preamble is not clipped.
pub const DEFAULT_LEAD_S: f64 = 0.2;

/// Silence between a CW ID and the burst after it.
const ID_GAP_S: f64 = 0.3;

/// [`PhyAir`] plus the lead-in silence.
pub struct NodeAir {
    pub lead_s: f64,
}

impl Air for NodeAir {
    fn duration(&self, wave: Wave, code: usize, bits: usize) -> f64 {
        self.lead_s + PhyAir.duration(wave, code, bits)
    }

    fn max_bits(&self, wave: Wave, code: usize) -> usize {
        PhyAir.max_bits(wave, code)
    }
}

pub struct Node {
    ofdm_gain: f32,
    station: Station,
    rx: Receivers,
    bursts: Vec<Incoming>,
    lead: usize,
    /// Our burst is out (handed to the host and not yet finished).
    on_air: bool,
    /// SNR of the last burst we decoded.
    pub last_snr_db: Option<f32>,
    /// Bursts decoded, and their codewords that failed.
    pub bursts_heard: u32,
    pub codewords_failed: u32,
    journal: Vec<BurstNote>,
    /// Mid-link CW ID this often (None: never, as in simulation).
    id_every_s: Option<f64>,
    /// When we last keyed, and the peer whose ID length the station knows.
    last_key: f64,
    id_peer: String,
    /// Broadcast groups' bursts (by their ids) are kept for them as well.
    groups: Vec<u8>,
    group_in: Vec<Incoming>,
}

impl Node {
    pub fn new(cfg: Config, lead_s: f64) -> Self {
        let air: Arc<dyn Air> = Arc::new(NodeAir { lead_s });
        Self {
            ofdm_gain: 1.0,
            station: Station::new(cfg, air),
            rx: Receivers::new(),
            bursts: Vec::new(),
            lead: (lead_s * 48_000.0) as usize,
            on_air: false,
            last_snr_db: None,
            bursts_heard: 0,
            codewords_failed: 0,
            journal: Vec::new(),
            groups: Vec::new(),
            group_in: Vec::new(),
            id_every_s: None,
            last_key: f64::NEG_INFINITY,
            id_peer: String::new(),
        }
    }

    /// On the air: a CW ID ahead of our first burst after each `every_s`
    /// of the link's ID clock ([`Station::link_t0`]), which the peer keeps
    /// too: its waits allow for our ID, ours for its.
    pub fn set_id_every(&mut self, every_s: Option<f64>) {
        self.id_every_s = every_s;
    }

    /// Bursts sent and heard since the last call (at most the last 256).
    pub fn take_journal(&mut self) -> Vec<BurstNote> {
        std::mem::take(&mut self.journal)
    }

    fn note(&mut self, n: BurstNote) {
        if self.journal.len() >= 256 {
            self.journal.remove(0);
        }
        self.journal.push(n);
    }

    /// A station config sized for the real codes, for `call`.
    pub fn config(call: &str) -> Config {
        audio::config(call)
    }

    pub fn station(&self) -> &Station {
        &self.station
    }

    pub fn station_mut(&mut self) -> &mut Station {
        &mut self.station
    }

    /// Received audio, 48 kHz, ending at `now`. Ignored while our own burst
    /// is out (so we never decode ourselves).
    pub fn push_audio(&mut self, samples: &[f32], now: f64) {
        if self.on_air {
            return;
        }
        self.bursts.clear();
        self.rx.follow(self.station.session());
        self.rx.process(samples, &mut self.bursts);
        for inc in std::mem::take(&mut self.bursts) {
            self.bursts_heard += 1;
            if self.groups.contains(&user_session(inc.user)) {
                self.group_in.push(inc.clone());
            }
            // Long-frame codewords are decoded (and counted) by the station.
            let before = self.station.stats().codewords_failed;
            self.last_snr_db = Some(inc.snr_db);
            self.station.on_rx(&inc, now);
            let long_failed = self.station.stats().codewords_failed - before;
            let short_failed = if inc.soft.is_some() {
                0
            } else {
                inc.frames.iter().filter(|f| f.is_none()).count() as u32
            };
            let lost = long_failed.max(short_failed);
            self.codewords_failed += lost;
            let first = inc.frames.first().and_then(|f| f.as_deref());
            let (what, calls, reported_db) = describe(inc.wave, inc.code, inc.user, inc.extra, first, inc.frames.len());
            self.note(BurstNote {
                tx: false,
                level: (inc.code < LADDER.len()).then_some(inc.code),
                what,
                snr_db: Some(inc.snr_db),
                lost,
                secs: 0.0,
                calls,
                reported_db,
            });
        }
    }

    /// Keep the bursts of the broadcast groups with these ids (for a
    /// [`super::bcast::BcastStation`] beside the link).
    pub fn set_groups(&mut self, ids: &[u8]) {
        if self.groups != ids {
            self.groups = ids.to_vec();
            self.group_in.clear();
        }
    }

    /// The groups' bursts heard since the last call.
    pub fn take_group(&mut self) -> Vec<Incoming> {
        std::mem::take(&mut self.group_in)
    }

    /// Receiving a burst, or transmitting one: not the moment to key. The
    /// lifeline's receiver counts only while the link holds.
    pub fn busy(&self) -> bool {
        self.on_air || self.rx.busy() || self.station.holding() && self.rx.lifeline_busy()
    }

    /// The slot meter: the third of the passband our receivers would have
    /// slice bursts in (0 the centre, 1 below, 2 above) and each third's
    /// score (lower is cleaner; `crate::slots`).
    pub fn band(&self) -> (u8, [f32; 3]) {
        self.rx.slot()
    }

    /// Audio to transmit now, if the station has something to send. The
    /// host plays it and calls [`Node::tx_done`] when the last sample has
    /// gone out.
    pub fn poll(&mut self, now: f64) -> Option<Vec<f32>> {
        if self.busy() {
            return None;
        }
        if let Some(every) = self.id_every_s
            && self.id_peer != self.station.peer()
        {
            self.id_peer = self.station.peer().to_string();
            let s = self.lead as f64 / 48_000.0
                + crate::sounding::cw(&format!("DE {}", self.id_peer)).len() as f64 / 48_000.0
                + ID_GAP_S;
            self.station.set_ids(every, s);
        }
        let b = self.station.poll_tx(now)?;
        let mut out = vec![0.0; self.lead];
        if let (Some(every), Some(t0)) = (self.id_every_s, self.station.link_t0()) {
            let due = t0 + ((now - t0) / every).floor() * every;
            if now - t0 >= every && due > self.last_key {
                out.extend(crate::sounding::cw(&format!("DE {}", self.station.call())));
                out.extend(std::iter::repeat_n(0.0, (ID_GAP_S * 48_000.0) as usize));
                self.note(BurstNote {
                    tx: true,
                    level: None,
                    what: "CW ID".into(),
                    snr_db: None,
                    lost: 0,
                    secs: out.len() as f64 / 48_000.0,
                    calls: None,
                    reported_db: None,
                });
            }
        }
        out.extend(audio::transmit_with(&b, self.ofdm_gain));
        self.on_air = true;
        self.last_key = now;
        let first = b.frames.first().map(Vec::as_slice);
        let (what, calls, reported_db) = describe(b.wave, b.code, b.user, b.extra, first, b.frames.len());
        self.note(BurstNote {
            tx: true,
            level: (b.code < LADDER.len()).then_some(b.code),
            what,
            snr_db: None,
            lost: 0,
            secs: out.len() as f64 / 48_000.0,
            calls,
            reported_db,
        });
        Some(out)
    }

    /// Extra level for OFDM bursts, dB ([`audio::ofdm_gain`]): on a real
    /// transmitter, so OFDM reads the same power as the steady carriers;
    /// 0 (the default) in simulation.
    pub fn set_ofdm_gain_db(&mut self, db: f32) {
        self.ofdm_gain = audio::ofdm_gain(db);
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
}
