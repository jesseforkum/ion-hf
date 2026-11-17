# Ion FEC: rate-compatible LDPC, GF(32) and header codes

| Code | Where | Used for |
|---|---|---|
| Main LDPC code, K = 1440, rates 1/4–5/6 | `crates/ion/src/fec.rs`, `fec/code.rs`, `fec/decoder.rs`, `fec/tables.rs` | OFDM data frames, HARQ |
| Short LDPC code, K = 256, rate 1/4 | the same | OFDM control frames |
| GF(32) LDPC code, K = 256 bits, rates 1/4–2/3 | `fec/nb.rs` | MFSK data and control frames |
| Tail-biting convolutional codes | `fec/conv.rs` | OFDM and MFSK burst headers |

The protograph design is in `fec/design.rs`. Tests and the generators of
every number on this page: `crates/ion/tests/fec.rs`, `nb.rs`,
`nb_prior.rs` and `header_codes.rs`; the ignored ones print the tables
(`cargo test -p ion --release --test fec <name> -- --ignored --nocapture`).
The sweeps use every core unless told otherwise.

## Summary

- One protograph, designed in this repository and lifted twice. The
  **main code** (Z = 90) carries **K = 1440** information bits (180 bytes)
  at rates 1/3, 1/2, 2/3, 3/4 and 5/6, all punctured from one rate-1/4
  mother codeword. The **short code** (Z = 16) carries **K_SHORT = 256**
  bits (32 bytes) at rate 1/4.
- No parity-check table comes from a standard. The base matrix comes from
  a seeded PEXIT-guided greedy search and the circulant shifts from a
  seeded girth-aware greedy search; both run in Rust here, and
  `tables_match_generator` checks that the shift tables are reproducible.
- Decoder: layered sum-product (`Decoder::ALGORITHM`), at most 50
  iterations, stopping as soon as every check is satisfied. Normalised
  min-sum is available as `Algorithm::MinSum`.
- Measured (BPSK on AWGN, 1% FER): every rate is within about 0.5–0.6 dB
  of the normal approximation for its length and about 1 dB of the
  Shannon limit.

## Construction

### Structure (protograph-based raptor-like, PBRL)

The base matrix has 50 rows and 66 columns:

| Columns | Role | Sent |
|---|---|---|
| 0, 1 | information, high degree | never (punctured) |
| 2..15 | information | yes |
| 16..19 | core parity | yes |
| 20..65 | extension parity, one per extension row | in order, as the rate needs |

- **Core:** rows 0..3 × columns 0..19, rate 16/18 once columns 0 and 1 are
  removed. The parity part is a dual diagonal with a weight-3 first
  column:
  - row 0: P^a·p0 + p1
  - row 1: p0 + p1 + p2
  - row 2: p2 + p3
  - row 3: P^a·p0 + p3

  with a = 1 and every other parity circulant the identity. Adding the
  four rows cancels p1..p3 and leaves p0, because P^a + I + P^a = I, so p0
  is the sum of the four information syndromes λ0..λ3; then
  p1 = λ0 + P^a·p0, p2 = λ1 + p0 + p1, p3 = λ2 + p2. This is the idea of
  the 5G NR base graphs; `encode_satisfies_all_checks` verifies the
  equations.
- **Extension rows** 4..49 each own one degree-1 parity column and connect
  only to information and core-parity columns. Each extension parity block
  is the XOR of its row, so encoding is linear time (O(edges · Z)).
- The two punctured high-degree information columns follow PBRL and 5G NR
  practice, which gives this family its low thresholds.

Method references (methods only; no code or tables were copied):

- T.-Y. Chen, K. Vakilinia, D. Divsalar, R. D. Wesel, "Protograph-based
  raptor-like LDPC codes", *IEEE Trans. Commun.* 63(5), 2015: the
  structure, and the row-by-row extension design.
- G. Liva, M. Chiani, "Protograph LDPC codes design based on EXIT
  analysis", *IEEE GLOBECOM* 2007: the PEXIT threshold.
- X.-Y. Hu, E. Eleftheriou, D. M. Arnold, "Regular and irregular
  progressive edge-growth Tanner graphs", *IEEE Trans. Inf. Theory* 51(1),
  2005: greedy cycle-avoiding edge placement, used for the shifts.
- Y. Polyanskiy, H. V. Poor, S. Verdú, "Channel coding rate in the finite
  blocklength regime", *IEEE Trans. Inf. Theory* 56(5), 2010: the normal
  approximation used for the gaps below.
- 3GPP TS 38.212 (5G NR) rate matching is the precedent for bit-granular
  prefix puncturing of a raptor-like mother code (the idea only; none of
  its tables are used).

### Design procedure (`design::design_base`, `generate_tables`)

1. **BI-AWGN J-function:** tabulated by numerical integration (Simpson,
   ±12σ), not from a curve fit.
2. **Core:** hill climbing over single entry flips of the 4 × 16
   information part, minimising the PEXIT threshold at the core rate
   16/18, from 4 seeded random starts. Sent core columns have degree at
   least 3: degree-2 core column pairs showed up as undetected errors at
   rates 2/3 to 5/6.
3. **Extension rows**, one at a time. Candidates are any subset of the
   punctured columns {0, 1} plus one to three other columns (information
   2..15 or core parity), each scored by the PEXIT threshold of the code
   with that row added; the lowest wins, ties within 0.01 dB go to the
   lighter row, then to the less-used columns. No column pair may appear in
   more than 10 rows, so the Z = 16 lifting can stay free of 4-cycles, and
   at most 6 rows may hold both punctured columns (`MAX_PUNCT_PAIR` in
   `tests/fec.rs`): PEXIT keeps choosing such rows, but in simulation each
   one beyond the sixth bought almost nothing.
4. **Shifts** (`design::lift`): edge by edge in row order, each edge takes
   the shift that closes the fewest 4-cycles, then the fewest 6-cycles,
   ties broken by a seeded shuffle; run for Z = 90 and Z = 16.
   `generate_tables` tries 256 seeds per lifting (`best_lift_seed`) and the
   best become `SEED_Z90` / `SEED_Z16`.

The seeds are in `tests/fec.rs`; they define the code on the air and
cannot change. The output is `src/fec/tables.rs`. `tables_match_generator`
re-derives both shift tables from the base matrix and the seeds, and
`liftings_have_no_4_cycles` checks girth ≥ 6.

Cycles of the chosen liftings (base-graph cycles that close in the lifted
graph, in the generator's own labels):

| Z | 4-cycles | 6-cycles | "4-cycles at Z/2" | core only (4 / 6 / Z/2) |
|---:|---:|---:|---:|---|
| 90 | 0 | 41 | 0 | 0 / 1 / 0 |
| 16 | 0 | 1774 | 47 | 0 / 160 / 13 |

PEXIT thresholds of the protograph at the rates the link uses (Es/N0 per
coded BPSK bit; `pexit_report` prints every row). Rates 5/6 and 3/4 fall
between whole extension rows.

| Extension rows | Rate | PEXIT threshold (dB) | Gap to the BI-AWGN limit (dB) |
|---:|---:|---:|---:|
| 0 (core) | 0.889 | 2.861 | 0.339 |
| 1 | 0.842 | 1.970 | 0.261 |
| 2 | 0.800 | 1.264 | 0.193 |
| 4 | 0.727 | 0.354 | 0.278 |
| 6 | 0.667 | −0.335 | 0.366 |
| 14 | 0.500 | −2.527 | 0.296 |
| 30 | 0.333 | −5.105 | 0.161 |
| 46 | 0.250 | −6.586 | 0.229 |

## Sizes, rates and the transmission order

**Position frame.** Positions index the full mother codeword returned by
`encode_mother`: N_FULL = 66 · Z bits, the two never-sent information
blocks included. Information bits are positions 0..K, so the code is
systematic.

**Transmission order** (`transmission_order`): positions 2Z, 2Z+1, …,
66Z−1, that is information columns 2..15, the core parity 16..19, then the
extension parity 20, 21, …, bit index ascending inside each block. Rate r
sends the first `r.n()` entries (`transmitted_positions`), so each rate is
a prefix of every lower one. A HARQ step sends the next bits of the same
order and decodes the combined prefix with `Decoder::decode_prefix`;
positions beyond rate 1/3 (up to N_MOTHER = 5760, rate 1/4) are
incremental redundancy only. `fec::encode_span` gives any span of the
order, wrapping round for chase repeats. The link's use of it is in
`crates/ion/src/link/harq.rs` ([link.md](link.md)).

| Rate | n | Blocks sent (of 90 bits) | Extension rows active |
|---|---:|---|---:|
| 5/6 | 1728 | 14 info + 4 core + 1.2 ext | 2 (the 2nd partly) |
| 3/4 | 1920 | … + 3.33 ext | 4 |
| 2/3 | 2160 | … + 6 ext | 6 |
| 1/2 | 2880 | … + 14 ext | 14 |
| 1/3 | 4320 | … + 30 ext | 30 |
| mother 1/4 | 5760 | … + 46 ext | 46 |

Every n is exactly K / rate and a multiple of 12, so it fills whole QPSK
(2), 8PSK (3) and 16QAM (4) symbols. The short code uses the same order
with Z = 16: n_short = 64 · 16 = 1024 bits, the whole rate-1/4 mother.

### Why K = 1440

- **Latency:** at QPSK rate 1/3 (about 900 bit/s of payload) one codeword
  is about 1.6 s of air time, the order of an HF ARQ cycle, with room for
  two codewords per burst on short cycles. K = 2048 would take 2.3 s.
- **Coding gain:** going from K ≈ 1000 to 2000 buys only about 0.2–0.3 dB
  at 1% FER (normal approximation); going much shorter costs more.
- **Arithmetic:** K = 16 · 90 is divisible by 30, so all five rates have
  integer n, and Z = 90 leaves room for a lifting without 4-cycles.

## Decoder

- Layered sum-product (`Algorithm::SumProduct`; `Algorithm::MinSum(alpha)`
  for normalised min-sum, compared by `algorithm_sweep`), at most 50
  iterations. After each iteration the hard decisions are checked against
  every active check, and decoding stops as soon as all are satisfied.
- `decode` returns `Some` only when every check is satisfied (never for an
  input with an exactly-zero posterior, such as all erasures). The checks
  are those of the code actually sent: all core checks, plus the extension
  checks whose degree-1 parity bit was sent. Unsent extension checks carry
  no information and are skipped, which also saves time at high rates.
- All buffers live in the `Decoder` and are reused; nothing is allocated
  per iteration. The free functions use a thread-local `Decoder`.
- Input LLRs are clipped to ±24 (`LLR_CLIP`) and NaN is an erasure. The
  sum-product `phi` is clamped below at 1e-12 (`PHI_MIN`), so a check can
  send up to ±28.3: more than any channel LLR, so the checks can always
  outvote a confident wrong bit. A lower cap (`phi` at 1e-6, messages up to
  ±14.5) left a one-frame slice burst with 21 raw errors in 4320, at
  |LLR| 15–30, undecodable at 10–40 dB
  (`a_slice_burst_is_heard_once_in_its_slot` in `tests/ladder.rs`).
- `set_prior` adds a priori LLRs to the information bits (net check-ins,
  [net.md](net.md)).

## Results

BPSK over AWGN, Es/N0 per coded bit, sum-product, up to 20 000 frames per
point. "Normal approx." is the Polyanskiy–Poor–Verdú estimate for that n
and k; QPSK Es/N0 is BPSK + 3.01 dB.

| Code | n | 1% FER Es/N0 (dB) | Normal approx. (dB) | Gap (dB) | Source |
|---|---:|---:|---:|---:|---|
| 5/6 | 1728 | ≈ 2.6 (n = 1710: 2.67) | 2.04 | ≈ 0.5 | `prefix_sweep`, one extension block |
| 3/4 | 1920 | ≈ 1.2 (n = 1890: 1.50; 1980: 1.08) | 0.83 | ≈ 0.5 | `prefix_sweep`, interpolated |
| 2/3 | 2160 | 0.34 | −0.26 | 0.60 | `prefix_sweep`, six extension blocks |
| 1/2 | 2880 | ≈ −1.76 | −2.39 | ≈ 0.63 | `fer_tables`, partial run |
| 1/3 | 4320 | ≈ −4.27 | −4.84 | ≈ 0.57 | `fer_tables`, partial run |

The rate-1/2 and 1/3 rows come from a partial `fer_tables` run; the short
code, `undetected_errors` and `decode_speed` have not been run on these
tables (Open points). Link frames carry a CRC-32 on top of the code.

## Soft-in soft-out decoding (`ShortSoft`)

For iterative demapping of bit-mapped MFSK bursts ([mfsk.md](mfsk.md)),
`fec::ShortSoft` keeps a short codeword's check messages between calls,
takes fresh channel LLRs at each call (`run`, a few layered iterations)
and returns the code's extrinsic LLR of every sent bit (`extrinsic`,
posterior minus channel). The decoder and its early stop are
`decode_short`'s.

## GF(32) short code

`fec::nb` codes the MFSK frames (32 bytes) over GF(32): one code symbol
per 32-ary tone, so the decoder uses each tone's likelihoods whole instead
of marginalising them into bit LLRs (BICM), which on noncoherent 32-FSK
at rate 1/4 costs about 2 dB ([mfsk.md](mfsk.md), Design numbers).

- **Field:** GF(2⁵), primitive polynomial x⁵ + x² + 1.
- **Code:** K = 52 information symbols (256 bits and 4 zero bits). At rate
  1/4, N = 205 symbols (the 205 tones a binary short codeword occupies),
  153 checks; variable degrees 2, except 20 columns of degree 6
  (`link_degrees`; 490 edges, checks of degree 3–4). Rates 1/3, 1/2 and
  2/3 (`nb::RATES`, `n_for`) use 156, 104 and 78 symbols with profiles of
  the same shape (`degrees_for`). Tanner graphs by progressive edge growth
  (Hu, Eleftheriou & Arnold 2005), coefficients pseudo-random non-zero,
  all from fixed seeds. Systematic encoding through the reduced
  row-echelon form of H.
- **Decoder:** belief propagation on probability vectors with the check
  convolution in the Walsh–Hadamard domain (Davey & MacKay 1998; Barnault
  & Declercq 2003), flooding, at most 100 iterations, early stop once every
  check holds; about 3.6 ms per rate-1/4 codeword (release, one core).
- **A priori bits** (`decode_bits_prior`, used by
  `phy_mfsk::decode_short_frames_prior` for net check-ins): the same bit
  LLRs the binary decoder's `set_prior` takes; each information symbol's
  five bit priors become a prior over its 32 values. The last information
  symbol carries one data bit and four zero padding bits, which are always
  known, so it can take only 2 of its 32 values, in blind decoding too.
- **Layout** (`nb::LINK_LAYOUT`, part of the wire format): a frame's 256
  bits are spread before they are packed five to a symbol (bit i goes to
  position 53·i mod 260), so a run of bits that a priori decoding cannot
  know (a check-in's CRC, say) leaves many symbols partly known instead of
  a few wholly unknown ([below](#a-priori-bits-and-the-layout)).

**Degree profiles** (`tests/nb.rs::nb_profiles`; ideal noncoherent
32-FSK with exact tone likelihoods; codeword success, 100–300 codewords
per point). Es/N0 per tone for 90%, rate 1/4:

| Profile | AWGN | Rayleigh (per tone) |
|---|---:|---:|
| variable degrees 2 and 3, checks 3 | 4.0 dB | 5.7 dB |
| all variables 2, checks 2–3 | 4.5 dB | 6.3 dB |
| regular (3, 4) | 4.8 dB | 6.3 dB |
| **2 + 20 columns of 6 (the link's)** | **3.9 dB** | **5.45 dB** |
| other few-heavy-column profiles (2 + 30 × 5, 15 × 8, 10 × 10, 25 × 6, 20 × 8, 40 × 4, 10 × 12 + 20 × 4) | 3.95–4.2 dB | 5.4–5.7 dB |
| finite-length normal approximation (k = 256 bits, n = 205 tones) | 3.43 dB | 4.43 dB |
| binary short code, BICM / BICM-ID ([mfsk.md](mfsk.md)) | 6.0 / 5.5 dB | 7.0 / 6.5 dB |

The GF(32) code is about 0.5 dB (AWGN) and 1.0 dB (Rayleigh) from the
bound at this length, and 1.5 dB (AWGN) / 1.0 dB (Rayleigh) ahead of the
binary code with iterative demapping. The few-heavy-column profiles are
all within each other's statistics. At the faster rates the profiles
tried all landed within about 0.3 dB of each other; the chosen ones decode
90% of codewords on AWGN at about 5.0 dB (1/3), 6.5 dB (1/2) and 8.0 dB
(2/3), against 4.5 dB at 1/4 in the same run (`NB_N`, 200 codewords per
point).

**Undetected errors** (`nb_undetected`, AWGN, the link's code, padding
bits known): of 3000 codewords at Es/N0 4.0 / 4.5 / 5.0 / 5.5 dB,
232 / 10 / 3 / 0 fail and 11 / 2 / 1 / 0 decode to a wrong codeword; at
3.5 dB 1497 of 3000 decode, 12 wrong. No error floor down to about 1e-3,
but more wrong codewords than the binary code's: the many degree-2 columns
leave low-weight codewords. The link's CRC-32 turns those into lost
frames. Coefficients chosen to break the short cycles' codewords
(Poulliat, Fossorier & Declercq 2008) would reduce them; not done.
(`nb_undetected` and `nb_profiles` place the bits in order. The spread
layout changes which codeword a frame becomes, not the code, so blind
decoding is the same.)

Through the real MFSK receiver the GF(32) code gains 1–2 dB at the 90%
point over the binary code with iterative demapping, on all six channels
and both modes ([mfsk.md](mfsk.md)).

### A priori bits and the layout

With the roster's priors (192 of a check-in's 256 bits known since
protocol 3's traffic byte, 200 when the tables below were measured;
`link::apriori`) the bit layout decides how much a priori decoding gains.
`tests/nb_prior.rs` shows why:

- The prior conversion and the check update are sound: with the
  information tones blank and sure priors, the priors alone decode, and
  certainty passes through a check into an erased parity symbol
  (`priors_reach_the_checks`).
- The code's structure is the limit. The echelon encoder makes positions
  0..55 (but three) the information symbols, so all 20 degree-6 columns
  are information and every parity column has degree 2. With the known
  symbols pinned, what is left of the graph is degree-2 symbols in long
  chains and closed cycles: belief propagation cannot even find the parity
  symbols from certain information with no channel (`nb_info_sets`), and
  greedy peeling needs 53 given symbols, one more than the code's
  dimension, so no information set avoids a cycle.
- Bits in order put a check-in's unknown fields whole onto a few degree-2
  information symbols, so the sub-code a priori decoding works in is weak.
  With only the last 8 CRC bits of one check-in open, the nearest of the
  other 255 candidates is 25 tones away with the bits in order, 41 with
  them spread, and 47 with those symbols moved onto degree-6 columns.

Layouts compared (`nb_layout_table`, ideal noncoherent 32-FSK with
check-in priors, 60 check-ins per point): Es/N0 for about 85% of
check-ins with the caller known (format alone in brackets):

| Layout | AWGN | Rayleigh |
|---|---:|---:|
| echelon, bits in order | 2.0 dB (2.7) | 3.0 dB (3.5) |
| **echelon, spread (the link's)** | **1.1 dB (1.9)** | **1.9 dB (2.9)** |
| echelon, unknown fields on the degree-6 columns | 1.5 dB (2.4) | 1.8 dB (2.9) |
| echelon, unknown bits dealt over the degree-6 columns | 1.0 dB (2.4) | 1.8 dB (2.9) |
| peeled information set, bits in order / spread / heavy / dealt | 1.8 / 1.0 / 1.3 / 1.0 dB | 2.4 / 1.8 / 1.6 / 1.6 dB |

(Read from the curves; blind decoding is the same in every layout, 90% at
3.9 dB on AWGN.) Spreading gives about 1 dB on both channels; placement on
the degree-6 columns 0.5 dB (AWGN) to 1.2 dB (Rayleigh); the peeling
information set (`InfoSet::Peeled`, which makes all but one cycle
peelable) 0.2–0.6 dB alone; all together no more than 1–1.4 dB. Also
tried and not kept: 300 iterations instead of 100 (no change); a list of
2¹⁰ re-encoded candidates around belief propagation's posteriors when it
fails (`Fallback::List`, +0.02 of check-ins); ordered-statistics decoding
of the open bits (worse than belief propagation at these SNRs).

The link uses the echelon information set with the bits spread: one fixed
permutation, not tuned to the check-in's fields, with the encoder
unchanged. `ION_NB_LAYOUT` (e.g. `peeled-heavy`) and `ION_NB_FALLBACK=list`
switch the alternatives on for comparisons; both ends must agree on the
layout.

## Header codes

Headers and tiny ACKs are 48 bits (32 + CRC-16; MFSK data headers 64)
and gate the ARQ loop, so they get the best short code that is cheap to
decode. Both PHYs use tail-biting convolutional codes with large memory,
decoded by the wrap-around Viterbi algorithm (WAVA, 3 passes; Shao, Lin &
Fossorier 2003) in `fec::conv`:

- **MFSK header:** K = 15, one 32-ary tone per bit (5 outputs, generators
  0o42101, 0o77127, 0o60225, 0o42073, 0o64667, free symbol distance 15),
  48 or 64 tones.
- **OFDM header:** K = 13, rate 1/8 (generators 0o14623, 0o10771,
  0o17567, 0o10433, 0o13351, 0o17265, 0o15237, 0o17221; d_free 64, one
  path at it), 384 coded bits sent stream-major, truncated or repeated to
  a copy's 352–408 slots.

Generators come from `conv::search`: random candidates, largest free
distance under the right weight (Hamming for bits, "tone differs" for
tones), then fewest paths at it, catastrophic codes rejected
(`search_codes` in `tests/header_codes.rs`).

**Comparison** (`mfsk_header_codes`, `ofdm_header_codes`: 300 headers per
point, success = all 48 bits right, against the finite-length normal
approximation computed numerically for the channel). Es/N0 for 90%
success:

| Header | Channel | K = 13 TB | K = 15 TB | Normal approx. |
|---|---|---:|---:|---:|
| MFSK, 32-FSK noncoherent, 48 tones | AWGN | 3.4 dB | **3.1 dB** | 3.17 dB |
| MFSK | Rayleigh per symbol | 4.45 dB | **4.4 dB** | — |
| OFDM, BPSK per bit, 352 slots | AWGN | **−8.85 dB** | −8.9 dB | −8.87 dB |
| OFDM, 352 slots | Rayleigh per bit | **−8.4 dB** | −8.4 dB | — |
| OFDM header-only, 704 slots | AWGN | **−11.6 dB** | | −12.09 dB |
| OFDM header-only, 704 slots | Rayleigh per bit | **−11.5 dB** | | — |

(Bold: the code in use. K = 11 rate 1/8, d_free 56: −8.8 dB AWGN, −8.1 dB
Rayleigh.) Decode time, release, one core: MFSK K = 15 5–7 ms (K = 13
1.2–1.7 ms), OFDM K = 13 about 2 ms (K = 15 7–10 ms).

Both codes sit on the normal approximation: the MFSK K = 15 code at 48
tones is where the bound for 48 tones is, and the OFDM K = 13 code is
within 0.05 dB of its bound. That leaves nothing for CRC-aided polar list
decoding or BP + OSD at these lengths. Header-only OFDM bursts repeat the
384-bit codeword to 704 slots and stay 0.5 dB short of their bound (a
rate-1/16 mother code would close it, at 65 536 states per step).

## Open points

- Re-run `fer_tables` (all rates and the short code), `undetected_errors`
  and `decode_speed` on the current tables. These sweeps use every core
  (`sim` spawns `available_parallelism` threads); cap the threads on a
  machine that is also running a radio.
- The GF(32) code is 0.5–1 dB from the finite-length bound; a degree
  profile optimised by density evolution, or coefficients chosen per
  check, might take a few tenths more.
