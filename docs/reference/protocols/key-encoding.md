---
title: Key encoding
sidebar:
  order: 6
---

The byte sequences felis emits to the PTY for keyboard, mouse, and bracketed-paste input.

The action-mapping layer above the encoder (keymap, clipboard policy, selection, IME) and design rationale live in
[input.md](../../explanation/input.md); default chords and keymap grammar live in [keybindings.md](../keybindings.md).

## Keyboard encoding

felis implements the **Kitty keyboard protocol** (`CSI > Pn ; Pn ... u`), which is unambiguous about modifiers, function
keys, and alternate / shift-printed forms.

Legacy (xterm) mode is the default: it applies whenever no Kitty keyboard flag is active, which is the state of a fresh
session until a program pushes flags. It encodes:

- xterm-style for arrow / function keys, modifier-encoded with CSI `;n` parameters, except that F3 with a modifier is
  `CSI 13 ; n ~`, since `CSI 1 ; n R` reads as a cursor position report, Menu as `CSI 29 ; n ~` (xterm's F16), and the
  keypad's middle key with Num Lock off as `CSI 1 ; n E` (`SS3 E` under DECCKM),
- the functional keys with no legacy encoding (F13–F35, Print Screen, Pause, the media and volume keys) in their
  [Kitty forms](#kitty-key-events), `CSI number ; n u`, as kitty sends them; modifier and lock keys send nothing,
- keypad keys as the keys they are equivalent to: the digits and operators as their text with Num Lock on, and the
  navigation keys, keypad Enter included, as their main-block counterparts,
- backspace as DEL (`0x7F`), and Ctrl+Backspace as BS (`0x08`),
- Ctrl with a key whose produced character (Shift already applied) is one of those below as its C0 byte:

  | Character        | Byte          |
  | ---------------- | ------------- |
  | `a`–`z`, `A`–`Z` | `0x01`–`0x1A` |
  | `@`, Space, `2`  | `0x00`        |
  | `[`, `3`         | `0x1B`        |
  | `\`, `4`         | `0x1C`        |
  | `]`, `5`         | `0x1D`        |
  | `^`, `6`, `~`    | `0x1E`        |
  | `_`, `/`, `7`    | `0x1F`        |
  | `?`, `8`         | `0x7F`        |

  Ctrl with any other character sends that character's text,

- Alt as an `ESC` prefix: on a key producing a single ASCII character, `ESC` and that character with its case kept (with
  Ctrl, `ESC` and the C0 byte above, or `ESC` and the character itself when the table has no row for it); on Enter, Tab,
  Backspace, Escape and Space, `ESC` and the bytes the key sends without Alt, so Shift+Alt+Tab is `ESC CSI Z`,
- Ctrl, Alt, or both, without Shift, on a key of another layout that produces a non-ASCII character as the key the same
  physical key types on the PC-101 US layout, when the front-end reports it: Ctrl+С on a Russian layout is `0x03`, as
  Ctrl+C is. Other non-ASCII text goes out as typed, without the `ESC` prefix,
- the modifier combinations these rules have no bytes for as `CSI keycode ; 1+modifiers u`, as in
  [Kitty key events](#kitty-key-events): Super with any key producing a single character or with Enter, Tab, Backspace,
  Escape and Space; Ctrl, Alt and Shift together on those five keys; Ctrl+Shift on a letter, whose C0 byte would hide
  the Shift; and Ctrl+Shift on a symbol whose shifted character the table above has no row for, named by its unshifted
  key, so Ctrl+Shift+3 on a US layout is `CSI 51 ; 6 u`. A shifted character the table has a row for keeps its C0 byte:
  Ctrl+Shift+- is Ctrl with `_` and sends `0x1F`. Without the unshifted key from the front-end, Ctrl+Shift+3 is Ctrl
  with `#` and sends `#`.

The mode is driven by the running program, not by config. felis mirrors the flags on a stack per screen (depth in
[Limits](#limits)): the main and alternate screens each keep their own, and every sequence below acts on the stack of
the screen that is active. The active flag set is the top of that stack, and an empty stack resolves to no flags, which
is what makes legacy the starting mode of a fresh session and of a first switch to the alternate screen.

| Sequence          | Effect                                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------------------ |
| `CSI > Pn u`      | push `Pn` as the new top; bits felis does not implement are masked off                                 |
| `CSI < Pn u`      | pop `Pn` entries (default 1), stopping at the empty stack                                              |
| `CSI = Pn ; Pm u` | set the top entry: `Pm=1` (default) replaces it with `Pn`, `2` sets the named bits, `3` clears them    |
| `CSI ? u`         | query; replies `CSI ? <flags> u` with the active flag set as a decimal integer, `0` for an empty stack |

`CSI = Pn ; Pm u` against an empty stack pushes a fresh entry for `Pm=1` and `Pm=2`, and does nothing for `Pm=3`; a `Pm`
above `3` is ignored.

### Kitty key events

With bit 1 (disambiguate) or bit 8 (report all keys as escapes) active, a key that needs escaping is sent as
`CSI keycode[:shifted[:base]] ; 1+modifiers[:event] [; text] u`:

- `keycode`: the legacy byte of the named keys that have one (Enter `13`, Tab `9`, Backspace `127`, Escape `27`, Space
  `32`) or, for a key that produces a single character, its unshifted key: Ctrl+Shift+3 on a US layout is `51`, and
  Ctrl+С on a Russian layout is `1089`. Where the front-end does not report the unshifted key, it is the character's
  code lowercased, so Shift on a symbol stays in the code (`35`).
- `shifted` and `base`, the alternate keys, present only for presses while bit 4 (alternate keys) is active. `shifted`
  is the character the key produces while Shift is held, when that differs from `keycode` (Ctrl+Shift+3 is `51:35`);
  without the unshifted key it is reported for an ASCII letter only. `base` is what the same physical key types on the
  PC-101 US layout, when the front-end reports it and it differs from `keycode`, after an empty `shifted` when there is
  none: Ctrl+С on a Russian layout is `1089::99`, Ctrl+Shift+С `1089:1057:99`.
- `modifiers`: shift `1`, alt `2`, ctrl `4`, super `8`, hyper `16`, meta `32`. The hyper and meta bits are set only by
  the press of a Hyper or Meta key itself, and the caps-lock and num-lock bits never, since the window system reports
  neither state. The parameter and everything after it are omitted when no modifier is held, the event is a press, and
  no text follows.
- `event`: `2` for an auto-repeat, `3` for a release, omitted for a press. Both are sent only while bit 2 (event types)
  is active; without it a repeat is reported as a press, and a release is not reported at all. A key that is not sent as
  an escape sequence (typed text, and Enter, Tab and Backspace without modifiers) reports no release, and its repeat is
  its bytes again. Every other encoding (legacy, `modifyOtherKeys`, DECKPAM) has no event field, so a repeat is a press
  there too. win32-input-mode reports both edges through `Kd` and always sets `Rc` to `1`, since each repeat arrives as
  its own event.
- `text`: the codepoints of the OS-composed text, colon-separated, present only for presses while bit 16 (associated
  text) is active, and left out when the text contains a control code or Ctrl turns the key into a control byte (the
  "Legacy ctrl mapping" table: Ctrl+a, Ctrl+[, Ctrl+2, Ctrl+Space).

Bit 1 escapes Enter, Tab, Backspace, Space, and any key producing a single character when Ctrl, Alt, or Super is held,
Ctrl+C included, and Escape even unmodified; an unmodified key keeps its legacy bytes. Bit 8 escapes every key, and
reports the modifier and lock keys besides. Shift+Enter, Shift+Backspace, and Shift+Space are escaped under bit 1
although shift alone would not require it, matching Kitty; Shift+Tab stays `CSI Z`.

With bit 1, bit 2, or bit 8 active, arrows, Home, End, the editing cluster, and function keys take the functional forms
of the Kitty spec, never `SS3` and whatever DECCKM says:

| Key                                | Form                                     |
| ---------------------------------- | ---------------------------------------- |
| Up, Down, Right, Left, End, Home   | `CSI 1 ; m {A B C D F H}`                |
| F1, F2, F4                         | `CSI 1 ; m {P Q S}`                      |
| Insert, Delete, Page Up, Page Down | `CSI {2 3 5 6} ; m ~`                    |
| F3, F5–F12                         | `CSI {13 15 17 18 19 20 21 23 24} ; m ~` |
| F13–F35                            | `CSI {57376–57398} ; m u`                |
| Keypad middle key, Num Lock off    | `CSI 1 ; m E`                            |
| Print Screen, Pause, Menu          | `CSI {57361 57362 57363} ; m u`          |
| Media play, pause, play/pause      | `CSI {57428 57429 57430} ; m u`          |
| Media stop, fast forward, rewind   | `CSI {57432 57433 57434} ; m u`          |
| Media next, previous, record       | `CSI {57435 57436 57437} ; m u`          |
| Volume down, up, mute              | `CSI {57438 57439 57440} ; m u`          |

`m` is `1+modifiers[:event]`; an unmodified press drops it, and with it the `1` before a letter final.

With bit 1 or bit 8 active, a keypad key takes its own code, as `CSI number ; m u`, when it types no text or is held
with Ctrl, Alt or Super; bit 8 gives every keypad key its own code. A digit or operator that types its text unmodified
under bit 1 sends the text. Without either bit, a keypad key is sent as the key it is equivalent to.

| Keypad key                                           | Number           |
| ---------------------------------------------------- | ---------------- |
| `0`–`9`                                              | `57399`–`57408`  |
| `.`, `/`, `*`, `-`, `+`, Enter, `=`, `,`             | `57409`–`57416`  |
| Left, Right, Up, Down, Page Up, Page Down, Home, End | `57417`–`57424`  |
| Insert, Delete                                       | `57425`, `57426` |

Bit 8 reports the modifier and lock keys as `CSI number ; m u`, by side: left Shift, Control, Alt, Super, Hyper and Meta
are `57441` to `57446`, the right ones `57447` to `57452`, AltGr (ISO level 3 shift) `57453`, ISO level 5 shift `57454`,
and Caps Lock, Scroll Lock and Num Lock `57358` to `57360`. A modifier key's press sets its own bit, and its release
reports the modifiers still held, so releasing one of two held Control keys keeps ctrl set and releasing the other
clears it. A lock key's press sets no lock bit.

### xterm `modifyOtherKeys`

`CSI > 4 ; Pv m` selects the xterm `modifyOtherKeys` level: `Pv=0` off, `1` level 1, `2` and anything above it level 2.
A bare `CSI > m` turns it off, and the other resource modifiers (`Pp` other than `4`) are no-ops, since felis emits the
modern forms whatever they say.

A key the level escapes is sent as `CSI keycode ; 1+modifiers u`, the Kitty key event without its optional parts:

- Level 1 escapes a key producing a single ASCII character when Super is held, or Ctrl with a character other than an
  ASCII letter, `@ [ \ ] ^ _` and Space, so Ctrl+2 and Ctrl+/ are escaped while Ctrl+C still sends `0x03`. Escape aside,
  a key held with Alt, and Enter, Tab, Backspace and Space under any modifier, keep their [legacy](#keyboard-encoding)
  encoding, which sends their combinations with Super as `CSI u` all the same.
- Level 2 escapes every key held with Ctrl, Alt, or Super, and Shift+Enter, Shift+Backspace and Shift+Space besides;
  Shift+Tab stays `CSI Z`.
- Escape held with Ctrl, Alt, or Super reports keycode 27 at either level; a bare key is never escaped, and arrows,
  function keys and the editing cluster keep their legacy `CSI` / `SS3` forms. Releases send nothing: the level has no
  event field to distinguish one with. An auto-repeat sends the key's press encoding, for the same reason.

Any active Kitty keyboard flag supersedes the level: the flag stack is read first, and `modifyOtherKeys` only while that
stack resolves to no flags, so a program that pushes flags gets [Kitty key events](#kitty-key-events) whatever level it
set earlier.

## Windows win32-input-mode

On Windows, ConPTY asks every terminal for **win32-input-mode** on startup, and PSReadLine re-requests it, via the
private mode `CSI ? 9001 h` (`l` to disable). While it is active felis encodes each key as a win32-input record instead
of the Kitty / legacy forms:

```
CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _
```

| Field | Meaning                     | Source                                                                                                                                    |
| ----- | --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `Vk`  | Windows virtual-key code    | named key → fixed `VK_*`, none for modifier, lock and media keys; single ASCII alphanumeric → its `VK_` (= uppercase byte); otherwise `0` |
| `Sc`  | scan code                   | always `0`: a layout scan code needs the physical key, and ConPTY / PSReadLine key off `Vk` + `Uc`                                        |
| `Uc`  | UTF-16 code unit            | the OS-composed text, or the control character for Enter / Tab / Backspace / Escape / Space                                               |
| `Kd`  | key down (`1`) / up (`0`)   | both edges are reported                                                                                                                   |
| `Cs`  | `dwControlKeyState` bitmask | Shift `0x10`, Ctrl `LEFT_CTRL 0x08`, Alt `LEFT_ALT 0x02`                                                                                  |
| `Rc`  | repeat count                | always `1` (each auto-repeat is a separate event)                                                                                         |

A key with neither a `Vk` nor a `Uc` (a modifier, lock or media key, or a dead key) emits nothing. The mode is a
per-session flag driven by the running program, and the daemon encodes keys against it like the other keyboard modes, so
a client on any OS attached to a Windows daemon sends win32-input records. The `Sc` / `ENHANCED_KEY` limitation is
argued in [input.md](../../explanation/input.md).

## Modifier convention

| Modifier | macOS   | Linux / Windows |
| -------- | ------- | --------------- |
| Control  | Control | Control         |
| Alt      | Option  | Alt             |
| Super    | Command | Super / Win     |
| Shift    | Shift   | Shift           |

The "macOS Option" key reports both as Alt (for input encoding) and as the macOS-native Option (for native shortcuts),
so Option-arrow word motions still work in shells while Cmd-W still closes the window.

When Option composes a character, which is the default (`window.macos_option_as_alt = false` in
[config](../config.md#window)), that key press reaches the encoder as the composed character without Alt: German
Option+L is `@`, Option+Space on the US layout is U+00A0. The press's repeats and its release report the same key and
keep Alt off, even when Option is released first; their other modifiers are the ones held at the time. Option still sets
Alt for keys it does not compose: named keys other than a composing Space, and any key pressed with Control or Command.
With `macos_option_as_alt = true`, Option is Alt for every key and the layout's Option characters are not typed.

## Mouse reporting

- Mouse position is reported in cell coordinates under the SGR encoding (`?1006h`); the SGR-pixels encoding (`?1016h`)
  reports pixel resolution instead, the middle of the cell for an event from `felis sessions send --mouse`.
- When the active program has a mouse protocol active (e.g. `?1000h` button events, `?1002h` button-event tracking,
  `?1003h` any-motion), wheel events are encoded as protocol mouse events and forwarded to the daemon. (Without an
  active mouse protocol, wheel input is consumed locally for scrollback; see [input.md](../../explanation/input.md).)
- Buttons 4 and 5 (wheel up / down) and 8 / 9 (back / forward, on mice that report them) are encoded and forwarded.

## Paste framing

- Bracketed paste (`?2004h`) is supported and starts off; `CSI ? 2004 h` turns it on. Pasted text is wrapped in
  `ESC [ 200~ ... ESC [ 201~`, and every C0 control in it except HT, LF and CR is removed first, so the text cannot
  close the bracket itself.
- With bracketed-paste mode off, pasted text is sent unframed. (The confirmation for a paste carrying control bytes is
  client-side; see [input.md](../../explanation/input.md).)

## Limits

- Kitty keyboard flag stack: 32 entries per screen; a push against a full stack evicts the bottom entry.
- A structured key event carries at most 32 UTF-8 bytes of character and 32 of composed text; a frame past either cap is
  refused at admission. Longer text is a paste, not a keystroke.

### Report size

An encoded key report is at most `MAX_KEY_REPORT_BYTES` = 288 bytes, which the daemon reserves against the session input
budget before it encodes, because the running program can flip keyboard modes between admission and the write.

The bound is the widest form the encoder can produce, which is the Kitty CSI u report carrying both alternate keys and
associated text. A keycode is a Unicode scalar or a Kitty functional code, so at most 7 decimal digits; `ESC [`, the
three keycodes and their two `:`, the `;` and two-digit modifier field, the `:` and one-digit event type, the `;`
opening the text field, and the final `u` come to 32 bytes. Each text codepoint costs at most 7 digits plus one `:`, and
a codepoint occupies at least one UTF-8 byte, so the 32-byte text cap admits at most 32 of them: 32 + 8 x 32.
