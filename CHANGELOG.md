# Changelog

User-affecting changes only: the CLI surface, config keys, keybindings, the wire protocol, defaults, and platform
support. Internal refactors, docs, and test work stay in `git log`; the design docs describe the current behavior with
no history at all, so this file is where a user (or an upgrade script) learns what moved between two builds.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning follows
`docs/reference/workspace.md` "Versioning". Each final release tag owns a dated section, which that page's "Release
gate" requires, and what has landed on `main` since the last tag accrues under Unreleased.

## [Unreleased]

- **Palette queries answer your palette**: a program asking for a palette color (`OSC 4 ; n ; ?`) gets the color the
  window paints from `[theme.palette]` and `[theme.palette.indexed]`, not the stock xterm one, so a program that picks
  readable colors from the palette picks against what you see. The window sends its palette in the theme report, a
  protocol minor 2 addition; against an older daemon or window, queries answer the stock palette as before.
- **Terminal reset keeps the window's colors**: after a full reset (`ESC c`, `reset`), color queries keep answering the
  window's configured colors and light/dark preference instead of xterm white, a window stops painting the colors the
  program had set and shows the default mouse pointer again, and pixel-size reports keep the window's size.
- **Kitty images of equal z stack as in kitty**: overlapping images at the same `z=` draw in the order they were
  created, where sending an image again under its id keeps its place, and one image's placements draw in the order they
  were first put; a window that reattaches draws them in the same order. The protocol minor is 2: `ImageNew.serial` and
  `ImagePlacement.serial` carry the two orders.
- **Kitty placeholder images under the text**: an image drawn through Unicode-placeholder cells sits between the cell
  backgrounds and the text, whatever `z=` it was placed with, as in kitty. An image re-sent under the same id disappears
  until the new data has arrived, rather than showing the old picture meanwhile.
- **Font names**: `felis fonts list` prints every family name `font.family` and the other `family` keys accept on this
  machine, with whether it is monospace and which weights and slants it has, so a name that would fall back to monospace
  can be checked before opening a window.
- **Scrollback jump keys**: jumping to the top or bottom of the scrollback moves from `Ctrl+Home` / `Ctrl+End` to
  `Ctrl+Shift+Home` / `Ctrl+Shift+End`, as in kitty and Windows Terminal, so `Ctrl+Home` / `Ctrl+End` reach editors such
  as vim and nano again. `"ctrl+home" = { kind = "scroll", step = "home" }` under `[keymap]` (and `step = "end"` for
  `ctrl+end`) brings the old chords back.
- **Legacy key bytes match xterm**: in the legacy encoding a session starts in, `Ctrl+Space` sends NUL and
  `Ctrl+Backspace` sends BS, `Ctrl+2` through `Ctrl+8`, `Ctrl+/` and `Ctrl+?` send xterm's control bytes, and Alt keeps
  the case Shift gave a letter (`Alt+Shift+B` sends `ESC B`), so Emacs `C-SPC`, readline `M-B` and a shell's
  `Ctrl+Backspace` word delete receive the key they bind.
- **Fonts through CoreText and DirectWrite**: on macOS and Windows, `font.family` resolves through the platform's font
  API, and a character the font misses draws from the face the platform picks for it, so Han takes the Japanese or the
  Chinese form your language list asks for. A symbols-only Nerd Font still comes first, and an explicit `font.fallback`
  still replaces both.
- The default font is your fixed-pitch font on macOS (Menlo unless you changed it) and Cascadia Mono on Windows.
- `felis doctor report` names the platform's per-character fallback after the fallback faces, and `--format json` adds
  it as `fonts.system_fallback`.

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

[unreleased]: https://github.com/felis-terminal/felis/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/felis-terminal/felis/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/felis-terminal/felis/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/felis-terminal/felis/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/felis-terminal/felis/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/felis-terminal/felis/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/felis-terminal/felis/releases/tag/v0.1.0
