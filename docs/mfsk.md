# Ion MFSK floor: noncoherent bursts for bad HF

`crates/ion/src/phy_mfsk.rs` (modulator and types), `phy_mfsk/frame.rs`
(burst layout and header), `phy_mfsk/conv.rs` (header code),
`phy_mfsk/rx.rs` with `rx/detect.rs`, `rx/acquire.rs`, `rx/model.rs` and
`rx/data.rs` (receiver), `phy_mfsk/id.rs` (frame decoding),
`phy_mfsk/util.rs`. `MfskLayout` stretches both modes in time for a
narrower band (Below the floor, a study). This is the robust floor of
the Ion ladder: every link connects here and climbs to the OFDM levels
([phy.md](phy.md)) when the channel allows.

The OFDM levels need coherent channel estimates, and those fail on flutter
and long echoes: on CCIR flutter and the disturbed ITU channels no OFDM
level works at any SNR ([phy.md](phy.md), Long against Normal).
Noncoherent MFSK only needs each tone's *energy*, which survives 10–30 Hz
of Doppler, and with a guard interval it also survives 7 ms echoes.

**Conventions.** Everything below is measured on the Watterson simulator
`ion_dsp::channel` (`crates/ion-dsp/src/channel.rs`). SNR is the mean
signal power of the whole burst against white Gaussian noise in 2.4 kHz,
`ion_dsp::dsp::noise_sigma(p, snr, 2400, 48000)`. Nothing here has been
measured on the air.

## Waveform

```
| preamble 16 × 28 ms | header H × T | data D × T | 1 ms ramp |   T = 28 ms (R0), 14 ms (R1)
```

Both modes send one tone at a time (constant envelope); each tone is held
for its whole symbol and frequency changes are swept over the first 1 ms,
phase continuous. The preamble is always R0's.

| Parameter | Value | Why |
|---|---|---|
| Analysis window (R0) | 20 ms (160 samples at 8 kHz) | Doppler: a CCIR-flutter tap (2σ = 10 Hz, Gaussian) keeps a correlation of exp(−2(π·5 Hz·20 ms)²) = 0.82 over the window, so ~95% of a tone's energy stays in its 50 Hz bin. At 30 Hz (high-latitude disturbed) about 70% stays and the rest lands in the next bins, which the receiver models. Longer windows gain on AWGN but lose on flutter; 31 Hz bins (32 ms) would hold only ~45% at 30 Hz |
| Tone spacing (R0) | 50 Hz | Orthogonal over the window; 48 bins fill the 2.4 kHz band |
| Guard (R0) | 8 ms | Covers the 6–7 ms differential delay of ITU low-latitude disturbed, NVIS disturbed and high-latitude disturbed, plus the 1 ms frequency sweep. An echo up to ~7 ms late still shows the *same* tone inside the window: no intersymbol interference, no equaliser |
| Frequency change | 1 ms raised-cosine sweep, phase continuous, at the start of the guard | Keeps the spectrum compact and the envelope constant. A 2 ms sweep leaves only 6 ms of clean guard behind a 7 ms echo, and the long-echo channels then lose codewords at high SNR |
| Centre | `center_hz` = 1500 Hz (the link's); R0's tones span `center_hz` ± 1175 Hz (± 1225 Hz with their main lobes) | 325–2675 Hz at 1500 Hz, inside a 300–2700 Hz passband. Below a 1450 Hz centre the lowest tones near the channel simulator's 250 Hz Hilbert limit |
| Hopping | Each header and data symbol's tone set is rotated by a pseudo-random offset (0..47 bins, SplitMix64 of the symbol index), cyclically over the 48 bins | Every bin is used equally, so a notch or a narrow-band interferer hits different symbol values each time instead of always biasing the same one |
| Preamble | 16 symbols of a Welch Costas array (p = 17, primitive root 3), values 1..16 on bins 3c − 2 (150 Hz apart) | Sharp time-frequency ambiguity: any shifted copy matches at most one symbol |

### Modes

| | R0 | R1 |
|---|---|---|
| Tones per symbol | 1 of 32 (5 bits) | 1 of 32 (5 bits) |
| Symbol | 28 ms = 20 ms window + 8 ms guard | 14 ms, all window (no guard) |
| Grid | 48 bins of 50 Hz, `center_hz` ± 1175 Hz | 32 bins of 71.4 Hz (= 1 / 14 ms), `center_hz` ± 1107 Hz |
| Raw bit rate (data part) | 178.6 bit/s | 357.1 bit/s |
| Envelope | constant (0 dB PAPR) | constant |
| Header symbols (header-only / data) | 48 / 64 | 48 / 64 |
| Header-only burst | 1.793 s (86 064 samples) | 1.121 s (53 808 samples) |
| Overhead of a data burst (`info().overhead_s`) | 2.241 s | 1.345 s |
| One short frame (205 tones) | 5.740 s; burst 7.981 s | 2.870 s; burst 4.215 s |
| Most data per burst | 20 475 bits (4095 symbols, 117 s) | 20 475 bits (4095 symbols, 57 s) |
| Bandwidth | 2450 Hz | 2357 Hz |

**Why R1 has no guard.** With an 8 ms guard one tone per symbol cannot
pass ~230 bit/s at any symbol length (log2(M)/T with M ≤ 2400 Hz ·
window). R1 drops the guard instead: 14 ms symbols that are all analysis
window. An echo then lands inside every window; the receiver answers with
shorter windows at the end of each symbol (10 and 12 ms, chosen per burst
like R0's), and at R1's operating SNR the leftover intersymbol
interference is well below the noise. Several tones per symbol (on R0's
grid, keeping the guard) would cost 4.7 dB of envelope PAPR for three
tones, and in a peak-limited transmitter that much mean power.

The link's ladder uses R0 and R1 at rate 1/4 and R1 at the GF(32) code's
rates 1/3, 1/2 and 2/3 ([fec.md](fec.md), GF(32) short code), which fill
the gap between R1 and the OFDM slice levels ([link.md](link.md)).

### Data mapping

**Symbol-mapped data** (`modulate_symbols`; what the link sends). The
data part is a sequence of 5-bit code symbols (the GF(32) code's, 205 per
frame at rate 1/4). Each is XORed with a pseudo-random 5-bit value
(SplitMix64 of its index) and becomes one whole tone; the symbols are
interleaved over the burst (`i → a·i mod n`, `a` ≈ n/φ coprime with n).
Bit 15 of the header's length field marks the burst (lengths stay below
2¹⁵); the length is 5 bits per symbol, so the burst is as long as a
bit-mapped one of that many bits. The receiver gives the tone metrics in
code-symbol order (`MfskBurst::symbols`). No Gray labelling: on orthogonal
tones any labelling of a symbol-level code is the same.

**Bit-mapped data** (`modulate`). Bits are XORed with a fixed
pseudo-random sequence (SplitMix64 of the bit index), padded to whole
symbols with scrambled zeros, interleaved over the whole burst, and
Gray-mapped onto tone values (adjacent bins differ in one bit, so leakage
errors cost one bit). The data part is any number of bits; codeword
boundaries do not matter to the PHY, and every codeword is spread across
the burst, so a fade hits a few bits of many codewords.

### Header

| Burst | Fields | Bits |
|---|---|---|
| Header-only (`bits` empty) | user 16, extra 16, CRC-16 | 48 |
| Data | user 16, extra 16, nbits 16, CRC-16 | 64 |

The mode and the header type are not sent as bits: they select the CRC's
initial value (CRC-16-CCITT, polynomial 0x1021; four initial values), and
the receiver tries all four hypotheses. A header decoded under the wrong
hypothesis fails its CRC.

**Code.** A K = 15 tail-biting convolutional code whose every trellis step
emits a whole tone: 5 output bits = one 32-ary symbol, one information bit
per step, no tail (48 or 64 tones). The receiver runs the wrap-around
Viterbi algorithm (3 passes, `fec::conv`) directly on tone metrics (the
log-likelihood of each tone value), never on bit LLRs. That matters: for
noncoherent orthogonal 32-FSK at the header's operating point, the
coded-modulation capacity is about twice the BICM (bit-LLR) capacity (1.7
against 0.8 bit/symbol at Es/N0 = 4 dB; Design numbers). The generators
(register bit 0 = newest input; output bit i = parity of `g_i & register`)
0o42101, 0o77127, 0o60225, 0o42073, 0o64667 come from `fec::conv::search`,
a random search that maximises the free *symbol* distance (trellis steps
whose tone differs, which is what counts for orthogonal tones) and then
minimises the paths at it: d_s = 15 = K, the most a K = 15 code can have,
one path. Step outputs are interleaved over the header's tone slots. The
code sits on the finite-length normal approximation ([fec.md](fec.md),
Header codes). A decode costs 5–7 ms; a detector candidate tries up to 12,
so a trigger on noise costs up to ~80 ms of CPU (~15 a minute).

## Receiver

Streaming: `process` accepts any chunk size and gives identical output
(tested from 1 sample to the whole stream), because all work happens at
fixed points of the baseband sample count (every quarter symbol).

1. **Front end.** An impulse blanker (`ion::blanker`, `crates/ion/src/blanker.rs`)
   zeroes clicks and crashes up to 20 ms long that rise 12 dB over the
   running level (complex Gaussian noise passes 12 dB above its mean once
   in 10⁷ samples); the audio comes out 24 ms late and the reported start
   times take that out. Then an NCO mix-down by `center_hz`, a 511-tap
   Blackman-windowed sinc (cut-off 1560 Hz at 48 kHz: flat to ±1.30 kHz,
   stop band from ±1.82 kHz) and decimation by 6 to an 8 kHz complex
   baseband, computing only every sixth output (~8 M multiplies per
   second), into a 16 s ring buffer with absolute sample indices. The stop
   band matters for R1: the mix-down puts a mirror image of every tone f at
   −2·`center_hz` − f (−1.83 kHz at the closest, at a 1500 Hz centre).
   R0's grid is orthogonal to it; R1's 71.4 Hz grid is not, and a wider
   filter lets it bias R1's frequency tracker. A data symbol whose analysis
   window is more than half blanked is erased. `set_blanker(false)` turns
   the blanker off.
2. **Detector.** Every 7 ms (a quarter symbol), a 320-point DFT (two-stage
   5 × 64 FFT) of the last 20 ms gives a 25 Hz-resolution spectrum. The
   statistic sums the energies at the 16 Costas tones, symbol by symbol,
   for frequency offsets of −75..+75 Hz in 25 Hz steps; each term is
   divided by the larger of that bin's long-term level (~0.9 s) and the
   column's mean (so a steady carrier or a broadband burst such as OFDM
   cannot trigger it) and capped at 10 (so MFSK data that happens to match
   one or two Costas tones cannot either). Threshold 1.9 (noise mean 1);
   the uncapped sum locates the peak.
3. **Sync.** Timing ±8 ms, frequency ±16 Hz around the detection, on the
   preamble tones: energy-maximising window position per window length,
   parabolic frequency peak. Accept if the preamble tones carry at least
   2.0 × the noise of the empty bins.
4. **Header.** For each hypothesis (R1 header-only, R1 data, R0
   header-only, R0 data; shortest first, each as soon as its symbols have
   arrived) and each analysis window: tone metrics from a preamble-only
   model, wrap-around Viterbi, CRC and field checks (nbits 1..max). R1's
   header symbols are on R1's grid, so they get their own energies:
   windows of 10, 12 and 14 ms, all ending where the preamble's
   whole-symbol window does, with the preamble's model scaled to the
   shorter window and a flat noise level from the median of the R1 header
   energies.
5. **Burst model.** Once a header decodes, the header (and for R0 the
   preamble, on the same grid) are known symbols (88 for an R0 data burst,
   72 for R1). The receiver refines frequency (±5 Hz) and timing on them,
   then fits, for each of three analysis windows ending at the symbol end
   (R0 20, 24 and 28 ms; R1 10, 12 and 14 ms) and five window positions
   (±12 samples):
   - a per-bin noise level from the bins at least 3 away from the known
     tones, smoothed over ±4 bins, with the tone's window SNR capped at
     30 dB;
   - an interference mixture for the empty bins: a fraction ε at r × the
     noise level (echo leftovers, sweeps, clicks), from the median and the
     first two moments;
   - a Rician model (diffuse A, specular S) for the tone bin and each
     neighbour at ±1 and ±2 bins (leakage from Doppler, window truncation
     and residual frequency error), from moments, with an
     upper-confidence variance so a few dozen faded samples do not produce
     a falsely sharp model.

   It keeps the window and position whose model gives the known header
   tones the highest mutual information. On AWGN that is the whole 28 ms
   symbol (+1.5 dB over the 20 ms window); on 6–7 ms echoes it is the 20 ms
   window placed where the echo has settled. R1 chooses the same way
   between its 14 ms symbol and the 10–12 ms windows that skip an echo.
6. **Data.** Energies of each data symbol (the grid plus two neighbours
   each side: 52 bins for R0, 36 for R1) as soon as it has arrived; every
   16 symbols a decision-directed discriminator (phase advance between the
   window's two halves at the likeliest tones; R1's 14 ms window leaves
   out 1 ms at each end, where the sweeps are) corrects the frequency
   (gain 0.3). At the end of the burst, per tone, the log-likelihood of
   every value is the sum of the five per-bin terms
   ln p(y | signal model j) − ln p(y | noise mixture).
   - **Local noise.** Band noise is not steady: a carrier keying in a few
     bins, a crash across a symbol, the radio's AGC. Each bin's noise is
     first raised to its local level: the largest of the burst's level,
     the symbol's median and the bin's mean over ±12 symbols (the decided
     tones left out), the signal model scaled down to match. A noise level
     held for the whole burst would make the LLRs of the cells hit then
     far too sure, and the decoder does not survive confident errors.
   - Bit LLRs by exact log-sum-exp over the Gray labels, clamped to ±12;
     de-interleave and de-scramble. The tone metrics go out too
     (`MfskBurst::metrics`, per symbol and tone, the best value at 0,
     floored 40 nats below), in tone-slot order for a bit-mapped burst and
     in code-symbol order for a symbol-mapped one (whose `llrs` are then
     each symbol's five bits, MSB first).
7. **Sample-clock drift.** An early-late gate on each data symbol's
   decided tone (three symbols back, so all its samples have arrived): the
   energy of the decided tone in the whole-symbol window shifted 2 ms late
   minus 2 ms early. For a tone held over the symbol the difference is
   `4(W − TD)·δ` against a mean of `(W − TD)²`, linear in the misalignment
   `δ` up to ±2 ms; the noise expected in the window is taken out of the
   mean. Every 0.9 s (32 R0 or 64 R1 symbols) this gives one drift sample;
   a least-squares line through them, shrunk towards 0 by its uncertainty
   (×4, since the samples are not independent on fading channels) and by a
   ±100 ppm prior, used from the sixth block (5.4 s) on and not believed
   beyond 300 ppm, gives the drift per symbol, and the windows of the following symbols move along
   it one sample at a time. An echo or the frequency sweep biases the gate
   by a constant, which the line's intercept absorbs. `MfskBurst::clock_ppm`
   reports the result; `MfskRx::set_drift_tracking(false)` turns it off.
8. **Decoding the frames** (`phy_mfsk::decode_short_frames`, used by the
   link for every MFSK burst).
   - **Symbol-mapped bursts** (the link's): each block of tones is a
     GF(32) codeword (`fec::nb`, [fec.md](fec.md)), decoded by belief
     propagation on the tone metrics directly.
   - **Bit-mapped bursts**: iterative demapping (BICM-ID). Round 1 is plain
     BICM: the bit LLRs into the binary LDPC decoder (`fec::ShortSoft`, 20
     layered iterations). Each later round (up to 11 more, 6 iterations
     each, check messages kept) feeds the decoder's extrinsic LLRs back as
     priors on the other four bits of each tone's label; the demapper's
     extrinsic output is the decoder's next channel input. A codeword that
     decodes is re-encoded and its bits become certain priors for the
     codewords that share its tones. Plain BICM throws away about half the
     information a tone carries at the floor's operating point; iterative
     demapping recovers about 0.5 dB of it (`diag_bicm_id`), the GF(32)
     code all of it.

`snr_db` is the fitted signal energy per symbol (all tones, all five bins)
over the noise, converted from the window's noise bandwidth to 2.4 kHz.
`start_sample` is the stream index of `modulate()`'s first sample (from the
28 ms window's timing; ±1 ms in the tests, plus the channel delay).
`freq_offset_hz` is the mean of the tracked offset over the data. `busy()`
is true from a strong preamble (≥ 3 × noise after sync) or a decoded header
until the burst is delivered.

**Channel measurement** (`crate::chan`, used by the link's channel
report): the delay spread from the share of the current tone's energy that
the previous tone leaves in the window starting 4 ms in (`ISI_FLOOR` =
0.005 in `phy_mfsk/rx/data.rs`: at most 0.002 on AWGN, CCIR good and flutter, 0.004 on CCIR poor,
0.008–0.023 on 6–7 ms echoes from +2 dB up, `tests/chan_meas.rs`), and the
Doppler spread from the phase coherence of the two halves of each tone.
R1's coherence reads 20–40 Hz of Doppler on a 7 ms, 1 Hz channel (echoes
reach both halves of every tone), so R1 reports neither, and the link
keeps what R0 or OFDM measured.

## Results

From `cargo test -p ion --release --test phy_mfsk -- --ignored
--nocapture tables` (`MFSK_RUNS` bursts per cell, `MFSK_SNRS=lo,hi,step`,
`MFSK_CELLS` from `r0,r1,h0,h1`, `MFSK_DEC` = `nb` (default), `id` or
`bicm`; two threads unless `ION_THREADS` says otherwise). Every cell runs
its bursts through the channel with real detection, sync and header
decoding (no genie), with 0.5 s of noise before and 0.6 s after each burst.
A data burst carries one rate-1/4 frame, GF(32)-coded and symbol-mapped as
the link sends it; success means the frame decodes, so a lost header
counts as a lost frame. Header-only: the burst is delivered with the right
mode, `user` and `extra`. SNR in dB (2.4 kHz), 40 bursts per cell: a cell
is good to about ±0.07; read a single 0.95 among 1.00s as noise.

### R0 data (one frame per burst)

| Channel | −16 | −15 | −14 | −13 | −12 | −11 | −10 | −9 | −8 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 0.00 | 0.00 | 0.80 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | 0.00 | 0.00 | 0.12 | 0.32 | 0.73 | 0.90 | 0.98 | 0.98 | 1.00 |
| CCIR flutter (0.5 ms, 10 Hz) | 0.00 | 0.00 | 0.03 | 0.35 | 0.90 | 1.00 | 1.00 | 1.00 | 1.00 |
| ITU low-lat disturbed (6 ms, 10 Hz) | 0.00 | 0.00 | 0.00 | 0.03 | 0.73 | 0.98 | 1.00 | 1.00 | 1.00 |
| NVIS disturbed (7 ms, 1 Hz) | 0.00 | 0.00 | 0.05 | 0.15 | 0.52 | 0.82 | 0.93 | 0.93 | 0.98 |
| ITU high-lat disturbed (7 ms, 30 Hz) | 0.00 | 0.00 | 0.00 | 0.00 | 0.03 | 0.35 | 0.88 | 1.00 | 1.00 |

### R1 data (one frame per burst)

| Channel | −13 | −12 | −11 | −10 | −9 | −8 | −7 | −6 | −5 | −4 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 0.00 | 0.03 | 1.00 | 0.98 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | 0.00 | 0.00 | 0.17 | 0.22 | 0.57 | 0.77 | 1.00 | 1.00 | 0.98 | 1.00 |
| CCIR flutter (0.5 ms, 10 Hz) | 0.00 | 0.00 | 0.00 | 0.65 | 0.90 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| ITU low-lat disturbed (6 ms, 10 Hz) | 0.00 | 0.00 | 0.00 | 0.00 | 0.10 | 0.60 | 0.95 | 1.00 | 0.98 | 1.00 |
| NVIS disturbed (7 ms, 1 Hz) | 0.00 | 0.00 | 0.00 | 0.03 | 0.15 | 0.40 | 0.57 | 0.82 | 0.90 | 0.98 |
| ITU high-lat disturbed (7 ms, 30 Hz) | 0.00 | 0.00 | 0.00 | 0.00 | 0.05 | 0.22 | 0.82 | 0.95 | 1.00 | 1.00 |

R1 on CCIR poor, re-run with 120 bursts per cell on other seeds: 0.81–0.82
at −8 dB, 0.98 at −7 dB.

### R0 header-only bursts (1.793 s)

| Channel | −18 | −17 | −16 | −15 | −14 | −13 | −12 | −11 | −10 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 0.00 | 0.00 | 0.25 | 0.73 | 0.88 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | 0.05 | 0.10 | 0.17 | 0.25 | 0.45 | 0.73 | 0.82 | 0.90 | 0.93 |
| CCIR flutter (0.5 ms, 10 Hz) | 0.00 | 0.05 | 0.10 | 0.28 | 0.55 | 0.88 | 0.98 | 0.98 | 1.00 |
| ITU low-lat disturbed (6 ms, 10 Hz) | 0.00 | 0.00 | 0.05 | 0.08 | 0.60 | 0.77 | 0.98 | 1.00 | 1.00 |
| NVIS disturbed (7 ms, 1 Hz) | 0.00 | 0.00 | 0.03 | 0.32 | 0.32 | 0.55 | 0.77 | 0.90 | 0.85 |
| ITU high-lat disturbed (7 ms, 30 Hz) | 0.00 | 0.00 | 0.00 | 0.03 | 0.08 | 0.30 | 0.60 | 0.90 | 1.00 |

### R1 header-only bursts (1.121 s)

| Channel | −15 | −14 | −13 | −12 | −11 | −10 | −9 | −8 | −7 | −6 | −5 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| AWGN | 0.03 | 0.08 | 0.45 | 0.73 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 | 1.00 |
| CCIR poor (2 ms, 1 Hz) | 0.05 | 0.17 | 0.28 | 0.22 | 0.62 | 0.70 | 0.88 | 0.93 | 0.93 | 0.98 | 1.00 |
| CCIR flutter (0.5 ms, 10 Hz) | 0.00 | 0.00 | 0.22 | 0.47 | 0.70 | 0.93 | 0.98 | 0.98 | 1.00 | 1.00 | 1.00 |
| ITU low-lat disturbed (6 ms, 10 Hz) | 0.00 | 0.00 | 0.00 | 0.03 | 0.32 | 0.75 | 0.85 | 0.93 | 1.00 | 1.00 | 1.00 |
| NVIS disturbed (7 ms, 1 Hz) | 0.00 | 0.10 | 0.15 | 0.25 | 0.35 | 0.43 | 0.45 | 0.77 | 0.85 | 1.00 | 0.95 |
| ITU high-lat disturbed (7 ms, 30 Hz) | 0.00 | 0.00 | 0.00 | 0.03 | 0.17 | 0.32 | 0.65 | 0.85 | 1.00 | 1.00 | 1.00 |

### Summary: lowest SNR with at least 90% success

| Channel | R0 data | R0 header-only | R1 data | R1 header-only | OFDM |
|---|---:|---:|---:|---:|---|
| AWGN | −13 | −13 | −11 | −11 | QPSK 1/3 from 0 dB |
| CCIR poor (2 ms, 1 Hz) | −11 | −11 | −7 | −8 | QPSK 1/3 from +6 dB |
| CCIR flutter (0.5 ms, 10 Hz) | −12 | −12 | −9 | −10 | none |
| ITU low-lat disturbed (6 ms, 10 Hz) | −11 | −12 | −7 | −8 | none |
| NVIS disturbed (7 ms, 1 Hz) | −10 | −11 (0.85 at −10) | −5 | −6 | long guard, QPSK 1/4 from +4 dB |
| ITU high-lat disturbed (7 ms, 30 Hz) | −9 | −11 | −6 | −7 | none |

(OFDM from [phy.md](phy.md).)

- R1 has twice R0's raw rate and needs 2 dB more on AWGN and 3–5 dB more
  on the fading channels; both are constant-envelope. Twice the bits alone
  asks 3 dB on AWGN (R1 gets some back from its whole-symbol window); on
  the echo channels the guard-less symbol pays.
- Header-only bursts decode at or below their mode's data floor on every
  channel, so a header-only ACK gets through wherever the data does. When
  R1 data works, the 1.121 s R1 header-only burst is the cheap ACK for R0
  data (`cheap_ack_waves`, [link.md](link.md)).
- R1's faster rates (GF(32) 1/3, 1/2 and 2/3) need what the faster codes
  need on AWGN (+0.7, +2.1 and +3.6 dB Es/N0 at 90% of codewords,
  `nb_profiles`) and about half their larger loss on Rayleigh fading; their
  gear-shift thresholds are in `crates/ion/src/level.rs`.

The GF(32) code moves every channel's 90% point 1–2 dB below the binary
short code with iterative demapping on the same channel and noise
realisations (`MFSK_DEC=id`), as on ideal channels ([fec.md](fec.md): 1.5
dB AWGN, 1.0 dB Rayleigh). Iterative demapping is itself 0.3–0.6 dB ahead
of plain BICM (`MFSK_DEC=bicm`).

### False triggers

| Test | Detector triggers | Synced | Headers | Bursts | `busy()` |
|---|---:|---:|---:|---:|---:|
| 60 s white noise + 1 s silence + 1 s DC step (`noise_only_gives_no_bursts`) | 17 | 16 | 0 | 0 | 0% |
| 300 s white noise (`diag_noise`, `MFSK_NOISE_S=300`) | 76 | 71 | 0 | 0 | 0% |
| 24 s of OFDM bursts: QPSK/8PSK/16QAM, clean/20/8/0 dB, AWGN and CCIR poor (`ofdm_bursts_give_no_mfsk_bursts`) | 6 | 5 | 0 | 0 | — |

The detector fires about 15 times a minute on noise so that it misses
almost nothing; the header CRC rejects what it lets through. OFDM fills
every bin about equally and the statistic is normalised by each column's
mean, so OFDM looks like noise to it.

### PAPR and spectrum

`papr_and_spectrum` (20 bursts of 2048 bits; envelope from a 481-tap
low-pass of the mixed-down audio, whose ringing at the sweeps adds a
little to the measured peak; mean power over the whole burst, preamble
included):

| | Envelope PAPR, peak | 99.9% CCDF | Audio crest | Mean power vs `TX_LEVEL²` | Power outside 300–2700 Hz |
|---|---:|---:|---:|---:|---:|
| R0 | 0 dB by construction (1.7 dB measured: filter ringing) | 0.4 dB | 3.0 dB | −3.0 dB | 0.22% |
| R1 | 0 dB by construction (1.2 dB measured) | 0.4 dB | 3.0 dB | −3.0 dB | 0.16% |

At the same peak, R0 and R1 send 3.5 dB more mean power than the data of
the low-PAPR QPSK OFDM levels and about 6 dB more than plain 16QAM
([phy.md](phy.md), PAPR). The SNR convention (mean power) hides that; in a
peak-limited transmitter the floor's margin over OFDM is that much larger
than the tables show. The modulator can still send several tones per
symbol (scaled per symbol so the envelope peaks at `TX_LEVEL`, with a
limiter on the analytic envelope for the sweeps); no mode uses that.

Spectrum (averaged periodogram, dB relative to the strongest 25 Hz bin;
the outer tones are at 325 and 2675 Hz for R0 and at 393 and 2607 Hz for
R1, at a 1500 Hz centre):

| | 100 Hz | 200 Hz | 250 Hz | 2750 Hz | 2800 Hz | 3000 Hz | 3500 Hz |
|---|---:|---:|---:|---:|---:|---:|---:|
| R0 | −29 | −23 | −21 | −22 | −25 | −36 | −50 |
| R1 | −31 | −22 | −27 | −28 | −24 | −32 | −50 |

The skirts come from the 1 ms frequency sweeps. They sit inside a normal
SSB transmit filter's skirts, but are not as clean as the filtered OFDM
(< −70 dB beyond its band).

### CPU (release, one core)

`cpu_cost`: 60 s of back-to-back 2048-bit bursts through CCIR poor at
+6 dB, received in 10 ms chunks.

| | RX per second of audio | Worst 10 ms chunk | TX per second of burst |
|---|---:|---:|---:|
| R0 | 19.7 ms (2.0% of a core) | 51 ms | 2.0 ms |
| R1 | 15.9 ms (1.6%) | 41 ms | 2.0 ms |

The steady part is the front end and the detector's FFT. The worst chunk
is the one where a header decodes and the burst model is fitted (3 windows
× 5 positions over up to 88 known symbols) or a burst ends; call `process`
from a worker thread, not the audio callback. Each detector trigger on
noise costs one sync and up to 12 header decodes of 5–7 ms each.

### LLR calibration

Hard-decision errors per |L| bin against the number the LLRs claim
(`llrs_are_calibrated_on_awgn`, 12 bursts each):

| Case | 0–1 | 1–2 | 2–3 | 3–4.5 | ≥ 4.5 |
|---|---:|---:|---:|---:|---:|
| R0, AWGN −10 dB: errors / claimed | 736 / 753 | 288 / 353 | 102 / 137 | 38 / 59 | 13 / 13 |
| R1, AWGN −5 dB | 126 / 139 | 68 / 72 | 27 / 41 | 16 / 28 | 8 / 11 |

On AWGN the LLRs are slightly under-confident, which is safe for belief
propagation. On NVIS at −4 dB (R0, `diag_burst`) the 2–6 bins run 1.2–2.4
× their claim, and long-echo channels at high SNR show about 0.1% errors
among bits with |L| ≥ 9 (Open points).

### Sample-clock drift

`diag_clock_drift`: 60 s R0 bursts (2142 symbols) resampled by the offset
with a 16-tap windowed sinc, 3 bursts per point; raw BER of the LLRs' hard
decisions with tracking on and off, and the offset each burst reported. A
60 s burst drifts 6 ms at 100 ppm.

| Channel | Offset ppm | BER on | BER off | Tracked ppm |
|---|---:|---:|---:|---|
| AWGN −4 dB | 0 | 0 | 0 | 0 0 0 |
| AWGN −4 dB | +50 | 1.2e-4 | 1.2e-4 | +51 +50 +49 |
| AWGN −4 dB | −100 | 0 | 3.7e-4 | −101 −100 −99 |
| AWGN −4 dB | +100 | 0 | 1.5e-3 | +100 +98 +100 |
| AWGN −4 dB | +200 | 0 | 2.4e-2 | +198 +202 +197 |
| NVIS disturbed 0 dB | 0 | 3.9e-2 | 3.9e-2 | +3 0 0 |
| NVIS disturbed 0 dB | +50 | 3.9e-2 | 4.0e-2 | +45 +49 +7 |
| NVIS disturbed 0 dB | −100 | 4.3e-2 | 4.8e-2 | −98 0 −115 |
| NVIS disturbed 0 dB | +100 | 4.1e-2 | 4.2e-2 | +26 +113 +2 |
| NVIS disturbed 0 dB | +200 | 4.5e-2 | 6.5e-2 | +121 +224 +112 |

R1 passes `sample_clock_drift_100ppm` at +4 dB on AWGN with no bit errors
at −100, +100 and +200 ppm, tracked within 0.6 ppm. The 8 ms guard already
tolerates a lot (a late drift keeps the 20 ms window inside its tone), so
untracked bursts lose little at 50–100 ppm; at 200 ppm (12 ms) they break,
tracked ones do not. The ±2 ms gate leaves room for the constant bias of
an echo; a ±1 ms gate scatters on CCIR poor. The ±100 ppm prior and the
six-block minimum matter: without them one 8 s burst at −12 dB took a
spurious 295 ppm.

## Tests (`crates/ion/tests/phy_mfsk.rs`)

| Test | Checks |
|---|---|
| `numerology` | `burst_samples` = `modulate().len()` for 0..5760 bits, peak ≤ `TX_LEVEL` and > 0.9 of it |
| `clean_round_trip_both_modes` | 7, 1024 and 5760 bits, both modes: hard decisions exact, fields, `start_sample` within 12 samples, offset < 0.5 Hz |
| `header_only_bursts` | Clean, 0 dB and −6 dB: fields, no LLRs, `start_sample` within 0.5 ms |
| `frequency_offsets_and_drift` | −50..+50 Hz (R0 at −4 dB, R1 at +2 dB): offset within 1 Hz, codeword decodes; a 5760-bit burst drifting 0.5 Hz/s from +35 Hz |
| `arbitrary_chunking_gives_identical_output` | Chunks of 1, 7, 480, 4801, random and the whole stream: identical LLRs, SNR and start (CCIR flutter, +10 Hz) |
| `back_to_back_bursts` | Five bursts (both modes, header-only and data) with gaps of 0, 50 ms and 0.3 s; clean and +12 dB; whole stream and 0.1 s chunks |
| `busy_during_a_burst_only` | Not busy before, busy mid-burst, not busy after delivery |
| `noise_only_gives_no_bursts` | 60 s of noise, silence, DC step: no bursts, busy ≤ 0.5% |
| `ofdm_bursts_give_no_mfsk_bursts` | 12 OFDM bursts, all constellations, clean to 0 dB: no MFSK bursts |
| `symbol_mapped_round_trip` | `modulate_symbols` with two GF(32) codewords: flag, symbol bits, metrics in code order, decoded clean and at −8 dB (R0) / −5 dB (R1) AWGN |
| `coded_ccir_flutter` | Short codewords on CCIR flutter: R0 at −6 dB and R1 at 0 dB, ≥ 9/10 each |
| `sample_clock_drift_100ppm` | 60 s bursts resampled by ±100 and +200 ppm (R0 −4 dB, R1 +4 dB, AWGN): low raw BER, tracked offset within 25 ppm |
| `llrs_are_calibrated_on_awgn` | Per-bin error counts within 0.6–1.6 × the claim where ≥ 30 errors are expected |
| unit tests (`phy_mfsk`, `fec::conv`) | Header code d_s = 15, tail-biting round trip (48 and 64 bits), free distances of known codes, Costas property, Gray, header fields and CRC hypotheses, 320-point FFT, ln I0 |
| `tests/nb.rs`, `tests/nb_prior.rs` | GF(32) code and layouts round trip; priors reach the checks; a known caller decodes below the blind threshold ([fec.md](fec.md)) |
| `tests/apriori.rs`, `tests/slotted.rs` | A priori and expected-slot reception of check-ins ([net.md](net.md)) |
| `tests/blanker.rs` | Clean bursts keep their bits and timing through the blanker; bursts with clicks survive with it |
| ignored | `tables`, `papr_and_spectrum`, `cpu_cost`, `diag_*` (single bursts, detection, noise, drift, `diag_ldpc_threshold`, `diag_bicm_id`) |
| `tests/phy_mfsk_narrow.rs` | Scaled layouts: burst lengths, constant envelope, round trips in the outer slots and at half band, repeated codewords; ignored: the tables of Below the floor and `diag_full_band_hash` |

## Design numbers

Measured with the real `fec::decode_short` on synthetic noncoherent M-FSK
LLRs (exact metrics, Gray labels; `diag_ldpc_threshold`, 60 codewords per
point, two seeds): Es/N0 per symbol for 90% of short codewords at rate
1/4, against the BICM limit for that rate (the numerical capacity of
noncoherent orthogonal FSK with bit LLRs) and the coded-modulation limit.

| M | Channel | Binary LDPC, BICM, 90% | BICM limit | CM limit |
|---|---|---:|---:|---:|
| 32 | AWGN | 5.8–6.0 dB | 5.0 dB | 3.0 dB |
| 32 | Rayleigh (ideal interleaving) | 6.9–7.0 dB | 5.6 dB | 3.9 dB |
| 16 | AWGN | 5.0 dB | 4.2 dB | 2.6 dB |
| 16 | Rayleigh | 6.2–6.5 dB | 4.8 dB | 3.4 dB |

The 2 dB between the BICM and coded-modulation limits for 32-FSK is what
the GF(32) code recovers (3.9 dB on AWGN, [fec.md](fec.md)). At SNR in
2.4 kHz, Es/N0 = SNR + 10·log10(2400 · T): +16.8 dB for the 20 ms window,
+18.3 dB for the whole 28 ms symbol, so R0 with the GF(32) code should
decode from about −14 dB on AWGN; it measures 0.80 at −14 dB and 1.00 at
−13 dB.

## Below the floor (study)

A study, not used by the link: weak-signal options for the worst HF below
R0, in two families. **Narrow:** the same noncoherent 32-ary MFSK in one
400 Hz slot of the six in [phy.md](phy.md) (Six slices, centres 500–2500
Hz), room for five other stations. **Full band:** something more robust
than R0 in the whole channel (longer symbols, repetition).
`crates/ion/tests/phy_mfsk_narrow.rs`; everything simulated, nothing on
the air.

### Time-scaled layouts

`MfskLayout` stretches a whole burst k times in time and divides every
tone's offset from the centre by k (`modulate_with`,
`modulate_symbols_with`, `MfskRx::with_layout`; `MfskLayout::FULL` is
k = 1). The receiver decimates by 6k instead of 6, so in its own samples
it sees exactly an R0 or R1 burst: detector, sync, burst model, metrics,
discriminator and drift gate run unchanged, and so do the header code,
the GF(32) code and the Costas preamble. What changes with k:

- **Front end.** 510k + 1 taps (the full band's filter in the receiver's
  own samples), cut-off widened with the detector range; the cost per
  second stays that of the full band's filter.
- **Detector range** ±`hmax` half-bins (25/k Hz each): 3k up to 9, so
  ±75 Hz for k ≤ 3 and ±225/k Hz beyond (±32 Hz at k = 7). Its 121-bin
  spectrum holds no more.
- **Sweep** k ms, proportional (`sweep`; a 2k ms variant was measured).
- **Reported units:** `snr_db` (the window's noise bandwidth is (8000/k)/w
  Hz), `freq_offset_hz`, the channel report's delay and Doppler are in
  real units again.

So R1/k on a channel with delay spread τ and Doppler spread σ behaves as
R1 on (τ/k, k·σ), 10·log10(k) dB lower on AWGN, at 1/k of the rate. The
full band is untouched: hashes of R0 and R1 bursts and of the receiver's
output (LLRs, metrics, SNR, offset, start, clock) on CCIR poor with an
offset are the same before and after the change (`diag_full_band_hash`),
and the rest of the `ion` tests pass. Every layout keeps the constant
envelope at `TX_LEVEL`: the same peak and the same mean power as the full
band (`scaled_layout_numbers`); no level's power changes.

### Candidates

| | R0 | R1 | R1/6 | R1/7 | R1/8 | R0/2 | R1/2 | R0 ×2 | Sixth QPSK 1/4 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Tone spacing, Hz | 50 | 71.4 | 11.9 | 10.2 | 8.9 | 25 | 35.7 | 50 | 25 (13 carriers) |
| Data symbol, ms | 28 | 14 | 84 | 98 | 112 | 56 | 28 | 28 | 50 |
| Preamble symbol, ms | 28 | 28 | 168 | 196 | 224 | 56 | 56 | 28 | — |
| Occupied, data / preamble, Hz | 2450 | 2357 / 2450 | 393 / 408 | 337 / 350 | 295 / 306 | 1225 | 1179 / 1225 | 2450 | 325 |
| Raw bit/s | 178.6 | 357.1 | 59.5 | 51.0 | 44.6 | 89.3 | 178.6 | 178.6 | — |
| Header-only burst, s | 1.79 | 1.12 | 6.73 | 7.85 | 8.97 | 3.59 | 2.24 | 1.79 | 2.21 |
| One 32-byte frame (205 tones), burst s | 7.98 | 4.22 | 25.29 | 29.50 | 33.72 | 15.96 | 8.43 | 13.72 | 15.71 ¹ |
| Net bit/s, one frame per burst | 32.1 | 60.7 | 10.1 | 8.7 | 7.6 | 16.0 | 30.4 | 18.7 | 91.6 ¹ |
| Mean power against a steady carrier at the same peak | 0 dB | 0 dB | 0 dB | 0 dB | 0 dB | 0 dB | 0 dB | 0 dB | −7.2 dB ² |

(`narrow_numbers`.) ¹ Sixth carries one K = 1440 codeword (a 180-byte
long frame), as in the Six slices study. ² The Six slices study's
waveform (link clip, no air gain). The link sends its slice levels
low-PAPR and up to 3 dB over the carriers, −2.5 to −2.7 dB (Air mean in
[link.md](link.md)); Sixth was not measured that way.

R1/k puts R1's 32 tones in a slot; R1/7 leaves 25 Hz to the slot edge on
either side (preamble 350 Hz), R1/6 overfills the slot with its preamble
(408 Hz), R1/8 leaves 47 Hz. R0/k would need k = 8 to fit (306 Hz) and
then sends 22 bit/s raw, a 64 s burst per frame: not measured. R0/2 and
R1/2 occupy half the band, centred. **R0 ×2** sends the GF(32) codeword
twice in one R0 burst (410 tones, interleaved over the burst as usual)
and adds the two copies' tone metrics before decoding; ×4 likewise.

### Thresholds

`narrow_table`: real detection, sync and header, one GF(32) short frame
(rate 1/4) per burst, symbol-mapped as the link sends it, 40 bursts per
point on a 1 dB grid, 0.5k s of noise before and 0.6k s after each burst.
Narrow layouts at 1300 Hz (the slot next to the centre), R0, R1 and the
half-band layouts at 1500 Hz, Sixth at 1300 Hz. SNR is the burst's mean
power against white noise in 2.4 kHz, as everywhere on this page; for the
MFSK layouts that is also the SNR of a steady carrier at the same drive.
IONOS MPD is `ion_dsp::ionos` (2 paths, 4 ms, 2 Hz, 3 kHz filter) with
its own noise off and the same white noise added. Each cell is the lowest
SNR with ≥ 90% right headers / ≥ 90% decoded frames. At 40 bursts a cell
is good to about ±1 dB (a point near 0.9 flips on ±0.05): R0 on CCIR poor
comes out −9 here, −11 in the 40-burst tables above (0.88 at −11 and −10
here). Read differences of 1 dB as noise.

| Wave | AWGN | CCIR moderate (1 ms, 0.5 Hz) | CCIR poor (2 ms, 1 Hz) | CCIR flutter (0.5 ms, 10 Hz) | NVIS disturbed (7 ms, 1 Hz) | ITU low-lat disturbed (6 ms, 10 Hz) | ITU high-lat disturbed (7 ms, 30 Hz) | IONOS MPD (4 ms, 2 Hz) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| R0 | −13 / −13 | −10 / −9 | −9 / −9 | −12 / −12 | −10 / −9 | −12 / −11 | −10 / −10 | −12 / −12 |
| R1 | −11 / −11 | −7 / −6 | −8 / −7 | −10 / −9 | −6 / −4 | −9 / −7 | −8 / −6 | −8 / −8 |
| Sixth QPSK 1/4 | −8 / −8 | −7 / −2 ³ | −6 / −6 | none ⁴ | −2 / +5 ³ | | | −6 / −6 |
| **R1/6** | −19 / −18 | −17 / −16 | −17 / −17 | −16 / −15 | −17 / −16 | | | −19 / −18 |
| **R1/7** | −20 / −19 | −17 / −17 | −18 / −17 | −16 / −15 | −18 / −17 | −16 / −15 | −11 / −11 | −19 / −18 |
| **R1/8** | −20 / −20 | −17 / −17 | −18 / −18 | −16 / −16 | −19 / −17 | −16 / −16 | −14 / −13 | −20 / −19 |
| **R0/2** | −17 / −16 | −13 / −13 | −14 / −14 | −15 / −14 | −14 / −14 | −15 / −14 | −12 / −12 | −16 / −16 |
| **R1/2** | −14 / −13 | −10 / −10 | −12 / −11 | −13 / −12 | −11 / −10 | −13 / −11 | −11 / −10 | −13 / −12 |
| R0, expected header | −15 / −14 | −11 / −10 | −12 / −10 | −14 / −12 | −12 / −10 | | | −14 / −12 |
| R0 ×2 | −14 / −14 | −10 / −10 | −10 / −10 | −12 / −12 | −9 / −9 | | | −12 / −12 |
| R0 ×2, expected header | −15 / −15 | −12 / −12 | −12 / −12 | −14 / −14 | −12 / −12 | | | −14 / −14 |
| R0 ×4, expected header | −15 / −15 | −12 / −12 | −12 / −12 | −14 / −14 | −12 / −12 | | | −15 / −15 |
| *Data2G c16r25, r1/3 (theirs)* ⁵ | −14.3 | | | −11.2 | −9.9 | | | −11.9 |
| *Data2G c8r50, r1/3 (theirs)* ⁵ | −11.1 | | | | | | | −8.1 |

³ Not thresholds: Sixth's loss floor on slow fading (Six slices in
[phy.md](phy.md)); NVIS frames run 0.62–0.95 from −3 to +14 dB, CCIR
moderate 0.77–0.88 from −7 to −3 dB. ⁴ Headers ≤ 0.55 and frames ≤ 0.05
up to +14 dB. ⁵ Not measured here: Data2G's end-to-end 10%-failure points
(`scripts/cpm_floor_study.py` at 12d1235), converted from mean power in
2.5 kHz by +0.18 dB; c16r25 is 16 tones at 25 Hz (400 Hz), c8r50 8 tones
at 50 Hz, both rate 1/3 with their GF(M) code where they quote it (AWGN,
MPD; flutter and NVIS are their binary-code numbers). Their MPD is the
same IONOS profile; their MPP (−11.3 c16r25, −7.6 c8r50) is IONOS's
2 ms/1 Hz, not this page's Watterson CCIR poor.

**Header-only bursts** (`r0h` … in `narrow_table`, 40 per point; the
burst delivered with the right fields):

| Wave | AWGN | CCIR poor | CCIR flutter | NVIS disturbed | IONOS MPD |
|---|---:|---:|---:|---:|---:|
| R0 (1.79 s) | −14 | −9 | −12 | −10 | −12 |
| R1 (1.12 s) | −11 | −8 | −10 | −8 | −8 |
| R1/7 (7.85 s) | −20 | −18 | −16 | −18 | −20 |
| R0/2 (3.59 s) | −17 | −14 | −15 | −14 | −16 |
| R1/2 (2.24 s) | −14 | −12 | −12 | −11 | −13 |

**What the table says.**

- **R1/k does what the scaling predicts on AWGN** (R1 −11 dB less
  10·log10(k): −18.8, −19.5, −20.0; measured −18, −19, −20) and loses
  only 1–4 dB to fading on every channel with Doppler up to 10 Hz:
  −17 ± 1 on CCIR moderate and poor, NVIS disturbed and IONOS MPD, −15 on
  flutter and low-latitude disturbed. Ladder-style (midway AWGN / CCIR
  poor) R1/7 sits at −18 dB, 6 dB under R0's −12, at a quarter of its
  speed per frame. Its header-only burst decodes at the frame's SNR.
- **Long symbols and Doppler.** The worry was 10 Hz tones against 10 Hz
  of Doppler. The receiver's five-bin model (Receiver, step 5) collects the
  leaked energy in ±2 bins; over a 98 ms window a fast-fading tone keeps a
  steady energy, so fast fading costs noise (more bins) rather than deep
  fades. On one Rayleigh path (`narrow_doppler`, frame thresholds):

  | Doppler spread (2σ) | 0.5 Hz | 1 Hz | 2 Hz | 4 Hz | 8 Hz | 16 Hz |
  |---|---:|---:|---:|---:|---:|---:|
  | R1 | −4 | −6 | −7 | −9 | −8 | −9 |
  | R1/7 | −16 | −17 | −17 | −17 | −16 | −14 |
  | R0/2 | −12 | −13 | −14 | −15 | −14 | −13 |

  Flat slow fading (0.5 Hz) is where a long burst helps most: R1's 4 s
  burst sits in one fade, R1/7's 29 s burst sees many. At 30 Hz (ITU
  high-latitude disturbed) R1/7 still decodes from −11 dB, level with R0
  (R1/8: −13; the two differ within the noise of a flat curve, 0.85–0.88
  from −14 to −12 dB).
- **Energy per bit.** Eb/N0 = SNR + 10·log10(2400 / information rate
  during the data): on AWGN R0, R1, R1/7, R0/2, R1/2 and Data2G's c16r25
  all need 3.3–4.3 dB (c16r25 taken as 4 bits per 40 ms at rate 1/3); on
  IONOS MPD R1/7 needs 4.8 dB, R0/2 4.3, R0 5.3,
  R1/2 5.3, R1 6.3, c16r25 6.7; on CCIR poor R1/7 5.8, R0/2 and R1/2 6.3,
  R0 6.3–8.3, R1 7.3. The narrow layouts are not less efficient than the
  full band; on fading they are a little more.
- **R0/2 and R1/2.** R0/2 is 3 dB under R0 on AWGN and 2–5 dB on fading,
  at half R0's speed. R1/2 carries R0's 32 bytes in 8.43 s (R0 7.98 s)
  in half the band and matches or beats R0 within the noise (CCIR poor
  −11 against R0's −9 here and −11 in the tables above).
- **Repetition is header-limited.** R0 ×2 decodes its frame wherever its
  header decodes (frame = header at every point of every curve), so the
  burst's threshold is R0's header: −14 / −10 / −9 dB (AWGN, CCIR poor,
  NVIS), no gain. With the receiver told when the burst starts and what
  its header holds (`Expect`, the net's primed reception), the
  expected-header test floors at −15 dB AWGN and −12 dB on fading, and ×2
  and ×4 both stop there: 1–2 dB over plain R0 with an expected header
  (−14 / −10), at half or a quarter of the speed. Going lower needs a
  longer preamble and header, which time stretching gives and repetition
  does not. HARQ (`link::harq`) is for long OFDM frames only and does not
  apply to the floor's short frames.

### Acquisition, drift, neighbours

**Frequency offset, drift, sample clock** (`narrow_offsets`, AWGN at −16
dB, 3 dB above R1/7's frame threshold, 20 bursts per point; header /
frame):

| | R1/7 | R1/8 |
|---|---|---|
| Offset decoded (all 20) | −35 … +35 Hz | −30 … +30 Hz |
| Offset lost (0 of 20) | ±40, ±50 Hz | ±35 Hz and beyond |
| Frequency drift ±0.1, ±0.2 Hz/s (from 0 Hz) | 1.00 / 1.00 | 1.00 / 1.00 |
| Frequency drift −0.5 / +0.5 Hz/s | 1.00 / 0.30, 1.00 / 0.10 | 1.00 / 0.25, 1.00 / 0.20 |
| Sample clock −200, −100, +100, +200 ppm | 1.00 / 1.00 each | 1.00 / 1.00 each |

The pull-in is the detector's ±9 half-bins (±32 Hz at k = 7, ±28 Hz at
k = 8) plus the sync's ±16 receiver-Hz; beyond it nothing is found.
Within it the offset costs nothing at 3 dB margin; how much it costs at
the threshold was not measured (Data2G finds 0.75 dB for a 4 Hz error on
c16r25, whose receiver does not track frequency the same way). A drift of
0.5 Hz/s moves a 29 s burst 15 Hz and the discriminator (every 16
symbols) loses it; 0.2 Hz/s (6 Hz over the burst) is fine. The clock
gate needs six 0.9k s blocks, so it starts 38 s into an R1/7 burst, after
the burst has ended: these bursts run untracked, and ±200 ppm (±6 ms over
29 s, against 98 ms symbols) does not matter.

**Spectrum** (`narrow_spectrum`, averaged periodogram of 6-frame bursts at
1300 Hz; dB per 10 Hz against the strongest 10 Hz, at offsets above the
centre, and the share of the power beyond ±200 Hz, in the next slots):

| | +175 Hz | +200 Hz | +225 Hz | +250 Hz | +300 Hz | +400 Hz | Beyond ±200 Hz |
|---|---:|---:|---:|---:|---:|---:|---:|
| R1/6 | −1 | −17 | −25 | −31 | −41 | −55 | −27 dB |
| R1/7 | −17 | −29 | −34 | −40 | −48 | −63 | −36 dB |
| R1/7, 14 ms sweep | −18 | −31 | −38 | −47 | −58 | −77 | −41 dB |
| R1/8 | −26 | −35 | −40 | −46 | −55 | −71 | −43 dB |

**Six slots side by side** (`narrow_neighbours`: a burst in every slot,
staggered so all six overlap for most of their length, a receiver in
every slot of unit power, noise against one slot; 10 runs per case):

| Case | R1/6 | R1/7 | R1/7, 14 ms sweep |
|---|---|---|---|
| One slot alone, six equal: −15 and −10 dB | — | 10/10, 60/60 | 10/10, 60/60 |
| 900 Hz slot +20 dB, on its centre and 10 Hz towards 500 Hz: −15, −10 dB | — | 50/50 each | 50/50 each |
| Six equal at −17 dB | 60/60 | 60/60 | 60/60 |
| 900 Hz slot +30 dB, −17 dB | 39/50 | 50/50 | 50/50 |
| 900 Hz slot +30 dB and 20 Hz towards 500 Hz, −17 dB | 40/50 | 50/50 | 50/50 |

No receiver delivered another slot's burst. R1/7 keeps every frame next
to a neighbour 30 dB stronger and 20 Hz closer, 2 dB above its threshold;
R1/6, whose preamble fills the slot, loses one in five there. The 14 ms
sweep cleans the skirts by 5 dB but was not needed. Not tested: a
stronger neighbour of another waveform (OFDM, full-band MFSK) or real
clicks, where the blanker (full band, before the mix-down) may zero up to
20 ms of a weak slot when a strong neighbour starts.

**False triggers** (`narrow_noise`, 600 s of white noise per receiver):

| | Detector triggers | Synced | Headers | Bursts | `busy()` |
|---|---:|---:|---:|---:|---:|
| R1 (full band) | 180 | 170 | 0 | 0 | 0% |
| R1/7 | 64 | 60 | 0 | 0 | 0% |
| R0/2 | 183 | 171 | 0 | 0 | 0% |

R1/7's detector runs every 49 ms instead of 7 ms, over 19 frequency
hypotheses instead of 7: about a third of the full band's triggers.

**CPU** (`narrow_cpu`, release, one thread; 120 s of noise with one-frame
bursts 40 s apart, 10 ms chunks; the machine was busy with another job,
so the absolute numbers are about 2.7× those of the CPU table above and
only the ratios count):

| | RX per second of audio | Worst 10 ms chunk | TX per second of burst |
|---|---:|---:|---:|
| R0 | 53.6 ms | 102 ms | 2.1 ms |
| R1 | 49.0 ms | 101 ms | 2.2 ms |
| R1/7 | 7.8 ms | 52 ms | 2.0 ms |
| R0/2 | 30.7 ms | 112 ms | 2.2 ms |

An R1/7 receiver costs a seventh of R0's (everything after the front end
runs k times less often, and the front end's taps grow as its output rate
falls); six of them, one per slot, about one R0 receiver.

### Not built, not measured

- **16 and 8 tones in a slot** (25 Hz × 16, 50 Hz × 8): not built. The
  header code emits one 32-ary tone per trellis step and the GF(32) code
  one per code symbol; fewer tones need a new header labelling and the
  binary short code with iterative demapping (1–2 dB worse, Data
  mapping). Data2G's c16r25 and c8r50 (table rows above) are the
  reference: c16r25 sends 2.6× R1/7's information rate for 5 dB more on
  AWGN and 6 dB more on MPD, about the same Eb/N0 on AWGN and 2 dB more
  on fading.
- **A purpose-built slot preamble.** R1/k keeps R0's preamble: 16 symbols
  of 28k ms (3.1 s at k = 7) on a 48-bin grid. A preamble on R1/k's own
  grid with 100 ms symbols would save about 1.5 s per burst (a fifth of a
  header-only burst).
- **Lower code rates** (a rate-1/8 GF(32) code) and **header repetition**
  (a fifth header hypothesis summing two copies before the Viterbi
  decoder): not built; repetition shows the header, not the data, sets
  the threshold, and stretching does both at once.
- Expected-header reception, LLR calibration, the channel report and
  clock tracking in scaled layouts; offsets and drift on fading channels;
  cross-detection between layouts (a full-band receiver and an R1/7
  receiver hearing each other's bursts; the CRC does not name the layout);
  Doppler beyond 16 Hz on one path; burst-to-burst behaviour in a link
  (ARQ timing, 7.85 s ACKs, connect). Nothing on the air.

### Recommendation

- **R1/7 as a slot mode and as a deep floor.** Frames from −19 dB on
  AWGN and −17 ± 1 dB on every slow- and moderate-Doppler channel, −15 dB
  on 10 Hz flutter, −11 dB at 30 Hz; ladder threshold about −18 dB, 6 dB
  under R0. It fits 350 Hz, sits beside five others and a neighbour
  30 dB stronger, pulls in ±35 Hz, costs a seventh of an R0 receiver.
  Price: 8.7 bit/s net per 32-byte frame (a 29.5 s burst) and a 7.85 s
  header-only ACK. R1/8 is the same within the noise with 47 Hz to the
  slot edge and 13% slower; R1/6 overfills the slot.
- **Not repetition** for the full band: header-limited, at most 1–2 dB
  with an expected header.
- **R0/2 as an intermediate rung** (−16 / −14 dB, 16 bit/s), and **R1/2**
  as a half-band equal of R0 (same speed, same or better thresholds),
  both worth a 120-burst rerun before anyone relies on the 1–2 dB
  differences.
- Sixth stays where the Six slices study left it: ten times R1/7's speed
  in the same slot, but 11 dB more SNR on AWGN and CCIR poor (another
  7 dB at the same peak), a loss floor on slow fading and nothing on
  flutter.

## Sources

- J. P. Costas, "A study of a class of detection waveforms having nearly
  ideal range-Doppler ambiguity properties", Proc. IEEE 72(8), 1984;
  S. W. Golomb, H. Taylor, "Constructions and properties of Costas
  arrays", Proc. IEEE 72(9), 1984 (Welch construction).
- C. C. Watterson, J. R. Juroshek, W. D. Bensema, "Experimental
  confirmation of an HF channel model", IEEE Trans. Commun. Technol.
  18(6), 1970; ITU-R F.1520 / F.1487 profiles as implemented in
  `ion_dsp::channel`.
- J. G. Proakis, *Digital Communications*: noncoherent orthogonal FSK,
  square-law detection, Rician and Rayleigh fading statistics.
- G. Caire, G. Taricco, E. Biglieri, "Bit-interleaved coded modulation",
  IEEE Trans. Inf. Theory 44(3), 1998 (BICM capacity).
- A. J. Viterbi, "Orthogonal tree codes for communication in the presence
  of white Gaussian noise", IEEE Trans. Commun. Technol. 15(2), 1967: the
  idea of convolutional codes whose branches are whole orthogonal signals.
  The generators here are found by `fec::conv::search`.
- X. Li, J. A. Ritcey, "Bit-interleaved coded modulation with iterative
  decoding", IEEE Commun. Lett. 1(6), 1997; M. C. Valenti, S. Cheng,
  "Iterative demodulation and decoding of turbo-coded M-ary noncoherent
  orthogonal modulation", IEEE JSAC 23(9), 2005 (BICM-ID).
- M. Abramowitz, I. Stegun, *Handbook of Mathematical Functions*,
  9.8.1–9.8.2 (ln I0).
- G. L. Steele, D. Lea, C. H. Flood, "Fast splittable pseudorandom number
  generators", OOPSLA 2014 (SplitMix64).
- ITU-T V.41: CRC-16-CCITT.

## Open points

- **Header-only R0 takes 1.793 s.** That is the price of ACKs that decode
  at R0's own data floor. Ways to shorten it, not done: a 12-symbol
  preamble (−0.11 s, about 1 dB worse detection), or puncturing the
  header code.
- **False headers.** On white noise the detector fires ~15 times a minute
  and almost all of those pass the sync check; each then gets 12 CRC-16
  checks (4 hypotheses × 3 windows). The expected rate of a false header
  is one in several hours of pure noise (none seen in about 8 minutes of
  noise and some 4000 table bursts), so `user` and `extra` of header-only
  bursts are untrusted input to the link.
- **R1's echo margin.** R1 has no guard; its 10–12 ms windows skip up to
  4 ms of echo, and longer echoes land in the window. A real path with
  stronger or longer late echoes than the simulator's two-path models
  could do worse; the link then falls back to R0, which keeps its 8 ms
  guard.
- **R1's rate.** 357 bit/s raw: log2(32) bits per 14 ms. Faster
  single-tone options (1 of 16 at 9.3 ms, 1 of 24 at 10.7 ms) shorten the
  window further; not tried.
- **LLR calibration** is close on AWGN (slightly under-confident) and on
  flutter; on NVIS (slow, frequency-selective fading) errors run 1.5–2.5 ×
  the claimed rate in the 2–6 |L| range, and long-echo channels at high SNR
  show a few errors at |L| ≥ 9. The clamp at ±12 keeps these from stalling
  belief propagation. A per-symbol SNR estimate or an EM refinement of the
  model on the data symbols would help.
- **Detection thresholds** (1.9 detector, 2.0 sync, 3.0 busy) are set
  from the measured noise distributions, not from a closed form.
- **Sample-clock drift on long echoes.** The early-late gate tracks to
  within a few ppm on AWGN and CCIR poor, but on disturbed NVIS (7 ms,
  1 Hz) at 0 dB its estimates scatter (+7 to +49 for 50 ppm, 0 to −115 for
  −100 ppm): the echo's share of the gate swings with every fade. It does
  not make the BER worse there (the echo, not the drift, sets it), and the
  shrinkage keeps a clean clock at 0 ± 3 ppm, but bursts of a minute or
  more on NVIS with a bad sound card would need a gate on the first path
  only.
- **Front-end cost and image.** The 511-tap front end costs ~2 ms of CPU
  per second of audio; a two-stage decimator would be cheaper. With a
  centre below ~1450 Hz the mix-down image moves into the passband again.
- **Frequency range:** acquisition searches ±75 Hz (detector) and ±16 Hz
  (sync) around the detection; tested ±50 Hz.
- **High-latitude disturbed (7 ms, 30 Hz)** works with R0 from about
  −9 dB and R1 from about −6 dB: the 20 ms window keeps ~70% of the tone in
  its bin and the leakage model collects the rest. The simulator's 30 Hz
  spread is two standard deviations; a real auroral path with more spread,
  or a three-path channel, may behave worse.
