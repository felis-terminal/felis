# felis-cli

The `felis` command of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for
persistent sessions and headless control: the headless verbs (sessions, notifications, completions) and the launcher
that opens a `felis-client` window.

## Installing

Install felis as the [install guide](https://github.com/felis-terminal/felis/blob/main/docs/how-to/install.md)
describes. `cargo install felis-cli felis-client felis-daemon` is unsupported: it installs only the three binaries,
while the packages also ship the terminfo entry, desktop entry, man pages and shell completions.
