# Ion PHY: OFDM burst modem

The OFDM levels of the Ion ladder: scattered pilots, Gray QPSK / 8PSK /
16QAM / 64QAM with exact soft demapping, a low-PAPR mode for the QPSK and
8PSK levels, and three guard and width variants. Code, in
`crates/ion/src/`:

| Where | What |
|---|---|
| `phy.rs` | types, numerologies, the public API (`modulate_full`, `PhyRx`, `info_for`, `burst_samples_for`, ...) |
| `phy/frame.rs`, `phy/frame/header.rs` | layouts, pilot patterns, preambles, the burst header and its code |
| `phy/tx.rs` | the modulator, clipping and the low-PAPR mode |
| `phy/rx.rs` | the streaming receiver: detection, sync, header |
| `phy/rx/demod.rs` | the data stage: phase tracking, channel estimation, noise, LLRs, SNR, channel measurement |
| `phy/rx/drift.rs` | sample-clock drift tracking |
| `phy/est.rs` | Wiener channel estimation, exact demapping, the equaliser for spread symbols |
| `blanker.rs` | the impulse blanker in front of every receiver |

All Rust, no external DSP crates. `modulate`, `PhyRx::new`, `info` and
`burst_samples` are the 2.45 kHz Normal numerology with robust pilots;
`modulate_full`, `PhyRx::with_numerology`, `info_for` and
`burst_samples_for` take the numerology, and the `_with` variants also the
pilot pattern.

**Conventions.** Everything below is measured on the Watterson simulator
`ion_dsp::channel` (`crates/ion-dsp/src/channel.rs`). SNR is the mean
signal power of the whole burst against white Gaussian noise in 2.4 kHz,
`ion_dsp::dsp::noise_sigma(p, snr, 2400, 48000)`. Nothing here has been
measured on the air.

## Numerologies

| Numerology | Spacing, CP | Carriers | Occupied | Used by the link |
|---|---|---:|---:|---|
| `Normal` | 50 Hz, 4 ms | 49 | 2450 Hz | yes: the full-band levels |
| `Long` | 25 Hz, 10 ms | 97 | 2425 Hz | yes: the long-guard levels, for echoes to ~8 ms (NVIS) |
| `Slice` | 50 Hz, 4 ms | 13 | 650 Hz | yes: the two slice levels |
| `NormalWide`, `LongWide` | as Normal, Long | 55, 109 | 2750, 2725 Hz | no (`link::audio::WIDE` = false) |
| `Slim`, `Half`, `Quarter` | | 25, 7, 5 | 650, 350, 250 Hz | no |
| `Sixth` | 25 Hz, 10 ms | 13 | 325 Hz | no: six slices 400 Hz apart (Six slices) |

Any of them can send a **header-only** burst (32 bits, no data symbols)
for the link's short messages. The link's centre is 1500 Hz
(`link::audio::CENTER_HZ`) for every waveform.

## Burst (Normal)

```
| A 44 ms | B 24 ms | header 4 x 24 ms | data n x 24 ms | ramp 2 ms |
| A 44 ms | B 24 ms | header 4 x 24 ms | header again 4 x 24 ms | ramp 2 ms |   header-only
```

| Part | What | Why |
|---|---|---|
| A | 352 samples at 8 kHz of a 40-sample (5 ms) periodic waveform: 13 carriers every 200 Hz (bins 0, ±4, ..., ±24), Newman phases, 16-sample raised-cosine ramp-in | Schmidl-Cox detection at lag 5 ms (frequency unambiguous to ±100 Hz); the 36 empty in-band bins measure the noise |
| B | One full OFDM symbol (CP + 160), all 49 carriers known, Newman phases | Fine timing (matched filter), first full-band channel estimate |
| Header | 4 OFDM symbols, QPSK on the 176 data cells | Constellation, data symbols, 16 user bits, data mode, CRC-16 |
| Data | `n` OFDM symbols (1..1023), 44 data cells each | Payload |
| Ramp | 16 samples of cyclic continuation, raised-cosine down | Clean end |

A and B are sent 3 dB above the mean data-symbol power (their own PAPR is
about 3 dB, so they stay below the data peaks of a plain burst).

## Parameters (Normal)

| Parameter | Value | Reason |
|---|---|---|
| Baseband rate | 8 kHz complex (48 kHz / 6) | |
| FFT | N = 160, 50 Hz spacing, T_u = 20 ms | |
| Cyclic prefix | 32 samples = 4 ms | CCIR poor has 2 ms; the rest is timing back-off (0.5 ms), the energy-window timing estimate and filter tails. 3 ms would buy 4.3% throughput but leave ~0.5 ms margin |
| Symbol | 192 samples = 24 ms | |
| Carriers | 49: bins −24..+24, 275–2725 Hz at the 1500 Hz centre | Inside a 300–2700 Hz passband; the lowest carrier stays above the simulator's 250 Hz Hilbert limit |
| Centre | `center_hz` ≥ 1300 Hz (`MIN_CENTER_HZ`); tested at 1500 and 1700 | Below that the lowest carriers (within 75 Hz of 0 Hz) and the filter skirt fold over 0 Hz |
| Pilots | Scattered: carrier c is a pilot in symbol g when c mod 12 = 3·(g mod 4), plus carrier 48 in every symbol: 5 pilots + 44 data per symbol | A constant 44 data carriers. After time interpolation every 3rd carrier (150 Hz) is sampled every 4th symbol (96 ms) |
| Pilot grid vs channel | Unaliased for delays up to 6.7 ms (CP 4 ms) and Doppler up to ±5.2 Hz | CCIR poor: 2 ms, 1 Hz two-sigma spread (σ = 0.5 Hz) |
| Pilot overhead, boost | 10.2% of cells, amplitude 4/3 (+2.5 dB, as DVB-T) | |
| Doppler ICI | 1 Hz spread (σ = 0.5 Hz) at 50 Hz spacing: about −35 dB, (2πσT_u)²/12 | Negligible on CCIR poor; flutter (10 Hz) is out of scope (the MFSK floor's job) |
| Front end | NCO mix-down, 481-tap Blackman-windowed sinc (cutoff 1600 Hz at 48 kHz), decimate by 6. Flat (±0.05 dB) to ±1.32 kHz, more than 70 dB down beyond ±1.88 kHz | Covers ±1225 Hz of carriers plus ±50 Hz of offset. The same filter interpolates on TX |
| Header | 32 bits: constellation 2 (QPSK, 8PSK, 16QAM, 64QAM), data symbols 10, user 16, data mode 3, sparse-pilot flag 1; CRC-16-CCITT; K = 13 rate-1/8 tail-biting convolutional code ([fec.md](fec.md), Header codes) → 384 bits, stream-major, truncated to 352 slots (repeated when a copy has more), interleaved, scrambled, QPSK on 4 symbols; wrap-around Viterbi, 3 passes | Must decode below QPSK rate-1/3 data |
| Data mapping | Bits scrambled (hash PRBS), interleaved over the whole burst (`i → a·i mod n`, `a` ≈ n/φ coprime with n), Gray QPSK / 8PSK / 16QAM / 64QAM, unit mean energy | BICM: every codeword spreads over all carriers and the burst; scrambling keeps padding and repetitive data from making PAPR peaks |
| Longest burst | 1023 data symbols (24.6 s) | 10-bit header field |

`info()`: 44 data carriers, `symbol_s` 0.024, `overhead_s` 0.176
(preamble, header, ramp and the TX filter tails, so `modulate().len() ==
(overhead_s + n·symbol_s)·48000` exactly), `bandwidth_hz` 2450. Payload
bit rate before coding (burst time = 176 ms + 24 ms per data symbol):

| Constellation | Continuous | 1 s burst (42 symbols) | 3 s (125) | 10 s (417) |
|---|---:|---:|---:|---:|
| QPSK | 3667 | 3122 | 3463 | 3603 |
| 8PSK | 5500 | 4682 | 5195 | 5405 |
| 16QAM | 7333 | 6243 | 6927 | 7207 |
| 64QAM | 11 000 | | | |

With ~50 Hz spacing across ~2.4 kHz there are 49 carriers × 41.7
symbols/s, about 2040 cells/s, of which 1830 carry data; no cyclic prefix
in the 2–4 ms range changes that by more than 9%.

## Long numerology

Normal fails on ITU mid-latitude disturbed NVIS (7 ms, 1 Hz) and ITU
low-latitude disturbed (6 ms, 10 Hz): both overrun the 4 ms cyclic prefix,
and Normal's 150 Hz pilot grid aliases beyond 6.7 ms of delay. Long is
built for the first; the second is out of reach for this OFDM (Results)
and is left to the MFSK floor.

```
| A 80 ms | B 50 ms | header 2 x 50 ms | data n x 50 ms | ramp 2 ms |
| A 80 ms | B 50 ms | header 2 x 50 ms | header again 2 x 50 ms | ramp 2 ms |   header-only
```

| Parameter | Normal | Long | Why (Long) |
|---|---|---|---|
| FFT, spacing | N = 160, 50 Hz, T_u = 20 ms | N = 320, 25 Hz, T_u = 40 ms | The longer symbol pays for the longer CP |
| Cyclic prefix | 32 samples = 4 ms | 80 samples = 10 ms | 7 ms echo + timing slack (energy window 8.5 ms with filter tails, 0.75 ms back-off, 0.75 ms spare) |
| Symbol | 24 ms | 50 ms | |
| Carriers | 49 (bins −24..24), 2450 Hz | 97 (bins −48..48), 2425 Hz | Same band and front-end filter; 97 = 8·12 + 1 keeps the data-carrier count constant |
| Pilots | step 12, shift 3, period 4, plus the top carrier: 5 per symbol (10.2%) | the same pattern in carrier indices: 9 per symbol (9.3%) | |
| Pilot grid after time interpolation | 150 Hz × 96 ms | **75 Hz** × 200 ms | |
| Unaliased delay / Doppler | 6.7 ms / ±5.2 Hz | **13.3 ms** / ±2.5 Hz | Delay alias well beyond the 10 ms CP; Doppler covers NVIS (1 Hz two-sigma) with room. A 2 Hz spread is at the edge (Open points) |
| Doppler ICI | σ 0.5 Hz: −35 dB | σ 0.5 Hz: −29 dB; σ 1 Hz: −23 dB | Harmless for QPSK–16QAM at these spreads |
| Data carriers | 44 | 88 | |
| Header | 4 symbols × 44 cells | 2 symbols × 88 cells | The same 352 QPSK slots, so the same code |
| A | 352 samples (44 ms), period 40 (5 ms): 13 carriers every 200 Hz | 640 samples (80 ms), period 64 (8 ms): 19 carriers every 125 Hz (bins ≡ 0 mod 5) | A period unrelated to 5 ms (next section); pull-in ±62.5 Hz (tested ±50 Hz); 78 empty bins for the noise estimate |
| Detector window, thresholds | 264; M_ON 0.035, B 0.06, preamble 0.08 | 480 (A − period − ramp − CP); 0.02, 0.05, 0.055 | Longer windows, lower noise floor; M_ON 0.035 missed a quarter of CCIR-poor bursts at −2 dB that sat in an 8 dB fade |
| Timing: energy window, back-off, plateau | 24, 4 samples, 95% | 68, 6 samples, **99%** | See below |
| Channel model grid (fd × delay span) | {0.3, 1, 2.5} Hz × {1, 2, 4} ms | {0.3, 1, 2} Hz × {2, 5, 10} ms | |
| `overhead_s` | 176 ms | 242 ms | |
| Header-only burst | 272 ms | 342 ms | |
| Longest burst (1023 symbols) | 24.6 s | 51.4 s | |

**Efficiency.** Data cells per second: Normal 44 / 24 ms = 1833, Long
88 / 50 ms = 1760, i.e. **96%** of Normal (QPSK 3520 against 3667 bit/s
raw). The CP share rises from 16.7% to 20%, the pilot share falls from
10.2% to 9.3%. The fixed overhead is 66 ms longer, so a one-codeword burst
is 7–13% longer. The alternatives:

- Normal spacing with a 10 ms CP: 20 of 30 ms useful, 80% of Normal, and
  the 150 Hz pilot grid would still alias at 6.7 ms.
- 31.25 Hz (32 ms + 10 ms): 91% of Normal, but ~77 carriers is not
  12k + 1 (73 or 85), so a different pilot pattern.
- 12.5 Hz (80 ms + 10 ms): 106%, but pilots every 360 ms alias at ±1.4 Hz,
  too close to a 1–2 Hz spread, and ICI rises fourfold.
- 25 Hz with an 8 ms CP: 100% of Normal, but 1 ms left after a 7 ms echo.

**Timing.** The FFT window is placed once per burst, from the B matched
filter: the middle of the run of windows that capture nearly the best
window's energy. With a 7 ms echo and 1 Hz fading, a path 13–20 dB down at
sync time can be the stronger one a second later, so it must stay inside
the window. A 95% plateau leaves it out: at 40 dB (genie header, 7 ms /
1 Hz) that leaves an uncoded BER floor of 1.5e-2 and loses codewords at
12–16 dB. A 99% plateau keeps paths down to −20 dB inside the window
(floor 3.1e-3, no codeword failures at high SNR). Normal keeps 95%.

## Slice numerology

`Numerology::Slice` puts a burst into a 650 Hz slice of the passband, so
that three can sit side by side in an SSB channel and a receiver tuned to
one is not disturbed by the others. It is not in `Numerology::ALL` (the
four full-width numerologies). The link uses it for its two slice levels
(QPSK 1/3 and 1/2, [link.md](link.md)), between the MFSK floor and
full-band OFDM, in the third of the passband the receiving side names
(centre, or ±800 Hz from it); `link::audio::Receivers` runs a slice
receiver in each third. The PHY tests below use centres 750, 1500 and
2250 Hz.

```
| A 176 ms | B 24 ms | header 18 x 24 ms | data n x 24 ms | ramp 2 ms |
```

```rust
let opt = TxOptions::for_burst(Constellation::Qpsk, Pilots::Robust);
let audio = phy::modulate_full(Numerology::Slice, Constellation::Qpsk, user, 0, &bits, 750.0, &opt);
let mut rx = PhyRx::with_numerology(Numerology::Slice, 750.0); // one receiver per slice
rx.process(&audio_in, &mut bursts);
```

| Parameter | Slice | Normal | Why |
|---|---|---|---|
| FFT, CP, symbol | N = 160, 32, 24 ms | the same | 50 Hz spacing and Normal's 4 ms guard |
| Carriers | 13 (bins −6..6), 600 Hz between the outer ones, 650 Hz occupied | 49 | 12 intervals = 6·2 = 12·1, so steps 6 and 12 both give `nc = step·k + 1` |
| Robust pilots | step 6, shift 3, period 2: 3 per symbol (23%), 10 data carriers | step 12 / 4: 5 (10.2%), 44 | See below |
| Sparse pilots | step 12, shift 3, period 4 (Normal's robust): 2 (15%), 11 data | step 24 / 8 | |
| A | every 2nd bin (7 carriers), period 80 = 10 ms, 1408 samples (176 ms) | every 4th bin, 5 ms, 44 ms | See below; frequency pull-in ±50 Hz (tested ±30) |
| Header | 18 symbols × 10 cells = 360 QPSK slots | 4 × 44 = 352 | Same code |
| CRC masks data / header-only | 0x6E3A / 0x1BC7 | 0x0000 / 0xB7E1 | Distinct from every other numerology |
| Filter (TX and RX) | 1681 taps, cutoff 370 Hz | 481, 1600 Hz | Flat (±0.05 dB) to ±300 Hz, at least 70 dB down beyond ±450 Hz |
| Lowest centre | 500 Hz | 1300 Hz | Carriers from 200 Hz; the −70 dB edge (centre − 450 Hz) stays above 0 Hz, and the RX image of any audio lands beyond −500 Hz, outside the filter |
| Detector window, M_ON | 1280 (A − period − ramp − CP), 0.035 | 264, 0.035 | Noise metric scales as 1/(noise bandwidth × window): 740 Hz × 160 ms ≈ Normal's 3.2 kHz × 33 ms |
| B search, ρ_B, preamble ρ | ±160, 0.06, 0.11 | ±80, 0.06, 0.08 | From noise and burst statistics (`diag_slice_rho`, `slice_noise_long`) |
| Noise estimate | 16 windows in A, 6 empty bins each, ±3 carriers | 4 windows, 36 bins, ±6 | |
| Estimator | Doppler {0.3, 1, 2.5} Hz × delay {1, 2, 4} ms, **40 pilots** per estimate, time scale × 0.15 | 20 pilots, × 1 | See below |
| `overhead_s` | 669 ms (header-only burst 1101 ms) | 176 ms (272) | |
| Data cells per second | 417 (23% of Normal; three slices 68%) | 1833 | |

- **Why longer fixed parts.** SNR is against the burst's own power in
  2.4 kHz, so a slice puts the same power into 13 carriers instead of 49:
  each data cell gets 3.45× (5.4 dB) Normal's energy, after the denser
  pilots' share. The data thresholds drop by about that much, and the
  preamble and header must drop with them. The header keeps its code, so
  each of its 360 slots gets 4.5× Normal's energy, about the data's share.
  A carries 4× Normal's energy for the same reason (176 ms instead of
  44 ms); a 44 ms A would also leave the frequency estimate ~2× noisier
  over a header four times as long. Measured (`diag_slice_rho`, 40 QPSK
  bursts per point, real header): AWGN −10 dB synced 37, header 27; −8 dB
  39 / 38; CCIR poor −6 dB 36 / 29.
- **Why every 2nd bin for A.** 160 = 2⁵·5, so A's step can be 2, 4 or 5
  (3, 3 or 1 carriers otherwise). Step 4 would copy Normal's A (5 ms, bins
  0 and ±4 at 200 Hz) exactly; step 5 is 4 ms, which Long's 8 ms detector
  would see as its own period. Step 2 gives 7 carriers (less like a tone,
  more frequency diversity for detection) at 10 ms; Normal's A is also
  10 ms-periodic and triggers the slice detector, but the preamble check
  rejects it.
- **Why pilots every 6th carrier.** On 13 carriers every cell is within
  300 Hz of an edge, so a Normal-density grid leaves the edge carriers with
  half the pilot neighbourhood an interior Normal carrier has. Step 6
  (shift 3, period 2) keeps Normal's 150 Hz frequency grid and samples it
  every 48 ms (±10.4 Hz) instead of 96 ms: twice the pilots in time, for
  1 carrier in 11 and 0.7 dB of data power. Step 4 would cost a third data
  carrier and 44% of the power in pilots.
- **Estimator neighbourhood.** With pilots every 2 symbols, the 20 pilots
  nearest a cell under Normal's distance all sit on the two pilot carriers
  next to it: a two-point interpolation across 150 Hz, 16 dB of estimation
  error on CCIR poor without noise (Normal 27.5 dB) and a 15–25%
  codeword-loss floor for 16QAM 3/4. `Layout::est_k` = 40 pilots and
  `est_lg` = 0.15 (time scale of the distance) take every pilot carrier of
  the slice over about ±6 symbols: 27.5 dB (`diag_slice_fading`), and
  16QAM 3/4 on CCIR poor reaches 90% at +14 dB.
- **Filter.** Slices 750 Hz apart occupy 650 Hz: the neighbour's nearest
  carrier is 450 Hz from the centre, 150 Hz from our edge carrier. A
  481-tap filter's transition is ~550 Hz wide, so the length is a layout
  field (`Layout::filter_len`; burst length, TX interpolation and the
  `start_sample` filter delay follow it). A slice receiver costs about
  13 ms per second of audio on one core. The TX filter removes the edge
  carriers' far sidelobes, which leaves an error ~27–30 dB below the edge
  carriers (38 dB in the middle; `diag_slice_cells`, clean). Harmless up to
  16QAM; **64QAM is not usable in a slice** (BER 1.2e-3 on a clean,
  unclipped burst).
- **Clipping buys less.** Audio crest factor, 20 bursts of 42 symbols
  (`diag_slice_papr`): unclipped 11.0 dB (Normal 12.0), 6 dB clip 10.1
  (9.7), 4.5 dB clip 9.3 (8.6). The clip distortion falls on 10 cells per
  symbol instead of 44, so it is heavier-tailed: clean 16QAM at a 5 dB clip
  has a BER of 2.7e-3 (Normal 2.5e-4, `diag_slice_clip`).
- **Timing.** B's matched filter has a main lobe 3.8× wider than Normal's,
  and `start_sample` comes out a constant 37 samples (0.77 ms) early on
  clean bursts (Normal: within ±7).

**Thresholds** (`slice_table`; centre 1500 Hz, one K = 1440 codeword per
burst, plain clip, real header, 20 bursts per point on a 2 dB grid; the
lowest SNR at which at least 90% of bursts deliver the right header, and
at least 90% decode the codeword; burst lengths 7.6 s, 4.1 s, 2.4 s and
1.8 s):

| Level | AWGN header | AWGN codeword | CCIR poor header | CCIR poor codeword |
|---|---:|---:|---:|---:|
| QPSK 1/4 | −8 | −6 | −4 | −4 |
| QPSK 1/2 | −8 | −2 | −4 | +2 |
| 8PSK 2/3 | −8 | +4 | −4 | +8 |
| 16QAM 3/4 | −8 | +6 | −4 | +14 |

Normal for comparison (genie header, 6 dB clip, 40 bursts; Results): QPSK
1/2 +2 / +10, 8PSK 2/3 +8 / +16, 16QAM 3/4 +12 / +18 dB. So a slice needs
4–6 dB less on AWGN, as its 5.4 dB more energy per cell says, at 23% of
the bit rate. On CCIR poor the header is the limit for QPSK 1/4: the
650 Hz slice fades nearly as a whole, where Normal's 2.45 kHz always keeps
part of the band. The link's slice levels, in the low-PAPR mode, are in
[link.md](link.md).

**Neighbouring slices.**

| Test | Result |
|---|---|
| Middle slice without noise, unclipped (`slice_adjacent_slices`) | reported SNR 40.6 dB alone, 40.0 dB between two full-power slice bursts at 750 and 2250 Hz |
| Three full-power slices overlapping in time, QPSK 1/2, +2 dB against one slice (4 runs) | every receiver delivers exactly its own burst, 12 of 12 codewords decode |
| Four steady carriers at 763, 1037, 1963 and 2237 Hz, each 20 dB above the middle slice (`slice_strong_neighbour_carriers`) | clean: 40.6 dB with and without them; at +2 dB 4 of 4 codewords decode, as without them, and no extra bursts |
| 60 s of noise per centre (`slice_noise_only`) | 750 / 1500 / 2250 Hz: 40 / 52 / 45 detector triggers, 0 syncs, 0 bursts, worst preamble correlation 0.087 |
| 600 s of noise (`slice_noise_long`) | 505 triggers, 0 syncs, worst 0.084 (threshold 0.11) |
| Slice bursts into the full-width receivers, and back (`diag_slice_cross`, 20 dB) | no burst delivered either way (CRC masks), but some preamble checks pass: 12 slice bursts gave Normal 1 and NormalWide 2 header attempts; 48 full-width bursts gave the slice receivers 0–2 (correlation up to 0.131), each a wasted header decode and ~0.9 s of `busy()` |

## Six slices

A study, not used by the link: can the passband hold six OFDM slices side
by side instead of three, and with which numerology? Centres 500, 900,
1300, 1700, 2100 and 2500 Hz (400 Hz apart, symmetric about 1500 Hz,
carriers 350–2650 Hz). Two candidates, both with their outer carriers at
±150 Hz, so the guard between neighbours is the same 100 Hz from carrier
to carrier (Slice: 150 Hz):

| | A: `Half` | B: `Sixth` | `Slice` (three) |
|---|---|---|---|
| Spacing, CP, symbol | 50 Hz, 4 ms, 24 ms | 25 Hz, 10 ms, 50 ms | 50 Hz, 4 ms, 24 ms |
| Carriers, occupied | 7 (bins −3..3), 350 Hz | 13 (bins −6..6), 325 Hz | 13, 650 Hz |
| Pilots per symbol, data carriers | 2, 5 | 3, 10 (the slice's pattern in carrier indices: 75 Hz × 100 ms) | 3, 10 |
| Energy per data cell, against Normal | 6.2× (7.9 dB) | 6.9× (8.4 dB) | 3.45× (5.4 dB) |
| A | 2464 samples (308 ms), 3 carriers, period 10 ms | the same | 1408 (176 ms), 7 carriers, 10 ms |
| Header | 36 symbols (864 ms) | 18 symbols (900 ms) | 18 symbols (432 ms) |
| `overhead_s`, header-only burst | 1.25 s, 2.11 s | 1.31 s, 2.21 s | 0.67 s, 1.10 s |
| Filter | 2521 taps, cutoff 197 Hz | the same | 1681, 370 Hz |
| Frequency pull-in | ±50 Hz (tested ±40) | ±50 Hz (tested ±40) | ±50 Hz |
| Data cells per second, one / all | 208 / 1250 | 200 / 1200 | 417 / 1250 |

Six slices carry no more than three: 1200–1250 data cells per second,
65–68% of Normal's 1833, the same as three Slices. What six buy is
granularity (six independent links, or six channels of one link) and
3 dB more energy per cell, at twice Slice's fixed overhead per burst.

**Sixth.** Long's spacing and guard on the slice's carrier and pilot
geometry; 18 header symbols are forced (17 × 20 = 340 slots < 352). A on
every 4th bin, as Half's every 2nd: 3 carriers with a 10 ms period (every
2nd bin would be 20 ms, ±25 Hz). A is as long as Half's: the same
bandwidth and power give the same energy per Hz. Receiver constants are
Long's (energy window 68, back-off 6, 99% plateau, delay grid 2–10 ms) and
the slice's estimator (40 pilots, time scale × 0.15). CRC masks 0x5C71 /
0xE2B9.

**Filter.** Flat to the outer carriers and ≥ 70 dB down where the
neighbour's carriers start: 2521 taps (Blackman sinc, cutoff 197 Hz),
±0.011 dB to 150 Hz, about −0.6 dB at 170 Hz and −1.8 dB at 180 Hz, at
least 74.9 dB down beyond 250 Hz (`sixth_filter_response`). The 100 Hz
transition takes 1.5× the slice's taps. Half had a 1681-tap filter with a
230 Hz cutoff, only 12.8 dB down at 250 Hz, where its neighbour's
carriers start; it now uses this one.

**Receiver changes** (layout fields, so the other numerologies are
unchanged):

- `sc_drop` 14: the detector waits 14 periods of A (140 ms) after a peak
  before it counts. A long window's metric wanders slowly; with Normal's 2
  periods the candidates came from local peaks hundreds of samples early
  or late on the metric's slopes, and B's search (±160) never reached the
  true B.
- `mf_a_free` (Sixth): B's matched filter leaves out A's three carriers.
  A's tail correlates with B at up to 0.23 (Half 0.38, Slice 0.14) without
  noise, and at −6 dB a lag inside A won the search about as often as B
  itself; the whole-preamble check then passes lags a whole number of A
  periods off, and the header fails. On Half this costs 3 of its 7 B
  carriers and measured worse, so Half keeps the full B.
- `mf_span` 320 for Sixth (a symbol is 400 samples), 160 for Half. A
  wider search (800) found header symbols that look like B.
- `drift` off for both: across 7 or 13 carriers a frequency-selective
  fade looks like a delay slope; on CCIR poor the clock estimate ran to
  its ±300 ppm limit, which moved Half's 14 s bursts out of their 4 ms
  guard (uncoded BER 1e-2–3e-1 at 30 dB, `diag_sixth_floor`). A real
  sound card's 50 ppm moves a 14 s burst by 0.7 ms: harmless in Sixth's
  10 ms guard, a third of Half's spare.
- `rho_full_min` 0.13 (slice 0.11): noise reached 0.10 in 120 s and
  0.139 in 3600 s (`sixth_noise_long`), hence Sixth's two syncs there.

**Sync at low SNR** (`diag_sixth_rho`: 40 QPSK bursts of 40 symbols per
point; delivered with the right header):

| | AWGN −12 / −10 / −8 / −6 dB | CCIR poor −12 / −10 / −8 / −6 dB |
|---|---|---|
| Half | 22 / 27 / 32 / 34 | 12 / 20 / 24 / 28 |
| Sixth | 22 / 29 / 35 / 38 | 11 / 20 / 26 / 34 |
| Slice | 8 / 26 / 36 / 39 | 3 / 12 / 24 / 30 |

Half's B, 7 carriers for 20 ms, is too weak a timing reference: its
header code would decode lower than Sixth's, but at −6 dB one burst in
seven still locks a few periods off. Sixth's B (13 carriers, 40 ms) has
Slice's time-bandwidth product and syncs as well as Slice from −8 dB up
and better below.

**Thresholds** (`sixth_table`; centre 1300 Hz, one K = 1440 codeword per
burst, link clip, real header, 20 bursts per point on a 2 dB grid, the
lowest SNR with ≥ 90% right headers and ≥ 90% decoded codewords; SNR
against the burst's own power in 2.4 kHz, as everywhere on this page; the
Slice rows are the table above, its NVIS row new):

| Level | | AWGN header / codeword | CCIR poor | NVIS disturbed |
|---|---|---:|---:|---:|
| QPSK 1/4 | Half | −6 / −6 | −4 / −4 | > 36 / > 36 |
| | Sixth | −8 / −8 | −4 / −4 | −4 / +4 ¹ |
| | Slice | −8 / −6 | −4 / −4 | +8 / > 36 |
| QPSK 1/2 | Half | −6 / −6 | −4 / 0 | > 36 / > 36 |
| | Sixth | −8 / −6 | −6 / −2 | −4 / +14 ¹ |
| | Slice | −8 / −2 | −4 / +2 | +8 / > 36 |
| 8PSK 2/3 | Half | −6 / 0 | −4 / +12 | > 36 / > 36 |
| | Sixth | −8 / 0 | −6 / +4 | +2 / +26 ¹ |
| | Slice | −8 / +4 | −4 / +8 | +8 / > 36 |
| 16QAM 3/4 | Half | −6 / +4 | −4 / +30 ² | > 36 / > 36 |
| | Sixth | −8 / +4 | −6 / +8 | −4 / +14 ¹ |
| | Slice | −8 / +6 | −4 / +14 | +8 / > 36 |

¹ Not a threshold: on NVIS disturbed (7 ms, 1 Hz) about one Sixth burst
in six fails at any SNR (codewords 60–95% from +10 to +36 dB, headers
75–100%). The same channel runs fail at every level and at short bursts
too; at 30 dB they report an SNR near −5 dB with the timing, frequency and
clock where the good ones have them (`diag_sixth_floor`), and Long decodes
every one of them. Not found: Sixth's estimator time scale (0.15 or 0.3)
and drift tracking make no difference.
² A loss floor: 75–90% from +12 to +36 dB, likely the channel estimate
from two pilots per symbol on 7 carriers. Half's AWGN rows were measured
with clock tracking still on (no effect without fading), its CCIR poor
rows with it off.

Six full-power slices are 7.8 dB more power in the channel than one
(three: 4.8 dB); the thresholds are per slice, against that slice's own
power. Against Normal (genie header; QPSK 1/2 +2 / +10, 16QAM 3/4 +12 /
+18) Sixth needs 8 dB less on AWGN and 10–12 dB less on CCIR poor, for
11% of the bit rate.

**Neighbours** (`sixth_six_slices`, `sixth_neighbours`; QPSK 1/2, link
clip, bursts staggered so all six overlap for most of their length, 10
runs per case; noise against one slice's power):

| Test | Half | Sixth |
|---|---|---|
| Reported SNR at 1300 Hz, no noise, unclipped: alone / five full-power neighbours | 31.2 / 30.4 dB | 41.3 / 38.4 dB |
| The same with the 1700 Hz neighbour 20 dB stronger | 19.1 dB | 30.1 dB |
| … and 20 Hz closer | 4.9 dB | 17.1 dB |
| All five neighbours 20 Hz closer (both sides) | 22.0 dB | 30.8 dB |
| Six slices at 0 / +2 / +4 dB: codewords decoded | 59 / 60 / 60 of 60 (one own burst missed at 0 dB) | 60 / 60 / 60 of 60 |
| One neighbour (900 Hz) 20 dB stronger, at 0 / +2 / +4 dB | 49 / 50 / 50 of 50 | 50 / 50 / 50 of 50 |
| … and 20 Hz towards 500 Hz | 49 / 50 / 50 of 50 | 50 / 50 / 50 of 50 |
| Bursts from the wrong slice | none | none |
| 600 s of noise at each centre (`sixth_noise_long`) | 273–305 triggers, 0 syncs, 0 bursts, worst correlation 0.127 | 292–324 triggers, 2 syncs (0.132, 0.139), 0 bursts |

Half's own floor of 31 dB is the filter: its 50 Hz carriers' spectra
fall off half as fast in Hz as Sixth's 25 Hz ones, and the 100 Hz guard
cuts more of them (on TX and RX). A neighbour 20 dB stronger and 20 Hz
off puts its edge carrier where the filter is only partly down: Half is
then left with 5 dB SINR (QPSK only), Sixth with 17 dB. QPSK 1/2 near its
threshold still decodes in both, so the overlap tests pass; the higher
levels would not survive such a neighbour in Half.

**CPU** (`sixth_cpu`, release, one core, one receiver over 60 s of noise
with six bursts): Normal 8.9 ms per second of audio, Slice 13.0, Half
15.2, Sixth 18.7; six Sixth receivers, by six times one, about 112 ms/s
(11% of a core), three Slices 39. The 2521-tap filter at 8 kHz output is most of it; a
two-stage decimator (a short anti-alias filter to 8 kHz, then ~440 taps
for the 100 Hz transition) should cut it several-fold (not tried).

**Recommendation: B (Sixth), for granularity, not for throughput.** It
syncs and decodes its header as well as Slice from −8 dB and better
below; its codewords need up to 6 dB less than Slice's on CCIR poor (4 dB
at QPSK 1/2; 3 dB more energy per cell); its 10 ms guard gets through NVIS disturbed, where
Half and Slice do not decode at all; it rejects its neighbours 10 dB
better than Half. A (Half) is limited by its timing reference (header
−6 dB on AWGN against Sixth's −8), its 31 dB own floor and 5 dB SINR next
to a strong neighbour, its 4 ms guard (no NVIS) and a 16QAM loss floor on
CCIR poor. What limits B: no gain in total capacity (six Sixths are 96% of
three Slices); 1.31 s of fixed overhead per burst, so a one-codeword QPSK
1/2 burst is 8.5 s against Slice's 4.1 s and header-only messages take
2.2 s; a loss floor of about one burst in six on NVIS disturbed (cause
open); six times a 2521-tap filter. Worth taking further only if the link
needs six independent channels: several stations at once in one SSB
channel, or finer slots than thirds.

## Width

`link::audio::WIDE` is false: the link uses the 2.45 kHz layouts, which
occupy 2.58–2.61 kHz by FCC §97.3(a)(8), inside the US 2.8 kHz HF data
limit. The 2.75 kHz layouts (`NormalWide`, `LongWide`) keep the spacing,
CP, preambles and receiver constants of their narrow twins and add
carriers; their normal-guard levels occupy 2.82–2.87 kHz
(`occupied_bandwidth_table` in `tests/ladder.rs`). Symbol windowing (1 ms
WOLA) only brings that to 2.82–2.83 kHz: the width is the outer carriers'
own spectra, not the symbol edges, so no windowing is used. Long's 25 Hz
carriers' skirts are already tight (LongWide 2.75 kHz), and its 10 ms CP
is kept whole for NVIS echoes. Both ends of a link must use the same
width: the two widths' CRC masks differ.

| | Normal | NormalWide | Long | LongWide |
|---|---:|---:|---:|---:|
| Carriers (bins) | 49 (±24) | 55 (±27) | 97 (±48) | 109 (±54) |
| Occupied | 2450 Hz | 2750 Hz | 2425 Hz | 2725 Hz |
| Default centre | 1500 Hz | 1600 Hz | 1500 Hz | 1600 Hz |
| Lowest valid centre | 1300 Hz | 1450 Hz | 1300 Hz | 1450 Hz |
| Robust pilots (step/shift/period) | 12/3/4: 5 per symbol | 18/3/6: 4 | 12/3/4: 9 | 12/3/4: 10 |
| Data carriers, robust / sparse | 44 / 46 | 51 / 52 | 88 / 88 | 99 / 99 |
| Header slots (one copy) | 352 | 408 | 352 | 396 |
| CRC masks data / header-only | 0x0000 / 0xB7E1 | 0x3C96 / 0x8D2B | 0x5A0F / 0xE45B | 0xC3A5 / 0x71D8 |
| Front-end filter cutoff | 1600 Hz | 1720 Hz | 1600 Hz | 1720 Hz |

NormalWide needs a pilot step dividing 54 (step 12 does not): step 18 with
period 6 keeps the 150 Hz frequency grid and samples it every 144 ms
(±3.5 Hz). The wide filter is flat (±0.05 dB) to ±1.44 kHz and more than
70 dB down beyond ±2.05 kHz (`wide_filter_response`). Since SNR is
against the mean power in 2.4 kHz, spreading the same power over 12% more
carriers costs ~0.5 dB per cell, for 12.5–16% more cells per symbol.

Several measurements below (pilot patterns, clipping, the low-PAPR mode,
drift) were made on the 2.75 kHz layouts; each says so.

The other narrow layouts, not used by the link: **Slim** (650 Hz with
Long's guard: 25 carriers 26.7 Hz apart, 22 data, 47.5 ms symbols, 8
header symbols), **Half** (350 Hz, 7 carriers, 5 data; A 7× Normal's
length, 36 header symbols; Sixth's filter and detector hold, see Six
slices) and **Quarter** (250 Hz, 5 carriers, 3 data; A 10× Normal's, 60
header symbols).

## Header-only bursts

`modulate_full(num, c, user, extra, &[], ..)` sends the preamble and the
header only: 32 bits (`user` and `extra`) for a tiny message. `RxBurst`
has `header_only = true`, `extra`, empty `llrs`, `constellation = Qpsk`
and `snr_db` measured over the header symbols.

- **Fields.** `extra` reuses the constellation, length and mode fields.
  What marks the burst as header-only is the CRC-16, XORed with a mask per
  numerology (table above). One Viterbi decode, two CRC comparisons; a
  numerology's receiver only knows its own two masks.
- **Two copies.** The header is sent twice. The first copy is an ordinary
  header and decodes on its own. The second continues the code sequence
  (slots 352..703 continue the 384 coded bits and wrap round, scrambled and
  interleaved within the copy), so every coded bit also lands on other
  carriers. When the first copy fails, the receiver waits for the second
  and decodes all 704 slots together, with the channel estimated from B and
  the pilots of both copies: +3 dB of energy and more diversity. Either way
  the burst is delivered at its end, so `busy()` covers it.
- **Why two.** SNR is against the mean power of the whole burst, and the
  boosted preamble is a large share of a short burst, so a one-copy
  header-only burst decodes worse than the header of a 1 s data burst. Two
  copies put it about 2 dB ahead of that header on AWGN and 2–3 dB on the
  fading channels, for 96 ms (Normal) or 100 ms (Long) more air time.
- **Cost.** When the first header copy of any burst fails, the receiver
  stays busy one header length longer and tries the two-copy decode on
  what follows. On a data burst that decode sees data symbols; it passes
  the header-only CRC with probability 2⁻¹⁶, and there are no field checks
  behind it.

## Two numerologies on one channel

The link runs Normal, Long and slice receivers on the same audio. Each
must ignore the others' bursts:

- **Detectors.** Normal's A repeats every 5 ms, Long's every 8 ms. A
  5 ms-periodic signal is also 10 ms periodic, so Long could not use
  10 ms; 8 ms shares no period. On the other's A the Schmidl-Cox lag
  product sums carriers with rotating phases: Long's A seen at lag 5 ms
  gives a normalised metric of 5·10⁻⁴, Normal's A at lag 8 ms 2·10⁻³,
  against thresholds of 0.035 and 0.02. The two combs share only 0 and
  ±1000 Hz.
- **Preamble check.** Each receiver accepts only a high whole-preamble
  correlation with its own A + B, at the frequency and timing it
  estimated; this rejects what does trigger, on the other numerology's
  data symbols as on noise.
- **Header.** The CRC masks differ by numerology, so even a false sync
  would need a 2⁻¹⁶ CRC accident.

## Pilot patterns

Every layout has a **robust** pattern (header symbols, and data symbols by
default) and a **sparse** one for data, named by a header bit. Both keep
the 150 Hz frequency grid that CCIR poor's 2 ms (plus the timing window)
needs; the sparse ones sample it less often in time:

| Layout | Robust | Overhead | Sparse | Overhead | Time grid, robust / sparse |
|---|---|---:|---|---:|---|
| Normal | step 12, period 4 | 10.2% | step 24, period 8 | 6.1% | 96 / 192 ms (±5.2 / ±2.6 Hz) |
| NormalWide | step 18, period 6 | 7.3% | step 27, period 9 | 5.5% | 144 / 216 ms (±3.5 / ±2.3 Hz) |
| Long, LongWide | step 12, period 4 | 9.3%, 9.2% | (the same) | | 200 ms |

The sparse patterns use a Doppler model grid of {0.3, 1, 2} Hz, below
their time Nyquist. The link uses robust pilots everywhere
(`level::Level::pilots`), because of `pilots_table` (`tests/ladder.rs`:
NormalWide, 40 bursts of ~1.5 s per point, codeword loss robust / sparse):

| Level | AWGN | CCIR poor |
|---|---|---|
| QPSK 1/2 | +2 dB: 0.26 / 0.26 | +6: 0.28 / 0.30; +8: 0.15 / 0.05 |
| 8PSK 2/3 | +8: 0.13 / 0.10 | +12: 0.39 / 0.38; +14: 0.04 / 0.09 |
| 16QAM 2/3 | +10: 0.25 / 0.27 | +14: 0.32 / 0.41; +16: 0.12 / 0.12 |
| 16QAM 3/4 | +11: 0.53 / 0.55 | +16: 0.31 / 0.38; +18: 0.12 / 0.18 |
| 16QAM 5/6 | +13: 0.07 / 0.06 | +16: 0.71 / 0.91; +18: 0.43 / 0.60; +20: 0.13 / 0.17 |
| 64QAM 2/3 | +15: 0.12 / 0.19 | +20: 0.23 / 0.22; +22: 0.08 / 0.12 |
| 64QAM 3/4 | +17: 0.00 / 0.02 | +22: 0.20 / 0.43; +24: 0.13 / 0.27 |
| 64QAM 5/6 | +18: 0.38 / 0.43 | +24: 0.54 / 0.66; +26: 0.18 / 0.28 |

On AWGN the two are the same within the noise. On CCIR poor the sparse
pattern costs the upper levels roughly 1 dB, more than the cells it frees
(on NormalWide one carrier in 51, +2%). A decision-directed channel
estimator would be the way to make the sparse pattern pay on fading.

## Transmitter: clipping and peak power

Every Ion waveform is built to peak at `ion_dsp::TX_LEVEL`. OFDM peaks are
reduced by clip-and-filter (Armstrong 2002), in the frequency domain per
header and data symbol: synthesize on a 4× oversampled grid (640 points),
clip the envelope at `clip_db` above the symbol's nominal RMS, take the
carriers back (out-of-band content is simply not re-synthesized), restore
the pilots, rescale the data cells by the Bussgang gain; 3 passes. A and B
are not clipped.

| Bursts | Clip |
|---|---|
| QPSK and 8PSK levels | the low-PAPR mode (below) |
| 16QAM levels | 5 dB (`QAM16_CLIP_DB`) |
| 64QAM levels | 8 dB (`QAM64_CLIP_DB`) |
| Header-only and control bursts (QPSK) | 4.5 dB (`QPSK_CLIP_DB`, `TxOptions::for_burst`) |
| `TxOptions::default()` | 6 dB (`DEFAULT_CLIP_DB`) |

**PAPR** (`PHY_TABLES=p tables`, 40 bursts of 1 s, preamble included).
Envelope = |complex baseband|, i.e. RF PEP over mean (what the SSB PA
sees); audio crest = peak/RMS of the 48 kHz audio (3 dB more for a real
signal); "mean power" is the audio mean power relative to the fixed peak
`TX_LEVEL`:

| Constellation | Clip | Envelope PAPR (burst peak) | 99.9% CCDF | Audio crest | Mean power vs peak |
|---|---|---:|---:|---:|---:|
| QPSK | none | 9.2 dB | 7.9 dB | 11.8 dB | −11.8 dB |
| QPSK | 6 dB | 7.0 dB | 6.5 dB | 9.7 dB | −9.7 dB |
| QPSK | 4.5 dB | 5.9 dB | 5.4 dB | 8.6 dB | −8.6 dB |
| QPSK | 4 dB | 5.7 dB | 5.2 dB | 8.4 dB | −8.4 dB |
| 8PSK | 4.5 dB | 6.1 dB | 5.5 dB | 8.7 dB | −8.7 dB |
| 16QAM | none | 9.2 dB | 8.0 dB | 11.9 dB | −11.9 dB |
| 16QAM | 5 dB | 6.2 dB | 5.8 dB | 9.0 dB | −9.0 dB |
| 64QAM | 8 dB | 8.4 dB | 7.8 dB | 11.2 dB | −11.2 dB |
| 64QAM | 6 dB | 6.9 dB | 6.5 dB | 9.7 dB | −9.7 dB |

Unclipped, the envelope peaks 9.2 dB over the mean, beyond the 6–8 dB
headroom of SSB PAs; a 6 dB clip brings it to 7.0 dB and raises mean
power by 2.1 dB at the same peak. Its cost at equal mean SNR is at most
0.2 dB of decoding (16QAM 3/4 on AWGN; nothing measurable for QPSK and
8PSK or on fading), so for a peak-limited transmitter it is a net gain of
about 1.9 dB. In uncoded BER at equal mean SNR (30 bursts) a 6 dB clip
is invisible for QPSK and 8PSK and on CCIR moderate; 16QAM on AWGN at
+20 dB goes from 3.2e-5 unclipped to 3.0e-4 (8 dB: 4.5e-5, 5 dB: 1.2e-3).

**Clip levels at equal peak power** (`ladder.rs::clip_table`: codeword
loss around each level's threshold, SNR against a tone at the burst's
peak, i.e. PEP − 3 dB, 40 bursts per point). Against a 6 dB clip, QPSK
and 8PSK gain 0.7–1.3 dB on AWGN and 0.4–1.1 dB on CCIR poor at 4.5 dB;
16QAM gains 0.1–0.6 dB at 5 dB (4.5 dB gains nothing more and leaves a
20 dB distortion floor under the top 16QAM rates). The in-band clip
distortion (`diag_clip_distortion`, the same for every constellation
within 0.3 dB) is 17.9 dB below the signal at a 4 dB clip, 19.7 at 4.5,
21.6 at 5 and 26 at 6; at 8 dB it is below the receiver's ~34 dB
estimation floor.

**64QAM** is clipped at 8 dB: a 6 dB clip's distortion floor (effective
SNR near 26 dB) costs it about 1 dB at equal mean power. `diag_clip64`
(NormalWide, sparse pilots, genie header, 4 bursts of 1 s; uncoded BER /
reported SNR):

| SNR | none | 9 dB | 8 dB | 7 dB | 6 dB |
|---:|---:|---:|---:|---:|---:|
| +20 | 1.9e-2 | 1.9e-2 | 2.0e-2 | 2.1e-2 | 2.6e-2 |
| +24 | 1.3e-3 | 1.3e-3 | 1.5e-3 | 2.5e-3 | 6.2e-3 |
| +28 | 0 | 0 | 9.5e-5 | 6.1e-4 | 2.4e-3 |
| +50 | 0 / 34.5 dB | 0 / 34.5 | 7.6e-5 / 34.4 | 2.9e-4 / 31.3 | 1.4e-3 / 26.5 |

At 20–24 dB, where 64QAM's codes work, 8 dB costs nothing measurable
against no clip. The price is peak power: an 8 dB clip sends about 1.5 dB
less mean power than 6 dB at the same PEP, which the simulator (SNR
against mean power) does not charge. Square 64QAM is Gray-mapped per axis
(levels −7…+7 / √42); demapping stays exact (log-sum-exp over all 64
points, each with its own variance σ² + MSE·|x|²). On AWGN 64QAM 5/6 needs
about 6 dB more than 16QAM 5/6 (`threshold_table`), the theoretical cost
of the denser constellation, so phase tracking and channel estimation show
no extra loss; the simulator has no oscillator phase noise.

### Low-PAPR data symbols

**The problem.** MFSK is constant-envelope, so its mean power is its
peak's. Plainly clipped OFDM QPSK averages 8.6 dB under the same peak, so
on the air OFDM goes out much weaker on average than MFSK. And a harder
clip of the data alone cannot raise the data's mean power at equal peak:
the preamble (B is sent 3 dB over the data's mean, with a Newman-phase
PAPR of ~2.6 dB) already peaks where 4.5 dB-clipped data does, which is
why the clip tables flatten below 4.5 dB. The preamble cannot change
(every receiver correlates against it), so the data symbols are raised
over it.

**DFT spreading.** DFT-s-OFDM (SC-FDMA) replaces the data cells of a symbol
by their DFT, so the data part of the symbol is a single-carrier signal.
With the scattered pilots in place the single-carrier property is broken
and unclipped peaks drop only 1.1–1.5 dB, but spread symbols keep 3–5 dB
more signal-to-distortion ratio than plain ones clipped to the same data
peak. More clip-and-filter passes leave lower peaks after the last
filtering; low-PAPR bursts use 6. Active constellation extension (Krongold
and Jones 2003) was worse than plain clipping here: its projection undoes
most of the clip.

**The mode** (`DataMode`, `TxOptions::low_papr`):

- **Header bits.** Three header bits carry the mode: DFT-spread, and a
  data gain code 0–3 in `GAIN_STEP_DB` = 0.75 dB steps. Plain bursts keep
  them zero.
- **Transmitter.** The data cells of each data symbol (in carrier order,
  skipping that symbol's pilots) go through a unitary DFT of the symbol's
  data-carrier count; the whole data symbol, pilots included, is scaled by
  the data gain; the clip is then applied with its level scaled by the
  gain, 6 passes. The header stays plain and unscaled but is clipped at
  3.5 dB (`LOW_PAPR_HDR_CLIP_DB`) so its QPSK peaks stay under the
  preamble's; A and B are unchanged. The link's settings
  (`low_papr_clip_db`, `low_papr_gain_db`): QPSK clip 2 dB, gain 2.25 dB;
  8PSK 3.5 dB, 1.5 dB (16QAM 4.5/0.75 and 64QAM 6/0.75 are defined but not
  on the ladder). At the same peak, QPSK data carries about 2.4 dB more
  mean power (2.6 dB on the slice), 8PSK 1.5 dB.
- **Receiver.** Once the header names the mode, the known value of every
  pilot in the data symbols is scaled by the gain, so the channel
  estimator's samples stay consistent across the header/data boundary.
  Plain cells are demapped against points scaled by the gain. Spread
  symbols are equalised per carrier (MMSE), `w_k = g_k*/(|g_k|² + v_k)`
  with `g_k` the channel estimate times the gain and `v_k` the cell's noise
  plus the estimate's error variance, despread by the inverse DFT, and
  each despread symbol demapped as `z = μ·x + e` with `μ = mean(w_k g_k)`
  and `var(e) = mean(|w_k g_k|² + |w_k|² v_k) − μ²` (`est::fde`). The
  decision-directed noise floor and the local noise keep working per
  carrier: the despread decisions are spread again and compared with the
  received cells. The delay profile of spread symbols uses the pilots'
  estimate: one wrong despread decision, spread again, puts a phase ramp
  across every carrier of its symbol, which the profile would read as an
  echo (7–28 ms on AWGN at +2 dB, `phy_lowpapr.rs::diag_delay_by_mode`).
- **SNR report.** The report takes out the mode's own clipping
  distortion as the receiver sees it (QPSK 11.5 dB below the signal; 10.2
  dB on the slice, whose 10 data carriers clip worse; 8PSK 17 dB): the
  reported SNR is within 0.5 dB of the channel from 0 to 20 dB on AWGN
  (`low_papr_snr_report_tracks_the_channel`; without it the distortion
  would cap a QPSK report near 12 dB).

**Codeword thresholds at equal peak power** (`ladder.rs::clip_table` with
`ION_CONFIGS`: SNR against a steady tone at the burst's peak, i.e.
PEP − 3 dB, seven SNRs 1 dB apart, 30 bursts per point, the SNR of 10%
codeword loss interpolated, the same seeds both ways; 2.75 kHz layouts).
"Plain" is the plain clip (4.5 dB QPSK and 8PSK, 5 dB 16QAM, 8 dB 64QAM),
"low-PAPR" the mode with the settings above:

| Level | AWGN plain | AWGN low-PAPR | Gain | CCIR poor plain | CCIR poor low-PAPR | Gain |
|---|---:|---:|---:|---:|---:|---:|
| slice QPSK 1/3 | +0.8 | −1.0 | **1.8** | +4.0 | +2.7 | **1.3** |
| slice QPSK 1/2 | +3.2 | +1.2 | **2.0** | +6.5 | +6.0 | 0.5 |
| long QPSK 1/4 | +4.3 | +2.2 | **2.1** | +10.0 | below +7 | ≥ 3 |
| long QPSK 1/2 | +8.0 | +6.2 | **1.8** | +12.0 | +11.0 | 1.0 |
| QPSK 1/3 | +5.3 | +3.3 | **2.0** | +11.2 | +9.0 | **2.2** |
| QPSK 1/2 | +8.0 | +6.1 | **1.9** | +12.5 | +11.6 | 0.9 |
| QPSK 2/3 | +10.1 | +8.2 | **1.9** | +16.6 | +14.6 | **2.0** |
| 8PSK 2/3 | +13.9 | +12.3 | **1.6** | +21.2 | +21.2 | 0.0 |
| 16QAM 2/3 (4.5/0.75) | +16.3 | +15.4 | 0.9 | +22.2 | +22.4 | −0.2 |
| 16QAM 3/4 (4.5/0.75) | +17.6 | +16.7 | 0.9 | +25.4 | +26.3 | −0.9 |
| 64QAM 3/4 (6/0.75) | +25.0 | +23.0 | 2.0 | floor | higher floor | worse |
| 64QAM 5/6 (6/0.75) | +26.5 | +25.0 | 1.5 | floor | higher floor | worse |

(The CCIR poor curves at 30 bursts are not monotone, so their points are
±0.5 dB. Other QPSK settings, clip 2.5 / gain 1.5 and clip 1.5 / gain
2.25, land within 0.2 dB of clip 2 / gain 2.25 on AWGN; plain cells with a
2 dB clip and 1.5 dB of gain gain 0.8–1.7 dB, so the spreading is worth
about 1 dB of the total.)

On AWGN every QPSK level gains 1.8–2.1 dB at equal peak power, 8PSK
1.6 dB. On CCIR poor the gain is smaller and less even (0.5–2.2 dB): the
spread symbols trade the per-carrier diversity of plain OFDM for the
equaliser's. 16QAM and 64QAM lose on CCIR poor (the linear equaliser's
residual interference rises with the constellation's density), so they
stay plain; `level::LADDER` uses the mode on every QPSK and 8PSK level.
At equal *mean* power the mode costs what its distortion and equaliser
cost; the link's thresholds ([link.md](link.md)) are mean-referenced and
include it.

## Receiver

Streaming, per baseband sample: `process` works with any chunk size
(tested from 1 sample to the whole stream) and allocates only the `llrs`
of each delivered burst. One `PhyRx` decodes one numerology. The numbers
below are Normal's; Long's differ as listed under Long numerology (lags
of 64/128/256 samples, a ±160-sample B search, a 68-sample energy window
with a 99% plateau, noise smoothed over ±12 carriers, decision-directed
noise over ±4).

1. **Front end.** The impulse blanker (`blanker.rs`) watches the complex
   envelope of 100–2900 Hz around the centre against a running mean of
   the clean samples. A run that starts 12 dB above the mean (complex
   Gaussian noise passes that once in 10⁷ samples; clipped OFDM bursts
   peak 4.5–8 dB above their mean), continues while 6 dB above and ends
   within 20 ms is an impulse and is zeroed; a longer run is a new level (a
   burst starting, a fade ending) and moves the mean. The audio comes out
   24 ms late and the reported start times take that out. Then mix down by
   `center_hz`, 481-tap low-pass, decimate to 8 kHz, into a 2.05 s ring
   buffer with absolute sample indices. A symbol whose FFT window was
   partly blanked (fraction b) is scaled by 1/(1 − b), and its cells get
   the noise the gap adds. `set_blanker(false)` turns it off.
2. **Detection** (Schmidl & Cox 1997): running lag-40 autocorrelation over
   a 264-sample window, f64 sums (recomputed exactly every 2048 samples),
   metric |P|²/R² > 0.035; the peak (plateau) is tracked and queued as a
   candidate with a coarse frequency (±100 Hz). The detector keeps running
   while a burst is being received; candidates queue, so a false trigger
   can never hide a real burst that starts right after it.
3. **Sync** for each candidate:
   - matched filter of B over ±80 samples at the coarse frequency (must
     exceed ρ = 0.06, a cheap pre-filter);
   - timing: the 24-sample window that captures the most matched-filter
     energy (middle of its plateau); the FFT window starts 4 samples
     before it, so any channel whose energy fits in 3.5 ms is ISI-free;
   - frequency: lag 40 → 80 → 160 autocorrelation chain over A's clean
     part, then a data-aided fit of the phase slope of A+B (64 ms of known
     signal) in six segments, worth ~1 dB of header sensitivity at low
     SNR;
   - accept: normalised correlation of the whole preamble (A+B, 544
     samples) at the refined frequency, best lag in the energy window,
     ≥ 0.08. Its noise floor is 3.4× lower than B alone (noise peaks: 0.063
     worst in 120 s), and it copes with two-path fading;
   - noise: 4 overlapping 160-sample DFTs inside A; its 36 empty bins per
     window give σ² per bin, smoothed over ±6 carriers and shrunk towards
     the global mean.
4. **Header**: DFTs of B and the header symbols (one fixed time-domain
   derotation per burst, phase continuous), channel estimate (below) from
   B and the header pilots, exact QPSK LLRs, de-scramble, combine the
   repetitions, Viterbi, CRC (data or header-only mask) and, for data
   headers, field checks. If it fails, the receiver collects one more
   header length and decodes both copies as a header-only burst. There is
   no frequency correction here: one from B and the header pilots is
   noisier at low SNR than the preamble estimate (it cost about 1.5 dB of
   header sensitivity); the Wiener model absorbs the residual and the data
   stage tracks it over the burst.
5. **Data**, when the last symbol has arrived (`phy/rx/demod.rs`):
   - residual frequency: products of pilot pairs 4 symbols apart, in
     blocks of 8 symbols, unwrapped block to block (pilot pairs alias every
     1/(lag·Ts): 10.4 Hz Normal robust, 4.6 Hz sparse, 5 Hz Long),
     weighted least-squares line (offset; also drift for bursts ≥ 2 s),
     applied as a phase ramp;
   - channel: linear MMSE (Wiener) interpolation from the 20 nearest
     pilots (Hoeher, Kaiser & Robertson 1997), real sinc correlation model
     with a uniform delay profile and a uniform Doppler spectrum (the
     "robust" design of Li, Cimini & Sollenberger 1998). Every burst picks
     (Doppler, delay span) from {0.3, 1, 2.5} Hz × {1, 2, 4} ms by
     leave-one-out cross-validation on its own pilots. The weights are
     rescaled to unit sum (the plain MMSE weights shrink the estimate,
     which biases 16QAM), and the error variance is computed exactly for
     those weights, then scaled up if cross-validation shows the model to
     be optimistic. On AWGN the implementation loss at 3 dB SNR is
     ~0.2 dB (a fixed robust model loses ~2.5 dB);
   - noise: per carrier, the larger of the preamble estimate, a
     decision-directed estimate on the data cells (residual minus the
     channel-estimate error, which catches what the preamble cannot see:
     TX clipping noise, Doppler ICI, timing error), and a **local** level
     per symbol and carrier. Band noise is not steady (a carrier keying on
     a few cells, a crash across a symbol, the radio's AGC); a noise level
     held for the whole burst would make the LLRs of the cells hit then far
     too sure, and the decoder does not survive confident errors;
   - LLRs: exact (log-sum-exp over all points), each point with its own
     variance σ²_c + MSE·|x|², so channel-estimate error enters the LLR;
     clamped to ±40;
   - SNR: Σ over carriers of (mean |Y|² − σ²_c) against σ² in 48 bins
     (2.4 kHz), with the sender's known clip distortion left out
     (`clip_distortion_db`, entered 0.6 dB stronger for the harder clips,
     since through noise the decision-directed estimate sees a little more
     of it). Without that, a 6 dB clip would cap every report near 26 dB
     and hide the SNR 64QAM needs; the remaining ceiling, ~34 dB, is the
     estimator's own;
   - channel measurement for the link's report (`crate::chan`,
     [link.md](link.md)): delay spread, Doppler spread and effective SNR.

`start_sample` is the stream index of `modulate()`'s first sample (from the
first path's timing, TX and RX filter delays removed): within ±7 samples
at 48 kHz in the tests, plus the channel's own delay. `freq_offset_hz` is
the mean offset over the burst. `busy()` is true from the acceptance of a
preamble (about 70 ms into a Normal burst, 140 ms into a Long one) until
the burst is delivered or its header fails (a failed first header copy
keeps it busy one header length longer, for the header-only retry); the
detector's triggers on noise do not make it busy.

### Sample-clock drift

Two sound cards (or a sound card and a TCI stream) never run at exactly
the same rate; tens of ppm are common, and the burst then arrives stretched
or squeezed. At 100 ppm the longest Normal burst (24.6 s) drifts 2.5 ms,
five times the CP's spare margin (~0.5 ms). The receiver tracks it
(`phy/rx/drift.rs`):

- **Model.** A clock offset `ε` delays symbol `g` by `d(g) = ε·g·sym`
  samples, which turns carrier `k` by `e^{−j2πk·d/n}` (the `f_k·ε`
  frequency scaling of the carriers; the common part, `ε` times the audio
  centre, is an ordinary frequency offset).
- **Estimate.** As each symbol arrives, its pilots are paired with pilots
  on the same carrier 1, 2, 4… pattern periods earlier, up to 0.45 s (CCIR
  poor decorrelates to 0.45 at 0.4 s). After the common phase of each pair
  set is removed, the phase left across the band is
  `−2π(f − f̄)·ε·L·Ts`; a pooled least-squares fit over every pair so far
  gives `ε`, shrunk towards 0 by its own uncertainty (×4 for the
  overlapping pairs), so short or noisy bursts are left alone.
- **Correction.** The FFT window steps one sample whenever `ε·g·sym` has
  moved 0.7 samples past the current step, so no path leaves the CP. The
  step's phase slope is taken out of the stored grid; when the burst is
  complete the final `ε` (from the whole burst) takes the remaining drift
  out as a phase slope, before frequency tracking and channel estimation.
- `RxBurst::clock_ppm` reports the tracked offset (positive: the burst
  arrived stretched); `PhyRx::set_drift_tracking(false)` turns it off.

Measured (`diag_clock_drift`, QPSK, longest bursts: 1023 symbols
NormalWide, 400 symbols LongWide; 4 bursts per point, resampled with a
16-tap windowed sinc; uncoded BER, tracked ppm per burst):

| Channel | Numerology | Offset ppm | BER tracked | BER untracked | Tracked ppm |
|---|---|---:|---:|---:|---|
| AWGN 15 dB | NormalWide | 0 | 2.4e-6 | 2.4e-6 | 0 0 0 0 |
| AWGN 15 dB | NormalWide | −60 | 0 | 1.4e-5 | −60 −60 −59 −60 |
| AWGN 15 dB | NormalWide | +100 | 0 | 6.7e-4 | +100 +100 +100 +99 |
| AWGN 15 dB | NormalWide | +200 | 0 | 1.5e-1 | +200 +200 +200 +199 |
| AWGN 15 dB | LongWide | ±100 | 0 | 0 | ±99…100 |
| AWGN 15 dB | LongWide | +200 | 3.2e-6 | 3.2e-5 | +200 +200 +200 +200 |
| CCIR poor 20 dB | NormalWide | 0 | 8.0e-3 | 7.9e-3 | +1 0 −14 0 |
| CCIR poor 20 dB | NormalWide | −60 | 8.0e-3 | 1.2e-2 | −53 −60 −75 −61 |
| CCIR poor 20 dB | NormalWide | +100 | 8.1e-3 | 3.2e-2 | +105 +100 +81 +98 |
| CCIR poor 20 dB | NormalWide | +200 | 8.0e-3 | 1.9e-1 | +203 +201 +184 +200 |

On AWGN a single path tolerates more drift than the margin suggests (the
estimator absorbs part of the slope), but at 200 ppm it breaks; on CCIR
poor the second path walks out of the CP at 60 ppm already. Tracked, the
BER is that of a 0 ppm burst everywhere up to 200 ppm.
`sample_clock_drift_100ppm` checks ±100 ppm on Normal, NormalWide and
LongWide and that a clean clock is left alone.

### Long echoes and the channel measurement

Echoes past a 4 ms guard do not show in the decision-directed delay
profile (the decisions follow a channel estimate whose model spans at most
the guard); they show as excess decision-directed noise instead.
`tests/chan_meas.rs` measured 3.4–9 dB of excess on a 7 ms echo from
+8 dB, under 0.5 dB on AWGN and CCIR poor; above a 2 dB threshold the
receiver reports a 6 ms echo, and the link moves to the long guard or MFSK.

## Results

From `cargo test -p ion --release --test phy -- --ignored --nocapture
tables` (`PHY_RUNS` bursts per point, `PHY_TABLES` picks the tables; two
threads unless `ION_THREADS` says otherwise) and `numerology_tables`
(`PHY_NUMS`, `PHY_NUM_TABLES`, `PHY_PROFILES`). The BER and SNR tables
use the "genie header" hook (`PhyRx::set_genie_header`): detection, sync
and channel estimation run normally, only the header bits are given, so
the BER is not conditioned on header success; `(Nm)` counts bursts the
detector never found (excluded from the BER). Bursts are 1 s of data (42
symbols, Normal), sent with the default 6 dB clip.

### Uncoded BER

QPSK

| Channel | −4 | −2 | +0 | +2 | +4 | +6 | +8 | +10 | +12 | +14 | +16 | +18 | +20 | +22 | +24 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 2.9e-1 | 2.4e-1 | 1.8e-1 | 1.2e-1 | 7.3e-2 | 3.4e-2 | 1.1e-2 | 1.9e-3 | 1.2e-4 | 9.0e-6 | <9e-6 | <9e-6 | <9e-6 | <9e-6 | <9e-6 |
| CCIR good | 3.1e-1 (6m) | 2.8e-1 | 2.3e-1 | 1.9e-1 | 1.4e-1 | 1.0e-1 | 7.2e-2 | 4.8e-2 | 3.2e-2 | 2.2e-2 | 1.5e-2 | 9.8e-3 | 6.9e-3 | 4.8e-3 | 3.5e-3 |
| CCIR moderate | 3.1e-1 (8m) | 2.8e-1 (4m) | 2.3e-1 (1m) | 1.9e-1 | 1.5e-1 | 1.1e-1 | 7.7e-2 | 5.2e-2 | 3.4e-2 | 2.3e-2 | 1.5e-2 | 9.1e-3 | 5.9e-3 | 3.8e-3 | 2.5e-3 |
| CCIR poor | 3.3e-1 (5m) | 2.8e-1 (5m) | 2.5e-1 (2m) | 2.0e-1 (1m) | 1.6e-1 | 1.1e-1 | 8.0e-2 | 5.4e-2 | 3.5e-2 | 2.3e-2 | 1.5e-2 | 9.1e-3 | 5.7e-3 | 3.4e-3 | 2.2e-3 |

- AWGN QPSK sits ~0.2 dB from theory at low SNR. The per-cell Es/N0 is
  the 2.4 kHz SNR − 0.66 dB (48 bins of noise against 49 carriers
  including boosted pilots, and the boosted preamble's share of the
  burst's mean power).
- On the fading channels uncoded BER falls as 1/SNR, as Rayleigh fading
  should (CCIR good at 24 dB: 3.5e-3; Rayleigh QPSK theory at the
  effective ~22 dB is ~3e-3). The FEC and the burst-wide interleaver turn
  this into working links.
- The 16QAM AWGN floor near 1e-4 at 20–24 dB is the 6 dB clip.
- The same output has 8PSK and 16QAM: on AWGN 8PSK reaches 1.0e-3 at
  +16 dB and 16QAM 1.2e-3 at +18 dB; on CCIR poor at +24 dB, 6.6e-3 and
  1.0e-2.

### Coded

The lowest SNR (2 dB grid, 40 bursts, one K = 1440 codeword per burst,
genie header, 6 dB clip) at which at least 90% of codewords decode, and
the continuous user bit rate (raw rate × code rate, before burst
overhead):

| Level | User bit/s | AWGN | CCIR good | CCIR moderate | CCIR poor |
|---|---:|---:|---:|---:|---:|
| QPSK 1/3 | 1222 | 0 | +6 | +4 | +6 |
| QPSK 1/2 | 1833 | +2 | +8 | +8 | +10 |
| QPSK 3/4 | 2750 | +6 | +14 | +12 | +14 |
| 8PSK 2/3 | 3667 | +8 | +16 | +14 | +16 |
| 16QAM 3/4 | 5500 | +12 | +20 | +18 | +18 |

### Header decode probability (no genie)

1 s QPSK bursts (Normal), 60 bursts per point (`PHY_TABLES=h`); success =
burst delivered with the right `user` and constellation (detection, sync
and CRC all count):

| Channel | −10 | −8 | −6 | −4 | −2 | +0 | +2 | +4 | +6 | +8 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 0.00 | 0.00 | 0.27 | 0.90 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR good | 0.03 | 0.08 | 0.30 | 0.53 | 0.72 | 0.90 | 0.97 | 0.98 | 0.98 | 1.00 |
| CCIR moderate | 0.02 | 0.13 | 0.25 | 0.43 | 0.68 | 0.88 | 0.93 | 0.98 | 1.00 | 1.00 |
| CCIR poor | 0.00 | 0.10 | 0.27 | 0.47 | 0.68 | 0.85 | 0.95 | 0.98 | 1.00 | 1.00 |

The header decodes below QPSK rate-1/3 data on every channel: about 4 dB
lower on AWGN. On AWGN the limit is detection and sync near −6 dB as much
as the code. On the fading channels the losses at 0–4 dB are bursts whose
100 ms header lands in a fade across most of the band; CCIR good (0.5 ms,
0.1 Hz) is nearly flat fading, so it has the least frequency diversity.

Header-only bursts (`PHY_NUM_TABLES=h PHY_NUMS=wide PHY_PROFILES=ap`, 60
bursts per point, 2.75 kHz layouts):

| Channel | Burst | −8 | −6 | −4 | −2 | 0 | +2 |
|---|---|---:|---:|---:|---:|---:|---:|
| AWGN | NormalWide | 0.08 | 0.63 | 0.93 | 1.00 | 1.00 | 1.00 |
| AWGN | LongWide | 0.03 | 0.42 | 0.88 | 1.00 | 1.00 | 1.00 |
| CCIR poor | NormalWide | 0.13 | 0.32 | 0.57 | 0.82 | 0.93 | 0.98 |
| CCIR poor | LongWide | 0.13 | 0.33 | 0.62 | 0.77 | 0.88 | 0.95 |

### False detections

| Test | Detector triggers | Syncs (header attempts) | Bursts |
|---|---:|---:|---:|
| 60 s white noise (`noise_only_gives_no_bursts`) | 240 | 0 | 0 |
| 600 s white noise (`noise_long`) | 2510 | 0 | 0 |

The detector fires ~4 times a second on noise; the whole-preamble check
rejects every one (worst noise value 0.063 against the 0.08 threshold in
120 s), and the CRC-16 plus field checks (constellation code, non-zero
length) stand behind that.

### SNR estimate

Mean `snr_db` reported for the QPSK bursts of the BER table (6 dB clip):

| Channel | −4 | −2 | +0 | +2 | +4 | +6 | +8 | +10 | +12 | +14 | +16 | +18 | +20 | +22 | +24 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | −4.3 | −2.3 | −0.3 | 1.8 | 3.8 | 5.7 | 7.6 | 9.5 | 11.5 | 13.5 | 15.4 | 17.2 | 19.0 | 20.5 | 21.9 |
| CCIR good | −4.1 | −3.3 | −1.4 | 0.6 | 2.6 | 4.6 | 6.5 | 8.5 | 10.4 | 12.4 | 14.2 | 16.1 | 17.8 | 19.4 | 20.8 |
| CCIR moderate | −4.0 | −2.4 | −0.9 | 0.9 | 2.9 | 4.9 | 6.8 | 8.8 | 10.7 | 12.6 | 14.5 | 16.3 | 18.0 | 19.5 | 20.9 |
| CCIR poor | −4.6 | −2.6 | −0.9 | 1.1 | 3.1 | 5.0 | 7.0 | 8.9 | 10.8 | 12.7 | 14.6 | 16.3 | 18.0 | 19.5 | 20.8 |

On AWGN the report is 0.3 dB low (the preamble's share of the burst power,
which the estimate over the data symbols does not see), and falls short
above ~18 dB with the clip. Unclipped bursts track within 0.5 dB from 0 to
24 dB, and clipped ones within 1.5 dB to 30 dB with the known clip
distortion left out (`snr_estimate_tracks_awgn`; QPSK reports 23.4 dB at
24 dB and 27.9 dB at 30 dB). On fading, a burst's own SNR varies with the
fade and the mean of dB values sits 1–1.5 dB under the long-term SNR, as
averaging log-Rayleigh values should.

### LLR calibration

Measured bit error rate of the hard decisions in bins of |L| against the
error rate the LLRs claim, 1/(1+e^|L|) (genie header, 30 bursts):

| Case | \|L\| | Bits | Measured | Claimed |
|---|---|---:|---:|---:|
| QPSK AWGN +2 dB | 0–0.5 | 10755 | 0.4353 | 0.4383 |
| | 1–2 | 22313 | 0.1643 | 0.1860 |
| | 3–4.5 | 24612 | 0.0234 | 0.0263 |
| | 6–9 | 6965 | 0.0032 | 0.0012 |
| QPSK CCIR poor +8 dB | 0–0.5 | 10181 | 0.4332 | 0.4412 |
| | 1–2 | 13032 | 0.1681 | 0.1896 |
| | 3–4.5 | 12537 | 0.0195 | 0.0256 |
| | 6–9 | 15296 | 0.0007 | 0.0008 |
| 8PSK CCIR moderate +14 dB | 1–2 | 11396 | 0.1774 | 0.1885 |
| | 6–9 | 19403 | 0.0008 | 0.0008 |
| 16QAM CCIR good +18 dB | 1–2 | 11282 | 0.1847 | 0.1898 |
| | 6–9 | 18703 | 0.0010 | 0.0008 |

Slightly under-confident in the middle bins (safe for belief propagation)
and right in the tails, except the AWGN 6–9 bin, which comes from bursts
whose noise estimate (72 preamble bins) came out low.
`llrs_are_calibrated_on_awgn` and `ccir_poor_moderate_snr` check this per
bin (within 0.6–1.6× where 30 or more errors are expected).

### CPU (release, one core)

| | Cost |
|---|---|
| RX, 62 s of back-to-back 3 s bursts (CCIR moderate, 10 dB), 10 ms chunks | 10.4 ms per second of audio (1.0% of a core) |
| RX, slowest single 10 ms chunk (the one that completes a 3 s burst) | 27.5 ms |
| TX, unclipped | 4.7 ms per second of burst |
| TX, 6 dB clip | 10 ms per second of burst |

(`cpu_cost`.) The steady part is mostly the 481-tap front end. Channel
estimation, model selection and demapping run once, when a burst's last
symbol arrives, so `process` has a spike of about 9 ms per second of burst
at that moment; call it from a worker thread, not the audio callback.

### Long against Normal

`numerology_tables` (`PHY_RUNS=40`): 6 dB clip, 1500 Hz centre, no
frequency offset, real header; QPSK, one K = 1440 codeword per burst, a
burst counts when its header is right and the codeword decodes.

Burst lengths (data symbols and air time, preamble, header, ramp and
filter tails included):

| | Normal | Long |
|---|---:|---:|
| Header-only | 272 ms | 342 ms |
| Rate 1/4 (5760 bits) | 66 symbols, 1760 ms | 33 symbols, 1892 ms |
| Rate 1/3 (4320 bits) | 50 symbols, 1376 ms | 25 symbols, 1492 ms |
| Rate 1/2 (2880 bits) | 33 symbols, 968 ms | 17 symbols, 1092 ms |

Codeword success at rate 1/4 (rates 1/3 and 1/2 follow the same pattern
some dB higher; the 90% points are below the table):

| Channel | Numerology | −4 | −2 | +0 | +2 | +4 | +6 | +8 | +10 | +12 | +14 | +16 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | Normal | 0.00 | 0.85 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| AWGN | Long | 0.00 | 0.62 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | Normal | 0.00 | 0.15 | 0.57 | 0.85 | 0.95 | 0.97 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | Long | 0.00 | 0.05 | 0.45 | 0.75 | 0.90 | 1.00 | 1.00 | 1.00 | 1.00 | 0.97 | 1.00 |
| NVIS disturbed (7 ms, 1 Hz) | Normal | 0.00 | 0.00 | 0.03 | 0.05 | 0.15 | 0.15 | 0.25 | 0.15 | 0.20 | 0.15 | 0.17 |
| NVIS disturbed (7 ms, 1 Hz) | Long | 0.00 | 0.05 | 0.35 | 0.60 | 0.88 | 1.00 | 1.00 | 0.97 | 1.00 | 1.00 | 1.00 |
| Low-lat disturbed (6 ms, 10 Hz) | either | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |

- **NVIS disturbed (7 ms, 1 Hz): Long works, Normal does not.** Normal
  never gets above ~25% at any SNR (the echo overruns its CP: an ISI
  floor). Long reaches 90% around +4 dB (rate 1/4), +5 dB (1/3) and
  +10 dB (1/2).
- **Elsewhere Long costs a little:** about 0.3–0.5 dB on AWGN (a 20%
  instead of 16.7% CP, a longer boosted preamble in the burst mean, fewer
  pilots per carrier); on CCIR poor within the 40-burst noise, perhaps
  0.5 dB.
- **NVIS needs ~2–4 dB more than CCIR poor at rate 1/2** with Long,
  although a 7 ms echo gives more frequency diversity: the 2-D channel
  estimate has few useful pilots when both the delay (coherence ~140 Hz)
  and the Doppler are large.
- **Low-latitude disturbed (6 ms, 10 Hz): nothing, either numerology.**
  σ = 5 Hz is far beyond both pilot grids' Doppler Nyquist (±2.5 / ±5.2
  Hz) and gives −9 dB (Long) / −15 dB (Normal) of ICI. This is for the
  MFSK floor.

On the 2.75 kHz layouts (`PHY_NUMS=wide`) a one-codeword burst is 12%
shorter and each cell gets ~0.5 dB less energy: LongWide reaches 90% at
about −1 dB (AWGN) and +4 dB (CCIR poor) at rate 1/4, +3.3 and +8.5 dB at
rate 1/2.

**Cross-triggering.**

| Test | Bursts | Preamble checks in the other receiver | Its syncs | Its bursts | Worst correlation / threshold |
|---|---:|---:|---:|---:|---:|
| Normal → Long (`cross_trigger_long`: AWGN, CCIR good/poor, NVIS; 0–30 dB; ±50 Hz) | 240 | 567 | 0 | 0 | 0.043 / 0.055 |
| Long → Normal (same) | 240 | 1303 | 0 | 0 | 0.073 / 0.08 |
| 600 s of noise, Normal receiver (`noise_long`) | | 2510 detector triggers | 0 | 0 | 0.070 / 0.08 |
| 600 s of noise, Long receiver | | 1344 detector triggers | 0 | 0 | 0.036 / 0.055 |

Each receiver delivers all of its own bursts in the same streams, and
`both_receivers_on_one_stream` (eight Normal and Long bursts back to back,
no gaps, data and header-only) gives each receiver exactly its own bursts.
The cross-trigger tests pass for the wide pair as for the narrow one.

### Clicks

`tests/blanker.rs`: clean bursts keep their bits and timing through the
blanker, OFDM and MFSK (`clean_bursts_keep_their_bits_and_timing`), and
slice QPSK 1/3 bursts at −4 dB, which white noise alone does not stop,
under 10 clicks a second 30 dB up, are lost without the blanker and kept
with it (`clicks_are_survived_with_the_blanker`). Without the blanker, crashes
get *worse* as the SNR rises, because the receiver's noise level falls
with the floor while the clicks do not.

## Tests

`crates/ion/tests/phy.rs` checks the numerologies and burst lengths, the
filters, clean round trips of every numerology, pilot pattern and
constellation, header fields and header-only bursts, timing, frequency
offsets and drift, streaming with any chunk size, back-to-back bursts,
`busy()`, noise rejection, cross-triggering between numerologies, LLR
calibration, the SNR report and sample-clock drift. `phy_slice.rs` covers
the slice (layout, filter, neighbours, noise), `phy_sixth.rs` the six-slice
candidates Half and Sixth (`sixth_table`, `sixth_neighbours`,
`sixth_noise_long`, `sixth_cpu` and `diag_sixth_rho` are its ignored
measurements), `phy_lowpapr.rs` the
low-PAPR modes and their SNR report, and `blanker.rs` the blanker. The
ignored tests (`tables`, `numerology_tables`, `cpu_cost`, `noise_long`,
`cross_trigger_long`, the `diag_*` tests, and `clip_table`, `pilots_table`,
`threshold_table` and `occupied_bandwidth_table` in `ladder.rs`) produce
the numbers on this page.

## Sources

- T. M. Schmidl, D. C. Cox, "Robust frequency and timing synchronization
  for OFDM", IEEE Trans. Commun. 45(12), 1997: repeated-half preamble,
  autocorrelation metric (here with a quarter-symbol period).
- P. Hoeher, S. Kaiser, P. Robertson, "Two-dimensional pilot-symbol-aided
  channel estimation by Wiener filtering", ICASSP 1997.
- Y. Li, L. J. Cimini, N. R. Sollenberger, "Robust channel estimation for
  OFDM systems with rapid dispersive fading channels", IEEE Trans. Commun.
  46(7), 1998: uniform delay/Doppler (sinc) correlation model.
- ETSI EN 300 744 (DVB-T): scattered pilot pattern (every 12th carrier,
  shifted by 3 per symbol) and the 4/3 pilot boost.
- J. Armstrong, "Peak-to-average power reduction for OFDM by repeated
  clipping and frequency domain filtering", Electronics Letters 38(5),
  2002.
- B. S. Krongold, D. L. Jones, "PAR reduction in OFDM via active
  constellation extension", IEEE Trans. Broadcast. 49(3), 2003.
- R. Y. Shao, S. Lin, M. P. C. Fossorier, "Two decoding algorithms for
  tailbiting codes", IEEE Trans. Commun. 51(10), 2003 (WAVA).
- ITU-T V.41 / X.25: CRC-16-CCITT, polynomial 0x1021, initial value 0xFFFF.
- S. Boyd, "Multitone signals with low crest factor", IEEE Trans. Circuits
  Syst. CAS-33(10), 1986, and D. J. Newman, "An L¹ extremal problem for
  polynomials", Proc. Amer. Math. Soc. 16, 1965: quadratic (Newman)
  phases, used for the low-PAPR A and B symbols.
- ITU-R F.1487 / CCIR Rec. 520 channel profiles, as implemented in
  `ion_dsp::channel`.
- Standard textbook material (e.g. Proakis, *Digital Communications*):
  Gray mapping, exact and max-log LLRs, BICM (Caire, Taricco, Biglieri,
  IEEE Trans. IT 44(3), 1998), single-carrier frequency-domain
  equalisation.

## Open points

- **Not on real radios.** Oscillator phase noise, TX/RX filter ripple,
  amplifier non-linearity, real sound-card jitter and TCI streams that
  drop or repeat blocks are not simulated. Each eats into the ~20–28 dB
  64QAM needs; expect 64QAM only on quiet, steady paths with clean
  transmitters.
- **Sample-clock drift at low SNR.** The tracker is exact on clean
  channels, but near the QPSK floor its variance estimate is optimistic
  (the pilot pairs overlap more than the ×4 allows): on 8 s bursts with no
  offset it reported up to 41 ppm at −2 dB AWGN and 69 ppm at +2 dB CCIR
  poor (`diag_clock_low_snr`, 16 bursts), with no change in BER (a few
  samples of window shift inside the CP are harmless). A sound card's
  offset is the same for every burst, so the link could average it across
  bursts.
- **Header in flat fades.** On CCIR good the header fails in a few per
  cent of bursts at +2 dB. Data fails more often there, so the header is
  not the limit, but a header failure loses the whole burst; more
  frequency diversity would be the next step.
- **Detection thresholds** (0.035 / 0.06 / 0.08; Long 0.02 / 0.05 / 0.055
  and its 99% plateau) are set from the measured noise and signal
  distributions (`diag_rho`, `noise_long`, the cross-trigger tests), not
  from a closed form.
- **Cross-numerology margin.** The preamble check of the other
  numerology's receiver stays below its threshold, but not by much: on
  Long bursts the Normal receiver's correlation reached 0.073 against 0.08
  (240 bursts). Long OFDM symbols carry all of Normal's carrier
  frequencies (25 Hz grid ⊃ 50 Hz grid), so they look more like Normal's
  template than noise does. A false sync would cost a header decode
  (≈ 100 ms of `busy()`), with the numerology-specific CRC masks (2⁻¹⁶)
  behind it; a stricter Normal threshold would cost Normal sensitivity.
- **Header-only false accepts.** Header-only bursts have no field checks
  behind the CRC, and the receiver also tries the two-copy decode after
  every failed data header: 2⁻¹⁶ per attempt, so `extra` is untrusted input to
  the link.
- **Frequency range.** Acquisition is unambiguous to ±100 Hz; tested
  ±50 Hz. Drift is tracked as offset plus slope per burst; faster or
  non-linear drift within one burst is not handled.
- **Flutter** (10 Hz Doppler) is outside the pilot design (±5.2 Hz
  Nyquist, model up to 2.5 Hz) and does not work; the MFSK floor covers
  it.
- **64QAM at a 6 dB clip.** At equal peak power it measured 0.5–1 dB
  better than the 8 dB clip on AWGN, against ~1 dB worse at equal mean
  power; adopting it needs the 64QAM thresholds re-measured.
- **Burst-end processing.** The channel estimate and LLRs for a burst are
  computed when its last symbol arrives: 27.5 ms for a 3 s burst, about
  0.2 s for the 24 s maximum, inside one `process` call. Caching the
  Wiener weights of the burst interior (the pilot pattern repeats every 4
  symbols) would cut this several-fold.
- **Long at 2 Hz spread is marginal.** On a 7 ms two-path channel with a
  2 Hz two-sigma spread (`diag_long_2hz`, 20 bursts per point) Long
  codewords decode in 0.90 (rate 1/4), 0.90 (1/3) and 0.85 (1/2) of bursts
  at +20 dB and do not reach 100%; uncoded QPSK BER at 20 dB is 7.2e-2
  against 1.3e-2 at 1 Hz (`diag_long`). The pilot grid samples the
  channel every 200 ms (±2.5 Hz), and with a 7 ms echo little help comes
  from neighbouring carriers; a period-3 or period-2 pilot pattern for
  Long would be the fix if real NVIS turns out this fast. Real NVIS often
  has more than two modes and the second hop arrives weaker, which is
  easier than the ITU profile.
- **Channel estimation at low SNR.** At 6 dB (genie header, uncoded QPSK)
  both numerologies lose about 3 dB to ideal channel knowledge on 2 ms /
  1 Hz (BER 0.12–0.13 against ~0.06), Long on 7 ms / 1 Hz a little more.
  Denser pilots for Long (every 6th carrier, period 3, 17.5% overhead)
  might help NVIS by about 1 dB for 9% of throughput; not tried.
- **Low-PAPR mode, not measured:** CCIR good and moderate for the 16QAM
  and 64QAM settings; an iterative (turbo) equaliser for the spread
  symbols, which would likely take back the fading loss; low-PAPR
  header-only and control bursts (their preamble and header dominate).
- **Test hooks.** `PhyRx::set_genie_header`, `set_debug`, `stats`,
  `Genie`, `CellDebug`, `debug_tx_cells`, `front_end_taps` and
  `set_drift_tracking` (here and on `MfskRx`) are `#[doc(hidden)]` public
  items for the tables and diagnostics.
