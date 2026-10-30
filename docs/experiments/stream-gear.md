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
- **When**: after 75 s of ARQ, while ARQ's goodput is under 1850 B/min
  and what is left would take it over 450 s (Decision, below), the gear's
  aim below 2 dB (2.4 kHz, effective), no turn or goodbye asked, no CW ID due
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

- **Threshold**: no fixed size; a break-even time at ARQ's own measured
  goodput (Decision).
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

## Decision: when to stream

The gear should never cost a short transfer anything and should pay on
large ones. Every threshold below comes from the traced sessions.

**Crossover**: the time from the start of a session after which the
gear-on link has delivered more than the gear-off one for good
(`lab_runs/stream_gear/cross.py` on `final_300.txt`/`final_900.txt`
against the gear-off traces; the gear then started at once, 15–32 s in):

| S:N | WGN | MPG | MPP |
|---:|---:|---:|---:|
| +3 | never (5 and 15 min) | 257 s (5 min); behind at 15 min | never |
| 0 | 82 s | 189 s | 218 s (5 min), 374 s (15 min) |
| −3 | 282 s | 282 s | > 300 s |
| −6 | 191 s | 239 s (5 min), 480 s (15 min) | 200 s (5 min), 551 s (15 min) |

ARQ's goodput (cumulative from its first data burst, gear off) 90 s in and
later: 0 dB at most 1700 B/min (WGN), +3 dB at least 1976 B/min (MPG at
180 s; 3138–7438 at 90 s).

**Rules** (`Station::stream_wanted`, `stream_next`):

- No decision before ARQ has sent data for 75 s (`MEASURE_S`): its
  goodput is the yardstick, and a short transfer is done by then, so it
  never streams and costs nothing.
- **Region**: ARQ's goodput under 1850 B/min (`REGION_BPS`), between the
  0 dB and +3 dB figures above; the aim below 2 dB and the hand-back at
  QPSK 1/4 stay. From a run's second block on, a block slower than ARQ was
  hands back to ARQ for 5 minutes.
- **Break-even**: what is left must take ARQ longer than 450 s
  (`BREAK_EVEN_S`) at its goodput: above most crossovers (82–374 s at
  0 dB, 191–551 s at −6 dB). The fixed 4 kB threshold is gone.
- **Tail**: the same test at each block's DONE; under it, End, and the
  rest goes on ARQ.
- **First block**: about two minutes at ARQ's goodput (`FIRST_BLOCK_S`,
  blocks from 1 kB), and its option no faster than four times that goodput
  (`start_cap`): MPP −3 dB's first block had been sized for QPSK 1/6 and
  never completed in 5 minutes.
- **Probes**: the receiver asks to go up from MFSK into QPSK only when its
  last two periods both read 0 dB or better (the lab's −1 dB on one period
  probed every few periods at −3 dB, a lost period each).
- **Host hint** (a host declaring a transfer's total size): not built.
  The rule sees only what is queued; Winlink clients queue a whole
  message at once.

**Mixed sizes** (`stream_sizes` in `tests/stream.rs`,
`lab_runs/stream_gear/sizes.sh`, `sizes_1.txt`, `sizes_2.txt`,
`sizes_table.py`): each size queued alone at the start, gear on against
gear off, seed 900 + S:N, timed until the receiving side holds it all,
capped at 15 minutes and past that projected from the rate so far (~, as
exp-stream). 50 kB is read from the 200 kB run, as the lab read its small
files. B/min, on / off (on over off):

| S:N | channel | 2 kB on / off (ratio) | 10 kB on / off (ratio) | 50 kB on / off (ratio) | 200 kB on / off (ratio) |
|---:|---|---|---|---|---|
| +3 | WGN | 3871 / 3871 (1.00) | 6522 / 6522 (1.00) | 7317 / 7317 (1.00) | ~7322 / ~7322 (1.00) |
| +3 | MPG | 2857 / 2857 (1.00) | 2247 / 2247 (1.00) | 3476 / 3476 (1.00) | ~3406 / ~3406 (1.00) |
| +3 | MPP | 2000 / 2000 (1.00) | 3846 / 3846 (1.00) | 4071 / 4071 (1.00) | ~4035 / ~4035 (1.00) |
| +0 | WGN | 1429 / 1429 (1.00) | 1648 / 1648 (1.00) | ~2379 / ~1655 (1.44) | ~2379 / ~1655 (1.44) |
| +0 | MPG | 526 / 526 (1.00) | 1015 / 857 (1.18) | ~1479 / ~792 (1.87) | ~1479 / ~792 (1.87) |
| +0 | MPP | 822 / 822 (1.00) | 1038 / 916 (1.13) | ~1584 / ~850 (1.86) | ~1584 / ~850 (1.86) |
| -3 | WGN | 686 / 686 (1.00) | 743 / 762 (0.98) | ~776 / ~767 (1.01) | ~776 / ~767 (1.01) |
| -3 | MPG | 418 / 418 (1.00) | ~581 / ~533 (1.09) | ~567 / ~533 (1.06) | ~567 / ~533 (1.06) |
| -3 | MPP | 513 / 513 (1.00) | ~661 / ~644 (1.03) | ~719 / ~645 (1.11) | ~719 / ~645 (1.11) |
| -6 | WGN | 755 / 755 (1.00) | 821 / 805 (1.02) | ~815 / ~807 (1.01) | ~815 / ~807 (1.01) |
| -6 | MPG | 403 / 403 (1.00) | ~426 / ~386 (1.10) | ~426 / ~386 (1.10) | ~426 / ~386 (1.10) |
| -6 | MPP | 396 / 396 (1.00) | ~526 / ~487 (1.08) | ~533 / ~488 (1.09) | ~533 / ~488 (1.09) |

- **Never worse than 0.98**, short transfers and +3 dB included: 2 kB
  and +3 dB never stream (identical runs), 10 kB at 0 dB on WGN stays on
  ARQ (it is under break-even there).
- **Gains on large transfers at 0 dB**: 1.44× on WGN, 1.86–1.87× on MPG
  and MPP; 1.06–1.11× at −3 dB on fading, 1.08–1.10× at −6 dB on fading,
  about level on WGN at −3 and −6 dB (R1 2/3 with its 11.4 s slot is
  little faster than ARQ's own MFSK there).
- One seed per cell: the fading cells vary by about 1.3× between seeds
  (the lab); ratios within a few percent are not significant.

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
