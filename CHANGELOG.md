# Changelog

User-affecting changes only: the CLI surface, config keys, keybindings, the wire protocol, defaults, and platform
support. Internal refactors, docs, and test work stay in `git log`; the design docs describe the current behavior with
no history at all, so this file is where a user (or an upgrade script) learns what moved between two builds.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning follows
`docs/reference/workspace.md` "Versioning". Each final release tag owns a dated section, which that page's "Release
gate" requires, and what has landed on `main` since the last tag accrues under Unreleased.

## [Unreleased]

### Changed

- **CLI**: `felis <name> …` runs `felis-<name>` (beside `felis`, then on `$PATH`) for any word that is not a built-in
  verb, and `felis help <name>` runs `felis-<name> --help`. This replaces `felis frontend <name>`, which is removed:
  write `felis tui` instead of `felis frontend tui`.

### Fixed

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
  text-presentation emoji such as `☺` from the color-emoji font) is shrunk to fit and centred in its cell, instead of
  overlapping the next character.
- Querying grapheme cluster mode (`CSI ? 2027 $ p`) reports it permanently set (`3`) rather than set (`1`), so an
  application can tell that turning it off has no effect.
- Pressing a bare modifier (Ctrl, Shift, Alt, Cmd) or releasing a key no longer returns a scrolled-back view to the live
  screen, so Cmd+C copies a selection made in scrollback.

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

[unreleased]: https://github.com/felis-terminal/felis/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/felis-terminal/felis/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/felis-terminal/felis/releases/tag/v0.1.0
