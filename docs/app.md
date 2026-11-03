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
      [--setup] [--log] [--net PAGE] [--shots DIR] [--quit S] [--gear goodput|ladder]
      [--stream on|off] [--size WxH] [--shot-set NAME --shot-at S]
      [--shot-every S]
```

- `--loopback`: no radio; simulated stations answer (below). `--loop-snr`
  sets the simulated channel's SNR (default 10 dB, the `loopback_snr_db`
  setting).
- `--tci`: use TCI at this address instead of the configured radio.
- `--connect`: call a station at start.
- `--setup`: start on Setup; `--log`: on the Log; `--net PAGE`: on that
  page of the Net (`profile`, `roster`, `traffic`, `checkin`, `inbox`,
  `log`, the newest net shown).
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

**Net log.** Each net run or joined from the Net screen is one file,
`nets/<UTC opening>-<net>.toml` (`20261103-010000Z-TNARES.toml`),
rewritten through a temporary file as the net goes, so a crash keeps it:
the net, net control, frequency, operator, open and close times (UTC),
check-ins (call, slot, line, traffic, route, time), each item of traffic
(from, to, precedence, time, subject, text, and Queued, Delivered,
Received or Handled), bulletins and missed polls. Files received go in a
folder of the same name beside it. **Export** (Net, Log) writes
`<name>.txt` (a report), `<name>.csv` (one row per check-in, item,
bulletin and missed poll: `time_utc,event,from,to,precedence,state,route,traffic,text`)
and `<name>-ics309.txt` (the ICS 309 Communications Log: incident name,
operational period, radio operator, then time, from, to and subject for
the opening, each check-in, item and bulletin, and the close; prepared by
and when) beside it. A member's copy takes net control's summary when it
arrives at the close.

## Window

| Screen | What it shows |
|---|---|
| Modem | The lamp panel and the link's counts (Bursts, Frames, Resent, HARQ increments / recoveries, Lost codewords, Polls, Ctrl bursts, Air time), the open broadcast groups and who is heard in them; the level readout with S/N, Rate, Data and the send progress; the decode plot; the band meter (how clean each third of the passband is here); the waterfall; the Log. **Disconnect** and **Abort** sit beside the state while linked. |
| Net | Pages: **Profile** (net control's profile, kept between nets: frequency, backup, start, end, next net, mode, purpose, alternate, regulars, and the Markdown body beside a live preview of the announcement), **Roster** (slot, call, line, traffic, S/N up and down, route, level, missed polls, whether it holds the announcement; **To** one member), **Traffic** (the traffic stations list, by precedence; what came in, by precedence, with **Handled**, **Pass** for traffic meant for the net, **Folder** for files), **Check-in** (the call from Setup, the line, traffic None, Message or File, precedence, **To** NCS or the net via NCS, **Short**; **Queue** before joining, **Send** once in; the outbox), **Inbox** (files received, the stations, the announcement), **Log** (past nets, **Export**, **Folder**). On the left **Open** (with **Check-ins** on or off, **Close**: the summary first, then the close, **Now** to stop waiting) or **Join** (the first net heard when the name is empty; **Leave**), with **Relay** and **Attended** announced at check-in. On the right the messages and bulletins as they come and a box to send: net control to all, or **To** one member; a member to net control. The Log below. A link and a net exclude each other; Abort (Esc) drops the net without transmitting. |
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
level (blue ▲) and the peer's (orange ▲). While the link holds in the
lifeline it reads **Hold**.

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
frames are queued, and ends a net (keying nothing) only for a client
attached to it with `NET OPEN` or `NET JOIN`. Event lines carry text
from the air (check-in lines, net names) with control characters made
spaces, so none starts a line of its own.

`DATA2G ON` (answered `OK`; off with `DATA2G OFF` or when the command
client leaves) is how a client made for Data2G's host (Ragtime) says it
knows that host's extensions. Ion offers one of them: `DISCONNECTED`
says why the link ended, in Data2G's words (`DISCONNECTED reason
[detail] [unconfirmed]`, its `docs/arq.md` §9):

| Reason | When |
|---|---|
| `CLIENT` | this end's `DISCONNECT` (or a call given up); `unconfirmed` if the goodbye went unacknowledged |
| `PEER` | the far end closed it |
| `ABORT` | `ABORT`, or the link stopped in Ion itself |
| `LINK_LOST` | nothing heard from the far end for too long |
| `LINK_FAILED weak path` | the lifeline's hold ran out: the path stayed below the floor for 15 minutes and both ends closed by agreement ([link.md](link.md), Lifeline) |
| `NO_ANSWER` | the call went unanswered |

Without `DATA2G ON` it stays plain `DISCONNECTED`. Data2G's other
`DATA2G ON` extensions (`CLEANTXBUFFER` with offsets, `TIMEOUT`,
`TXPOWER`) are answered `WRONG`, as before.

**Holding below the floor.** When the link falls below its slowest level
it holds in the lifeline ([link.md](link.md), Lifeline) and stays
`CONNECTED`: `BUFFER` keeps counting what is unacknowledged (nothing
moves until the link resumes), `PTT ON/OFF` follows the lifeline's 8 s
bursts and `BUSY` the far end's. The level readout shows **Hold**. A
client with its own timeout (Pat's, a Winlink session's) may
give up first; the lifeline holds for up to 15 minutes.

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

Ion Net ([ion-net.md](ion-net.md), [net.md](net.md)): a net runs in
place of the link, so `NET OPEN` and `NET JOIN` are `WRONG` while a link
is called or up, and `CONNECT` is `WRONG` while a net is up. Broadcast
groups hold their frames until the net is over.

| Command | Reply | |
|---|---|---|
| `NET OPEN name` | `NET PORT n` | run the net `name` as net control; `n` is its KISS port (1–15, beside the groups') |
| `NET JOIN [name]` | `NET PORT n` | join `name`, or the first net heard |
| `NET CHECKINS ON` / `OFF` | `OK` / `WRONG` | net control: open or close check-ins |
| `NET LINE text` | `OK` | the check-in line (the rest of the command, at most 40 bytes) |
| `NET TRAFFIC n R\|W\|P\|E [SHORT]` | `OK` / `WRONG` | traffic listed: count, precedence, short time |
| `NET OFFER` | `OK` / `WRONG` | net control: the next frame on the net's port is an object to offer |
| `NET CLOSE` | `OK` / `WRONG` | net control: close the net once the turn under way is over (its Close goes out three times); member: leave |

Events, as they happen:

| Line | |
|---|---|
| `NET JOINED ncs net slot` | we are in the net, as `slot` |
| `NET IN call slot [VIA call]` | a station checked in (through another, `VIA`) |
| `NET LINE call text` | net control: a member's check-in line, new or changed (no text: none) |
| `NET TRAFFIC call n R\|W\|P\|E [SHORT]` | net control: the traffic a member lists, new or changed |
| `NET HEARD call up_db down_db` | net control: how it hears a member and how the member hears it, when either moves a dB; `-` not known yet |
| `NET MISSED call` | net control: a member did not answer its poll |
| `NET RELAY call VIA call` | net control: traffic or a check-in came through another member |
| `NET ROUTE call VIA call` / `NET ROUTE call DIRECT` | net control: a station's route changed |
| `NET OBJECT id [CACHED]` | member: an object net control offers, its frame next; `id` is 20 hex digits (the first 8 bytes of its SHA-256, then its length); `CACHED`: it was held already and cost no airtime |
| `NET CLOSED` | the net closed, we left it, or it was refused (no call, a link up) |
| `NET CLOSED LOST` | member: nothing heard of net control (or our relay) for 12.5 minutes; the net is over for us |

- Frames on the net's port (KISS data, command 0) carry a header both
  ways: `[to length][to][from length][from][data]`, calls in ASCII. From
  the host `from` is empty (Ion puts in its own call) and an empty `to`
  is the whole net: a bulletin from net control, traffic for net control
  from a member. Net control may address one member; a member only net
  control. To the host both are filled in; an empty `to` was a bulletin,
  and `#` and the id an object.
  Messages overheard between other stations are not passed on.
- `NET OPEN` or `NET JOIN` while a net is up (the client's, or one
  started on the Net screen) answers that net's port.
- When the command client goes, the net keeps running; the next client
  attaches with `NET OPEN` or `NET JOIN`.
- ACKMODE frames (0x0C, a 2-byte tag, then the frame) work on the net's
  port too; the tag comes back once the net has taken the frame (it goes
  on the air over the polls that follow, so not once it has gone, as for
  a group).
- `NET LINE` and `NET TRAFFIC` hold from one net to the next: set them
  before `NET JOIN` and they go with the check-in. Net control offers up
  to 8 objects at once (the oldest goes); each goes on the air only while
  a member lacks it. Ion HF keeps received objects in `nets/objects/` in
  its settings folder, one file per id, so one offered again costs
  nothing.
- Net control sends its CW ID once the Close bursts are over, a member
  after net control's (members in slot order), and both at least every
  570 s: a member right after a burst of its own, net control after one
  nobody answers. Past 590 s either takes the next gap, so a quiet net,
  where every burst of net control's is answered, still hears it (net
  control then loses one answer). A relay keeps its ID's slot while it
  passes the Close to its leaves.

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
`crates/ion/tests/node.rs`). A link holding in the lifeline probes right
after each ID time, so the ID still goes on time
(`cw_ids_stay_on_time_through_a_hold` in `crates/ion/tests/lifeline.rs`).
An ID still owed when a link or net starts (say a member's, waiting its
turn after a Close) goes at once, before anything of the new session
keys, and an ID owed from before a call change goes out under the old
call.

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
