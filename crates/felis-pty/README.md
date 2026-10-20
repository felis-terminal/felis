# felis-pty

The async PTY layer of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for
persistent sessions and headless control: `posix_openpt` on Unix and `ConPTY` on Windows, bridged to Tokio.
