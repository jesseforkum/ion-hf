# The stream gear, built

Branch `stream-gear` (off main `f06d79f`, 0.1.0-alpha.5). The rate-less
stream of [rateless-stream.md](rateless-stream.md) (variant A, tones only)
with the receiver timekeeping of [stream-sync.md](stream-sync.md), as a
second gear inside the Ion link: production code, simulation and
loopback only (nothing touched a radio). Protocol: [link.md](../link.md),
Stream gear; signals and receiver: [phy.md](../phy.md), Stream tones and
timekeeping; program: [app.md](../app.md).

## What was built

- **Agreement**: `caps::STREAM` (2) in the ConnReq's flags and the
  ConnAck's `extra`, offered while `Config::stream` is on (the default;
  `stream = false` in `ionhf.toml` or `--stream off` turns it off). With it
  off, or with a peer that does not offer it (alpha.5 and before), the link
  is plain ARQ and its ConnReq/ConnAck bytes are alpha.5's.
- **When**: 4 kB or more queued, nothing in flight, the gear's aim below
  2 dB (2.4 kHz, effective), no turn or goodbye asked, no CW ID due
  within two periods. The queue then goes in blocks of 2–16 kB (about four
  minutes of the option's nominal rate), one after another: at DONE the
  next Start goes at once and its Go confirms the block before; End and
  Report close the stream. Source pieces are delivered as they arrive.
- **Handshake and ending**: Start / Go and End / Report control frames
  (`Kind::Stream`). The Report alone acknowledges a block; DONE only stops
  the periods. A block given up goes back to ARQ.
- **Periods**: an 11.4 s slot each on the sender's clock (lead, at most
  9.8 s of burst, pause), outer-coded 28-byte pieces (systematic Cauchy
  over GF(2^16)) in the option's frames: MFSK R0 and R1 at the GF(32)
  rates, or low-PAPR QPSK OFDM at rates 1/8 to 2/3 (chase below 1/4).
  The lab's finer rates and 16/64QAM are left out (above 2 dB the stream is
  not used).
- **Tones**: DONE / SLOWER / FASTER as the lab's pairs (0.2 s, 45-bin
  grid, CFAR), at `TX_LEVEL`; the detector searches the answer's start in
  the sender's 1.15 s listening window.
- **Receiver**: exp-stream's rule with exp-sync's fix (two empty periods:
  SLOWER); exp-sync's timekeeping (anchors, clock slope, known-head search
  with a window from the anchors' scatter, the receiver's own header as
  fallback) in `Receivers`, on a sample clock that counts what the host
  mutes while we transmit.
- **Turns**: not taken mid-block; the Report carries the wish.
- **CW ID**: a block ends before an ID falls due and resumes after it.
- **Ion HF**: STREAM, CUE, DONE, SLOWER, FASTER lamps; the stream's
  option, period and pieces in the level readout; log lines; the decode
  plot clipped to its frame.

### Choices

- **Threshold** 4 kB (`stream::MIN_BYTES`): the lab's smallest file was
  10 kB; a block needs a minute or two to pay for its Start/Go and
  End/Report (two control round trips, 3–20 s by waveform) and its
  overshoot (about half a period and a pause).
- **Gate** 2 dB aim (`stream::BELOW_DB`, about +4 dB IONOS S:N): at +3 dB
  the build drew with ARQ on fading and lost 12% on WGN (Sessions); the
  lab's stream drew there too.
- **Turn handling**: the simplest safe one. The stream is never
  interrupted; the receiving side's wish to send goes in its Report at the
  block's end (or, between blocks, in the next Go) and the sender then
  hands over by the link's rules: the peer waits at most one block.
- **Pause timing with PTT lead and tail**: the slot is fixed (11.4 s) so a
  receiver unsure of the option still knows when the next period comes.
  With the 0.2 s lead the pause is at least 1.4 s of air: the receiver's
  0.3 s turnaround, its 0.2 s lead, 0.4 s of tones, 0.5 s of slack for
  both sides' sound-card and PTT latency. An answer more than 0.1 s late
  (decoding held it up) is kept for the next pause. The sender listens
  1.15 s from its own end of transmission and searches the answer's start
  in that window, so the receiver's switching time does not matter within
  it.
- **Sender audio through the pauses**: not built. Ion HF's engine keys
  and plays each burst on its own (`shared.keyed` gates both the PTT and
  the sound card's TX ring), and making the output run on through an
  unkeyed pause means changing the sound-card and TCI paths that every
  other burst uses. Instead the sender hands each period over at its
  slot's absolute time on its own clock, so the start delay (engine tick,
  PTT confirmation) is an independent draw each period, never a walk; the
  receiver's window takes it (exp-sync: 100 ms of such jitter cost
  nothing). The receiver's side of the same problem is built: the engine
  reports the samples it mutes while keyed (`IonIn::Gap`), so the receive
  clock stays whole across the receiver's own answers.
- **`mf_span` not widened**: the search runs around a fresh receiver, which
  is cued at the find; the cue lands inside `PhyRx`'s own ±10 ms in
  96–100% of periods at 0 dB (exp-sync), so the PHY and its false-alarm
  behaviour stay as they are.

## Sessions

IONOS SIM, 2 paths, S:N in 3 kHz against a sine at each burst's peak,
real PHYs (`AudioMedium`), seeds as `tests/ionos.rs` seed 0 (900 + S:N),
400 kB queued at the start. `stream_trace` in `tests/stream.rs`
(`lab_runs/stream_gear/trace.sh`) runs each cell with the gear and with
`stream = false` (the link without it, whose ARQ is alpha.5's: the bytes
at connect and every ARQ path are unchanged), same seed and length, and
breaks the session down. Bytes are those the receiving side had
delivered in order at the end. Simulation only; 1–2 threads.

**Where the air time goes, and what each side delivers** (`final_300.txt`,
`final_900.txt` against `cmp_300.txt`, `off_300_m3.txt`, `trace_900.txt`;
B/min, the gear's periods with their pauses / ARQ data / control as % of
the session):

| S:N | channel | 5 min: gear | without | ratio | air | 15 min: gear | without | ratio |
|---:|---|---:|---:|---:|---|---:|---:|---:|
| −6 | WGN | 819 | 783 | 1.05 | 88 / 0 / 12 | 890 | 807 | 1.10 |
| −6 | MPG | 415 | 410 | 1.01 | 91 / 0 / 9 | 410 | 386 | 1.06 |
| −6 | MPP | 493 | 398 | 1.24 | 91 / 0 / 9 | 630 | 488 | 1.29 |
| −3 | WGN | 765 | 724 | 1.06 | 86 / 0 / 14 | | | |
| −3 | MPG | 707 | 424 | 1.67 | 87 / 0 / 13 | | | |
| −3 | MPP | 0 | 573 | 0 | 89 / 0 / 11 | | | |
| 0 | WGN | 2439 | 1597 | 1.53 | 89 / 0 / 11 | 2654 | 1655 | 1.60 |
| 0 | MPG | 1060 | 816 | 1.30 | 92 / 0 / 8 | 1470 | 792 | 1.86 |
| 0 | MPP | 1329 | 1081 | 1.23 | 86 / 0 / 14 | 1584 | 850 | 1.86 |
| +3 | WGN | 6362 | 6933 | 0.92 | 19 / 74 / 7 | | | |
| +3 | MPG | 2454 | 2262 | 1.08 | 53 / 33 / 14 | | | |
| +3 | MPP | 3303 | 3908 | 0.85 | 42 / 48 / 11 | | | |

- **The stream runs as designed.** From the first period (15–32 s in:
  connect, then Start/Go) the link spends 86–95% of the air in periods
  and their pauses, none in ARQ data; control (connect, Start/Go between
  blocks) is the rest. The periods are found by the timekeeping, not by
  their own header: in every session's last block (the counters start
  again with each block) the search found the head over its mark
  (`AcqStats::found`), the receiver's own header was never needed
  (`fallback` 0), and at most one period was missed; the prediction's
  half-window had settled at 3–7 ms.
- **The threshold and the gate do not hold it back**: 400 kB queued, and
  the aim is under 2 dB at every S:N up to +3 dB (it starts from the
  ConnAck, read on the MFSK call, which does not tell 0 dB from +3 dB).
- **Steady state is the lab's.** At 0 dB on WGN the stream sits at QPSK
  1/6 with every codeword decoded (259 of 260 in 15 minutes): 24 pieces a
  slot, 3537 B/min, against the lab's 3229–3459. At −6 dB on WGN it sits
  at R1 2/3 with every frame (497 of 497): 7 pieces a slot, about 1030
  B/min against the lab's 1105–1139.
- **What the sessions lose against those figures**, in order of size:
  - *Accounting*: bytes count when delivered in order. Source pieces are
    delivered as they arrive, but after a lost one the rest of the block
    waits for the block to decode; the block still in flight at the end
    counts nothing. At 0 dB on fading that is most of the gap between
    1470–1584 B/min and the pieces received (about 1700). MPP −3 dB
    delivered nothing in 5 minutes: 113 of the 127 pieces its first
    block needed were in at the end.
  - *Start*: connect (15 s), then the first block starts at the more
    cautious of the two sides' picks and climbs (MFSK steps after two
    clean periods): 40–60 s before the stream runs at its option.
  - *The slot*: 11.4 s whatever the option, so a 9.6 s QPSK burst fills
    84% of it and a 9.0 s R1 2/3 burst 79%, against the lab's 91% (its
    pause was 1 s with no PTT lead, its codewords ran across pauses).
    The pause holds the PTT lead and both sound cards' latency for the
    air; the fixed slot lets a receiver unsure of the option still find the
    next period.
  - *Blocks*: each costs a Start/Go (about 20 s in MFSK R1 control
    frames) and the rest of the period after its last piece. Blocks are
    one minute of nominal rate, two and a half after a block that needed
    at most 1.05 times its pieces.
- **Above 0 dB the link's own HARQ is as good or better** (+3 dB: 0.85,
  1.08, 0.92), as in the lab (its stream drew with ARQ from +3 dB, 0.93 on
  WGN). A block that ends at QPSK 1/4 or faster therefore hands the link
  back to ARQ for 5 minutes (the +3 dB cells' 33–74% ARQ data).

Each cell is one seed; fading cells vary by 1.3× between seeds (the lab).
The 5-minute table of `tests/ionos.rs` (`lab_runs/stream_gear/run.sh`,
2 seeds, WGN/MPG/MPP/MPD at −6 to +20 dB) was run on main
(`main_a5.txt`) and on two earlier versions of this branch
(`gear_partial_79db312.txt`, `old_*.txt`), not on the final one: the
diagnosis above came first. Its 5-minute snapshot is the accounting
problem above at its worst for the stream.

**Clock, carrier offset and jitter**: `AudioMedium` has none of them, so
the exp-sync impairment sets were not run on this code (see below).

## What is not known (first on-air test)

- **The receive clock across our own transmissions.** The timekeeping
  counts every sample; the engine reports what it mutes while keyed. If
  a radio stops sending receive audio while it transmits (TCI, a USB
  codec), those samples are never counted and the receiver's clock jumps
  back by its tone answer (about 0.6 s) each time it answers. The
  tracker would see that as jitter; the window (up to ±0.6 s) and the
  receiver's own header may cover it, may not. Check on the air: the
  receiving side's periods found against those heard by header.
- **Decoding time against the pause.** Three to six LDPC decodes at rate
  1/8 must end within the receiver's 0.3 s turnaround plus 0.1 s, or the
  answer waits a pause (the speed loop runs one period late, 10–35% in
  the lab). Not measured on the target machines.
- **The sender's start jitter** (engine tick, PTT confirmation per
  period): independent per period, taken by the window; its size on a real
  radio is unknown.
- **The sender's listening window** (1.15 s from its end of transmission)
  must hold the receiver's turnaround, PTT lead, tones and both latencies.
  A radio with a long TX-to-RX switch or a large sound-card buffer may
  push the tones out of it: then no answer is heard, the stream runs on at
  its start option, and a block ends only by giving up (back to ARQ;
  nothing is lost).
- **Carrier offset before the first anchor**: the first answers are not
  pre-corrected; the detector's bins are 6 Hz apart.
- **The simulator has no clock or carrier offset and no jitter**
  (`AudioMedium` carries each burst on exact time): the exp-sync
  impairment sets were not run on this code. The two-node test runs the
  timekeeping on a continuous timeline with the real Node and tone watch,
  but with ideal clocks.
- **CW ID** handling is covered by a model-channel test only.
