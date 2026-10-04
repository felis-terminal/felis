---
title: VT / ANSI compliance
sidebar:
  order: 2
---

The behavioral semantics of the supported VT/ANSI escape sequences, and the exact reply payloads of the reporting
queries.

Normative support status is cataloged in the [protocol support matrix](support-matrix.md#vt--ansi). A sequence listed as
supported that is not mentioned here behaves according to XTerm Control Sequences (`ctlseqs`) without felis-specific
caveats. Design rationale, target compatibility baselines, and omissions are documented in the companion explanation
document [vt-compliance.md](../../explanation/protocols/vt-compliance.md).

## Cursor and screen

- C0 control characters (BEL, BS, HT, LF, VT, FF, CR) act per `ctlseqs`; framing recognizes ESC, CSI, OSC, DCS, ST, with
  C1 handling under "Charset handling" below.
- CUU / CUD clamp at the active scroll margins per VT220 §5.7. The xterm cursor aliases HPA (``CSI ` ``), HPR (`CSI a`),
  VPR (`CSI e`), CHT (`CSI Pn I`), CBT (`CSI Pn Z`) are recognized; CHT clamps forward tabs to the right margin, and CBT
  deliberately ignores the left margin, matching xterm (`test_CBT_*` in esctest).
- Repeat: REP (`CSI Pn b`) reprints the last printed grapheme `Pn` times, a cluster whole (`❤️`, `🇯🇵`) rather than its
  base scalar. The anchor is cleared by any non-print dispatch, so REP after a control sequence is a no-op.
- Back / forward index: DECBI (`ESC 6`), DECFI (`ESC 9`). At the left or right margin they scroll the region
  horizontally instead of moving the cursor.
- Erase in display: ED (`CSI J`), 0 / 1 / 2 / 3, where 3 clears scrollback.
- Two-cell glyphs: an erase whose edge falls on one half of a two-cell glyph (ECH, EL, ED, DECSEL, DECSED, DECERA,
  DECSERA) widens to take the whole glyph, and a print, DECFRA, or OSC 66 block that overwrites one half blanks the
  other. This holds for a single wide scalar (`字`) and for a cluster (`❤️`, `👩‍💻`, `🇯🇵`) alike.
- Selective erase: DECSCA (`CSI Ps " q`) marks the pen protected (`1`) or unprotected (`0` / `2`); DECSEL (`CSI ? Ps K`)
  and DECSED (`CSI ? Ps J`) take the same `Ps` vocabulary as EL and ED but leave protected cells standing. SPA / EPA
  (`ESC V` / `ESC W`) set and clear a second, ISO protection bit: DECSEL and DECSED honor both bits, DECSERA only the
  DEC one.
- Column insert / delete: DECIC (`CSI Pn ' }`) and DECDC (`CSI Pn ' ~`) shift the columns from the cursor rightward or
  leftward. Both no-op when the cursor sits outside the active scroll region.
- Screen alignment: DECALN (`ESC # 8`) fills every cell with `E`, homes the cursor, and drops the top / bottom and left
  / right margins.
- Insert / delete line: IL / DL no-op when the cursor sits outside the active scroll region.
- Insertion / replacement mode: IRM (`CSI 4 h/l`); printed characters shift cells right by their width while set.
- Scrolling: SU (`CSI S`), SD (`CSI T`), each operating on the active scroll region.
- Scrolling region: DECSTBM (`CSI Pt ; Pb r`). Rows scrolled off the top reach scrollback only when the region is
  anchored at row 0 and the primary screen is active.
- Left / right margins: DECSLRM (`CSI Pl ; Pr s`), gated by DECLRMM (`?69`). With `?69` set, editing and scrolling honor
  the left / right margins; clearing `?69` collapses them to the full row width. SL / SR shift the full row width; the
  margins are not consulted. The `?69` gate is also what disambiguates `CSI s` between SCOSC (save cursor) and DECSLRM.
- Reverse index: RI (`ESC M`), which scrolls the region down at the top margin instead of moving the cursor up.
- Origin mode: DECOM (`?6`). With DECOM set, CUP / VPA address rows relative to the top margin and the cursor cannot
  leave the region.
- Auto-wrap mode: DECAWM (`?7`). With DECAWM clear, prints at the right edge overwrite the rightmost cell.
- Reverse wrap, xterm's two post-2023 bits, each requiring DECAWM: `?45` (`ReverseWrapInline`) lets a backward move
  cross the left edge only when the current row is an autowrap continuation, and never around the top margin; `?1045`
  (`ReverseWrapExtend`) wraps unconditionally, and at the top margin the cursor lands on the bottom margin.
- More fix: `?41` (xterm's `MoreFix`). While set, an HT at a cell with a wrap already pending performs the deferred line
  feed first instead of consuming the pending wrap in place.
- 132-column switch: DECCOLM (`?3`), gated by Allow80To132 (`?40`, reset by default as in xterm). With `?40` set,
  DECCOLM resets the margins, homes the cursor, and clears the screen; the column count itself does not change, because
  the daemon owns the geometry. DECNCSM (`?95`) suppresses only the clear, and only at DECSCL level 5 or above. The
  parameters of one `CSI ? … h` are applied in stream order, so `?40;3h` arms the permission before DECCOLM consults it
  and `?3;40h` does not.
- Reverse-video screen: DECSCNM (`?5`). The renderer XORs it with each cell's SGR 7 and swaps the resolved foreground /
  background pair, explicit indexed and direct-RGB colors included, so a reversed cell on a reversed screen paints
  normally. The swap covers the frame's default-background fill and padding; image pixels and the client's own chrome
  (scrollbar, search bar) are untouched. State is screen-global: it survives an alt-screen switch, and DECSTR and RIS
  are what clear it.
- Tab stops: HT (`0x09`), HTS (`ESC H`), TBC (`CSI g`, modes 0 and 3). One global stops table per grid; the default is
  every 8 columns.
- Alternate screen buffer, all four xterm variants with their distinct cursor and buffer semantics: `?1049` (save
  cursor, switch, home, blank; drop the alt buffer on leave), `?47` and `?1047` (switch without cursor save), and
  `?1048` (save / restore cursor with no buffer switch). `?47l` preserves the alt buffer for a later re-entry while
  `?1047l` / `?1049l` discard it.
- xterm title stack: `CSI 22 t` pushes the window / icon title, `CSI 23 t` pops it. `Ps` selects icon and window (`0`),
  icon (`1`) or window (`2`). The stack holds 32 entries; a push against a full stack evicts the oldest.
- Conformance level: DECSCL (`CSI Pl ; Ps " p`), `Pl` 61–65 for VT100 through VT500, default level 4. Selecting a level
  drops the `OSC 4 / 5 / 10 / 11 / 12` color overrides and sets the C1 form; screen, cursor, margins, and modes are
  untouched. The level gates two replies: DECRQM is unrecognized below level 3, and DECSLRM below level 4. `Ps=1`
  selects 7-bit C1 responses, `Ps=0` and `Ps=2` 8-bit; S7C1T (`ESC SP F`) and S8C1T (`ESC SP G`) set the same flag on
  their own. With 8-bit C1 selected, every reply felis emits ships its introducer as a single C1 byte.
- Mode save / restore: xterm's `CSI ? Ps s` saves the current state of each named DEC private mode and `CSI ? Ps r`
  restores it, replaying the mode's full side effects as a DECSET / DECRST would. A mode that was never saved restores
  to nothing; a saved mode felis does not implement restores its soft-tracked flag.
- Application-cursor / -keypad mode: DECCKM (`?1`), DECKPAM (`ESC =`), DECKPNM (`ESC >`). State is mirrored to the
  client over IPC so the keyboard encoder consults it at keystroke time.
- Soft reset: DECSTR (`CSI ! p`). Restores scroll region, autowrap, origin mode, IRM, mouse / focus / paste flags, tab
  stops, and pen while leaving cells alone.

## Rectangular editing

The `$`-intermediate rectangle operations share one bounds model: `Pt ; Pl ; Pb ; Pr` are 1-based and inclusive; a `0`
or missing parameter defaults to `1` for top/left and to the grid edge for bottom/right; an inverted rectangle
(`Pt > Pb` or `Pl > Pr`) is a no-op. With DECOM set the coordinates are relative to the top margin, and to the left
margin too when DECLRMM (`?69`) is on. DECSTBM / DECSLRM margins never clip the affected area.

- Copy area: DECCRA (`CSI Pt;Pl;Pb;Pr;Pp;Pt';Pl';Pp' $ v`). The page parameters are accepted and ignored (felis is
  single-page). Source and destination are each clipped to the grid, so only cells inside both are copied; a
  self-overlapping copy snapshots the source before writing.
- Fill area: DECFRA (`CSI Pch;Pt;Pl;Pb;Pr $ x`). Fills the rectangle with `Pch` under the active pen. `Pch` must be
  printable Latin-1 (`0x20–0x7E`, `0xA0–0xFF`); any other value makes the command a no-op.
- Erase area: DECERA (`CSI Pt;Pl;Pb;Pr $ z`). Cells revert to the default (empty character, default pen) regardless of
  protection.
- Selective erase area: DECSERA (`CSI Pt;Pl;Pb;Pr $ {`). Like DECERA but preserves DECSCA-protected cells. SPA / EPA
  (ISO) protection does not shield a cell from DECSERA; the two protection bits are tracked separately.
- Change attributes in area: DECCARA (`CSI Pt;Pl;Pb;Pr;Ps... $ r`). The `Ps` list names SGR attributes to set or clear
  on every cell of the extent. Everything it does not name survives (colors, the underline color, the hyperlink, both
  protection bits). The reachable set is bold (`1` / `22`), faint (`2` / `22`), italic (`3` / `23`), underline (`4`, its
  `4:n` shapes, `21` for double, `24`), blink (`5` / `6` / `25`), reverse (`7` / `27`), invisible (`8` / `28`) and
  strikethrough (`9` / `29`). `0`, which is also what an omitted list means, clears all of them; any other selector,
  colors included, is ignored.
- Reverse attributes in area: DECRARA (`CSI Pt;Pl;Pb;Pr;Ps... $ t`). Same selector vocabulary, toggled rather than
  assigned; an "off" spelling names the same attribute as its "on" twin, and each selector in the list is applied in
  turn, so naming one twice reverses it twice and leaves the cell unchanged.
- DECSACE (`CSI Ps * x`) selects the extent DECCARA and DECRARA cover: `2` is the rectangle itself, `0` and `1` (the
  default) the stream from the start point to the end point in reading order, which spans the full row width in between.
  The other four rectangle operations are always rectangular and do not branch on it.

The rectangle checksum query, DECRQCRA, shares this bounds model; its reply is specified under
[Reporting and queries](#reporting-and-queries).

## Text attributes

- Blink (SGR 5 / 6) is parsed and tracked, with fast blink folded to the single slow flag, but never rendered:
  accessibility.
- 256-color and 24-bit truecolor (SGR 38 / 48) parse both the legacy semicolon forms (`38;2;r;g;b`, `38;5;n`) and the
  ITU colon sub-parameter forms. In the colon form the optional colorspace-id slot after the `2` is honored:
  `38:2::r:g:b` skips the empty slot rather than reading it as the red channel.

## Mouse and focus

- SGR-pixels mouse encoding (`?1016h`): the report carries the pointer's true pixel position within the text area,
  filled by the client from the physical cursor (only the client knows the cell metrics). Motion events are still
  emitted at cell granularity: a move that stays inside one cell produces no separate event. Every event that is emitted
  carries real pixels, not cell-derived approximations.

## OSC

- An OSC string is terminated by either BEL or ST; both are accepted.
- Color specs in the ChangeColor families felis acts on (`OSC 4 / 5 / 10–12`) are `rgb:r/g/b` and `#rgb`. The `#` form
  is X11's 1-to-4-digit hex channels, left-justified into 16 bits per `XParseColor(3)`, so `#abc` is
  `(0xA0, 0xB0, 0xC0)`; felis left-justifies the `rgb:` channels the same way. Named colors, the colorimetric spaces
  (`CIELab`, `CIELuv`, `CIEXYZ`, `CIExyY`, `CIEuvY`, `TekHVC`) and the `rgbi:` intensity form are refused: the addressed
  color keeps its value and no reply is sent. `OSC 13–19` are accepted and ignored whatever the spec form.
- `OSC 4 / 104`: indexed-palette set / reset. One `OSC 4` may carry several `idx ; spec` pairs; `OSC 104` takes a list
  of indices, and a bare `OSC 104` drops every override. The overrides are session state: they reach each attached
  window as a per-index delta, are replayed on attach, and are resolved at paint time: a cell stores the palette index,
  never the color, so a recolor reaches cells already on screen and in scrollback. A `?` spec queries the slot,
  answering with the override where one is set and the xterm-256 baseline otherwise. `OSC 104` leaves the
  `OSC 10 / 11 / 12` channels alone; their reset is `OSC 110 / 111 / 112`.
- `OSC 5 / 105`: special-color set / reset (the colors addressed above the 256-entry ANSI palette).
- `OSC 8`: hyperlink. The form is `OSC 8 ; params ; URI ST`, where `params` is a colon-separated `key=value` list of
  which felis recognizes `id=` alone. `OSC 8 ; ; ST` closes the open link, as does a bare `OSC 8`. A control byte in
  `params` or the URI, a URI that is not valid UTF-8, and a URI outside the scheme allowlist each leave the pen
  unlinked, so the text that follows prints without a link rather than inheriting the previous one (REQ-910). CAN and
  SUB are the exception: they cancel the sequence before it is dispatched, as they cancel any other, so the pen keeps
  its link.
- `OSC 10 / 11 / 12`: default fg / bg / cursor color. A `?` query reports the attached client's configured color (so a
  background detector reads felis's real surface, not the xterm baseline); a running program's set still wins, and a
  bare daemon with no client attached falls back to xterm's default pair (fg/cursor black, bg white). See
  [`session-lifecycle.md`](../../explanation/architecture/session-lifecycle.md) "Attach".
- `OSC 52`: clipboard write and query. The **write** to the system clipboard is the config-gated part (off by default;
  see [input.md](../../explanation/input.md)). A `?` **query** never reads the OS clipboard (that read is not
  implemented, REQ-802); it answers only from a daemon-local mirror holding the values the session itself most recently
  wrote.
- `OSC 22`: mouse pointer shape (a CSS cursor keyword; the kitty form), including the stack: `> name` pushes and makes
  the shape live, `<` pops back to the pushed shape (a pop on an empty stack resets to the default arrow). A
  comma-separated fallback list takes its first well-formed keyword: the daemon cannot know which shapes the client's
  `winit` build recognizes, so "first the terminal knows" degrades to "first well-formed", and the client maps an
  unknown-but-valid keyword to the arrow. The stack is daemon-side bookkeeping: only the live top ships, so there is no
  wire change.
- `OSC 133`: semantic prompt markers (A / B / C / D), used by jump-to-prompt features in shells; felis stores them on
  the grid for client use.
- `OSC 9 / 99 / 777`: desktop-notification families (iTerm2, kitty, rxvt-unicode). felis decodes them into typed events
  and relays them on its notification surface; it never draws a popup or links an OS backend. A first parameter that is
  a single ASCII digit followed by further parameters is ConEmu's subcommand family, not a message: it is consumed
  without effect, progress (`OSC 9 ; 4 ; …`) included. See [notifications.md](notifications.md).

## Charset handling

- Input is treated as UTF-8 only. Bytes that do not form valid UTF-8 are replaced with U+FFFD on the grid.
- C1 controls are recognized in their 7-bit `ESC <byte>` forms only (IND, NEL, RI, HTS, …). 8-bit C1 single bytes
  (`0x80–0x9F`) are never interpreted as controls: they participate in UTF-8 decoding (REQ-208).
- Single shifts SS2 / SS3 (`ESC N` / `ESC O`) are consumed by the parser as two-byte escapes and do not switch charsets.
  (`SS3` still appears on the _output_ side: the keyboard encoder emits it for cursor keys in application-keypad mode.)
- SOS and PM strings (`ESC X` / `ESC ^` … `ST`): the framing is recognized and the payload discarded, so a stray string
  does not reach the grid.
- Charset selection sequences (SCS, e.g. `ESC ( B`) are _parsed_ but no-op: felis will not honor requests to switch into
  the legacy DEC Special Graphics charset, and national replacement character sets (NRCS) are not implemented (REQ-211).
  Programs that rely on those characters should use the Unicode equivalents. The shipped terminfo entry cancels `acsc` /
  `smacs` / `rmacs` / `sgr` to match, so an ncurses program draws its boxes with ASCII approximations instead of
  emitting a charset switch felis ignores ([terminal-identity.md](../terminal-identity.md)).

## Reporting and queries

- DA1 (`CSI c` or `CSI 0 c`): replies `\e[?64;1;2;6;9;15;16;17;18;21;22;28;29c`, the xterm VT420 default under
  `--max-vt-level=4`.
- DA2 (`CSI > c`): replies `\e[>41;400;0c` (VT420 model, firmware 400).
- DA3 (`CSI = c` or `CSI = 0 c`): replies `\eP!|66656c69732d31\e\\` (`DCS ! |` … `ST`): the tertiary device-attributes
  unit ID, the hex of the ASCII string `felis-1` (`66 65 6c 69 73 2d 31`), so the reply names felis rather than the
  xterm all-zeroes placeholder.
- DECID (`ESC Z`): legacy alias for DA1; same response.
- XTVERSION (`CSI > 0 q`): replies `DCS > | felis <version> ST`.
- DECRQSS (`DCS $ q <setting> ST`): reports the active setting for the queried sequence: SGR (`m`, whose report includes
  underline color so a program that set SGR 58 round-trips it back), the scroll region (`r`), DECSCA (`"q`), DECSACE
  (`*x`), DECSCL (`"p`), DECSLRM (`s`), and the cursor shape (` q`). Any other setting, including the ones felis parses
  but does not implement (DECSASD `$}`, DECSSDT `$~`, DECSNLS `*|`, DECSLPP `t`), answers the invalid form under
  [Consumed without effect](#consumed-without-effect), which tells a probing program to fall back to its defaults.
- XTGETTCAP (`DCS + q`): answers four caps, each under its short and long name: `Co` / `colors` (`256`), `TN` / `name`
  (the `TERM` the session's program was spawned with, `xterm-felis` unless `FELIS_TERM` or the spawn request names
  another; see [terminal-identity.md](../terminal-identity.md)), and `co` / `cols` and `li` / `lines`, which report the
  live grid dimensions rather than a hardcoded `80` / `24`. Any other cap is answered as unknown (`DCS 0 + r`). The
  `;`-joined query body is bounded at 512 bytes so a multi-cap request is not truncated into an unanswerable form.
- DSR (`CSI 5 n`, `CSI 6 n`): replies `CSI 0 n` (status OK) and `CSI <row> ; <col> R` (cursor position, 1-based and
  respecting DECOM).
- Private DSR (`CSI ? Ps n`): felis has no printer, no user-defined keys, no locator and no macro store, so each reply
  reports the device absent, locked, or unsupported in xterm's shapes. A `Ps` outside this table is answered with
  nothing.

  | Request                    | Reply                                      | Reports                                                             |
  | -------------------------- | ------------------------------------------ | ------------------------------------------------------------------- |
  | `CSI ? 6 n`                | `CSI ? <row> ; <col> ; 1 R`                | DECXCPR, the cursor position with its page; the page is always `1`  |
  | `CSI ? 15 n`               | `CSI ? 13 n`                               | no printer                                                          |
  | `CSI ? 25 n`               | `CSI ? 21 n`                               | user-defined keys locked (felis has no DECUDK)                      |
  | `CSI ? 26 n`               | `CSI ? 27 ; 1 ; 0 ; 0 n`                   | North American keyboard, no ready state, default keypad mode        |
  | `CSI ? 53 n`, `CSI ? 55 n` | `CSI ? 50 n`                               | no DEC Locator                                                      |
  | `CSI ? 56 n`               | `CSI ? 57 ; 0 n`                           | locator type none                                                   |
  | `CSI ? 62 n`               | `CSI 0 * {`                                | DECMSR, zero macro space                                            |
  | `CSI ? 63 ; Pid n`         | `DCS <Pid> ! ~ 0000 ST`                    | DECCKSR, the checksum of an empty macro store; `Pid` is echoed back |
  | `CSI ? 75 n`               | `CSI ? 70 n`                               | data integrity ready, no errors                                     |
  | `CSI ? 85 n`               | `CSI ? 83 n`                               | not configured for multiple sessions                                |
  | `CSI ? 996 n`              | `CSI ? 997 ; 1 n` (dark) / `; 2 n` (light) | the OS color scheme                                                 |

- DECRQCRA (`CSI Pid ; Pp ; Pt ; Pl ; Pb ; Pr * y`): replies `DCS <Pid> ! ~ <4 hex digits> ST` with the checksum of the
  rectangle, under the bounds model of [Rectangular editing](#rectangular-editing); `Pp` is accepted and ignored. The
  arithmetic is xterm's `do_dec_check_sum`: each cell subtracts its codepoint (an empty cell counts as a space), and its
  attributes subtract bold 1, underline 2, reverse 4, blink 8, conceal 32. The result is truncated to 16 bits and
  written as four uppercase hex digits. An inverted rectangle checksums as `0000`.
- DECRQM (`CSI [?] Pn $ p`): `Ps=1` set, `2` reset, `4` permanently reset, `0` unknown. Modes whose state is canonical
  report it live: DECCKM, DECSCNM, DECOM, DECAWM, DECTCEM, alt-screen (`?1049`), bracketed paste, focus, synchronized
  output, DECLRMM (`?69`), the split reverse-wrap bits (`?45` / `?1045`), more fix (`?41`), color-scheme notify
  (`?2031`), in-band resize notify (`?2048`), and IRM and LNM (the two ANSI modes with an implementation). Grapheme
  cluster mode (`?2027`) always reports set. Mouse modes and DECCOLM (`?3`) are soft-tracked: after a DECSET / DECRST
  they report the written state (DECCOLM as modifiable, since the column-count change is deferred). Modes xterm treats
  as permanently reset (DECARM among them) report `4`. The modes xterm treats as modifiable but felis does not implement
  (`?3`, `?4`, `?18`, `?19`, `?34`, `?35`, `?36`, `?42`, `?57`, `?66`, `?67`) report `2` before any write and their
  written state after. Every remaining DEC mode reports `0` until a DECSET / DECRST writes it, and its written state
  thereafter. Below DECSCL level 3 the whole sequence is unrecognized and nothing is sent. Every remaining ANSI mode
  (KAM and SRM among them) reports `0` unconditionally: felis accepts the SM/RM write and drops it, so there is no state
  to echo and it does not invent one.
- DECREQTPARM (`CSI Ps x`): VT100 legacy. `Ps=0` replies `\e[2;1;1;120;120;1;0x` (unsolicited); `Ps=1` replies the same
  with `sol=3` (solicited).
- Window-size query (`CSI 18 t`, `CSI 19 t`): replies `CSI 8 ; <rows> ; <cols> t` and `CSI 9 ; <rows> ; <cols> t`, in
  cells.
- Window-state and title queries: `CSI 11 t` replies `CSI 1 t` (felis is always "shown"); `CSI 20 t` replies
  `OSC L <icon name> ST` and `CSI 21 t` replies `OSC l <window title> ST`. An unset name answers with an empty payload
  rather than nothing, so a probe can tell "supported" from "unsupported".
- Pixel-size queries (`CSI 14 t`, `CSI 15 t`, `CSI 16 t`): stubbed, see
  [Consumed without effect](#consumed-without-effect).
- In-band resize notify (`CSI ? 2048 h` / `l`): while set, the terminal writes `CSI 48 ; rows ; cols ; height ; width t`
  to the PTY on every change of the session's cell geometry, whatever moved it: a window resize, a `window retarget`, or
  a second attached client taking the size. The set answers with one immediate report of the current geometry; the reset
  answers with nothing. The height and width axes are `0`, on the same rule as the pixel-size queries above. A resize
  that changes only the pixel extents reports nothing, and a session whose shell has already exited is silent.
- Synchronized output (`CSI ? 2026 h` / `l`): the client defers a frame until the daemon signals "synchronized update
  ended".

## Consumed without effect

Each input below is read to its end and has no functional effect: it draws nothing and changes no later session
behavior. Two kinds of residue survive that, and the rows carrying them say so: a tolerated DEC private mode is
soft-tracked, so a later DECRQM reports the flag the producer wrote, and `SGR 5` / `6` sets a cell attribute that ships
over the wire. The table is the complete set, and its replies are normative. The admission rule for a new row is
[the landscape](../../explanation/protocols/landscape.md#admitting-a-tolerated-input)'s.

The "Pinned by" column names the test that holds the row; unqualified names live in `felis-grid`.

| Input                                                                                                                                                                                                                               | Reply                                                                                                                                      | Supported alternative                                                                            | Pinned by                                                                                                          |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `CSI ? 9 h` / `l` (X10 mouse)                                                                                                                                                                                                       | none; soft-tracked, so a later `CSI ? 9 $ p` answers `CSI ? 9 ; 1 $ y` (set)                                                               | `?1000` / `?1002` / `?1003` tracking under the `?1006` encoding                                  | `x10_mouse_mode_is_consumed_without_enabling_tracking`                                                             |
| `CSI ? 1005 h` / `l`, `CSI ? 1015 h` / `l` (legacy UTF-8 and URXVT encodings)                                                                                                                                                       | none; soft-tracked for DECRQM as `?9` is                                                                                                   | `?1006` (SGR), `?1016` (SGR-pixels)                                                              | `mouse_legacy_encodings_1005_1015_are_tolerated`                                                                   |
| `CSI ? 12 h` / `l` (cursor blink)                                                                                                                                                                                                   | none; soft-tracked for DECRQM as `?9` is                                                                                                   | DECSCUSR (`CSI Ps SP q`), whose `Ps` selects a blinking shape                                    | —                                                                                                                  |
| `OSC 13`–`19` (pointer, highlight, Tektronix colors)                                                                                                                                                                                | none                                                                                                                                       | `OSC 22` for the pointer shape; none for the colors themselves                                   | `tolerated_osc_families_leave_no_state_and_no_reply`                                                               |
| `OSC 9 ; <digit> ; …` (ConEmu subcommands, progress `9 ; 4` included)                                                                                                                                                               | none                                                                                                                                       | none; the notification form is `OSC 9 ; <message>`                                               | `conemu_osc9_4_progress_is_not_a_notification`                                                                     |
| `OSC 633` (VS Code shell integration)                                                                                                                                                                                               | none                                                                                                                                       | `OSC 133`                                                                                        | `tolerated_osc_families_leave_no_state_and_no_reply`                                                               |
| `OSC 1337` non-image subcommands                                                                                                                                                                                                    | none                                                                                                                                       | `OSC 133` for shell integration, the Kitty graphics protocol for images                          | `tolerated_osc_families_leave_no_state_and_no_reply`                                                               |
| SCS charset selection (`ESC ( B`, `ESC ) 0`, …)                                                                                                                                                                                     | none                                                                                                                                       | Unicode, box-drawing characters included                                                         | `scs_charset_selection_leaves_printed_text_unchanged`                                                              |
| `SGR 5` / `6` (blink)                                                                                                                                                                                                               | none; the cell flag is tracked and shipped but never rendered                                                                              | none for text; the cursor's own blink is `cursor.blink`                                          | `each_set_flag_sgr_lights_its_own_bit` (the flag; no renderer reads it)                                            |
| `CSI 1 t`–`CSI 9 t` (XTWINOPS window manipulation: resize, move, raise, iconify)                                                                                                                                                    | none                                                                                                                                       | none                                                                                             | —                                                                                                                  |
| `CSI 13 t` / `14 t` / `15 t` / `16 t` (window position, pixel sizes)                                                                                                                                                                | `CSI 3 ; 0 ; 0 t` / `CSI 4 ; 0 ; 0 t` / `CSI 5 ; 0 ; 0 t` / `CSI 6 ; 0 ; 0 t`                                                              | `CSI 18 t` / `19 t` in cells; `TIOCGWINSZ` for a window's pixels                                 | `xterm_window_op_report_arms_emit_their_fixed_replies`                                                             |
| `CSI Ps $ p` for an ANSI mode without an implementation and outside the row below, `CSI ? Ps $ p` for an unrecognized DEC mode (DECRQM)                                                                                             | `CSI Ps ; 0 $ y` / `CSI ? Ps ; 0 $ y`; a DEC mode a DECSET / DECRST has written is soft-tracked from then on and answers `1` / `2` instead | the modes listed under [Reporting and queries](#reporting-and-queries)                           | `ansi_unimplemented_modes_report_unknown`, `decrqm_dec_unknown_mode_reports_ps_zero_until_written`                 |
| `CSI ? 4 / 18 / 19 / 34 / 35 / 36 / 42 / 57 / 66 / 67 h` / `l` (DECSCLM smooth scroll, DECPFF / DECPEX printing, DECRLM / DECHEBM / DECHEM right-to-left and Hebrew, DECNRCM national charsets, DECNAKB / DECNKM / DECBKM keyboard) | none; soft-tracked, and their DECRQM answers `CSI ? Ps ; 2 $ y` before any write                                                           | DECKPAM / DECKPNM (`ESC =` / `ESC >`) for the `?66` keypad, Unicode for `?42`; none for the rest | `soft_dec_modes_round_trip_decset_decreset` (the replies; no test pins the absence of an effect)                   |
| `CSI ? 8 h` / `l` (DECARM), `CSI ? 60 / 61 / 64 / 68 / 73 / 81 h` / `l` (cursor coupling, keyboard usage, transmit rate, keypad)                                                                                                    | none; their DECRQM answers `CSI ? Ps ; 4 $ y` whatever SM / RM wrote                                                                       | none                                                                                             | `permanently_reset_dec_modes_always_report_ps4`                                                                    |
| `CSI Ps $ p` for an ANSI mode xterm reports permanently reset (GATM, SRTM, VEM, HEM, PUM, FEAM, FETM, MATM, TTM, SATM, TSM, EBM)                                                                                                    | `CSI Ps ; 4 $ y`, whatever SM / RM wrote                                                                                                   | none                                                                                             | `ansi_permanently_reset_modes_report_ps4`                                                                          |
| `DCS $ q <setting> ST` for a setting felis does not keep (DECRQSS)                                                                                                                                                                  | `DCS 0 $ r ST`                                                                                                                             | the settings listed under [Reporting and queries](#reporting-and-queries)                        | `decrqss_answers_supported_settings_and_rejects_the_rest`                                                          |
| `CSI Ps $ }` (DECSASD), `CSI Pn $ ~` (DECSSDT), `CSI Pn * \|` (DECSNLS), `CSI Ps t` with `Ps ≥ 24` (DECSLPP)                                                                                                                        | none; their DECRQSS queries answer `DCS 0 $ r ST`                                                                                          | none                                                                                             | `csi_pn_t_ge_24_is_consumed_and_decslpp_is_unsupported`, `decrqss_answers_supported_settings_and_rejects_the_rest` |

## Limits

- OSC body: 8192 bytes between the introducer and the terminator, the numeric prefix included. A longer body is
  truncated to the cap and still dispatched.
- DCS body: 512 bytes. A longer DECRQSS or XTGETTCAP request is truncated before it is answered, which is why the
  `;`-joined multi-cap form is the one to prefer.
- xterm title stack: 32 entries; a push against a full stack evicts the oldest.
- Kitty keyboard flag stack: 32 entries ([key-encoding.md](key-encoding.md)).
