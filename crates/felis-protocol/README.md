# felis-protocol

The IPC protocol types of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for
persistent sessions and headless control: the version preface, frame layout, message families and connection modes, as
specified in [ipc.md](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md).

Wire compatibility follows `PROTOCOL_MINOR` and the
[minor ledger](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md#the-minor-ledger), independent of
the crate version.
