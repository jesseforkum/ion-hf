**Alpha.** Ion HF has made its first contacts on the air, and its measured results still come from simulation. It is released for on-air testing; expect rough edges and changes that break compatibility between versions.

## New in this version

- **Faster on fading paths.** In simulation against alpha.4: about 2× at +3 dB on most channels (6× on a gentle fading path), 1.4–1.7× at 0 dB, and up to 7× on fast-fading (MPD) paths, where alpha.4 barely got through.
- **Level choice by throughput.** The sender picks each burst's level by the throughput it expects, not one rung at a time. `gear = "ladder"` in `ionhf.toml` (or `--gear ladder`) brings back alpha.4's way, for comparing on the air.
- **MFSK cue below +7 dB.** Weak OFDM bursts go out behind a short MFSK cue that carries their header, so the receiver finds them. It is used only when both stations are alpha.5 or later, and it is no louder than Ion's other MFSK.
- **Better receiver.** Echoes near the end of the guard interval no longer break the timing; a second decoding pass recovers more of each OFDM burst; damaged headers are matched against the ones the link expects next.
- **New window.** Backlit annunciators for TX, RX, link, CW ID, host, radio, ARQ/broadcast, HARQ, retries, losses and warnings, and for the waveform on the air; the level as a readout and a 17-step strip; a decode plot of the last bursts heard; a full log with filters, search, Copy, Save and Folder, kept in `logs/ionhf.log` in the settings folder.
- New options: `--log` (open on the Log), `--gear goodput|ladder`.

## Download

| System | File |
|---|---|
| Windows 10/11 | `IonHF-…-windows.zip`: unzip, run `ionhf.exe` |
| macOS 11+ (Apple silicon and Intel) | `IonHF-…-macos.dmg`: drag Ion HF to Applications |
| Debian 12+, Ubuntu 22.04+ | `ionhf_…_amd64.deb`: `sudo apt install ./ionhf_…_amd64.deb` |
| Other Linux (x86_64) | `IonHF-…-linux-x86_64.tar.gz`: unpack, run `ionhf` |

`SHA256SUMS.txt` lists each file's checksum.

The builds are not signed:

- **Windows:** at the SmartScreen warning choose *More info*, then *Run anyway*.
- **macOS:** the first time, right-click Ion HF and choose *Open*. Allow the microphone when asked; that is how Ion hears the radio.

## First run

1. **Setup:** your call, then the radio link (TCI, CI-V, CAT, rigctld, or a sound card with RTS/DTR/VOX keying) and the audio devices.
2. Try it without a radio first: `ionhf --loopback --connect LOOP` calls a simulated station that sends back what it receives; or point a host program at Ion with `--loopback` and connect to `LOOP`.
3. On the air, Ion sends a CW ID at the end of every link and every 9.5 minutes during one. *Esc* or *Abort* stops transmitting at any time.

Ion answers calls on its own while no link is up. All stations in a test need the same Ion HF version.

## Report problems

Open an issue with what you did, what happened, your system and radio, and `ion.log` from the settings folder (see `README.txt` in the download).

## Licence

Ion HF is now licensed under Apache-2.0 (LICENSE, with the credit in NOTICE). Releases up to 0.1.0-alpha.4 were GPL-3.0-or-later and stay so.
