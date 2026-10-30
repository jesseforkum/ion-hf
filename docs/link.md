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
| `link/stream.rs`, `link/stream/`, `link/station/stream.rs` | the stream gear: options, outer code, tones, timekeeping, and the protocol |
| `link/report.rs`, `chan.rs` | the channel report and the channel measurement behind it |
| `slots.rs` | slice placement: which third of the passband a slice burst goes in |
| `link/audio.rs` | bursts to audio and back through the PHYs, and every receiver at once |
| `link/node.rs` | `Node`: a station driven by audio, with the CW sense |
| `link/compress.rs` | optional compression of the stream |
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
channel class and slice slot, whether both directions will compress, and
the capabilities both ends have.

**Capabilities** (`frame::caps`): what a station can do beyond the alpha.4
link, four bits offered in the ConnReq's flags (bits 4–7) and answered in
the ConnAck's `extra` (bits 11–14) with those the called station has too.
The link uses only what both have; a station that knows none (Ion HF
0.1.0-alpha.4 and before) leaves them clear and ignores them, so it agrees
to nothing and never sees anything new on the air. Two are defined:
`caps::CUE` (MFSK cue, below) and `caps::STREAM` (Stream gear, below).
With `Config::stream` off a station's ConnReq and ConnAck are byte for
byte those of Ion HF 0.1.0-alpha.5 (`the_conn_req_without_the_stream_is_alpha5s`).
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
compress, and its capabilities (Connect).

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
HARQ increments may stretch a burst up to `max_burst_long_s`
(`harq_stretch`) rather than be dropped when a loss has shrunk the burst
below what they need (HARQ, below).
`burst_climb_db` (off) would hold bursts short while the SNR is close to a
faster level's entry; it measured even over short transfers and worse over
20 kB.

## Gear shifting

The sending side picks each burst's level from the peer's ACKs, by
expected throughput (`gear`, `Gear::Goodput`) or, selectable for an A/B
on the air, up and down the ladder as before (`Gear::Ladder`, the rule of
Ion HF 0.1.0-alpha.4).

- **Aim** (`smooth_target`): from a cautious recent SNR, the lower of the
  last report and the median of the last three. On a slow-fading channel
  one good fade's report would otherwise send the link up into levels the
  next fade kills. The target is the highest level whose threshold plus
  `margin_db` the SNR reaches; `margin_db` is −1 dB, since a level a little
  past its threshold loses a few frames, and with HARQ each costs a
  fraction of a frame (−1 dB beat the +1 dB used without HARQ by 6.6% over
  the audio link).

**Goodput** (the default). After each ACK the next level is the one
expected to move the most: its chance of getting a frame through times its
speed (payload bits per second of air in a `max_burst_s` burst, preamble
and header included). The chance is a level's observed success (the share
of fresh frames delivered, newest burst weighted 0.3), fading back with a
180 s time constant to what the SNR report predicts: a logistic in the
report's distance from the level's threshold plus margin, 0.88 at the
threshold and 0.12 a dB below (the codes' waterfall is that steep). Each
further whole-burst loss at a level doubles how long its record is
remembered (at most 16×). Levels more than 2 dB above the report are not
tried, nor waveforms the channel class rules out; a change of level must
promise 15% more, 32% across frame sizes (which re-cut what is in flight).
There are no holds: a level that keeps losing scores itself out. A whole
OFDM burst lost with 4 dB to spare is the channel's doing (fast fading, an
echo past the guard), and counts as a loss at the other levels of its
waveform and the faster ones whose guard is no longer, so a dead waveform
is not found out level by level (`a_dead_waveform_is_not_retried_over_and_over`). Long-frame
bursts rejoin contiguous pieces in flight that a visit to the MFSK floor
cut into 22-byte pieces back up to the long frame (`merge_pieces`); pieces
the peer holds HARQ bits of stay as they are.

On IONOS SIM (5 minutes, 2 seeds, no cue), against alpha.4's ladder:
WGN +3 dB 3638 → 7140 B/min, MPG +3 528 → 1348, MPG +6 5115 → 7004, MPP +3
1904 → 2499, MPG 0 499 → 733; +10 dB unchanged within seed noise
([experiments/next-link.md](experiments/next-link.md)).

**Ladder** (`Gear::Ladder`):
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

The increments for the whole stored set go out or none of them do (then
those codewords are sent fresh again). When a loss has cut the burst back
to `max_burst_s` and they no longer fit, they may stretch it up to
`max_burst_long_s` (`harq_stretch`): dropping them near a level's
threshold restarts every codeword, the next burst fails the same way, and
the link stalls (16QAM 3/4 pinned at WGN +10 dB delivered nothing in 5
minutes without it, 13 566 B/min with it). Fresh frames never push the
stored set past the 64 slots an ACK can name.

**Iterative receiver** (`Config::turbo`, default 2 passes). When an OFDM
long-frame burst arrives with a codeword that does not decode, the
station gives the receiver's snapshot of the burst (`Incoming::grid`,
[phy.md](phy.md#iterative-data-stage)) a prior per bit: the re-encoded
bits of the codewords that decoded, the decoder's posteriors of the
others, and decodes the burst again from the store as it was before it
(`HarqRx::receive_turbo`; behind a cue, with the cue's partial count).
Passes go on while each one decodes more; the
store keeps the LLRs of the best. A pass costs about 0.5 ms of CPU per
data symbol, so a burst gets at most `TURBO_SYMBOLS` (1000) symbol
passes: two on an 8 s Normal burst (~0.4 s), one on a 16 s one, none on
the longest, so that the ACK is not held up past the peer's reply slack.
The posteriors fed back do not make the stored LLRs of codewords that
still fail any worse (`rx_lab.rs` `turbo_fer`: 0.422 bit of information
per coded bit either way on MPP, 0.4054 → 0.4058 on MPG).

## MFSK cue

At low S:N most full-band OFDM data bursts that are lost are never heard:
the OFDM header (and from about −2 dB the preamble check) fails, while the
data with HARQ could still get through. On a link that agreed to
`caps::CUE` (`Config::cue`, on), a full-band OFDM data burst (normal or
long guard) goes out behind an **MFSK cue** while the SNR the gear aims by
is below `cue_below_db` (7 dB, IONOS S:N about +9 dB; above it the OFDM
header is heard anyway and the cue only costs air time):

```text
| MFSK R1 cue: preamble 16 × 28 ms, header 48 × 14 ms | OFDM burst as without it |
```

- The cue is an MFSK header-only burst of its own kind (`Kind::Cue`, its
  own CRC initial values, never read as a tiny message or a data header),
  1.12 s: the burst's 16 `user` bits, 10 bits of OFDM data-symbol count
  and 6 bits of HARQ parity field (below). The rest of the OFDM header
  follows from the level in `user`.
- It is a steady carrier at `TX_LEVEL`, exactly as loud as every MFSK
  burst; the OFDM air gain and level trim apply to the OFDM part only
  (`a_cue_is_no_louder_than_mfsk_and_leaves_the_ofdm_as_it_was`).
- The receivers run MFSK first, in 20 ms blocks, so a decoded cue reaches
  the OFDM receiver while the burst is still in its 2 s ring
  (`PhyRx::cue`): detection is skipped, the header is taken from the cue,
  and the burst is not rejected on its preamble correlation (the cue's
  CRC vouched for it). A burst taken on its cue alone reports its SNR but
  no delay or Doppler spread, whose timing came from noise.
- The cue is part of the burst everywhere its length counts: the burst
  budget (`max_burst_s` holds the cue too), the level's worth in the gear
  shift, `clean_burst_s`, the air time.
- **Partial HARQ increments** (`harq_partial`, on): behind a cue, when the
  increments for the whole stored set do not fit the burst, those that do
  go (the longest stored first) and the cue names how many; the receiver
  continues those and keeps the rest stored ahead of the burst's failures.
  A partial burst's check bits count one more than the stored set, so a
  receiver that missed the cue (and so the count) keeps nothing rather
  than misreading the layout. Bursts without a cue use `harq_stretch`.

Heard 90% of the time (IONOS SIM, S:N against the burst's peak in 3 kHz):
today's full-band data header at +1.4…+1.8 dB on WGN, +4…+4.7 on MPG,
+3.4…+6 on MPP and not by +6 dB on MPD; with the R1 cue at −11.0, −7.2,
−8.0 and −8.4 dB (the `exp-mfskhdr` lab). With the goodput shift, MPG
+3 dB 1348 → 2933 B/min and MPP +3 dB 2499 → 3910
([experiments/next-link.md](experiments/next-link.md), the cue rule).
`tests/cue.rs` checks a cue-less peer gets no cue (as caller and as called
station) and that two stations with cues use them.

## Stream gear

For a big transfer at low S:N the sending side stops sending bursts
that each wait for an ACK and streams instead: periods of outer-coded
pieces back to back, a pause after each in which the receiving side may
answer with two tones, and silence meaning "go on"
([experiments/rateless-stream.md](experiments/rateless-stream.md),
variant A; the build: [experiments/stream-gear.md](experiments/stream-gear.md)).

**When.** Streaming is for large transfers at low S:N; every transfer
starts on ARQ, and ARQ's own goodput (data bytes acknowledged over the
seconds spent sending data) is the yardstick. On a link that agreed to
`caps::STREAM`, once ARQ has sent data for `stream::MEASURE_S` (75 s; a
short transfer is done by then and never streams), the queue streams
when all of these hold (calibrated on IONOS SIM,
[experiments/stream-gear.md](experiments/stream-gear.md), Decision):

- ARQ's goodput is under `stream::REGION_BPS` (1850 B/min): the stream
  wins where ARQ is slow (0 dB and below) and not above;
- what is left would take ARQ longer than `stream::BREAK_EVEN_S`
  (450 s) at that goodput: the gear-on link overtook the gear-off one
  82–374 s into 0 dB sessions and 191–551 s into −6 dB ones;
- the SNR the gear aims by is below `stream::BELOW_DB` (2 dB in 2.4 kHz);
- the peer asks for neither the turn nor a goodbye, and no CW ID is due
  within two periods.

With frames still in flight, the data bursts send only those until they
are acknowledged. The same test runs at each block's end: what is left
under break-even (the tail) finishes on ARQ. From a run's second block on,
a block slower than ARQ was goes back to ARQ for 5 minutes. The first
block of a run starts no faster than an option of four times ARQ's
goodput (`stream::start_cap`).

**A block.** The sender takes the next 1–16 kB of its queue: the first
block of a run about two minutes at ARQ's goodput (`stream::FIRST_BLOCK_S`;
the stream should do at least that, so it completes wherever the stream
settles), later ones about 2.5 minutes of the option's nominal rate after
a block that needed at most 1.05 times its pieces, one minute otherwise
(`stream::block_bytes`), and cuts it
into 28-byte pieces, K of them. Pieces are numbered 0–65535: 0..K are the
block itself, the rest repairs of a systematic Cauchy code over GF(2^16)
(`stream::code`), any K distinct pieces giving the block back. The
receiver delivers each source piece as it arrives (in order, as the link
delivers anything), and the rest once K are in.

```text
Start → Go | period 0 | pause | … DONE → Start → Go | period 0 | … DONE → End → Report
```

- **Start**: stream id, start option, the block's offset (24 bits),
  length and CRC-32, in a control frame in the link's waveform; **Go**
  answers with the option (for the first block the more cautious of the
  sender's pick from its last reports and the receiver's from the Start's
  SNR; later blocks go on in the option the last ended in) and how far
  the receiver's stream is delivered. A Go that does not come after the
  control frame's tries leaves the block to ARQ and holds the stream off 5
  minutes.
- **Blocks follow one another**: at DONE, with enough still queued, the
  sender opens the next block at once (unless the block ended at QPSK 1/4
  or faster, `stream::HIGH_OPT`: there the link's HARQ does as well or
  better, so End, and ARQ for 5 minutes); its Go's delivered offset confirms
  the block before (if it does not, both go back to ARQ), and its flags
  carry the receiver's turn and goodbye wishes (if set, the new block goes
  back to the queue and the link hands over).
- **Periods** start every `SLOT_S` (11.4 s) on the sender's clock from its
  turnaround after the Go, whatever the option, so a receiver unsure of
  the option still knows when the next one starts. Each holds at most
  9.8 s of burst (`BURST_S`) after the PTT lead: an OFDM period whole
  LDPC codewords of six pieces each (id, option, first index, pieces,
  CRC-32), an MFSK period GF(32) frames of one piece (index, piece,
  CRC-16 seeded by the stream id). Fresh pieces in order, sources first.
- **Options** (`stream::OPTS`), slowest first; SLOWER and FASTER move one:

| Option | Frames | Pieces | Burst | Nominal |
|---|---:|---:|---:|---:|
| MFSK R0 1/4 | 1 | 1 | 7.98 s | 147 B/min |
| MFSK R1 1/4 | 2 | 2 | 7.08 s | 295 B/min |
| MFSK R1 1/3 | 3 | 3 | 7.90 s | 442 B/min |
| MFSK R1 1/2 | 5 | 5 | 8.62 s | 737 B/min |
| MFSK R1 2/3 | 7 | 7 | 8.99 s | 1032 B/min |
| QPSK 1/8 | 3 | 18 | 9.61 s | 2653 B/min |
| QPSK 1/6 | 4 | 24 | 9.61 s | 3537 B/min |
| QPSK 1/4 | 6 | 36 | 9.61 s | 5305 B/min |
| QPSK 1/3 | 8 | 48 | 9.61 s | 7074 B/min |
| QPSK 1/2 | 12 | 72 | 9.61 s | 10611 B/min |
| QPSK 2/3 | 16 | 96 | 9.61 s | 14147 B/min |

  QPSK is full-band normal-guard low-PAPR OFDM as ladder level 10; rates
  below 1/4 repeat the mother code (chase). Every QPSK option fills the
  same data symbols, so they share one head. Nominal: every piece
  through, per slot.
- **The pause** holds the receiver's turnaround and PTT lead, its two
  0.2 s tones, and slack for both sound cards' latency (at least 1.4 s
  of air with a 0.2 s lead). The receiver answers after its turnaround
  from the end it heard; an answer that would start more than 0.1 s late
  (decoding held it up) waits for the next pause rather than run into the
  next period. Tones, detector and timekeeping: [phy.md](phy.md), Stream
  tones and timekeeping.
- **The receiver's rule** (`stream::Rule`, the lab's): DONE once it holds
  K pieces (and decodes and delivers the block; a block whose CRC fails
  starts again). MFSK: SLOWER under 60% of frames, FASTER after two clean
  periods (into QPSK only when the last two periods both read 0 dB or
better), longer after a failed probe. OFDM,
  over the last two periods: each codeword's blind information predicts
  how the neighbouring rates would do; it moves to the better by 3%,
  faster only if 80% would decode; down into MFSK when, over four
  periods, MFSK would carry more. Two periods in a row with nothing
  decoded: SLOWER, whatever the prediction (exp-sync's fix). A SLOWER or
  FASTER may not have been heard: the receiver then tries the next
  period at both options.
- **End**: after the last DONE (or when it gives up: four times K pieces
  sent, or four minutes with no answer and twice K sent) the sender sends End, in
  an OFDM control frame from QPSK 1/3 up, MFSK below. The receiver's
  **Report** says whether it has the block, how far its stream is
  delivered, the SNR it heard the periods at, and (in the frame's flags)
  whether it wants the turn or a goodbye. Only a Report or the next Go
  acknowledges a block: a DONE heard where none was sent costs the End and
  Report, and the block goes on. A block given up goes back to the front of the queue
  for ARQ.
- **CW ID.** A block never runs over an ID time: the sender ends it with
  End/Report when an ID falls due within two slots, both sides' IDs go
  ahead of their next bursts, and the same block starts again (Start with
  the same id, offset and CRC: the receiver keeps its pieces). Node never
  puts an ID ahead of a period or an answer.

**Turns.** A stream is not interrupted: the receiving side cannot ask
for the turn mid-block, so it waits up to a block (a few minutes); its
Report carries the wish, and the sender does not open another block
while the peer wants the turn (`the_receiver_with_data_gets_the_turn_after_the_block`,
`after_the_turn_the_other_side_streams_too`).

**Progress.** Periods heard count as progress at the receiving side,
tones heard at the sending side, so a stream does not end the link by
`link_timeout_s`; a receiving side that hears nothing for that long fails
the link as in ARQ.

`tests/stream.rs`: the model channel (a 20 kB transfer in blocks, a
station without the gear, small transfers and high S:N staying on ARQ,
the ConnReq bytes, turns, CW IDs), IONOS SIM with the real PHYs, and two
`Node`s on one continuous audio timeline.

## Expected headers

While a link is up the station lists the OFDM bursts its peer may send
next (`Station::expected`), and the receivers decode a header that fails
its check against them ([phy.md](phy.md#expected-headers)). Receiving
side: data bursts at every OFDM level whose effective-SNR threshold is at
most 5 dB (the sender may jump several levels after a loss; headers fail
only below about +5 dB, where faster levels lose their data anyway), with
and without the HARQ increments for the stored codewords (check bits
`1 + P mod 3`, or 0), every frame count up to `max_burst_s`, the next two
burst numbers; poll and idle; and the turn (with each 6-bit report or
none) only while our ACKs ask for it. Sending side: a full ACK frame in
the waveform of our data and of the last ACK. The goodbyes only while we
are saying goodbye ourselves: a turn or a goodbye taken from a guess
would hand the link over or end it, so those are expected only when our
side has asked for them. Tiny ACKs carry a bitmap and a report, too many
values to list. Nothing is expected outside a link: a call comes in
MFSK. A Normal receiver gets about 190 headers (`tests/expect.rs`, which also checks
that every slow, short data burst and tiny burst of a running link was in
the set). `Config::expect_headers` (on) turns it off. A cued burst
takes its header from the cue, and a partial HARQ burst is useless
without its cue's count, so neither layout is listed for it.

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
  `link_timeout_s` (120 s) ends the link.
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
| `turbo` | 2 | passes of the iterative receiver (0: off) |
| `expect_headers` | on | the receivers try the expected headers |
| `pick_family`, `eff_snr` | on, on | channel report |
| `burst_adapt`, `burst_overhead`, `burst_steady_db` | on, 12%, 3 dB | |
| `tiny_ack_all`, `cheap_ack_margin_db` | on, 0 dB | |
| `smooth_target`, `hold_by_goodput` | on, on | |
| `power_aware`, `burst_climb_db` | off, off | |
| `start_margin_db` | 3 dB | |
| `slice_slots` | on | |
| `compress` | off | |
| `gear` | `Goodput` | `Ladder`: the alpha.4 rule |
| `harq_stretch`, `merge_pieces` | on, on | |
| `cue`, `cue_below_db`, `harq_partial` | on, 7 dB, on | MFSK cue |
| `stream` | on | the stream gear (offered; used when both have it) |

`tests/ionos.rs` switches some of these for comparisons (`ION_SMOOTH=0`,
`ION_HOLD=0`, `ION_POWER=1`, `ION_START=floor`, `ION_GEAR=ladder`,
`ION_STRETCH=0`, `ION_MERGE=0`, `ION_CUE=0`, `ION_CUE_DB`, `ION_PARTIAL=0`,
`ION_TURBO=n`, `ION_EXPECT=0`; its header lists the rest).

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
