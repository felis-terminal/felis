# Changelog

User-affecting changes only: the CLI surface, config keys, keybindings, the wire protocol, defaults, and platform
support. Internal refactors, docs, and test work stay in `git log`; the design docs describe the current behavior with
no history at all, so this file is where a user (or an upgrade script) learns what moved between two builds.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning follows
`docs/reference/workspace.md` "Versioning". Each final release tag owns a dated section, which that page's "Release
gate" requires, and what has landed on `main` since the last tag accrues under Unreleased.

## [Unreleased]

### Added

- **Platform**: screen readers can read the window. The GUI client exposes the visible rows, the cursor as the caret,
  and the selection through UI Automation (Windows), NSAccessibility (macOS), and AT-SPI (Linux), building nothing until
  a screen reader first asks.
- **Platform**: on Windows and macOS, a screen reader speaks the output of each finished shell command. A shell that
  emits `OSC 133` marks (fish 4, or another shell's integration) delimits it; the focused window announces at most the
  last 256 rows. Protocol minor 3 adds the window's opt-in and the daemon's output message.
- **Config**: `shader.post` takes an array of up to 16 stage tables that run in order, each sampling the output of the
  one before, so an effect can run on top of the bundled trail. A single table keeps working. A stage that fails to load
  is skipped and the rest still run.
- **Config**: a `shader.post` stage's `params` table sets the shader's WGSL `override` constants, checked against each
  override's declared type, so one shader file serves several strengths.
- **Config**: a `shader.post` stage's `animation = { on = [...], duration_ms = ... }` plays the stage's animation once
  per bell, focus change, left press, or `shader_event` binding, drawing frames only until it ends, focused or not.
- **Config**: `shell.command` sets the argv a window launched without a command runs (bare `felis`, `new_session`) on
  the local daemon, in place of `$SHELL` or `powershell.exe`, without changing either for other programs.
- **Windows**: `felis daemon upgrade` switches the daemon to the installed binary in place, keeping every session, as on
  Linux and macOS. The successor is a new process; a daemon on the inbox ConPTY (an elevated one) still refuses with
  `unsupported`.
- **Keymap**: the `shader_event` binding kind fires a named event for `shader.post` animations.
- **Config**: `font.cell_width_scale` and `font.cell_height_scale` widen or narrow the cell by a factor, keeping each
  glyph centered in it; box-drawing and block characters stretch to fill the new cell.
- **Config**: `font.weight` and a `weight` in `[font.bold]`, `[font.italic]` and `[font.bold_italic]` pick each style's
  weight (1–1000), and an `axes` table in the same four places sets variation-axis coordinates such as `wdth`. Unset
  keys keep the faces drawn today; the per-character system fallback is unaffected.
- **Shaders**: uniform contract v2 appends `time_whole_s`, `time_fract_s`, `focus_change_time_s`, `stage_index`,
  `animation_start_s` and `animation_progress`; a shader written against v1 loads unchanged. Binding 3 gives a stage its
  own output from the previous frame, for afterglow and motion blur.
- **Wire**: protocol minor 3 adds `PushSessionExited.status`, the session program's exit code or, on Unix, the signal
  that terminated it. An older window skips it; a window facing an older daemon reads it as absent.
- **Wire**: protocol minor 3 lets an `Ops` connection read and drive a session without attaching to it: a session id on
  `Search::Query`, `Region::Request` and `Region::Rows`, and `Ops::Send` / `Ops::SendAndWait`, which write ordered
  paste, raw, key and mouse steps and, for the second, stream the next `OSC 133 ; D` mark. A client refuses each against
  an older daemon before sending it.

### Changed

- **CLI**: `felis sessions search`, `capture` and `send` no longer attach to the session; they name it in the request.
  Against a daemon older than protocol minor 3 they exit `2` under `unsupported`. A `send --wait` on a session that has
  already exited ends at once under `session_ended`, and `send` to one exits `1` under `session_ended`.

- **Windows**: the daemon runs shells on Windows Terminal's ConPTY, embedded in `felis-daemon.exe` and extracted under
  `%LOCALAPPDATA%\felis\data\conpty\`, instead of the conhost built into Windows, so every Windows install,
  `cargo install` included, gets the same tested host, which measures text widths by grapheme cluster like felis does.
  An elevated daemon keeps the built-in one. The zip adds `LICENSE-conpty.txt`.
- A post-process shader's `time_s` counts from when the window's renderer started and no longer restarts when a live
  reload rebuilds the shader.
- `felis config show-effective` and the `felis doctor report` config diff print `shader.post` as an array of stages,
  whichever form the config wrote.
- On Windows, an elevated felis runs its own daemon at `\\.\pipe\felis.<sid>.high.daemon` instead of sharing the
  non-elevated one, and each daemon only admits clients at its own integrity level.

### Removed

- **CLI**: `felis bridge` and its `felis-bridge-v1.schema.json` are gone. A client in another language dials the daemon
  socket and speaks the protobuf wire (`docs/reference/ipc.md`), or drives the `felis` verbs with `--format json` or,
  for a stream, `--format jsonl`. The CLI schema drops the unversioned `point_body`, `stream_item_body` and `error_body`
  definitions, and `error.kind` no longer lists `malformed_request`, `canceled` or `output_failed`, which only the
  bridge produced.
- **Wire**: `felis-protocol` drops `MAX_BRIDGE_LINE_BYTES`.

### Fixed

- On Linux, a family name that is not installed and that fontconfig resolves only to a generic family's face is reported
  missing: `font.family` falls back to the default monospace face, a styled `family` to the regular face, and a
  `font.fallback` entry is skipped. A typo such as `JetBrainsMono Nerd Fnt` or `Noto Sans Qwxyz` used to draw that face,
  a proportional one included, with no warning. A `fonts.conf` alias to any other face still applies and is logged.
- Closing the terminal a window was launched from, or pressing Ctrl+C in it, no longer stops the daemon that window
  started, which ended every session. This hit Windows and macOS, and Linux when the launcher was not under the systemd
  user manager.
- A post-process shader that declares a resource the bind layout does not have, or at the wrong type, is refused with an
  error instead of crashing the client at pipeline creation.
- Returning from the transient session of a `run` or `pipe` binding no longer drops the connection the window just
  landed on and reconnects to the same session, which redrew the screen a second time and logged a false "outgoing queue
  full".

## [0.3.1] - 2026-10-24

macOS Option types what the layout puts behind it, low-contrast text can be raised to a readable ratio, `ssh` brings the
terminfo entry along, and a pasted control byte no longer runs a command unasked.

### Added

- **Config**: `theme.minimum_contrast` raises text whose color is too close to its background to a WCAG 2 contrast
  ratio, keeping its hue. The default of 1 leaves colors as programs set them.
- **Shell integration**: `share/felis/shell-integration/ssh.bash`, `ssh.zsh` and `ssh.fish` define an `ssh` function
  that installs the `xterm-felis` terminfo entry on the remote host before each interactive login, and logs in with
  `TERM=xterm-256color` after a warning where it cannot. The home-manager module sources it with
  `programs.felis.enableSshIntegration`.
- **Config**: `window.macos_option_as_alt` (default `false`) makes macOS Option act as Alt for every key, so Option+B
  sends `ESC b`, at the cost of the characters the layout puts behind Option.
- **Keymap**: `background = true` on a `run` binding spawns the command without a transient session, so the window stays
  usable while it runs. The command still sees `FELIS_ORIGIN_SESSION_ID`, `FELIS_HOST` and `FELIS_CWD` and starts in the
  session's directory. A chord can now open another window, or a second window on the same session, without `open -n` or
  another launcher that returns at once.

### Fixed

- A paste into a program that has not enabled bracketed paste now asks for confirmation when it contains any C0 control
  character other than tab, such as `^O`, `^C` or `ESC`, not only when it contains a line break. Those bytes act as
  typed keys, and some run the shell's command line without a newline.
- On macOS, a character typed with Option reaches the program as that character. Before, one that came out as ASCII
  arrived with Alt still set: German Option+L sent `ESC @` instead of `@`, Option+8 `ESC {`, and under the Kitty
  keyboard protocol Option+L reported Alt+@. Option+Space now types its composed character (U+00A0 on the US layout)
  instead of Alt+Space, and the release of a key whose press went into a dead-key composition no longer reaches the
  program on its own.
- A program that follows the OS light/dark preference, such as Neovim, no longer switches to its light theme while its
  session has no window attached, for example behind a session picker. A session without a window keeps answering color
  queries with the colors and preference it last answered with.
- A keymap command no longer sees `FELIS_HOST`, `FELIS_CWD` or another origin variable left over from an earlier chord.
  Before, a window launched from a `run` command passed that command's values on to its own chords and new sessions, so
  a local window opened from a `--host` window kept reporting the remote host.

## [0.3.0] - 2026-10-20

Every felis crate is now on crates.io, sessions report more about what runs in them, and fonts and scrollback behave as
on other terminals.

- **crates.io**: every crate under `crates/` is published, the libraries on their own versions from 0.1.0.
- **Session details**: `felis sessions list` and `felis sessions info` show the directory a session was started in and
  the last `OSC 9 ; 4` progress its program reported, such as cargo's.
- **Fonts**: `felis fonts list` shows the family names `font.family` accepts; on macOS and Windows fonts resolve through
  the platform, and `font.fallback` adds to the system fallback instead of replacing it.
- **Scrollback that stays put**: a window scrolled back keeps showing the same lines while the program prints, and quick
  scrolls no longer lose steps.
- **Kitty graphics**: video through `mpv --vo=kitty` plays smoothly in a large window, and overlapping images stack in
  kitty's order.
- **Hardening**: terminal replies, bracketed paste, Kitty graphics file reads and the Windows daemon pipe are tightened.

Upgrading from 0.2:

- `felis daemon upgrade` moves a 0.2 daemon to 0.3.0 in place, keeping every session.
- Jumping to the top or bottom of the scrollback is now `Ctrl+Shift+Home` / `Ctrl+Shift+End`; see Changed to bring back
  `Ctrl+Home` / `Ctrl+End`.
- A package that compiles the terminfo entry from a source checkout reads it from
  `crates/felis-daemon/terminfo/felis.terminfo`.

### Added

- **CLI**: `felis sessions list` and `felis sessions info` report the directory a session's child was started in as
  `spawn_cwd` in `--format json` and as a `spawned:` line in `info`, so a session whose program never announces its
  directory with OSC 7 (a coding agent, a command spawned without a shell) still has one. It is the `--cwd` the session
  was created with, or the daemon's own directory when there was none.
- **Wire**: protocol minor 2 adds `SessionInfo.spawn_cwd`. An older client skips it; a client facing an older daemon
  lists no spawn directory.
- **CLI**: `felis sessions list --format json` and `felis sessions info` report the last ConEmu `OSC 9 ; 4` progress a
  program in the session sent, as `terminal_progress` and as a `progress:` line in `info` (`progress: error 71%`). A new
  shell prompt clears it. felis draws nothing for it; cargo sends it with `term.progress.term-integration = true`.
- **Wire**: protocol minor 2 adds `SessionInfo.terminal_progress`. An older client skips it; a client facing an older
  daemon lists no progress.
- **CLI**: `felis fonts list` prints every family name `font.family` and the other `family` keys accept on this machine,
  with whether it is monospace and which weights and slants it has, so a name that would fall back to monospace can be
  checked before opening a window.
- **CLI**: `felis doctor report` names the platform's per-character fallback after the fallback faces, and
  `--format json` adds it as `fonts.system_fallback`.
- **Library**: a program that serves sessions from `felis-daemon` in its own executable can hand them the `xterm-felis`
  terminfo entry: `felis_daemon::TERMINFO_SOURCE` is the entry's source to compile with `tic -x`, and
  `DaemonCaps::terminfo_dir` names the compiled directory the daemon puts first in each session's `TERMINFO_DIRS`.
- **Library**: every crate under `crates/` is published on crates.io. The libraries carry their own versions, starting
  at 0.1.0; `felis-cli`, `felis-client` and `felis-daemon` carry the app version. felis itself installs from the
  packages in the install guide, which also ship the terminfo entry, desktop entry, man pages and shell completions.
- **Library**: a program embedding `felis-grid` names itself in the XTVERSION and DA3 replies with
  `Grid::set_reply_identity`. With no identity set, XTVERSION goes unanswered and DA3 reports xterm's all-zero unit ID;
  `felis-daemon` sets felis's own.

### Changed

- **Logs**: a daemon or window on a socket other than the default one logs into that socket's own `0700` directory
  instead of the user's log directory, so test and experiment daemons no longer interleave with, or rotate away, the
  real daemon's `daemon.log`.
- The terminfo source moved from `share/terminfo/felis.terminfo` to `crates/felis-daemon/terminfo/felis.terminfo`; a
  package or a `tic -x` command that compiles it from a source checkout needs the new path.
- Jumping to the top or bottom of the scrollback moves from `Ctrl+Home` / `Ctrl+End` to `Ctrl+Shift+Home` /
  `Ctrl+Shift+End`, as in kitty and Windows Terminal, so `Ctrl+Home` / `Ctrl+End` reach editors such as vim and nano
  again. `"ctrl+home" = { kind = "scroll", step = "home" }` under `[keymap]` (and `step = "end"` for `ctrl+end`) brings
  the old chords back.
- On macOS and Windows, `font.family` resolves through the platform's font API, and a character the font misses draws
  from the face the platform picks for it, so Han takes the Japanese or the Chinese form your language list asks for. A
  symbols-only Nerd Font still comes first.
- A `font.fallback` list now goes before the system's font fallback instead of replacing it, so listing one CJK font no
  longer turns emoji, symbols and other scripts into boxes. The listed fonts still win for every character they cover,
  except that a character shown as emoji by default (⭐) takes a color emoji font even when only monochrome fonts are
  listed.
- The default font is your fixed-pitch font on macOS (Menlo unless you changed it) and Cascadia Mono on Windows.
- A window scrolled back keeps showing the same lines while the program keeps printing, and the next scroll moves one
  line from them instead of jumping by the lines printed meanwhile; search highlights and a selection stay on their
  lines too. A resize, a cleared scrollback, a full reset, or a full-screen program starting or ending while scrolled
  back returns the window to the live bottom, and leaving a scrolled-back view clears a selection made there, since it
  no longer covers the rows on screen. The viewport messages and search hits carry the history point they count from, a
  protocol minor 2 addition; an older window keeps its frozen view, and a scroll it sends while a frame is still on its
  way can land off by that frame's output.

### Fixed

- Video through Kitty graphics (`mpv --vo=kitty`) no longer stutters in a large window: the window uploads only the
  newest frame instead of every frame that arrived since its last redraw, and on a discrete GPU reuses its upload
  buffers. A 2500 × 1400 frame at 30 fps now paints every frame, where it painted about one in six.
- When the daemon dies, open windows close without starting a new daemon on their way out. That replacement could only
  report the session gone, and on Linux, while the dead daemon's systemd unit was still shutting down, it started inside
  a window's own unit, where an out-of-memory kill of one shell later took it and every session down.
- Scrolling faster than the window redraws no longer loses or undoes steps: two quick line scrolls move two lines, a
  scroll right after jumping to the bottom moves up from the bottom, and typing right after scrolling up returns to the
  prompt. The window sends each scroll as a step the daemon applies to its own view, a protocol minor 2 addition;
  against an older daemon the window scrolls as before.
- A program asking for a palette color (`OSC 4 ; n ; ?`) now gets the color the window paints from `[theme.palette]` and
  `[theme.palette.indexed]`, not the stock xterm one, so a program that picks readable colors from the palette picks
  against what you see. The window sends its palette in the theme report, a protocol minor 2 addition; against an older
  daemon or window, queries answer the stock palette as before.
- After a full reset (`ESC c`, `reset`), color queries keep answering the window's configured colors and light/dark
  preference instead of xterm white, a window stops painting the colors the program had set and shows the default mouse
  pointer again, and pixel-size reports keep the window's size.
- In the legacy encoding a session starts in, `Ctrl+Space` now sends NUL and `Ctrl+Backspace` sends BS, `Ctrl+2` through
  `Ctrl+8`, `Ctrl+/` and `Ctrl+?` send xterm's control bytes, and Alt keeps the case Shift gave a letter (`Alt+Shift+B`
  sends `ESC B`), so Emacs `C-SPC`, readline `M-B` and a shell's `Ctrl+Backspace` word delete receive the key they bind.
- Overlapping Kitty graphics images at the same `z=` now draw, as in kitty, in the order they were created, where
  sending an image again under its id keeps its place, and one image's placements draw in the order they were first put;
  a window that reattaches draws them in the same order. The protocol minor is 2: `ImageNew.serial` and
  `ImagePlacement.serial` carry the two orders.
- A Kitty graphics image drawn through Unicode-placeholder cells now sits between the cell backgrounds and the text,
  whatever `z=` it was placed with, as in kitty. An image re-sent under the same id disappears until the new data has
  arrived, rather than showing the old picture meanwhile.
- XTGETTCAP, OSC 52, and OSC 99 replies no longer repeat query bytes outside the protocol's alphabet: an undecodable
  XTGETTCAP cap name is answered empty, and an OSC 52 selector or OSC 99 `i=` with such a byte gets no reply.
- On Windows, a client refuses a daemon pipe whose server runs as another user before sending anything, as the Unix
  dialer already did with the listener's uid.
- A Kitty graphics `t=t` transmission deletes its file only when the file sits under `/tmp`, `/dev/shm`, or `$TMPDIR`
  and its path contains `tty-graphics-protocol`, as kitty does; any other path is read and left in place.
- Kitty graphics `t=f` and `t=t` refuse a path under `/proc`, `/sys`, or `/dev` (other than a file under `/dev/shm`)
  before opening it, and a FIFO without waiting on it, as kitty does. Once a `t=f`, `t=t`, or `t=s` command passes the
  checks on its own bytes, every failure, data that does not decode included, answers `EBADF:Failed to read image file`
  instead of a code naming the cause.
- A bracketed paste drops every control byte but tab, LF, and CR before framing, as xterm and kitty do, so pasted text
  can no longer end the bracket early.

## [0.2.0] - 2026-10-14

The daemon can now replace itself with a new build without ending your sessions, and scripts and agents can do more in a
session.

- **In-place daemon upgrade**: `felis daemon upgrade` replaces the running daemon with the installed build on Linux and
  macOS, keeping every session and its screen.
- **Mouse input from the command line**: `felis sessions send --mouse click:12,4` clicks, drags and scrolls in a
  fullscreen program, at the cells `felis sessions search --source visible` finds on the live screen.
- **Program status**: a program that reports its state over OSC 7501, such as a coding agent waiting on you, shows it in
  `felis sessions list` and `felis sessions info`.
- **Bug reports**: `felis doctor report` prints the builds, platform, GPU, fonts and non-default config as Markdown to
  paste into a report.
- **Fonts through fontconfig on Linux**: the default face, `font.family` and the fallback order follow the host's
  fontconfig.
- **Kitty graphics as in kitty**: placement, deletion, scrolling, eviction and replies now follow kitty, and
  `kitten icat` over ssh and yazi's previews draw whole.

Upgrading from 0.1:

- A 0.1 daemon predates the upgrade request, so restart it once after installing 0.2.0; for example,
  `felis daemon stop --when-empty` stops it after its last session ends.
- On Linux, felis now needs `libfontconfig.so.1` on the host (`libfontconfig1` on Debian and Ubuntu).

### Added

- **CLI**: `felis sessions send --mouse EVENT` (repeatable) clicks, drags and scrolls in a fullscreen program at a
  0-based cell, `[MODS+]ACTION[-BUTTON]:COL,ROW` (for example `--mouse click:12,4`), the cell numbers
  `search --source visible` returns. The daemon encodes each event against the program's current mouse modes, and under
  SGR-pixels (`?1016`) reports the middle of the cell. The verb exits `1` under `invalid_request` with nothing sent when
  the program has no reporting on for an event or the cell is off the grid. `--key` and `--mouse` go out in command-line
  order.
- **CLI**: `felis daemon upgrade` asks the running daemon to replace its binary in place with the `felis-daemon`
  installed beside `felis`, keeping every session. It runs on Linux and macOS; on Windows it refuses with `unsupported`.
  A refusal leaves the daemon serving and names its reason (`--format json` puts it in `error.reason`);
  `felis daemon stop --when-empty` remains the fallback. The verb never starts a daemon; with `--host` it runs the
  remote host's own `felis daemon upgrade` over `ssh`.
- **Wire**: protocol minor 1 adds `Ops::Upgrade` and `Ops::UpgradeReply`, with the closed refusal enum `UpgradeRefusal`.
  A client refuses to send `Upgrade` to a minor-0 daemon, so `felis daemon upgrade` against an older daemon exits `2`
  with `unsupported`.
- **CLI**: `felis sessions search --source visible` searches only the live screen, including the alternate screen a
  fullscreen program draws on, so its `col_spans` give the cell columns of on-screen text. `--source scrollback` names
  the default, which is unchanged. The stdio bridge's `sessions.search` takes the same values as `source`.
- **Wire**: protocol minor 1 also adds `SearchQuery.source` with the closed enum `SearchSource`. A client refuses to
  send a non-default source to a minor-0 daemon, which would ignore it and search scrollback, so `--source visible`
  against an older daemon exits `2` with `unsupported`.
- **CLI**: `felis doctor` notes on the `daemon` row when the running daemon is a build other than the installed
  `felis-daemon`, naming the installed one.
- **CLI**: `felis doctor report` prints the doctor checklist plus the builds, OS, display server, GPU driver version,
  resolved fonts, non-default config keys, and log paths as Markdown to paste into a bug report, or as JSON with
  `--format json`. Paths under your home directory print as `~`, and keybinding commands and texts are redacted.
- **CLI**: `felis doctor`'s `gpu` row names the driver version where the backend reports one.
- **Protocol / CLI**: programs that report their status over OSC 7501 (experimental, spec revision 0.4) show it in
  `felis sessions list --format json` and `felis sessions info`, so a script can find the session whose program is
  waiting on you. felis answers the `OSC 7501 ; ?` detection query and draws nothing.
- **Wire**: protocol minor 1 adds `SessionInfo.program_status`; a minor-0 client skips it as an unknown field.

### Changed

- `CSI 14 t` and `CSI 16 t` now answer the text area and the cell size in pixels from the PTY's winsize, instead of
  `0;0`. `CSI 13 t` and `CSI 15 t` (window position, screen size) still answer `0;0`.
- Kitty graphics replies now follow kitty: only a command with a non-zero `i=` or `I=` is answered, a delete is never
  answered even when it fails, a query is answered only when it carries `i=`, and a malformed escape or an unknown `a=`
  is dropped without a reply. A command with both `i=` and `I=` is refused with `EINVAL` before it runs. An `i=0` or
  `I=0` is the same as no id, so `a=t,i=0` stores an anonymous image instead of failing. Error codes match kitty's: a
  `t=f`/`t=t`/`t=s` source that cannot be read is `EBADF` (was `EIO`), PNG data that does not decode is `EBADPNG` (was
  `EBADF`), bad base64 or zlib data is `EINVAL` (was `EBADF`), and raw pixel data shorter than `s=`×`v=` is `ENODATA`
  (was `EINVAL`).
- On Linux, fonts are chosen by the host's fontconfig: the default face is `fc-match monospace`, `font.family` and the
  `[font.bold]`/`[font.italic]` families resolve through it (so `fonts.conf` aliases apply), and with `font.fallback`
  unset the fallback order is fontconfig's ranked list for the primary, following its prefer lists, per-language rules
  and rejected fonts and the locale's language. A text-presentation symbol takes a monochrome face before an
  earlier-ranked color one. Bitmap (PCF, BDF) and Type 1 faces that fontconfig lists are left out, so a family with only
  such faces counts as not installed. felis now needs `libfontconfig.so.1` on the host (`libfontconfig1` on Debian and
  Ubuntu).
- A `font.fallback` list longer than 62 entries keeps the first 62, with a warning.

### Fixed

- A new session's program sees the window's size from its first `TIOCGWINSZ`; it used to see 24 × 80 until the resize
  that followed the spawn landed.
- On macOS, a session's shell no longer inherits the PTY master of a session created at the same moment.
- A Textual app with mouse support no longer crashes on its first click. The `?2048` resize report now carries the PTY's
  pixel extents instead of `0`, and reports a change of those extents alone; a session no window has sized gets 8 × 16
  pixels per cell in its winsize and in the report.
- A `felis` verb piped into a reader that quits early (`felis sessions capture <id> | head -1`) no longer panics with
  "failed printing to stdout: Broken pipe". It stops at that write and exits `0`.
- A `felis bridge` stream canceled by a `cancel` op or by closing stdin no longer ends with a clean `end` whose `count`
  claims items it never wrote. Items that reach the bridge after the cancel are dropped, and the clean `end` now counts
  only the item lines that preceded it.
- A `pipe` or `run` keybinding no longer gets refused with "this pipe/run visit still owes a return" on a session that a
  `run` command switched the window to (a session picker running `felis sessions switch`, say). The refusal used to hold
  until some other switch moved the window.
- A Kitty graphics placement with only `c=` or only `r=` now keeps the image's aspect ratio, as kitty does: the other
  axis follows from the given one, and the image draws at that size instead of stretching to a box whose missing axis
  came from the image's own size.
- Searching a long soft-wrapped line with many matches (`felis sessions search`, the search overlay) no longer stalls
  the session: mapping each match to screen columns rescanned every row of the line, so a grid-sized line held the
  session's lock, and froze its output, for billions of steps.
- A case-insensitive search without `--regex` (`felis sessions search --case-insensitive`, the search overlay) now
  reports `byte_spans` and `col_spans` at the match itself on a line where a character before it changes length when
  lowercased, such as `İ` or the Kelvin sign; they used to land shifted, sometimes past the end of the text.
- Clearing the screen now treats Kitty graphics images as kitty does. `ED 2` (`clear`), `DECSED 2` and `RIS` drop every
  placement on the screen, `C=1` ones included, and `ED 3` drops scrollback placements too; each then frees every image
  nothing displays, so transmitted-but-unplaced images no longer pile up until the byte cap evicts them. Partial erases
  (`ED 0`, `ED 1`, `EL`) and the soft reset `DECSTR` no longer remove images, so redrawing one line next to an image
  keeps it.
- A Kitty graphics image sent without `i=` or `I=` is now freed once a delete or a scroll leaves it without a placement,
  as in kitty; nothing could display it again, yet it used to hold its share of the byte cap until evicted.
- When Kitty graphics images fill the session's byte cap, felis now evicts as kitty does: every image without a
  placement first, then the least recently transmitted or placed. It used to evict strictly in transmit order, so an
  image redrawn a moment ago could go before a stale one.
- A Kitty graphics Unicode-placeholder cell colored with a 256-color foreground (`\e[38;5;Nm`) now shows image `N`, as
  kitty does; only a 24-bit foreground used to name the image.
- A Kitty graphics Unicode-placeholder cell with the default foreground no longer borrows the image id of the cell
  before it; as in kitty its foreground counts as `0` in the id. A cell without the id's high-byte diacritic now takes
  it from its run.
- Kitty graphics file (`t=f`) and temp-file (`t=t`) transmissions now read the `S=` bytes at offset `O=`, as kitty does;
  they used to read the whole file.
- A reply to a Kitty graphics frame command (`a=f`) now echoes its `r=`, as kitty does.
- A Kitty graphics placement with only some of `x`, `y`, `w`, `h` now crops as kitty does: `x` and `y` default to 0 and
  `w` and `h` to the image's size. It used to draw the whole image unless all four were given.
- A Kitty graphics placement whose image was evicted in the same burst that placed it no longer lingers in the window.
- Kitty graphics images inside a scroll region (`DECSTBM`) now scroll with its text, as in kitty, and so do images on a
  reverse scroll (`RI`, `SD`). An image the scroll pushes partly past a margin is cut off there, and one pushed wholly
  out of the region is removed. They used to stay put while the text moved under them.
- The alternate screen now has a Kitty graphics image store of its own, as in kitty. A full-screen program can no longer
  free the images on the shell screen it covers, whether by clearing its screen, deleting by id range, or filling the
  byte cap, and an image it sends with an id the shell already used no longer replaces the shell's image. Leaving the
  alternate screen with `?47l` keeps its images along with its text for the next `?47h`, as in kitty.
- Kitty graphics Unicode-placeholder placements of one image now stay apart by `p=`, and a placeholder cell picks its
  placement by its underline color, as in kitty; felis kept one per image, so a second placement resized the first's
  tiles. `d=i`/`I`, `d=n`/`N`, `d=r`/`R` and transmitting the image again now remove its placeholder placements too.
- Kitty graphics image numbers (`I=`) now work as in kitty. Each transmission with `I=` stores a new image under a free
  id and answers `i=<id>,I=<number>`; a put or animation command by `I=` acts on the newest image with that number; and
  the `d=n`/`d=N` deletes are supported. felis used to take the number as the image id, so a second transmission with
  the same number replaced the first.
- A Kitty graphics image shown through Unicode-placeholder cells now keeps its aspect ratio, centered in the placement's
  `c=` by `r=` box, as kitty does; it used to be stretched to fill the box.
- A Kitty graphics `d=a` or `d=A` delete now leaves Unicode-placeholder images and images scrolled wholly into history
  in place, as kitty does. It used to drop placeholder images in the daemon without telling the window, so they vanished
  once the window reattached.
- A Kitty graphics put, delete, compose or animation command sent while a chunked (`m=1`) transmission is open now runs
  at once, as kitty does; it used to be taken as the transmission's last chunk. A delete drops the open transmission;
  the others leave it open. A non-zero `q=` on a later chunk now overrides the first chunk's.
- Placing the same Kitty graphics image twice without a placement id now shows two copies, as kitty does; the second put
  used to move the first. A reply no longer echoes `p=0`.
- Sending a Kitty graphics image again under the same id now removes its old placements, as kitty does; they used to
  stay on screen showing the new pixels.
- Deleting one placement of a Kitty graphics image with an uppercase delete (`d=I` with `p=`, or `d=P`/`Q`/`X`/`Y`/`Z`/
  `C`/`R`) now keeps the image while other placements still show it, as kitty does; it used to free the image and orphan
  them.
- `kitten icat --transfer-mode=stream`, the mode icat uses over ssh, now shows its images. felis cut every Kitty
  graphics command at 8 KiB, while kitty's own client sends up to 128 KiB at once; the limit is now kitty's 256 KiB.
- An image that a program places one cell at a time, as yazi's previewer does, now draws whole. Past the first 64
  placements in one write, the rest were dropped and the bell rang, leaving only a few strips of the image.
- A program repeating Kitty graphics puts no longer grows the daemon and every attached window without bound. Each
  screen keeps at most 16384 placements, and a put past that removes the oldest one.

## [0.1.4] - 2026-10-09

OSC 66 multi-row text and erases that follow a scroll no longer leave stale pieces on screen.

### Fixed

- Resizing the window no longer leaves broken pieces of OSC 66 multi-row text on screen. A narrowing that re-wraps a
  scaled run draws its text at natural size, and a block whose top row scrolls into history leaves no lower half behind;
  both used to leave stray half-blocks.
- Erasing from the middle of a line that a scroll had just brought into view no longer resurfaces text left of the
  cursor that the line held before the scroll. It hit a selective erase (`CSI ? 0 K`), and an erase (`ECH`, `EL`) with a
  background color or protection set.

## [0.1.3] - 2026-10-08

Font fixes. Variable fonts draw bold and italic cells at the weight and slant each style asks for, and fallback glyphs
(CJK, symbols) follow a cell's bold or italic face; a variable fallback font such as Noto Sans CJK no longer draws at
Thin.

### Fixed

- A variable font (one file with a `wght` axis, such as JetBrains Mono's `JetBrainsMono[wght].ttf`) draws bold cells at
  bold weight and regular cells at regular weight; both used to draw the file's default instance.
- A variable font whose italic is a slant axis rather than a separate file (such as `Monaspace Neon Var.ttf`) draws
  italic cells at the italic position the font names; they used to draw upright.
- Bold and italic cells draw fallback glyphs (CJK, symbols) from the fallback family's own bold or italic face, where it
  has one; they used to draw regular.
- A variable fallback font draws at regular weight. Noto Sans CJK's variable file (the one many distributions install)
  used to draw all CJK text at its default instance, Thin.

## [0.1.2] - 2026-10-07

Mostly fixes. Wide characters, emoji sequences and keycaps keep their cells through edits, resizes and font fallback;
text-sizing (OSC 66) characters stay whole at the screen edges and under line and column edits; Kitty graphics place the
cursor beside an image and keep a naturally sized image at its pixel size across font zoom; and variation sequences such
as `葛󠄀` draw their variant glyph. Also new: zsh shell integration, and `felis <name>` running external `felis-<name>`
commands in place of `felis frontend <name>`.

### Added

- `share/felis/shell-integration/felis.zsh`: sourced from `.zshrc`, it makes zsh emit `OSC 133` prompt marks with exit
  codes and `OSC 7` directory reports, which `sessions send --wait`, `last_exit_code`, `scroll_to_prompt` and
  `capture --source command-output` read. See the "Mark shell prompts" how-to.
- **home-manager**: `programs.felis.enableZshIntegration` sources that script from the generated `.zshrc`; it defaults
  to `home.shell.enableZshIntegration`.

### Changed

- **CLI**: `felis <name> …` runs `felis-<name>` (beside `felis`, then on `$PATH`) for any word that is not a built-in
  verb, and `felis help <name>` runs `felis-<name> --help`. This replaces `felis frontend <name>`, which is removed:
  write `felis tui` instead of `felis frontend tui`.

### Fixed

- A variation sequence such as `葛󠄀` (U+845B U+E0100) draws its variant glyph when the font that draws the base
  character defines one (Noto Sans CJK JP and HackGen do; Moralerspace does not). felis does not switch to another font
  for the sequence, so with a font that lacks the variant the default glyph stays.
- Resizing the window while a shell waits at an `OSC 133` prompt no longer leaves a stale copy of the prompt's first
  line on every resize (zsh or fish with a prompt line that fills the width). felis blanks the prompt and lets the shell
  repaint it, as kitty and Ghostty do; a shell that does not repaint opts out with `OSC 133;A;redraw=0`.
- Writing over or erasing one half of a multi-codepoint wide character (`❄️`, `🇯🇵`, `👩‍💻`, a keycap) removes the whole
  character instead of losing neighboring text: a shell prompt that redraws an unchanged emoji keeps it, and ECH,
  DECERA, DECFRA, DECSEL, DECSED and OSC 66 no longer leave half a glyph behind (for a plain wide `字` too). REP after a
  cluster repeats the whole cluster instead of its base character.
- Inserting or deleting characters or columns (ICH, DCH, insert mode, SL/SR, DECIC/DECDC, DECBI/DECFI), scrolling inside
  left/right margins, DECCRA, and shrinking the alternate screen no longer split a wide character into an orphaned half
  that the next edit misplaces, and an emoji that asks for two cells at the right margin no longer spills past it.
  Resizing no longer shifts a line right after an emoji that could not widen, and DECSED, SR and DECBI/DECFI no longer
  reveal text a scroll had already removed.
- A glyph that a fallback face draws wider than its cells (an East Asian Ambiguous `※` from a CJK font, or a
  text-presentation emoji such as `☺` from the color-emoji font) is shrunk to fit and centered in its cell, instead of
  overlapping the next character.
- An emoji sequence or a character with a mark that the font draws wider than its cells is shrunk to fit them in the
  same way: a regional-indicator pair with no flag (`🇦🇦`), a ZWJ sequence the font does not join, a skin tone on a base
  that takes none (`😀🏻`), a `※` with a combining mark, or a `❤️` left in one cell at the last column.
- With `font.features` set, a ligature wider than the characters it replaces is drawn as those characters instead.
- A character that Unicode displays as an emoji by default (`⭐`, `⚡`, `☕`, `⌚`, `🀄`) is drawn in color from the
  emoji font, even when a symbol font earlier in the fallback chain, an explicit `font.fallback` list or the primary
  font also covers it; it was drawn as a small monochrome symbol. Adding VS15 (`⭐︎`) still asks for the text form.
- A keycap emoji (`#️⃣`, `1️⃣`) is drawn as one full-size keycap instead of a tiny `#` beside an empty keycap.
- Printing no longer slows down by up to eight times for the rest of the session once a text-sizing (OSC 66) character
  has been written.
- A text-sizing (OSC 66) character taller than one row is no longer torn apart by inserting or deleting characters on
  one of its rows, by insert mode, by column shifts or scrolls in a region whose margin crosses it, or by inserting or
  deleting lines through it: it is erased whole, as kitty does. A scroll that pushes its upper rows into scrollback
  keeps them there. Writing over its lower rows also erases it whole, and printing in insert mode in front of a sized
  character no longer erases part of it.
- A text-sizing (OSC 66) character near the bottom or right edge is moved onto the screen whole, as kitty does, instead
  of being cut off at the edge and overwritten by the next line: it wraps or scrolls the region up to fit, steps past
  the lower rows of a taller character, and in insert mode shifts every row it covers. A run wider than the screen is no
  longer dropped whole when each of its characters fits; only a character too big for the screen or the scroll region is
  dropped. A combining mark or VS16 in the run stays with its character, and a `w` narrower than the glyph no longer
  leaves the glyph's right half outside the block.
- A zero-width joiner between characters that are not both emoji (`👩‍字`, `क‍ख‍ग`, `x‍👍`) no longer collapses them
  into one two-cell glyph: they take the cells the application counts (4, 3 and 3), so the rest of the line stays where
  the application put it.
- Querying grapheme cluster mode (`CSI ? 2027 $ p`) reports it permanently set (`3`) rather than set (`1`), so an
  application can tell that turning it off has no effect.
- The cursor, a selection, and the bidi-override warning cover both cells of a wide character (`字`, `あ`, an emoji),
  from either half: a block cursor no longer hides the right half of the glyph, and a drag that ends or starts on a wide
  character highlights and copies all of it. The cursor trail of a post-process shader follows the same shape.
- The IME pre-edit, the search and confirmation bars, and the link preview give a combining mark or an emoji sequence
  the cells the terminal gives it (`é` typed as `e` + U+0301 takes one cell, `❤️` and `🇯🇵` two), draw a flag or a skin
  tone as itself, and no longer draw a wide character past the window's right edge.
- Pressing a bare modifier (Ctrl, Shift, Alt, Cmd) or releasing a key no longer returns a scrolled-back view to the live
  screen, so Cmd+C copies a selection made in scrollback.
- Text printed right after a Kitty graphics image in the same write lands beside the image, as in kitty, instead of
  under it: the cursor moves to the image's last row, right of it, and an image placed near the bottom scrolls the
  screen instead of overlapping the rows above. On the alternate screen, images scroll with the text. A
  Unicode-placeholder put (`a=p,U=1`) no longer draws a stray copy of the image at the cursor.
- A Kitty graphics image placed without `c=`/`r=` keeps its pixel size when the cell size changes (font zoom, a config
  font change, a scale-factor change) instead of being stretched or squashed into its old cell box, and an image placed
  before the window reported its size no longer keeps an extent of one cell per pixel. With a source rectangle (`x`,
  `y`, `w`, `h`), the extent and the cursor move follow the rectangle instead of the whole image.

## [0.1.1] - 2026-09-27

### Added

- **Platform**: a Homebrew tap installs felis with prebuilt bottles on macOS (Apple silicon) and Linux (x86_64):
  `brew install felis-terminal/tap/felis`.

### Fixed

- Sessions recognize `TERM=xterm-felis` in a macOS window opened from Finder, or any launch that bypasses `bin/felis`:
  `felis.app` carries the terminfo entry, and on Unix the daemon prepends the entry shipped beside it (the bundle's, or
  the install prefix's `share/terminfo`) to every session's `TERMINFO_DIRS`.

## [0.1.0] - 2026-09-26

Initial release.

[unreleased]: https://github.com/felis-terminal/felis/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/felis-terminal/felis/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/felis-terminal/felis/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/felis-terminal/felis/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/felis-terminal/felis/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/felis-terminal/felis/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/felis-terminal/felis/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/felis-terminal/felis/releases/tag/v0.1.0
