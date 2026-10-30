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
  5 dB (2.4 kHz, effective), no turn or goodbye asked, no CW ID due
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
  The lab's finer rates and 16/64QAM are left out (above 5 dB the stream is
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
- **Gate** 5 dB aim (`stream::BELOW_DB`): the lab's stream drew with ARQ
  above +3 dB IONOS S:N and lost a little on WGN at +3/+6 dB.
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

RESULTS

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
