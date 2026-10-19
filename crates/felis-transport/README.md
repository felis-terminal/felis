# felis-transport

The IPC transport of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for persistent
sessions and headless control: framing, the carriers (a Unix socket, a Windows named pipe, SSH stdio) and peer-identity
verification.

Wire compatibility follows `PROTOCOL_MINOR` and the
[minor ledger](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md#the-minor-ledger), independent of
the crate version.
