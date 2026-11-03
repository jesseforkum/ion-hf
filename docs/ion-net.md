# Ion Net: design for the build-out

Status: design, 2026-11-03. Builds on the net mode in `docs/net.md`
(`crates/ion/src/link/net/`), which is simulated only and not in the app.

## The split

- **The modem runs the net.** Check-in windows, polling, primed receivers,
  relays and coded repair need the air timing, so they stay in
  `crates/ion`. The net costs airtime only while one is running.
- **Every operator feature is reachable from the host interface.** The
  Net section in Ion HF calls the same engine commands (`IonCmd::Net*`)
  that the `NET` host commands map to. Nothing is possible only from the
  screen.
- **Content is the application's.** The modem carries bytes: messages,
  bulletins and objects. The announcement format, check-in records, the
  net log and exports belong to the app.

## Wire changes (`crates/ion`, wave 1, agent A)

`PROTOCOL` goes from 2 to 3. The net has never been on the air, so
nothing older has to be understood. The check-in prior in `prime.rs`, which
assumes the version byte, changes with it.

1. **Traffic byte**, in the check-in (after the software version) and in
   the report (after the stations heard):
   - bits 0–2: traffic items listed (0–7, 7 meaning 7 or more);
   - bits 3–4: precedence of the most urgent: 0 Routine, 1 Welfare,
     2 Priority, 3 Emergency;
   - bit 5: short time ("in and out").
2. **Check-in line.** Free text the operator sets ("Trousdale County
   ARES"), at most 40 bytes. It does not fit the check-in, so the member
   sends it in its first turn, and again whenever it changes, as one or
   two `C_LINE` control frames (subtype 11: index, total, text). It
   relays like a report.
3. **Priority polling.** NCS polls members whose traffic byte says
   Priority or Emergency first in each round, Emergency before Priority,
   then the others in slot order. Short-time members go next after those.
4. **Objects by hash.** An object is opaque bytes, up to `MAX_MSG`, named
   by its id: the first 8 bytes of its SHA-256 and its length.
   - The host gives NCS objects to offer. NCS sends `C_OFFER` frames
     (subtype 12) listing the current offers: up to 8 objects, each with
     its index in the offer list, id and length. Offers repeat after each
     beacon and while anyone lacks one.
   - Each member keeps received objects in a store
     (`NetConfig::store: Option<PathBuf>`, files named by the id in hex)
     and reports in its report's **objects byte** which of the offer list
     it holds (bit i for offer i).
   - NCS sends an object through the bulletin stream only while some
     member it polls (or a leaf through its relay) lacks it, and sends it
     once for all of them. A member that already holds an object never
     costs airtime for it.
   - A member checks a received object against its id before storing it.
   - `NetEvent::Object { id, data, cached }` hands every offered object to
     the host, from the air or from the store.
5. **Events and roster.** `RosterEntry` gains `line`, `traffic`
   (`Traffic { items, precedence, short }`) and `holds` (offer bits).
   New events: `NetEvent::Line { call, line }`,
   `NetEvent::Traffic { call, traffic }` and `NetEvent::Object { .. }`.
   `NetConfig` gains `line`, `traffic` and `store`, and `NetStation` gains
   `set_line`, `set_traffic`, `offer(data) -> ObjectId` and `withdraw(id)`.

The report keeps to 22 bytes: with four stations heard it was 20, so the
traffic and objects bytes fill it.

## Host interface (`crates/ionhf`, wave 1, agent B)

After the `BCAST` block in `host.rs`. Commands on the command port:

| Command | Meaning |
|---|---|
| `NET OPEN name` | run the net `name` as net control |
| `NET JOIN [name]` | join `name`, or the first net heard |
| `NET CHECKINS ON` / `OFF` | net control: open or close check-ins |
| `NET LINE text` | the check-in line (the rest of the command) |
| `NET TRAFFIC n R\|W\|P\|E [SHORT]` | traffic listed: count, precedence, short time |
| `NET OFFER` | the next frame on the net port is an object to offer |
| `NET CLOSE` | net control: close the net; member: leave |

Answers: `NET PORT n` (the net's KISS port, 1–15, like `BCAST PORT n`),
then events as they happen:

```text
NET JOINED ncs net slot
NET IN call slot [VIA call]
NET LINE call text
NET TRAFFIC call n R|W|P|E [SHORT]
NET HEARD call up_db down_db
NET MISSED call
NET RELAY call VIA call
NET ROUTE call VIA call | NET ROUTE call DIRECT
NET OBJECT id [CACHED]
NET CLOSED
```

**Frames on the net's KISS port** carry a short header, both ways:
`[to length][to][from length][from][data]`, calls in ASCII.
- From the host, `from` is empty (Ion fills in its own call) and an empty
  `to` means the whole net: a bulletin when sent by net control, traffic
  for net control when sent by a member.
- To the host, both are filled in; an empty `to` was a bulletin.
- An object comes to the host as a frame with `to` = `#` followed by the
  id in hex (20 digits, `ObjectId`'s `Display`), after its `NET OBJECT`
  line.

As built (`crates/ionhf/src/host.rs`, documented in `docs/app.md`): the
other commands answer `OK` or `WRONG`; `NET OPEN` and `NET JOIN` are
`WRONG` while a link is called or up, and `CONNECT` while a net is up;
`NET CLOSED` also comes when a net is refused, left or dropped. A member
addresses only net control. Messages overheard between other stations
are not passed on. The net outlives the command client; the next one
attaches with `NET OPEN` or `NET JOIN`.

The engine side (`ion_engine.rs`) ports the private prototype's
`IonCmd::Net*`, `NetStatus` and `NetEvent` handling to ion-hf's newer net
API, with a `NetNode` running in place of the link node while a net is
up. The Net section starts as a port of the prototype's three-panel view.

## Net section (`crates/ionhf`, wave 2)

Labels follow the one-word rule.

- **Net control:** a profile kept between nets (name, frequency,
  schedule, preamble, regulars; regulars also prime the receivers).
  Open, Check-ins on or off, Close. The roster shows call, line, traffic
  with precedence, S/N both ways and the route ("via AG5B").
  Traffic is called up in order, and bulletins go to all.
- **Member:** the check-in form, filled before Join so it goes out by
  itself when the window opens:
  - the line;
  - traffic: None, Message or File;
  - precedence;
  - the addressee: net, net control or a station;
  - a Short flag.

  The announcement is rendered when it arrives, and messages are shown.
- **Announcement:** predefined fields as compact codes:
  - net name, net control and alternate net control (packed calls);
  - frequency and backup frequency (Hz);
  - start, end and next net (UTC minutes);
  - mode and purpose.

  Then a Markdown body. It is sent as an object, so stations that hold
  it already cost nothing. The renderer handles a safe subset: headings,
  emphasis, lists and tables. Links show as text, and nothing is fetched.
- **Log:** every net saved under `Config::dir().join("nets")`:
  - check-ins with their line and route, traffic and its delivery,
    bulletins, all in UTC;
  - listed in the app to recall;
  - exported as text, CSV and ICS-309 (time, from, to, subject);
  - the summary offered to all stations at close.

## Not in this build-out

- **Diffs and templates by hash:** the object store is built so these can
  follow.
- **Stream nets:** on `exp-stream`, not main.
- **A separate file transfer for members:** a member's file is a message
  across turns (the outbox already spans turns), so it is slow but works.
- **The phone layout of the Net section.**
- **Anything on the air.**
