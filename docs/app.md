# Ion HF, the program

`ionhf` (`crates/ionhf`) is the Ion modem as a desktop program. Programs
use it through a VARA-style host interface: Winlink clients (Pat, Winlink
Express) for links, and group chat such as Ragtime's for broadcast groups.
Its window shows the modem; chat, files and nets belong to the programs on
top. It drives the radio over TCI
(ExpertSDR / SunSDR), Icom CI-V, Kenwood / Elecraft / Yaesu CAT or Hamlib
`rigctld`, or uses a sound card alone with PTT by CAT, RTS, DTR or VOX.

```text
ionhf [--loopback] [--loop-snr DB] [--tci ws://host:port] [--connect CALL]
      [--setup] [--log] [--shots DIR] [--quit S] [--gear goodput|ladder]
      [--stream on|off] [--size WxH] [--shot-set NAME --shot-at S]
      [--shot-every S]
```

- `--loopback`: no radio; simulated stations answer (below). `--loop-snr`
  sets the simulated channel's SNR (default 10 dB, the `loopback_snr_db`
  setting).
- `--tci`: use TCI at this address instead of the configured radio.
- `--connect`: call a station at start.
- `--setup`: start on Setup; `--log`: on the Log.
- `--shots DIR`, `--quit S`: save the window to `DIR` every 15 s and close
  after `S` seconds (screenshots, scripted checks).
- `--gear`: the link's choice of level for this run, over the `gear`
  setting (below).
- `--stream on|off`: the stream gear for this run, over the `stream`
  setting (below).
- `--shot-every S` (with `--shots`): every `S` seconds instead of 15.
- `--size WxH`: the window at so many pixels, one point to a pixel
  whatever the display's scaling (the saved window size is left alone).
- `--shot-set NAME --shot-at S` (with `--shots`): instead of the 15 s
  shots, one set `S` seconds in, at 1280x720 and 1920x1080, saved as
  `NAME_1280.png` and `NAME_1920.png`.
  `lab_runs/ui/capture.sh` takes the whole set in loopback, driven through
  the host ports by `lab_runs/ui/drive.py`.

## Settings

`ionhf.toml` holds the settings, `window.ron` the window, `logs/` the
operator's log, and `ion.log` the program's diagnostics (written by a
release build started without a terminal):

| System | Folder |
|---|---|
| Windows | `%APPDATA%\Ion HF` |
| macOS | `~/Library/Application Support/Ion HF` |
| Linux | `$XDG_CONFIG_HOME/ionhf`, else `~/.config/ionhf` |

**Portable:** with an `ionhf.toml` (even an empty one) next to the program,
Ion keeps everything in that folder instead.

`gear` (no screen of its own) is how the sending side picks each data
burst's level: `"goodput"` (the default) by expected throughput,
`"ladder"` up and down the ladder as Ion HF 0.1.0-alpha.4 did, for an A/B
on the air ([link.md](link.md), Gear shifting). Each end chooses for the
bursts it sends; it takes effect from the next link.

`clipper` (Setup, **Clipper**) is how OFDM bursts are clipped:
`"classic"` (the default), the per-symbol clip and low-PAPR mode of
0.1.0-alpha.5, or `"burst"`, Data2G's whole-burst clipper, which sends
1–2.4 dB more mean power at the same peak and eases off on fast fading. It
takes effect from the next burst. Both stations should use the same one:
the receiver tells the burst clipper's QPSK and 8PSK apart but not its
16QAM and 64QAM.

`stream` (`true` by default; no screen of its own) offers the stream gear
([link.md](link.md), Stream gear): big transfers at low S:N go as a
stream of coded periods answered by tones. `false`: the station does not
offer it and every link is plain ARQ, as with Ion HF 0.1.0-alpha.5 and
before (which never offer it). It takes effect from the next link.

**Log file.** Everything the Log shows goes to `logs/ionhf.log` as it
happens, one line each (`2026-10-29 19:38:53Z RX   CONACK +15 dB`), kept
across runs. Past 2 MB it rolls over to `ionhf.1.log` (then `.2`, `.3`;
the oldest goes). **Save** writes the lines the Log shows (its filter and
search) to `logs/saved-<UTC time>.txt`; **Folder** opens `logs/`.

## Window

| Screen | What it shows |
|---|---|
| Modem | The lamp panel and the link's counts (Bursts, Frames, Resent, HARQ increments / recoveries, Lost codewords, Polls, Ctrl bursts, Air time), the open broadcast groups and who is heard in them; the level readout with S/N, Rate, Data and the send progress; the decode plot; the band meter (how clean each third of the passband is here); the waterfall; the Log. **Disconnect** and **Abort** sit beside the state while linked. |
| Log | The Log in place of the passband, the panel still above. |
| Setup | Station (call, grid), Radio (link type, port, model, sound cards with **Rescan**; applies on restart), Levels (drive and the OFDM level), Keying (PTT, **TX audio** and lead time), Host (the VARA-style ports). |

### Lamps

Annunciators: a lens with its legend in it, lit from behind in the
colour of what it means, the legend then dark on the lit lens. An unlit
lens stays dark with its legend dim but readable, so colour is never the
only cue.
Only WARN does anything when clicked (the Log, errors only).

| Lamp | Lit |
|---|---|
| TX | keyed (red) |
| RX | a burst is being received (green) |
| LINK | green connected; amber calling or closing; red failed |
| CW ID | our CW ID is on the air (amber), at a link's end or mid-link |
| HOST | blue: a host program is on the command port; faint: the port is open, no program |
| RIG | green: the radio link is up; amber: a radio setting will stop Ion (mode, filter) |
| ARQ | a link is up or being set up (green) |
| STREAM | the link is in its stream gear (green) |
| BCAST | a host program has broadcast groups open (violet); faint while a link holds the channel |
| HARQ | for 3 s after we send a HARQ increment or a codeword decodes thanks to one (amber) |
| RETRY | for 3 s after frames are sent again (amber) |
| LOST | for 3 s after a codeword is lost after combining (red) |
| WARN | for 60 s after a warning or error (amber) |
| MFSK R0, MFSK R1, SLICE, LONG, OFDM | the waveform of the level on the air: blue ours, orange the peer's |
| CUE | for 3 s after an MFSK cue is sent or heard (amber) |
| DONE, SLOWER, FASTER | for 2 s after that stream answer is sent (blue) or heard (orange) |

### Level

The level on the air (ours while we send, the peer's while it does; our
last broadcast burst's when groups are open and no link is up), with its
modulation and code rate, its waveform and the payload rate of a full
burst of it. Under it the ladder L0–L16: the levels up to the current one
lit, the ceiling the peer's reports allow marked above (▼), and below our
level (blue ▲) and the peer's (orange ▲).

While the stream gear runs, a line under S/N, Rate and Data gives
**Stream** (its option, blue ours, orange the peer's), **Period** (periods
so far in this block) and, receiving, **Have** (pieces held of those the
block needs).

### Decode plot

The last three data bursts heard (link or broadcast), older ones fainter. OFDM: the
equalised data symbols (at most 1000 a burst, despread on the low-PAPR
levels) over the constellation's points. MFSK: per symbol, the share of
its likeliest tone value (up) against 1 less the runner-up's (across),
from the tone metrics, as Data2G plots its tones; clean symbols sit top
right. Under it, the burst, its S/N and **OK** or **LOST n** codewords.
Points past the frame (low S:N) are cut off at its edge.
The Ion worker makes plots only while the modem screen shows
(`ion::link::node::Node::set_plot`).

### Log

Every note, burst, host event and error, newest at the bottom: **All**,
**Link**, **Bursts**, **Host** or **Errors**, and a search. It follows the
newest line until scrolled back; **LIVE** goes back to it. **Copy** puts
the lines shown on the clipboard. Esc in the search box leaves the box;
elsewhere it aborts the link as before. Drag the panel's
top edge to resize it, or **Log** in the header gives it the window.

## Winlink host interface

`crates/ionhf/src/host.rs` follows "VARA Protocol Native TNC Commands"
(2022): a command port (default 8300) and a data port one above, on
127.0.0.1 only, one client at a time. `VERSION` is answered
`VERSION IonHF <version>`. Settings that mean nothing to Ion (`BW…`,
`COMPRESSION …`, `CWID …`) are answered `OK`. `BUFFER n` counts bytes the
far end has not yet acknowledged (Pat waits for 0 before closing).
`DISCONNECT` closes the link once what is queued has been acknowledged
(a client's farewell still goes), giving up on the rest after 30 s;
`ABORT` closes at once.
`PTT ON/OFF` is for information only: Ion keys the radio itself. `SN`
gives the S/N of the last burst heard. `ABORT` also drops what broadcast
frames are queued.

Broadcast groups ([broadcast.md](broadcast.md)), as Data2G's host offers
them, on the KISS port 200 below the command port (8100):

| Command | Reply | |
|---|---|---|
| `BCAST OPEN group [FROM call]` | `BCAST PORT n` | a KISS port (1–15) for the group; the sender's call always goes |
| `BCAST CLOSE n` | `OK` / `WRONG` | |
| `BCAST MODE n ...` | `OK` / `WRONG` | `OK` for an open port: Ion picks the level |
| `MODES` | `MODE ION 2400 255 16 3.0 30.0`, `OK` | one mode, for a client sizing frames and timing repairs |

- Port 0 is the group "KISS 0" while a KISS client is connected.
- Frames: KISS data (command 0) on the group's port, or ACKMODE (0x0C,
  a 2-byte tag, then the frame); the tag comes back once the frame has
  gone out. KISS persistence (command 2) sets P for every group.
- Once the command client has sent a `BCAST` command it gets `BCAST n
  HEARD [call] snr` (a burst of the group, its call when its header
  decoded), `BCAST n LOST k` and `BCAST n DROPPED k`.
- When the command client goes, its groups close.

## On the air

A host program's `CONNECT` and broadcast frames key the radio, and so
does answering: with a call set, Ion answers a station that calls it while
no link is up (`answer = false` in `ionhf.toml` receives only). Disconnect
closes a link once what is queued has gone; Abort (Esc) stops it without
transmitting.

**Identification.** Ion sends a CW ID at least every 570 s during a link
and once at its end (§97.119). The station that did not close the link
waits for the other's CW ID plus 1.5 s before it transmits. Both ends time
the mid-link IDs from the same instant, so each waits out the other's ID
instead of polling into it (`cw_id_breaks_do_not_cost_polls` in
`crates/ion/tests/node.rs`).

**Radio notes.**

- TCI: Ion sets DIGU on connect. The RX filter is left to ExpertSDR3 and
  must pass 0–3000 Hz; a narrower one is reported as a problem.
- **TX audio** picks the input a CAT key takes on radios with two: Data
  (the default) keys a Kenwood with `TX1;` (DATA SEND, audio from USB or
  ACC2) and rigctld with `T 3` (Hamlib's data PTT); Mic keys `TX;` and
  `T 1`. A radio that refuses the data command gets the plain one. `TX;`
  keys Kenwood and Elecraft but is a read on Yaesu.
  Yaesu wants 2 stop bits on its older menus.
- The 0.2 s PTT-to-RF lead (`DEFAULT_LEAD_S`) has not been measured over
  TCI; sound-card radios set the lead under Keying.

## Loopback

`ionhf --loopback` runs Ion against a simulated station on a simulated
channel, with no radio:
the station `LOOP` answers calls and sends every line back ("LOOP
copies: ...").

## Simulated band

`ionhf --sim` runs headless Ion stations, no window and no radio, for
testing host programs such as Ragtime or Pat against Ion. Each station has
its own host ports: station 0 on 8300 (KISS 8100), station 1 on 8310 (KISS
8110), and so on. What one transmits reaches each of the others through
its own IONOS channel (`ion_dsp::ionos`), in real time.

```
ionhf --sim [--stations 2] [--ionos WGN|MPG|MPM|MPP|MPD] [--paths 2]
      [--snr 10[,DB...]] [--port 8300] [--calls N0AAA,N0BBB] [--seed 1]
      [--gear goodput|ladder]
```

`--snr` is what each station hears, in station order (the last value
repeats), against a sine at full transmit level in 3 kHz, as in the IONOS
charts; `--snr 15,0` makes a lopsided link. The log goes where Ion's log
goes, so run it from a folder with its own `ionhf.toml` (portable).

## Not in this version

- Calling CQ or beaconing.
- Setting the radio's frequency: the operator tunes the radio.
