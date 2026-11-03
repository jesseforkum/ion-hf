//! The relay tree: a station NCS cannot reach is the leaf of a willing
//! member, which polls it during its own turn and forwards both ways.

use super::*;

/// A relay sends its call to its leaves at least this often (station
/// identification every ten minutes).
const ID_EVERY_S: f64 = 600.0;

/// A relay's view of one of its leaves.
#[derive(Debug)]
pub(super) struct Kid {
    pub(super) burst: u8,
    /// It has answered since we became its relay.
    pub(super) confirmed: bool,
    /// When we last sent it our call.
    pub(super) id_at: f64,
    pub(super) missed: u32,
    pub(super) turns: u32,
    /// Its latest report, to forward to NCS.
    pub(super) report: Option<Vec<u8>>,
    pub(super) report_fwd: bool,
    /// What it holds of the bulletin and of its direct messages, and how it
    /// hears us (from its report).
    pub(super) bul: Option<(u16, u8)>,
    /// (block, pieces of it sent) to it.
    pub(super) bul_sent: (u16, u32),
    pub(super) dir: u32,
    pub(super) snr_down: Option<f32>,
    /// NCS's messages for it.
    pub(super) dir_in: Inbox,
    /// Its check-in line's parts, to forward to NCS, and which went.
    pub(super) line: [Option<Vec<u8>>; 2],
    pub(super) line_fwd: [bool; 2],
    /// Our offer list's version last sent to it.
    pub(super) offer_ver: u32,
}

impl Kid {
    fn new() -> Self {
        Self {
            burst: 0,
            confirmed: false,
            id_at: f64::NEG_INFINITY,
            missed: 0,
            turns: 0,
            report: None,
            report_fwd: true,
            bul: None,
            bul_sent: (0, 0),
            dir: 0,
            snr_down: None,
            dir_in: Inbox::default(),
            line: [None, None],
            line_fwd: [true, true],
            offer_ver: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Stage {
    /// Send it what it lacks (and who we are).
    Down(u8),
    Poll(u8),
    /// Waiting for its answer.
    Wait(u8),
}

/// A relay's turn: its leaves first, one by one, then its answer to NCS.
#[derive(Debug)]
pub(super) struct Turn {
    pub(super) queue: VecDeque<u8>,
    pub(super) stage: Option<Stage>,
    pub(super) deadline: Option<f64>,
}

impl NetStation {
    /// NCS polled us and we relay for leaves: poll them first.
    pub(super) fn start_turn(&mut self, now: f64) {
        let lim = self.cfg.miss_limit;
        let mut queue = VecDeque::new();
        for (&c, k) in self.kids.iter_mut() {
            k.turns += 1;
            // A leaf that stopped answering is tried every fourth turn.
            if k.missed < lim || k.turns.is_multiple_of(4) {
                queue.push_back(c);
            }
        }
        if queue.is_empty() {
            self.reply = Some(self.uplink_burst(now));
        } else {
            self.turn = Some(Turn {
                queue,
                stage: None,
                deadline: None,
            });
        }
    }

    pub(super) fn turn_step(&mut self, now: f64) -> Option<Outgoing> {
        loop {
            let stage = self.turn.as_ref()?.stage;
            match stage {
                None => {
                    let next = self.turn.as_mut()?.queue.pop_front();
                    match next {
                        Some(c) => self.turn.as_mut()?.stage = Some(Stage::Down(c)),
                        None => {
                            self.turn = None;
                            return Some(self.uplink_burst(now));
                        }
                    }
                }
                Some(Stage::Down(c)) => {
                    self.turn.as_mut()?.stage = Some(Stage::Poll(c));
                    if let Some(b) = self.kid_down_burst(c, now) {
                        return Some(b);
                    }
                }
                Some(Stage::Poll(c)) => {
                    let Some(k) = self.kids.get(&c) else {
                        self.turn.as_mut()?.stage = None;
                        continue;
                    };
                    let up = self.heard.get(&c).map(|h| h.0);
                    let cap = up.map_or(FLOOR, |s| self.level_for(s));
                    let wave = self.tiny_wave(k.snr_down.or(up));
                    let extra = c as u16 & 63 | (cap.min(15) as u16) << 6 | (k.burst as u16 & 3) << 10;
                    let t = self.turn.as_mut()?;
                    t.stage = Some(Stage::Wait(c));
                    t.deadline = None;
                    self.talker = Some(c);
                    return Some(self.tiny(K_SUBPOLL, extra, wave));
                }
                Some(Stage::Wait(c)) => {
                    let d = self.turn.as_ref()?.deadline?;
                    if now < d {
                        return None;
                    }
                    self.turn.as_mut()?.stage = None;
                    if let Some(k) = self.kids.get_mut(&c) {
                        k.missed += 1;
                    }
                    self.kid_status.push((c, false));
                }
            }
        }
    }

    /// What leaf `c` lacks of the bulletin, offered objects and its own
    /// messages, and our call if it does not know us yet (or is due an ID).
    fn kid_down_burst(&mut self, c: u8, now: f64) -> Option<Outgoing> {
        let k = self.kids.get(&c)?;
        let up = self.heard.get(&c).map(|h| h.0);
        let level = k.snr_down.or(up).map_or(FLOOR, |s| self.level_for(s));
        let size = LADDER[level].frame_bytes();
        let max = Frame::max_payload(size);
        let room = self.room(level, 64);
        let mut frames: Vec<Frame> = Vec::new();
        let id = !k.confirmed || now - k.id_at >= ID_EVERY_S;
        if id {
            let mut p = vec![C_CHILD, c];
            p.extend(pack_call(self.calls.get(&c).map_or("", |s| s.as_str())));
            p.push(self.my_slot.unwrap_or(ALL));
            p.extend(pack_call(&self.cfg.call));
            p.extend(pack_call(self.ncs_call.as_deref().unwrap_or("")));
            frames.push(ctrl_frame(self.my_slot.unwrap_or(ALL), p));
        }
        // The offer list, when it changed, or when its report says it has
        // everything else but missed an offer.
        let unaware = k.report.as_ref().is_some_and(|p| {
            let b = u16::from_le_bytes([p[6], p[7]]);
            b < OBJ_BLOCK
                && b >= self.bul_known
                && (0..MAX_OFFERS).any(|i| self.known_offers[i].is_some() && p[13] >> i & 1 == 0)
        });
        let offers = k.offer_ver != self.offer_ver || unaware;
        if offers {
            // As much of it as fits, a different part each turn.
            let list = offer_frames(&self.known_offers, self.my_slot.unwrap_or(ALL));
            let fit = room.saturating_sub(frames.len()).clamp(1, list.len());
            let from = k.turns as usize * fit;
            frames.extend((0..fit).map(|x| list[(from + x) % list.len()].clone()));
        }
        let down = |slot: u8, hist: &[u8], from: u32, frames: &mut Vec<Frame>| {
            let mut off = from as usize;
            while off < hist.len() && frames.len() < room {
                let n = (hist.len() - off).min(max);
                let mut f = Frame::new(Kind::Data, off as u32);
                f.flags = T_DOWN << 6 | (slot & 63);
                f.payload = hist[off..off + n].to_vec();
                frames.push(f);
                off += n;
            }
        };
        down(c, &k.dir_in.hist, k.dir, &mut frames);
        // The bulletin block it lacks, coded as NCS codes it: the same
        // pieces help anyone else who hears them.
        let mut bul_sent = None;
        if let Some((b, short)) = k.bul
            && let Some(data) = self.block_data(b)
        {
            let sources = net_code::k_of(data.len());
            let want = lacking(short, sources);
            let sent = if k.bul_sent.0 == b { k.bul_sent.1 } else { 0 };
            let per = pieces_per_frame(max);
            let n = want.min(room.saturating_sub(frames.len()) * per);
            if n > 0 {
                frames.extend(bul_frames(b, &data, sent, n, per));
                let repairs = (sent..sent + n as u32)
                    .filter(|i| (i % 256) as usize >= sources)
                    .count() as u32;
                if b >= OBJ_BLOCK {
                    self.stats.obj_pieces += n as u32;
                    self.stats.obj_repairs += repairs;
                } else {
                    self.stats.bul_pieces += n as u32;
                    self.stats.bul_repairs += repairs;
                }
                bul_sent = Some((b, sent + n as u32));
            }
        }
        if frames.is_empty() {
            return None;
        }
        if let Some(k) = self.kids.get_mut(&c) {
            if id {
                k.id_at = now;
            }
            if let Some(bs) = bul_sent {
                k.bul_sent = bs;
            }
            if offers {
                k.offer_ver = self.offer_ver;
            }
        }
        Some(Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, self.id, ASIDE),
            extra: 0,
            frames: frames.iter().map(|f| f.encode(size)).collect(),
            spans: Vec::new(),
        })
    }

    /// NCS (or a relay) says leaf `c` goes through `r` ([`ALL`]: nobody).
    pub(super) fn set_parent(&mut self, c: u8, r: u8) {
        if r == ALL {
            self.parents.remove(&c);
        } else {
            self.parents.insert(c, r);
        }
        if Some(c) == self.my_slot {
            self.parent = (r != ALL).then_some(r);
        }
        if r == ALL || Some(r) != self.my_slot {
            self.kids.remove(&c);
        } else {
            self.kids.entry(c).or_insert_with(Kid::new);
        }
    }
}
