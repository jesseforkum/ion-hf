# Ion HF, the program

`ionhf` (`crates/ionhf`) is the Ion modem as a desktop program: Winlink
through a VARA-style host interface (Pat, Winlink Express), keyboard links
with chat, files and pictures, and nets. It drives the radio over TCI
(ExpertSDR / SunSDR), Icom CI-V, Kenwood / Elecraft / Yaesu CAT or Hamlib
`rigctld`, or uses a sound card alone with PTT by CAT, RTS, DTR or VOX.

```text
ionhf [--loopback] [--loop-snr DB] [--tci ws://host:port] [--connect CALL]
      [--send TEXT] [--file PATH] [--net NAME] [--tab link|listen|net|setup]
      [--view chat|charts|log] [--mini] [--shots DIR] [--quit S]
```

- `--loopback`: no radio; simulated stations answer (below). `--loop-snr`
  sets the simulated channel's SNR (default 10 dB, the `loopback_snr_db`
  setting).
- `--tci`: use TCI at this address instead of the configured radio.
- `--connect`, `--send`, `--file`: call a station at start, and send the
  text or file once linked. `--net NAME`: run that net as NCS at start.
- `--tab`, `--view`: the tab shown at start, and what the Link tab's centre
  shows. `--mini`: start in the small window.
- `--shots DIR`, `--quit S`: save the window to `DIR` every 15 s and close
  after `S` seconds (screenshots, scripted checks).

## Settings

`ionhf.toml` holds the settings, `window.ron` the window, and `ion.log`
the log (written by a release build started without a terminal):

| System | Folder |
|---|---|
| Windows | `%APPDATA%\Ion HF` |
| macOS | `~/Library/Application Support/Ion HF` |
| Linux | `$XDG_CONFIG_HOME/ionhf`, else `~/.config/ionhf` |

**Portable:** with an `ionhf.toml` (even an empty one) next to the program,
Ion keeps everything in that folder instead.

## Screens

| Tab | What it shows |
|---|---|
| Link | Each side's level and SNR report (Rpt Sent, Rpt Rcvd), Aim (the level the reports allow), Queue, Rate, Data and Time; the band meter (how clean each third of the passband is here). The centre is **Chat** (each line under its sender's call; files and pictures in the conversation with progress, a picture filling in as it arrives; drop a file on the window or pick one with **Browse** to send it), **Charts** (level, SNR and data over the last minutes) or **Log** (every burst sent and heard). |
| Listen | Call and Connect; Recent; Answer (Auto: answer calls whenever no link is up; Off: receive only); Heard (stations heard calling); Band and the bursts heard lately; the log. |
| Net | Start as NCS or Join; Open, Close net, Leave; Caps (Relay, Wide, QSY, Attended); the roster and the traffic. |
| Setup | Station (call, grid), Radio (link type, port, model, sound cards with **Rescan**; applies on restart), Levels (drive and the OFDM level), Keying (PTT, **TX audio** and lead time), Host (the VARA-style ports). |
| Mini | A small window for running behind a Winlink client: state, SNR, level, rate, progress, Abort. |

## Files and pictures

Text and files share the link's byte stream (`crates/ionhf/src/ion_files.rs`).
A file is a header line and its bytes:

```text
0x1E "HF1 " kind " " size " " name "\n" <size bytes>
```

0x1E does not occur in typed text, so the receiving side sorts files back
out of the stream. A picture (jpg, png, bmp, gif, webp) is scaled to fit
320 × 240 and sent as eight horizontal JPEG strips, so the other side sees
it fill in; any other file goes as it is, up to 16 MB. Received files are
saved to `Documents/Ion` (a name already there gets `-2`, `-3`, ...). A
Winlink client on the host ports has the stream to itself and never sees
this framing.

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

`CLEANTXBUFFER` is VARA's: `CLEANTXBUFFER OK`, `CLEANTXBUFFER
BUFFEREMPTY` with nothing unacknowledged, `CLEANTXBUFFER FAILED ` (with
VARA's trailing space) with no link. What has never been on the air is
dropped at once. What is on the air but not yet acknowledged (the limbo)
still goes until the far end has it: Ion's receiver delivers one
unbroken stream, and nothing on the air can tell it to skip a gap. `BUFFER`
leaves the limbo out at once, as VARA has it. Commands sent after it are
answered after it.

`DATA2G ON` (answered `OK`; off with `DATA2G OFF` or when the command
client leaves) is how a client made for Data2G's host (Ragtime) says it
knows that host's extensions (Data2G's `docs/arq.md` §9). Without it
`TIMEOUT`, `TXPOWER` and `CLEANTXBUFFER n` are answered `WRONG`, and
everything else is as above. With it:

| Command | Reply | Data2G | |
|---|---|---|---|
| `CLEANTXBUFFER` | `CLEANTXBUFFER OK drop_from drop_to limbo_from limbo_to` | PR #118 | host offsets (bytes written since `CONNECTED`), ranges `[from, to)`; an empty limbo is `drop_from drop_from`; a second clean before the limbo settles repeats it; `BUFFEREMPTY` and `FAILED ` as VARA's |
| `CLEANTXBUFFER n` | as above, or `CLEANTXBUFFER LATE` | PR #118 | drops only bytes before offset `n` (`drop_to` is `n`, or what has been written if less); `LATE`, nothing dropped, if a byte at or past `n` is on the air |
| | `CLEANTXBUFFER SETTLED from split to` | PR #118 | once the limbo is acknowledged: delivered before `split` (Ion: always `to`) |
| | `CLEANTXBUFFER SETTLED from split to UNKNOWN` | PR #118 | the link ended first (before its `DISCONNECTED`): acknowledged before `split`, the rest on the air unacknowledged |
| `TIMEOUT` | `TIMEOUT s` | PR #122 | the link idle close, seconds; `1000000000.0` (Ion's default) is none |
| `TIMEOUT s` | `OK` / `WRONG` | PR #122 | any finite `s` > 0, for the link under way and later ones; `1000000000` or more: none again |
| `TXPOWER` | `TXPOWER dB` | PR #122 | how far below the operator's drive Ion transmits (0.0 at first) |
| `TXPOWER dB` | `OK` / `WRONG` | PR #122 | −60 to 0; anything above 0 is `WRONG`: a host can only turn Ion down |
| | `DISCONNECTED reason [detail] [unconfirmed]` | PR #122 | why the link ended (below) |

- `BUFFER` counts the limbo until it settles.
- Data written to the data port before `CONNECTED` is dropped (VARA
  queues it), so offset 0 is the first byte written after `CONNECTED`.
- `TIMEOUT` and `TXPOWER` go back to Ion's own (no idle close, 0 dB)
  when the command client leaves.
- **Idle close.** A link with nothing queued or in flight either way and
  no data heard for `TIMEOUT` seconds says goodbye: `DISCONNECTED IDLE`
  here, `PEER` at the far end. Data stuck on a bad path is not idle: that
  is the link's own business (`LINK_LOST`, the lifeline). In a lifeline
  hold the goodbye goes in the lifeline; a long `TIMEOUT` never keeps a
  hold up longer than its own 15 minutes (`LINK_FAILED weak path`) or
  its silence rule (`LINK_LOST`). Without a `TIMEOUT` a quiet link stays
  up: its idle check-ins (every 10 s, on the air) are answered.
- **TXPOWER** scales everything Ion transmits (bursts, broadcast and the
  CW ID alike, so their levels against each other stay as they are), from
  the next burst, the way lowering the drive would. The Ion worker clamps
  whatever it is given to −60..0 dB again, so no value can make it louder
  than the operator set.

Where Ion differs from Data2G, and why:

- The limbo never drops: Data2G's receiver can be told to skip bytes
  (its abandon); Ion's cannot without a change on the air. So `SETTLED`
  always says delivered (`split` = `to`) unless the link ended first.
  Data2G also holds the next burst 0.25 s after `SETTLED` for a reply;
  Ion does not. A client that waits for `SETTLED` before writing again
  (Ragtime does) waits for the limbo's acknowledgement: one ACK on a
  good path, more on a lossy one.
- `TIMEOUT`'s default: Data2G's is 300 s; Ion never closed a quiet link,
  and VARA clients count on that, so its default is none, reported as
  `1000000000.0` (a client reads a number; Ragtime writes the value it
  read to go back to the host's own).
- `TXPOWER`: Data2G takes −60 to 20 dB of output volume; Ion takes −60 to
  0 dB below the drive (Ion HF's owner: never louder than configured).
  Ragtime's Auto ALC, which may ask for up to 6 dB over its base, is
  answered `WRONG` and reads back the level unchanged.
- `ABORT` with a limbo unsettled: `OK` (Ion's order, as before), then
  `CLEANTXBUFFER SETTLED ... UNKNOWN`, then `DISCONNECTED ABORT`.
- A link that compresses (not used by Ion HF) answers `CLEANTXBUFFER
  FAILED `: its records are coded against each other.

`DISCONNECTED` reasons:

| Reason | When |
|---|---|
| `CLIENT` | this end's `DISCONNECT` (or a call given up); `unconfirmed` if the goodbye went unacknowledged |
| `IDLE` | the idle close (`TIMEOUT`); `unconfirmed` as `CLIENT` |
| `PEER` | the far end closed it |
| `ABORT` | `ABORT`, or the link stopped in Ion itself |
| `LINK_LOST` | nothing heard from the far end for too long |
| `LINK_FAILED weak path` | the lifeline's hold ran out: the path stayed below the floor for 15 minutes and both ends closed by agreement ([link.md](link.md), Lifeline) |
| `NO_ANSWER` | the call went unanswered |

Without `DATA2G ON` it stays plain `DISCONNECTED`.

**Holding below the floor.** When the link falls below its slowest level
it holds in the lifeline ([link.md](link.md), Lifeline) and stays
`CONNECTED`: `BUFFER` keeps counting what is unacknowledged (nothing
moves until the link resumes), `PTT ON/OFF` follows the lifeline's 8 s
bursts and `BUSY` the far end's. The Link screen shows **Hold** for the
level. A client with its own timeout (Pat's, a Winlink session's) may
give up first; the lifeline holds for up to 15 minutes, whatever
`TIMEOUT` says.

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

Connect, Answer (when called), and net Start, Join and Close all key the
radio. Answer is on by default: with a call set, Ion answers a station
that calls it. Turn Answer off to receive only. Abort (Esc) stops a link
without transmitting.

**Identification.** Ion sends a CW ID at least every 570 s during a link
and once at its end (§97.119). The station that did not close the link
waits for the other's CW ID plus 1.5 s before it transmits. Both ends time
the mid-link IDs from the same instant, so each waits out the other's ID
instead of polling into it (`cw_id_breaks_do_not_cost_polls` in
`crates/ion/tests/node.rs`). A link holding in the lifeline probes right
after each ID time, so the ID still goes on time
(`cw_ids_stay_on_time_through_a_hold` in `crates/ion/tests/lifeline.rs`).

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

`ionhf --loopback` runs Ion against simulated stations on a simulated
channel, with no radio:

- **Link:** the station `LOOP` answers calls, sends every line back
  ("LOOP copies: ...") and acknowledges every file ("LOOP got ...").
- **Net, Start:** W5AAA, W5BBB and W5HID join the net you run. Only W5BBB
  hears W5HID, so W5HID's traffic arrives relayed.
- **Net, Join:** `LOOPNC` runs net `LOOPNET` with two members and a
  bulletin.

## Simulated band

`ionhf --sim` runs headless Ion stations, no window and no radio, for
testing host programs such as Ragtime or Pat against Ion. Each station has
its own host ports: station 0 on 8300 (KISS 8100), station 1 on 8310 (KISS
8110), and so on. What one transmits reaches each of the others through
its own IONOS channel (`ion_dsp::ionos`), in real time.

```
ionhf --sim [--stations 2] [--ionos WGN|MPG|MPM|MPP|MPD] [--paths 2]
      [--snr 10[,DB...]] [--port 8300] [--calls N0AAA,N0BBB] [--seed 1]
```

`--snr` is what each station hears, in station order (the last value
repeats), against a sine at full transmit level in 3 kHz, as in the IONOS
charts; `--snr 15,0` makes a lopsided link. The log goes where Ion's log
goes, so run it from a folder with its own `ionhf.toml` (portable).

## Not in this version

- Calling CQ or beaconing outside a net.
- Setting the radio's frequency: the operator tunes the radio.
