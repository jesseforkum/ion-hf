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

Built on `ion-net`; `docs/net.md` has the formats in full. `PROTOCOL` is
3. The net has never been on the air, so nothing older is understood. The
check-in prior in `apriori.rs` treats the new traffic byte as unknown: the
format now fixes 152 of the 256 bits (was 160), a known call 40 more.

1. **Traffic byte** (`Traffic { items, precedence, short }`, with
   `Precedence::{Routine, Welfare, Priority, Emergency}`): bits 0–2 items
   (7 meaning 7 or more), 3–4 precedence, 5 short time. It goes in the
   check-in (after the software version) and in the report. Changed from
   the design: in the report it sits at a fixed place, after the direct
   bytes held and before the stations heard, with the objects byte:
   `[3][slot][snr][queued u24][block u16][short][direct u24][traffic][objects][(slot, snr) × 0–4]`,
   14 to 22 bytes. Seeks and overheard entries carry it too, so a leaf's
   traffic reaches NCS with its admission.
2. **Check-in line**, at most 40 bytes, in one or two `C_LINE` frames
   (subtype 11): `[11][slot | part << 6 | two parts << 7][text, up to 20]`.
   The index and total share the slot's byte, so 40 bytes fit in two
   frames and a relay forwards a leaf's frames as they are. Sent in the
   first turn with room, each part until acknowledged, and again when the
   line changes.
3. **Priority polling** as designed: Emergency, then Priority, then short
   time, then the rest, each in slot order. A relay is polled as early as
   its most urgent leaf.
4. **Objects by hash.** `ObjectId { hash: [u8; 8], len: u16 }`, shown as
   20 hex digits (the hash, then the length), which is also its file name
   in the store. SHA-256 is in the crate (no new dependency).
   - `C_OFFER` (subtype 12): `[12][first index | total << 4][(hash 8, len u16) × 2]`,
     22 bytes; the index of each entry follows from the first. An empty
     entry is a free index. NCS sends the list in its own burst before the
     round's bulletin pieces: when it changes, after every beacon, and in
     a round in which a member that has every bulletin block neither holds
     nor fetches an offer.
   - The objects byte in the report: bit i, holds offer i.
   - Changed from the design: objects do not go in the bulletin's byte
     stream (where a member that holds one would still have to decode,
     and so could still need repairs for, the blocks it rides in). They use
     the bulletin's *code* in their own blocks, numbered
     `0x8000 | offer << 5 | block`. A member that has every bulletin block
     reports the first object block it lacks in the report's block and
     shortfall field, and NCS sends an object's pieces only while a member
     it polls reports one: its sources once for all, then the largest
     shortfall in repairs. A member that holds an object never reports it,
     so it never drives a piece and never lowers the level (object-only
     bursts go at the weakest fetching member's level).
   - A relay passes the list to its leaves and serves their object blocks
     from its own copy; NCS sends nothing while the relay holds it.
   - The member checks the whole object against the id before it keeps
     it; a store file that does not match its name is not trusted.
   - `NetEvent::Object { id, data, cached }`, once per object per session;
     `cached` when it was already held (store, or `offer` at a member).
5. **Events and roster**, as designed: `RosterEntry` gains `line: String`
   (empty: none), `traffic: Traffic` and `holds: u8`. `NetEvent::Line
   { call, line }`, `NetEvent::Traffic { call, traffic }` and
   `NetEvent::Object { id, data, cached }`. `NetConfig` gains `line`,
   `traffic` and `store: Option<PathBuf>`. `NetStation` gains
   `set_line(&str)`, `set_traffic(Traffic)`, `offer(&[u8]) -> ObjectId`
   (at a member: hold it as received) and `withdraw(ObjectId)`, plus
   `line()`, `traffic()` and `offers() -> [Option<ObjectId>; MAX_OFFERS]`.
   `NetNode` passes the four setters and `take_events` through.
   `NetStats` gains `obj_pieces`, `obj_repairs` and `obj_rejected`.

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
NET OBJECT id len [CACHED]
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
