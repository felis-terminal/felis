---
title: Input
sidebar:
  order: 8
---

The client owns input capture. Keyboard, mouse, paste, IME, and focus events originate at the OS, are translated by
winit, and ship to the daemon as protocol messages. Turning a keystroke or a mouse event into PTY bytes is the daemon's,
because that encoding reads terminal modes the daemon owns. This document covers capture, the policies that live on the
client side, and where the boundary between them falls. The byte-level PTY encodings (Kitty keyboard sequences, the
legacy xterm forms, mouse reporting) are specified in [key-encoding](../reference/protocols/key-encoding.md); this page
owns the _why_ behind them.

## Layers

```
OS event  (winit)
   ↓
Action mapping  (client config; keymap)
   ↓
IPC message     (Input kind: the key, the mouse event, the paste)
   ↓ (daemon)
Encoded input   (Kitty keyboard / SGR mouse / paste framing)
```

The client's job is _translation_, not interpretation. Whether a key press should "do something" inside the terminal
session is decided by the byte stream the daemon writes to the PTY.

## Keyboard

### Encoding

felis implements the **Kitty keyboard protocol**, which is unambiguous about modifiers, function keys, and alternate /
shift-printed forms. The flag stack starts empty, so a session encodes legacy xterm until a program pushes flags. The
mode is driven by the running program, not by config: a program pushes / pops Kitty keyboard flags at runtime, which
felis tracks on a per-session flag stack.

A keystroke crosses the wire as the facts a keyboard delivers, not as bytes: `InputMsg::Key` carries the logical key,
the OS-composed text, the modifiers, press / repeat / release, and the key location. The daemon encodes it against the
flag stack and mode snapshot it has already parsed, under the same lock a mouse event is encoded under. The encoder is
platform-free and lives in `crates/felis-daemon/src/serve/key_encode.rs`; each front-end only has to name the key (the
GUI client's `crates/felis-client/src/input.rs` binds `winit`; a browser-hosted front-end would bind `KeyboardEvent`),
so every front-end shares one encoding policy without linking an encoder.

The alternative, the client encoding from modes the daemon mirrors to it, loses a race it cannot win. Mode changes reach
the client as grid frames, and a producer can enable a keyboard mode and then read input in the window before that frame
arrives; a window that has stopped pulling frames, because it is occluded, holds that window open indefinitely. Pushing
mode frames eagerly does not close it, because there is no ordering barrier between the daemon's next frame and the
user's next key. Encoding where the modes are authoritative removes the window rather than narrowing it.

The byte-level forms of both modes and the modifier convention per platform are specified in
[key-encoding](../reference/protocols/key-encoding.md).

Literal bytes keep their own message. `InputMsg::KeyBytes` carries what a caller means byte for byte:
`sessions send --raw`, the `send_string` action, and a committed IME string. A paste has its own arm, because the daemon
frames it under `?2004`, and a file drop's path takes that arm too: the path is inserted text, and the bracketed-paste
guards a program relies on apply to it.

### Windows win32-input-mode

Windows ConPTY does not consume the same VT keyboard input a Unix PTY does. It asks the terminal for
**win32-input-mode** (`CSI ? 9001 h`, emitted by ConPTY on startup and again by PSReadLine) and then expects each key as
a record carrying the raw `INPUT_RECORD` fields (`CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`), which it reconstitutes losslessly
for the child
([microsoft/terminal #4999](https://github.com/microsoft/terminal/blob/main/doc/specs/%234999%20-%20Improved%20keyboard%20handling%20in%20Conpty.md)).
A terminal that ignores the request and keeps sending plain VT leaves PSReadLine (and therefore an interactive
PowerShell, the default Windows shell) unable to receive keystrokes, while `cmd.exe` (which never enables the mode) is
unaffected.

So felis adopts it: the ConPTY is created with `PSEUDOCONSOLE_WIN32_INPUT_MODE`, the daemon parses `?9001` into a
per-session flag, and the encoder emits win32-input records while it is set. The byte-level form is in
[key-encoding](../reference/protocols/key-encoding.md) (REQ-507).

This is the **input half of the ConPTY shim**, not an OS-gated feature: the intent ("keystrokes reach the shell
faithfully") is uniform, and only the ConPTY mechanism forces the Windows-specific encoding. That is the
[non-goals](non-goals.md) cross-platform carve-out already covering the ConPTY-vs-`/dev/ptmx` PTY shim. The encoder
stays platform-free (not `cfg(windows)`) on purpose: a Windows daemon reached over SSH stdio from a Linux or macOS
client encodes win32-input records for a key that client captured, so the logical-key → Windows-`VK` mapping is shared
data, activated only by the runtime flag that no non-ConPTY producer ever sets.

Two fidelity shortcuts are deliberate. The scan code is sent as `0`: felis encodes from the _logical_ key, and a layout
scan code cannot be derived without the physical key position; ConPTY and PSReadLine key off `Vk` + `Uc`, so `0` costs
nothing for the target workflow. And the `ENHANCED_KEY` control-state bit is never set.

_Revisit if_ a target program is found to depend on the scan code or the enhanced-key bit (e.g. a console app
distinguishing the numpad Enter from the main Enter); recovering either means threading the client's physical `KeyCode`
through the encoder.

### Action mapping

The client config maps key combinations to a closed set of `Action` values. Variants take typed parameters but do not
host expressions. The config spellings (each kind's token, its fields, its accepted values, and the default chords) are
owned by [keybindings](../reference/keybindings.md) § "Binding kinds" and § "Chord grammar quick facts"; this section
carries the arguments behind that shape.

Actions outside this list do not exist: the action enum can grow, but the _mechanism_ (key → typed action) cannot.
`SendString` and `Ipc` carry data, not code: felis runs no logic on their parameters beyond decoding the named escape
set or routing the named IPC command. See [principles](principles.md) principle 1 and [non-goals](non-goals.md)
"Scripting and extensibility".

Keybind actions are one of felis's five control surfaces: the interactive, in-window one. For where a capability belongs
(keybind vs CLI verb vs config vs env) and the cross-plane name map, see
[control-surfaces](architecture/control-surfaces.md).

Forwarding a keystroke is not an action. A key no binding matches is encoded in the active keyboard mode and sent to the
PTY: the dispatcher declining to consume the keypress _is_ that path, so it needs no variant of its own and no token a
config could spell.

`detach` is the _only_ way a client closes itself: the window-close button runs the same
`SessionToDaemonMsg::Detach`-then-exit path, so "quit the window" and "detach from the session" are one operation with
one name, not two spellings that would have to promise the same thing. The same close happens when a
`felis sessions evict` disconnects this client ([control-surfaces](architecture/control-surfaces.md)).

The `scroll` page steps move half a screen and are named for kitty's `scroll_half_page`: a whole-screen jump leaves no
overlapping row to reorient against.

#### A direction is a parameter, not a family of tokens

`font_size`, `scroll_to_prompt`, and `switch_session` each take the direction as a typed value (`step`, `to`). One token
per action keeps the list the size of the action set.

Spelling the directions into the token names instead (`increase_font_size`, `decrease_font_size`, …) grows the token
list with the _product_ of action and direction, and reads as a set of unrelated actions to anyone scanning it, while
the runtime `Action` carries a direction parameter either way.

`scroll_to_prompt` carries nothing but the direction for a second reason: the daemon resolves the target offset from its
own `OSC 133` marks ([scrollback](data-model/scrollback.md)).

#### `send_string` decodes a closed escape set

`send_string`'s escape set is closed and decoded by felis rather than handed to a general unescaper. C-style escapes are
the default because a binding that sends a control sequence is the reason to reach for `send_string` at all; the
byte-for-byte mode exists for the binding whose payload _is_ a backslash. An unrecognized backslash sequence drops the
binding instead of reaching the PTY as its own spelling, so a typo fails where the user can see it.

Beyond that decoding there is no shell expansion, no command substitution, and no nested binding. Bracketed-paste
wrapping is not applied either: the bytes look like a typed sequence to the running program, and a multi-line
`send_string` does not arm the paste confirmation bar, whose threat model is hostile output rather than the user's own
keymap.

#### `Ipc` dispatches to a closed set of daemon operations

`Ipc(IpcAction)` is a dispatcher to a closed set of named daemon operations, not a passthrough for arbitrary IPC
messages. The set grows by adding enum variants, not by accepting strings.

Two of those variants overlap a CLI verb without duplicating it: `new_session` creates a session and switches to it
in-window, distinct from the CLI's _detached_ `felis sessions spawn`, and `kill_session` matches `felis sessions kill`
behind the confirmation bar ([control-surfaces](architecture/control-surfaces.md)). The two session-creating paths ride
different messages, because they differ in what the caller gets back: `new_session` rides
`SessionToDaemonMsg::Create { args }`, which attaches the connection to the new session, while the detached CLI spawn
rides `OpsToDaemonMsg::Spawn { args }`, which returns the session's identity and leaves it unattached.

#### `pipe` ships a region to an external sink

The **`pipe`** token ships a region of the session buffer to an external sink, modeled on foot's `pipe-*` action family
(<https://man.archlinux.org/man/foot.ini.5.en>); the sink set is modeled on Ghostty's `write_scrollback_file` `:open` /
`:copy` / `:paste` variants (<https://ghostty.org/docs/config/keybind/reference>) rather than a pager special-case. The
region is the subject; `source` is always a real region (launching a command with _no_ region is the separate `run`
action below). Three arguments shape the fields.

Each sink carries its own payload, so an argv handed to the clipboard sink or a path handed to the command sink has no
spelling at all: the config grammar makes the combination unrepresentable instead of accepting it and reporting it. That
is why the payloads are not sibling fields of the `pipe` table: siblings deserialize cleanly whatever they are paired
with, which buys a validator, a warning class, and a documented failure row for a mistake the type can simply not have.
`temp_file` is its own token for the same reason: "the file sink with no path" would otherwise need either a second
spelling of `file` or an unwritable-in-TOML null. A command sink is an argv vector, never a shell string: no `sh -c`
eval lives in the action surface (principle 1); a user who wants a pipeline points the binding at their own script.

The `selection` source is client-owned (the daemon grid has no selection), so the **client** serializes it, via
`felis_grid::Selection::extract_text` over the same `logical_line_spans` soft-wrap rule the daemon encoders use
([scrollback](data-model/scrollback.md) "Selection source: client-side serialization"), and feeds those bytes straight
to the sink instead of asking for a region (`RegionToDaemonMsg::Request`): the selection is presentation the client owns
(principle 3), so the client is the only side that can read it. An empty source (no selection, no OSC 133 marks) is a
no-op.

`ansi` defaults off because the tools a region is usually handed to (hint pickers, editors, text utilities) re-parse it
as data, where embedded escapes corrupt the match. Color is the exception a color-rendering sink asks for.

A `command` target spawns as a transient session the client switches into and back from on exit, while the other targets
complete in place with no view switch. The encoders, the soft-wrap stitching and the ceiling on a reply's size live in
[scrollback](data-model/scrollback.md) "Piping to an external command"; the carrier and the transient's lifecycle are
below. Pipe is keymap-only: interactive, attached-window, and it _pushes_ a region to a sink. The headless _pull_
analogue (reading a typed region to a script's stdout) is `sessions capture`, whose `--source` selection mirrors the
same semantic sources ([control-surfaces](architecture/control-surfaces.md)).

#### `run` launches a command with no region

The **`run`** token launches an argv in a transient session sized to the live grid, fed no region. The command is the
subject. It shares `pipe`'s transient-session spawn (the client switches in and back on exit) but carries no
`source`/`target` (a separate, simpler wire message), so a `run` binding can never spell a region/sink combination.

`command` is required and non-empty: unlike `pipe`, `run` has no default command to fall back to, so an empty argv is a
binding that eats the chord and launches nothing; the loader drops it with a warning rather than installing it.

felis does not read the command's output, evaluate it, or hold a callback: the launched tool drives felis back over the
public IPC/CLI surface (`felis sessions …`). It therefore introduces no in-process evaluator: it adds a typed action
variant, which [principles](principles.md) principle 1 expressly permits.

One chord bound to `run` can also open a menu that fans a single stateless chord out to many `pipe` / `sessions`
workflows; the worked idiom is "A menu instead of a prefix" below.

#### Why `run` earns a keybind rather than a WM hotkey

`run` is justified against principle 1's "a dedicated tool does it as well or better" test by the felis-unique context
only the focused terminal can supply, either (a) the grid-sized transient session a redraw-style TUI needs to draw over
the live screen (an `fzf` session picker the WM cannot host), or (b) the focused session's environment (its id, its cwd)
that a WM hotkey does not know. The canonical use is a session picker:
`felis sessions list --format json | jq -r '.sessions[].id' | fzf | xargs felis sessions switch`.

The cwd comes from the session's own `OSC 7` report, used only when it names a directory that exists on the _client's_
host, where the transient runs. A report from an `ssh` inside the session (or from a `--host` window's remote shell)
describes somewhere this command cannot go, so the local daemon's cwd applies instead. The report still reaches the
command as `FELIS_CWD`, for a script that knows how to get there ([keybindings.md](../reference/keybindings.md)).

A command needing neither gains nothing here. felis does not sniff or restrict the argv to enforce that (that would be a
principle-4 content heuristic), so an unrelated command is _not recommended but not prevented_.

_Revisit if_ real bindings drift to felis-unrelated background commands a WM hotkey handles equally well: then `run` is
paying for itself in the wrong currency and the keymap token should be cut.

#### Carrier: temp file, not a stdin pipe

The client writes the region to a temp file and spawns `command <file>`: argv with the path appended, all stdio on the
transient PTY. A pipe consumer runs interactively, in a transient session with its own PTY, so the portable carrier is
the temp-file path: mandatory on Windows, merely convenient on Unix. It also suits the usual consumer, since a pager or
editor takes a path argument and wants a named file rather than a stream.

A stdin pipe is rejected. Handing the capture to the child on stdin is conventional on Unix and cheap to wire, but
Windows has no equivalent seam: a ConPTY child takes all three std handles from its pseudoconsole, so "interactive on
the console plus a private stdin" cannot be expressed.

Stdin still has a place: a _non-interactive_ filter (a plain child with no pseudoconsole) can read the region from its
own stdin even under ConPTY.

The file is written under a per-pid directory in the client's system temp dir and unlinked when the transient that was
reading it exits, the last unlink taking the staging directory with it, so a clean run leaves nothing behind. The client
owns it because the client wrote it: the file lives on the machine the command runs on, and no other process knows when
the reader is done with it.

The unlink is a drop, not a call: the handoff state that tracks the transient holds the file as a guard, so the paths
that end a handoff without one ever coming up (a switch that fails to land, a daemon socket that will not resolve)
reclaim the file by the same route the ordinary exit does. Hand-written unlinks are rejected for the opposite reason:
the failure paths outnumber the success path, and each is a place to forget one.

A client-startup sweep reclaims what a crash orphaned, which is the only other way those files would go away. It sweeps
by pid liveness, not by its own name: a starting run's own directory does not exist yet, so every orphan is named after
a different, dead pid. A directory whose pid still runs is spared (another window is reading the regions it staged), and
so is one the platform will not let felis test, which on a shared `/tmp` is another user's client.

_Revisit if_ felis grows such a filter mode; the interactive consumer it spawns stays on the temp file.

#### Which daemon runs the command

A daemon spawns, not the client: the client is a wgpu window, not a PTY host, so a daemon holds the transient's PTY
whichever host runs the command. The question is _which_ daemon, and the answer is the client's own, never the attached
one. Three arguments settle it:

- The keymap is client config, and the models for this action surface (foot's `pipe-*` family, Ghostty's
  `write_scrollback_file`) run the command where the config lives, so a user reads a binding's argv as naming a program
  on _their_ machine. Executing on the attached daemon would make the same config line mean different programs depending
  on where the window happens to be attached, and its bare-name-on-`PATH` contract fights configs that pin absolute
  paths (a Nix store path is valid only on the host whose closure produced it).
- Reachability is asymmetric. A client-local command can always reach the attached host: the ssh access that carried the
  attach, and the IPC surface via `felis --host`. A command on the attached daemon has no channel back to the client's
  machine beyond the clipboard and notification relays. The default belongs on the side that has an escape hatch.
- The sink split comes out uniform: every sink runs client-side, and `Paste` re-enters the session through the same
  `InputMsg::Paste` a `Ctrl+V` uses. No sink is an exception to anything.

So the daemon's part ends at the read. It answers with `RegionToClientMsg::Reply` (the bytes plus the region's viewport
anchor, which only the side that stitched the region can compute) and the client resolves the argv, writes the file,
loads the clipboard, or pastes. `run` never asks: with no region to read it goes straight to
`SessionToDaemonMsg::Create` on the local daemon.

The transient is seeded to the live grid's rows × cols at spawn; the client's resize-on-attach still refines the
geometry to the real window, but seeding it up front avoids a first-frame reflow, and the live-grid size is what lets a
redraw-style tool line its overlay up cell-for-cell. The other targets complete in place, with no transient session and
no view switch, so only the genuinely interactive case carries the spawn → attach → restore weight.

Remote context stays reachable, but explicitly. The spawned command's environment carries the originating session's id,
the window's ssh destination and the session's verbatim `OSC 7` report ([keybindings.md](../reference/keybindings.md)
names the variables), so a user script can re-enter the remote through the CLI. The cwd report travels unresolved, as
the `file://host/path` URL the shell emitted, because it names a path on the origin's host, which is the one machine
that can judge it; the script strips it to a path and `--cwd` carries that typed to the daemon that will run it.

That the same report is _rejected_ as the transient's own cwd when it does not name a directory there is a separate
decision: what the child is told and where the child starts answer different questions.

The ssh destination travels alone, not with the `--ssh-arg` splat a window may have been dialed with: the environment is
one variable per fact, and a shell-quoted token list in a second variable would hand every consumer a re-splitting
problem felis cannot solve for it.

Three alternatives are rejected or moot:

- **A per-binding `exec = "local" | "remote"` field.** Either default surprises half the bindings, and the remote case
  is already expressible through the IPC surface.
- **A general local↔remote bridge runtime.** `OSC 52`, the notification relay, and `--host` cover the real flows without
  a new extension surface.
- **A daemon-side name-to-argv mapping** is moot for the same reason: the executing host is the one whose config named
  the argv.

_Revisit if_ the carrier round trip on every transient exit (each return from a remote-attached window re-dials the
remote; OpenSSH ControlMaster amortizes it) proves unusable on high-latency links.

#### Opening the pager where the user was looking

The region a `Command` sink hands over is the whole buffer, so the pager still has to be told _where_ in it to open.
Without that, a chord pressed mid-scrollback drops the user wherever their pager starts, and the place they were reading
is lost exactly when they asked to read it more closely. So the position ships with the region, in the child's
environment ([keybindings.md](../reference/keybindings.md) "Viewport anchor variables" names the variables). The one
region that reaches the child without a position is one the daemon trimmed to fit its reply ceiling, which drops the
anchor with the head it removed ([scrollback.md](data-model/scrollback.md#a-region-too-large-to-carry) "A region too
large to carry").

The environment carries it, **not** kitty's argv-token substitution (`scrollback_pager less +INPUT_LINE_NUMBER`). A
keymap `command` is a typed argv vector felis passes through verbatim; rewriting a token inside it would make an
ordinary string argument silently mean something felis chose, and a user whose program takes a literal
`INPUT_LINE_NUMBER` argument could not spell it. That is the explicit-over-heuristic line (principle 4), and the
environment reaches every consumer without touching argv at all. A tool that insists on the number _on_ its command line
gets it from a one-line wrapper script, the same place a `pipe` binding already sends anything needing shell syntax.

The built-in default pager is the one exception, and only because it is not the user's argv: when a binding names no
command felis composes `less -R +N` itself, so it knows the program and may spell its start flag. A `$PAGER` the user
set is opaque (felis appends no flag to it, since a wrong guess would leave the pager rejecting its own command line)
and reads the position from the environment like any other configured command.

Coordinates are 1-based logical lines of the emitted region, not grid rows. The region stitches soft-wrapped rows into
single lines ([scrollback.md](data-model/scrollback.md) "Logical lines: soft-wrap stitching"), so a wrapped screen row
is not a line of the file at all and can only be named by the line it was stitched into: a continuation row reports its
logical line's head. Two clamps follow from the same "name something that exists in the file" rule: a row below the
blank tail the serializer trims reports the last line, and the cursor column counts _rendered_ characters, since the SGR
escapes an `ansi` region carries occupy no visual column.

Only the two viewport-anchored sources carry a position. A `command_output` / `last_command` range is anchored to a
command, and a `selection` to a gesture; neither has a "where the user is looking" to report, so the variables are unset
rather than defaulting to the region's head, which lets a consumer tell an unanchored region from a real line 1. `run`
sets nothing for the same reason: no region, no position in one.

### Why pipe, not a copy mode

felis ships no in-window copy / vi mode (WezTerm's Copy Mode, <https://wezterm.org/copymode.html>; Alacritty's Vi mode,
<https://github.com/alacritty/alacritty/blob/master/docs/features.md>). A modal cursor with vim motions, visual
selection, and incremental search is a large persistent surface and a new modal state machine that re-implements `less`
/ vim inside the terminal: squarely the growth principle 1 pushes back on (no capability a dedicated tool does better;
extend via external processes, not an embedded surface). `Pipe` takes the foot / Ghostty hybrid shape instead: the
lightweight native search overlay (REQ-607) answers the in-place "where did that scroll past?" question, and everything
heavier (regex over a build log, vim motions, multi-screen copy, "open in `$EDITOR`") is handed to the user's own pager
/ editor in a separate process. Kitty draws the same line: its scrollback "mode" _is_ `less`
(<https://sw.kovidgoyal.net/kitty/overview/>).

The pipe also subsumes Kitty-style hints without violating principle 4. Kitty's hints kitten is itself "pipe the screen
to an external program that detects matches and returns a pick" (<https://sw.kovidgoyal.net/kitty/kittens/hints/>);
`visible` → `command` (`urlscan`, `fzf`, a user picker) gives the same extract-and-act flow with the URL / path
_heuristic living in the external tool_, never in felis core. For this flow the pipe's default-plain encoding and
soft-wrap stitching matter: the picker sees clean text with a wrapped URL reconstructed whole, not SGR-laced rows split
at the screen edge ([scrollback](data-model/scrollback.md) "Encoding: plain by default, `ansi` for color", "Logical
lines: soft-wrap stitching"). foot ships exactly this as a `pipe-visible` URL example
(<https://man.archlinux.org/man/foot.ini.5.en>).

A felis-rendered hint-label overlay is rejected: felis would have to _detect_ the matches to place the labels, and that
detection is the shell-content heuristic principle 4 forbids. The line is "who detects and draws", not "does it look
in-place": the transient session is sized to the live grid precisely so a redraw-style tool can reproduce the screen
cell-for-cell and overlay its own labels ([scrollback](data-model/scrollback.md)).

The select-then-act case a copy mode owns ("select with the mouse, _then_ send the selection somewhere") is
`source = selection`, which the client serializes and routes itself ([scrollback](data-model/scrollback.md) "Selection
source"), so the pipe-not-copy-mode stance covers select-then-act too.

_Revisit if_ real usage always reaches for the pager and never for the in-place locate-and-act flow: the search overlay
would then have stopped earning its surface (principle 1).

## Keybinding design

### Default keymap

The default keymap leans on Kitty's defaults (<https://sw.kovidgoyal.net/kitty/conf/#keyboard-shortcuts>) where those
bindings map cleanly onto felis's closed action enum. felis does not commit to bit-for-bit parity (Kitty's keymap
exposes actions that have no felis equivalent: multiplexer ops, kitten shell-outs, scripted hooks), but the alignment
lets users coming from Kitty expect familiar bindings for copy / paste, font sizing, and scrollback navigation.

Specific assignments are not pinned in this document: keymaps are config, and config is not a stable interface
(principle 1). The defaults and their per-platform chords are [keybindings](../reference/keybindings.md)'s, under the
rule "match Kitty unless the binding has no felis action."

### Why some actions ship unbound

The session actions (`switch_session` / `new_session`) ship without a chord because which keys feel natural for session
cycling is too personal to guess: any default would be wrong for most hands and would squat a chord the user wanted for
something else. `pipe` goes further: its `source`, `target`, and `ansi` choices are all workflow-personal, so there is
no one binding to default; the manual gives samples instead. `run` follows the same reasoning as the session tokens. The
[keybindings page](../reference/keybindings.md) "Unbound by default" carries the full list and copy-paste bindings.

### A menu instead of a prefix

felis has no leader / prefix key: chords are single strokes
([`crates/felis-client-core/src/keymap/chord.rs`](../../crates/felis-client-core/src/keymap/chord.rs)), and
multiplexer-style keymaps belong to the WM. When a `pipe`-heavy setup would otherwise want one chord per workflow and
run the chord space dry, bind **one** chord to `run` a menu and let the menu hold the fan-out:

```toml
[keymap]
# One chord opens a menu; the menu (an external TUI) picks the workflow.
"ctrl+shift+p" = { kind = "run", command = ["felis-pipe-menu"] }
```

`felis-pipe-menu` is the user's own script. It draws a transient `fzf` / `gum` menu over the live grid. felis paints no
menu itself (principle 1's no-in-terminal-selector line); it only hosts the user's TUI in the transient session, the
same way it hosts `vim`. The script reads the buffer through the headless CLI
(`felis sessions capture "$FELIS_ORIGIN_SESSION_ID"`) and, on a pick, acts back over it (`felis sessions switch`, a hint
picker, an editor). The menu, not felis, holds the second keystroke, and unlike a bare prefix it _shows_ its options.
Hints take the same shape: one chord into a picker, with the URL / path / hash detection living in the picker ("Why
pipe, not a copy mode" above).

## Mouse

The gesture inventory is in [keybindings](../reference/keybindings.md) § "Mouse"; the byte encodings programs see, and
which buttons are forwarded, are in [key-encoding](../reference/protocols/key-encoding.md). What follows is why the
gestures are drawn that way.

- Notch-wheel scrollback scrolling scales by `[mouse] scroll_multiplier`. Linux delivers a wheel notch as
  `LineDelta(0, ±1)` with no OS scroll acceleration (winit Wayland `axis_discrete` / X11 core buttons 4/5), so the bare
  one-notch-one-row mapping crawls; the multiplier restores a usable feel. Touchpad pixel-deltas are already
  velocity-scaled and ignore it, and so does everything that is not a scrollback row: Shift+wheel half-pages, alt-screen
  arrow translation, and mouse-protocol button events stay one-event-per-notch.
- The zoom-vs-scroll choice is _latched_ at the start of each wheel stream (a finger gesture plus the trackpad inertia
  it spawns) and held for the whole stream, so a Ctrl press landing during leftover momentum cannot reclassify an
  in-flight scroll as a zoom. Rationale: macOS delivers inertia as ordinary wheel events and winit 0.30 collapses the
  momentum phase onto the same `TouchPhase` as a live gesture, so the inter-event idle gap is the only per-event signal
  that separates a new gesture from trailing inertia. _Revisit if_ winit exposes a distinct momentum phase: then gate on
  that directly instead of on the idle gap.

### Momentum scrolling is adopted where the OS sends it, synthesized nowhere

Momentum ("inertial") scrolling is adopted where the OS sends it and synthesized nowhere. That gives momentum on macOS
and none on Linux, because the two platforms assign the duty differently.

AppKit continues to deliver scroll events after the fingers lift, tagged through
[`NSEvent.momentumPhase`](https://developer.apple.com/documentation/appkit/nsevent/momentumphase), so the inertia
arrives as ordinary wheel events felis already handles.

Wayland does not: `wl_pointer` carries `axis_source` and `axis_stop` but no momentum phase
([wayland.xml](https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml)), and libinput
deliberately stops at the finger lift: it emits a zero-delta scroll event to mark the end of a gesture _so that_
toolkits can run their own kinetic scrolling
([libinput scrolling docs](https://wayland.freedesktop.org/libinput/doc/latest/scrolling.html)), which is why GTK ships
`GtkKineticScrolling` in the toolkit. So the choice on Linux is not "adopt OS momentum" but "write an animator":
estimate velocity at the stop event, then decay it across frames.

The cost of an animator is a timer that redraws while the user's hands are still, which the client avoids elsewhere:
`cursor.blink` defaults to `program` so an idle window never wakes the CPU for an animation nobody asked for. A decaying
scroll costs more frames than a caret, so it has to be tuned well enough to beat the current one-gesture-one-jump feel
before it is worth those frames.

_Revisit if_ the compositor stack grows a momentum phase (making it adoptable rather than synthesized), or if the
animator is prototyped and measurably beats the current feel on a touchpad.

## Selection

The selection model is client-side pure data
([`crates/felis-client-core/src/selection.rs`](../../crates/felis-client-core/src/selection.rs)): the App owns the
mouse-down / drag / up lifecycle against the shadow screen, the highlight and the copied text are computed locally, and
no selection state crosses the wire.

- The line the triple-click streak selects is the _logical_ one: the clicked row plus the rows its `soft_wrap_continued`
  bits tie to it, clipped to the visible screen. The wrap bit rides the `RowDelta` row codec
  ([row-codec.md](../reference/row-codec.md)) onto the client's shadow screen, so the gesture stitches the same rows
  daemon-side search stitches ([scrollback](data-model/scrollback.md) "Soft wrap"). Copying a selection that crosses a
  soft-wrap edge joins the rows without a `\n`: it pastes back as the one line the program printed.
- A wide character is selected as a whole: a range that reaches either of its two cells highlights and copies all of it,
  in linear and rectangle mode alike, so what is highlighted is what is copied.
- Word class is fixed, not configurable: alphanumeric characters or `_` (Unicode `is_alphanumeric()`) form a word run,
  whitespace is its own run, everything else is per-cell punctuation (`crates/felis-client-core/src/selection.rs`).
- Rectangle selection has two gestures deliberately, because each covers the other's dead spot: Alt+drag is the macOS
  convention (Terminal.app; iTerm2 defaults to Cmd+Option+drag) and the natural trackpad gesture, while many Linux
  window managers grab Alt+drag for window moves; there the right button still works. The mode is fixed at press time,
  and Shift layers onto either gesture to override a program's mouse capture, same as the linear drag. Alt+press resets
  the click streak instead of counting toward a double-click: streaks select word/line runs in reading order, which has
  no rectangle analogue.

### Only a drag paints a highlight, and any press dismisses one

A press dismisses a highlight even when the program, not felis, receives it. Under an active mouse protocol a bare left
or right press forwards to the program instead of starting a selection, but it is still the user aiming at the screen,
so the highlight they did not re-draw is stale either way.

Shift is the override for _starting_ a selection over a program's capture; making it the override for clearing one too
would leave a full-screen TUI (`lazygit`, `htop`, anything that grabs the mouse the moment it starts) with no
discoverable gesture that dismisses a highlight drawn before it launched.

Middle-press is excluded: its meaning is "paste PRIMARY", which xterm defines as independent of selection state.

### A selection lives only as long as the rows under it

The selection model stores _visible-row_ coordinates, not a handle on the characters, so the highlight survives only as
long as the rows under it do. Two events replace them wholesale and drop the selection: the composed view scrolling off
the live bottom into scrollback, and the `?1049` alternate-screen swap in either direction (a TUI taking the screen, or
exiting back to the shell).

Dropping the highlight is the honest state: keeping it would paint a range over glyphs the user never selected, and
copying it would yield that other text. The alternative is to anchor the range to the content (a per-screen selection
preserved across the swap, restored when the primary screen comes back), which needs coordinates that outlive the
visible rows, the same grid-relative anchoring daemon-side selection would need.

_Revisit if_ that anchoring lands: the clear on swap becomes a restore, and the scrollback clear goes with it.

## Clipboard

OSC 52 clipboard access from the shell is gated, and the two directions are not symmetric. The **write** to the system
clipboard is **off by default**: it is a known foot-gun (a pasted command can rewrite the clipboard), so the user must
opt in. The **read** of the OS clipboard is not implemented at all (REQ-802), so a hostile program cannot exfiltrate
clipboard contents even with the write gate open ([security-model.md](security-model.md) "Output-to-input injection").

The system clipboard and the Linux PRIMARY selection stay independent in both directions: a `Paste { from: System }` on
an empty `CLIPBOARD` does not fall back to `PRIMARY`, because a paste whose source depends on which buffer happens to be
empty is not one the user can predict.

## Confirmation bar

The two destructive gates, the `kill_session` chord and the multi-line unbracketed paste (REQ-804), share one dialog
surface: a one-line question along the window's bottom edge, on the same chrome row the search bar uses, tinted to the
warning register so it never reads as a search. Its key handling is [keybindings](../reference/keybindings.md) §
"Confirmation bar". Enter deliberately cancels rather than confirms: the prompt interrupts typing, and the likeliest
in-flight keystroke at a paste prompt is the newline the dialog exists to intercept. A session switch clears a pending
question, since the question the user read would not be the question `y` answers.

The paste gate reads the shadow screen's mirror of `?2004`. Under bracketed paste the application receives the text
framed and inert, so no dialog interrupts; with the mode off, a line break in the pasted text (`\n` or `\r`) acts as
Enter at the shell, which is the injection the question exists to catch. The parked bytes ship verbatim on confirm.

The same row carries the window's one-shot refusal notices, such as a paste past `MAX_PASTE_BYTES`
([ipc.md](../reference/ipc.md) "Semantic limits"). A refusal that only reaches the log is invisible where it matters:
the user pressed the paste chord and nothing happened, which reads exactly like an unbound chord or an empty clipboard.
A notice is not a question, so it takes no key to dismiss (the next keypress clears it), and an armed question outranks
it, since the question is waiting on the key the notice would otherwise consume.

A confirmed `kill_session` destroys the session over a one-shot `Ops` connection on the window's own carrier: the
window's streaming connection states the `Window` connection mode, which the daemon's mode gate refuses `Ops::Destroy`
from. That refusal is correct: destroying is what a scripted operator connects for, so the chord does what
`felis sessions kill` does and lets the daemon's eviction push tear the window down through the ordinary session-exit
path.

Two alternatives are rejected:

- **A native OS dialog** (`rfd` or per-platform FFI). It buys a focus model, an event-loop re-entrancy hazard, and a
  platform dependency to ask a yes/no question the bar answers with one keystroke, and a modal that steals focus from a
  terminal is exactly the interruption a default-deny single-key bar avoids.
- **Enter confirms**, for the reason above.

_Revisit if_ a third destructive gate needs a payload the one-line label cannot state.

## Link preview

Holding the OSC 8 activation modifier (Ctrl) over a hyperlink cell shows the re-validated target in a one-line bar on
the same bottom chrome row the search and confirmation bars use ("Confirmation bar" above); see
[security-model.md](security-model.md) "OSC 8 hyperlinks and OSC 7 CWD" for what the target is validated against before
it reaches this bar.

### Which bar owns the bottom row

The three bars share one row and cannot stack, so an order decides which one paints when more than one is live. Search
and confirm outrank the preview: both are the user's own deliberate action (typing a query, answering a prompt), while a
hover is incidental to moving the pointer and can arrive under either. Search and confirm can both be live (a chord arms
a confirmation before the search bar sees the keystroke), so the question outranks the search bar too, and a search that
loses the row keeps only its hit highlights. Two bars in one row would not stack: every foreground quad draws after
every background one, so their glyphs would interleave instead of one hiding the other.

An active IME composition outranks it for the same reason. The preedit panel is drawn in place at the cursor, so it
collides with the preview only when the cursor sits on the bottom row. There every foreground quad is emitted after
every background one, so the two would interleave glyphs instead of one hiding the other, leaving a target the user
cannot read as either. Composing text is deliberate; the preview yields the row for as long as the composition is live
rather than for the rows it happens to occupy, because a preedit that grows or moves during composition would otherwise
flip the bar on and off under the hand.

Losing the row disarms the gesture rather than only hiding the bar ([security-model.md](security-model.md) "OSC 8
hyperlinks and OSC 7 CWD" carries why activation depends on the preview). While another bar holds it, Ctrl over a
hyperlink cell drops the Pointer affordance back to the shape the program asked for (the plain arrow unless an `OSC 22`
request is standing), and a Ctrl+Left there is an ordinary press, routed to the program or to a selection like any
other.

Two other orders are rejected:

- **The preview wins the row**, painting over whatever is open. Its background is opaque, so it would hide the question
  a user is mid-answer to behind a target they merely hovered. The pointer resting on a link while the hand types `y` is
  not a request to replace the prompt.
- **Sharing the row**, clipping the preview beside the open bar. Both halves then truncate, and a truncated target is
  the one thing this bar exists to prevent (the preview is a security surface, not a status line).

_Revisit if_ a bottom-row overlay lands that must stay legible _while_ a target is previewed; the answer would be a
second chrome row, not a re-ranking of this one.

### A target too wide for the row is clipped

A target too wide for the row is clipped to the columns available and marked with `…`. Clipping is measured in the cells
the terminal would print the text in (a combining mark or an emoji sequence shares its base's cell), so a two-cell glyph
is never split across the row's edge.

Scrolling or wrapping the target to fit is rejected: wrapping costs a second row the WM's window sizing did not grant,
and a bar that animates under the pointer is unreadable at exactly the moment the user is comparing it against the link
text.

### The preview borrows the search bar's tint

The bar is tinted with the same background the search bar uses rather than the confirmation bar's warning register:
activating a link is not a destructive action, and painting it in the warning color would read as one.

A third tint of its own is rejected: it would ask the user to learn a color code for a bar that appears and vanishes
under the pointer, for no distinction the two existing registers do not already draw (informational versus destructive).

_Revisit if_ the preview ever has to report a refusal rather than a target: a rejected activation is a warning, and
today it shows nothing at all.

## Drag-and-drop

Dropping a file on the window inserts its path as input, the universal terminal convention. The path is inserted
**verbatim**: no quoting, no space-escaping, no `file://` decoding. Adding any of those would parse intent into the path
string, which is shell _content_, and "Explicit over heuristic" ([principles](principles.md) principle 4) pushes that
out. A user who needs a quoted path quotes it; the shell, not the terminal, owns that.

The drop is ferried through the same path as a clipboard paste, so bracketed-paste mode and its control-code guard
apply: a path carrying embedded control bytes is framed, not executed. One file is delivered per drop event, so each
path is followed by a single space; this separates a multi-file drop and is harmless on a lone drop.

felis does **not** implement Kitty's OSC 72 drag-and-drop protocol (the structured variant where a TUI negotiates MIME
types and receives the file _bytes_ over an escape sequence). It is a Kitty extension, not a shared standard, and no
real consumer earns it yet ([principles](principles.md) principle 1).

_Revisit if_ a target workflow (e.g. a file manager the user runs) depends on it; it would slot in as a protocol doc
under `protocols/`, with the drop bytes flowing client → daemon → PTY so the daemon still owns the protocol exchange
([principles](principles.md) principle 3).

## IME

The client drives IME through winit (REQ-805). A composition renders as an underline-styled overlay on the active row
and shifts the cursor to show its extent; only the commit crosses the wire, and it crosses as `InputMsg::KeyBytes`: a
finalized UTF-8 string is not a keystroke, so the modifier and disambiguation logic a key report carries does not apply
to it. The daemon's cursor therefore does not move while a candidate is being chosen.

### Input-method substitutions that arrive without marked text

macOS routes some single-key substitutions through the input method rather than the keyboard layout. The JIS yen keycap
is the case users hit: Kotoeri's "character to input with the ¥ key" setting can select a backslash, and Kotoeri then
delivers `\` by calling `insertText:`.

winit forwards `insertText:` to the application as `Ime::Commit` only while `hasMarkedText()` holds
([`view.rs`](https://github.com/rust-windowing/winit/blob/v0.30.13/src/platform_impl/macos/view.rs),
`insertText:replacementRange:`). A bare keypress in Roman mode has no preedit, so the substitution is dropped and winit
falls back to building the key event from `NSEvent.characters`. felis receives U+00A5 and sends it, while Terminal.app
and iTerm2, which consume `insertText:` directly, send `\`.

felis does not correct this by inspecting the physical key. Mapping `IntlYen` to a backslash would put one layout's
policy inside an encoder that is deliberately platform-free and shared by every front-end, and it would also capture the
legitimate U+00A5 that Option+Y produces on the ABC layout, so the terminal would gain a character with no way left to
type it. The remedy is a `[keymap]` entry binding the key to `send_string`, which states the substitution where the rest
of the user's key policy already lives.

_Revisit if_ winit forwards `insertText:` for unmarked input, which would make the keyboard layer's own answer reach
felis and retire the binding.
