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

- xterm-style for arrow / function keys,
- modifier-encoded with CSI `;n` parameters,
- backspace as DEL (`0x7F`).

The mode is driven by the running program, not by config. felis mirrors the flags on a per-session stack, 32 entries
deep; a push against a full stack evicts the bottom entry. The active flag set is the top of the stack, and the empty
stack of a fresh session resolves to no flags, which is what makes legacy the starting mode.

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
`CSI keycode[:alternate] ; 1+modifiers[:event] [; text] u`:

- `keycode`: the legacy byte of the named keys that have one (Enter `13`, Tab `9`, Backspace `127`, Escape `27`, Space
  `32`) or, for a key that produces a single ASCII character, that character's code lowercased. Arrows, function keys,
  and the editing cluster keep their legacy `CSI` / `SS3` forms with the same `1+modifiers` parameter; felis emits no
  Private Use Area functional-key codes.
- `alternate`: the shifted ASCII letter, present only for presses while bit 4 (alternate keys) is active.
- `modifiers`: shift `1`, alt `2`, ctrl `4`, super `8`; the hyper, meta, caps-lock, and num-lock bits are never set. The
  parameter and everything after it are omitted when no modifier is held, the event is a press, and no text follows.
- `event`: `2` for an auto-repeat, `3` for a release, omitted for a press. Both are sent only while bit 2 (event types)
  is active; without it a repeat is reported as a press, and a release is not reported at all. Every other encoding
  (legacy, `modifyOtherKeys`, DECKPAM) has no event field, so a repeat is a press there too. win32-input-mode reports
  both edges through `Kd` and always sets `Rc` to `1`, since each repeat arrives as its own event.
- `text`: the codepoints of the OS-composed text, colon-separated, present only for presses while bit 16 (associated
  text) is active.

A key that needs no escaping under bit 1 (an unmodified letter, or a modified key with a distinct legacy byte such as
Ctrl+C) keeps its legacy bytes; bit 8 escapes every key. Escape is escaped under bit 1 even unmodified. Shift+Enter,
Shift+Backspace, and Shift+Space are escaped under bit 1 although shift alone would not require it, matching Kitty;
Shift+Tab stays `CSI Z`.

### xterm `modifyOtherKeys`

`CSI > 4 ; Pv m` selects the xterm `modifyOtherKeys` level: `Pv=0` off, `1` level 1, `2` and anything above it level 2.
A bare `CSI > m` turns it off, and the other resource modifiers (`Pp` other than `4`) are no-ops, since felis emits the
modern forms whatever they say.

A key the level escapes is sent as `CSI keycode ; 1+modifiers u`, the Kitty key event without its optional parts:

- Level 1 escapes the combinations that have no legacy byte of their own, Ctrl with a digit or punctuation and Super
  with anything: Ctrl+C still sends `0x03`, and Alt still sends its `ESC`-prefixed byte.
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

| Field | Meaning                     | Source                                                                                             |
| ----- | --------------------------- | -------------------------------------------------------------------------------------------------- |
| `Vk`  | Windows virtual-key code    | named key → fixed `VK_*`; single ASCII alphanumeric → its `VK_` (= uppercase byte); otherwise `0`  |
| `Sc`  | scan code                   | always `0`: a layout scan code needs the physical key, and ConPTY / PSReadLine key off `Vk` + `Uc` |
| `Uc`  | UTF-16 code unit            | the OS-composed text, or the control character for Enter / Tab / Backspace / Escape / Space        |
| `Kd`  | key down (`1`) / up (`0`)   | both edges are reported                                                                            |
| `Cs`  | `dwControlKeyState` bitmask | Shift `0x10`, Ctrl `LEFT_CTRL 0x08`, Alt `LEFT_ALT 0x02`                                           |
| `Rc`  | repeat count                | always `1` (each auto-repeat is a separate event)                                                  |

A key with neither a `Vk` nor a `Uc` (a bare modifier or dead key) emits nothing. The mode is a per-session flag driven
by the running program, and the daemon encodes keys against it like the other keyboard modes, so a client on any OS
attached to a Windows daemon sends win32-input records. Design rationale and the `Sc` / `ENHANCED_KEY` limitation are
documented in [input.md](../../explanation/input.md).

## Modifier convention

| Modifier | macOS   | Linux / Windows |
| -------- | ------- | --------------- |
| Control  | Control | Control         |
| Alt      | Option  | Alt             |
| Super    | Command | Super / Win     |
| Shift    | Shift   | Shift           |

The "macOS Option" key reports both as Alt (for input encoding) and as the macOS-native Option (for native shortcuts),
so Option-arrow word motions still work in shells while Cmd-W still closes the window.

## Mouse reporting

- Mouse position is reported in cell coordinates under the SGR encoding (`?1006h`); the SGR-pixels encoding (`?1016h`)
  reports pixel resolution instead.
- When the active program has a mouse protocol active (e.g. `?1000h` button events, `?1002h` button-event tracking,
  `?1003h` any-motion), wheel events are encoded as protocol mouse events and forwarded to the daemon. (Without an
  active mouse protocol, wheel input is consumed locally for scrollback; see [input.md](../../explanation/input.md).)
- Buttons 4 and 5 (wheel up / down) and 8 / 9 (back / forward, on mice that report them) are encoded and forwarded.

## Paste framing

- Bracketed paste (`?2004h`) is supported and starts off; `CSI ? 2004 h` turns it on. Pasted text is wrapped in
  `ESC [ 200~ ... ESC [ 201~`.
- With bracketed-paste mode off, pasted text is sent unframed. (The multi-line paste confirmation policy is client-side;
  see [input.md](../../explanation/input.md).)

## Limits

- Kitty keyboard flag stack: 32 entries; a push against a full stack evicts the bottom entry.
- A structured key event carries at most 32 UTF-8 bytes of character and 32 of composed text; a frame past either cap is
  refused at admission. Longer text is a paste, not a keystroke.

### Report size

An encoded key report is at most `MAX_KEY_REPORT_BYTES` = 280 bytes, which the daemon reserves against the session input
budget before it encodes, because the running program can flip keyboard modes between admission and the write.

The bound is the widest form the encoder can produce, which is the Kitty CSI u report carrying both an alternate keycode
and associated text. A keycode is a Unicode scalar or a Kitty functional code, so at most 7 decimal digits; `ESC [`, the
two keycodes and their `:`, the `;` and two-digit modifier field, the `:` and one-digit event type, the `;` opening the
text field, and the final `u` come to 24 bytes. Each text codepoint costs at most 7 digits plus one `:`, and a codepoint
occupies at least one UTF-8 byte, so the 32-byte text cap admits at most 32 of them: 24 + 8 x 32.
