# felis-client

The `felis-client` window of [felis](https://github.com/felis-terminal/felis), a daemon-backed terminal emulator for
persistent sessions and headless control: the winit event loop, the OS clipboard, the font picker and the wgpu renderer.
Closing the window detaches, and the session keeps running in the daemon.

## Installing

Install felis as the [install guide](https://github.com/felis-terminal/felis/blob/main/docs/how-to/install.md)
describes. `cargo install felis-cli felis-client felis-daemon` is unsupported: it installs only the three binaries,
while the packages also ship the terminfo entry, desktop entry, man pages and shell completions.
