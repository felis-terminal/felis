---
title: Protocol admission decisions
sidebar:
  order: 1
---

The [support matrix](../../reference/protocols/support-matrix.md) records what every escape-sequence family's status is.
This page records why the families that took a real decision took it, and what would reverse it. The inventory of
families is XTerm's `ctlseqs` document plus the vendor specs (Kitty, iTerm2, ConEmu, mintty, Ghostty).

## Admitting a tolerated input

Tolerate is the stance that costs nobody anything at the moment it is taken, which is what makes it the shape a
no-decision takes. So it is admitted on one of two grounds only. Either the capability has a supported form felis
documents and the tolerated input is the older spelling producers still emit beside it: `?1005` / `?1015` beside the
supported `?1006`, `OSC 633` beside `OSC 133`, SCS beside Unicode; or the capability is a recorded non-goal, and
tolerating buys nothing more than keeping the producer's bytes off the screen. An input that is neither is rejected: an
unknown sequence is something a producer can detect and branch on, and a silent swallow is not.

The tolerated set is enumerated in the [compliance reference](../../reference/protocols/vt-compliance.md)'s "Consumed
without effect", replies included, because the stub answers (`0;0`, `Ps=0`, `0$r`) are what a probing program branches
on: they are observable behavior, and a change to one is a break rather than a tweak. _Revisit if_ a target workflow
needs a tolerated input to take effect; the row then earns an implementation or its family is re-argued, and the shim
goes away with it.

## Control codes and charset

8-bit C1 (the single bytes `0x80–0x9F` read as controls) is rejected while their `ESC <byte>` spellings are adopted:
`0x80–0xFF` belongs to UTF-8, and every modern producer emits the ESC-prefix form (REQ-208).

## CSI

`?2048` is adopted while window-size reporting stays out (`CSI 14 / 15 / 16 t` answer the `0;0` stub, and XTWINOPS's
window manipulation and pixel-geometry reporting is a non-goal), because the two carry opposite risks. `?2048` is a
notification a program opts into: setting the mode draws one immediate report, and from then on the terminal pushes a
report on every effective geometry change and on nothing else. The notification reports the session's own PTY geometry,
with the pixel axes carrying the same `0` stub `CSI 14 t` answers, so the push never widens what a program can learn
about the host. Over the `SIGWINCH` a PTY resize raises, delivery is in band: the new size arrives on the read stream
the program is draining, in order against adjacent input. A `SIGWINCH` races that input, collapses rapid resizes into a
single signal, and requires a `TIOCGWINSZ` round-trip to learn what the signal meant. That ordering matters more under
felis than under a single-window terminal, because the daemon resizes a PTY for a window resize, for a
`window retarget`, and when size ownership moves to another attached client. A window-geometry query, by contrast, is
pulled at any moment by any program that inherits the PTY, and on a terminal that answers it honestly it replies with
the host's pixel extents, which is desktop information the shell has no claim on. The report-only XTWINOPS arms
(`CSI 11 t`, `CSI 18 t` / `19 t`, `CSI 20 t` / `21 t`) pass the same test from the other side: each answers from grid
state alone and tells the host nothing.

Two smaller admissions inside CSI:

- **The alternate-screen legacy trio.** `?47` / `?1047` / `?1048` are implemented beside `?1049` because the legacy trio
  is what pre-1049 curses apps still emit.
- **The rectangular-editing family.** One bounds model serves the whole family, so the attribute pair (DECCARA /
  DECRARA) is an increment on it rather than a surface of its own.

Of the omissions inside CSI, DECCOLM's resize arm is the one that takes an argument, and it is in
[the compliance design](vt-compliance.md) "Conscious omissions". DECDLD, NRCS and the VT500 page-memory controls are
statuses, which the [support matrix](../../reference/protocols/support-matrix.md) carries.

## OSC

- **`OSC 13` through `19` are tolerated for three reasons at once.** There is no Tektronix mode to color (15 / 16 / 18),
  the mouse pointer is drawn by the OS compositor rather than by felis (13 / 14), and selection colors (17 / 19) are a
  client-config concern that no target workflow sets dynamically. _Revisit 17 / 19 if_ one does.
- **ConEmu's `OSC 9` subcommands, progress (`9 ; 4`) included, are tolerated rather than decoded.** Progress waits on a
  window-chrome indicator, and winit exposes no portable taskbar / dock progress API to draw one; decoding the family
  into a typed event before anything can consume it would put a wire shape on the IPC surface that no reader constrains.
  The bare `OSC 9 ; <message>` form is iTerm2's notification, which felis decodes and relays
  ([notifications.md](notifications.md)).
- **`OSC 72` (kitty structured drag-and-drop) is rejected for now.** A window-side file drop inserts the path verbatim
  instead ([input.md](../input.md) "Drag-and-drop"). _Revisit if_ a target workflow depends on the structured form.

## DCS

**XTSETTCAP is rejected**, and not on the "no producer needs it" ground the other DCS rejections rest on: rewriting the
cap table lets a producer reshape what every later program in the session believes the terminal is.

## Kitty keyboard

The protocol is adopted with every progressive-enhancement bit, and the flag stack starts empty, so what a session
encodes before a program pushes flags is legacy xterm. The Kitty encoding is the one felis prefers, not one it imposes
on programs that never ask for it.
