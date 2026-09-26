# Changelog

User-affecting changes only: the CLI surface, config keys, keybindings, the wire protocol, defaults, and platform
support. Internal refactors, docs, and test work stay in `git log`; the design docs describe the current behavior with
no history at all, so this file is where a user (or an upgrade script) learns what moved between two builds.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning follows
`docs/reference/workspace.md` "Versioning". Each final release tag owns a dated section, which that page's "Release
gate" requires, and what has landed on `main` since the last tag accrues under Unreleased.

## [Unreleased]

### Added

- **Platform**: a Homebrew tap installs felis with prebuilt bottles on macOS (Apple silicon) and Linux (x86_64):
  `brew install felis-terminal/tap/felis`.

### Fixed

- Sessions recognize `TERM=xterm-felis` in a macOS window opened from Finder, or any launch that bypasses `bin/felis`:
  `felis.app` carries the terminfo entry, and on Unix the daemon prepends the entry shipped beside it (the bundle's, or
  the install prefix's `share/terminfo`) to every session's `TERMINFO_DIRS`.

## [0.1.0] - 2026-09-26

Initial release.

[unreleased]: https://github.com/felis-terminal/felis/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/felis-terminal/felis/releases/tag/v0.1.0
