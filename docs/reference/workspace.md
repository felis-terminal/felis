---
title: Workspace
sidebar:
  order: 7
---

The Cargo workspace: the crate map and per-crate boundaries, the dependency graph, the two version axes, the filesystem
conventions, and the platform support matrix.

Architectural decision records, crate boundary rationales, and rejected workspace configurations are documented in
[overview.md](../explanation/architecture/overview.md). Crate dependency choices and technology rationales are detailed
in [implementation.md](../explanation/implementation.md).

## Crate map

felis is a virtual Cargo workspace. Twelve `felis-*` crates sit under `crates/` (the prefix keeps the crates.io
namespace consistent); the thirteenth member, `tests/`, is a test-only package outside that tree ("Workspace guards"
below). Each crate's `Cargo.toml` is the source for its direct dependencies. `just deny` enforces the map: `deny.toml`
admits each felis crate only as a direct dependency of the crates allowed to use it, and `tools/dep_bans.py` checks each
crate's production closure against its "MUST NOT depend on" cell, which is normative.

### Per-crate contracts

| Crate               | Type      | Owns                                                                                                                | MUST NOT depend on                                           |
| ------------------- | --------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `felis-protocol`    | lib       | the `felis.proto` schema and its generated types, the IPC frame layout, the preface and protocol version constants  | `tokio`, anything OS-specific, anything graphical            |
| `felis-vt`          | lib       | the hand-rolled VT state machine and the Kitty graphics, text-sizing and keyboard dispatch                          | `tokio`, OS-specific I/O, the grid crate                     |
| `felis-grid`        | lib       | cells, grids, scrollback, the image store, reflow, selection, and the row codec ([row-codec.md](row-codec.md))      | `tokio`, graphics, fonts                                     |
| `felis-pty`         | lib       | the self-hosted PTY backends (openpty / ConPTY) and their async bridge                                              | `felis-vt`, `felis-grid`, protocol/transport                 |
| `felis-transport`   | lib       | framing, the local and SSH-stdio carriers, peer identity, the shared connection driver, and process plumbing        | `felis-vt`, `felis-grid`, daemon logic                       |
| `felis-daemon`      | bin + lib | the session pool, per-session owner tasks, the IPC server, input encoding, and the `serve`/`relay` CLI              | anything graphical                                           |
| `felis-client-core` | lib       | the shadow screen, the action enum, the IPC connector, the `config.toml` schema, and frontend-neutral client policy | `swash`, `wgpu`, `winit` (must run a headless attach)        |
| `felis-shaping`     | lib       | `swash` shaping, the font fallback chain, and rasterization                                                         | `wgpu`, `winit`                                              |
| `felis-render-core` | lib       | the renderer-neutral half of the renderer: palette, cell appearance, glyph index, and the per-row display list      | `wgpu`, `winit`                                              |
| `felis-render-wgpu` | lib       | the wgpu pipeline that draws the display list, and the glyph and image atlases                                      | `winit` (surface comes from a caller-supplied window handle) |
| `felis-cli`         | bin       | the `felis` front door: headless verbs and exec of the GUI and of `felis-<name>` commands                           | `swash`, `wgpu`, `winit` (must stay GPU-free)                |
| `felis-client`      | bin       | the GUI frontend the front door execs: the winit event loop and OS clipboard, wiring winit and wgpu                 | —                                                            |

felis hand-rolls the VT engine in `felis-vt`: it does **not** depend on `vte`, `alacritty_terminal`, or `libghostty`.
Those upstreams are read as reference only; the rationale is in [implementation.md](../explanation/implementation.md)
"VT engine".

`fuzz/` is a separate workspace (per cargo-fuzz convention) so the nightly-only libFuzzer dependency does not propagate
to the main workspace.

### Workspace guards

`tests/` is a workspace member with no library or binary target: the package `felis-workspace-tests` exists only to host
the guards on workspace-wide invariants, listed in [testing.md](testing.md) "Workspace guards". It sits outside
`crates/` because `crates/*` is the extraction set and these guards belong to no crate
([overview.md](../explanation/architecture/overview.md) "Rejected workspace shapes"). Tests that exercise one crate
against another are the opposite case: they live in the `tests/` directory of the crate that owns the seam.

## Dependency direction

Runtime edges only; no cycles. An arrow points from a crate to a crate that depends on it. An edge that a longer path
already implies, such as `felis-daemon`'s direct `felis-vt` dependency, is not drawn again.

```
felis-protocol  ← depended on by every crate except felis-pty and felis-shaping

felis-pty      felis-vt       felis-transport      felis-shaping
    │              │             │       │               │
    │              ▼             │       │               │
    │         felis-grid         │       │               │
    │          │   │   │         │       │               │
    │          │   │   └─────────┼───────┼───────┐       │
    │          │   │             │       │       ▼       ▼
    │          │   │             │       │    felis-render-core
    │          │   │             │       │           │
    │          │   │             │       │           ▼
    │          │   │             │       │    felis-render-wgpu
    │          │   │   ┌─────────┘       │           │
    │          ├───┼───┘                 │           │
    ▼          ▼   │                     │           │
    felis-daemon   └──────────┐          │           │
                              ▼          ▼           │
                            felis-client-core        │
                              │          │           │
                      ┌───────┘          ▼           │
                      ▼             felis-client ◄───┘
                  felis-cli          (GUI bin)
                 (bin: felis)
                      ╎
                      ╎ exec (runtime, not a compile edge)
                      └┄┄┄► felis-client / felis-<frontend>
```

The `felis` front-door (felis-cli) launches a frontend by _execing_ its binary, not by linking it, so there is no
`felis-cli → felis-client` compile edge; the dotted arrow is a runtime `exec`.

Backend extraction = `felis-protocol`, `felis-vt`, `felis-grid`, `felis-pty`, `felis-transport`, `felis-daemon`. Client
extraction = `felis-protocol`, `felis-vt`, `felis-grid`, `felis-transport`, `felis-client-core`, `felis-shaping`,
`felis-render-core`, `felis-render-wgpu`, `felis-client` (and `felis-cli` for the headless verbs). The shared row
(`felis-protocol`, `felis-vt`, `felis-grid`, `felis-transport`) lives in both.

## Reuse paths for non-Rust clients

`felis-protocol` is the cross-language public surface; a non-native client implements against it (and substitutes its
own transport / renderer). felis core ships no `examples/` bridge crates: the reuse paths below are realized by the
sibling repos in the ecosystem.

| Client kind                       | Crates reused                                                                                  | Bridge required                                                                             |
| --------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| Native Rust GPU (this repo)       | all client-side crates                                                                         | none                                                                                        |
| Software-rendered native          | `felis-protocol`, `felis-transport`, `felis-client-core`, `felis-shaping`, `felis-render-core` | pixel composer drawing the `felis-render-core` display list in place of `felis-render-wgpu` |
| Browser tab                       | `felis-client-core` + `felis-grid` compiled to wasm                                            | a WebSocket relay: a native `felis-client-core` attach re-exposing the session              |
| Editor without a protobuf runtime | none: codec generated from `felis.proto` by a `protoc-gen-*` of its own                        | none; attaches over the Unix socket and speaks the daemon wire                              |
| Host-terminal TUI                 | `felis-client-core` + `felis-grid` (`ansi` for the re-encode)                                  | none; attaches over the Unix socket and repaints the host terminal                          |

Two mechanisms carry that column. A language with a protobuf runtime generates its codec from `felis.proto` and speaks
the daemon wire directly, which is the only encoding the socket has ([ipc.md](ipc.md)), and a language without one does
the same with a generator of its own and the golden vectors in [row-codec.md](row-codec.md). A client that carries no
codec at all runs the CLI verbs with `--format json`, whose shapes belong to the CLI output contract ([cli.md](cli.md))
rather than to the wire, so wire-minor growth reaches it as nothing at all.

Five shared wire facilities support external satellite clients (see
[overview.md](../explanation/architecture/overview.md) for architectural tradeoffs):

- **Grid→ANSI re-encoding:** `felis-grid::ansi` reconstructs SGR and OSC 8 from stored cell state, for the pipe action,
  `felis sessions capture --ansi`, and any client painting into a host terminal; host-specific emission is a caller
  option.
- **Structural session JSON:** `felis-grid::json_v1` (behind the `json` feature) is the `felis-json` v1 format that
  out-of-repo grid mirrors consume ("Structural session JSON" in [ipc.md](ipc.md)); its schema and golden frames are
  regenerated by `just schema` and `just golden`.
- **Kitty image layout:** `felis-grid::image_layout` turns placements and Unicode-placeholder cells into per-layer
  source and destination rectangles at the caller's cell size, which a frontend paints with its own primitive.
- **Cross-language wire contract:** `felis.proto` itself, with each oneof arm's routing declared as field options
  ([ipc.md](ipc.md#the-arm-table)), so a non-Rust client needs no second source. The row payload's interior is the one
  hand-off, to [row-codec.md](row-codec.md).
- **Server-side handshake helpers:** the daemon's `Hello → Welcome → attach-ack` sequence is a shared function (no
  `felis-server-core` crate; [overview.md](../explanation/architecture/overview.md) "Shared wire knowledge across the
  satellite clients" records why not).

## Versioning

Two version axes meet in `felis-protocol`, and they are different numbers. The **protocol** version is `PROTOCOL_MAJOR`
/ `PROTOCOL_MINOR`, exchanged in the frozen preface and declared in `felis.proto`: a major is a semantic break, a minor
is additive growth, and a connection speaks the lower of the two peers' minors (see [ipc.md](ipc.md) "Versioning").
Until the compatibility freeze the dev wire carries no promise, and a break changes the bytes under major 1 rather than
burning a number.

The **crate** version is Cargo's, and it carries no wire information. The three binary crates (felis-cli, felis-client,
felis-daemon) inherit `[workspace.package] version`: that is the app version, the one a release tag and `felis version`
name. Every library crate declares its own version, which Cargo's semver rules govern for its Rust API. Every crate
under `crates/` is publishable to crates.io; the `tests/` member is not.
`cargo install felis-cli felis-client felis-daemon` is unsupported: it installs only the three binaries, while the
packages in [install.md](../how-to/install.md) also ship the terminfo entry, desktop entry, man pages and shell
completions. A wire-schema change is recorded where a peer can act on it (`PROTOCOL_MINOR` and [ipc.md](ipc.md) "The
minor ledger"), and `felis-protocol`'s crate version is not bumped alongside it
([implementation.md](../explanation/implementation.md) "Why the crate version carries no wire information").

A third axis names the _build_ rather than either contract: the `BuildIdentity` in `felis-protocol`, holding a semver,
the full git revision the binary was compiled from, and a `dirty` flag for a revision whose tracked working tree had
uncommitted changes. Its canonical rendering is `<semver> (<revision>[-dirty])`, which `felis-client` and `felis-daemon`
emit from `--version` and which `felis version` parses back out of them (`felis --version` and the human tables
abbreviate the revision to twelve digits; the abbreviation is not a parse target). The revision comes from
`FELIS_GIT_HASH` at compile time, which the flake and the Windows CI jobs set; a build without it, such as a bare
`cargo build` or a registry install, reports `unknown`.

## Cargo configuration

The workspace root `Cargo.toml` is the source of truth for `[workspace]`, `[workspace.package]`, and
`[workspace.dependencies]`. Library crates take `thiserror` and must not depend on `anyhow`; the binaries
(`felis-daemon`, `felis-cli`, `felis-client`) pass library error types through `anyhow::Context` rather than re-wrapping
them.

## Lint policy, edition, and MSRV

Edition, MSRV, the workspace lint set, and the `unsafe_code` policy are defined by the workspace config itself:
`Cargo.toml` (`[workspace.package]` and `[workspace.lints]`), `clippy.toml`, and `deny.toml`. That config governs, and
its rationale and revisit triggers live alongside it rather than in a doc. The toolchain ships through the Nix flake
(`dev/flake-module.nix`), not `rust-toolchain.toml`.

## Filesystem layout

felis paths follow the platform's standard application-data conventions, accessed via the `directories` crate
(<https://docs.rs/directories>) `ProjectDirs` slots. On Linux this yields the XDG Base Directory Specification layout;
on macOS and Windows it yields the OS's native equivalents; felis does not override the per-platform defaults.

The slots felis uses:

- **Config:** `ProjectDirs::config_dir()`. felis reads its single TOML config from this directory.
- **Cache:** `ProjectDirs::cache_dir()`. Reserved for regenerable data such as atlas dumps; nothing writes here today.
  Scrollback never lands here: it is RAM-only (REQ-609).
- **Runtime / daemon socket:** `/tmp/felis.<uid>/`, derived from the uid alone rather than from
  `ProjectDirs::runtime_dir()` or any environment variable, so an SSH login, a relay and a desktop session reach one
  daemon ([cli.md](cli.md#carrier-and-connection-lifetime) "Carrier and connection lifetime" states the rule). It holds
  `daemon.sock` and the `daemon.sock.agent` symlink; the daemon creates it `0700` and refuses anything else there. Why
  `/tmp` rather than a runtime directory is argued in
  [ipc.md](../explanation/architecture/ipc.md#cross-host-attach-ssh-stdio). Windows uses named pipes
  (`\\.\pipe\felis.<sid>.daemon`) rather than filesystem sockets; see
  [security-model.md](../explanation/security-model.md) "Daemon IPC" and [ipc.md](ipc.md).
- **Logs:** `ProjectDirs::state_dir()` (`$XDG_STATE_HOME/felis/`), `data_local_dir()` on Windows, and
  `~/Library/Logs/felis/` on macOS, where `directories` has no log or state slot. Per-binary file names and rotation are
  in [cli.md](cli.md) "Log files".
- **ConPTY host (Windows):** `data_local_dir()\conpty\<version>\`, where `<version>` is the embedded
  `Microsoft.Windows.Console.ConPTY` package version. A daemon at Medium integrity writes `conpty.dll`,
  `OpenConsole.exe` and `LICENSE-conpty.txt` there unless present, refuses every spawn when a file differs from the copy
  embedded in `felis-daemon.exe` or the host fails to load, and keeps the two binaries open, so they cannot be replaced
  or deleted, while it runs. A daemon at any other integrity level uses the ConPTY built into Windows and neither
  creates nor reads the directory. felis never removes a version directory (REQ-009f).

The directory mode (`0700`) and socket mode (`0600`) rules from [security-model.md](../explanation/security-model.md)
"Daemon IPC" apply on every platform that exposes filesystem permissions. Abstract sockets on Linux are not used.

## Build and platform matrix

Four targets are committed, named by exact identifier (Nix system ids for the Unix three, a Rust target triple for
Windows):

| Target                   | GPU backend | Runtime gate                                                                                                                                                                                    | Published artifact                                                                                                                     |
| ------------------------ | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `x86_64-linux`           | Vulkan      | **passed**: `pr.yml` runs the suite and a headless frontend smoke (Xvfb, mesa lavapipe) against the flake package; no hardware driver or Wayland compositor is witnessed                        | the flake package through the binary cache, and `felis-x86_64-linux.tar.gz` ([install.md](../how-to/install.md) "The Linux archive")   |
| `aarch64-linux`          | Vulkan      | none; packaged by the flake, run by no CI                                                                                                                                                       | none                                                                                                                                   |
| `aarch64-darwin`         | Metal       | no window gate: `darwin.yml` builds and runs the archive's headless checks; the window rests on maintainer use ([testing.md](../explanation/testing.md) "Maintainer use, for `aarch64-darwin`") | the flake package through the binary cache, and `felis-aarch64-darwin.tar.gz` ([install.md](../how-to/install.md) "The macOS archive") |
| `x86_64-pc-windows-msvc` | DX12        | **passed**: `windows.yml` runs the suite and the frontend smoke on the `x86_64-windows` runner in a real desktop session                                                                        | `felis-x86_64-pc-windows-msvc.zip` ([install.md](../how-to/install.md) "The Windows deliverable")                                      |

Committed is a roadmap category, not a support tier: a committed target with no evidence behind it carries no support
claim and publishes no artifact, though the flake's source build may still expose it. A release is published per tag and
only after every job of that tag's own chains passes ("Release gate" below). The binary cache
(`nix-cache.natsukium.com`, advertised by the flake's `nixConfig`) is not gated: it receives each `main` and tag build
of `x86_64-linux` and `aarch64-darwin`, and the tag build is the supported artifact. Why the evidence rule is shaped
this way is in [testing.md](../explanation/testing.md) "Why a support claim needs a passed gate".

A runtime gate is: the test suite on the real target, plus a frontend smoke that launches the packaged artifact, opens a
real winit window and wgpu surface on the platform backend, attaches a PTY, drives input, and verifies a rendered frame.
Cargo tests alone do not constitute one (the suite deliberately leaves pixel verification out,
[testing.md](testing.md)), and emulation does not, since it cannot validate the production GPU and window stack. The
smoke is a mode of `felis-client` itself, armed by `FELIS_SMOKE_MARKER` ([testing.md](testing.md) "CI shape"), so a
`main` or tag job drives the shipped binary; a pull request's smoke drives the debug build and is not part of the gate.
`x86_64-linux` and `x86_64-pc-windows-msvc` have a passed gate and carry the support claim, since P-2 makes a durable
distribution part of the claim and each has one. `aarch64-darwin` carries the claim on the maintainer's use of the build
instead. `aarch64-linux` carries no claim, and it moves on that target's first green run, not on this table.

`x86_64-darwin` is **not** committed: no flake package, no test coverage, no claim. `meta.platforms` in
`nix/package.nix` names the three Nix systems above rather than `lib.platforms.unix`, whose extra members (the BSDs,
Solaris, `x86_64-darwin`) felis has never built. Nothing beyond these four is committed either: no i686 Linux, 32-bit
ARM, or FreeBSD.

### Release gate

A release is an annotated `v*` tag. `.forgejo/workflows/release.yml` reruns the whole gate on the tag's own revision
rather than trusting an earlier green run on `main` ([testing.md](../explanation/testing.md) "Why a support claim needs
a passed gate"): a tag check (`just release-check <tag>`), the Cargo suite, the wire gate ([testing.md](testing.md)
"Wire compatibility gates"), and every supported target's chain, each of which checks that the built binaries report the
tag's version and clean revision and proves its archive off Nix. The Forgejo release page is created as a draft and
flipped to published only once every asset is attached. The GitHub mirror republishes it, and for a final tag crates.io
and the Homebrew tap follow. The checks and steps are those scripts: `.forgejo/workflows/release.yml`,
`.github/workflows/release-mirror.yml`, and `tools/release/`; the maintainer's order of operations is the `release`
skill.

[non-goals.md](../explanation/non-goals.md) "Cross-platform constraints" states: "Where a feature is not uniformly
implementable across macOS, Linux, and Windows, it is omitted rather than gated."

The PTY layer is self-hosted in `felis-pty` (see [implementation.md](../explanation/implementation.md) "PTY layer
(self-hosted)"), so its platform reach is bounded by `rustix` / `windows-sys` rather than a PTY crate's support list.
The Windows backend (ConPTY) is lint-checked by cross-compilation (`just check-windows`) and exercised for real by the
`x86_64-windows` runner's job against both ConPTY hosts, the inbox conhost and the embedded one
([implementation.md](../explanation/implementation.md) "ConPTY host").
