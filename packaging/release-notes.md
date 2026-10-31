**Alpha.** Ion HF has made its first contacts on the air, and its measured results still come from simulation. It is released for on-air testing; expect rough edges and changes that break compatibility between versions.

## New in this version

- **Stream gear for big transfers at low S:N.** A large transfer on a weak or fading link goes as a continuous stream of coded periods, answered by short tones, instead of burst-by-burst ARQ: in simulation 1.4–1.9× faster on large transfers around 0 dB, and never more than 2% slower. Short exchanges stay on ARQ. Used only when both stations are alpha.6; `stream = false` in `ionhf.toml` (or `--stream off`) turns it off.
- **Lifeline.** When a link fades below Ion's slowest MFSK level, both ends hold it in a slow 337 Hz MFSK mode (heard from about −20 dB) instead of retrying until a timer gives up, pick up again without reconnecting when the band comes back, and after 15 minutes of hold close by agreement with a reason the host program can act on (with `DATA2G ON`, `DISCONNECTED LINK_FAILED weak path`). Same power as Ion's other MFSK. Used only when both stations are alpha.6.
- **Clipper setting (experimental).** Setup › Clipper: *Classic* (the default, as alpha.5) or *Burst*, Data2G's whole-burst clipper by Andrew Rodland, KC2G: 1–2.4 dB more average power at the same peak, about 1.25× faster in simulation from 0 dB up, eased on fast-fading paths. It switches from the next burst. Both stations should use the same setting. Peaks, MFSK and CW are unchanged.
- **Safer transmit.** If the radio or sound card stops taking audio, Ion now unkeys within about 2 seconds and says so in the log, where alpha.5 could stay keyed and silent until its 15-second watchdog.
- **CW ID** after broadcasts and after a call that was given up.
- **Host ports** wait for a port that is still busy (for example right after restarting Ion) instead of failing; Setup shows *Waiting*.
- 16QAM and 64QAM bursts decode more reliably (a fix in how the receiver allows for clipping distortion).

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

Ion answers calls on its own while no link is up. Alpha.6 works with alpha.5 stations; the stream gear and the lifeline need alpha.6 at both ends.

## Report problems

Open an issue with what you did, what happened, your system and radio, and `ion.log` from the settings folder (see `README.txt` in the download).

## Licence

Ion HF is now licensed under Apache-2.0 (LICENSE, with the credit in NOTICE). Releases up to 0.1.0-alpha.4 were GPL-3.0-or-later and stay so.
