# Ion HF

Ion HF is an HF data modem for amateur radio, written in Rust. It runs an
ARQ link that adapts burst by burst across a ladder of waveforms, from
noncoherent 32-tone MFSK that decodes below −10 dB SNR (in 2.4 kHz) up to
64QAM OFDM,
inside a 2.45 kHz channel. It is a modem: programs use it through a
VARA-style host interface, Winlink clients such as Pat and Winlink Express
for links and group chat such as Ragtime's for broadcast groups.

## Features

- 17-level ladder: MFSK (R0 and R1) at the bottom, a 650 Hz OFDM slice,
  long-guard OFDM for NVIS echoes, and full-band OFDM from QPSK to 64QAM.
- Rate-compatible LDPC (K = 1440, rates 1/3 to 5/6) with HARQ incremental
  redundancy down to its rate-1/4 mother code; a GF(32) LDPC code for the
  MFSK levels.
- Gear shifting from the peer's ACKs, with a channel report (echo spread,
  Doppler spread, effective SNR) that keeps the link off waveforms the
  channel rules out.
- Low-PAPR (DFT-spread) OFDM for the QPSK and 8PSK levels: about 2 dB more
  mean power at the same peak.
- VARA-style host interface (TCP 8300/8301 on 127.0.0.1) for Pat and
  Winlink Express, with broadcast groups on a KISS port (8100) as Data2G's
  host offers them.
- Radio control over TCI (ExpertSDR / SunSDR), Icom CI-V, Kenwood /
  Elecraft / Yaesu CAT and Hamlib `rigctld`, or a sound card with PTT by
  CAT, RTS, DTR or VOX.
- CW identification during and at the end of every link.

## Status

Alpha. Ion made its first contacts on the air in October 2026 and did
well. The results in `docs/` are still measured in simulation (Watterson
channels and a port of the IONOS SIM channel simulator). More on-air
testers are wanted: see [docs/acoustic-test.md](docs/acoustic-test.md)
for a first test without radios.

## Download and build

Windows, macOS, Debian and Linux packages are on the
[Releases](https://github.com/jesseforkum/ion-hf/releases) page, built by
`.github/workflows/release.yml` from each version tag. They are unsigned;
`packaging/README.txt` has the run instructions, where the settings live,
and the portable mode.

To build from source with stable Rust:

```text
cargo build --release -p ionhf
```

The binary is `target/release/ionhf` (`ionhf.exe` on Windows). On Linux,
install the ALSA and udev development packages first (`libasound2-dev`,
`libudev-dev` on Debian and Ubuntu). The tests run with
`cargo test --workspace --release`; the result tables in `docs/` come from
the ignored tests named there.

## Quick start

```text
ionhf --loopback
```

runs Ion with no radio: a simulated station, `LOOP`, answers calls and
echoes what you send. Set your call under Setup, then have a host program
connect to `LOOP` (or start with `--connect LOOP`).
[docs/app.md](docs/app.md) describes the program, its options and the
Winlink host interface.

## Documentation

| Document | What it covers |
|---|---|
| [docs/app.md](docs/app.md) | The Ion HF program: options, settings, screens, files, host interface, CW ID, loopback |
| [docs/link.md](docs/link.md) | The ARQ link: the level ladder, gear shifting, ACKs, the channel report, HARQ, turn-over |
| [docs/phy.md](docs/phy.md) | The OFDM PHY: numerologies, pilots, clipping and the low-PAPR mode, the receiver, measured results |
| [docs/mfsk.md](docs/mfsk.md) | The MFSK floor: waveform, receiver, measured results |
| [docs/fec.md](docs/fec.md) | The LDPC codes, the GF(32) code and the header codes |
| [docs/net.md](docs/net.md) | Net mode: check-ins, rounds, relays, bulletins, primed reception, chorus check-ins |
| [docs/ionos.md](docs/ionos.md) | Ion on the IONOS SIM channels, against the published Winlink modem results |
| [docs/mercury.md](docs/mercury.md) | Ion against the Mercury modem on the same channels |
| [docs/acoustic-test.md](docs/acoustic-test.md) | A first end-to-end test with two computers and no radios |

## How this was built

The code was written with substantial help from AI coding tools. Ion was
designed, directed and tested by the author. Every result in `docs/` comes
from the tests and tools in this repository.

## Licence

Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Anything built on
Ion's code carries the NOTICE file with it. If Ion's code or ideas help
your project, please credit "Ion HF by Jesse Forkum, N0JLF"
([CITATION.cff](CITATION.cff)). Releases up to 0.1.0-alpha.4 were
GPL-3.0-or-later and stay so.

Jesse Forkum, N0JLF
