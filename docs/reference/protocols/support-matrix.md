---
title: Protocol support matrix
sidebar:
  order: 1
---

Implementation status of the ANSI/VT control sequences, private modes, and extension protocols, per platform.

Status tables own normative feature coverage. Detailed architectural rationale and edge-case behavior are documented in
companion explanation pages ([vt-compliance.md](../../explanation/protocols/vt-compliance.md),
[kitty-graphics.md](../../explanation/protocols/kitty-graphics.md),
[kitty-text-sizing.md](../../explanation/protocols/kitty-text-sizing.md), and [input.md](../../explanation/input.md)).

**Legend:** ✅ supported · ⚠️ partial, stubbed, deferred, or
[consumed without effect](vt-compliance.md#consumed-without-effect), which is the tolerated set a new input joins under
the admission rule in [landscape.md](../../explanation/protocols/landscape.md#admitting-a-tolerated-input) · 🚫 the
capability is a non-goal ([non-goals.md](../../explanation/non-goals.md)) and the input stays outside that tolerated
set, answering an error or nothing at all. Membership in the tolerated set is what decides between the two: an input the
parser consumes and drops is ⚠️ even where the capability behind it is a non-goal, and those rows name the non-goal. A
✅ records what the code implements, not a support claim: support claims follow evidence plus a distribution, which
`x86_64-linux`, `aarch64-darwin` and `x86_64-pc-windows-msvc` have and `aarch64-linux` does not
([workspace.md](../workspace.md#build-and-platform-matrix)).

## VT / ANSI

Detail and per-sequence caveats: [vt-compliance.md](vt-compliance.md).

### Cursor and screen

| Sequence                                                                             | Status                                                                                                    |
| ------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------- |
| C0 controls (BEL, BS, HT, LF, VT, FF, CR)                                            | ✅                                                                                                        |
| IND (`ESC D`) / NEL (`ESC E`)                                                        | ✅                                                                                                        |
| CUP / HVP / CUU / CUD / CUF / CUB / CHA / VPA                                        | ✅                                                                                                        |
| xterm aliases HPA / HPR / VPR / CHT / CBT                                            | ✅                                                                                                        |
| DECSC / DECRC (+ `CSI s` / `CSI u`)                                                  | ✅                                                                                                        |
| ED / EL (incl. ED 3 → scrollback)                                                    | ✅                                                                                                        |
| DECSCA / DECSEL / DECSED selective erase (`CSI " q`, `CSI ? K`, `CSI ? J`)           | ✅                                                                                                        |
| IL / DL / ICH / DCH / ECH                                                            | ✅                                                                                                        |
| DECIC / DECDC column insert / delete (`CSI ' }` / `CSI ' ~`)                         | ✅                                                                                                        |
| IRM insert/replace (`CSI 4 h/l`)                                                     | ✅                                                                                                        |
| SU / SD scrolling                                                                    | ✅                                                                                                        |
| DECSTBM scroll region                                                                | ✅                                                                                                        |
| RI reverse index                                                                     | ✅                                                                                                        |
| REP repeat last graphic (`CSI Pn b`)                                                 | ✅                                                                                                        |
| DECBI / DECFI back / forward index                                                   | ✅                                                                                                        |
| Tab stops: HTS / TBC (modes 0, 3)                                                    | ✅                                                                                                        |
| DECTCEM cursor visibility (`?25`)                                                    | ✅                                                                                                        |
| DECSCUSR cursor shape                                                                | ✅                                                                                                        |
| Alternate screen (`?1049` / `?47` / `?1047` / `?1048`)                               | ✅                                                                                                        |
| DECCKM / DECKPAM / DECKPNM (cursor + keypad → SS3)                                   | ✅                                                                                                        |
| DECSLRM left-right margins (gated by DECLRMM `?69`)                                  | ✅                                                                                                        |
| xterm title stack (`CSI 22 / 23 t`)                                                  | ✅                                                                                                        |
| DECSTR soft reset / RIS full reset                                                   | ✅                                                                                                        |
| DECALN screen alignment (`ESC # 8`)                                                  | ✅                                                                                                        |
| DECSCL conformance level (`CSI Pl ; Ps " p`)                                         | ✅ (default level 4; gates DECRQM and DECSLRM)                                                            |
| S7C1T / S8C1T 7- / 8-bit C1 responses (`ESC SP F` / `ESC SP G`)                      | ✅                                                                                                        |
| xterm mode save / restore (`CSI ? Ps s` / `CSI ? Ps r`)                              | ✅                                                                                                        |
| Status display (DECSASD `CSI Ps $ }` / DECSSDT `CSI Pn $ ~`)                         | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (no status line to select or type) |
| DECSNLS lines per screen (`CSI Pn * \|`) / DECSLPP page length (`CSI Pn t`, Pn ≥ 24) | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (the daemon owns the row count)    |
| VT500 page memory (NP / PP / PPA / PPB / PPR)                                        | 🚫 (one page per grid)                                                                                    |
| Double-sized chars (DECDHL / DECDWL / DECSWL)                                        | 🚫                                                                                                        |
| DECCOLM 80↔132 (`?3`)                                                                | ⚠️ destructive arm (clear/home/margins) only; resize deferred                                             |
| VT52 mode (DECANM clear) / DEC Locator / Sun-keyboard sequences                      | 🚫                                                                                                        |
| XTWINOPS window manipulation arms (`CSI 1 t`–`CSI 9 t`)                              | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (the WM owns the window)           |

### Rectangular editing

| Sequence                                         | Status |
| ------------------------------------------------ | ------ |
| DECCRA copy area                                 | ✅     |
| DECFRA fill area                                 | ✅     |
| DECERA erase area                                | ✅     |
| DECSERA selective erase area                     | ✅     |
| DECCARA / DECRARA change / reverse attrs in area | ✅     |
| DECRQCRA rectangle checksum                      | ✅     |

### DEC private modes

| Mode                                                                                                                  | Status                                                                                                                                                                       |
| --------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| DECOM origin (`?6`)                                                                                                   | ✅                                                                                                                                                                           |
| DECAWM auto-wrap (`?7`)                                                                                               | ✅                                                                                                                                                                           |
| DECSCNM reverse video (`?5`)                                                                                          | ✅                                                                                                                                                                           |
| Reverse wrap (`?45` inline / `?1045` extend)                                                                          | ✅                                                                                                                                                                           |
| More fix (`?41`)                                                                                                      | ✅                                                                                                                                                                           |
| Allow80To132 (`?40`)                                                                                                  | ✅ (permission flag for the DECCOLM arm)                                                                                                                                     |
| DECNCSM (`?95`)                                                                                                       | ✅ (suppresses the DECCOLM clear at DECSCL level 5+)                                                                                                                         |
| Synchronized output (`?2026`)                                                                                         | ✅                                                                                                                                                                           |
| Grapheme cluster mode (`?2027`)                                                                                       | ✅ (always on)                                                                                                                                                               |
| in-band resize notify (`?2048`)                                                                                       | ✅ (cells; pixel axes `0`)                                                                                                                                                   |
| color-scheme notify (`?2031` + `DSR ?996`/`?997`)                                                                     | ✅ (OS light/dark → PTY)                                                                                                                                                     |
| DECARM auto-repeat (`?8`)                                                                                             | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (the OS owns repeat)                                                                                  |
| Cursor blink (`?12`)                                                                                                  | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (blink is DECSCUSR's `Ps`)                                                                            |
| Smooth scroll (DECSCLM, `?4`)                                                                                         | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (historical-terminal emulation is a [non-goal](../../explanation/non-goals.md#compatibility-theater)) |
| Print, right-to-left, Hebrew, NRCS and keyboard modes (`?18`, `?19`, `?34`, `?35`, `?36`, `?42`, `?57`, `?66`, `?67`) | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (historical-terminal emulation is a [non-goal](../../explanation/non-goals.md#compatibility-theater)) |

### Text attributes (SGR)

| Attribute                                                     | Status                                                                                                                     |
| ------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| bold / faint / italic / reverse / conceal / strike / overline | ✅                                                                                                                         |
| underline: single, double, curly, dotted, dashed              | ✅                                                                                                                         |
| blink                                                         | ⚠️ parsed, never rendered (accessibility); fast→slow ([consumed without effect](vt-compliance.md#consumed-without-effect)) |
| colored underline (SGR 58 / 59)                               | ✅                                                                                                                         |
| 8 / 16 / 256 / 24-bit truecolor (SGR 38 / 48)                 | ✅                                                                                                                         |

### Mouse and focus

| Feature                                                                       | Status                                                                                                         |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| SGR mouse (`?1006`)                                                           | ✅                                                                                                             |
| SGR-pixels mouse (`?1016`)                                                    | ✅                                                                                                             |
| X11 normal tracking (`?1000`) / button-event (`?1002`) / all-motion (`?1003`) | ✅                                                                                                             |
| X10 compatibility (`?9`)                                                      | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect)                                         |
| legacy UTF-8 (`?1005`) / URXVT (`?1015`) encodings                            | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (SGR `?1006` is the supported encoding) |
| focus tracking (`?1004`)                                                      | ✅                                                                                                             |
| bracketed paste (`?2004`)                                                     | ✅                                                                                                             |

### OSC

Notification wire formats and the relay contract: [notifications.md](notifications.md).

| OSC             | Purpose                                                                  | Status                                                                                                                                                                                                        |
| --------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 0 / 2           | window title                                                             | ✅                                                                                                                                                                                                            |
| 1               | icon name                                                                | ⚠️ recorded per session and answered by `CSI 20 t` ([vt-compliance.md](vt-compliance.md#reporting-and-queries) "Reporting and queries"); no wire arm carries it to a client, so no window chrome shows it     |
| 4 / 104         | palette set / reset                                                      | ✅ `rgb:` and `#hex` color specs; colorimetric specs (`CIELab`, `TekHVC`, …) and `rgbi:` are refused: the slot keeps its value and no reply is sent (🚫 non-goal; [vt-compliance.md](vt-compliance.md) "OSC") |
| 5 / 105         | special-color set / reset                                                | ✅ same color-spec subset as `OSC 4`                                                                                                                                                                          |
| 6               | legacy hyperlink                                                         | 🚫 (`OSC 8` is the supported form)                                                                                                                                                                            |
| 7               | working directory                                                        | ✅                                                                                                                                                                                                            |
| 8               | hyperlink (id + URI; scheme allowlist)                                   | ✅                                                                                                                                                                                                            |
| 10 / 11 / 12    | default fg / bg / cursor color                                           | ✅ same color-spec subset as `OSC 4`, plus the `?` query                                                                                                                                                      |
| 110 / 111 / 112 | color resets                                                             | ✅                                                                                                                                                                                                            |
| 22              | mouse pointer shape (CSS keyword; kitty, incl. `>` push / `<` pop stack) | ✅                                                                                                                                                                                                            |
| 52              | clipboard write / query                                                  | ✅ write always updates the session mirror (which answers `?`); propagation to the OS clipboard is the client's `clipboard.osc_52 = "system"` opt-in, off by default; OS-clipboard read not implemented       |
| 133             | semantic prompt marks (A/B/C/D)                                          | ✅                                                                                                                                                                                                            |
| 9               | notification (iTerm2)                                                    | ✅ decode + relay; the ConEmu subcommand family `OSC 9 ; <digit> ; …`, progress included, is [consumed without effect](vt-compliance.md#consumed-without-effect)                                              |
| 99              | desktop notifications (kitty)                                            | ✅ decode + relay (subset)                                                                                                                                                                                    |
| 777             | notify (rxvt-unicode)                                                    | ✅ decode + relay                                                                                                                                                                                             |
| 13–19           | highlight / pointer / Tektronix colors (and their `113`–`119` resets)    | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect)                                                                                                                                        |
| 50              | set font                                                                 | 🚫 (the client owns the font)                                                                                                                                                                                 |
| 72              | kitty drag-and-drop                                                      | 🚫 (a window-side file drop inserts the path instead; see [input](../../explanation/input.md))                                                                                                                |
| 633             | iTerm2 / VS Code shell integration                                       | ⚠️ [consumed without effect](vt-compliance.md#consumed-without-effect) (OSC 133 is the supported form)                                                                                                        |
| 666             | kitty config notify                                                      | 🚫 (felis has no config language)                                                                                                                                                                             |
| 1337            | iTerm2 images / controls                                                 | 🚫 images; non-image subcommands [consumed without effect](vt-compliance.md#consumed-without-effect)                                                                                                          |

### Reporting and queries

The exact reply payloads are specified in [vt-compliance.md](vt-compliance.md) "Reporting and queries".

| Query                                                | Status                                                                                                                                          |
| ---------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| DA1 (`CSI c`)                                        | ✅                                                                                                                                              |
| DA2 (`CSI > c`)                                      | ✅                                                                                                                                              |
| DA3 (`CSI = c`)                                      | ✅                                                                                                                                              |
| DECID (`ESC Z`)                                      | ✅ (DA1 alias)                                                                                                                                  |
| XTVERSION (`CSI > 0 q`)                              | ✅                                                                                                                                              |
| DSR (`CSI 5 n` / `6 n`)                              | ✅                                                                                                                                              |
| DSR private (`CSI ? Ps n`)                           | ✅ (`?6`, `?15`, `?25`, `?26`, `?53`, `?55`, `?56`, `?62`, `?63`, `?75`, `?85`)                                                                 |
| DSR color-scheme (`CSI ? 996 n`)                     | ✅                                                                                                                                              |
| DECRQM (`CSI ? Pn $ p`)                              | ✅ (of the ANSI modes only IRM and LNM report a state; the rest answer per [consumed without effect](vt-compliance.md#consumed-without-effect)) |
| DECRQSS (`DCS $ q`)                                  | ✅ (a setting felis does not keep answers the invalid form per [consumed without effect](vt-compliance.md#consumed-without-effect))             |
| DECRQCRA (rect checksum)                             | ✅                                                                                                                                              |
| XTSMGRAPHICS (graphics sizing `CSI ? … S`)           | 🚫 (sizes the Sixel/ReGIS surfaces and color registers felis does not have)                                                                     |
| XTGETTCAP (`DCS + q`)                                | ✅ (`Co`, `TN`, `co`, `li` and their long names)                                                                                                |
| XTSETTCAP (`DCS + p`)                                | 🚫 (the cap table is not writable)                                                                                                              |
| Legacy synchronized output (`DCS = 1 s` / `= 2 s`)   | 🚫 (`?2026` is the supported form)                                                                                                              |
| DECREQTPARM (`CSI Ps x`)                             | ✅ (VT100 legacy)                                                                                                                               |
| window-size cells (`CSI 18 t` / `19 t`)              | ✅                                                                                                                                              |
| window state / titles (`CSI 11 t` / `20 t` / `21 t`) | ✅                                                                                                                                              |
| pixel-size (`CSI 14 t` / `15 t` / `16 t`)            | ⚠️ stubbed; applications should query `TIOCGWINSZ` ([consumed without effect](vt-compliance.md#consumed-without-effect))                        |

### Charset

| Item                                        | Status                                                                                                     |
| ------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| UTF-8 input (invalid → U+FFFD)              | ✅                                                                                                         |
| 8-bit C1 bytes (`0x80–0x9F`)                | 🚫 never read as controls; the 7-bit `ESC <byte>` forms are (REQ-208)                                      |
| Single shifts SS2 / SS3 (`ESC N` / `ESC O`) | ⚠️ framing consumed, no charset switch (`SS3` is still emitted for cursor keys in application-keypad mode) |
| PM / SOS strings (`ESC ^` / `ESC X` … `ST`) | ⚠️ framing recognized, payload discarded                                                                   |
| SCS charset selection (`ESC ( B` …)         | ⚠️ no-op; use Unicode ([consumed without effect](vt-compliance.md#consumed-without-effect))                |
| NRCS national replacement charset mapping   | 🚫 (Unicode equivalents; REQ-211)                                                                          |
| DECDLD soft character sets (DRCS)           | 🚫 (REQ-210)                                                                                               |
| DEC Special Graphics line-drawing           | 🚫 (Unicode box-drawing is first-class; the terminfo entry cancels `acsc` to match)                        |

### APC

| Body                                             | Status                                                                        |
| ------------------------------------------------ | ----------------------------------------------------------------------------- |
| `ESC _ G …` (Kitty graphics)                     | ✅ ([Kitty graphics](#kitty-graphics))                                        |
| Any other body (mintty / tmux passthrough abuse) | 🚫 (no dispatcher reads it; passthrough to a nested terminal is out of scope) |

## Kitty graphics

Detail: [kitty-graphics.md](kitty-graphics.md).

### Transmission (`t=`)

The Unix targets below are the committed Unix set (`x86_64-linux`, `aarch64-linux`, `aarch64-darwin`), named per the
[build and platform matrix](../workspace.md#build-and-platform-matrix).

| `t=` | Method                                                  | Status                                                                                                          |
| ---- | ------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `d`  | direct (base64 in APC)                                  | ✅ every committed target                                                                                       |
| `f`  | local file path                                         | ✅ committed Unix targets · 🚫 `x86_64-pc-windows-msvc` (rejected with `ENOTSUP`)                               |
| `t`  | temp file (deleted after read)                          | ✅ committed Unix targets · 🚫 `x86_64-pc-windows-msvc` (rejected with `ENOTSUP`)                               |
| `s`  | shared memory object (read; unlink at session teardown) | ✅ committed Unix targets (Linux `pread`, macOS `mmap`) · 🚫 `x86_64-pc-windows-msvc` (rejected with `ENOTSUP`) |

### Image formats (`f=`) and compression

| Item                                   | Status                        |
| -------------------------------------- | ----------------------------- |
| `f=24` RGB / `f=32` RGBA / `f=100` PNG | ✅                            |
| JPEG / WebP                            | 🚫 (use PNG or raw)           |
| `o=z` zlib compression                 | ✅ (all transmission methods) |

### Display / lifecycle (`a=`)

| Key                   | Action                                    | Status                                                                                   |
| --------------------- | ----------------------------------------- | ---------------------------------------------------------------------------------------- |
| `a=t`                 | transmit only                             | ✅                                                                                       |
| `a=T`                 | transmit and display                      | ✅                                                                                       |
| `a=p`                 | display previously-transmitted            | ✅                                                                                       |
| `a=q`                 | query capabilities                        | ✅                                                                                       |
| `a=d`                 | delete (basic + extended `c/p/q/x/y/z/r`) | ✅                                                                                       |
| `a=a` / `a=f` / `a=c` | animation                                 | ✅                                                                                       |
| `d=f` / `d=F`         | delete frame                              | ✅                                                                                       |
| `z=`                  | z-index ordering                          | ✅                                                                                       |
| `U=1`                 | Unicode placeholder                       | ✅                                                                                       |
| `X=` / `Y=`           | destination pixel offset within the cell  | ⚠️ consumed without effect ([kitty-graphics.md](kitty-graphics.md#placement-parameters)) |

## Kitty text sizing (OSC 66)

Detail: [kitty-text-sizing.md](kitty-text-sizing.md).

| Item                                            | Status                                                                             |
| ----------------------------------------------- | ---------------------------------------------------------------------------------- |
| metadata keys `s` / `w` / `n` / `d` / `v` / `h` | ✅ (ranges and defaults: [kitty-text-sizing.md](kitty-text-sizing.md#wire-format)) |
| multi-cell width (`w`) / height (`s ≥ 2`)       | ✅                                                                                 |
| fractional sub-cell shrink (`s × n/d`)          | ✅                                                                                 |
| vertical / horizontal alignment                 | ✅                                                                                 |
| run exceeding screen dims                       | ✅ discarded (REQ-406)                                                             |
| resize-then-restore round-trip                  | ✅ deterministic while every run fits the intermediate width (REQ-407)             |
| legacy `CSI Pn:…:Pn t` sizing form              | 🚫 (OSC 66 only)                                                                   |

## Kitty keyboard

Detail: [key-encoding.md](key-encoding.md). felis implements the Kitty keyboard protocol; the flag stack starts empty,
so a fresh session encodes legacy xterm until a program pushes flags. The progressive-enhancement bits:

| Bit     | Feature                         | Status |
| ------- | ------------------------------- | ------ |
| 0b1     | disambiguate escape codes       | ✅     |
| 0b10    | report event types              | ✅     |
| 0b100   | report alternate keys           | ✅     |
| 0b1000  | report all keys as escape codes | ✅     |
| 0b10000 | report associated text          | ✅     |

xterm `modifyOtherKeys` (`CSI > 4 ; Pv m`) is tolerated for TUIs that set it before discovering the Kitty protocol:
level 2 routes modified keys through CSI u, as if the Kitty protocol were on (REQ-506).

Windows win32-input-mode (`CSI ? 9001 h`) is honored: while active, keys ship as win32-input records
(`CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`) so a ConPTY daemon rebuilds faithful `INPUT_RECORD`s for PSReadLine / PowerShell
(REQ-507, [key-encoding.md](key-encoding.md)).

| Mode                       | Status |
| -------------------------- | ------ |
| win32-input-mode (`?9001`) | ✅     |

## Image protocols out of scope

| Protocol                                     | Status                                                                              |
| -------------------------------------------- | ----------------------------------------------------------------------------------- |
| Sixel                                        | 🚫 (Kitty graphics covers the ground; Kitty itself does not implement Sixel either) |
| iTerm2 OSC 1337 inline images                | 🚫                                                                                  |
| mintty / mosh inline images                  | 🚫                                                                                  |
| FD-passing image transfer                    | 🚫                                                                                  |
| ReGIS / Tektronix 4014 vector graphics (DCS) | 🚫                                                                                  |

See [non-goals.md](../../explanation/non-goals.md) for the rationale behind every 🚫.
