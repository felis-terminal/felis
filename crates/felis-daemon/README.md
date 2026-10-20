# felis-daemon

The daemon of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for persistent
sessions and headless control: the session pool, the IPC server and signal handling, as the `felis-daemon` binary and as
a library for a program that serves sessions itself.

Wire compatibility follows `PROTOCOL_MINOR` and the
[minor ledger](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md#the-minor-ledger), independent of
the crate version.

## Installing

Install felis as the [install guide](https://github.com/felis-terminal/felis/blob/main/docs/how-to/install.md)
describes. `cargo install felis-cli felis-client felis-daemon` is unsupported: it installs only the three binaries,
while the packages also ship the terminfo entry, desktop entry, man pages and shell completions.
