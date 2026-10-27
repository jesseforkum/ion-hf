//! Cleaning the send queue (a host's `CLEANTXBUFFER`, Data2G's `docs/arq.md`
//! §9): bytes never on air are dropped at once; bytes on air but not yet
//! acknowledged (the limbo) keep going until they are, since the receiver
//! delivers one unbroken stream and nothing on the air can tell it to skip
//! a gap. Offsets are host offsets: the application's bytes since the
//! link came up, which a cut never renumbers.

use super::{State, Station};

/// What [`Station::clean`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clean {
    /// Nothing unacknowledged.
    Empty,
    /// No link, or a compressed one (its records are coded against each
    /// other: one cannot be dropped).
    Failed,
    /// A byte at or past the offset asked for is on air: nothing dropped.
    Late,
    /// Host offsets `[from, to)` dropped, and the limbo `[from, to)`
    /// (empty at the drop's start when nothing is on air unacknowledged).
    Done { drop: (u64, u64), limbo: (u64, u64) },
}

/// A limbo resolved: host offsets `from`, `split`, `to`. Before `split`
/// delivered (acknowledged); from it, with `unknown`, on air but never
/// acknowledged before the link ended (the peer may have it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    pub from: u64,
    pub split: u64,
    pub to: u64,
    pub unknown: bool,
}

/// Stream positions `[from, to)` on air, unacknowledged at the clean, and
/// the host offset of `from` (no cut falls inside it).
#[derive(Clone, Copy, Debug)]
pub(super) struct Limbo {
    from: u64,
    to: u64,
    host_from: u64,
}

impl Limbo {
    fn host(&self, pos: u64) -> u64 {
        self.host_from + pos.clamp(self.from, self.to) - self.from
    }
}

impl Station {
    /// Stream position of the first byte never on air.
    fn sent_pos(&self) -> u64 {
        self.stream_total - self.out.len() as u64
    }

    /// Stream position of each piece in flight, with its length.
    fn inflight_pos(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        let sent = self.sent_pos();
        self.inflight
            .iter()
            .map(move |(&k, v)| (sent - self.next_offset.wrapping_sub(k) as u64, v.len()))
    }

    /// Stream position of the first unacknowledged byte.
    fn unacked_pos(&self) -> u64 {
        self.inflight_pos()
            .map(|p| p.0)
            .min()
            .unwrap_or_else(|| self.sent_pos())
    }

    /// Drops what has never been on air (`upto`: only host offsets before
    /// it; bytes from it on are kept, and [`Clean::Late`] if one of them is
    /// on air). What is on air unacknowledged becomes the limbo, unless one
    /// is still unsettled (then that one is reported again); it settles
    /// once acknowledged ([`Station::take_settled`]). `vara`: the limbo
    /// counts as gone for [`Clean::Empty`], as VARA has it.
    pub fn clean(&mut self, upto: Option<u64>, vara: bool) -> Clean {
        let unacked = if vara {
            self.pending() - self.limbo_bytes()
        } else {
            self.pending()
        };
        if unacked == 0 {
            return Clean::Empty;
        }
        if self.state != State::Connected || self.compressing() {
            return Clean::Failed;
        }
        let sent = self.sent_pos();
        let drop_from = sent + self.cut;
        let written = self.stream_total + self.cut;
        if upto.is_some_and(|n| n < drop_from) {
            return Clean::Late;
        }
        let drop_to = upto.map_or(written, |n| n.min(written));
        let k = drop_to - drop_from;
        self.out.drain(..k as usize);
        self.stream_total -= k;
        for s in &mut self.sends {
            if s.0 > sent {
                s.0 = s.0.saturating_sub(k).max(sent);
            }
        }
        if self.limbo.is_none() {
            let from = self.unacked_pos();
            if from < sent {
                self.limbo = Some(Limbo {
                    from,
                    to: sent,
                    host_from: from + self.cut,
                });
            }
        }
        self.cut += k;
        let limbo = self
            .limbo
            .map_or((drop_from, drop_from), |l| (l.host_from, l.host(l.to)));
        Clean::Done {
            drop: (drop_from, drop_to),
            limbo,
        }
    }

    /// Bytes of the limbo still unacknowledged.
    pub fn limbo_bytes(&self) -> usize {
        let Some(l) = self.limbo else {
            return 0;
        };
        self.inflight_pos()
            .filter(|&(p, _)| p >= l.from && p < l.to)
            .map(|(_, n)| n)
            .sum()
    }

    /// The limbo, if one is unsettled: host offsets from, the first byte
    /// not yet acknowledged, to.
    pub fn limbo(&self) -> Option<(u64, u64, u64)> {
        self.limbo
            .map(|l| (l.host_from, l.host(self.unacked_pos()), l.host(l.to)))
    }

    /// A limbo resolved since the last call.
    pub fn take_settled(&mut self) -> Option<Settled> {
        self.settled.take()
    }

    /// After an ACK: the limbo settles once all of it is acknowledged.
    pub(super) fn check_limbo(&mut self) {
        if let Some(l) = self.limbo
            && self.unacked_pos() >= l.to
        {
            let to = l.host(l.to);
            self.limbo = None;
            self.settled = Some(Settled {
                from: l.host_from,
                split: to,
                to,
                unknown: false,
            });
        }
    }

    /// The link is ending: an unsettled limbo goes as [`Settled`] with
    /// `unknown`, split at the first byte never acknowledged. Call before
    /// what is in flight is dropped.
    pub fn end_limbo(&mut self) {
        if let Some(l) = self.limbo.take() {
            self.settled = Some(Settled {
                from: l.host_from,
                split: l.host(self.unacked_pos()),
                to: l.host(l.to),
                unknown: true,
            });
        }
    }
}
