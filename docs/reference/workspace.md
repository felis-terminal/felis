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

felis is a virtual Cargo workspace. Eleven `felis-*` crates sit under `crates/` (the prefix keeps the crates.io
namespace consistent); the twelfth member, `tests/`, is a test-only package outside that tree ("Workspace guards"
below). The "MUST NOT depend on" column is normative. Two of its rows are CI tests over `cargo tree`: the
`felis-protocol` async-runtime and OS-crate ban (`crates/felis-protocol/tests/crate_purity.rs`) and the `felis-grid`
font, shaping and GPU ban (`crates/felis-grid/tests/no_render_deps.rs`). `cargo deny` and clippy would not catch a
violation of the rest, so review enforces them.

### Per-crate contracts

| Crate               | Type      | Owns                                                                                                                                                                                                                                                                                                                                                             | Depends on (felis-\*)                                                                       | MUST NOT depend on                                           |
| ------------------- | --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `felis-protocol`    | lib       | the `felis.proto` schema and its generated types, IPC frame layout, message families, connection modes, the frozen preface and the protocol version constants                                                                                                                                                                                                    | — (none)                                                                                    | `tokio`, anything OS-specific, anything graphical            |
| `felis-vt`          | lib       | hand-rolled VT state machine, UTF-8/grapheme handling, Kitty graphics/text-sizing/keyboard dispatch                                                                                                                                                                                                                                                              | `felis-protocol`                                                                            | `tokio`, OS-specific I/O, the grid crate                     |
| `felis-grid`        | lib       | cells, attributes, primary+alt grids, scrollback ring, image store, reflow, daemon-side selection, the row codec (`wire.rs`, implementing [row-codec.md](row-codec.md))                                                                                                                                                                                          | `felis-protocol`, `felis-vt`                                                                | `tokio`, graphics, fonts                                     |
| `felis-pty`         | lib       | self-hosted PTY backends (openpty / ConPTY) + async bridge                                                                                                                                                                                                                                                                                                       | — (none)                                                                                    | `felis-vt`, `felis-grid`, protocol/transport                 |
| `felis-transport`   | lib       | framing, local carrier (Unix socket / Windows named pipe), peer-identity verification, SSH-stdio carrier, connect retry/backoff policy (shared by client autospawn and the relay), the shared typed connection driver (phase / direction / correlation, run by the daemon and by every client), shared process plumbing (socket-path policy, logging bootstrap)  | `felis-protocol`                                                                            | `felis-vt`, `felis-grid`, daemon logic                       |
| `felis-daemon`      | bin + lib | session pool, per-session owner tasks, IPC server loop, keyboard / mouse input encoding, `serve`/`relay` CLI                                                                                                                                                                                                                                                     | `felis-protocol`, `felis-vt`, `felis-grid`, `felis-pty`, `felis-transport`                  | anything graphical                                           |
| `felis-client-core` | lib       | shadow screen, redraw scheduler (boolean redraw flag, not damage tracking), action enum, IPC client connector, OS-clipboard trait, shared `config.toml` schema + loader, keymap chord grammar, frontend-neutral client policy (redraw/pull/blink pacing, session roster + id resolution, viewport math, paste framing, the renderer effect of each grid message) | `felis-protocol`, `felis-grid`, `felis-transport`                                           | `swash`, `wgpu`, `winit` (must run a headless attach)        |
| `felis-shaping`     | lib       | `swash` shaper, font fallback chain, shape cache, rasterization                                                                                                                                                                                                                                                                                                  | — (none)                                                                                    | `wgpu`, `winit`                                              |
| `felis-render-wgpu` | lib       | wgpu pipeline, glyph atlas, image atlas, decoration shader                                                                                                                                                                                                                                                                                                       | `felis-protocol`, `felis-grid`, `felis-shaping`                                             | `winit` (surface comes from a caller-supplied window handle) |
| `felis-cli`         | bin       | the `felis` front-door: headless verbs (`sessions`, `notifications`, `config`, `doctor`, `version`, `daemon`, `completions`), the `bridge` JSONL stdio subcommand, frontend selection / exec                                                                                                                                                                     | `felis-protocol`, `felis-transport`, `felis-client-core`, `felis-grid`                      | `swash`, `wgpu`, `winit` (must stay GPU-free)                |
| `felis-client`      | bin       | GUI frontend the front-door execs: winit event loop, OS clipboard, font picker; wires winit + wgpu                                                                                                                                                                                                                                                               | `felis-protocol`, `felis-grid`, `felis-transport`, `felis-client-core`, `felis-render-wgpu` | —                                                            |

The "Depends on" column lists runtime (non-dev) dependencies only. `felis-client-core`'s `felis-transport` edge sits
behind its default `native` feature, which a wasm32 build turns off. One extraction-relevant set of edges differs from
the idealized contracts:

- A few crates pull other workspace crates only as **dev-dependencies** for in-process integration tests and benches:
  `felis-cli` tests against `felis-daemon`/`felis-pty`, `felis-client-core` against
  `felis-daemon`/`felis-pty`/`felis-vt`, `felis-daemon` against `felis-client-core`, `felis-pty` against
  `felis-vt`/`felis-grid`, and `felis-render-wgpu` against `felis-vt`. These do not appear in the runtime graph below.

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
already implies, such as `felis-daemon`'s direct `felis-vt` dependency, is not drawn again; the "Depends on" column
above lists every direct edge.

```
felis-protocol  ← depended on by every crate except felis-pty and felis-shaping

felis-pty      felis-vt       felis-transport      felis-shaping
    │              │             │       │               │
    │              ▼             │       │               │
    │         felis-grid         │       │               │
    │          │   │   │         │       │               │
    │          │   │   └─────────┼───────┼───────┐       │
    │          │   │             │       │       ▼       ▼
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
`felis-render-wgpu`, `felis-client` (and `felis-cli` for the headless verbs). The shared row (`felis-protocol`,
`felis-vt`, `felis-grid`, `felis-transport`) lives in both.

## Reuse paths for non-Rust clients

`felis-protocol` is the cross-language public surface; a non-native client implements against it (and substitutes its
own transport / renderer). felis core ships no `examples/` bridge crates: the reuse paths below are realized by the
sibling repos in the ecosystem.

| Client kind                         | Crates reused                                                                                             | Bridge required                                                                                   |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Native Rust GPU (this repo)         | all client-side crates                                                                                    | none                                                                                              |
| Software-rendered native            | `felis-protocol`, `felis-transport`, `felis-client-core`, `felis-shaping`                                 | custom pixel composer in place of `felis-render-wgpu`                                             |
| Browser tab (`felis-web-component`) | `felis-client-core` + `felis-grid` compiled to wasm                                                       | `felis-web-gateway`, a native `felis-client-core` attach re-exposing the session over a WebSocket |
| Emacs (`felis.el`)                  | none (pure Elisp): runs `felis bridge` as a subprocess and reads one JSON object per line from its stdout | `felis bridge`, which ships with the CLI                                                          |
| Host-terminal TUI (`felis-tui`)     | `felis-client-core` + `felis-grid` (`ansi` for the re-encode)                                             | none; attaches over the Unix socket and repaints the host terminal                                |

Two mechanisms carry that column. A language with a protobuf runtime generates its codec from `felis.proto` and speaks
the daemon wire directly, which is the only encoding the socket has ([ipc.md](ipc.md)). A language without one, Elisp,
runs `felis bridge` instead: protobuf to the daemon, JSONL on the subprocess's stdin and stdout, with the line shapes
belonging to the CLI output contract ([cli.md](cli.md)) rather than to the wire, so wire-minor growth reaches the editor
as nothing at all.

Four shared wire facilities support external satellite clients (see
[overview.md](../explanation/architecture/overview.md) for architectural tradeoffs):

- **Grid→ANSI re-encoding:** `felis-grid::ansi` (`row_ansi`, `row_ansi_with`, `sgr_set`) reconstructs SGR and OSC 8 from
  stored cell state for the pipe action, `felis sessions capture --ansi`, and any client painting into a host terminal.
  Host-specific emission (256-color down-map, clip and pad to a column count, blanking Kitty Unicode-placeholder cells)
  is passed in via `RowAnsiOptions`, whose `Default` is the full-fidelity text form. A round-trip test through the real
  parser keeps it honest in-repo.
- **Structural session JSON:** `felis-grid::json_v1` (behind the `json` feature) is the `felis-json` v1 format: an
  in-band `felis_json` version and dedicated DTOs for the `Grid`, `Image`, `Conn`, `Session` and `Input` families, down
  to the cells of a dirty row, which it recodes through `felis-grid::wire`. Its schema is
  `crates/felis-grid/schemas/felis-json-v1.schema.json` and its golden frames are
  `crates/felis-grid/tests/golden/json-v1/`, regenerated by `just schema` and `just golden`; a `json → protobuf → json`
  round-trip keeps them honest in-repo. The format serves the out-of-repo consumers that mirror a grid ("Structural
  session JSON" in [ipc.md](ipc.md)); what `felis bridge` writes is the CLI contract instead, spelled in `felis-cli`.
- **Cross-language wire contract:** `felis.proto` itself. There is no committed contract manifest beside it and none is
  needed: the message and enum shapes, the frame-kind numbering, the preface constants, and each oneof arm's routing row
  are all declared in the schema, with tests asserting the Rust values match, so a non-Rust client generates its codec
  from the one file the daemon is built from. Routing rides the descriptor as a `(felis.v1.arm)` field option per arm
  ([ipc.md](ipc.md#the-arm-table)), which is why a client needs no second source for direction, correlation, modes,
  phases, or introducing minor. The interior of the row payload is the single hand-off, to [row-codec.md](row-codec.md);
  the keyboard and capability bit tables stay in [ipc.md](ipc.md) (the manifest's adopt/reject record is in the
  architecture overview).
- **Server-side handshake helpers:** the daemon's `Hello → Welcome → attach-ack` sequence is a shared function (no
  `felis-server-core` crate; [overview.md](../explanation/architecture/overview.md) "Shared wire knowledge across the
  satellite clients" records why not).

## Versioning

Two version axes meet in `felis-protocol`, and they are different numbers. The **protocol** version is `PROTOCOL_MAJOR`
/ `PROTOCOL_MINOR`, exchanged in the frozen preface and declared in `felis.proto`: a major is a semantic break, a minor
is additive growth, and a connection speaks the lower of the two peers' minors (see [ipc.md](ipc.md) "Versioning").
Until the compatibility freeze the dev wire carries no promise, and a break changes the bytes under major 1 rather than
burning a number.

The **crate** version is Cargo's, and it carries no wire information. It is declared once, as
`[workspace.package] version` in the root `Cargo.toml`, and every member inherits it with `version.workspace = true`, so
all members sit at one version and float together with the project by construction; none is published, so nothing
resolves them and no consumer can read a promise out of them. A wire-schema change is recorded where a peer can act on
it (`PROTOCOL_MINOR` and the minor ledger in [ipc.md](ipc.md) "The minor ledger", one row per addition), and
`felis-protocol`'s crate version is not bumped alongside it ([implementation.md](../explanation/implementation.md) "Why
the crate version carries no wire information").

A third axis names the _build_ rather than either contract: the `BuildIdentity` in `felis-protocol`, holding a semver,
the full git revision the binary was compiled from, and a `dirty` flag for a revision whose tracked working tree had
uncommitted changes. Its canonical rendering is `<semver> (<revision>[-dirty])`, which `felis-client` and `felis-daemon`
emit from `--version` and which `felis version` parses back out of them (`felis --version` and the human tables
abbreviate the revision to twelve digits; the abbreviation is not a parse target).

How a build gets stamped differs by path, and only one path can promise the stamp is exact. `crates/build-common.rs`
runs `git rev-parse HEAD` plus a `git status --porcelain` check at build time, re-running when `.git/HEAD`,
`.git/logs/HEAD` or `.git/index` moves. A cargo build therefore picks up commits and staged changes, but an _unstaged_
edit does not re-stamp, and the binary claims a cleanliness it does not have. The Nix path has no such gap: `flake.nix`
passes `self.rev` (clean) or `self.dirtyRev` (dirty) as `FELIS_GIT_HASH`, evaluated per build, and that is the path a
release is cut from. A build with neither reports `unknown`. A dirty _release_ is refused by the tag workflow ("Release
gate" below).

## Cargo configuration

The workspace root `Cargo.toml` is the source of truth for `[workspace]`, `[workspace.package]`, and
`[workspace.dependencies]`. The load-bearing rules:

- The workspace is virtual (no root package), and `resolver = "3"` is set explicitly so it inherits the resolver (Cargo
  1.84+ default for edition 2024 packages).
- Every external dependency enters through `[workspace.dependencies]` (Cargo 1.64+); `[workspace.lints]` (1.74+) shares
  the lint config the same way.
- Member crates pull metadata via `{key}.workspace = true` per the upstream inheritance pattern
  (<https://doc.rust-lang.org/cargo/reference/workspaces.html>).

The baseline crate picks follow the Rust ecosystem recommendations at <https://blessed.rs/crates>: `serde` for
serialization, `tokio` for the async runtime, `thiserror` / `anyhow` for errors, `clap` for the CLIs. Library crates
take `thiserror` and must not depend on `anyhow`; the binaries (`felis-daemon`, `felis-cli`, `felis-client`) pass
library error types through `anyhow::Context` rather than re-wrapping them.

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
- **Runtime / daemon socket:** `/tmp/felis.<uid>/`, derived from the uid alone, so felis does not use
  `ProjectDirs::runtime_dir()` here ([cli.md](cli.md#carrier-and-connection-lifetime) "Carrier and connection lifetime"
  states the rule). No environment variable takes part, which is what lets an SSH login, a relay and a desktop session
  reach one daemon ([how-to: attach over SSH](../how-to/attach-over-ssh.md) "Operational notes"). The directory holds
  `daemon.sock` and the `daemon.sock.agent` symlink; the daemon creates it `0700` and refuses anything else there. Why
  `/tmp` rather than a runtime directory is argued in
  [ipc.md](../explanation/architecture/ipc.md#cross-host-attach-ssh-stdio). Windows uses named pipes
  (`\\.\pipe\felis.<sid>.daemon`) rather than filesystem sockets; see
  [security-model.md](../explanation/security-model.md) "Daemon IPC" and [ipc.md](ipc.md).
- **Logs:** `ProjectDirs::state_dir()` (`$XDG_STATE_HOME/felis/`, falling back to `data_local_dir()` on Windows, which
  has no state slot and lands at `%LOCALAPPDATA%\felis\data\`, the same extra segment `directories` appends to the
  config path there). `directories` exposes no log directory at all and leaves `state_dir()` empty on macOS, so the
  macOS rule (`~/Library/Logs/felis/`, the platform's user-log home) is spelled out in `felis-daemon`'s `logging` module
  rather than delegated. Per-binary file names and rotation are in [cli.md](cli.md) "Log files".

Every call passes an empty qualifier and organization (`ProjectDirs::from("", "", "felis")`). Linux discards both, but
macOS joins them into a bundle id, so any other spelling would move the config directory to
`Library/Application Support/dev.felis.felis` and contradict the path documented in [config.md](config.md).

The directory mode (`0700`) and socket mode (`0600`) rules from [security-model.md](../explanation/security-model.md)
"Daemon IPC" apply on every platform that exposes filesystem permissions. Abstract sockets on Linux are not used.

## Build and platform matrix

Four targets are committed, named by exact identifier (Nix system ids for the Unix three, a Rust target triple for
Windows):

| Target                   | GPU backend | Runtime gate                                                                                                                                                                                                                                                                                                                                                                                                | Published artifact                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ------------------------ | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `x86_64-linux`           | Vulkan      | **passed**. `pr.yml` builds and tests on the `x86_64-linux` runner and runs the frontend smoke there against `nix build .#felis` (a pull request drives the debug Cargo build instead), headless: Xvfb in front of winit's X11 backend and mesa's lavapipe behind wgpu's Vulkan backend, both out of the flake's `smoke` shell. What that run does not witness is a hardware driver or a Wayland compositor | the flake package, pushed to the niks3 binary cache (`nix-cache.natsukium.com`, which the flake's `nixConfig` advertises) by `pr.yml`'s `linux-smoke` job as each `main` revision builds, and by `release.yml`'s on each tag. The tag build is the supported artifact; a `main` build is a preview of the next one. Each tag also attaches `felis-x86_64-linux.tar.gz`, the same build relocated to run on a host without Nix, which the release gate below unpacks and exercises in a container before it is attached ([install.md](../how-to/install.md) "The Linux archive") |
| `aarch64-linux`          | Vulkan      | none; packaged by the flake, run by no CI                                                                                                                                                                                                                                                                                                                                                                   | none                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `aarch64-darwin`         | Metal       | no window gate in CI; `darwin.yml` builds on the `aarch64-darwin` runner and runs the release archive's headless checks there, and the package sets `doCheck = false`. The window claim rests instead on the maintainer's daily use of the build ([testing.md](../explanation/testing.md) "Maintainer use, for `aarch64-darwin`")                                                                           | the flake package, pushed to the same niks3 cache by `darwin.yml`'s `darwin-build` job as each `main` revision builds, and by `release.yml`'s `darwin-package` on each tag. Each tag also attaches `felis-aarch64-darwin.tar.gz`, the same build with its Mach-O load commands rewritten and ad-hoc signed to run on a host without Nix, which the release gate below unpacks, verifies against `codesign` and runs on the `aarch64-darwin` runner before it is attached ([install.md](../how-to/install.md) "The macOS archive")                                               |
| `x86_64-pc-windows-msvc` | DX12        | **passed**. `windows.yml` runs the suite and the frontend smoke on the `x86_64-windows` runner (the release build on `main`, the debug build on a pull request; a real desktop session: DX12 needs a surface, which a session-0 service never has), and the mingw cross job lints `x86_64-pc-windows-gnu`                                                                                                   | `felis-x86_64-pc-windows-msvc.zip`, attached to the release page by `release.yml` after that tag's own Windows suite and smoke pass. `windows.yml`'s `windows-package` job uploads the same zip per `main` revision as a run artifact ([install.md](../how-to/install.md) "The Windows deliverable")                                                                                                                                                                                                                                                                            |

Committed is a roadmap category, not a support tier: a committed target with no evidence behind it carries no support
claim and publishes no artifact, though the flake's source build may still expose it. A release is published per tag and
behind every job of that tag's own chains ("Release gate" below); `windows.yml`'s `windows-package` likewise uploads a
`main` revision's zip only after that revision's Windows suite and smoke. The binary cache is not gated: it receives
each `main` and tag build of `x86_64-linux` and `aarch64-darwin` as the build finishes. Why the evidence rule is shaped
this way is in [testing.md](../explanation/testing.md) "Why a support claim needs a passed gate".

A runtime gate is: the test suite on the real target, plus a frontend smoke that launches the packaged artifact, opens a
real winit window and wgpu surface on the platform backend, attaches a PTY, drives input, and verifies a rendered frame.
Cargo tests alone do not constitute one (the suite deliberately leaves pixel verification out,
[testing.md](testing.md)), and emulation does not, since it cannot validate the production GPU and window stack. The
smoke exists as a mode of `felis-client` itself, armed by `FELIS_SMOKE_MARKER` ([testing.md](testing.md) "CI shape"), so
every target's `main` and tag job drives the shipped binary rather than a stand-in; a pull request's smoke drives the
debug build and is not part of the gate. Two targets have a passed gate, `x86_64-linux` and `x86_64-pc-windows-msvc`,
and both carry the support claim, since P-2 makes a durable distribution part of the claim and each has one. Every
supported target's distribution is an archive attached to the release, so a user without Nix gets the same build the
cache serves. `aarch64-darwin` carries the claim on the maintainer's use of the build instead
([testing.md](../explanation/testing.md) "Maintainer use, for `aarch64-darwin`"). `aarch64-linux` carries no claim, and
it moves on that target's first green run, not on this table.

`x86_64-darwin` is **not** committed: no flake package, no test coverage, no claim. `meta.platforms` in
`nix/package.nix` names the three Nix systems above rather than `lib.platforms.unix`, whose extra members (the BSDs,
Solaris, `x86_64-darwin`) felis has never built. Nothing beyond these four is committed either: no i686 Linux, 32-bit
ARM, or FreeBSD.

### Release gate

A release is a tag, and `.forgejo/workflows/release.yml` is what turns one into a published artifact. It triggers on
`push: tags: ['v*']`, serializes through `concurrency: felis-release` with cancellation off, and runs the whole gate
against the tag's own revision rather than trusting an earlier green run on `main`
([testing.md](../explanation/testing.md) "Why a support claim needs a passed gate").

`verify-tag` runs first and everything else waits on it. It runs `tools/release/verify.py` (`just release-check <tag>`
locally, and its own self-test first) on a full-history checkout, asserting:

1. the tag name is `v<major>.<minor>.<patch>`, optionally with an `-rc.<n>` prerelease suffix and nothing else;
2. the tag is annotated, not lightweight;
3. it points at the commit the run is publishing (`github.sha`);
4. the tag's core semver, with any prerelease suffix stripped, equals the root manifest's `[workspace.package] version`
   (the source `nix/package.nix` reads the package version from), and every crate inherits that version rather than
   pinning one of its own;
5. a final tag has a `## [<core semver>] - <date>` section in `CHANGELOG.md`; a candidate does not, since its entries
   are still accruing under Unreleased;
6. the working tree is clean.

Behind it the tag reruns `nix flake check`, the Cargo suite with clippy and `cargo deny`, the wire gate on its tag path
(`buf breaking` against the newest published final release, [testing.md](testing.md) "Wire compatibility gates"), and
the Linux frontend smoke, and the release page waits on all of them. `linux-smoke` pushes the store path to the niks3
cache as soon as it builds, and it checks the built binary before it does: `felis version --format json` must report the
tag's core semver, the tag's full revision, and `dirty: false`. That comparison reads the JSON object, never the
`felis --version` line, whose revision is abbreviated for a reader and so is not a parse target ("Versioning" above).

`linux-package` then builds `felis-dist`, the relocated archive, and proves it off Nix before it can be attached: it
unpacks the tarball, checks that the tree carries the binaries, libraries, terminfo, desktop entry, completions and man
pages and that no launcher or data file names a store path, and runs the archive in an `ubuntu:24.04` container that has
no `/nix`, where the CLI runs, the client passes the frontend marker smoke through the host's Vulkan stack, the daemon
it spawns is the archive's own, a PTY child's environment carries no loader or locale override, and a session started
through a symbolic link to `bin/felis` resolves `TERM=xterm-felis` from the archive's terminfo. The same identity check
runs once more against `bin/felis` from the unpacked tree, so the archive is tied to the tag and not only the store path
is.

The other two supported targets run their own chains on the same revision. `darwin-package` builds on the `macos`
runner, applies the same identity check, pushes to the cache, and then builds and proves its own `felis-dist`: it
unpacks the tarball, checks the manifest and that no Mach-O load command or rpath under `felis.app` names a store path,
verifies the ad-hoc signature with Apple's own `codesign --verify --strict --deep`, runs the bundled client directly
with `--version`, spawns a daemon from the extracted tree and confirms it is the bundle's own, checks that a session
started through a symbolic link to `bin/felis` resolves `TERM=xterm-felis` with the system `tput`, and applies the
identity check once more to `bin/felis`. The runner has no Aqua session and cannot open a window, so the direct
`--version` run is what catches a library dyld cannot resolve; the window rests on maintainer use
([testing.md](../explanation/testing.md) "Maintainer use, for `aarch64-darwin`"). It uploads the tarball as a run
artifact because jobs have separate workspaces and the Linux `release` job cannot rebuild a darwin archive. The Windows
chain repeats `windows.yml`'s `windows-test` and `windows-smoke` on the tag before `windows-package` assembles the zip,
since a tag may point at a revision that never had a green `main` run. A runner that is down fails the release, which is
the outcome a claimed target asks for.

The `release` job then downloads those run artifacts and, through `tools/release/publish.py` (which runs its own
self-test first), creates the Forgejo release on the tag through the repository API as a **draft**, attaches five files,
re-reads the draft to confirm all five arrived, and only then flips it to published. A release page is visible from the
moment it exists, so an upload that failed halfway would otherwise leave a public release claiming a build nobody can
trace. A rerun deletes an unfinished draft for the tag and cuts it again; a tag whose release is already published stops
the job instead, since publication is the one step the workflow cannot take back. The five files:

| Asset                              | What it pins                                                                                                                                                                                         |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `felis-x86_64-linux.tar.gz`        | the three binaries, the loader and libraries they need, terminfo, a desktop entry, completions, man pages, the README and the license, relocated to run on a host without Nix                        |
| `felis-aarch64-darwin.tar.gz`      | `felis.app` with the client, the daemon, the CLI and terminfo inside it, a `bin/felis` launcher, terminfo, completions, man pages, the README and the license, ad-hoc signed and free of store paths |
| `felis-x86_64-pc-windows-msvc.zip` | `felis.exe`, `felis-client.exe`, `felis-daemon.exe`, the README and the license, built from the tag's revision                                                                                       |
| `felis-config.schema.json`         | the config keys that build accepts                                                                                                                                                                   |
| `felis.proto`                      | the wire schema that build speaks                                                                                                                                                                    |

The release body says how to install each asset and through Nix, and for a final tag through Homebrew, pointing at the
GitHub repository, names the three supported targets, says `aarch64-linux` is not supported, and ends with the version's
CHANGELOG section when that section has entries. A tag with a prerelease suffix publishes with `prerelease: true`.

The GitHub repository is the public face and a push mirror of Forgejo, and a push mirror carries refs but not release
pages. `.github/workflows/release-mirror.yml`, the one GitHub workflow, fires when the mirror delivers a `v*` tag and
runs `tools/release/mirror.py` (`just release-mirror-test` is its self-test). It waits, for up to five hours, until the
Forgejo release for the tag is published, which is only after all five assets are attached. It then downloads each
asset, checks its size against the Forgejo listing, and cuts the GitHub release with the same body, prerelease flag and
assets, by the same draft-then-publish sequence. It never gates or builds anything: a tag whose Forgejo release never
publishes leaves GitHub without one. A rerun deletes an unfinished GitHub draft for the tag, and it leaves a published
GitHub release that already carries every asset alone.

Once the GitHub release exists, the workflow's `homebrew` job runs `tools/release/homebrew.py`
(`just release-homebrew-test` is its self-test). For a tag without a prerelease suffix it opens a pull request on
`felis-terminal/homebrew-tap` that moves the formula to the tag's source archive. The pull request is a draft, which the
tap does not publish on its own, when `nix/package.nix`, `nix/make-macos-app.sh`, `nix/compile-terminfo.sh` or `share/`
changed since the previous final tag.

[non-goals.md](../explanation/non-goals.md) "Cross-platform constraints" states: "Where a feature is not uniformly
implementable across macOS, Linux, and Windows, it is omitted rather than gated."

The PTY layer is self-hosted in `felis-pty` (see [implementation.md](../explanation/implementation.md) "PTY layer
(self-hosted)"), so its platform reach is bounded by `rustix` / `windows-sys` rather than a PTY crate's support list.
The Windows backend (ConPTY) is lint-checked by cross-compilation (`just check-windows`) and exercised for real by the
`x86_64-windows` runner's job, which runs it green against real conhost.
