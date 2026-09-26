---
title: Comparison with other terminals
sidebar:
  order: 5
---

Each terminal below made a decision felis had to make too, and comparing the two is how the felis decision earns its
place. This is a per-peer argument, not an inventory: per-sequence status is the
[protocol support matrix](../reference/protocols/support-matrix.md), published performance measurements are in
[benchmarks.md](../reference/benchmarks.md), and the capabilities felis declines are the [non-goals](non-goals.md).

Where a peer ships a multiplexer, felis does not, for the reason
[principles](principles.md#1-add-only-what-earns-its-place) gives: layout belongs to the window manager. The entries
below record what else each peer's decisions change.

## Direct architectural neighbors

These ship in roughly the same problem space as felis.

### Kitty

C and Python, with a hand-rolled VT engine; it defined the Kitty graphics, text sizing, and keyboard protocols. It
carries a built-in multiplexer (tabs, layouts, sessions) and persists sessions through session files rather than detach
and attach.

Kitty is the protocol target: felis reuses its graphics and text-sizing designs at the wire level, and takes the parser
pre-scan idea, procedural decorations, and snapshot-style image handling. What felis changes is the scope around them.
Persistence becomes detach and attach against a daemon, which survives a closed window where a session file does not,
and the Python config becomes declarative TOML, because an embedded evaluator makes the terminal responsible for a
runtime ([non-goals](non-goals.md#scripting-and-extensibility)).

### Alacritty

Rust. GPU-rendered through OpenGL, using the `vte` parser and `alacritty_terminal` for grid and scrollback. It does no
text shaping. No multiplexer; it pairs with tmux.

Alacritty is closest to felis in philosophy and opposite in protocol coverage, which is the pairing that shows the two
are separable: a small, unscripted terminal need not also be a protocol-poor one. felis takes the discipline of a small
client and reflow on grow. It changes the coverage (shaping, Kitty extensions first-class) and the process model,
splitting daemon from client so closing the window is not closing the shell.

### foot

C, Wayland-only, CPU-rendered into shared-memory buffers with a per-row dirty bitmap and Wayland partial damage. It has
no multiplexer but does have a `--server` mode.

foot is the nearest architectural relative: a daemon-client split with a minimalist scope. Its per-row damage feeding
compositor partial damage is the power-efficiency lesson felis draws on for its own row-granularity tracking
([damage tracking](rendering/damage-tracking.md)). What felis changes is the reach: wgpu rather than a Wayland-only CPU
path, so one renderer covers macOS and Windows too.

### Ghostty

Zig, with `libghostty-vt` for parsing, grid, and scrollback. Built-in tabs and splits, and full Kitty graphics and
keyboard protocols.

Ghostty is the existence proof that a custom VT engine with modern protocol coverage is achievable by a small team in a
Zig- or Rust-class language, which is the premise felis's own hand-rolled engine rests on. Where it adopts a library
felis hand-rolls the engine, to keep the Kitty extensions first-class rather than layered on
([implementation](implementation.md#vt-engine)). The language difference is not part of the contrast; a language choice
is not evidence about throughput at terminal workloads ([principles](principles.md#2-render-everything-fast)).

### WezTerm

Rust, with `harfbuzz_rs` shaping, a built-in multiplexer that attaches across hosts, and Lua scripting.

WezTerm is the existence proof for cross-host attach in a Rust terminal, and its `harfbuzz_rs` integration is the
fallback pattern if felis ever needs to leave swash. On scope it is the deliberate inverse: WezTerm includes everything
and accepts the steady-state cost, while felis admits a capability only when no dedicated tool does it better
([design values](design.md#a-drawn-boundary)).

### iTerm2

Objective-C and Swift, macOS-only, CoreText shaping over CoreAnimation, with optional disk-backed scrollback. It adopted
the OSC 133 prompt marks FinalTerm defined, and invented the OSC 1337 inline-image protocol.

felis takes OSC 133, and treats CoreText as the macOS rendering reference. It declines OSC 1337: a second inline-image
protocol splits every producer's negotiation without adding a capability Kitty graphics lacks
([non-goals](non-goals.md#future-revisits-require-a-recorded-decision-to-lift)).

### tmux

tmux is the arrangement felis is reacting against, and [the vision page](vision.md#persistence-without-a-multiplexer)
carries the critique: two terminal emulators stacked, with protocol support depending on both layers and their
passthrough. Where tmux reattaches by replaying its stored state as terminal output, felis rehydrates an attaching
client from the state the daemon holds ([session-lifecycle.md](architecture/session-lifecycle.md#attach)). felis does
not aim to be a good citizen inside tmux, and SSH stdio attach replaces it on the remote side of an SSH session. The
wider multiplexer prior art (Screen, zellij, abduco, dtach, wezterm-mux, mosh, zmx) is weighed in
[architecture/overview.md](architecture/overview.md#prior-art-and-alternatives-considered).
