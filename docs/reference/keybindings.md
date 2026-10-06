---
title: Keybindings and mouse
sidebar:
  order: 3
---

Default chords are layered under `[keymap]` overrides: any chord can be rebound or removed using the `unbind` sentinel.
The chord grammar is in "Chord grammar quick facts" and the value grammar in "Binding kinds" below.

On macOS the `Ctrl+Shift` defaults all remain bound; `Cmd+<key>` and `Cmd+Shift+<key>` are added for the letter and zoom
chords.

## Default chords

| Action                                   | Linux / Windows                                           | macOS                                                                             |
| ---------------------------------------- | --------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Copy selection to clipboard              | `Ctrl+Shift+C`                                            | `Cmd+C`                                                                           |
| Paste from clipboard                     | `Ctrl+Shift+V`                                            | `Cmd+V`                                                                           |
| Paste from PRIMARY selection             | `Shift+Insert`                                            | `Shift+Insert` (no PRIMARY on macOS; falls back to the system clipboard)          |
| Open scrollback search                   | `Ctrl+Shift+F`                                            | `Cmd+F`                                                                           |
| Reload config                            | `Ctrl+Shift+R`                                            | `Cmd+R`                                                                           |
| Font size up                             | `Ctrl+Shift+=` / `Ctrl+Shift++`                           | `Cmd+=` / `Cmd++`                                                                 |
| Font size down                           | `Ctrl+Shift+-` / `Ctrl+Shift+_`                           | `Cmd+-` / `Cmd+_`                                                                 |
| Font size reset                          | `Ctrl+Shift+0` / `Ctrl+Shift+)`                           | `Cmd+0` / `Cmd+)`                                                                 |
| Scroll half a page up / down             | `shift+page_up` / `shift+page_down`                       | same                                                                              |
| Jump to scrollback top / bottom          | `Ctrl+Home` / `Ctrl+End` (also bare `End` while browsing) | same                                                                              |
| Jump to previous / next prompt (OSC 133) | `Ctrl+Shift+Z` / `Ctrl+Shift+X`                           | same (`Cmd+Shift+Z` is system redo, so prompt jump keeps `Ctrl+Shift` everywhere) |

Both spellings of the zoom symbols are bound because keyboard layouts disagree on whether Shift produces the shifted
character (US `Shift+=` → `+`; DE has a separate `+` keycap).

The canonical default table is defined in `crates/felis-client-core/src/keymap/default.rs`. The defaults lean on
[Kitty's](https://sw.kovidgoyal.net/kitty/conf/#keyboard-shortcuts) where bindings map onto felis's closed action set;
see [input.md](../explanation/input.md) for alignment rationale.

## Chord grammar quick facts

- Each `[keymap]` entry replaces the default binding for its chord, or removes it when the value is `unbind`; chords the
  section does not name keep their defaults. A `[client.<name>]` overlay's `[keymap]` merges key by key over the base
  section ([config.md](config.md) § "Per-client overrides"). Two spellings of one chord in the merged section
  (`ctrl+shift+c`, `shift+ctrl+c`) are applied in the byte order of the strings, so the later-sorting spelling wins.
- Chord strings are case-insensitive for modifier and named-key tokens (`ctrl`, `shift`, `alt`, `super`, `enter`,
  `page_up`, `f5`, …). Each token has exactly one spelling: `super` is the Command key on macOS and Super/Win elsewhere.
  Variant spellings used by other terminals (`control`, `cmd`, `win`, `esc`) are rejected.
- An ASCII letter key matches in lowercase, with Shift as a separate modifier: `ctrl+shift+c` fires on Ctrl+Shift+C. An
  uppercase ASCII letter in a chord string is read as its lowercase letter plus Shift, so `A`, `shift+A` and `shift+a`
  are three spellings of one chord. Any other character key matches the character the layout produces, case included, so
  non-Latin layouts where Shift does not change case still bind.
- A modifier repeated in one chord (e.g. `ctrl+ctrl+a`) is rejected as a config error
  (`crates/felis-client-core/src/keymap/chord.rs`).
- The literal `+` key is whatever follows the final separator, so it is spelled `ctrl+shift++` (bare: `+`). A chord that
  ends in a single `+` with no key after it (`ctrl+`) is a config error, as is one that starts with a separator (`+a`).
- Chords are single strokes: there are no multi-stroke sequences and no leader or prefix key (see
  [input.md](../explanation/input.md)).
- The `kind = "..."` tag admits only the binding kinds tabulated below. There is no free-form action.

## Binding kinds

Every `[keymap]` value is a table tagged by `kind`. The set is closed: an unrecognized `kind`, an unknown field inside a
binding, a value outside the vocabulary below, or a missing required field drops that entry with a warning
([config.md](config.md) § "Behavior on missing / malformed values").

| `kind`                   | Fields    | Accepted values                                                                       | Default           |
| ------------------------ | --------- | ------------------------------------------------------------------------------------- | ----------------- |
| `unbind`                 | —         | —                                                                                     | —                 |
| `send_string`            | `text`    | Any string; backslashes read per `escapes`                                            | Required          |
|                          | `escapes` | `c_style`, `none`                                                                     | `c_style`         |
| `paste`                  | `from`    | `system`, `primary`                                                                   | Required          |
| `copy`                   | `what`    | `system`, `primary`                                                                   | Required          |
| `reload`                 | —         | —                                                                                     | —                 |
| `detach`                 | —         | —                                                                                     | —                 |
| `toggle_fullscreen`      | —         | —                                                                                     | —                 |
| `font_size`              | `step`    | `increase`, `decrease`, `reset`                                                       | Required          |
| `scroll`                 | `step`    | `line_up`, `line_down`, `half_page_up`, `half_page_down`, `home`, `end`               | Required          |
| `scroll_to_prompt`       | `to`      | `previous`, `next`                                                                    | Required          |
| `open_scrollback_search` | —         | —                                                                                     | —                 |
| `switch_session`         | `to`      | `previous`, `next`                                                                    | Required          |
| `new_session`            | —         | —                                                                                     | —                 |
| `kill_session`           | —         | —                                                                                     | —                 |
| `pipe`                   | `source`  | `scrollback`, `visible`, `selection`, `command_output`, `last_command`                | Required          |
|                          | `target`  | `"clipboard"`, `"temp_file"`, `"paste"`, `{ command = [...] }`, `{ file = "<path>" }` | The default pager |
|                          | `ansi`    | `true`, `false`                                                                       | `false`           |
| `run`                    | `command` | Argv array of strings, non-empty; never a shell string                                | Required          |

`escapes = "c_style"` recognizes `\n`, `\r`, `\t`, `\\`, `\0`, `\e`, and `\xNN` (exactly two hex digits); any other
backslash sequence, and a trailing backslash, drop the binding with a warning. `escapes = "none"` sends `text`
byte-for-byte.

`scroll_to_prompt` needs `OSC 133` prompt marks ([Mark shell prompts](../how-to/mark-shell-prompts.md)) and is a no-op
while an alternate-screen application holds the screen.

`ansi = true` retains ANSI SGR styling for tools that render escapes (`less -R`, `bat`, `fzf --ansi`); the default plain
text is what parsers and hint pickers want.

## Unbound by default

Session and utility actions ship without default chords. Bind them in the `[keymap]` section of `config.toml`:

```toml
[keymap]
"ctrl+shift+]" = { kind = "switch_session", to = "next" }
"ctrl+shift+[" = { kind = "switch_session", to = "previous" }
"ctrl+shift+n" = { kind = "new_session" }
```

- **`switch_session`**: Steps the window through active sessions in creation order. Sessions whose shells have exited
  are skipped. Times out after 5 seconds if the listing query receives no response. In `--host` (SSH) windows, each
  switch reconnects over the SSH carrier (see [attach-over-ssh.md](../how-to/attach-over-ssh.md)).
- **`new_session`**: Creates a new session on the current daemon and attaches the window to it.
- **`kill_session`**: Destroys the attached session (mirrors `felis sessions kill`) after the confirmation bar below.
- **`detach`**: Detaches and closes the window; the session continues running in the background (identical to the window
  close button).
- **`toggle_fullscreen`**: Toggles native window fullscreen state.
- **`run`**: Launches a command in a transient session over the live grid (e.g. an interactive session picker).
- **`pipe`**: Extracts buffer text and sends it to a configured sink:

```toml
[keymap]
# Scrollback to a color pager (kitty's show_scrollback chord).
"ctrl+shift+h" = { kind = "pipe", source = "scrollback", ansi = true }
# Visible region to a hint picker.
"ctrl+shift+u" = { kind = "pipe", source = "visible", target = { command = ["urlscan"] } }
# Selection to the system clipboard.
"ctrl+shift+y" = { kind = "pipe", source = "selection", target = "clipboard" }
# Scrollback to a client-chosen temporary file.
"ctrl+shift+s" = { kind = "pipe", source = "scrollback", target = "temp_file" }
```

What each sink does with the captured region:

- Omitted `target` (and `{ command = [] }`): opens the default pager, which is `$PAGER` split on whitespace with no
  flags injected, else `less -R +N`.
- `{ command = [...] }`: writes the captured region to a temporary file and spawns the argv with that file's path
  appended as its last argument.
- `"clipboard"`: copies the captured region to the system clipboard.
- `{ file = "<path>" }`: writes to `<path>`, resolved like every other file-valued key: a relative path against the
  directory of the `config.toml` that names it, a leading `~/` against the home directory, an absolute path as written.
- `"temp_file"`: writes to a client-managed temporary file whose path is logged.
- `"paste"`: sends the captured region back as input to the session.

The `source` regions are the same ones `felis sessions capture` reads; see the
[scrollback capture guide](../how-to/search-and-capture-scrollback.md).

Command argvs (`pipe` and `run`) execute locally on the window host, resolved against its `PATH` and filesystem. The
exception is `paste`, which delivers input directly back into the originating session.

Commands run with origin environment variables:

| Variable                  | Value                                                                       |
| ------------------------- | --------------------------------------------------------------------------- |
| `FELIS_ORIGIN_SESSION_ID` | Session ID on which the chord was triggered                                 |
| `FELIS_HOST`              | Window SSH destination; unset for local windows                             |
| `FELIS_CWD`               | Origin session `OSC 7` working directory URI (`file://host/path`), verbatim |

### Viewport anchor variables

Command sinks fed `scrollback` or `visible` export viewport line anchors to the environment:

| Variable                  | Value                                         |
| ------------------------- | --------------------------------------------- |
| `FELIS_INPUT_LINE_NUMBER` | 1-based logical line at the top of the window |
| `FELIS_CURSOR_LINE`       | 1-based logical line containing the cursor    |
| `FELIS_CURSOR_COLUMN`     | 1-based rendered column of the cursor         |

Line numbers count stitched logical lines (soft-wrapped rows combined). Columns count rendered characters, unaffected by
`ansi = true`. These variables are unset for unanchored regions (`command_output`, `last_command`, `selection`), for
`run`, and for a region the daemon trimmed to fit `MAX_REGION_REPLY_BYTES` ([ipc.md](ipc.md#semantic-limits) "Semantic
limits").

Argv elements are passed verbatim without placeholder expansion.

Design rationale for unbound defaults is discussed in [input.md](../explanation/input.md) § "Keybinding design".

## Confirmation bar

Two actions park themselves behind a one-line question on the window's bottom edge:

| Trigger                                                                        | Question                                                                 |
| ------------------------------------------------------------------------------ | ------------------------------------------------------------------------ |
| The `kill_session` binding                                                     | `Kill this session? The program running in it will be terminated. [y/N]` |
| Pasting text containing `\n` or `\r` while the program has not enabled `?2004` | `Paste N lines? The program did not request bracketed paste. [y/N]`      |

While the bar is open it owns the keyboard, chords included: `y` or `Y` runs the parked action, every other key cancels
(Enter included), and bare modifier presses are ignored. One question stands at a time, and a session switch clears a
pending one. The rationale is in [input.md](../explanation/input.md) § "Confirmation bar".

## IME

The client allows IME on every window and drives it through winit 0.30, so the backend is the platform's own:

| Platform      | IME backend                                 |
| ------------- | ------------------------------------------- |
| macOS         | `NSTextInputClient` (marked text)           |
| Linux Wayland | `zwp_text_input_v3`                         |
| Linux X11     | XIM                                         |
| Windows       | IMM32 (`Imm*`), when `SM_IMMENABLED` is set |

A composition renders as an underline-styled overlay on the active row, taking the cells the committed text will take,
and shifts the cursor to its extent. A wide character that reaches past the row's right edge is cut at the edge. Only
the commit reaches the daemon, as raw UTF-8 bytes rather than key events, so no keyboard-mode encoding applies; pre-edit
text never crosses the wire, so the daemon's cursor stays put while a candidate is chosen.

## Mouse

| Gesture                      | Action             | Description                                                                                                                                                           |
| ---------------------------- | ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Left drag                    | Select text        | Hold `Shift` to extend. Double-click selects a word; triple-click selects a logical line (soft wraps stitched).                                                       |
| Alt + Left drag / Right drag | Block selection    | Selects a rectangular block of cells.                                                                                                                                 |
| Plain Left click             | Dismiss selection  | Clears selection. Forwards to application if mouse protocol is active; hold `Shift` to force local selection.                                                         |
| Middle click                 | Paste PRIMARY      | Pastes PRIMARY buffer on Linux / X11 / Wayland; does not clear current selection.                                                                                     |
| Wheel                        | Scroll             | Scrolls scrollback on primary screen; translates to Up/Down on alternate screen; forwards raw events if mouse protocol active. Scaled by `[mouse] scroll_multiplier`. |
| Shift + Wheel                | Scroll half a page | One tick per half page. Overrides an active mouse protocol on the primary screen; not scaled by `scroll_multiplier`.                                                  |
| Ctrl + Wheel                 | Font zoom          | Zooms font within 4–72 px band; mode latched at start of wheel gesture.                                                                                               |
| Ctrl + Left click            | Open hyperlink     | Activates hovered OSC 8 hyperlink previewed in bottom bar.                                                                                                            |

Mouse behavior specifications:

- **Selection lifetime**: Selections are cleared upon plain click, or when the underlying cells are replaced (e.g.
  scrolling into scrollback or switching between primary and alternate screens).
- **PRIMARY selection**: Completing a selection auto-copies to PRIMARY on Linux (the X11 `PRIMARY` selection, or
  `zwp_primary_selection_v1` on Wayland); a failed write is logged and never interrupts the gesture. On macOS, PRIMARY
  is unsupported and `Shift+Insert` falls back to the system clipboard. A `paste` binding with `from = "system"` reads
  the X11 `CLIPBOARD` selection and one with `from = "primary"` reads `PRIMARY`; an empty `CLIPBOARD` never falls back
  to `PRIMARY`. The same split applies on Wayland through the protocol's `Primary` selection role.
- **Hyperlink validation**: Hyperlinks previewed in the bottom bar and activated via `Ctrl+Click` are re-validated
  before execution (see [security-model.md](../explanation/security-model.md)).
- **Input architecture**: Modifier mapping and event encoding are specified in
  [key-encoding.md](protocols/key-encoding.md); the design rationale is [input.md](../explanation/input.md).
