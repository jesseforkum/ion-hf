# Ion net mode

A net is one **net control station (NCS)** and a pool of **members** on one
frequency. NCS runs it: it opens check-ins, polls each member in turn,
sends bulletins to everyone, and closes the net. Members transmit only when
polled, in the check-in window after a beacon, or (a station that hears no
beacon at all) with an occasional "looking for a net" call.

Code: `crates/ion/src/link/net.rs` (`NetStation`, `NetConfig`) and its
submodules in `crates/ion/src/link/net/`: `wire.rs` (formats), `stream.rs`
(messages), `ncs.rs`, `member.rs`, `relay.rs`, `bulletin.rs`, `object.rs`
(objects by hash), `prime.rs` (primed receivers, chorus) and `node.rs`
(`NetNode`, which adds the receivers and audio). `link/net_code.rs` is the bulletin erasure code,
`link/net_sim.rs` an N-station simulator on a model channel, and
`crates/ion/src/phy_chorus.rs` the chorus waveform. Tests:
`crates/ion/tests/net.rs` (model channel), `net_audio.rs` (real modulated
audio), `net_prime.rs` (primed reception and chorus check-ins over audio,
with per-path SNR and fading), `apriori.rs`, `slotted.rs` and `chorus.rs`
(the receive side alone).

## Overview

- **Check-in.** NCS sends a beacon (net name, its call, the roster so far)
  followed by a short contention window. Stations that want in send a
  check-in in a random slot of the window; collisions retry after the next
  beacon. NCS answers with the slots it has assigned.
- **Rounds.** NCS polls each member in turn. A polled member sends its
  traffic (a burst of up to six frames, at its own gear-shifted level,
  capped by what NCS hears) or says it has nothing. NCS acknowledges with a
  frame bitmap and either grants another burst or moves on.
- **Received list.** Every message has an ID (originator slot, number). At
  the end of a round NCS says, when it is news, which messages it holds; a
  member that overheard one NCS lacks forwards it when NCS names it in a
  poll.
- **Relay tree.** A station NCS cannot reach (it is only overheard, or it
  hears no beacon and calls "looking for a net") becomes a leaf of a
  willing member, which polls it during its own turn and forwards both
  ways. NCS moves leaves when their relay goes quiet.
- **Bulletins.** NCS sends a bulletin once, at a level the weakest member
  it polls can decode, then repairs it with coded pieces: each round as
  many as the member furthest short lacks.
- **Capabilities** (rig control, bandwidth, relaying, attended, chorus,
  version) come with the check-in and show in the roster.
- **Traffic and the check-in line.** Each member lists its traffic (how
  many items, the most urgent precedence, short time) at check-in and in
  its reports, and sends a free-text line once. NCS polls Emergency, then
  Priority, then short-time members first.
- **Objects by hash.** NCS offers objects (an announcement, say) by their
  SHA-256. A member that already holds one never costs a piece of it; the
  others fetch it with the bulletin's coded repair.

What this gives an ARES/RACES-style net: every check-in and report is
acknowledged; a station NCS cannot hear (or that cannot hear NCS) joins
through a member that can; nothing one member overheard is lost; every
member ends up with an exact copy of each bulletin; and NCS has a live
picture of who is in, what each station can do, how well each hears NCS
and is heard, who hears whom, who relays for whom, and who has gone quiet.

## On the air

Everything rides in ordinary Ion bursts: header-only bursts for the short
messages, short-code frames for control, ladder-level frames for traffic.
The 8-bit session field of the burst header carries the **net id**, a hash
of the net name. A member configured with no net name joins the first net
it hears.

| Burst | Who | What |
|---|---|---|
| header-only, kind 1 **Poll** | NCS | slot, uplink level cap, expected burst id |
| header-only, kind 2 **PollAck** | NCS | ACK bitmap for the member's burst, and another burst granted |
| header-only, kind 3 **Ack** | NCS | ACK bitmap, end of the member's turn |
| header-only, kind 4 **Idle** | member | "nothing to send", and how well it hears NCS (or its relay) |
| header-only, kind 5 **Relay poll** | NCS | a poll that also names the station whose traffic this member should forward: relayer slot, source slot, cap |
| header-only, kind 6 **Close** | NCS | the net is closed (sent 3×) |
| header-only, kind 7 **SubPoll** | relay | polls one of its leaves: slot, cap, burst id |
| header-only, kind 8 **SubAck** | relay | ACK bitmap for its leaf's burst |
| control 1 **Beacon** | NCS | NCS call, net name, open flag, window slots, window timing, chorus slots; roster |
| control 2 **CheckIn** | member | call, SNR it heard the beacon at, caps, protocol, software version, traffic |
| control 3 **Report** | member | how well it hears NCS, bytes still queued, bulletin (or object) block and shortfall, direct bytes held, traffic, objects held, up to 4 (slot, SNR) it hears |
| control 4 **Overheard** | member | up to 2 stations it heard asking in that NCS has not taken: call, SNR, caps, version, traffic |
| control 5 **Acks** | NCS | slot ← call assignments |
| control 6 **RxList** | NCS | the end-of-round received list (below) |
| control 7 **Seek** | lone station | "looking for a net": call, caps, protocol, version, traffic |
| control 8 **Tree** | NCS | (leaf slot, relay slot) pairs, up to 10 per frame; relay 63 = polled directly |
| control 9 **Child** | relay | to its leaf: leaf slot and call, relay slot and call, NCS call |
| control 10 **Kids** | relay | to NCS: per leaf polled this turn, answered (bit 7) and the SNR it is heard at |
| control 11 **Line** | member (or relay) | one part of a member's check-in line: slot, part, text |
| control 12 **Offer** | NCS (or relay) | the objects on offer: index of the first entry and the list's length, then 2 × (id, length) |
| data **Up** | member (or relay) | one message's piece: slot of the *originator*, `field = message number (8 bits) << 16 \| offset in the message` |
| data **Down**, slot 63 | NCS (or relay) | coded bulletin pieces: `field = block << 8 \| first piece number`, payload `[block length u16][20-byte pieces…]`; blocks from 0x8000 are object blocks |
| data **Down**, slot n | NCS (or relay) | member n's direct messages, by byte offset |

- **Control frames** are short-code frames (32 bytes, 22 of payload) in the
  waveform the sender picks.
- **Burst header bit `ASIDE`** (bit 3 of the high nibble of a data or
  control burst) marks bursts that do not answer NCS's poll: check-ins,
  seeks, a relay's bursts to its leaves, and leaves' answers to their
  relay. NCS never takes one of those as the polled member's reply.
- **Callsigns** are packed into 5 bytes: up to 7 characters of A–Z, 0–9
  and /.
- **Messages** are length-prefixed and numbered per originator from 0.
  Each is reassembled on its own and delivered as soon as it is whole, so a
  lost piece holds up only its own message. A message is at most 65 533
  bytes. Its ID (originator slot, number) stays the same however many
  stations forward it, and NCS delivers each ID once.
- **Compression.** A member that can take compressed direct messages says
  so at check-in (caps bit 7); an NCS with `NetConfig::compress` on then
  sends it its direct messages packed with `link::compress` (bit 15 of the
  length prefix marks them). Bulletins and members' traffic go as they are.

### Capability flags

The check-in, seek and overheard entries carry one flags byte and a
version:

| Bit | Meaning |
|---|---|
| 0 | can QSY (has rig control) |
| 1 | 2.75 kHz channel (else 2.4 kHz) |
| 2 | willing to relay |
| 3 | attended |
| 4 | checks in by chorus when offered |
| 5–6 | reserved (QSY sub-nets) |
| 7 | takes compressed direct messages (check-in only) |

The check-in also carries the protocol version (`PROTOCOL` = 3) and the
software version (major and minor, a nibble each). NCS shows all of it in
the roster (`RosterEntry`), and picks only willing stations as relays.

### Payloads

Control payloads are at most 22 bytes. Multi-byte numbers are little
endian; SNRs are dB + 24 in one byte.

```text
CheckIn  [2][call 5][snr][caps][protocol][version][traffic]               11 bytes
Report   [3][slot][snr][queued u24][block u16][short][direct u24]
         [traffic][objects][(slot, snr) × 0–4]                         14–22 bytes
Line     [11][slot | part << 6 | two parts << 7][text, up to 20]          2–22 bytes
Offer    [12][first | total << 4][(hash 8, length u16) × 1–2]           12 or 22 bytes
```

### Traffic and the check-in line

**Traffic byte** (`Traffic`), in the check-in, the report, a seek and an
overheard entry:

| Bits | Meaning |
|---|---|
| 0–2 | items listed (0–7, 7 meaning 7 or more) |
| 3–4 | precedence of the most urgent: 0 Routine, 1 Welfare, 2 Priority, 3 Emergency |
| 5 | short time ("in and out") |
| 6–7 | reserved |

- **Changes:** a member's traffic (`NetConfig::traffic`,
  `NetStation::set_traffic`) goes in its next report. A chorus check-in
  carries none; it comes with the first report.
- **In the roster:** NCS keeps the latest in `RosterEntry::traffic` and
  says `NetEvent::Traffic` when it changes, for leaves too (their reports
  and overheard entries carry it).
- **Polling order:** each round NCS polls the members it reaches directly
  in this order: Emergency, then Priority, then short time, then the rest,
  each group in slot order. A relay goes as early as its most urgent leaf.

**Check-in line** (`NetConfig::line`, `NetStation::set_line`): free text,
at most `MAX_LINE` (40) bytes, cut on a character boundary.

- **On the air:** one or two Line frames of up to 20 bytes. The second
  byte holds the member's slot (bits 0–5), the part (bit 6) and whether
  there are two parts (bit 7), so a relay can forward a leaf's line as it
  is.
- **When:** in the member's first turn with room for it (as with the
  report, traffic goes first at the floor), each part until NCS
  acknowledges it, and again when the line changes. An empty line clears
  it.
- **At NCS:** `RosterEntry::line` and `NetEvent::Line` once every part has
  arrived.

### Objects by hash

An object is opaque bytes, up to `MAX_MSG`, named by its `ObjectId`: the
first 8 bytes of its SHA-256 and its length (20 hex digits, the hash then
the length). SHA-256 is implemented in the crate (`object.rs`, checked
against the FIPS vectors).

- **Offers:** `NetStation::offer(data)` at NCS puts it on the offer list
  (up to `MAX_OFFERS`, 8; when full, the oldest goes); `withdraw(id)` takes
  it off. NCS sends the list in Offer frames (two entries each, an empty
  entry for a free index) in its own burst before the round's bulletin
  pieces, at the level the weakest member it polls can decode. It sends it
  when the list changed, after every beacon (for late joiners), and in any
  round in which a member that has every bulletin block reports that it
  neither holds nor is fetching an offer (it missed the list).
- **The store:** a member keeps received objects in memory, and in
  `NetConfig::store` if set, one file per object named by its id. When it
  hears an offer it holds (in memory, from `NetStation::offer` at a
  member, or in the store) it hands it to the host as
  `NetEvent::Object { cached: true }`. A file whose bytes do not match its
  name is not trusted: the member fetches the object over the air and
  rewrites the file.
- **Objects byte** in the report: bit i says the member holds offer i
  (`RosterEntry::holds`).
- **Fetching:** an object is cut into blocks of the bulletin code (2 560
  bytes, at most 26 blocks), numbered `0x8000 | offer << 5 | block` in the
  bulletin's block space. A member with every bulletin block reports the
  first object block it lacks, and how short of it it is, in the report's
  block and shortfall field. NCS sends an object's pieces only while a
  member it polls reports one of its blocks: all its sources once, for all
  of them, then each round as many repairs as the member furthest short
  needs (a member with none of a block counts as lacking every block from
  there on). Bursts with only object pieces go at the level the weakest
  fetching member can decode. A member that holds an object never reports
  it, so it costs no pieces and does not lower the level.
- **Leaves:** a relay passes the offer list to its leaves (when it changed,
  or the leaf's report says it missed it) and serves a leaf's object blocks
  from its own copy, decoded or from its store; NCS sends nothing for a
  leaf whose relay holds the object.
- **Checked:** once a member has every block it puts the object together
  and checks it against the id. A mismatch is dropped
  (`NetStats::obj_rejected`) and fetched again; a match is stored and
  handed over as `NetEvent::Object { cached: false }`, once per object.

### The received list

At the end of a round, when it has news, NCS sends `RxList` frames (7
entries of 3 bytes each) at the level the weakest member it polls can
decode:

```text
[slot] [first message number NCS lacks, mod 256] [bitmap: which of the 8 after it NCS holds]
[63]   [bulletin blocks, u16]
```

- **News** means a message that arrived through a relay (its originator
  never got an ACK for it), or whole messages past a hole. Traffic NCS
  acknowledged directly needs no list: the sender and everyone who heard
  the ACK already know. Rounds with only direct traffic send no list, so
  the list costs nothing in a healthy net.
- **The originator** drops every message the list shows as held.
- **Other members** update their picture of what NCS holds. They also track
  every ACK they overhear, frame by frame.
- **Relaying:** NCS knows who hears whom from the reports. When a member
  missed its poll, or its messages arrived with a hole, NCS names *one*
  relayer in a poll: the willing member that hears it best. Members keep
  what they overhear of others' bursts. The named relayer appends to its
  own report what it holds of that station's traffic that NCS lacks. If
  nothing comes of it, NCS names the next-best hearer the next round.
  Nobody transmits unpolled, and two members never send the same missing
  message.
- **Pieces that got no answer at all** go to the back of a station's
  queue, so a station NCS cannot hear still gets every piece of its message
  out to the members who will relay it, rather than repeating its first
  piece.

### Bulletins: coded repair

The bulletin stream is cut into **blocks** of up to 128 **pieces** of 20
bytes (2 560 bytes) as it goes out. Piece number `i < k` is source piece
`i`. Piece `i ≥ k` is a repair: the sum over GF(256) of every source `s`
times `1/(i ⊕ s)` (a systematic Cauchy code). Any `k` distinct pieces give
the block back, so one repair fills a *different* hole at every station
(`net_code.rs`, with a test that random `k` of all 256 decode at
`k` = 128).

- A frame holds 1 piece at the floor and 8 at OFDM levels.
- **Each member reports** the first block it lacks and how many pieces
  short of it it is (255: it has none).
- **Each round**, before polling, NCS sends the sources it has not sent
  yet. Then, for each block, it sends as many *new* repair pieces as the
  member furthest short of it needs. A member that holds every block it
  knows of reports the next block number, with nothing short; if that
  block exists, NCS counts the member as lacking all of it.

## How a net runs

1. **Beacon and check-in window.**
   - NCS sends a beacon in the beacon waveform (default: MFSK R0, the
     floor, so anyone who can hear the net at all hears it).
   - A window of `window_slots` (default 4) check-in slots follows, each
     long enough for one check-in burst.
   - A station that wants in picks a slot at random. Two in one slot
     collide; both try again after the next beacon.
   - Beacons come every 5 minutes, or 45 s after a window that had
     check-ins or a busy channel, since a busy channel means more may be
     waiting or have just collided.
   - A primed NCS's beacon also carries the window's timing, and chorus
     slots may follow the ordinary ones (below).
2. **Acks.** After the window, NCS sends the slot assignments, plus the
   relay of any new leaf, in the waveform the weakest new member (or, for a
   leaf, its relay) can hear. A leaf learns its slot from its relay's Child
   frame, even when it can hear NCS at the floor.
3. **Rounds.** NCS polls every member it reaches directly: Emergency,
   Priority and short-time members first, then the rest, in slot order.
   - **Polling:** a poll goes in the fastest waveform the member reports
     hearing NCS in.
   - **The answer:** a member with nothing to say answers with a
     header-only Idle. Otherwise it sends a burst of up to 6 frames, in
     order: its report (when something in it changed), its check-in line
     (until acknowledged), stations it heard asking in, what its leaves
     sent (their reports and lines), any traffic NCS named it to relay,
     then its own. At the floor, where only one frame fits, traffic goes
     first.
   - **Speed:** each member gear-shifts its own uplink as a link does. It
     starts at the floor, climbs a level per clean ACK, drops on losses,
     and stays off a level that lost a whole burst for a minute. NCS caps
     the climb with the poll, from how well it hears the member.
   - **Acknowledgement and turns:** NCS answers each burst with a bitmap.
     PollAck grants another burst while the member says it has more, up to
     4 per turn. Ack ends the turn.
   - **End of the round:** the received list, if it has news. A round in
     which nobody had traffic is followed by a 15 s pause.
4. **Bulletins and objects.** Once per round, before polling, NCS sends
   the offer list when it is due, then the coded pieces above at the level
   the weakest member it polls can decode, then what members are fetching
   of the objects on offer. A message for one member goes just before that
   member's poll (or its relay's), at its level.
5. **Relays** (the tree has two levels: NCS → relay → leaf).
   - **Finding the net:** a station that has heard no beacon for
     `seek_after_s` (default 6 min) sends a Seek about every `seek_every_s`
     (default 90 s, ±50%), once the channel has been quiet for
     `seek_quiet_s` (5 s); in practice that lands in the idle pause between
     rounds or in a check-in window.
   - **Joining through a member:** members report Seeks the same way as
     check-ins only they heard, with the SNR and the caps. NCS adds the
     station as a **leaf** of the reporting member, if that member is
     willing to relay and is itself polled directly, and announces the pair
     in a Tree frame.
   - **The relay's first contact:** the relay sends its leaf a Child frame
     (the leaf's slot, the relay's call, NCS's call). That is how a leaf
     that never hears NCS learns it is in.
   - **A relay's turn:** when NCS polls a relay, the relay first serves
     each leaf in turn: what the leaf lacks of the bulletin (the same coded
     pieces, so anyone else who hears them can use them too) and of its
     direct messages; a SubPoll; on the leaf's answer, a SubAck. Then the
     relay answers NCS with a Kids frame (who answered), the leaves'
     reports, their traffic, and its own. NCS never needs to hear a leaf.
   - **Waiting on a relay:** NCS hears the relay's SubPoll, and waits out a
     whole leaf burst plus the relay's ACK before it gives up on the turn.
   - **Leaves that stop answering** are polled by their relay only every
     fourth turn.
   - **Moving a leaf:** NCS moves it when its relay misses `miss_limit`
     polls in a row, or when the leaf stops answering its relay. The new
     relay is the willing member that hears the leaf best (members hear a
     leaf answering its old relay and report it). NCS announces the move in
     a Tree frame, and the new relay introduces itself with a Child frame.
   - **The roster for a leaf:** "hears you" is how the leaf hears its
     *relay*, and "you hear" is how the relay hears it.
   - **Identification:** every relay identifies with its own call (the
     Child frame) when it takes a leaf on and at least every 10 minutes.
     Forwarded messages keep the originator's slot and message number, so
     NCS logs them under the originator's call ("N0CALL, relayed by N1CALL").
   - **One-way stations:** a station NCS cannot hear that still hears NCS
     becomes a leaf the same way, when its check-in was only overheard.
   - **Acknowledgement:** relays acknowledge hop by hop. A leaf drops a
     message once its relay has it. If the relay then vanishes, a member
     that overheard the leaf can still forward it when NCS names it.
   - **After a miss:** NCS waits out a whole burst before polling again. A
     member NCS cannot hear may still be transmitting, and stations that
     can hear it would get a collision.
6. **Close.** NCS sends Close three times, spaced a burst apart.

## Primed reception: NCS knows what is coming

NCS knows a lot about the bursts it waits for: when they should start,
what their burst header holds, and most of what is in a check-in or a
report. With `NetConfig::prime` (on by default) its receivers use that.
It is receive side only: the waveforms do not change, and a burst outside
an expected slot is received as usual.

**Window timing in the beacon.** The beacon payload carries, after the
window size, where slot 0's burst starts (counted from the start of the
beacon's own preamble) and the slot length, both in ms (u16, little
endian). A member that knows where the beacon started (`NetNode` places
every burst on its clock from the receiver's sample count;
`NetStation::on_rx_at`) keys its check-in so that its preamble starts on
NCS's slot, its own lead-in before it. A member that ignores the timing
still lands close to the slot: NCS puts slot 0 at "beacon end +
turnaround + lead-in", which is where such a check-in starts.

**Check-in slots.** For each slot of the window, NCS's MFSK receiver is
told (`MfskRx::expect`) where the burst should start (±`slot_tol_s`,
60 ms) and its header (check-in code, net id, `ASIDE`, one short frame).
Inside the slot weaker preambles become candidates, and a header that does
not decode is tested against the expected one; it must also beat every
other header by 6 nats, so another net's check-in is not taken for ours.
A check-in frame that does not decode on its own is decoded again with
priors (`link::apriori`): the check-in format fixes 152 of the frame's 256
bits, a known caller 40 more, leaving 64 to the channel (protocol 2, before
the traffic byte, fixed 160 and 200; the tables below were measured then). The priors are
the check-in format, then each member's call, then each of
`NetConfig::regulars` (at most 30). Each pass costs one iterative decode
(a few ms) and still has to pass the frame's CRC-32.

**Polled replies.** When NCS polls a member (or grants it another burst)
it expects the answer a turnaround and a lead-in after its poll ends. For
a member capped at the floor (R0) with no leaves, the header is known but
for a few bits: an Idle (header-only; the 6 bits of how well it hears NCS
unknown) or one short frame (the "more" flag unknown, and the burst number
unless the poll named it). The receiver tests for the mixture of every
allowed header (`ExpectedHeader::user_mask` / `extra_mask`, and a second
header, `Expect::alt`); the margin test then runs over every other `user`
and, for an Idle, every other `extra`, so another member's Idle is not
taken for the polled one's. A one-frame answer that does not decode is
decoded again with the layout of a report from that slot (0–4 stations
heard) as priors (its traffic and objects bytes unknown).

**Calibration.** NCS compares where every decoded check-in and reply
started with its prediction and keeps a running offset (one for
check-ins, one per member for replies, and a net-wide one for members not
heard yet), so PTT, sound-card and TCI latencies only have to be stable,
not known. In the audio simulation the offsets are −14 ms (check-ins) and
−12 to −22 ms (replies) before calibration, and within 10 ms of the
calibrated prediction once the first reply has been heard
(`primed_bursts_land_in_their_slots`).

**Not primed:** OFDM bursts (`Expect` is MFSK only), replies from members
capped at R1 or above, a relay's answer (its leaves go first, so the
timing is unknown), seeks, and the members' own receivers.

**Compatibility.** The beacon's timing and chorus fields come after the
fields a member without priming reads, and the chorus flag is a caps bit
such a member ignores. `PROTOCOL` is 3 since the traffic byte, the line
and objects (the net has not been on the air, so nothing older is
understood); the check-in format prior assumes that version byte.

### Measured (over real modulated audio)

`tests/net_prime.rs` (`net_checkin_table`, `net_poll_table`): every burst
through the Ion PHYs and receivers, each path with its own gain (and
Watterson channel), noise at every receiver, the same seeds with `prime`
off and on. These tables were run with check-ins in the binary short code;
the link now sends them in the GF(32) code with the spread layout, which
[fec.md](fec.md) compares (A priori bits and the layout).

**Check-ins**, one member, one slot, NCS hears it at the given SNR (R0
burst power, dB in 2.4 kHz), it hears NCS at +10 dB; 40 trials per cell;
"prior" is the fraction that decoded only with a prior, "expected header"
the fraction whose burst the receiver took on the expected-header test:

| Channel | SNR dB | blind | primed | prior | expected header | wrong |
|---|---:|---:|---:|---:|---:|---:|
| AWGN | −17 | 0.00 | 0.35 | 0.35 | 0.35 | 0 |
| | −16 | 0.00 | 0.70 | 0.70 | 0.55 | 0 |
| | −15 | 0.00 | 0.90 | 0.90 | 0.40 | 0 |
| | −14 | 0.00 | 0.95 | 0.93 | 0.05 | 0 |
| | −13 | 0.68 | 1.00 | 0.30 | 0.00 | 0 |
| | −12 | 1.00 | 1.00 | 0.00 | 0.00 | 0 |
| CCIR poor | −16 | 0.00 | 0.40 | 0.40 | 0.40 | 0 |
| | −14 | 0.03 | 0.75 | 0.73 | 0.38 | 0 |
| | −13 | 0.15 | 0.88 | 0.68 | 0.17 | 0 |
| | −12 | 0.50 | 0.95 | 0.45 | 0.08 | 0 |
| | −11 | 0.62 | 0.93 | 0.28 | 0.03 | 0 |
| | −10 | 0.85 | 1.00 | 0.10 | 0.05 | 0 |
| | −9 | 0.93 | 0.98 | 0.03 | 0.05 | 0 |
| NVIS disturbed | −14 | 0.00 | 0.70 | 0.70 | 0.38 | 0 |
| | −13 | 0.03 | 0.85 | 0.80 | 0.17 | 0 |
| | −12 | 0.32 | 0.95 | 0.55 | 0.17 | 0 |
| | −11 | 0.47 | 0.98 | 0.47 | 0.08 | 0 |
| | −10 | 0.77 | 1.00 | 0.22 | 0.03 | 0 |
| | −9 | 0.95 | 1.00 | 0.03 | 0.00 | 0 |

The 90% point moves about 2.5 dB on AWGN (−12.5 → −15 dB) and about 3.5 dB
on CCIR poor and NVIS disturbed (−9 → −12.5 dB).

The report prior's gain shrinks a little with protocol 3: the traffic and
objects bytes are 16 more unknown bits (128 known with nothing heard,
against 144).

**Polled turns**, a member with nothing to send at the floor (it answers
with an Idle), polled round after round for 150 s once NCS has heard it at
−8 dB; AWGN; 6 runs per cell; a turn counts when NCS hears its Idle on the
poll or its one retry:

| SNR dB | blind | primed |
|---:|---:|---:|
| −17 | 0.04 (1 of 25) | 0.33 (12 of 36) |
| −16 | 0.13 (4 of 30) | 0.67 (33 of 49) |
| −15 | 0.50 (22 of 44) | 0.79 (44 of 56) |
| −14 | 0.78 (42 of 54) | 0.96 (66 of 69) |
| −13 | 0.97 (70 of 72) | 0.99 (72 of 73) |

About 2 dB. Primed runs also fit more turns in the 150 s: a missed turn
costs a retry and a whole burst of waiting.

**Replies at the PHY** (`slotted_reply_table` in `tests/slotted.rs`: one
Idle or one report frame in its slot, ±50 ms told, ±30 ms real error; 40
per cell): fraction of Idles whose header the receiver delivered, and of
report frames that decoded:

| Channel | SNR dB | Idle: blind | Idle: primed | Report: blind | Report: primed |
|---|---:|---:|---:|---:|---:|
| AWGN | −17 | 0.00 | 0.25 | 0.00 | 0.00 |
| | −16 | 0.30 | 0.77 | 0.00 | 0.17 |
| | −15 | 0.62 | 0.88 | 0.00 | 0.80 |
| | −14 | 0.95 | 1.00 | 0.00 | 0.98 |
| | −13 | 1.00 | 1.00 | 0.70 | 1.00 |
| | −12 | 1.00 | 1.00 | 0.95 | 1.00 |
| CCIR poor | −15 | 0.35 | 0.62 | 0.00 | 0.38 |
| | −13 | 0.68 | 0.85 | 0.17 | 0.75 |
| | −12 | 0.70 | 0.90 | 0.47 | 0.98 |
| | −11 | 0.90 | 0.98 | 0.73 | 1.00 |
| | −10 | 0.88 | 0.95 | 0.90 | 0.98 |
| NVIS disturbed | −14 | 0.35 | 0.70 | 0.00 | 0.38 |
| | −13 | 0.52 | 0.82 | 0.10 | 0.60 |
| | −12 | 0.75 | 0.88 | 0.25 | 0.93 |
| | −11 | 0.90 | 1.00 | 0.55 | 0.98 |
| | −10 | 0.90 | 0.95 | 0.85 | 0.98 |

No reply was taken for the wrong slot. The report gains most, 2–3 dB
(AWGN −12.5 → −15 dB for 90%): its header is easy, its data hard, and the
priors fix about 140 of its 256 bits. The Idle gains 1–2 dB.

**False and cross-net decodes** (`net_false_checkin_table`,
`another_nets_checkin_in_our_slot_is_not_taken`,
`another_members_idle_is_not_ours`): NCS alone, slot and header expected,
100 empty slots: no check-in taken, no burst taken on the expected-header
test. Another net's check-in placed in our slot (±30 ms), 50 each at −17,
−15, −12 and −8 dB: none taken for ours. No wrong check-in in any of the
primed check-in trials of the table above (920 in all, with the rows
not shown). At the PHY, another
member's Idle in the polled member's slot (pass mark dropped to 0, so
every burst reaches the margin test): never taken.

**Timing and CPU.** Priming changes no burst and no timing on the model
channel: the `tests/net.rs` timings are identical with and without it. The
margin test encodes and scores every other header: about 0.5 s of CPU per
pass, 1 s for an Idle (users and extras; `margin_test_cost`). It runs only
when a burst in an expected slot fails to decode blind and passes the
score, so about once per weak reply, but on the receive path.

## Chorus check-ins

A protocol slot in which every station checks in at once, each in its own
sub-band; off by default. With `NetConfig::chorus_slots` > 0 (and
`prime`), the beacon adds three bytes: the number of chorus slots and
their length (ms). They follow the ordinary slots.

**Waveform** (`phy_chorus.rs`): MFSK R0's numerology (28 ms symbols, a
20 ms window and an 8 ms guard, a 50 Hz grid of 48 bins, `center_hz` ±
1175 Hz), cut into sub-bands of tones plus empty guard bins: 5 sub-bands
of 8 tones (400 Hz) or 9 of 4 (200 Hz). Each station sends one tone at a
time, constant envelope, with 1 ms phase-continuous sweeps, so its whole
transmitter power sits in its own sub-band. The message is a compact
80-bit check-in (packed call, the SNR it hears NCS at, caps, software
version, CRC-16, with the net id folded into the CRC so another net's
fails), coded with the short LDPC code shortened (the other information
bits are known zeros, given to the decoder as priors): 240 bits sent
(rate 1/3), after 8 known preamble tones, 2.5 s in all. NCS records the
slot and receives every sub-band from the recording, searching each
sub-band's timing and frequency on its own preamble, since every station
has its own clock error and frequency offset. This is OFDMA, as in the
uplink of 4G and 5G, done with noncoherent tones.

A station whose caps say `chorus` (bit 4) checks in there on every other
try, in a random chorus slot and sub-band; on the other tries it uses an
ordinary slot, in case it is too weak for a chorus (a chorus check-in
needs a few dB more than an R0 one). NCS takes each chorus check-in as it
would an ordinary one; a slot whose in-band power rose by a quarter
counts as busy (a collision: beacon again soon).

**Join time** (`net_join_table`): all members want in at once, each heard
by NCS at the given SNR (AWGN), hearing NCS at +10 dB; 4 ordinary slots,
or 1 ordinary slot and 2 chorus slots with every member chorus-capable;
seconds until every member has joined:

| Members | SNR dB | Window | All joined in 600 s | Median | Mean | Slowest | By chorus |
|---:|---:|---|---:|---:|---:|---:|---:|
| 5 | 0 | 4 slots | 4 of 6 | 419 s | 284 s | 420 s | — |
| 5 | 0 | 1 slot + 2 chorus slots | 6 of 6 | 26 s | 68 s | 178 s | 28 of 30 |
| 5 | −6 | 4 slots | 3 of 6 | 461 s | 459 s | 464 s | — |
| 5 | −6 | 1 slot + 2 chorus slots | 5 of 6 | 26 s | 57 s | 178 s | 28 of 30 |

Typically every member is in after the first window (26 s), against
several minutes of four-slot windows whose collisions wait for the next
beacon. The slow chorus runs are a collision in the chorus (two stations
in one sub-band), retried at the next beacon. The slots themselves take
15.8 s (one 9.1 s ordinary slot and two 3.4 s chorus slots) against 36.3 s
for four ordinary ones.

**Limits.** `window_slots` must stay at least 1 while any member without
chorus support may be on the net: such a member reads 0 as 1 and would
send an R0 check-in on top of the first chorus slot. Busy detection is by
in-band power, so a slot where every chorus check-in collided and all were
below about −6 dB does not bring the next beacon forward. Chorus runs over
audio only (`NetNode`), not on the model channel (`NetSim`). No hopping and
no power control.

## Measured on the model channel

`crates/ion/tests/net.rs` (seeded runs of `NetSim`; only burst lengths and
the channel model matter there):

| Test | Result |
|---|---|
| 5 members check in, report, and get a bulletin (`a_net_checks_in_reports_and_gets_the_bulletin`) | whole net in 284 s (6 rounds) |
| 12 stations checking in at once, 4-slot windows (`a_pile_up_of_check_ins_sorts_itself_out`) | all in after 971 s |
| 1968-byte bulletin to 4 members through 6 dB fading (`a_bulletin_reaches_everyone_through_fading`) | 177 s, 99 pieces, no repairs needed |
| A message NCS missed, recovered from an overhearer (`a_message_ncs_missed_is_recovered_through_the_received_list`) | 93 s, one received list |
| A station that cannot hear NCS at all seeks, joins through a relay, reports, gets the bulletin and a direct message (`a_station_that_cannot_hear_ncs_joins_through_a_relay`) | passes |
| Its relay goes silent: NCS moves the leaf to the other member that hears it, and its next message gets through (`a_leaf_is_moved_when_its_relay_goes_silent`) | passes |
| A weak member at −9 dB gets through at the floor while the others run at OFDM levels (`a_weak_station_gets_through_at_the_floor`) | passes |
| Capability flags reach the roster; with relaying off on the best hearer, NCS names the willing one (`capability_flags_reach_the_roster_and_steer_relays`) | passes |
| Traffic (with the check-in) and a 40-byte line reach the roster and events, and changes come with the next report (`traffic_and_lines_reach_the_roster`) | passes |
| A leaf's line and traffic reach NCS through its relay (`a_leafs_line_and_traffic_come_through_its_relay`) | passes |
| Emergency, then Priority, then short time, then slot order (`priority_and_emergency_members_are_polled_first`) | passes |
| A 6 011-byte object to 3 members (`an_offered_object_reaches_every_member_once`) | 102 s, 301 pieces (its sources, no repairs), one `Object` each |
| All 3 hold it in their store (`a_member_that_holds_the_object_costs_no_pieces`) | 0 pieces; with one lacking it, 301 |
| A member that joins after the others have it (`a_late_joiner_gets_the_object`) | gets it: 301 more pieces |
| A store copy that does not match its id (`a_store_copy_that_does_not_match_its_id_is_not_trusted`) | fetched over the air, file rewritten |
| An object to a leaf (`an_object_reaches_a_leaf_through_its_relay`) | its relay serves it |
| Pieces that do not hash to the offer's id (`an_object_that_does_not_match_its_id_is_rejected`, unit test) | rejected, never stored or handed over |

Over real modulated audio, `tests/net_audio.rs` runs NCS and two members
(`a_net_over_real_audio`) and a four-station net with a leaf
(`a_leaf_joins_through_a_relay_over_real_audio`).

**Coded repair against resending** (`coded_repair_needs_fewer_sends_than_resending`):
50 pieces to 10 stations, each losing 10% independently, averaged over
400 runs with the real encoder and decoder:

| Scheme | Extra sends after the first 50 |
|---|---:|
| Resend everything from the lowest piece any station lacks | 102.3 |
| Resend every piece any station lacks | 37.8 |
| Coded: send the largest shortfall in new repair pieces | 9.7 |

## QSY sub-nets (not built)

The pieces are shaped for them:

- **Grouping:** NCS knows who can QSY (caps bit 0) and each station's
  bandwidth (bit 1); bits 5–6 are kept for sub-net capabilities.
- **Running a sub-net:** a "relay turn" moved to another frequency. NCS
  names a key station (a willing, attended, QSY-capable member) and a
  group, and sends them the frequency and a time box in a new control
  frame. The key station runs the group exactly as a relay runs its leaves
  (SubPoll, SubAck, Child for its ID, coded bulletin pieces).
- **Return:** at the end of the time box everyone returns. The key station
  hands the group's traffic to NCS in its next turn, with the originators'
  slots and message numbers, so the received list and the dedupe already
  handle it.

## Open points

- **Check-in pile-ups** are slow with only 4 slots per window: 12 stations
  take about 16 minutes. The window could grow when a window was busy with
  no decodes, or use chorus slots.
- **A leaf whose relay dies with nobody else hearing it** stays with the
  dead relay; NCS keeps trying.
- **A leaf that has gone quiet** while two members still list it as heard
  can be moved back and forth between them every `miss_limit` rounds, each
  move costing a Tree frame and the new relay's Child frames.
- **The tree has two levels.** A leaf's leaf is not supported.
- **Seeks are not slotted.** They wait for a quiet channel, but can still
  collide with an NCS burst the seeker cannot hear.
- **Relaying is hop by hop.** If a relay vanishes right after
  acknowledging its leaf, the message survives only if another member
  overheard it.
- **One block at a time per member:** a member reports only its lowest
  incomplete bulletin block, so a bulletin of several blocks with losses in
  each is repaired one block per round.
- **Uplink bursts hold at most 6 frames,** because a header-only ACK has a
  6-bit bitmap: plenty for reports and forms, not for files.
- **Message numbers are 8 bits on the air,** widened against what the
  receiver has. A station more than 127 messages ahead of what NCS holds
  would confuse it.
- **The net id is 8 bits:** two nets on one frequency with colliding names
  would hear each other.
- **Primed reception costs CPU in bursts:** the margin test takes about
  0.5–1 s per weak burst in an expected slot. Caching the rival encodings
  per expected header would remove it.
- **One check-in calibration for everyone:** members that follow the
  beacon's timing and ones that do not share NCS's check-in offset; it
  settles between them if they differ.
- **Primed timing assumes stable latencies.** NCS learns the mean offset;
  jitter beyond ±60 ms (`slot_tol_s`) falls back to blind reception.
- **One object block at a time per member,** after every bulletin block,
  as for bulletins; only a member with none of a block is counted as
  lacking all the blocks after it.
- **The offer list repeats** each round while a member that has
  everything else neither holds nor fetches an offer. A member whose
  report cannot get out (at the floor, with traffic queued) keeps it
  repeating until it does.
- **Offer indices are reused** after a withdrawal: pieces of the old
  object that reach a member which has not heard the new list fail the id
  check and are fetched again.
- **Not tried on the air,** like the rest of Ion.
