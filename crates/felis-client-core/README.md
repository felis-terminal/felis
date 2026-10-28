# felis-client-core

The client core that the frontends of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal
emulator for persistent sessions and headless control share: the shadow screen, the action enum, the IPC client
connector, the clipboard trait and the shared config.

With `default-features = false` it builds for wasm32, with or without the `json` feature, which adds the serde types of
the `felis sessions` machine output.

The `attach` example (`examples/attach.rs`) is the smallest headless client: it attaches to a running session, applies
the replayed grid to a `ShadowScreen` through the connection's driver, and prints the screen as text.

Wire compatibility follows `PROTOCOL_MINOR` and the
[minor ledger](https://github.com/felis-terminal/felis/blob/main/docs/reference/ipc.md#the-minor-ledger), independent of
the crate version.
