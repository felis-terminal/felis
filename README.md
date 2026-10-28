# felis

A daemon-backed terminal emulator for persistent sessions and headless control.

The daemon holds live sessions, terminal state, scrollback, and images in memory; GUI windows and CLI tools connect as
clients over a public, versioned IPC.

## What felis owns

- **Persistent sessions, local and remote.** Close a window without killing the shell, a long build, or an agent.
  Reattach locally with `felis attach`, or put a local GPU window onto a remote daemon with `felis --host user@remote`.
  Because the daemon transfers live terminal state instead of stacking a second emulator, sessions survive disconnects
  without tmux's protocol degradation or keybind collisions ([attach over SSH](docs/how-to/attach-over-ssh.md),
  [tmux workflows without tmux](docs/how-to/tmux-workflows-without-tmux.md)).
- **Headless control and typed IPC.** Every session is reachable without a window. `felis sessions` lets scripts, editor
  integrations, and AI agents spawn sessions, inject input, wait for commands to finish, and inspect screen state with
  `--format json` or `jsonl` ([driving a session without a window](docs/tutorials/drive-without-a-window.md),
  [CLI reference](docs/reference/cli.md), [IPC protocol](docs/reference/ipc.md)).
- **Modern protocol fidelity.** felis implements Kitty graphics, Kitty text sizing, Kitty keyboard protocol, OSC 8
  hyperlinks, OSC 133 prompt marks, OSC 7501 program status, East Asian width, and ligature font shaping
  ([protocol support matrix](docs/reference/protocols/support-matrix.md)). Because the daemon retains protocol state in
  memory, rich TUIs keep their fidelity across detach and reattach
  ([correct by construction](docs/explanation/design.md#correct-by-construction-not-by-replay)).

## Quick start

With [Nix](https://nixos.org/download) and flakes enabled:

```sh
nix run github:felis-terminal/felis
```

A window opens on a fresh session, and the shell inside it keeps running once you close the window.

The [install guide](docs/how-to/install.md) covers the profile, Home Manager, Homebrew, standalone release archives
(Linux, macOS, Windows), and from-source paths. [Your first session](docs/tutorials/first-session.md) walks the
close-and-reattach loop. One TOML file controls fonts, colors, keybindings, and window appearance
([configuration](docs/reference/config.md), [keybindings](docs/reference/keybindings.md)).

## Performance

felis is measured against Kitty, Alacritty, WezTerm, Ghostty and foot with the in-tree cross-terminal harness
(`just bench-vs`). On an Apple M4 Max it has the best throughput of the field on every benchmark. On Linux it is faster
than Kitty, WezTerm and Ghostty on every throughput benchmark and splits them with Alacritty, Ghostty's tip build and
foot, while foot and Alacritty keep the lead on keystroke latency, startup, and memory.
[Benchmarks](docs/reference/benchmarks.md) summarizes the standing per suite and has every figure.

## Scope and design

Each felis window displays one session. Layout (tiling, tabs, workspaces) belongs to your window manager, and workflow
belongs to your shell and external programs. There are no in-terminal tabs or splits, and no embedded scripting runtime;
session pickers, notification consumers, and editor integrations run as separate tools over the same public CLI and IPC
as your own scripts.

Read the [vision](docs/explanation/vision.md) for where felis draws the line, the
[design values](docs/explanation/design.md) and [testable principles](docs/explanation/principles.md) for why, and
[non-goals](docs/explanation/non-goals.md) for what stays outside the boundary.

## Status and compatibility

felis is at 0.x, an early release distributed as a Nix flake and as an archive attached to the release for each
supported target. The CLI and IPC are versioned, but expect edges.

Three targets are supported: `x86_64-linux` and `aarch64-darwin` are published to the project's binary cache and also
ship as tarballs that run on a host without Nix, and `x86_64-pc-windows-msvc` ships as a zip. `aarch64-linux` is
committed and builds from source, but carries no claim. See the
[platform matrix](docs/reference/workspace.md#build-and-platform-matrix) for details and the
[protocol support matrix](docs/reference/protocols/support-matrix.md) for terminal compatibility.

Open work is tracked in [Forgejo issues](https://git.natsukium.com/felis-terminal/felis/issues) and mirrored on
[GitHub](https://github.com/felis-terminal/felis).

## Documentation and development

The manual and its full index live in [`docs/`](docs/README.md). For architecture and comparisons with other terminals,
follow the [explanation index](docs/README.md#explanation).

felis is written in Rust with wgpu and winit. The Nix flake owns the development toolchain:

```sh
nix develop
cargo build
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and verification gates, or the
[install guide](docs/how-to/install.md) to build and run from source.

## License

Apache-2.0. See [`LICENSE`](LICENSE).

One file is excepted: `tools/bench/doom-fire-bench.patch` is GPL-3.0-or-later, since a patch carries the upstream lines
it modifies and DOOM-fire-zig is GPLv3. It is applied to that project's source at build time by the benchmark dev shell
and reaches no felis binary.
