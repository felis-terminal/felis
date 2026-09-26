# felis

A terminal for your toolkit. Not an environment.

felis is a fast, GPU-accelerated terminal that keeps your sessions alive and renders modern protocols with full
fidelity, while leaving layout to your window manager and workflow to your tools.

## What felis owns

- **Sessions outlive windows.** Close the window without killing the shell, a long build, or an agent. Reattach locally
  or over SSH with the grid and scrollback intact in the daemon. Sessions live in memory; you get process persistence
  without the double-emulation layer, protocol degradation, or keybind collisions of tmux.
- **Modern protocol fidelity.** A narrow scope does not mean reduced capabilities. felis implements modern protocols in
  full: Kitty graphics, Kitty text sizing, Kitty keyboard protocol, OSC 8 hyperlinks, OSC 133 prompt marks, East Asian
  width, and ligature font shaping. If a rich TUI runs in Kitty, it runs identically in felis.
- **Engineered for throughput.** Because felis declines tabs, splits, and an embedded scripting runtime, engineering
  focuses strictly on parser dispatch, damage tracking, and GPU rendering. [Performance](#performance) shows where that
  lands against Kitty, Alacritty, WezTerm, Ghostty, and foot on macOS and Linux.
- **An interface for your tools.** Automation belongs outside the terminal process. Typed CLI commands
  (`felis sessions`) and a public, versioned IPC let scripts, programs, and AI agents spawn sessions headlessly, inject
  input, and inspect screen state with `--format jsonl`.

## The rest stays yours

Your window manager handles layout (tiling, tabs, workspaces). Your shell and tools handle workflow. felis provides the
interfaces for those tools to interact with it, without moving their logic inside the terminal. There are no in-terminal
tabs or splits, and no embedded scripting language.

Small utilities belong alongside the terminal: a session picker, a notification consumer, an editor integration. They
can use the same public interfaces as your own scripts. The point is to let you choose and combine dedicated tools, not
require an entire environment to use a terminal.

Read the [vision](docs/explanation/vision.md) for where felis draws the line, and the
[design values](docs/explanation/design.md) for why.

## Performance

felis is measured against Kitty, Alacritty, WezTerm, Ghostty and foot with the in-tree cross-terminal harness
(`just bench-vs`). On an Apple M4 Max it has the best throughput of the field on every benchmark. On Linux it is faster
than Kitty, WezTerm and Ghostty on every throughput benchmark and splits them with Alacritty, Ghostty's tip build and
foot, while foot and Alacritty keep the lead on keystroke latency, startup, and memory.
[Benchmarks](docs/reference/benchmarks.md) summarizes the standing per suite and has every figure.

## Quick start

With [Nix](https://nixos.org/download) and flakes enabled:

```sh
nix run github:felis-terminal/felis
```

A window opens on a fresh session, and the shell inside it keeps running once you close the window. Reattach with
`felis attach`.

The [install guide](docs/how-to/install.md) covers the profile, Home Manager, Homebrew, standalone release archives
(Linux, macOS, Windows), and from-source paths. [Your first session](docs/tutorials/first-session.md) walks the
close-and-reattach loop, and [tmux workflows without tmux](docs/how-to/tmux-workflows-without-tmux.md) maps everyday
tmux patterns to felis.

## Working with your tools

`felis --host user@remote` puts a local window on a remote daemon that keeps its sessions across disconnects
([attach over SSH](docs/how-to/attach-over-ssh.md)). Session verbs speak JSON (`--format json` or `jsonl`) and
`felis bridge` keeps a connection open for a longer-lived integration
([driving a session without a window](docs/tutorials/drive-without-a-window.md), [CLI reference](docs/reference/cli.md),
[IPC protocol](docs/reference/ipc.md)). One TOML file controls fonts, colors, keybindings, and window appearance
([configuration](docs/reference/config.md), [keybindings](docs/reference/keybindings.md)).

## Status and compatibility

felis is at 0.1.0, an early release distributed as a Nix flake and as an archive attached to the release for each
supported target. The CLI and IPC are versioned, but expect edges.

Three targets are supported: `x86_64-linux` and `aarch64-darwin` are published to the project's binary cache and also
ship as tarballs that run on a host without Nix, and `x86_64-pc-windows-msvc` ships as a zip. `aarch64-linux` is
committed and builds from source, but carries no claim. See the
[platform matrix](docs/reference/workspace.md#build-and-platform-matrix) for details and the
[protocol support matrix](docs/reference/protocols/support-matrix.md) for terminal compatibility.

Open work is tracked in [Forgejo issues](https://git.natsukium.com/felis-terminal/felis/issues) and mirrored on
[GitHub](https://github.com/felis-terminal/felis).

## Documentation and development

The manual and its full index live in [`docs/`](docs/README.md).

To understand the design, read the [vision](docs/explanation/vision.md), [values](docs/explanation/design.md), and
[testable principles](docs/explanation/principles.md), in that order. For architecture and comparisons with other
terminals, follow the [explanation index](docs/README.md#explanation).

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
