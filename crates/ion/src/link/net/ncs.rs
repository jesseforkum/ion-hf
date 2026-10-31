//! Net control: beacons and check-in windows, polled rounds, received
//! lists, direct messages, and moving leaves of the relay tree.

use super::*;

/// Most members in one net.
pub const MAX_MEMBERS: usize = 62;

/// Bursts a member may send per turn before NCS moves on.
const MAX_TURN_BURSTS: u32 = 4;

/// NCS's view of one member.
#[derive(Debug)]
pub(super) struct Member {
    pub(super) call: String,
    /// Its relay (NCS never polls it directly).
    pub(super) via: Option<u8>,
    pub(super) caps: Option<Caps>,
    pub(super) version: Option<u8>,
    pub(super) snr_up: Option<f32>,
    pub(super) snr_down: Option<f32>,
    pub(super) last_heard: Option<f64>,
    pub(super) missed: u32,
    pub(super) up: MsgIn,
    /// (next, bitmap) as last sent in a received list.
    pub(super) rx_told: (u32, u8),
    /// Relayers tried for its traffic since it last moved, and where it was.
    pub(super) relay_tried: Vec<u8>,
    pub(super) relay_mark: (u32, u8),
    /// Expected id of its next burst.
    pub(super) burst: u8,
    /// Its bulletin state: first block it lacks, and how many pieces short
    /// (0 or 255: all of it).
    pub(super) bul: Option<(u16, u8)>,
    /// NCS's messages for it (length-prefixed), and how much of them it holds.
    pub(super) dir: Vec<u8>,
    pub(super) dir_delivered: u32,
    pub(super) dir_sent_round: Option<u32>,
    pub(super) hears: BTreeMap<u8, f32>,
    pub(super) bytes_in: u64,
    /// A leaf NCS has heard directly after all (its own burst to its relay,
    /// decoded here): when, and how well. Makes it a direct member again.
    pub(super) direct: Option<(f64, f32)>,
    /// It announced [`CAPS_COMPRESS`] in its own check-in.
    pub(super) compress: bool,
    /// How much later than predicted its replies start, s (learned).
    pub(super) reply_cal: Option<f64>,
}

impl Member {
    fn new(call: &str, via: Option<u8>) -> Self {
        Self {
            call: call.to_string(),
            via,
            caps: None,
            version: None,
            snr_up: None,
            snr_down: None,
            last_heard: None,
            missed: 0,
            up: MsgIn::default(),
            rx_told: (0, 0),
            relay_tried: Vec::new(),
            relay_mark: (0, 0),
            burst: 0,
            bul: None,
            dir: Vec::new(),
            dir_delivered: 0,
            dir_sent_round: None,
            hears: BTreeMap::new(),
            bytes_in: 0,
            direct: None,
            compress: false,
            reply_cal: None,
        }
    }

    fn willing(&self) -> bool {
        self.caps.is_none_or(|c| c.relay)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Wait {
    /// A poll answered by `slot`, which also forwards `relay_for`'s traffic.
    Reply {
        slot: u8,
        relay_for: Option<u8>,
        tries: u32,
        turn_bursts: u32,
    },
    /// Our ACK that ends a turn: nothing to wait for once sent.
    Nothing,
}

impl NetStation {
    /// NCS: the members, in slot order.
    pub fn roster(&self, now: f64) -> Vec<RosterEntry> {
        self.members
            .iter()
            .map(|(&slot, m)| RosterEntry {
                slot,
                call: m.call.clone(),
                via: m.via.and_then(|v| self.calls.get(&v).cloned()),
                snr_up: m.snr_up,
                snr_down: m.snr_down,
                cap: self.cap_for(m),
                last_heard_s: m.last_heard.map(|t| now - t),
                missed: m.missed,
                bytes_in: m.bytes_in,
                hears: m
                    .hears
                    .iter()
                    .filter_map(|(s, &snr)| Some((self.calls.get(s)?.clone(), snr)))
                    .collect(),
                caps: m.caps,
                version: m.version,
                children: self
                    .members
                    .values()
                    .filter(|c| c.via == Some(slot))
                    .map(|c| c.call.clone())
                    .collect(),
            })
            .collect()
    }

    fn active(&self) -> impl Iterator<Item = &Member> {
        self.members
            .values()
            .filter(|m| m.missed < self.cfg.miss_limit && m.last_heard.is_some())
    }

    /// Active members NCS polls itself (not leaves).
    pub(super) fn direct(&self) -> impl Iterator<Item = &Member> {
        self.active().filter(|m| m.via.is_none())
    }

    /// The level a burst for `slot` goes at: its own, or its relay's.
    fn reach_level(&self, slot: u8) -> usize {
        let Some(m) = self.members.get(&slot) else { return FLOOR };
        match m.via.and_then(|v| self.members.get(&v)) {
            Some(r) => self.down_level(r),
            None => self.down_level(m),
        }
    }

    /// How long a relay's leaf exchange may still take: a whole burst from
    /// the leaf (which NCS cannot hear), the relay's ACK, and its answer
    /// starting.
    fn leaf_wait_s(&self) -> f64 {
        3.0 * self.cfg.turnaround_s + self.cfg.max_burst_s + self.cfg.detect_s + 2.0 * self.cfg.reply_slack_s + 1.0
    }

    /// Moves leaves whose relay has gone quiet (or who no longer answer it)
    /// to the willing member that hears them best.
    fn reroute(&mut self) {
        // A leaf NCS hears well on its own (say it joined through another
        // member only because its check-in collided) leaves its relay: a
        // relay tree is for stations NCS cannot hear.
        let floor = self.cfg.thresholds_db[FLOOR] + self.cfg.margin_db;
        let promote: Vec<u8> = self
            .members
            .iter()
            .filter(|(_, m)| m.via.is_some() && m.direct.is_some_and(|(_, snr)| snr >= floor))
            .map(|(&c, _)| c)
            .collect();
        for c in promote {
            let m = self.members.get_mut(&c).unwrap();
            m.via = None;
            m.missed = 0;
            m.snr_up = m.direct.map(|d| d.1);
            m.direct = None;
            let call = m.call.clone();
            self.tree_news.insert(c);
            self.events.push(NetEvent::Routed { call, via: None });
        }
        let lim = self.cfg.miss_limit;
        // Leaves whose relay or exchange failed, and members NCS has never
        // heard directly that keep missing their polls (admitted on another
        // member's word): both go through whoever hears them best.
        let leaves: Vec<(u8, Option<u8>)> = self
            .members
            .iter()
            .filter(|(_, m)| m.via.is_some() || (m.missed >= lim && m.snr_up.is_none()))
            .map(|(&c, m)| (c, m.via))
            .collect();
        for (c, p) in leaves {
            let parent_down = p.is_none_or(|p| self.members.get(&p).is_none_or(|r| r.missed >= lim));
            let leaf_down = self.members[&c].missed >= lim;
            if !parent_down && !leaf_down {
                continue;
            }
            let best = self
                .members
                .iter()
                .filter(|&(&r, m)| {
                    r != c && Some(r) != p && m.via.is_none() && m.missed == 0 && m.last_heard.is_some() && m.willing()
                })
                .filter_map(|(&r, m)| m.hears.get(&c).map(|&snr| (r, snr)))
                .filter(|&(_, snr)| snr >= self.cfg.relay_snr_db)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(r, _)| r);
            if let Some(r) = best {
                let m = self.members.get_mut(&c).unwrap();
                m.via = Some(r);
                m.missed = 0;
                let call = m.call.clone();
                self.tree_news.insert(c);
                self.events.push(NetEvent::Routed {
                    call,
                    via: self.calls.get(&r).cloned(),
                });
            }
        }
    }

    pub(super) fn cap_for(&self, m: &Member) -> usize {
        m.snr_up.map_or(FLOOR, |s| self.level_for(s))
    }

    pub(super) fn down_level(&self, m: &Member) -> usize {
        m.snr_down.or(m.snr_up).map_or(FLOOR, |s| self.level_for(s))
    }

    pub(super) fn ncs_next(&mut self, now: f64) -> Option<Outgoing> {
        if self.closed {
            if self.close_left == 0 {
                return None;
            }
            self.close_left -= 1;
            // Spaced out, so a reply already on its way cannot hide them all.
            self.tx_at = now + self.cfg.max_burst_s;
            return Some(self.tiny(K_CLOSE, 0, LADDER[self.cfg.beacon_level].wave));
        }
        if let Some(Wait::Reply {
            slot,
            relay_for,
            tries,
            turn_bursts,
        }) = self.wait
        {
            let deadline = self.deadline?;
            if now < deadline {
                return None;
            }
            // No answer.
            self.deadline = None;
            if tries == 0 {
                // Once more, in the most robust waveform.
                self.wait = Some(Wait::Reply {
                    slot,
                    relay_for,
                    tries: 1,
                    turn_bursts,
                });
                return Some(self.poll_burst(slot, relay_for, true));
            }
            self.wait = None;
            if let Some(m) = self.members.get_mut(&slot) {
                m.missed += 1;
                let call = m.call.clone();
                self.events.push(NetEvent::Missed { call });
            }
            self.turn_over(slot, relay_for);
        }
        if self.wait.is_some() || now < self.window_end {
            return None;
        }
        if std::mem::take(&mut self.window_activity) {
            // More may be waiting (or just collided): beacon again soon.
            self.next_beacon_at = self.next_beacon_at.min(now + self.cfg.quick_beacon_s);
        }
        // Beacon (opens a check-in window).
        if self.open && now >= self.next_beacon_at {
            self.next_beacon_at = now + self.cfg.beacon_every_s;
            return Some(self.beacon(now));
        }
        if !self.to_ack.is_empty() || !self.tree_news.is_empty() {
            let b = self.acks_burst();
            self.wait = Some(Wait::Nothing);
            return Some(b);
        }
        if self.round.is_empty() {
            if self.members.is_empty() {
                return None;
            }
            // The end of a round: say what has arrived, if that is news.
            if self.round_no > 0 && self.rx_sent_round != Some(self.round_no) {
                self.rx_sent_round = Some(self.round_no);
                if let Some(b) = self.rx_list_burst() {
                    self.wait = Some(Wait::Nothing);
                    return Some(b);
                }
            }
            if !self.round_had_traffic && self.round_no > 0 && now < self.next_round_at {
                return None;
            }
            self.reroute();
            if !self.tree_news.is_empty() {
                let b = self.acks_burst();
                self.wait = Some(Wait::Nothing);
                return Some(b);
            }
            self.round_no += 1;
            self.stats.rounds += 1;
            self.round_had_traffic = false;
            let rn = self.round_no;
            let lim = self.cfg.miss_limit;
            self.round = self
                .members
                .iter()
                .filter(|(_, m)| m.via.is_none() && (m.missed < lim || rn.is_multiple_of(4)))
                .map(|(&s, _)| s)
                .collect();
            self.next_round_at = now + self.cfg.idle_round_gap_s;
        }
        // The bulletin (or what some member still lacks of it), once per round.
        if self.bul_sent_round != Some(self.round_no)
            && let Some(b) = self.bulletin_burst()
        {
            self.bul_sent_round = Some(self.round_no);
            self.round_had_traffic = true;
            self.wait = Some(Wait::Nothing);
            return Some(b);
        }
        let slot = *self.round.front()?;
        // A message for this member (or one of its leaves) goes just
        // before its poll.
        let mut to: Vec<u8> = vec![slot];
        to.extend(
            self.members
                .iter()
                .filter(|(_, m)| m.via == Some(slot))
                .map(|(&c, _)| c),
        );
        if let Some(b) = to.into_iter().find_map(|s| self.direct_burst(s)) {
            self.round_had_traffic = true;
            self.wait = Some(Wait::Nothing);
            return Some(b);
        }
        self.round.pop_front();
        // Its poll also names it the one to forward a station whose traffic
        // NCS lacks, if it is the best placed to.
        let relay_for = self.relay_src_for(slot);
        self.wait = Some(Wait::Reply {
            slot,
            relay_for,
            tries: 0,
            turn_bursts: 0,
        });
        Some(self.poll_burst(slot, relay_for, false))
    }

    /// NCS may lack some of `a`'s traffic that others overheard: it missed
    /// its poll, or messages arrived past a hole.
    fn needs_relay(&self, a: u8) -> bool {
        self.members
            .get(&a)
            .is_some_and(|m| m.via.is_none() && (m.missed > 0 || m.up.holes()))
    }

    /// The member to forward `a`'s traffic: the willing one that hears it
    /// best, passing over those already tried without result.
    fn relayer_for(&self, a: u8) -> Option<u8> {
        let mut c: Vec<(u8, f32)> = self
            .members
            .iter()
            .filter(|&(&r, m)| r != a && m.via.is_none() && m.missed == 0 && m.last_heard.is_some() && m.willing())
            .filter_map(|(&r, m)| m.hears.get(&a).map(|&snr| (r, snr)))
            .filter(|&(_, snr)| snr >= self.cfg.relay_snr_db)
            .collect();
        c.sort_by(|x, y| y.1.total_cmp(&x.1));
        let tried = &self.members.get(&a)?.relay_tried;
        c.iter().find(|(r, _)| !tried.contains(r)).or(c.first()).map(|x| x.0)
    }

    /// The station `slot` should forward traffic for when polled.
    fn relay_src_for(&self, slot: u8) -> Option<u8> {
        self.members
            .keys()
            .copied()
            .filter(|&a| a != slot && self.needs_relay(a))
            .find(|&a| self.relayer_for(a) == Some(slot))
    }

    /// `slot`'s turn has ended; it was to forward `relay_for`'s traffic.
    fn turn_over(&mut self, slot: u8, relay_for: Option<u8>) {
        let Some(a) = relay_for else { return };
        let Some(m) = self.members.get(&a) else { return };
        let held = (m.up.next, m.up.bitmap());
        if held != m.relay_mark {
            let m = self.members.get_mut(&a).unwrap();
            m.relay_mark = held;
            m.relay_tried.clear();
            return;
        }
        // Nothing came of it: try another next time, and start over once
        // every member that hears it has had a go.
        let mut tried = m.relay_tried.clone();
        if !tried.contains(&slot) {
            tried.push(slot);
        }
        let left = self
            .members
            .iter()
            .any(|(&r, x)| r != a && x.hears.contains_key(&a) && !tried.contains(&r));
        if !left {
            tried.clear();
        }
        self.members.get_mut(&a).unwrap().relay_tried = tried;
    }

    fn poll_burst(&mut self, slot: u8, relay_for: Option<u8>, robust: bool) -> Outgoing {
        self.stats.polls += 1;
        let (cap, wave, burst) = match self.members.get(&slot) {
            Some(m) => {
                let wave = if robust {
                    LADDER[FLOOR].wave
                } else {
                    self.tiny_wave(m.snr_down.or(m.snr_up))
                };
                (self.cap_for(m), wave, m.burst)
            }
            None => (FLOOR, LADDER[FLOOR].wave, 0),
        };
        if let Some(src) = relay_for {
            let extra = slot as u16 & 63 | (src as u16 & 63) << 6 | (cap.min(15) as u16) << 12;
            return self.tiny(K_RELAY, extra, wave);
        }
        self.tiny(
            K_POLL,
            slot as u16 & 63 | (cap.min(15) as u16) << 6 | (burst as u16 & 3) << 10,
            wave,
        )
    }

    /// The end-of-round received list, when it has news: per station, the
    /// first message NCS lacks and which of the 8 after it it holds.
    fn rx_list_burst(&mut self) -> Option<Outgoing> {
        let mut entries: Vec<[u8; 3]> = Vec::new();
        // Traffic NCS acknowledged directly needs no list: the sender and
        // everyone who heard the ACK know. Relayed traffic and holes do.
        let mut news = std::mem::take(&mut self.rx_dirty);
        for (&s, m) in self.members.iter_mut() {
            let held = (m.up.next, m.up.bitmap());
            let holes = m.up.holes();
            news |= holes;
            if held != m.rx_told || holes {
                m.rx_told = held;
                entries.push([s, held.0 as u8, held.1]);
            }
        }
        if !news || entries.is_empty() {
            return None;
        }
        // And how many bulletin blocks there are, when that changed.
        if self.bul_tx.len() as u16 != self.bul_told {
            self.bul_told = self.bul_tx.len() as u16;
            let n = self.bul_told.to_le_bytes();
            entries.push([ALL, n[0], n[1]]);
        }
        self.stats.rx_lists += 1;
        let frames: Vec<Frame> = entries
            .chunks(7)
            .map(|c| {
                let mut p = vec![C_RXLIST];
                p.extend(c.iter().flatten());
                ctrl_frame(ALL, p)
            })
            .collect();
        // In the waveform the weakest member NCS polls can hear.
        let level = self.direct().map(|m| self.down_level(m)).min().unwrap_or(FLOOR);
        Some(self.ctrl(frames, LADDER[level].wave))
    }

    fn beacon(&mut self, now: f64) -> Outgoing {
        let mut p = vec![C_BEACON];
        p.extend(pack_call(&self.cfg.call));
        let mut name = self.cfg.net.as_bytes().to_vec();
        name.resize(6, b' ');
        p.extend(&name[..6]);
        p.push(self.open as u8);
        p.push(self.cfg.window_slots);
        // The roster so far, so late joiners learn the slots.
        let entries: Vec<(u8, String)> = self.members.iter().map(|(&s, m)| (s, m.call.clone())).collect();
        let acks: Vec<Frame> = self.ack_frames(&entries).into_iter().take(3).collect();
        let wave = LADDER[self.cfg.beacon_level].wave;
        let size = LADDER[CTRL_CODE].frame_bytes();
        let dur = self
            .air
            .duration(wave, CTRL_CODE, frame_bits(CTRL_CODE, 1 + acks.len()));
        self.slots = None;
        if self.cfg.prime {
            // The window's timing, after the window size: where slot 0's
            // burst starts after the beacon's own start, and the slot
            // length, in ms. Members time their check-ins to it; one that
            // ignores it still lands close.
            let lead = self.lead_s;
            let first = dur - lead + self.cfg.turnaround_s + lead + self.cfg.rx_latency_s;
            let first_ms = (first * 1000.0).round().clamp(0.0, 65535.0) as u16;
            let len_ms = (self.slot_len() * 1000.0).round().clamp(1.0, 65535.0) as u16;
            p.extend(first_ms.to_le_bytes());
            p.extend(len_ms.to_le_bytes());
            let start = now + lead;
            let s0 = start + first_ms as f64 / 1000.0;
            self.slots = Some((s0, len_ms as f64 / 1000.0, self.cfg.window_slots));
            self.chorus = None;
            if self.cfg.chorus_slots > 0 {
                // How many chorus slots follow the ordinary ones, and how
                // long each is (ms).
                let c_ms = (chorus_slot_s(self.cfg.turnaround_s) * 1000.0).round() as u16;
                p.push(self.cfg.chorus_slots);
                p.extend(c_ms.to_le_bytes());
                let c0 = s0 + self.cfg.window_slots as f64 * len_ms as f64 / 1000.0;
                self.chorus = Some((c0, c_ms as f64 / 1000.0, self.cfg.chorus_slots));
            }
        }
        let mut frames = vec![ctrl_frame(ALL, p)];
        frames.extend(acks);
        debug_assert!(frames.iter().all(|f| f.payload.len() <= Frame::max_payload(size)));
        self.to_ack.clear();
        let b = self.ctrl(frames, wave);
        debug_assert_eq!(dur, self.air.duration(b.wave, b.code, b.coded_bits()));
        // Nobody else keys until the window has passed.
        self.window_end = now
            + dur
            + self.cfg.turnaround_s
            + self.cfg.window_slots as f64 * self.slot_len()
            + self.chorus.map_or(0.0, |c| c.2 as f64 * c.1)
            + self.cfg.reply_slack_s;
        self.wait = Some(Wait::Nothing);
        b
    }

    fn ack_frames(&self, entries: &[(u8, String)]) -> Vec<Frame> {
        entries
            .chunks(3)
            .map(|c| {
                let mut p = vec![C_ACKS];
                for (s, call) in c {
                    p.push(*s);
                    p.extend(pack_call(call));
                }
                ctrl_frame(ALL, p)
            })
            .collect()
    }

    /// Slot assignments, and the relay of every leaf among them or moved.
    fn acks_burst(&mut self) -> Outgoing {
        let mut slots: BTreeSet<u8> = std::mem::take(&mut self.to_ack).into_iter().collect();
        let entries: Vec<(u8, String)> = slots
            .iter()
            .filter_map(|&s| Some((s, self.members.get(&s)?.call.clone())))
            .collect();
        let mut frames = self.ack_frames(&entries);
        let moved = std::mem::take(&mut self.tree_news);
        slots.extend(&moved);
        let tree: Vec<(u8, u8)> = slots
            .iter()
            .filter_map(|&s| {
                let m = self.members.get(&s)?;
                // Only leaves, and members that used to be.
                (m.via.is_some() || moved.contains(&s)).then_some((s, m.via.unwrap_or(ALL)))
            })
            .collect();
        for c in tree.chunks(10) {
            let mut p = vec![C_TREE];
            for (s, v) in c {
                p.extend([*s, *v]);
            }
            frames.push(ctrl_frame(ALL, p));
        }
        // In the waveform the weakest of them (or their relays) can hear.
        let wave = slots
            .iter()
            .map(|&s| self.reach_level(s))
            .min()
            .map_or(LADDER[FLOOR].wave, |l| LADDER[l].wave);
        self.ctrl(frames, wave)
    }

    fn direct_burst(&mut self, slot: u8) -> Option<Outgoing> {
        let round = self.round_no;
        let m = self.members.get(&slot)?;
        if m.dir_sent_round == Some(round) || m.dir_delivered as usize >= m.dir.len() {
            return None;
        }
        let level = self.reach_level(slot);
        let from = m.dir_delivered;
        let data = m.dir[from as usize..].to_vec();
        self.members.get_mut(&slot)?.dir_sent_round = Some(round);
        Some(self.down_burst(slot, from, &data, level))
    }

    fn down_burst(&self, slot: u8, from: u32, data: &[u8], level: usize) -> Outgoing {
        let size = LADDER[level].frame_bytes();
        let max = Frame::max_payload(size);
        let room = self.room(level, 64);
        let frames = data
            .chunks(max)
            .take(room)
            .enumerate()
            .map(|(i, c)| {
                let mut f = Frame::new(Kind::Data, from + (i * max) as u32);
                f.flags = T_DOWN << 6 | (slot & 63);
                f.payload = c.to_vec();
                f.encode(size)
            })
            .collect();
        Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, self.id, 0),
            extra: 0,
            frames,
            spans: Vec::new(),
        }
    }

    pub(super) fn ncs_rx(&mut self, inc: &Incoming, frames: &[Option<Frame>], now: f64) {
        let in_turn = matches!(self.wait, Some(Wait::Reply { .. }));
        let aside = inc.code != TINY && user_hi(inc.user) & ASIDE != 0;
        if in_turn && (aside || inc.code == TINY && matches!(user_hi(inc.user), K_SUBPOLL | K_SUBACK)) {
            // The relay we polled is busy with its leaves: give it time.
            self.deadline = Some(now + self.leaf_wait_s());
        }
        if inc.code == TINY {
            if user_hi(inc.user) == K_IDLE {
                let slot = (inc.extra & 63) as u8;
                let heard_us = snr_from_q((inc.extra >> 6 & 63) as u8);
                if let Some(m) = self.members.get_mut(&slot) {
                    if m.via.is_some() {
                        // A leaf answering its relay, heard here directly.
                        m.direct = Some((now, inc.snr_db));
                    }
                    m.snr_up = Some(smooth(m.snr_up, inc.snr_db));
                    m.snr_down = Some(heard_us);
                    m.last_heard = Some(now);
                    m.missed = 0;
                }
                if let Some(Wait::Reply { slot: s, relay_for, .. }) = self.wait
                    && s == slot
                {
                    self.calibrate(false, Some(slot));
                    self.stats.replies += 1;
                    self.wait = None;
                    self.deadline = None;
                    self.turn_over(slot, relay_for);
                }
            }
            return;
        }
        // Not an answer to a poll: a check-in, or a relay and its leaves.
        let polled = match self.wait {
            Some(Wait::Reply { slot, .. }) if !aside => Some(slot),
            _ => None,
        };
        let mut sender: Option<u8> = None;
        let mut more_pending = false;
        if aside {
            // A leaf's own burst to its relay (relays answer NCS without the
            // aside flag), decoded here: NCS can hear that leaf after all.
            for f in frames.iter().flatten() {
                if f.flags >> 6 == T_UP
                    && let Some(m) = self.members.get_mut(&(f.flags & 63))
                    && m.via.is_some()
                {
                    m.direct = Some((now, inc.snr_db));
                }
            }
        }
        for f in frames.iter().flatten() {
            let ty = f.flags >> 6;
            let slot = f.flags & 63;
            match ty {
                T_CTRL => match f.payload.first().copied() {
                    Some(C_CHECKIN) if f.payload.len() >= 7 => {
                        self.window_activity = true;
                        if now < self.window_end + self.cfg.max_burst_s {
                            self.calibrate(true, None);
                        }
                        let p = &f.payload;
                        let call = unpack_call(&p[1..6]);
                        let heard_us = snr_from_q(p[6]);
                        let caps = p.get(7).map(|&b| Caps::from_byte(b));
                        let version = p.get(9).copied();
                        let s = self.admit(&call, None, now);
                        if let Some(m) = s.and_then(|s| self.members.get_mut(&s)) {
                            m.snr_up = Some(smooth(m.snr_up, inc.snr_db));
                            m.snr_down = Some(heard_us);
                            m.last_heard = Some(now);
                            m.caps = caps.or(m.caps);
                            m.version = version.or(m.version);
                            m.compress = p.get(7).is_some_and(|&b| b & CAPS_COMPRESS != 0);
                        }
                    }
                    Some(C_REPORT) if f.payload.len() >= 12 => {
                        let s = f.payload[1];
                        let p = &f.payload;
                        let forwarded = polled.is_some()
                            && self.members.get(&s).is_some_and(|m| m.via.is_some() && m.via == polled);
                        if forwarded {
                            // A leaf's report, through its relay.
                            if let Some(m) = self.members.get_mut(&s) {
                                m.last_heard = Some(now);
                            }
                        } else {
                            sender = Some(s);
                            more_pending |= u24(&p[3..6]) > 0;
                        }
                        if let Some(m) = self.members.get_mut(&s) {
                            m.snr_down = Some(snr_from_q(p[2]));
                            m.bul = Some((u16::from_le_bytes([p[6], p[7]]), p[8]));
                            m.dir_delivered = m.dir_delivered.max(u24(&p[9..12]));
                            for h in p[12..].as_chunks::<2>().0 {
                                m.hears.insert(h[0], snr_from_q(h[1]));
                            }
                        }
                    }
                    Some(C_OVERHEARD) => {
                        // Stations a member heard asking in that NCS did
                        // not: they join as its leaves, if it will relay.
                        let relay = self
                            .members
                            .get(&slot)
                            .filter(|r| r.via.is_none() && r.willing())
                            .map(|_| slot);
                        for e in f.payload[1..].as_chunks::<8>().0 {
                            let call = unpack_call(&e[..5]);
                            if self.members.values().any(|m| m.call == call) {
                                continue;
                            }
                            let Some(c) = self.admit(&call, relay, now) else {
                                continue;
                            };
                            if let Some(m) = self.members.get_mut(&c) {
                                m.caps = Some(Caps::from_byte(e[6]));
                                m.version = Some(e[7]);
                            }
                            if let Some(r) = self.members.get_mut(&slot) {
                                r.hears.insert(c, snr_from_q(e[5]));
                            }
                        }
                    }
                    Some(C_KIDS) => {
                        // Which of its leaves the relay reached this turn.
                        for e in f.payload[1..].as_chunks::<2>().0 {
                            let Some(m) = self.members.get_mut(&e[0]) else { continue };
                            if m.via != Some(slot) {
                                continue;
                            }
                            if e[1] & 0x80 != 0 {
                                m.missed = 0;
                                m.last_heard = Some(now);
                                m.snr_up = Some(snr_from_q(e[1] & 63));
                            } else {
                                m.missed += 1;
                                let call = m.call.clone();
                                self.events.push(NetEvent::Missed { call });
                            }
                        }
                    }
                    _ => {}
                },
                T_UP => {
                    let Some(m) = self.members.get_mut(&slot) else { continue };
                    m.last_heard = Some(now);
                    let Some((_, data)) = m.up.accept(f.field, &f.payload) else {
                        continue;
                    };
                    m.bytes_in += data.len() as u64;
                    let call = m.call.clone();
                    self.round_had_traffic = true;
                    // Forwarded by the station we polled: it keeps its
                    // originator's slot and number.
                    if let Some(v) = polled.filter(|&p| p != slot).and_then(|p| self.calls.get(&p).cloned()) {
                        self.stats.relays += 1;
                        self.rx_dirty = true;
                        self.events.push(NetEvent::Relayed {
                            source: call.clone(),
                            via: v,
                        });
                    }
                    self.events.push(NetEvent::Message {
                        from: call,
                        to: Some(self.cfg.call.clone()),
                        data,
                        overheard: false,
                    });
                }
                _ => {}
            }
        }
        let Some(Wait::Reply {
            slot,
            relay_for,
            turn_bursts,
            ..
        }) = self.wait
        else {
            return;
        };
        if aside || inc.code >= LADDER.len() {
            return;
        }
        // A burst of traffic from the station we polled.
        let from = sender.unwrap_or(slot);
        if from != slot {
            return;
        }
        self.calibrate(false, Some(slot));
        self.stats.replies += 1;
        if let Some(m) = self.members.get_mut(&slot) {
            m.snr_up = Some(smooth(m.snr_up, inc.snr_db));
            m.last_heard = Some(now);
            m.missed = 0;
        }
        let burst = user_hi(inc.user) & 3;
        let more = user_hi(inc.user) & 4 != 0 || more_pending;
        let bitmap = frames
            .iter()
            .take(6)
            .enumerate()
            .fold(0u16, |a, (i, f)| a | (f.is_some() as u16) << i);
        if let Some(m) = self.members.get_mut(&slot) {
            m.burst = (burst + 1) & 3;
        }
        let wave = self
            .members
            .get(&slot)
            .map_or(LADDER[FLOOR].wave, |m| self.tiny_wave(m.snr_down));
        let extra = slot as u16 & 63 | (burst as u16) << 6 | bitmap << 8;
        self.deadline = None;
        if more && turn_bursts + 1 < MAX_TURN_BURSTS {
            self.wait = Some(Wait::Reply {
                slot,
                relay_for,
                tries: 0,
                turn_bursts: turn_bursts + 1,
            });
            self.reply = Some(self.tiny(K_POLLACK, extra, wave));
        } else {
            self.wait = Some(Wait::Nothing);
            self.reply = Some(self.tiny(K_ACK, extra, wave));
            self.turn_over(slot, relay_for);
        }
    }

    /// Puts `call` on the roster (or finds it there). Returns its slot.
    pub(super) fn admit(&mut self, call: &str, via: Option<u8>, now: f64) -> Option<u8> {
        if let Some((&s, _)) = self.members.iter().find(|(_, m)| m.call == call) {
            if !self.to_ack.contains(&s) {
                self.to_ack.push(s);
            }
            return Some(s);
        }
        if self.members.len() >= MAX_MEMBERS {
            return None;
        }
        let slot = (0..MAX_MEMBERS as u8).find(|s| !self.members.contains_key(s))?;
        let mut m = Member::new(call, via);
        m.last_heard = via.is_none().then_some(now);
        self.members.insert(slot, m);
        self.calls.insert(slot, call.to_string());
        self.to_ack.push(slot);
        let via_call = via.and_then(|v| self.calls.get(&v).cloned());
        if let Some(v) = &via_call {
            self.events.push(NetEvent::Relayed {
                source: call.to_string(),
                via: v.clone(),
            });
        }
        self.events.push(NetEvent::CheckedIn {
            call: call.to_string(),
            slot,
            via: via_call,
        });
        Some(slot)
    }
}
