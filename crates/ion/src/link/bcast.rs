//! Broadcast groups, as Data2G's host offers them (its `docs/broadcast.md`):
//! a client's frames go to every station of a named group, once each, with
//! no acknowledgement and no resend. Reliability is the application's
//! (Ragtime's Acks and repair rounds, for one).
//!
//! - **Burst:** an ordinary Ion burst at one ladder level, the group's 8-bit
//!   id in the PHY header's session field. Its first codeword starts with a
//!   header: version, the sender's call, the group's 16-bit key (which
//!   settles the rare 8-bit id clash).
//! - **Frames** are COBS-encoded and each ended by a zero byte, back to back
//!   across the codewords (the field of each says its place), so a lost
//!   codeword loses only the frames it touches. A frame that would start a
//!   codeword gets a zero before it, so its start is known even when the
//!   codeword before is lost.
//! - **Channel access:** a frame queued while the channel is free goes at
//!   once; one that had to wait takes each [`BcastConfig::slot_s`] with
//!   probability (P + 1) / 256 (KISS persistence), and after
//!   [`BcastConfig::busy_limit_s`] of unbroken busy goes anyway.
//! - **Level:** the fastest the weakest station heard lately in the group
//!   copies, with a margin; [`BcastConfig::fallback_level`] until anyone is.
//!   A burst takes the first queued frame's group, then every later frame of
//!   that group that fits [`BcastConfig::max_burst_s`] (the first one always
//!   goes, however long).
//!
//! Driven like the link [`super::Station`]: [`BcastStation`] hands out bursts
//! and takes them on the caller's clock.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use ion_dsp::dsp::NoiseGen;

use super::frame::{Frame, Kind, crc32};
use super::harq::HarqRx;
use super::net::{net_id, pack_call, unpack_call};
use super::station::{Air, Incoming, Outgoing, frame_bits, user_bits, user_session};
use crate::level::{FrameSize, LADDER};

/// Header version.
const VERSION: u8 = 1;
/// Header bytes: version, packed call (5), group key (2).
const HEADER: usize = 8;

#[derive(Clone, Debug)]
pub struct BcastConfig {
    pub call: String,
    /// Longest burst, s (the first frame goes whatever its length).
    pub max_burst_s: f64,
    /// Channel access slot, s.
    pub slot_s: f64,
    /// KISS persistence P: a waiting burst takes a slot with probability
    /// (P + 1) / 256.
    pub persistence: u8,
    /// After this long of unbroken busy, a waiting burst goes anyway, s.
    pub busy_limit_s: f64,
    /// Level until a station of the group has been heard.
    pub fallback_level: usize,
    pub thresholds_db: Vec<f32>,
    /// A station's S/N must clear a level's threshold by this to count, dB.
    pub margin_db: f32,
    /// A station heard within this long counts for the level, s.
    pub heard_s: f64,
    pub turnaround_s: f64,
    pub seed: u64,
}

impl BcastConfig {
    pub fn new(call: &str) -> Self {
        Self {
            call: call.trim().to_ascii_uppercase(),
            max_burst_s: 8.0,
            slot_s: 1.0,
            persistence: 63,
            busy_limit_s: 60.0,
            fallback_level: 3,
            thresholds_db: crate::level::thresholds_db(),
            margin_db: 3.0,
            heard_s: 900.0,
            turnaround_s: 0.3,
            seed: crc32(call.as_bytes()) as u64 | 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BcastEvent {
    /// A frame for group `group` (as [`BcastStation::open`] numbered it).
    Frame { group: usize, data: Vec<u8> },
    /// A burst of the group from `call` (`None`: its header was lost), at
    /// this S/N (dB in 2.4 kHz).
    Heard {
        group: usize,
        call: Option<String>,
        snr_db: f32,
    },
    /// At least `frames` frames of a burst of the group were lost.
    Lost { group: usize, frames: u32 },
    /// The burst carrying our frame tagged `tag` has gone out.
    Sent { tag: u16 },
    /// Our frame for `group` was too big for any burst, and dropped.
    Dropped { group: usize, tag: Option<u16> },
}

#[derive(Clone, Debug, Default)]
pub struct BcastStats {
    pub bursts: u32,
    pub frames_sent: u32,
    pub airtime_s: f64,
    pub frames_heard: u32,
    pub frames_lost: u32,
}

struct Group {
    name: String,
    id: u8,
    key: u16,
}

struct Queued {
    group: usize,
    data: Vec<u8>,
    tag: Option<u16>,
}

/// The 16-bit key of a group name: FNV-1a over the packed name, never 0.
pub fn group_key(name: &str) -> u16 {
    let mut h: u32 = 0x811C_9DC5;
    for b in name.trim().to_ascii_uppercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    match (h ^ h >> 16) as u16 {
        0 => 1,
        k => k,
    }
}

/// COBS: `data` with no zero byte left in it (one byte more per 254).
pub fn cobs_encode(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0];
    let mut code_at = 0;
    let mut code = 1u8;
    for &b in data {
        if b == 0 {
            out[code_at] = code;
            code_at = out.len();
            out.push(0);
            code = 1;
        } else {
            out.push(b);
            code += 1;
            if code == 0xFF {
                out[code_at] = code;
                code_at = out.len();
                out.push(0);
                code = 1;
            }
        }
    }
    out[code_at] = code;
    out
}

pub fn cobs_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let code = data[i] as usize;
        if code == 0 || i + code > data.len() + 1 {
            return None;
        }
        let end = (i + code).min(data.len());
        out.extend_from_slice(&data[i + 1..end]);
        i += code;
        if code < 0xFF && i < data.len() {
            out.push(0);
        }
    }
    Some(out)
}

pub struct BcastStation {
    cfg: BcastConfig,
    air: Arc<dyn Air>,
    rng: NoiseGen,
    harq: HarqRx,
    groups: BTreeMap<usize, Group>,
    next_group: usize,
    queue: VecDeque<Queued>,
    /// The frames in the burst on the air: group and tag.
    sending: Vec<(usize, Option<u16>)>,
    /// Stations heard, per group: S/N and when.
    heard: BTreeMap<(usize, String), (f32, f64)>,
    busy: bool,
    busy_since: Option<f64>,
    /// The channel was busy (or a burst arrived) since our queue filled.
    waited: bool,
    on_air: bool,
    tx_at: f64,
    slot_at: f64,
    events: Vec<BcastEvent>,
    pub stats: BcastStats,
}

impl BcastStation {
    pub fn new(cfg: BcastConfig, air: Arc<dyn Air>) -> Self {
        Self {
            rng: NoiseGen::new(cfg.seed),
            cfg,
            air,
            harq: HarqRx::new(),
            groups: BTreeMap::new(),
            next_group: 0,
            queue: VecDeque::new(),
            sending: Vec::new(),
            heard: BTreeMap::new(),
            busy: false,
            busy_since: None,
            waited: false,
            on_air: false,
            tx_at: 0.0,
            slot_at: 0.0,
            events: Vec::new(),
            stats: BcastStats::default(),
        }
    }

    pub fn set_call(&mut self, call: &str) {
        self.cfg.call = call.trim().to_ascii_uppercase();
    }

    /// Opens a group; returns its number for [`BcastStation::send`] and the
    /// events.
    pub fn open(&mut self, name: &str) -> usize {
        let name = name.trim().to_ascii_uppercase();
        let g = self.next_group;
        self.next_group += 1;
        self.groups.insert(
            g,
            Group {
                id: net_id(&name),
                key: group_key(&name),
                name,
            },
        );
        g
    }

    /// Closes a group; returns the tags of its frames dropped unsent.
    pub fn close(&mut self, group: usize) -> Vec<Option<u16>> {
        self.groups.remove(&group);
        let (gone, keep): (Vec<Queued>, Vec<Queued>) = self.queue.drain(..).partition(|q| q.group == group);
        self.queue = keep.into();
        self.heard.retain(|k, _| k.0 != group);
        gone.into_iter().map(|q| q.tag).collect()
    }

    pub fn group_name(&self, group: usize) -> Option<&str> {
        self.groups.get(&group).map(|g| g.name.as_str())
    }

    /// The open groups' names.
    pub fn groups(&self) -> Vec<String> {
        self.groups.values().map(|g| g.name.clone()).collect()
    }

    /// Stations heard in the open groups within [`BcastConfig::heard_s`],
    /// newest first: call and S/N.
    pub fn heard(&self, now: f64) -> Vec<(String, f32)> {
        let mut h: Vec<(&String, f32, f64)> = self
            .heard
            .iter()
            .filter(|(_, x)| now - x.1 <= self.cfg.heard_s)
            .map(|((_, c), x)| (c, x.0, x.1))
            .collect();
        h.sort_by(|a, b| b.2.total_cmp(&a.2));
        let mut out: Vec<(String, f32)> = Vec::new();
        for (c, snr, _) in h {
            if !out.iter().any(|o| o.0 == *c) {
                out.push((c.clone(), snr));
            }
        }
        out
    }

    /// The 8-bit ids of the open groups (the PHY header's session field).
    pub fn ids(&self) -> Vec<u8> {
        self.groups.values().map(|g| g.id).collect()
    }

    /// Queues a frame for `group`; `tag` comes back as
    /// [`BcastEvent::Sent`] once it has gone out. False: no such group.
    pub fn send(&mut self, group: usize, data: &[u8], tag: Option<u16>) -> bool {
        if !self.groups.contains_key(&group) {
            return false;
        }
        if self.queue.is_empty() {
            self.waited = self.busy;
        }
        self.queue.push_back(Queued {
            group,
            data: data.to_vec(),
            tag,
        });
        true
    }

    /// KISS persistence (command 2).
    pub fn set_persistence(&mut self, p: u8) {
        self.cfg.persistence = p;
    }

    /// Frames waiting.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn on_air(&self) -> bool {
        self.on_air
    }

    pub fn take_events(&mut self) -> Vec<BcastEvent> {
        std::mem::take(&mut self.events)
    }

    /// The fastest level the weakest station heard lately in `group`
    /// copies.
    fn level(&self, group: usize, now: f64) -> usize {
        self.heard
            .iter()
            .filter(|(k, h)| k.0 == group && now - h.1 <= self.cfg.heard_s)
            .map(|(_, h)| {
                (0..self.cfg.thresholds_db.len().min(LADDER.len()))
                    .rev()
                    .find(|&l| h.0 >= self.cfg.thresholds_db[l] + self.cfg.margin_db)
                    .unwrap_or(0)
            })
            .min()
            .unwrap_or(self.cfg.fallback_level)
    }

    /// The burst's byte stream: header, then each frame COBS-encoded and
    /// ended by a zero, a zero before any that would start a codeword.
    fn stream(&self, g: &Group, frames: &[&[u8]], chunk: usize) -> Vec<u8> {
        let mut s = vec![VERSION];
        s.extend(pack_call(&self.cfg.call));
        s.extend(g.key.to_le_bytes());
        for f in frames {
            if s.len().is_multiple_of(chunk) {
                s.push(0);
            }
            s.extend(cobs_encode(f));
            s.push(0);
        }
        s
    }

    fn burst(&mut self, now: f64) -> Option<Outgoing> {
        let group = self.queue.front()?.group;
        let level = self.level(group, now);
        let size = LADDER[level].frame_bytes();
        let chunk = Frame::max_payload(size);
        let g = self.groups.get(&group)?;
        let max_bits = self.air.max_bits(LADDER[level].wave, level);
        // The first frame, then every later one of the group that fits.
        let mut take: Vec<usize> = Vec::new();
        for (i, q) in self.queue.iter().enumerate() {
            if q.group != group {
                continue;
            }
            let mut trial: Vec<&[u8]> = take.iter().map(|&j| self.queue[j].data.as_slice()).collect();
            trial.push(&q.data);
            let n = self.stream(g, &trial, chunk).len().div_ceil(chunk);
            let bits = frame_bits(level, n);
            let fits = bits <= max_bits && self.air.duration(LADDER[level].wave, level, bits) <= self.cfg.max_burst_s;
            if !take.is_empty() && !fits || bits > max_bits {
                break;
            }
            take.push(i);
        }
        if take.is_empty() {
            // Too big for any burst at this level: dropped.
            let q = self.queue.pop_front()?;
            self.events.push(BcastEvent::Dropped {
                group: q.group,
                tag: q.tag,
            });
            return None;
        }
        let frames: Vec<&[u8]> = take.iter().map(|&j| self.queue[j].data.as_slice()).collect();
        let s = self.stream(g, &frames, chunk);
        let id = g.id;
        let codewords: Vec<Vec<u8>> = s
            .chunks(chunk)
            .enumerate()
            .map(|(i, c)| {
                let mut f = Frame::new(Kind::Data, i as u32);
                f.payload = c.to_vec();
                f.encode(size)
            })
            .collect();
        for &j in take.iter().rev() {
            let q = self.queue.remove(j)?;
            self.sending.push((q.group, q.tag));
        }
        self.sending.reverse();
        self.stats.frames_sent += take.len() as u32;
        let b = Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, id, 0),
            extra: 0,
            frames: codewords,
            spans: Vec::new(),
        };
        self.stats.bursts += 1;
        self.stats.airtime_s += self.air.duration(b.wave, b.code, b.coded_bits());
        self.on_air = true;
        Some(b)
    }

    /// The burst to send now, if any.
    pub fn poll_tx(&mut self, now: f64) -> Option<Outgoing> {
        if self.on_air || now < self.tx_at || self.queue.is_empty() {
            return None;
        }
        if self.busy {
            self.waited = true;
            let since = *self.busy_since.get_or_insert(now);
            if now - since < self.cfg.busy_limit_s {
                return None;
            }
        }
        self.busy_since = None;
        if self.waited && !self.busy {
            // p-persistence, a slot at a time.
            if now < self.slot_at {
                return None;
            }
            self.slot_at = now + self.cfg.slot_s;
            let p = (self.cfg.persistence as f32 + 1.0) / 256.0;
            if self.rng.uniform() >= p {
                return None;
            }
        }
        self.waited = false;
        self.burst(now)
    }

    /// Stop at once (the host's ABORT): the burst on the air (cut by the
    /// caller) and every frame queued are dropped. Returns the group of each.
    pub fn abort(&mut self) -> Vec<usize> {
        self.on_air = false;
        let mut gone: Vec<usize> = std::mem::take(&mut self.sending).into_iter().map(|f| f.0).collect();
        gone.extend(self.queue.drain(..).map(|q| q.group));
        gone
    }

    /// Our burst has finished at `t`.
    pub fn on_tx_done(&mut self, t: f64) {
        self.on_air = false;
        self.tx_at = t;
        for (_, tag) in std::mem::take(&mut self.sending) {
            if let Some(tag) = tag {
                self.events.push(BcastEvent::Sent { tag });
            }
        }
        if !self.queue.is_empty() {
            self.waited = true;
        }
    }

    /// Receiving a burst (or the channel otherwise busy): do not key.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        if !busy {
            self.busy_since = None;
        }
    }

    /// A burst arrived, ending at `now`.
    pub fn on_rx(&mut self, inc: &Incoming, now: f64) {
        self.tx_at = self.tx_at.max(now + self.cfg.turnaround_s);
        if !self.queue.is_empty() {
            self.waited = true;
        }
        let id = user_session(inc.user);
        let candidates: Vec<usize> = self
            .groups
            .iter()
            .filter(|(_, g)| g.id == id)
            .map(|(&k, _)| k)
            .collect();
        if candidates.is_empty() || inc.code >= LADDER.len() {
            return;
        }
        let decoded: Vec<Option<Vec<u8>>> = match &inc.soft {
            Some(soft) if LADDER[inc.code].frame == FrameSize::Long => self.harq.receive(inc.code, 0, soft).frames,
            _ => inc.frames.clone(),
        };
        // Codewords by place; a frame that fails (or is not ours) is a gap.
        let mut chunks: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut last = 0;
        for f in decoded.iter().flatten().filter_map(|b| Frame::decode(b)) {
            if f.kind == Kind::Data {
                last = last.max(f.field);
                chunks.insert(f.field, f.payload);
            }
        }
        let n = (decoded.len() as u32).max(last + 1);
        // The header names the group (by key) and the sender.
        let head = chunks
            .get(&0)
            .filter(|c| c.len() >= HEADER && c[0] == VERSION)
            .map(|c| (unpack_call(&c[1..6]), u16::from_le_bytes([c[6], c[7]])));
        let group = match &head {
            Some((_, key)) => match candidates.iter().find(|g| self.groups[g].key == *key) {
                Some(&g) => g,
                None => return,
            },
            // Header lost: ours only if no other open group shares the id.
            None if candidates.len() == 1 => candidates[0],
            None => return,
        };
        let call = head.map(|h| h.0);
        if let Some(c) = &call {
            self.heard.insert((group, c.clone()), (inc.snr_db, now));
        }
        self.events.push(BcastEvent::Heard {
            group,
            call,
            snr_db: inc.snr_db,
        });
        let (frames, lost) = unstream(&chunks, n);
        self.stats.frames_heard += frames.len() as u32;
        self.stats.frames_lost += lost;
        for data in frames {
            self.events.push(BcastEvent::Frame { group, data });
        }
        if lost > 0 {
            self.events.push(BcastEvent::Lost { group, frames: lost });
        }
    }
}

/// The frames out of a burst's codewords (`n` of them, by place, those not
/// in `chunks` lost), and how many runs of frames the losses cut.
fn unstream(chunks: &BTreeMap<u32, Vec<u8>>, n: u32) -> (Vec<Vec<u8>>, u32) {
    let mut out = Vec::new();
    let mut lost = 0;
    let mut cur: Vec<u8> = Vec::new();
    // In step: the bytes are a frame's from its start (as the stream's are,
    // after the header).
    let mut in_step = true;
    for i in 0..n {
        let Some(c) = chunks.get(&i) else {
            // A gap: whatever frame it touches is gone, up to the next zero.
            if in_step || !cur.is_empty() {
                lost += 1;
            }
            cur.clear();
            in_step = false;
            continue;
        };
        let body = if i == 0 { c.get(HEADER..).unwrap_or(&[]) } else { &c[..] };
        for &b in body {
            if b == 0 {
                if in_step && !cur.is_empty() {
                    match cobs_decode(&cur) {
                        Some(f) => out.push(f),
                        None => lost += 1,
                    }
                }
                cur.clear();
                in_step = true;
            } else if in_step {
                cur.push(b);
            }
        }
    }
    if in_step && !cur.is_empty() {
        lost += 1;
    }
    (out, lost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cobs_round_trips() {
        for d in [
            vec![],
            vec![0],
            vec![0, 0],
            vec![1, 2, 3],
            vec![1, 0, 2, 0, 0, 3],
            (0..=255u8).collect::<Vec<_>>(),
            vec![7u8; 600],
        ] {
            let e = cobs_encode(&d);
            assert!(!e.contains(&0), "{d:?}");
            assert_eq!(cobs_decode(&e), Some(d.clone()));
        }
    }

    fn chunked(s: &[u8], chunk: usize) -> BTreeMap<u32, Vec<u8>> {
        s.chunks(chunk)
            .enumerate()
            .map(|(i, c)| (i as u32, c.to_vec()))
            .collect()
    }

    #[test]
    fn a_lost_codeword_loses_only_the_frames_it_touches() {
        let air: Arc<dyn Air> = Arc::new(crate::link::sim::ModelAir::default());
        let mut st = BcastStation::new(BcastConfig::new("N0AAA"), air);
        let g = st.open("D2G");
        let frames: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i + 1; 7 + 3 * i as usize]).collect();
        let refs: Vec<&[u8]> = frames.iter().map(Vec::as_slice).collect();
        let chunk = 22;
        let s = st.stream(&st.groups[&g], &refs, chunk);
        let all = chunked(&s, chunk);
        let n = all.len() as u32;
        assert_eq!(unstream(&all, n), (frames.clone(), 0));
        for gone in 0..n {
            let mut some = all.clone();
            some.remove(&gone);
            let (got, lost) = unstream(&some, n);
            assert!(lost >= 1, "codeword {gone}");
            // Every frame that arrived is one we sent, in order, and the
            // frames not touched by the lost codeword all arrived.
            let mut it = frames.iter();
            for f in &got {
                assert!(it.any(|x| x == f), "codeword {gone}: {f:?} out of order or unknown");
            }
            let mut at = 0;
            let start = gone as usize * chunk;
            let end = start + chunk;
            let mut pos = HEADER;
            for f in &frames {
                if pos.is_multiple_of(chunk) {
                    pos += 1;
                }
                let len = cobs_encode(f).len() + 1;
                let untouched = pos + len <= start || pos >= end;
                if untouched {
                    assert!(got[at..].contains(f), "codeword {gone}: lost an untouched frame {f:?}");
                }
                pos += len;
                at = got.iter().position(|x| x == f).map_or(at, |p| p + 1);
            }
        }
    }
}
