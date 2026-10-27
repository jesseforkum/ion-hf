# Ion link: ARQ, gear shifting and HARQ

The point-to-point link: connect, ARQ with gear shifting over the level
ladder, turn-over and disconnect. Half duplex, burst by burst: the sending
side (ISS) sends a data burst, the receiving side (IRS) answers with an ACK
listing which frames of that burst arrived, and so on.

Code, in `crates/ion/src/`:

| Where | What |
|---|---|
| `level.rs` | the ladder: waveform, constellation, code rate and frame size of each level, and the gear-shift thresholds |
| `link/station.rs` | `Station`: one end of a link; `link/station/config.rs` (`Config`, the defaults below), `gear.rs` (gear shifting), `sender.rs`, `receiver.rs` |
| `link/frame.rs` | frames and tiny (header-only) messages |
| `link/harq.rs` | hybrid ARQ with incremental redundancy |
| `link/report.rs`, `chan.rs` | the channel report and the channel measurement behind it |
| `slots.rs` | slice placement: which third of the passband a slice burst goes in |
| `link/audio.rs` | bursts to audio and back through the PHYs, and every receiver at once |
| `link/node.rs` | `Node`: a station driven by audio, with the CW sense |
| `link/compress.rs` | optional compression of the stream |
| `link/station/lifeline.rs` | the lifeline: holding below the floor (Lifeline) |
| `link/sim.rs` | a model channel and two-station simulator for the tests |

The station knows nothing about audio: it hands out `Outgoing` bursts and
takes `Incoming` ones, on the caller's clock in seconds. Tests:
`crates/ion/tests/link.rs` (model channel), `ladder.rs` (real PHYs over
simulated audio), `harq.rs`, `node.rs`; the ignored tables in `ladder.rs`
and `link.rs` measure throughput and the overhead options.

## The ladder

Links open at the MFSK floor and climb. Between the floor and full-band
OFDM sit the faster MFSK R1 rates and two slice levels: QPSK on 13 carriers
(650 Hz, [phy.md](phy.md), Slice numerology), the same power on a quarter
of the carriers.

| # | Waveform | Modulation | Code rate | Frames | Low-PAPR | Threshold (dB) | Effective-SNR threshold (dB) | Air mean (dB) |
|---:|---|---|---|---|:---:|---:|---:|---:|
| 0 | MFSK R0 | 1 of 32 | 1/4 | short | | −12.0 | −13.0 | 0.0 |
| 1 | MFSK R1 | 1 of 32 | 1/4 | short | | −9.0 | −11.0 | 0.0 |
| 2 | MFSK R1 | 1 of 32 | 1/3 | short | | −8.0 | −10.3 | 0.0 |
| 3 | MFSK R1 | 1 of 32 | 1/2 | short | | −6.5 | −8.9 | 0.0 |
| 4 | MFSK R1 | 1 of 32 | 2/3 | short | | −4.5 | −7.4 | 0.0 |
| 5 | OFDM slice | QPSK | 1/3 | long | yes | −3.0 | −5.0 | −2.7 |
| 6 | OFDM slice | QPSK | 1/2 | long | yes | −0.5 | −2.5 | −2.5 |
| 7 | OFDM long guard | QPSK | 1/4 | long | yes | +2.0 | −0.5 | −2.4 |
| 8 | OFDM long guard | QPSK | 1/2 | long | yes | +6.5 | +3.8 | −2.4 |
| 9 | OFDM | QPSK | 1/3 | long | yes | +2.5 | 0.0 | −2.5 |
| 10 | OFDM | QPSK | 1/2 | long | yes | +5.5 | +3.0 | −2.5 |
| 11 | OFDM | QPSK | 2/3 | long | yes | +8.0 | +5.0 | −2.5 |
| 12 | OFDM | 8PSK | 2/3 | long | yes | +12.0 | +9.5 | −3.4 |
| 13 | OFDM | 16QAM | 2/3 | long | | +13.5 | +11.0 | −5.2 |
| 14 | OFDM | 16QAM | 3/4 | long | | +15.5 | +12.0 | −5.1 |
| 15 | OFDM | 64QAM | 3/4 | long | | +20.5 | +17.0 | −7.3 |
| 16 | OFDM | 64QAM | 5/6 | long | | +23.5 | +19.0 | −7.3 |

- **Frames.** Short frames are 32 bytes: GF(32)-coded in MFSK
  ([fec.md](fec.md)). Long frames are 180 bytes: the main LDPC code at the
  level's rate. The full-band OFDM part of the ladder was chosen from
  measured tables: at each speed, the pair needing the least SNR.
- **Low-PAPR.** The QPSK and 8PSK OFDM levels send their data DFT-spread
  and raised over the preamble ([phy.md](phy.md), Low-PAPR data symbols):
  at the same peak power their codewords decode 1.6–2.1 dB lower on AWGN
  and up to 2.2 dB lower on CCIR poor. 16QAM and 64QAM stay plain, where
  the spread receiver loses on fading.
- **Threshold** (`level::THRESHOLDS_DB`): the SNR (dB in 2.4 kHz) from
  which each level loses at most about 10% of frames, midway between a
  steady channel (AWGN) and a fading one (CCIR poor). MFSK from
  [mfsk.md](mfsk.md); R1's faster rates from what their codes need
  (`nb_profiles`); the long-guard levels from `numerology_tables`
  ([phy.md](phy.md)); the slice and full-band levels from `threshold_table`
  in `tests/ladder.rs` (1 dB grid, 40 bursts; `ION_GUARD=slice` or `long`
  for those levels). The OFDM thresholds were measured on the 2.75 kHz
  layouts and are used unchanged at 2.45 kHz.
- **Effective-SNR threshold** (`level::EFF_THRESHOLDS_DB`): each level's
  AWGN threshold, used against the effective SNR, which already holds what
  fading costs (Channel report).
- **Air mean** (`level::AIR_MEAN_DB`): each level's mean power on the air
  against a steady carrier at `TX_LEVEL`, averaged over 8 bursts of 4–8 s
  (`air_power_table`, checked by `slice_levels_average_full_ofdm_power`).
  MFSK is the carrier; the OFDM levels average lower at the same drive
  (their peaks are capped), QAM most.

## Connect

The caller sends a ConnReq frame (payload "FROMCALL TOCALL" and its
highest level) in the floor waveform; the called station answers with a
ConnAck, a header-only burst carrying the SNR it heard the call at, its
channel class and slice slot, and whether both directions will compress.
The call is tried `retries` (6) times. The first data burst then goes at
the highest level the ConnAck's SNR allows, less `start_margin_db` (3 dB:
the report was measured on the MFSK call, stronger than OFDM on the air),
and a loss steps down from there. At +20 dB a link starts in 64QAM instead
of climbing from the floor over five or six bursts.

## Frames and tiny messages

**Frames** fill one codeword each (32 bytes at the floor, 180 above):

```text
kind:u8  flags:u8  field:u24le  len:u8  payload[len]  zero pad  crc32:u32le
```

`field` is the byte offset of the payload in the sender's stream for data
frames, and the session for control frames. Offsets, not frame numbers,
let the sender re-cut unacknowledged data into the frame size of whatever
level comes next, larger or smaller. The CRC-32 covers everything before
it. ACK flags: the receiving side wants the turn, wants to disconnect, or
has stored the missing frames for HARQ; on a ConnReq, the caller can
compress.

**Tiny messages** ride in a header-only burst: 4 bits of kind in the
burst's `user` word plus a 16-bit `extra`, for messages that must get
through at the floor in a fraction of a second: the tiny ACK (burst number
mod 4, a bitmap of up to 6 frames, the SNR, the turn and disconnect
wishes, the HARQ flag, the channel class and the slice slot), the ConnAck,
Poll ("repeat your last ACK"), Turn ("your turn to send", with the SNR
at which the giver hears the taker), Idle ("still here, nothing to
send"), DiscReq and DiscAck.

## ACKs

- Bursts of up to 6 frames, and bigger ones that arrived whole
  (`tiny_ack_all`), are answered with a tiny ACK; others with a full ACK
  frame (a 64-bit bitmap and the channel report). A tiny ACK takes 0.27 s
  in OFDM against 0.44 s for a full ACK. A sender that does not know the
  all-set tiny ACK reads it as "the first six arrived" and resends the
  rest: slower, never wrong.
- **Cheap ACKs** (`cheap_ack_margin_db`): an ACK goes back in the waveform
  the burst came in, or in a shorter one when the burst's SNR is at least
  this margin (0 dB) above where that waveform's header-only burst gets
  through 90% of the time on CCIR poor (`cheap_ack_waves`): slice data is
  acknowledged in full-band or long-guard OFDM from +2 dB (0.27 s instead
  of 1.10 s), MFSK R0 data in MFSK R1 from −8 dB (1.12 s instead of
  1.79 s). Every receiver listens to every waveform, so the peer hears it
  either way; a Poll (a lost ACK) turns cheap ACKs off for 8 bursts.
- **A lost ACK** is asked for again with a Poll rather than by resending
  data, after the first try in the floor waveform.

## Burst length

At most `max_burst_s` (8 s; 8 s beats 4 s at every SNR on the audio link:
fewer turnarounds per byte), or longer while the channel stays clean and
steady (`burst_adapt`): after each burst whose fresh frames all arrived,
with the level unchanged and the SNR reports within `burst_steady_db`
(3 dB) of each other, the next may be twice as long, up to the length at
which the level's fixed cost per round trip (preamble and header, the tiny
ACK in its waveform, two turnarounds) is `burst_overhead` (12%) of the
cycle, and at most `max_burst_long_s` (32 s). That is long for the slow
levels (MFSK R0 has 2.3 s of preamble and header and a 1.8 s ACK for 5.7 s
of data in an 8 s burst) and about `max_burst_s` from the long-guard levels
up. Any loss, level change or jump in the SNR goes back to `max_burst_s`.
`burst_climb_db` (off) would hold bursts short while the SNR is close to a
faster level's entry; it measured even over short transfers and worse over
20 kB.

## Gear shifting

The sending side picks each burst's level from the peer's ACKs:

- **Aim** (`smooth_target`): from a cautious recent SNR, the lower of the
  last report and the median of the last three. On a slow-fading channel
  one good fade's report would otherwise send the link up into levels the
  next fade kills. The target is the highest level whose threshold plus
  `margin_db` the SNR reaches; `margin_db` is −1 dB, since a level a little
  past its threshold loses a few frames, and with HARQ each costs a
  fraction of a frame (−1 dB beat the +1 dB used without HARQ by 6.6% over
  the audio link).
- **Climb**: as clean ACKs come back, capped by the target, quickly while
  far below it. The climb steps over the slice levels and the faster R1
  rates once the SNR allows a full-band level past the first long-guard
  one (`skip_slices_from`): they are only slow steps on the way up, and on
  a long echo the slice fails where the long guard does not.
- **Losses** (`hold_by_goodput`): a lost burst does not give up a level
  whose recent success rate still beats the next one down on expected
  throughput (its success above their speed ratio): on a fading channel a
  burst lost in a fade is the channel's normal behaviour. R1 at 1/3 losing
  a burst in three in a slow fade still moves more than R0.
- **Holds**: a level that lost part of a burst is held off for
  `loss_hold_s` (15 s); one that lost a whole burst for `fail_hold_s`
  (30 s), doubled for each further failure up to `fail_hold_max_s`
  (480 s). A fast-fading channel kills the OFDM levels whatever the SNR,
  and the SNR report alone would climb straight back into them. After a
  failure with SNR to spare (4 dB above the threshold) only the levels
  whose guard is no longer than the failed one's are held: a slice level
  that a 7 ms echo breaks says nothing about the long-guard levels built
  for that echo.
- **Stepping down** counts only levels whose waveform the channel class
  allows, and from a long-frame level stops at the lowest allowed
  long-frame level: the floor's short frames would re-cut everything in
  flight into 22-byte pieces.
- `power_aware` (off) moves each SNR report by the difference in mean
  power on the air between the level it was measured on and the level
  aimed at (Air mean above). An MFSK burst at full carrier power reads
  2.4–2.8 dB better than the OFDM levels it sends the link to (QPSK
  2.6–4.8 dB better than QAM); on IONOS SIM with it on the link lost
  10–17% at 0 dB, and the margins would need retuning with it.

## Channel report

The receiving side measures every burst (`chan.rs`):

- **Delay spread.** OFDM: the power delay profile from the
  decision-directed channel of every data symbol, Hann-windowed across the
  carriers; the shortest delay span holding 90% of its power above the
  noise floor, less what a single path shows through the same window.
  MFSK: from the energy the previous tone leaves in the current window
  ([mfsk.md](mfsk.md)).
- **Doppler spread** (2σ of a Gaussian spectrum): OFDM from the pilots'
  correlation over a time lag, MFSK from the phase coherence of the two
  halves of each tone.
- **Effective SNR** (EESM): the AWGN SNR that would decode as well, over
  the demapper's own per-cell SINR (OFDM), or from the spread of the tone
  energies mapped onto OFDM cells (MFSK). On a flat steady channel it
  equals the mean SNR. The EESM entry points come from the ladder's AWGN
  thresholds: 8PSK from +9 dB, 16QAM +11, 64QAM +17.

`tests/chan_meas.rs` measured delay spans of 0–3 ms on AWGN, CCIR good and
poor, and 5–7.5 ms on the 6–7 ms profiles; Doppler 0–3 Hz on the 1 Hz
profiles and 8–12 Hz on the 10 Hz ones. Echoes past a 4 ms guard hide
from the decision-directed profile (the decisions follow a channel
estimate whose model spans at most the guard) and show as excess
decision-directed noise instead: 3.4–9 dB excess on a 7 ms echo from
+8 dB, under 0.5 dB on AWGN and CCIR poor; past a 2 dB threshold the echo
is reported as 6 ms.

The receiver averages the measurements over the link's data bursts
(newest weighted 0.5) and carries back two class bits and an SNR in every
ACK (`link/report.rs`):

- *echo*: the smoothed delay span exceeds `ECHO_MS` (4 ms, the guard of
  the slice and full-band OFDM levels): only the MFSK levels and the long
  guard are used;
- *doppler*: the smoothed Doppler spread exceeds `DOPPLER_HZ` (6 Hz): only
  MFSK is used;
- the SNR field carries the effective SNR instead of the mean
  (`eff_snr`), aimed at with the effective-SNR thresholds.

A tiny ACK carries the class (and the slice slot) in its kind nibble; a
ConnAck in bits 6–7 of `extra` (bits 0–5 are its SNR, the slot in 9–10); a
full ACK four more bytes: class and slot, effective SNR, delay span
(0.25 ms steps) and Doppler spread (0.25 Hz steps). The sender keeps off
the waveforms the class rules out (`pick_family`); holding off a level
that lost a whole burst stays as the fallback. So the sender picks its
waveform from the measurement instead of learning by losing bursts.

## HARQ

A long frame that fails is kept by the receiver as LLRs, and the next
burst opens with more parity of it instead of a resend (`link/harq.rs`).
A long data burst is `[P parity slots][F fresh frames]`. The parity slots
continue, in order, the codewords that failed in the previous burst (the
*stored set*): slot i carries the next bits of that codeword's
transmission order ([fec.md](fec.md)), up to the next rate boundary of
the code (`Rate::ALL`) or, once the whole mother code has been sent, a
chase repeat of one frame's length. Nothing in the burst names them: both
sides derive the layout from the stored set, and two check bits in the
burst's `user` word (0 for none, else 1 + P mod 3) catch a disagreement.
A codeword that has had two full passes of the mother code without
decoding is dropped by both sides and sent fresh. LLRs combine across a
level change (`llrs_combine_across_a_level_change`), but stored codewords
are not continued in another guard
(`stored_codewords_are_not_continued_in_another_guard`).

## Slice placement

The receiving side watches the three thirds of the passband a slice burst
can sit in (the centre and ±800 Hz) and names the cleanest in its ACKs
(`slots.rs`, `SlotMeter`); the sender puts slice bursts there
(`slice_slots`), and the receivers listen in all three. What the link
steps aside from are signals inside the passband that come and go (a
station keying, a carrier, a beacon group): a burst that overlaps one is
lost whatever the SNR, and a slice can move clear of it.

- The meter is a 2048-point FFT (23 Hz bins, 47 frames a second), each
  third compared with the whole passband in the same frame, so the radio's
  AGC, which moves all three together, cancels. Frames under the link's own
  bursts are left out.
- A *hit* is a bin in the third `HIT_DB` (13 dB) over the passband's
  median: at 10 dB white noise alone crosses it in about 3% of frames, at
  13 dB about one in 30 000. *Excess* is the third's median over
  the passband's (a wideband signal or noise source there).
- A third's score is its share of frames with a hit plus 1 point per dB of
  excess, averaged over `MEMORY_S` (8 s: signals in the passband come and
  go within tens of seconds), with 5 points of hysteresis. Slot 0 (the centre) is also what a station
  without slots uses.

## Turn-over, idle and disconnect

- The receiving side asks for the turn with a flag in its ACKs; with
  something queued and the channel quiet for 4 s, it sends an ACK unasked
  (again every 10 s). When the sender has nothing left in flight, or
  `break_in_s` (20 s) after the ask with data still queued, it sends Turn
  with the SNR it hears the peer at. The new sender starts at the level
  that report allows (its own direction: on a lopsided link the way back
  differs), with a first burst no longer than `max_burst_s`. With nothing
  queued, it checks in at once so the peer sees it took over. A Poll from
  the new sender also counts as the turn taken; a Turn heard while sending
  means our burst was lost, and it goes again no faster than the Turn.
- Polls, Turns and goodbyes that go unanswered are repeated in the same
  waveform, then one more robust, then the floor; in the floor at once
  after 45 s without progress. After a poll, the receiving side's ACKs go
  no faster than the poll and one rung below the lost ACK, for a run of
  bursts that doubles each time the way back fails again; the sender waits
  for an ACK in the slowest waveform the last one came in.
- A sender with nothing to send checks in with an Idle every `idle_s`
  (10 s). No progress (nothing acknowledged, delivered or handed over) for
  `link_timeout_s` (120 s) ends the link; with the lifeline on (the
  default) the link holds well before that instead (Lifeline).
- Either side can ask to disconnect (a flag in the ACKs); once nothing is
  left in flight the sender sends DiscReq and the other side answers
  DiscAck. `Station::disconnect` drops what is still queued and sends
  DiscReq at once; the goodbye gets `disc_retries` tries, then the link
  closes anyway.

## CW ID

`Node` keys a mid-link CW ID ahead of its first burst after each ID
period, timed from the link's ID clock (`Station::link_t0`: when the
answering station keyed the ConnAck the caller heard), which both ends
keep. A wait for an answer is stretched by the length of the peer's ID
when an ID time has passed since the peer last keyed. Ion HF sends the
end-of-link ID itself ([app.md](app.md)).

## Lifeline

When the link can no longer hold the floor, both ends hold it in R1/7
instead of retrying until a timer gives up: the floor's R1 stretched 7
times ([mfsk.md](mfsk.md), Below the floor), 32 tones 10.2 Hz apart in
337 Hz, which gets its 7.85 s header-only bursts through from about
−20 dB on AWGN and −18 dB on CCIR poor, 6 dB under R0. They report how
each end hears the other, keep the session through the fade, resume
without reconnecting when the path comes back, and close by agreement
when it does not. Code: `link/station/lifeline.rs`; `Config::lifeline`
off gives the old behaviour.

**On the air.** R1/7 sits in the 400 Hz around the link's centre
(1500 Hz), where both ends already are; its pull-in is ±35 Hz (R0's
±75 Hz), so the lifeline's receiver follows the frequency offset of the
peer's last bursts (`Receivers::follow`, moved once it is 8 Hz off, and
by the lifeline's own bursts through a hold). Its bursts are
`Wave::Lifeline`, header-only, at R0's level: one constant-envelope tone
peaking at `TX_LEVEL`, the same peak and mean power as R0; no level
changes. What tells them apart: the MFSK header's CRC initial value now
carries the layout's time scale as well as the mode and header type
(unchanged at k = 1, so full-band bursts are bit for bit as before), so
no full-band receiver takes an R1/7 header and the R1/7 receiver takes no
full-band one (`tests/lifeline.rs`: R0, R1, slice and OFDM bursts from
−6 to +20 dB never come out of it, R1/7 bursts never out of the others).
The R1/7 receiver tries R1 headers only. It does start on other MFSK in
its 400 Hz (an R1 data burst, an R0 burst 10–50 Hz off) and takes up to
6 s to give up, so it counts as carrier sense only while the link holds.
It costs about 13% more receiver CPU on noise, 23% on a link's bursts
(`receiver_cpu`).

**Messages** (`frame::Life`): header-only bursts in R1/7, their 4-bit kind
their own (read only from the lifeline's receiver; tiny messages use all
16 kinds):

| Kind | Message | `extra` |
|---:|---|---|
| 1 | Probe: the holding sender's, with how it last heard the peer | bit 6 set: an SNR in bits 0–5 (1 dB from −24) |
| 2 | Report: the answer, how the probe was heard | SNR, bits 0–5 |
| 3 | Bye: goodbye | bit 0: the hold ran out (else a host ended it) |
| 4 | ByeAck | |

Between probes the sender **knocks**: a header-only R0 Poll, which an
answering side that hears it answers with its ACK (not as a poll for a
lost ACK, which would hold its ACKs back after the resume). A path that
has come back answers the knock at once, so the link need not wait for
the next probe. No data goes in R1/7: nothing needs a frame.

**Trigger** (`life_enter_s`, 45 s): no data has moved for 45 s, at either
end: nothing acknowledged or delivered, no idle check-in answered, no
turn. ACKs that say nothing arrived do not count (on a path that is weak
one way, the polls' ACKs keep coming). The sending side then holds as the
prober, the other as the answering side; the receiving side's timer
stops its unasked ACKs as well. A probe heard makes an end the answering
side at once (two ends that both thought they were sending sort it out
that way; the probes' timing differs by up to 4.9 s by call sign).

**Hold.** What is in flight stays in flight and what is queued stays
queued, offsets and all; what the peer has acknowledged stays
acknowledged. The HARQ store is given up at both ends (the sender's
when the hold begins, the receiver's at the first probe), so frames go
again fresh. The prober probes at once, then after 60 s, doubling to
120 s (back to 60 s once a report is within 3 dB of resuming), and right
after each CW ID time (below); it knocks every 30 s between probes when
there is time for the answer. It waits for a Report 0.3 + 7.85 + 2 s,
plus the peer's CW ID when one is due.

**Resume**, without reconnecting: when a Report says the peer hears us,
and we hear the Report, at `life_resume_db` (1 dB) over the floor's
threshold (−12 dB mean SNR, so −11 dB), or when we hear a knock's ACK
there that reports the same of us (the ACK's report is the effective
SNR: 1 dB over the floor's −13). The gear starts afresh from the report,
as after a ConnAck (the old channel's holds and failures dropped), and
data goes at once. The answering side leaves its hold at the first
ladder burst it hears. **Hysteresis**: the hold begins at no data for
45 s (in the model, a mean SNR of about −15 dB) and ends at −11 dB both
ways; a hold within 300 s of a resume is a flap and adds 2 dB to what the
next resume wants (up to 6 dB), stops the knocks, and keeps the hold's
clock running from the first hold.

**Ending.**

- The hold has lasted `life_max_s` (15 min, counted across flaps): the
  prober says Bye (weak) in R1/7, up to `disc_retries` (3) tries; both
  ends close, `End::Weak` (the host's `LINK_FAILED weak path`), or
  unconfirmed if the ByeAck never came.
- Nothing heard from the peer for `life_silent_s` (300 s; the answering
  side, which only hears probes, 300 + 120 s): `End::LinkLost` at each
  end. Nobody can agree with nobody.
- A host disconnects in a hold (`Station::disconnect`, or
  `disconnect_when_sent` once its 30 s are up, since nothing moves): Bye
  in R1/7 from either end at once, `End::Client` there, `End::Peer` at
  the other.
- Not built: **retry later** ("try again in N minutes" in the Bye). It
  fits the Bye's spare bits, but neither the VARA nor Data2G's host
  interface has a way to tell a client, and calling back on our own is a
  station placing an automatic call on HF, which the owner should decide
  on (§97.221). Nor **lifeline ACKs** for a link weak one way (data at R0
  or above where it gets through, ACKs back in R1/7): the Report already
  says which way is weak, and the ARQ would take the 7.85 s ACK in its
  waits; at present such a link holds and, if it stays so, closes.

**Timers with 8 s bursts.** A probe exchange is 7.85 + 0.3 + 7.85 s; the
reply wait covers it. **CW ID** (§97.119) goes ahead of each end's first
burst after each 570 s of the link's ID clock (CW ID above); a hold
probes 0.5 s after each ID time, and the answering side answers at once,
so neither end's ID slips by a probe gap (`cw_ids_stay_on_time_through_a_hold`:
two `Node`s on audio, IDs every 100 s, a 6-minute fade: every burst after
an ID time follows its ID; in the hold the prober's IDs went 0.6 s after
each ID time and the answering side's 14 s after, after the probe and the
prober's ID, both every 100 s).

**Host** ([app.md](app.md)): the link stays `CONNECTED` through a hold;
`BUFFER` keeps counting what is unacknowledged, `PTT` follows the R1/7
bursts, `BUSY` the peer's. With `DATA2G ON`, `DISCONNECTED` says why
(`Station::end`). The Link screen shows Hold for the level.

### Results (simulation)

`tests/lifeline.rs`; `--ignored --nocapture` for the tables. A sends a
long stream to B at +10 dB (lopsided: +15 dB), the SNR drops from 120 s
as the row says. *Today* is `lifeline` off, and once the SNR is back,
A calls again (as an operator or a client that noticed would; B answers
again whenever its link has ended): the most favourable case for today.
Resume: from the SNR's return to the next byte delivered. Lost: bytes
of the stream B never got (today's reconnect starts from what was still
queued; what was in flight is gone). Air: both stations' air time from
the fade until data flows again (until the end where it never does).
Ended: when each end's link ended, from the start of the fade (over the
seeds where it did); in the ends, `up` is a link that never ended,
`weak` `End::Weak`, `lost` `End::LinkLost`. Medians over the seeds,
range in brackets.

**Model channel** (`lifeline_table`, `ModelMedium`, 12 seeds):

| Scenario | | connected at end | stream intact | resume, s | lost, bytes | air, s | holds (A) | ended A / B, s | ends A / B |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| fade −15 dB 2 min | today | 12/12 | 9/12 | 15 (9–129) | 0 (0–2210) | 46 (25–63) | 0 | 191 (120–233) / 120 (120–120) | lost/lost, up/up |
|  | lifeline | 12/12 | 12/12 | 30 (29–45) | 0 | 53 (52–67) | 1 | — / — | up/up |
| fade −15 dB 5 min | today | 12/12 | 3/12 | 17 (11–123) | 22 (0–2210) | 91 (33–111) | 0 | 264 (120–407) / 159 (120–287) | lost/lost, up/up |
|  | lifeline | 12/12 | 12/12 | 33 (13–34) | 0 | 73 (72–87) | 1 | — / — | up/up |
| fade −15 dB 10 min | today | 12/12 | 1/12 | 17 (16–81) | 22 (0–2210) | 95 (33–199) | 0 | 362 (120–665) / 241 (120–544) | lost/lost, up/up |
|  | lifeline | 12/12 | 12/12 | 30 (12–46) | 0 | 126 (110–139) | 1 | — / — | up/up |
| dies at −15 dB | today | 0/12 | 12/12 | — | 0 | 79 (17–197) | 0 | 378 (120–720) / 268 (120–626) | lost/lost |
|  | lifeline | 0/12 | 12/12 | — | 0 | 172 (164–186) | 1 | 970 (962–1001) / 970 (962–1001) | weak/weak |
| dies at −30 dB | today | 0/12 | 12/12 | — | 0 | 7 (7–14) | 0 | 112 (112–120) / 120 (120–120) | lost/lost |
|  | lifeline | 0/12 | 12/12 | — | 0 | 30 (30–37) | 1 | 292 (292–300) / 420 (420–420) | lost/lost |
| A→B −15 dB 10 min | today | 12/12 | 4/12 | 17 (15–89) | 5950 (0–11900) | 164 (65–331) | 0 | 347 (215–672) / 234 (121–560) | lost/lost, up/up |
|  | lifeline | 12/12 | 12/12 | 34 (11–53) | 0 | 161 (136–194) | 1 | — / — | up/up |
| B→A −15 dB 10 min | today | 12/12 | 0/12 | 17 (16–17) | 3570 (1020–6970) | 102 (101–194) | 0 | 114 (113–230) / 122 (121–237) | lost/lost |
|  | lifeline | 12/12 | 12/12 | 24 (10–53) | 0 | 147 (143–169) | 1 | — / — | up/up |
| A→B −15 dB for good | today | 0/12 | 12/12 | — | 0 | 149 (49–529) | 0 | 361 (215–1216) / 312 (121–1107) | lost/lost |
|  | lifeline | 0/12 | 12/12 | — | 0 | 210 (189–266) | 1 | 965 (963–996) / 965 (963–996) | weak/weak |

**Through the real PHYs** (`lifeline_audio_table`, `AudioMedium`, Watterson CCIR poor, SNR against each burst's mean power, 3 seeds; today's 5- and 10-minute fades and the −15 dB death come out the same because today's link is gone long before the fade's end and the same seeds then call again the same way):

| Scenario | | connected at end | stream intact | resume, s | lost, bytes | air, s | holds (A) | ended A / B, s | ends A / B |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| fade −15 dB 2 min | today | 3/3 | 2/3 | 43 (36–143) | 0 (0–22) | 67 (46–72) | 0 | 227 (120–227) / 128 (123–128) | lost/lost, up/up |
|  | lifeline | 3/3 | 3/3 | 34 (28–66) | 0 | 67 (65–74) | 1 | — / — | up/up |
| fade −15 dB 5 min | today | 3/3 | 1/3 | 36 (18–54) | 22 (0–22) | 72 (38–106) | 0 | 227 (120–235) / 123 (122–236) | lost/lost |
|  | lifeline | 3/3 | 3/3 | 40 (38–49) | 0 | 104 (93–116) | 1 | — / — | up/up |
| fade −15 dB 10 min | today | 3/3 | 1/3 | 36 (18–54) | 22 (0–22) | 72 (38–106) | 0 | 227 (120–235) / 123 (122–236) | lost/lost |
|  | lifeline | 3/3 | 3/3 | 50 (44–60) | 0 | 174 (158–179) | 1 | — / — | up/up |
| dies at −15 dB | today | 0/3 | 3/3 | — | 0 | 45 (21–72) | 0 | 227 (120–235) / 123 (122–236) | lost/lost |
|  | lifeline | 0/3 | 3/3 | — | 0 | 228 (205–237) | 1 | 966 (961–976) / 966 (961–976) | weak/weak |
| dies at −30 dB | today | 0/3 | 3/3 | — | 0 | 19 (19–20) | 0 | 120 (120–120) / 122 (122–123) | lost/lost |
|  | lifeline | 0/3 | 3/3 | — | 0 | 43 (42–43) | 1 | 294 (294–294) / 422 (422–423) | lost/lost |
| A→B −15 dB 10 min | today | 3/3 | 0/3 | 18 (18–37) | 4420 (4420–6290) | 85 (75–128) | 0 | 242 (224–350) / 124 (123–245) | lost/lost |
|  | lifeline | 3/3 | 3/3 | 31 (24–74) | 0 | 186 (167–193) | 1 (1–2) | — / — | up/up |
| B→A −15 dB 10 min | today | 3/3 | 0/3 | 18 (18–36) | 4930 (1870–5100) | 143 (123–156) | 0 | 174 (122–221) / 178 (122–225) | lost/lost |
|  | lifeline | 3/3 | 3/3 | 51 (14–71) | 0 | 206 (200–221) | 1 | — / — | up/up |
| A→B −15 dB for good | today | 0/3 | 3/3 | — | 0 | 59 (58–111) | 0 | 242 (224–350) / 124 (123–245) | lost/lost |
|  | lifeline | 0/3 | 3/3 | — | 0 | 240 (225–277) | 1 (1–2) | 968 (964–971) / 968 (964–971) | weak/weak |

**SNR wandering around the floor** (`flapping_table`, model channel, a
slow fade of σ about the mean, τ 60 s, from 60 s, 30 min, 12 seeds;
both modes call again at once when a link ends, today's best case):

| Mean, σ (dB) | | holds (A) | resumes | bytes delivered | links ended |
|---|---|---:|---:|---:|---:|
| −11, 2 | today | 0 | 0 | 18574 (14018–25324) | 0 |
| −11, 2 | lifeline | 1 (0–2) | 1 (0–2) | 18464 (14370–25324) | 1 |
| −13, 2 | today | 0 | 0 | 16946 (11740–21826) | 8 |
| −13, 2 | lifeline | 3 (1–3) | 1 (0–1) | 15146 (10486–20990) | 12 |
| −14, 3 | today | 0 | 0 | 15384 (12242–19696) | 48 |
| −14, 3 | lifeline | 3 (2–4) | 1 (0–2) | 14834 (11752–18860) | 13 |
| −15, 2 | today | 0 | 0 | 14780 (10288–19986) | 58 |
| −15, 2 | lifeline | 2 (2–3) | 0 (0–1) | 14648 (10200–19128) | 13 |

**Healthy links unchanged.** IONOS WGN and MPP at −6, 0 and +6 dB, two
seeds (`ionos_table`): byte-identical to before the lifeline in 11 of 12
runs and in all 12 with `ION_LIFELINE=0`; the twelfth (MPP 0 dB, seed 1)
held four times and moved 6718 bytes instead of 6356. The model
throughput table (`throughput_table` in `tests/link.rs`, 16 seeds) is
identical from +2 dB up; with 6 dB of slow fading at −2 and 0 dB, where
the fades reach under the floor and the link holds, within ±2% (161 →
163, 169 → 168, 256 → 260, 395 → 396 bit/s). The existing link, IONOS,
ladder, node and host tests pass unchanged except
`a_silent_peer_times_the_receiver_out`, whose receiver now holds before
it gives up (465 s instead of 120 s; the old bound with the lifeline off).

**What the numbers say.**

- Fades under the floor no longer cost the link: every fade from 2 to 10
  minutes was held and resumed with nothing lost or repeated, where today
  the link is lost at both ends and what was in flight goes with it
  (up to 2.2 kB here; 3.6–6 kB median on a lopsided path, 4.4–4.9 kB
  through the PHYs).
- Resuming takes 24–34 s median after the SNR is back on the model
  channel and 31–51 s through the PHYs (about the knock interval),
  against 15–17 s and 18–43 s for today's call placed exactly then;
  today's real cost is noticing and calling again.
- A hold is not free on the air: probes, reports and knocks take 15–30%
  of the time (both stations together; under 400 Hz for the R1/7 part),
  about what today's retries take before they give up, but for as long
  as the hold lasts.
- A path that stays weak closes by agreement after 16 minutes, both ends
  told why, instead of each end timing out on its own after 2–12 minutes;
  a path that goes silent is lost after 5 minutes (prober) and 7 minutes
  (answering side) instead of 2.
- Around the floor's threshold the link holds 2–3 times in 30 minutes,
  not every burst; it moves 1–11% less than today's link with an
  instant redial there (R0 still trickles at −13 dB in the model, and a
  hold waits for −11 dB), and its links end 12–13 times in 360 minutes
  (holds that ran out) against today's 8–58.

## Compression

With `Config::compress` on at both ends (agreed in the ConnReq and
ConnAck), the stream goes as records of compressed or raw bytes
(`link/compress.rs`): a small context-mixing model after lpaq1 (orders 0–3
and the current word, binary arithmetic coding, integer arithmetic so both
ends agree bit for bit), primed before the first message with on-air text
(`crates/ion/data/chat_train.txt`: net scripts, check-ins, shelter and
weather reports, radiograms, conversation), so short messages compress from
their first byte. Each direction keeps its model for the whole connection
and goes on learning from every message; the ARQ stream is exact and in
order, so both ends stay in step. An end-of-message flag costs about 6 bits
per message. Records are at most 1024 bytes; incompressible data grows by
one header byte per record. `tests/compress.rs` measures it on
`chat_heldout.txt`, written separately from the priming text; the
parameters were chosen on a split of the training text alone
(`tuning_table`). Winlink's B2F messages arrive already compressed, so Ion
HF leaves compression off on links; nets compress direct messages
([net.md](net.md)).

## Defaults (`Config::new`)

| Field | Default | |
|---|---|---|
| `turnaround_s` | 0.3 s | from the end of the peer's burst to our audio |
| `max_burst_s` / `max_burst_long_s` | 8 s / 32 s | burst length |
| `margin_db` | −1 dB | over each level's threshold |
| `retries`, `disc_retries` | 6, 3 | the call and the turn; the goodbye |
| `reply_slack_s` | 2 s | beyond an answer's expected end |
| `idle_s`, `break_in_s`, `link_timeout_s` | 10 s, 20 s, 120 s | |
| `loss_hold_s`, `fail_hold_s`, `fail_hold_max_s` | 15 s, 30 s, 480 s | |
| `harq` | on | |
| `pick_family`, `eff_snr` | on, on | channel report |
| `burst_adapt`, `burst_overhead`, `burst_steady_db` | on, 12%, 3 dB | |
| `tiny_ack_all`, `cheap_ack_margin_db` | on, 0 dB | |
| `smooth_target`, `hold_by_goodput` | on, on | |
| `power_aware`, `burst_climb_db` | off, off | |
| `start_margin_db` | 3 dB | |
| `slice_slots` | on | |
| `compress` | off | |
| `lifeline`, `life_enter_s` | on, 45 s | Lifeline |
| `life_probe_s`, `life_probe_max_s`, `life_knock_s` | 60 s, 120 s, 30 s | |
| `life_silent_s`, `life_max_s`, `life_resume_db` | 300 s, 900 s, 1 dB | |

`tests/ionos.rs` switches some of these for comparisons (`ION_SMOOTH=0`,
`ION_HOLD=0`, `ION_POWER=1`, `ION_START=floor`, `ION_LIFELINE=0`).

## Over the air

The OFDM levels go out `OFDM_AIR_GAIN_DB` (1.2 dB) above their simulated
level: every waveform is built to peak at `TX_LEVEL`, where the steady MFSK
and CW carriers set the station's power, and OFDM at the same peak reads
lower on a power meter.
The slice levels get a further 0.6 dB (1/3) and 0.95 dB (1/2)
(`level_trim_db`), so every OFDM level averages the same power as the
full-width QPSK levels; the total never exceeds 3 dB over the steady
carriers (`OFDM_AIR_GAIN_MAX_DB`), and the steady carriers never change.

`crates/ion/examples/sounding.rs` writes and decodes an over-the-air test
of the whole ladder: a numbered sequence of bursts, every waveform and
level from the floor up, between two CW IDs, for a recording elsewhere
(a public KiwiSDR or WebSDR, say) to be checked against.
