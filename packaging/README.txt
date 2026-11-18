Ion HF (alpha)
==============

An HF data modem for amateur radio, behind a VARA-style port for Winlink
clients (Pat, Winlink Express) and group chat (Ragtime).

Run
---
Windows: ionhf.exe. Unsigned for now: at the SmartScreen warning choose
"More info", then "Run anyway".

macOS: open "Ion HF.dmg" and drag Ion HF to Applications. Unsigned for
now: the first time, right-click Ion HF and choose Open (or, in
Terminal: xattr -d com.apple.quarantine "/Applications/Ion HF.app").
Allow the microphone when asked: that is how Ion hears your radio.

Linux: install the .deb (sudo apt install ./ionhf_*.deb; Ion HF appears
in the applications menu), or run ionhf from the tarball.

Try it with no radio: ionhf --loopback (a simulated station, LOOP,
answers calls).

Settings
--------
ionhf.toml, with window.ron (the window), ion.log (diagnostics) and
logs/ (what the Log shows):
  Windows  %APPDATA%\Ion HF
  macOS    ~/Library/Application Support/Ion HF
  Linux    ~/.config/ionhf
Portable: put an ionhf.toml (even an empty one) next to the program and
Ion keeps everything in that folder instead.

Received files and pictures go to Documents/Ion.

On the air
----------
Ion sends a CW ID at the end of every link and every 9.5 minutes during
one (Setup, CW ID). With that off Ion does not identify your station:
that is then up to you. Esc or Abort stops it transmitting at any time.

Apache-2.0, see LICENSE and NOTICE. N0JLF.
