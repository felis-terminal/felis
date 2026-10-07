# Changelog

User-affecting changes only: the CLI surface, config keys, keybindings, the wire protocol, defaults, and platform
support. Internal refactors, docs, and test work stay in `git log`; the design docs describe the current behavior with
no history at all, so this file is where a user (or an upgrade script) learns what moved between two builds.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning follows
`docs/reference/workspace.md` "Versioning". Each final release tag owns a dated section, which that page's "Release
gate" requires, and what has landed on `main` since the last tag accrues under Unreleased.

## [Unreleased]

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

[unreleased]: https://github.com/felis-terminal/felis/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/felis-terminal/felis/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/felis-terminal/felis/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/felis-terminal/felis/releases/tag/v0.1.0
