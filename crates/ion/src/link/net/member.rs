//! A member: check-in, answers to polls (report, traffic, and others'
//! traffic NCS lacks), and gear shifting on NCS's ACKs.

use super::*;

/// Most frames in a member's burst (a tiny ACK's bitmap is 6 bits).
const MAX_UP_FRAMES: usize = 6;

/// How long a member stays below a level on which it lost a whole burst, s.
const HOLD_OFF_S: f64 = 60.0;

/// An uplink piece in an overheard frame: (source, message, offset, length).
pub(super) type Piece = (u8, u32, usize, usize);

/// One frame of our last uplink burst, for matching the ACK bitmap.
#[derive(Clone, Debug)]
pub(super) enum Sent {
    Data {
        seq: u32,
        off: usize,
    },
    Report([u8; 7]),
    Overheard(Vec<String>),
    Relay {
        src: u8,
        seq: u32,
        off: usize,
        len: usize,
    },
    /// A leaf's report, forwarded.
    KidReport(u8),
    Other,
}

/// A station heard asking to join that NCS has not taken yet.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Applicant {
    call: String,
    snr: f32,
    caps: Caps,
    version: u8,
}

impl NetStation {
    pub(super) fn member_next(&mut self, now: f64) -> Option<Outgoing> {
        if self.closed {
            return None;
        }
        if self.turn.is_some() {
            return self.turn_step(now);
        }
        let Some(at) = self.checkin_at else {
            return self.seek(now);
        };
        if now < at {
            return None;
        }
        self.checkin_at = None;
        if self.my_slot.is_some() {
            return None;
        }
        let mut p = checkin_payload(
            &self.cfg.call,
            self.snr_ncs.unwrap_or(0.0),
            self.cfg.caps,
            self.cfg.version,
        );
        if self.cfg.compress {
            p[7] |= CAPS_COMPRESS;
        }
        let f = ctrl_frame(ALL, p);
        Some(self.ctrl_hi(vec![f], LADDER[self.cfg.beacon_level].wave, ASIDE))
    }

    /// "Looking for a net", now and then, while we hear no beacon: a member
    /// that hears it tells NCS, and becomes our relay.
    fn seek(&mut self, now: f64) -> Option<Outgoing> {
        if self.my_slot.is_some() || self.cfg.seek_after_s <= 0.0 {
            return None;
        }
        let since = self.last_beacon_at.or(self.started_at)?;
        if now - since < self.cfg.seek_after_s
            || now < self.next_seek_at
            || now - self.last_rx_at < self.cfg.seek_quiet_s
        {
            return None;
        }
        if self.cfg.net.is_empty() {
            // No name: the net whose traffic we hear, if any.
            self.id = self.heard_net?;
        }
        self.next_seek_at = now + self.cfg.seek_every_s * (0.5 + self.rng.uniform() as f64);
        let mut p = vec![C_SEEK];
        p.extend(pack_call(&self.cfg.call));
        p.extend([self.cfg.caps.to_byte(), PROTOCOL, self.cfg.version]);
        let f = ctrl_frame(ALL, p);
        Some(self.ctrl_hi(vec![f], LADDER[self.cfg.beacon_level].wave, ASIDE))
    }

    pub(super) fn member_rx(&mut self, inc: &Incoming, frames: &[Option<Frame>], now: f64) {
        if inc.code == TINY {
            let kind = user_hi(inc.user);
            let e = inc.extra;
            let slot = (e & 63) as u8;
            match kind {
                // A leaf hears its relay, not NCS.
                K_POLL | K_POLLACK | K_ACK | K_RELAY | K_CLOSE if self.parent.is_none() => {
                    self.snr_ncs = Some(smooth(self.snr_ncs, inc.snr_db))
                }
                K_SUBPOLL | K_SUBACK if Some(slot) == self.my_slot => {
                    self.snr_ncs = Some(smooth(self.snr_ncs, inc.snr_db))
                }
                K_SUBPOLL | K_SUBACK => {
                    if let Some(&r) = self.parents.get(&slot) {
                        self.note_heard(r, inc.snr_db, now);
                    }
                }
                K_IDLE => self.note_heard(slot, inc.snr_db, now),
                _ => {}
            }
            match kind {
                K_IDLE => {
                    if let Some(Turn {
                        stage: Some(Stage::Wait(c)),
                        ..
                    }) = self.turn
                        && c == slot
                    {
                        // Our leaf has nothing to send.
                        if let Some(k) = self.kids.get_mut(&c) {
                            k.confirmed = true;
                            k.missed = 0;
                            k.snr_down = Some(snr_from_q((e >> 6 & 63) as u8));
                        }
                        self.kid_status.push((c, true));
                        let t = self.turn.as_mut().unwrap();
                        t.stage = None;
                        t.deadline = None;
                    }
                }
                K_SUBPOLL => {
                    self.talker = Some(slot);
                    if Some(slot) == self.my_slot {
                        // Our relay polls us, as NCS would.
                        self.cap = (e >> 6 & 15) as usize;
                        self.burst_id = (e >> 10 & 3) as u8;
                        self.relay_src = None;
                        if !self.last_sent.is_empty() {
                            self.on_ack(0, now, false);
                            self.outbox.stale();
                        }
                        self.reply = Some(self.uplink_burst(now));
                    }
                }
                K_SUBACK => {
                    self.talker = self.parents.get(&slot).copied();
                    if Some(slot) == self.my_slot && (e >> 6 & 3) as u8 == self.burst_id {
                        self.on_ack((e >> 8) as u8 & 63, now, true);
                    }
                }
                K_POLL | K_RELAY => {
                    self.talker = Some(slot);
                    if Some(slot) == self.my_slot && self.turn.is_none() {
                        if kind == K_POLL && self.parent.is_some() {
                            // NCS polls us itself: it hears us and took us off
                            // our relay (we missed that announcement). Answer
                            // it directly, not as a leaf.
                            self.set_parent(slot, ALL);
                        }
                        if kind == K_POLL {
                            self.cap = (e >> 6 & 15) as usize;
                            self.burst_id = (e >> 10 & 3) as u8;
                            self.relay_src = None;
                        } else {
                            self.cap = (e >> 12 & 15) as usize;
                            self.relay_src = Some((e >> 6 & 63) as u8);
                        }
                        if !self.last_sent.is_empty() {
                            // Polled again: NCS never got our last burst.
                            self.on_ack(0, now, false);
                            self.outbox.stale();
                        }
                        self.start_turn(now);
                    }
                }
                K_POLLACK | K_ACK => {
                    let burst = (e >> 6 & 3) as u8;
                    let bitmap = (e >> 8) as u8 & 63;
                    if Some(slot) == self.my_slot {
                        if burst == self.burst_id {
                            self.on_ack(bitmap, now, true);
                        }
                        if kind == K_POLLACK {
                            self.talker = Some(slot);
                            self.reply = Some(self.uplink_burst(now));
                        } else {
                            self.relay_src = None;
                        }
                    }
                    self.overheard_ack(slot, bitmap);
                }
                K_CLOSE if !self.closed => {
                    self.closed = true;
                    self.events.push(NetEvent::Closed);
                }
                _ => {}
            }
            return;
        }
        // Who sent this burst: NCS (beacon, acks, lists, bulletins) or the
        // member that had the channel.
        let aside = user_hi(inc.user) & ASIDE != 0;
        let ctrl_is = |f: &Frame, c: &[u8]| f.flags >> 6 == T_CTRL && f.payload.first().is_some_and(|x| c.contains(x));
        let from_ncs = !aside
            && frames
                .iter()
                .flatten()
                .any(|f| f.flags >> 6 == T_DOWN || ctrl_is(f, &[C_BEACON, C_ACKS, C_RXLIST, C_TREE]));
        // A relay talking to its leaves.
        let from_relay = aside
            && frames
                .iter()
                .flatten()
                .any(|f| f.flags >> 6 == T_DOWN || ctrl_is(f, &[C_CHILD]));
        let asking = frames.iter().flatten().any(|f| ctrl_is(f, &[C_CHECKIN, C_SEEK]));
        if from_ncs && self.parent.is_none() || from_relay && self.parent.is_some() {
            self.snr_ncs = Some(smooth(self.snr_ncs, inc.snr_db));
        } else if !from_ncs
            && !asking
            && let Some(t) = self.talker
        {
            self.note_heard(t, inc.snr_db, now);
        }
        // Frame by frame, the uplink pieces in it (for matching NCS's ACK).
        let mut pieces: Vec<Option<Piece>> = Vec::new();
        for f in frames {
            let Some(f) = f else {
                pieces.push(None);
                continue;
            };
            let ty = f.flags >> 6;
            let slot = f.flags & 63;
            let mut piece = None;
            match ty {
                T_CTRL => self.member_ctrl(f, inc, now),
                T_DOWN => {
                    if slot == ALL {
                        self.bul_piece(f);
                    } else if !aside && let Some(k) = self.kids.get_mut(&slot) {
                        // NCS's message for our leaf: we pass it on.
                        k.dir_in.accept(f.field, &f.payload);
                    } else if Some(slot) == self.my_slot && self.dir_in.accept(f.field, &f.payload) {
                        for data in self.dir_in.split(self.cfg.compress) {
                            let from = self.ncs_call.clone().unwrap_or_default();
                            self.events.push(NetEvent::Message {
                                from,
                                to: Some(self.cfg.call.clone()),
                                data,
                                overheard: false,
                            });
                        }
                    }
                }
                T_UP if Some(slot) != self.my_slot => {
                    let mi = self.overheard_up.entry(slot).or_default();
                    if let Some(seq) = widen8((f.field >> 16) as u8, mi.next) {
                        piece = Some((slot, seq, (f.field & 0xFFFF) as usize, f.payload.len()));
                    }
                    if let Some((_, data)) = mi.accept(f.field, &f.payload) {
                        let from = self.calls.get(&slot).cloned().unwrap_or_else(|| format!("#{slot}"));
                        self.events.push(NetEvent::Message {
                            from,
                            to: self.ncs_call.clone(),
                            data,
                            overheard: true,
                        });
                    }
                }
                _ => {}
            }
            pieces.push(piece);
        }
        if !from_ncs
            && !aside
            && let Some(t) = self.talker
        {
            self.overheard_last.insert(t, pieces);
        }
        // Our leaf's answer: acknowledge it, and on to the next.
        if let Some(Turn {
            stage: Some(Stage::Wait(c)),
            ..
        }) = self.turn
            && aside
            && !asking
            && frames.iter().all(|f| f.as_ref().is_none_or(|f| f.flags & 63 == c))
        {
            let burst = user_hi(inc.user) & 3;
            let bitmap = frames
                .iter()
                .take(6)
                .enumerate()
                .fold(0u16, |a, (i, f)| a | (f.is_some() as u16) << i);
            let mut wave = LADDER[FLOOR].wave;
            if let Some(k) = self.kids.get_mut(&c) {
                k.confirmed = true;
                k.missed = 0;
                k.burst = (burst + 1) & 3;
                wave = LADDER[k.snr_down.map_or(FLOOR, |s| level_of(&self.cfg, s))].wave;
            }
            self.kid_status.push((c, true));
            let t = self.turn.as_mut().unwrap();
            t.stage = None;
            t.deadline = None;
            let extra = c as u16 & 63 | (burst as u16) << 6 | bitmap << 8;
            self.reply = Some(self.tiny(K_SUBACK, extra, wave));
            self.talker = self.my_slot;
        }
    }

    fn member_ctrl(&mut self, f: &Frame, inc: &Incoming, now: f64) {
        let p = &f.payload;
        match p.first().copied() {
            Some(C_BEACON) if p.len() >= 14 => {
                let ncs = unpack_call(&p[1..6]);
                let name = String::from_utf8_lossy(&p[6..12]).trim().to_string();
                let open = p[12] != 0;
                let slots = p[13].max(1);
                if self.ncs_call.is_none() {
                    // A member with no net name joins this one.
                    self.id = user_session(inc.user);
                    self.net_name = name.clone();
                }
                self.ncs_call = Some(ncs);
                self.last_beacon_at = Some(now);
                if open && self.my_slot.is_none() && !self.closed {
                    let k = (self.rng.uniform() * slots as f32) as u32 % slots as u32;
                    // The window's timing, when the beacon carries it and we
                    // know where the beacon started: our burst starts on
                    // NCS's slot, our lead-in before it.
                    let timing = (p.len() >= 18).then(|| {
                        let ms = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]) as f64 / 1000.0;
                        (ms(14), ms(16))
                    });
                    self.checkin_at = Some(match (timing, self.rx_air) {
                        (Some((first, len)), Some((start, _))) => {
                            (start + first + k as f64 * len - self.lead_s).max(now)
                        }
                        _ => now + self.cfg.turnaround_s + k as f64 * self.slot_len(),
                    });
                    // Chorus slots after the ordinary ones: a station that
                    // can, checks in there, in a random sub-band of a random
                    // chorus slot; every other try it uses an ordinary slot,
                    // in case it is too weak for a chorus (which needs a few
                    // dB more than an R0 check-in).
                    self.chorus_at = None;
                    self.checkin_tries += 1;
                    if self.cfg.caps.chorus
                        && self.checkin_tries % 2 == 1
                        && p.len() >= 21
                        && p[18] > 0
                        && let (Some((first, len)), Some((start, _))) = (timing, self.rx_air)
                    {
                        let c_len = u16::from_le_bytes([p[19], p[20]]) as f64 / 1000.0;
                        let subs = CHORUS_FMT.subbands();
                        let cells = p[18] as usize * subs;
                        let cell = (self.rng.uniform() * cells as f32) as usize % cells;
                        let at = start + first + p[13] as f64 * len + (cell / subs) as f64 * c_len;
                        self.chorus_at = Some(((at - self.lead_s).max(now), cell % subs));
                        self.checkin_at = None;
                    }
                }
            }
            Some(C_ACKS) => {
                for e in p[1..].as_chunks::<6>().0 {
                    let slot = e[0];
                    let call = unpack_call(&e[1..6]);
                    let new = self.calls.get(&slot) != Some(&call);
                    self.calls.insert(slot, call.clone());
                    self.overheard_checkins.retain(|a| a.call != call);
                    if call == self.cfg.call && self.my_slot != Some(slot) {
                        self.my_slot = Some(slot);
                        self.checkin_at = None;
                        let ncs = self.ncs_call.clone().unwrap_or_default();
                        let net = self.net_name.clone();
                        self.events.push(NetEvent::Joined { ncs, net, slot });
                    }
                    if new {
                        self.events.push(NetEvent::CheckedIn { call, slot, via: None });
                    }
                }
            }
            Some(C_CHECKIN) if p.len() >= 7 => {
                // Another station checking in: if NCS never acks it, we can
                // vouch for it.
                let call = unpack_call(&p[1..6]);
                let a = Applicant {
                    call,
                    snr: inc.snr_db,
                    caps: p.get(7).map_or(Caps::default(), |&b| Caps::from_byte(b)),
                    version: p.get(9).copied().unwrap_or(0),
                };
                self.note_applicant(a);
            }
            Some(C_SEEK) if p.len() >= 6 => {
                // A station that cannot hear NCS: we can bring it in.
                let a = Applicant {
                    call: unpack_call(&p[1..6]),
                    snr: inc.snr_db,
                    caps: p.get(6).map_or(Caps::default(), |&b| Caps::from_byte(b)),
                    version: p.get(8).copied().unwrap_or(0),
                };
                self.note_applicant(a);
            }
            Some(C_REPORT) if p.len() >= 12 => {
                // Our leaf's report: kept to forward.
                if let Some(k) = self.kids.get_mut(&p[1]) {
                    k.snr_down = Some(snr_from_q(p[2]));
                    k.bul = Some((u16::from_le_bytes([p[6], p[7]]), p[8]));
                    k.dir = k.dir.max(u24(&p[9..12]));
                    k.report = Some(p.clone());
                    k.report_fwd = false;
                }
            }
            Some(C_TREE) => {
                for e in p[1..].as_chunks::<2>().0 {
                    self.set_parent(e[0], e[1]);
                }
            }
            Some(C_CHILD) if p.len() >= 18 => {
                let (c, r) = (p[1], p[7]);
                let call = unpack_call(&p[2..7]);
                self.calls.insert(c, call.clone());
                self.calls.insert(r, unpack_call(&p[8..13]));
                if call == self.cfg.call && self.my_slot != Some(c) {
                    // We are in, through this relay.
                    self.my_slot = Some(c);
                    self.checkin_at = None;
                    self.id = user_session(inc.user);
                    let ncs = unpack_call(&p[13..18]);
                    self.ncs_call = Some(ncs.clone());
                    let net = self.net_name.clone();
                    self.events.push(NetEvent::Joined { ncs, net, slot: c });
                }
                self.set_parent(c, r);
            }
            Some(C_RXLIST) => {
                for e in p[1..].as_chunks::<3>().0 {
                    let slot = e[0] & 63;
                    let bits: Vec<u32> = (0..8).filter(|i| e[2] >> i & 1 == 1).map(|i| i + 1).collect();
                    if Some(slot) == self.my_slot {
                        if let Some(next) = widen8(e[1], self.outbox.next_seq) {
                            let also: Vec<u32> = bits.iter().map(|b| next + b).collect();
                            self.outbox.held(next, &also);
                        }
                    } else if slot == ALL {
                        // The bulletin's blocks so far: any we never heard
                        // a piece of, we lack whole.
                        self.bul_known = self.bul_known.max(u16::from_le_bytes([e[1], e[2]]));
                    } else {
                        let near = self.overheard_up.get(&slot).map_or(0, |m| m.next);
                        let h = self.held.entry(slot).or_default();
                        if let Some(next) = widen8(e[1], near.max(h.next)) {
                            h.next = h.next.max(next);
                            h.also = bits.iter().map(|b| next + b).collect();
                            let n = h.next;
                            h.acked.retain(|&s, _| s >= n);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn note_applicant(&mut self, a: Applicant) {
        if a.call == self.cfg.call || self.calls.values().any(|c| *c == a.call) {
            return;
        }
        match self.overheard_checkins.iter_mut().find(|x| x.call == a.call) {
            Some(x) => *x = a,
            None => self.overheard_checkins.push(a),
        }
    }

    fn note_heard(&mut self, slot: u8, snr: f32, now: f64) {
        if Some(slot) == self.my_slot || slot == ALL {
            return;
        }
        let prev = self.heard.get(&slot).map(|h| h.0);
        self.heard.insert(slot, (smooth(prev, snr), now));
    }

    /// NCS acknowledged a burst we overheard: what we would relay of it has
    /// got through.
    fn overheard_ack(&mut self, slot: u8, bitmap: u8) {
        let Some(last) = self.overheard_last.remove(&slot) else {
            return;
        };
        for (i, p) in last.into_iter().enumerate() {
            if bitmap >> i & 1 == 1
                && let Some((src, seq, off, len)) = p
            {
                self.held
                    .entry(src)
                    .or_default()
                    .acked
                    .entry(seq)
                    .or_default()
                    .push((off, len));
            }
        }
    }

    /// Pieces (message, offset, bytes) of `src`'s traffic that we hold and
    /// NCS, as far as we know, does not.
    fn relay_pieces(&self, src: u8, max: usize, room: usize) -> Vec<(u32, usize, Vec<u8>)> {
        let mut out = Vec::new();
        let Some(mi) = self.overheard_up.get(&src) else {
            return out;
        };
        let held = self.held.get(&src);
        for (&seq, ib) in &mi.msgs {
            if held.is_some_and(|h| h.has(seq)) {
                continue;
            }
            let h = &ib.hist;
            let mut off = 0;
            while off < h.len() {
                if out.len() >= room {
                    return out;
                }
                let len = (h.len() - off).min(max);
                if !held.is_some_and(|x| x.covered(seq, off, len)) {
                    out.push((seq, off, h[off..off + len].to_vec()));
                }
                off += len;
            }
        }
        out
    }

    /// Our answer to a poll: report, traffic (ours, and what NCS asked us
    /// to forward), or a tiny "nothing to send".
    pub(super) fn uplink_burst(&mut self, now: f64) -> Outgoing {
        let slot = self.my_slot.unwrap_or(ALL);
        if now >= self.hold_until {
            self.hold_ceiling = LADDER.len() - 1;
        }
        let level = self.level.min(self.cap).min(self.hold_ceiling);
        self.level = level;
        let size = LADDER[level].frame_bytes();
        let max = Frame::max_payload(size);
        let room = self.room(level, MAX_UP_FRAMES);
        let mut frames: Vec<Frame> = Vec::new();
        let mut sent: Vec<Sent> = Vec::new();
        // Other stations' traffic NCS lacks: our leaves', and that of the
        // station NCS named in its poll.
        let mut srcs: Vec<u8> = self.kids.keys().copied().collect();
        srcs.extend(self.relay_src.filter(|s| !self.kids.contains_key(s)));
        let fwd: Vec<(u8, u32, usize, Vec<u8>)> = srcs
            .iter()
            .flat_map(|&src| {
                self.relay_pieces(src, max, room + 1)
                    .into_iter()
                    .map(move |(q, o, d)| (src, q, o, d))
            })
            .collect();

        // Traffic first when only one frame fits (the floor): the report and
        // overheard check-ins wait for a burst with room to spare.
        let has_traffic = !fwd.is_empty() || self.outbox.pending() > 0;
        let extras = room >= 2 || !has_traffic;
        // The report, when something in it changed.
        let report = self.report_body();
        if extras && (self.acked_report != Some(report.0) || report.1 != self.acked_hears) {
            let mut p = vec![C_REPORT, slot];
            p.extend_from_slice(&report.0[..1]);
            put_u24(&mut p, self.outbox.pending().min(0xFF_FFFF) as u32);
            p.extend_from_slice(&report.0[1..7]);
            // Who we hear, most recent first.
            let mut hs: Vec<(u8, f32, f64)> = self.heard.iter().map(|(&s, &(snr, t))| (s, snr, t)).collect();
            hs.sort_by(|a, b| b.2.total_cmp(&a.2));
            for (s, snr, _) in hs.into_iter().take(4) {
                p.push(s);
                p.push(snr_q(snr));
            }
            frames.push(ctrl_frame(slot, p));
            sent.push(Sent::Report(report.0));
        }
        if extras && !self.overheard_checkins.is_empty() && frames.len() < room {
            let heard: Vec<Applicant> = self.overheard_checkins.iter().take(2).cloned().collect();
            let mut p = vec![C_OVERHEARD];
            for a in &heard {
                p.extend(pack_call(&a.call));
                p.extend([snr_q(a.snr), a.caps.to_byte(), a.version]);
            }
            frames.push(ctrl_frame(slot, p));
            sent.push(Sent::Overheard(heard.into_iter().map(|a| a.call).collect()));
        }
        // Which leaves answered us, and what they reported.
        if extras && !self.kid_status.is_empty() && frames.len() < room {
            let mut p = vec![C_KIDS];
            for (c, ok) in std::mem::take(&mut self.kid_status).into_iter().take(10) {
                let snr = self.heard.get(&c).map_or(0, |h| snr_q(h.0));
                p.extend([c, snr | (ok as u8) << 7]);
            }
            frames.push(ctrl_frame(slot, p));
            sent.push(Sent::Other);
        }
        let reports: Vec<(u8, Vec<u8>)> = self
            .kids
            .iter()
            .filter(|(_, k)| !k.report_fwd)
            .filter_map(|(&c, k)| Some((c, k.report.clone()?)))
            .collect();
        for (c, r) in reports {
            if !extras || frames.len() >= room {
                break;
            }
            frames.push(ctrl_frame(slot, r));
            sent.push(Sent::KidReport(c));
        }
        // Others' traffic (older than ours: first).
        let space = room.saturating_sub(frames.len());
        let relay_left = fwd.len() > space;
        for (src, seq, off, data) in fwd.into_iter().take(space) {
            let mut f = Frame::new(Kind::Data, msg_field(seq, off));
            f.flags = T_UP << 6 | (src & 63);
            sent.push(Sent::Relay {
                src,
                seq,
                off,
                len: data.len(),
            });
            f.payload = data;
            frames.push(f);
            self.stats.relayed_frames += 1;
        }
        let space = room.saturating_sub(frames.len());
        for (seq, off, data) in self.outbox.take(max, space) {
            let mut f = Frame::new(Kind::Data, msg_field(seq, off));
            f.flags = T_UP << 6 | (slot & 63);
            f.payload = data;
            frames.push(f);
            sent.push(Sent::Data { seq, off });
        }
        if frames.is_empty() {
            // Nothing to say: a header-only answer, with how we hear NCS.
            self.last_sent.clear();
            let wave = LADDER[self.cap.min(self.hold_ceiling)].wave;
            let extra = slot as u16 & 63 | (snr_q(self.snr_ncs.unwrap_or(0.0)) as u16) << 6;
            return self.tiny(K_IDLE, extra, wave);
        }
        self.last_sent = sent;
        let more = relay_left || self.outbox.has_new();
        // A leaf answers its relay, not NCS.
        let aside = if self.parent.is_some() { ASIDE } else { 0 };
        let hi = self.burst_id & 3 | (more as u8) << 2 | aside;
        Outgoing {
            td_ease: false,
            cue: false,
            wave: LADDER[level].wave,
            code: level,
            user: user_bits(level, self.id, hi),
            extra: 0,
            frames: frames.iter().map(|f| f.encode(size)).collect(),
            spans: Vec::new(),
        }
    }

    /// (snr of NCS, bulletin block and shortfall, direct delivered) as sent
    /// in a report, and the hears list.
    fn report_body(&self) -> ([u8; 7], BTreeMap<u8, f32>) {
        let mut r = [0u8; 7];
        r[0] = snr_q(self.snr_ncs.unwrap_or(0.0));
        let (b, short) = self.bul_status();
        r[1..3].copy_from_slice(&b.to_le_bytes());
        r[3] = short;
        r[4..7].copy_from_slice(&(self.dir_in.delivered & 0xFF_FFFF).to_le_bytes()[..3]);
        // The hears list counts as changed when an SNR moves 3 dB.
        let hears: BTreeMap<u8, f32> = self
            .heard
            .iter()
            .map(|(&s, &(snr, _))| {
                let old = self.acked_hears.get(&s).copied();
                (
                    s,
                    match old {
                        Some(o) if (o - snr).abs() < 3.0 => o,
                        _ => snr,
                    },
                )
            })
            .collect();
        (r, hears)
    }

    fn on_ack(&mut self, bitmap: u8, now: f64, advance: bool) {
        let sent = std::mem::take(&mut self.last_sent);
        if sent.is_empty() {
            return;
        }
        let mut got = 0;
        for (i, s) in sent.iter().enumerate() {
            if bitmap >> i & 1 == 0 {
                continue;
            }
            got += 1;
            match s {
                Sent::Data { seq, off } => self.outbox.ack(*seq, *off),
                Sent::Report(r) => {
                    self.acked_report = Some(*r);
                    self.acked_hears = self.report_body().1;
                }
                Sent::Overheard(calls) => self.overheard_checkins.retain(|a| !calls.contains(&a.call)),
                Sent::Relay { src, seq, off, len } => {
                    self.held
                        .entry(*src)
                        .or_default()
                        .acked
                        .entry(*seq)
                        .or_default()
                        .push((*off, *len));
                }
                Sent::KidReport(c) => {
                    if let Some(k) = self.kids.get_mut(c) {
                        k.report_fwd = true;
                    }
                }
                Sent::Other => {}
            }
        }
        // Gear shifting, as on a link: climb on clean ACKs, drop on losses,
        // and keep off a level that lost everything for a while.
        let loss = 1.0 - got as f32 / sent.len() as f32;
        let at = self.level;
        self.level = if loss > 0.99 {
            self.hold_ceiling = at.saturating_sub(1);
            self.hold_until = now + HOLD_OFF_S;
            at.saturating_sub(2)
        } else if loss > 0.3 {
            at.saturating_sub(1)
        } else if loss == 0.0 {
            (at + 1).min(self.cap)
        } else {
            at
        };
        if advance {
            self.burst_id = (self.burst_id + 1) & 3;
        }
    }
}
