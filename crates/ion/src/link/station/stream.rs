//! The stream gear on the link (`docs/link.md`, Stream gear): the sending
//! side streams a block of its queue as outer-coded periods, the receiving
//! side answers with tones; a control exchange opens and closes each block.

use super::{Awaiting, CONTROL, Incoming, Outgoing, Role, State, Station, user_bits};
use crate::level::{FLOOR_WAVE, Guard, Wave};
use crate::link::frame::{Frame, Kind, caps, flags};
use crate::link::stream::{
    self, Ctl, Fam, Listen, Msg, OPTS, Rule, SLOT_S, STREAM, Stat, TONES, Watch, code, decode_codeword, decode_frame,
};

/// After a block the sender gave up on, or a start the peer never
/// answered, no new block for this long.
const HOLD_S: f64 = 300.0;
/// No tone at all for this long and the block well over its pieces: the
/// receiver is gone, or cannot answer.
const SILENT_S: f64 = 240.0;

/// Why the sender ended a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EndWhy {
    Done,
    GiveUp,
    /// A CW ID falls due: the block goes on after it.
    Id,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum TxPhase {
    /// Start sent, waiting for Go.
    Starting,
    /// Period `k` goes at `t0 + k · SLOT_S`.
    Running { t0: f64 },
    /// End sent, waiting for the Report.
    Ending(EndWhy),
}

/// The sending side of a block.
#[derive(Clone, Debug)]
pub(super) struct StreamTx {
    pub id: u8,
    pub offset: u32,
    pub block: Vec<u8>,
    pub crc: u32,
    enc: code::Encoder,
    pub phase: TxPhase,
    pub opt: usize,
    /// The next period, and the next piece index.
    pub k: u32,
    next: usize,
    pub pieces_sent: usize,
    pub last_tone: f64,
    /// The last period's end (we listen from there).
    pub pause_from: Option<f64>,
    /// After an ID gate: start again once this time has passed.
    pub resume_at: f64,
    /// DONE heard: the End goes after our turnaround.
    done_heard: bool,
}

/// The receiving side of a block.
#[derive(Clone, Debug)]
pub(super) struct StreamRx {
    pub id: u8,
    pub offset: u32,
    pub crc: u32,
    pub dec: code::Decoder,
    rule: Rule,
    /// The option we believe the next period is in, and the other one it
    /// may be in (we asked to change and do not know if the sender heard).
    pub opt: usize,
    alt: Option<usize>,
    /// The block is decoded and delivered.
    pub done: bool,
    /// Periods taken in, and whether the last gave a piece.
    pub seen: u32,
    pub last_ok: bool,
    /// Streaming now (a control burst from the sender ends it; a Start
    /// for the same block resumes it).
    pub live: bool,
    /// Counts the Starts heard (each one restarts the timekeeping).
    pub epoch: u32,
    pub snr_db: f32,
    /// An answer that could not go in its own pause.
    queued: Option<Msg>,
}

/// For the operator: what the stream is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamStatus {
    /// In stream mode now.
    pub on: bool,
    pub sending: bool,
    /// The option of the period going out (sending) or expected (receiving).
    pub opt: Option<usize>,
    /// Periods so far in this block.
    pub period: u32,
    /// Receiving: pieces held and needed.
    pub have: usize,
    pub need: usize,
    /// Every tone answer sent or heard so far: (ours, message), oldest
    /// first, with a counter so a poller sees new ones.
    pub tones: u32,
    pub last_tone: Option<(bool, Msg)>,
}

impl Station {
    fn ctl(&self, c: Ctl, wave: Wave) -> Outgoing {
        let mut f = Frame::new(Kind::Stream, self.session as u32);
        f.payload = c.to_bytes();
        if self.role == Role::Receiver {
            if self.has_data() {
                f.flags |= flags::WANT_TURN;
            }
            if self.want_disc {
                f.flags |= flags::WANT_DISC;
            }
        }
        self.control(f, wave)
    }

    /// The stream is in use now (periods and pauses own the channel).
    pub fn streaming(&self) -> bool {
        self.stream_tx
            .as_ref()
            .is_some_and(|t| matches!(t.phase, TxPhase::Running { .. }))
            || self.stream_rx.as_ref().is_some_and(|r| r.live)
    }

    /// For the operator's panel.
    pub fn stream_status(&self) -> StreamStatus {
        let mut s = StreamStatus {
            tones: self.tone_count,
            last_tone: self.last_tone,
            ..Default::default()
        };
        if let Some(t) = &self.stream_tx {
            s.on = matches!(t.phase, TxPhase::Running { .. });
            s.sending = true;
            s.opt = Some(t.opt);
            s.period = t.k;
            s.need = t.enc.k();
        } else if let Some(r) = self.stream_rx.as_ref().filter(|r| r.live) {
            s.on = true;
            s.opt = Some(r.opt);
            s.period = r.seen;
            s.have = r.dec.have().min(r.dec.k());
            s.need = r.dec.k();
        }
        s
    }

    /// Receiving side: what the timekeeping needs to find the next period.
    pub fn stream_listen(&self) -> Option<Listen> {
        let r = self
            .stream_rx
            .as_ref()
            .filter(|r| r.live && self.state == State::Connected)?;
        let mut opts = vec![r.opt];
        opts.extend(r.alt);
        Some(Listen {
            session: self.session,
            id: r.id,
            epoch: r.epoch,
            opts,
            after_tx_s: self.cfg.turnaround_s + self.air.lead_s(),
            seen: r.seen,
            last_ok: r.last_ok,
        })
    }

    /// Sending side: listen for an answer now (a period has just ended).
    pub fn stream_watch(&self) -> Option<Watch> {
        let t = self.stream_tx.as_ref()?;
        if !matches!(t.phase, TxPhase::Running { .. }) {
            return None;
        }
        Some(Watch {
            seed: stream::plan_seed(self.session, t.id),
            from: t.pause_from?,
        })
    }

    /// The plan seed of the answers this station sends or hears.
    pub fn stream_seed(&self) -> Option<u64> {
        let id = self
            .stream_tx
            .as_ref()
            .map(|t| t.id)
            .or(self.stream_rx.as_ref().map(|r| r.id))?;
        Some(stream::plan_seed(self.session, id))
    }

    /// The next CW ID time after `now` on the link's clock, if IDs are kept.
    fn next_id(&self, now: f64) -> Option<f64> {
        let (t0, (every, _)) = (self.link_t0?, self.ids?);
        Some(t0 + (((now - t0) / every).floor() + 1.0).max(1.0) * every)
    }

    /// A block would run into a CW ID within two slots of `t`.
    fn id_near(&self, t: f64) -> bool {
        self.next_id(t).is_some_and(|d| d < t + 2.0 * SLOT_S)
    }

    /// Whether the queue should go as a stream: the peer has the gear,
    /// enough is queued, the SNR is low enough for it to pay, and nothing
    /// else is pending (a turn, a goodbye, an ID).
    pub(super) fn stream_wanted(&self, now: f64) -> bool {
        self.caps & caps::STREAM != 0
            && self.role == Role::Sender
            && self.state == State::Connected
            && self.out.len() >= stream::MIN_BYTES
            && now >= self.stream_hold_until
            && !self.peer_wants_turn
            && !self.peer_wants_disc
            && !self.want_disc
            && self.cfg.pin_level.is_none()
            && self.aim_db.is_some_and(|a| a < stream::BELOW_DB)
            && !self.id_near(now)
    }

    /// Opens a block (or reopens one after an ID): the Start, awaiting Go.
    pub(super) fn stream_start(&mut self, now: f64) -> Option<Outgoing> {
        let resume = self
            .stream_tx
            .as_ref()
            .is_some_and(|t| t.phase == TxPhase::Ending(EndWhy::Id) && now >= t.resume_at);
        if !resume {
            if !self.stream_wanted(now) || !self.inflight.is_empty() || self.stream_tx.is_some() {
                return None;
            }
            let opt = stream::start_pick(self.aim_db.unwrap_or(0.0));
            let n = stream::block_bytes(self.out.len(), opt);
            let block: Vec<u8> = self.out.drain(..n).collect();
            let offset = self.next_offset;
            self.next_offset = self.next_offset.wrapping_add(n as u32);
            self.stream_ids = self.stream_ids.wrapping_add(1);
            self.stream_tx = Some(StreamTx {
                id: self.stream_ids,
                offset,
                crc: code::block_crc(&block),
                enc: code::Encoder::new(&block),
                block,
                phase: TxPhase::Starting,
                opt,
                k: 0,
                next: 0,
                pieces_sent: 0,
                last_tone: now,
                pause_from: None,
                resume_at: 0.0,
                done_heard: false,
            });
        }
        let t = self.stream_tx.as_mut()?;
        t.phase = TxPhase::Starting;
        t.done_heard = false;
        let c = Ctl::Start {
            id: t.id,
            opt: t.opt as u8,
            offset: t.offset & 0xFF_FFFF,
            len: t.block.len() as u32,
            crc: t.crc,
        };
        let b = self.ctl(c, self.cur_wave());
        Some(self.send_awaiting(b, Awaiting::StreamGo))
    }

    /// The block goes back to the ARQ queue, in front.
    fn stream_requeue(&mut self) {
        if let Some(t) = self.stream_tx.take() {
            for &b in t.block.iter().rev() {
                self.out.push_front(b);
            }
            self.next_offset = t.offset;
        }
        self.stream_hold_until = self.last_heard.max(self.last_tx_end) + HOLD_S;
    }

    /// Sending side, while running: the next period when its slot comes,
    /// or the End.
    pub(super) fn stream_poll(&mut self, now: f64) -> Option<Outgoing> {
        let t = self.stream_tx.as_ref()?;
        let TxPhase::Running { t0 } = t.phase else { return None };
        if t.done_heard {
            return Some(self.stream_end(EndWhy::Done, now));
        }
        let at = t0 + t.k as f64 * SLOT_S;
        if now < at {
            return None;
        }
        let k_all = t.enc.k();
        let why = if t.pieces_sent >= stream::GIVE_UP_X * k_all
            || (now - t.last_tone > SILENT_S && t.pieces_sent > 2 * k_all)
        {
            Some(EndWhy::GiveUp)
        } else if self.id_near(now) {
            Some(EndWhy::Id)
        } else {
            None
        };
        if let Some(w) = why {
            return Some(self.stream_end(w, now));
        }
        let t = self.stream_tx.as_mut()?;
        let o = t.opt;
        let per = stream::pieces_per_frame(o);
        let nf = stream::frames(o);
        let mut frames = Vec::with_capacity(nf);
        for _ in 0..nf {
            let pieces: Vec<[u8; code::PIECE]> = (t.next..t.next + per).map(|i| t.enc.piece(i)).collect();
            frames.push(stream::encode_frame(o, t.id, t.next, &pieces));
            t.next += per;
        }
        t.pieces_sent += nf * per;
        t.k += 1;
        t.pause_from = None;
        let (wave, bits) = match OPTS[o].fam {
            Fam::Ofdm => (Wave::Ofdm(Guard::Normal), OPTS[o].n),
            Fam::Mfsk(m, r) => (Wave::Mfsk(m), 5 * crate::fec::nb::n_for(r)),
        };
        let b = Outgoing {
            wave,
            code: STREAM,
            user: stream::period_user(o, self.session),
            extra: o as u16 | (t.id as u16) << 8,
            spans: vec![(0, bits); frames.len()],
            frames,
            cue: false,
        };
        self.stats.stream_periods += 1;
        Some(self.note_tx(b))
    }

    fn stream_end(&mut self, why: EndWhy, now: f64) -> Outgoing {
        let id = self.stream_tx.as_ref().map_or(0, |t| t.id);
        let wave = self.stream_ctl_wave();
        let resume_at = self.next_id(now).map_or(now, |d| d + 1.0);
        if let Some(t) = self.stream_tx.as_mut() {
            t.phase = TxPhase::Ending(why);
            t.resume_at = resume_at;
        }
        let b = self.ctl(Ctl::End { id }, wave);
        self.send_awaiting(b, Awaiting::StreamReport)
    }

    /// Control bursts around a stream go in the waveform of the option in
    /// use: OFDM control frames are heard down to about 0 dB, as QPSK
    /// periods; MFSK R1 below.
    fn stream_ctl_wave(&self) -> Wave {
        let o = self
            .stream_tx
            .as_ref()
            .map(|t| t.opt)
            .or(self.stream_rx.as_ref().map(|r| r.opt));
        match o.map(|o| OPTS[o].fam) {
            Some(Fam::Ofdm) => Wave::Ofdm(Guard::Normal),
            Some(Fam::Mfsk(m, _)) => Wave::Mfsk(m),
            None => FLOOR_WAVE,
        }
    }

    /// A stream control frame (either side).
    pub(super) fn stream_ctl_rx(&mut self, c: Ctl, f: &Frame, inc: &Incoming, now: f64) {
        match (self.role, c) {
            (
                Role::Receiver,
                Ctl::Start {
                    id,
                    opt,
                    offset,
                    len,
                    crc,
                },
            ) => {
                self.last_progress = now;
                let same = self
                    .stream_rx
                    .as_ref()
                    .is_some_and(|r| r.id == id && r.offset == offset && r.crc == crc && r.dec.len() == len as usize);
                if !same {
                    self.stream_rx = Some(StreamRx {
                        id,
                        offset,
                        crc,
                        dec: code::Decoder::new(len as usize),
                        rule: Rule::new(),
                        opt: 0,
                        alt: None,
                        done: false,
                        seen: 0,
                        last_ok: false,
                        live: true,
                        epoch: 0,
                        snr_db: inc.snr_db,
                        queued: None,
                    });
                }
                let mine = stream::start_pick(self.report_snr());
                let r = self.stream_rx.as_mut().unwrap();
                r.live = true;
                r.epoch += 1;
                r.opt = (opt as usize).min(OPTS.len() - 1).min(mine);
                r.alt = None;
                let go = Ctl::Go { id, opt: r.opt as u8 };
                self.last_ack = (0, 0, 0, false);
                self.reply = Some(self.ctl(go, inc.wave));
            }
            (Role::Receiver, Ctl::End { id }) => {
                self.last_progress = now;
                let (done, snr) = match self.stream_rx.as_mut().filter(|r| r.id == id) {
                    Some(r) => {
                        r.live = false;
                        (r.done, r.snr_db)
                    }
                    None => (false, self.report_snr()),
                };
                let rep = Ctl::Report {
                    id,
                    done,
                    delivered: self.delivered & 0xFF_FFFF,
                    snr_db: snr,
                };
                self.reply = Some(self.ctl(rep, inc.wave));
            }
            (Role::Sender, Ctl::Go { id, opt }) if self.awaiting == Awaiting::StreamGo => {
                let Some(t) = self.stream_tx.as_mut().filter(|t| t.id == id) else {
                    return;
                };
                t.opt = (opt as usize).min(OPTS.len() - 1);
                t.phase = TxPhase::Running {
                    t0: now + self.cfg.turnaround_s - t.k as f64 * SLOT_S,
                };
                t.last_tone = now;
                self.clear_wait();
                self.last_progress = now;
            }
            (
                Role::Sender,
                Ctl::Report {
                    id,
                    done,
                    delivered,
                    snr_db,
                },
            ) if self.awaiting == Awaiting::StreamReport => {
                if self.stream_tx.as_ref().is_none_or(|t| t.id != id) {
                    return;
                }
                self.clear_wait();
                self.last_progress = now;
                self.note_turn_ask(f.flags & flags::WANT_TURN != 0, now);
                self.peer_wants_disc = f.flags & flags::WANT_DISC != 0;
                self.aim_db = Some(snr_db);
                let t = self.stream_tx.as_ref().unwrap();
                let end = t.offset.wrapping_add(t.block.len() as u32) & 0xFF_FFFF;
                let got = done && (delivered.wrapping_sub(end) & 0xFF_FFFF) < 0x80_0000;
                let why = match t.phase {
                    TxPhase::Ending(w) => w,
                    _ => EndWhy::GiveUp,
                };
                if got {
                    self.stats.bytes_acked += t.block.len() as u64;
                    self.stats.stream_blocks += 1;
                    self.stream_tx = None;
                } else if why == EndWhy::Id {
                    // Resumes from `poll_tx` once the ID is out.
                } else if why == EndWhy::Done {
                    // A DONE heard that was not: carry on with the block.
                    let t = self.stream_tx.as_mut().unwrap();
                    t.phase = TxPhase::Ending(EndWhy::Id);
                    t.resume_at = now;
                } else {
                    self.stream_requeue();
                }
                // Back to ARQ: our next data burst starts where the link was.
                self.last_tx_end = f64::NEG_INFINITY;
            }
            _ => {}
        }
    }

    /// Sending side, connected: the stream's own bursts, `Some(None)` to
    /// stay quiet (a period's slot has not come, or an ID is not out yet),
    /// `None` to go on with the link.
    pub(super) fn stream_sender_poll(&mut self, now: f64) -> Option<Option<Outgoing>> {
        let t = self.stream_tx.as_ref()?;
        // A handshake whose wait something else cleared (a Turn heard,
        // say): the block goes back to the link.
        let lost = match t.phase {
            TxPhase::Starting => self.awaiting != Awaiting::StreamGo,
            TxPhase::Ending(EndWhy::Id) => false,
            TxPhase::Ending(_) => self.awaiting != Awaiting::StreamReport,
            TxPhase::Running { .. } => false,
        };
        if lost {
            self.stream_requeue();
            return None;
        }
        let t = self.stream_tx.as_ref()?;
        match t.phase {
            TxPhase::Running { .. } => Some(self.stream_poll(now)),
            TxPhase::Ending(EndWhy::Id) if self.awaiting == Awaiting::Nothing => {
                if now >= t.resume_at {
                    Some(self.stream_start(now))
                } else {
                    Some(None)
                }
            }
            _ => None,
        }
    }

    /// The handshake went unanswered.
    pub(super) fn stream_giveup(&mut self) {
        self.stream_requeue();
    }

    /// Stream bursts and control frames, before the link's own handling;
    /// true when done with `inc`.
    pub(super) fn stream_rx_routed(&mut self, inc: &Incoming, frames: &[Option<Frame>], now: f64) -> bool {
        if inc.code == TONES {
            if self.role == Role::Sender
                && let Some(m) = Msg::from_code(inc.extra & 0xFF)
                && self.stream_tx.as_ref().is_some_and(|t| t.id == (inc.extra >> 8) as u8)
            {
                self.stream_tones_rx(m, now);
            }
            return true;
        }
        if inc.code == STREAM {
            if self.role == Role::Receiver {
                self.note_rx_channel(inc);
                self.stream_period_rx(inc, now);
            }
            return true;
        }
        if let Some((f, c)) = frames
            .iter()
            .flatten()
            .filter(|f| f.kind == Kind::Stream)
            .find_map(|f| Ctl::from_bytes(&f.payload).map(|c| (f.clone(), c)))
        {
            if self.state == State::Connected {
                if self.role == Role::Receiver {
                    self.note_rx_channel(inc);
                }
                self.stream_ctl_rx(c, &f, inc, now);
            }
            return true;
        }
        if self.role == Role::Receiver {
            self.stream_rx_interrupted(inc, frames);
        }
        false
    }

    /// Sending side: the tones heard in a pause.
    pub(super) fn stream_tones_rx(&mut self, m: Msg, now: f64) {
        self.tone_count += 1;
        self.last_tone = Some((false, m));
        let Some(t) = self.stream_tx.as_mut() else { return };
        if !matches!(t.phase, TxPhase::Running { .. }) {
            return;
        }
        t.last_tone = now;
        self.last_progress = now;
        match m {
            Msg::Slower => t.opt = stream::down(t.opt).unwrap_or(t.opt),
            Msg::Faster => t.opt = stream::up(t.opt).unwrap_or(t.opt),
            Msg::Done => {
                t.done_heard = true;
                self.tx_at = now + self.cfg.turnaround_s;
            }
        }
    }

    /// Receiving side: a period as the timekeeping heard it.
    pub(super) fn stream_period_rx(&mut self, inc: &Incoming, now: f64) {
        let Some(r) = self.stream_rx.as_mut().filter(|r| r.live) else {
            return;
        };
        if (inc.extra >> 8) as u8 != r.id {
            return;
        }
        let tried_opt = ((inc.extra & 0xFF) as usize).min(OPTS.len() - 1);
        let mut st = Stat {
            opt: tried_opt,
            snr_db: inc.snr_db,
            ..Default::default()
        };
        let mut true_opt = None;
        let mut pieces = Vec::new();
        match OPTS[tried_opt].fam {
            Fam::Ofdm => {
                if let Some(crate::link::harq::Soft::Llrs(llrs)) = &inc.soft {
                    st.heard = true;
                    // The rate the sender used: the one we expected, else
                    // the other one we were unsure of.
                    let mut cands = vec![tried_opt];
                    cands.extend(r.alt.filter(|&a| a != tried_opt && OPTS[a].fam == Fam::Ofdm));
                    for (ci, &o) in cands.iter().enumerate() {
                        let n = OPTS[o].n;
                        let mut s = Stat {
                            opt: o,
                            heard: true,
                            snr_db: inc.snr_db,
                            ..Default::default()
                        };
                        let mut got = Vec::new();
                        for cw in llrs.chunks_exact(n).take(stream::codewords(o)) {
                            let (bytes, blind) = decode_codeword(cw);
                            s.tried += 1;
                            s.ivals.push(blind);
                            if let Some(b) = bytes {
                                let (p, o2) = decode_frame(true, r.id, &b);
                                if !p.is_empty() {
                                    s.ok += 1;
                                    true_opt = true_opt.or(o2);
                                    got.extend(p);
                                }
                            }
                        }
                        if s.ok > 0 || ci + 1 == cands.len() {
                            st = s;
                            pieces = got;
                            break;
                        }
                    }
                } else if !inc.frames.is_empty() {
                    // A model channel hands frames over decoded.
                    st.heard = true;
                    for f in &inc.frames {
                        st.tried += 1;
                        if let Some(b) = f {
                            let (p, o2) = decode_frame(true, r.id, b);
                            if !p.is_empty() {
                                st.ok += 1;
                                true_opt = true_opt.or(o2);
                                pieces.extend(p);
                            }
                        }
                    }
                }
            }
            Fam::Mfsk(..) => {
                st.heard = !inc.frames.is_empty();
                for f in &inc.frames {
                    st.tried += 1;
                    if let Some(b) = f {
                        let (p, _) = decode_frame(false, r.id, b);
                        if !p.is_empty() {
                            st.ok += 1;
                            pieces.extend(p);
                        }
                    }
                }
                if st.heard {
                    true_opt = Some(tried_opt);
                }
            }
        }
        if st.heard {
            self.last_progress = now;
            r.snr_db = inc.snr_db;
        }
        for (i, p) in &pieces {
            r.dec.add(*i, p);
        }
        r.seen += 1;
        r.last_ok = st.ok > 0;
        if let Some(o) = true_opt {
            st.opt = o;
            r.opt = o;
        } else if r.alt.is_some() && st.heard && st.ok == 0 {
            // Neither rate decoded: we cannot tell; keep both in mind.
        }
        r.alt = None;
        r.rule.push(st);
        self.stats.stream_periods += 1;
        if r.dec.complete() && !r.done {
            match r.dec.decode() {
                Some(b) if code::block_crc(&b) == r.crc => {
                    let off = r.offset;
                    r.done = true;
                    self.accept_bytes(off, &b);
                    self.stats.stream_blocks += 1;
                }
                _ => {
                    // A bad piece got through: start the block again.
                    r.dec = code::Decoder::new(r.dec.len());
                }
            }
        }
        let r = self.stream_rx.as_mut().unwrap();
        let m = if r.done {
            Some(Msg::Done)
        } else {
            r.rule.decide().or(r.queued.take())
        };
        match m {
            Some(Msg::Slower) => r.alt = stream::down(r.opt).map(|d| std::mem::replace(&mut r.opt, d)),
            Some(Msg::Faster) => r.alt = stream::up(r.opt).map(|u| std::mem::replace(&mut r.opt, u)),
            _ => {}
        }
        if let Some(m) = m {
            let b = self.tone_burst(m);
            self.reply = Some(b);
            self.reply_by = Some(now + self.cfg.turnaround_s + stream::LATE_S);
        }
    }

    fn tone_burst(&self, m: Msg) -> Outgoing {
        let id = self.stream_rx.as_ref().map_or(0, |r| r.id);
        Outgoing {
            wave: FLOOR_WAVE,
            code: TONES,
            user: user_bits(CONTROL, self.session, 0),
            extra: m.code() | (id as u16) << 8,
            frames: Vec::new(),
            spans: Vec::new(),
            cue: false,
        }
    }

    /// A tone answer handed out too late for its pause: it waits for the
    /// next one.
    pub(super) fn stream_late(&mut self, b: &Outgoing) {
        if let Some(r) = self.stream_rx.as_mut()
            && let Some(m) = Msg::from_code(b.extra & 0xFF)
            && m != Msg::Done
        {
            r.queued = Some(m);
        }
    }

    /// Notes a tone answer we send.
    pub(super) fn stream_note_answer(&mut self, b: &Outgoing) {
        if let Some(m) = Msg::from_code(b.extra & 0xFF) {
            self.tone_count += 1;
            self.last_tone = Some((true, m));
        }
    }

    /// A burst that is not part of the stream ends stream mode at the
    /// receiving side (the sender has gone back to the link).
    pub(super) fn stream_rx_interrupted(&mut self, inc: &Incoming, frames: &[Option<Frame>]) {
        let ctl = frames.iter().flatten().any(|f| f.kind == Kind::Stream);
        if inc.code == STREAM || inc.code == TONES || ctl {
            return;
        }
        if let Some(r) = self.stream_rx.as_mut() {
            r.live = false;
        }
        // ARQ data from the block's offset on: the sender took it back.
        if inc.code < crate::level::LADDER.len() {
            self.stream_rx = None;
        }
    }
}
