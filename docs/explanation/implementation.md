---
title: Implementation style
sidebar:
  order: 9
---

The crate-level choices felis's implementation rests on, each with the alternative it rejects and the trigger that would
reopen it. The facts those choices produce live in the reference pages they cite.

## Language and runtime

### Rust

Rust is the implementation language, for two structural reasons:

- Sum types and slice patterns make the parser ergonomic.
- The borrow checker prevents whole classes of memory errors in the parser hot loop.

The implementation language itself is below the noise floor for terminal performance (VT parsing and text shaping
dominate), so the call rests on ergonomics plus the mature ecosystem for the supporting crates felis needs (swash, png,
regex, tokio), under a single-developer effort budget that rules out maintaining three native GPU backends (see "wgpu"
below).

- Rejected: Zig + libghostty-vt + native Metal/Vulkan. It triples the per-platform GPU work for a solo developer, and
  reusing `libghostty-vt` would tie felis's protocol set to Ghostty's release cadence (felis owns its VT engine; see "VT
  engine").
- Rejected: C++ + raw Metal / Vulkan / DX12, the same tripling problem, with the added cost of memory-safety footguns in
  the parser.

The known costs, accepted up front: winit's IME on Linux has rough edges, and matching Kitty's IME quality there takes
Wayland-specific work. The text-shaping path is settled on swash (see "swash" below), and wgpu's small runtime overhead
is covered under "wgpu" below.

### Tokio (multi-thread)

Source: <https://docs.rs/tokio/latest/tokio/runtime/index.html>.

The async runtime is Tokio. [ipc.md](../reference/ipc.md) uses `tokio::net::UnixStream` for the v1 transport;
[pipeline.md](rendering/pipeline.md) mentions "the IPC reader pulls grid / image / control frames from the daemon on a
tokio task."

Both binaries run the **multi-thread** scheduler, which the Tokio docs describe as: "Executes futures on a thread pool,
using a work-stealing strategy. By default, it will start a worker thread for each CPU core." The daemon builds it with
`tokio::runtime::Builder::new_multi_thread()` (`crates/felis-daemon/src/main.rs`). The client binaries do the same: the
CLI front-door for headless verbs (`crates/felis-cli/src/main.rs`) and the GUI runtime for window sessions
(`crates/felis-client/src/main.rs`). This follows the Tokio docs guide: "Most applications should use the multi-thread
scheduler, except in some niche use-cases."

Multi-thread suits the daemon because PTY reads, parsing, IPC writes, and animation timers are independent and can
overlap. The client uses the same flavor for uniformity; its winit event loop owns the renderer and shadow-grid read
side while the IPC reader runs on a tokio task (see [pipeline.md](rendering/pipeline.md) "Process model").

One overlap is deliberate and load-bearing on the daemon: **the VT parse does not run on a tokio task.** Each session's
PTY master has two dedicated OS threads in `felis-pty`: a reader that stays on the PTY, and a parse thread that advances
the grid via a sink closure (`felis_daemon::parse_sink`), consuming whole swap-buffer accumulations. The macOS PTY line
discipline delivers output in ~1 KiB chunks, so a 150 MB `cat` is ~156K reads. Two alternatives are rejected by
measurement. Handing each chunk to a tokio task to parse costs one cross-thread wake plus a cache-line bounce per chunk,
~45% of the `cat`'s wall-clock (0.87 s against 0.48 s with the parse on the reader thread). Parsing inline on the reader
thread instead holds a child's synchronous `write(2)` hostage to parse cost: the kernel drains a bulk write in a ~1 KiB
lockstep (wake the reader, hand over a chunk, wake the writer to refill), so the queue drains at 1 KiB/(read+parse) and
a DOOM-fire-style sim+write loop caps at ~628 fps. The reader/parse split with a condvar-only-when-parked handoff plus a
short non-blocking spin in the macOS reader (`felis-pty`'s sink-mode doc owns the numbers, including why neither half
works alone) reaches ~666 fps, and bulk accept reaches 250 MB/s against the inline path's 210. The session's owner task
keeps only what is downstream of the parse (effect replay, diff compose, fan-out), reaching the shared parser+grid unit
(`pool::ParseCore`) through a `parking_lot::Mutex` whose `!Send` guard makes holding it across an `await` a compile
error. The owner task coalesces its per-chunk work under a flood so it does not contend the parse thread off that lock,
takes the lock once per cycle, and the lock hands off to a waiting owner task so the parse thread cannot starve it off
in turn (see [pipeline.md](rendering/pipeline.md) "Demand-driven emission", which argues the choice of lock).

Linux's PTY queue holds ~68 KiB (64 KiB of flip buffers plus the line discipline's 4 KiB read buffer), so the writer is
not held to the reader chunk by chunk, and there the spin moves no DOOM-fire frame rate while costing 1.4 to 2.1 s of
reader user time per 12 s; the Linux reader parks straight in `poll(2)`. A producer that writes a few bytes per
`write(2)` still costs one read per write, because the reader stays caught up with it: vtebench's scrolling rows cost
most of a core in ~13-byte reads. Rejected: letting that output pile up in the kernel while the parser is busy. The
Linux write path slows down when output sits unread. A bare consumer that pauses 20 to 50 µs between reads makes the
same producer, fully kernel-bound either way, take about 55% longer per vtebench scrolling sample (~255 against ~160 ms
at 82x212), and a felis build whose reader waits like that runs the bench harness's scrolling rows up to twice as slow.
The reader's CPU would be bought back with exactly the throughput those rows measure.

The split costs one ordering rule at the end of the stream: the reader thread signals EOF to the parse thread, then
waits for it to finish before letting the lifecycle `PtyReader` report end-of-stream. Without that wait the two events
race, and the session task treats EOF as "the child is done" (it stamps the exit, flushes, and evicts subscribers), so
an attached client could lose the tail an exiting shell writes, the part users most want to read. Alternatives rejected:
making the owner task poll the grid after EOF (a retry loop with no correct bound), and routing a completion signal
through the owner task (the same join, paid on the hot task instead of the thread that is about to exit anyway). The
wait is unbounded on purpose: a sink wedged long enough to matter has already stalled the pipeline through backpressure,
so a deadline here would only convert a visible hang into silent truncation.

Cooperative scheduling note from the Tokio docs: "If the total number of tasks does not grow without bound, and no task
is blocking the thread, then it is guaranteed that tasks are scheduled fairly."

## GPU and windowing

### wgpu

Source: <https://www.w3.org/TR/webgpu/>.

wgpu is the GPU layer, with the Rust crate as the implementation (pinned to `wgpu = "30"` in
`[workspace.dependencies]`). It is a single backend abstraction over Metal (macOS), Vulkan (Linux), and DX12 (Windows)
sharing one WGSL shader: one codebase, three platforms. The render path draws cells as instanced quads (see
[rendering/pipeline.md](rendering/pipeline.md)).

- Rejected: raw Metal / Vulkan / DX12 backends, the solo-developer tripling cost again (see "Language and runtime"
  above), and the GPU-API layer is not where the complexity should live.

felis's image-atlas sizing in [rendering/pipeline.md](rendering/pipeline.md) ("`min(8192, max_texture_dimension_2d)` per
side") rests on the spec's floor: `maxTextureDimension2D` defaults to 8192 (<https://www.w3.org/TR/webgpu/#limits>), so
8192² is guaranteed on every conforming adapter. (The glyph atlas deliberately uses a smaller 2048², well under the
floor and so always available; see [rendering/pipeline.md](rendering/pipeline.md) "Atlases".)

wgpu carries a small overhead vs. native Metal / Vulkan, dwarfed by VT and shaping costs; benchmarks show terminal
workloads do not notice. Revisit if profiling on representative TUI workloads shows measurable per-frame overhead (>1 ms
of frame time directly attributable to wgpu vs. native).

### winit

Source: [input.md](input.md).

winit is the windowing/input library (pinned to `winit = "0.30"`): the de facto standard, well-maintained, handling the
per-platform messy bits (IME, DPI, multi-monitor, focus events) once instead of three times. [input.md](input.md) uses
it as the source of OS event translation (keyboard, mouse, IME, focus). _Revisit if_ winit's IME on a target platform
proves unfixable for felis's use case.

### Wayland fractional scaling

Source: <https://wayland.app/protocols/fractional-scale-v1>.

[feature-baseline.md](feature-baseline.md) commits to Wayland fractional-scale-v1. The upstream protocol describes
itself as "Protocol for requesting fractional surface scales" and specifies that "the sent scale is the numerator of a
fraction with a denominator of 120." felis's renderer applies the scale by computing physical cell metrics from logical
metrics × scale/120, matching the protocol's wire convention.

## VT engine

felis hand-rolls the whole VT engine: the parser state machine, the grid + scrollback data structures, and the Kitty
graphics, text sizing, and keyboard protocol dispatchers. Owning the engine makes the Kitty protocols first-class
citizens shaped by felis's needs (cell-side sizing handle, image-store integration), lets the hot path be tuned for
felis's workload (the SWAR print-run pre-scan below), and avoids both upstream-coordination overhead and FFI crossings
in the hot path. The accepted costs: significantly more code to write and test than reusing a library, re-discovering
edge cases prior art already found, and XTerm `ctlseqs` conformance being felis's own responsibility (see
[vt-compliance.md](../reference/protocols/vt-compliance.md)).

The prior art, and why each is rejected:

- Rejected: depend on `vte` (Alacritty's parser) and build the grid in felis. It is fast and well-tested, but bare:
  cells and grid live above it, and Kitty graphics, text sizing, and keyboard are not in the crate's scope. The parser
  is the easy part; grid, scrollback and the dispatchers are most of the engineering, so the net win is small.
- Rejected: `alacritty_terminal`. It is parser + grid + scrollback, mature, the best ergonomics for getting started
  fast; adds Kitty keyboard but fails the Kitty graphics + text sizing first-class requirement.
- Rejected: FFI to `libghostty-vt` (Ghostty). It has the best feature alignment (full Kitty extensions) and the worst
  tooling cost: Rust ↔ Zig crossings in the hot path, lifetimes that need care across the FFI boundary, release
  schedules tied to Ghostty's, and constrained protocol divergence.

Revisit if a `libghostty-vt` Rust binding emerges that is maintained by a team larger than one, with stable lifetimes
and zero copy across the FFI boundary, or if a proven, full-fidelity Kitty protocol crate becomes available in the Rust
ecosystem.

### Williams DFA

The parser layer follows Paul Flo Williams's VT500-series state diagram (<https://vt100.net/emu/dec_ansi_parser>) as
published, including its ESC/CAN/SUB cancel semantics. The diagram does not address UTF-8; that layer sits above the DFA
in felis's design ("Prior art: existing VT parsers" below).

### Pre-scan for printable runs

Source: the Williams DFA spec (<https://vt100.net/emu/dec_ansi_parser>).

The Williams DFA processes one byte per state-table lookup. felis's parser adds a SIMD pre-scan ahead of the DFA for the
printable fast path, surveyed under "Prior art: existing VT parsers" below.

The pre-scan is a stable `u64` **SWAR** (SIMD-within-a-register), not `std::simd`: the portable SIMD API is nightly-only
and would drag the MSRV (declared in `Cargo.toml`'s `[workspace.package]`) down to nightly. The `u64` form
(`scan_printable_run` / `nonprintable_mask` in `crates/felis-vt/src/lib.rs`) tests 8 bytes per iteration with a
branchless mask, stays on stable, keeps `felis-vt`'s `forbid(unsafe_code)`, and delivers the win (~3.8× on plaintext,
measured by the `parser_throughput` criterion bench).

A wider stride buys nothing end to end. The scan is about 2–3% of the ASCII print path: the `NoopSink`
`parser_throughput` bench dispatches ASCII at ~20 GiB/s, while the parse into the grid takes the same bytes at 430–590
MiB/s (`ascii_throughput`, bare `\n` and `\r\n` line endings). That gap is the grid write, not the scan: `print_str`'s
per-cell store and the per-row damage and occupancy bookkeeping (`crates/felis-grid/src/sink.rs`) are where ASCII
throughput is spent. Both wider-stride options also carry a cost the shipped `u64` SWAR avoids: the `unsafe` line the
workspace `unsafe_code` lint (`Cargo.toml` `[workspace.lints]`) guards for `std::arch` AVX2 / NEON, and the nightly pin
for `std::simd`. Revisit if a plaintext-flood profile ever shows the scan itself, not the grid mutation, dominating.

### Grapheme clustering

felis folds clusters incrementally, in the grid's print path, rather than segmenting a buffered string. The parser
delivers one Unicode scalar at a time and the grid decides per scalar whether it extends the previous grapheme, using
`unicode-width` for the width scores and hand-coded UAX#29 boundary rules (GB9 marks, emoji modifiers, the GB11 post-ZWJ
join, GB12/GB13 flag pairing). This is the hand-rolled choice Kitty and foot also make, and it suits a terminal: scalars
arrive across PTY reads, so a whole-string segmenter (the `unicode-segmentation` crate, the ecosystem's UAX#29
reference) would have to re-segment a growing buffer on every print. The full rule set is in
[grid-and-cells.md](data-model/grid-and-cells.md) "Building a cluster".

### Prior art: existing VT parsers

The Williams DFA is the shared base; terminals differ in what they layer on top for throughput.

| Parser            | Pre-scan      | Action dispatch                        | Cluster lib                   |
| ----------------- | ------------- | -------------------------------------- | ----------------------------- |
| `vte` (Alacritty) | none          | `Perform` trait (devirtualized)        | `unicode-segmentation`        |
| Kitty             | AVX2 / NEON   | function pointer                       | hand-rolled                   |
| libghostty-vt     | comptime SIMD | comptime                               | hand-rolled                   |
| WezTerm           | `memchr`      | enum                                   | `unicode-segmentation`        |
| foot              | scalar        | `switch`                               | hand-rolled subset            |
| felis             | `u64` SWAR    | `Sink` trait, compile-time specialized | hand-rolled (`unicode-width`) |

Beyond the SWAR pre-scan above, felis diverges from the reference `vte` in dispatch: actions are specialized over a
`Sink` trait so the compiler inlines call sites a function pointer or virtual call would block, and the print fast path
batches contiguous same-SGR runs into one grid mutation, stamping a packed pen template per cell and flushing at an SGR
change or row boundary. Rejected: a hand-rolled DFA layout beyond the Williams diagram (the diagram is correct;
reinventing it would be cosmetic), and a bytewise in-DFA UTF-8 decoder (more parser states; felis instead buffers the ≤
3-byte tail across PTY reads, simpler and bounded). Malformed sequences are dropped and resynced, never repaired by
heuristic inference.

## PTY abstraction

### PTY layer (self-hosted)

`felis-pty` owns the platform PTY surface directly: the `posix_openpt` primitives via `rustix::pty`
(<https://docs.rs/rustix/latest/rustix/pty/>) plus `std::process::Command` with a `pre_exec` controlling-terminal
handshake on Unix, and ConPTY (`CreatePseudoConsole`,
<https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session>) via `windows-sys` on Windows.

The surface a terminal needs from the platform is five thin syscall wrappers (open, spawn, resize, `tcgetpgrp`,
kill/`try_wait`), and the async bridge above them is felis code either way, so felis hosts those wrappers itself rather
than through a PTY crate:

- **Owned semantics.** The child-env snapshot (`felis_pty::Command::new`), the Linux `EIO`-is-EOF mapping, and kill
  idempotence are specified by felis-pty's own docs and tests instead of notes about a dependency's behavior: the same
  argument as "felis owns its VT engine" (see "Rust" above).
- **Owned concurrency invariants.** macOS's ptmx clone path races under concurrent PTY allocation (spurious `ENXIO`);
  `openpty(3)` users never see it only because Apple's libc happens to serialize internally around `ptsname`'s static
  buffer. felis-pty serializes allocation with an explicit, documented lock rather than relying on a libc accident
  (measurements in `crates/felis-pty/src/unix.rs` `open_pair`).
- **No dead weight.** The backends reuse crates already in the workspace graph (`rustix`, `libc`, `windows-sys`).

Costs, accepted deliberately: three audited `unsafe` relaxations (the Unix `pre_exec` handshake, the bare `tcgetpgrp` in
`foreground_pgrp`, and the ConPTY FFI module; each is an `#[allow(unsafe_code)]` site in `crates/felis-pty/src/unix.rs`
or `crates/felis-pty/src/windows.rs` carrying `// SAFETY:` comments), and the Windows backend is lint-checked by
cross-compilation (`just check-windows`) with its runtime behavior exercised by the `x86_64-windows` runner's job
against real conhost; its known behavioral gap is recorded in `crates/felis-pty/src/windows.rs`.

- Rejected: `portable-pty` (WezTerm's cross-platform PTY crate, <https://docs.rs/portable-pty/latest/portable_pty/>). It
  buys the five wrappers at the price of `nix` (0.28), which the workspace otherwise does not carry at all, a second
  `thiserror` (1.x via `filedescriptor`), and `serial2` / `shell-words` that felis never uses; leaves the macOS
  allocation race above to the libc accident; and turns the spawn/EOF/kill semantics into upstream-behavior notes rather
  than owned specs.
- Rejected: `alacritty_terminal`'s tty module. It brings a full terminal-state crate along for its spawn path, and ties
  the PTY layer to Alacritty's release cadence (again the VT-engine argument).
- Rejected: `pty-process`. It is Unix-only; the Windows backend would have to be hand-written anyway, at which point the
  Unix half is the easy part.

The [non-goals.md](non-goals.md#cross-platform-constraints) platform-shim carve-out (ConPTY vs. `/dev/ptmx`) holds, with
the shim living in `felis-pty`.

Revisit if the committed target set grows beyond what `rustix` + `windows-sys` cover.

## Text shaping and rasterization

### swash

felis shapes and rasterizes with swash (`crates/felis-shaping`, swash-only: no rustybuzz / cosmic-text / HarfBuzz;
pinned to `swash = "0.2"`, <https://github.com/dfrg/swash>); the choice rationale and the cache shape are in
[text-shaping.md](rendering/text-shaping.md). swash is dual Apache-2.0 / MIT, compatible with felis's Apache-2.0, whose
canonical text is `LICENSE` at the repo root.

## IPC encoding

Source: [ipc.md](../reference/ipc.md) "Application messages", which specifies the frame layer and the one encoding a
frame body carries: prost-generated Protocol Buffers over the `felis.v1` schema
(`crates/felis-protocol/proto/felis.proto`, <https://protobuf.dev/>). The schema is the authority for the shapes; the
hand-written types in `messages/` are validated domain constructors over them, not a second contract, and `convert/` is
the validator between the two, checking what proto3 cannot state. Row payloads are the one exception the schema hands
off explicitly: an opaque `bytes` field whose interior is specified by [row-codec.md](../reference/row-codec.md) and
implemented by hand in `felis-grid`.

A consumer whose language has no protobuf runtime reaches the daemon through `felis bridge`
(`crates/felis-cli/src/cli_bridge.rs`), which speaks protobuf on the socket and JSON lines on its own stdio, rather than
through a second wire encoding. [ipc.md](architecture/ipc.md) "Why protobuf" records why postcard, a second JSON wire,
MessagePack / CBOR, and a hand-rolled TLV are rejected. Authentication (peer UID via `SO_PEERCRED` / `getpeereid`) is
fixed by [security-model.md](security-model.md) "Daemon IPC".

## Diagnostics

The daemon emits diagnostics through `tracing`
([feature-baseline.md](feature-baseline.md#diagnostics-and-performance-budgets) "Diagnostics and performance budgets");
the subscriber's filter enforces the no-payload-bytes rule ([security-model.md](security-model.md) "Logging and
persistence").

## Security-related implementation details

### File-descriptor hygiene

Source: [security-model.md](security-model.md) "Process and environment boundary".

- `O_CLOEXEC` on every fd opened by the daemon (sockets, log files, image temp files). This is a Linux-side flag
  (`man 2 open`); macOS exposes the same via `O_CLOEXEC` since 10.7.
- File-bearing image transmissions (`t=f`, `t=t` per the Kitty graphics protocol) open the final path component with
  `O_NOFOLLOW` (`t=t` through `openat(2)` from a pinned parent directory) to defeat symlink-race attacks. `O_NOFOLLOW`
  is POSIX.1-2008.
- Path-named SHM (`t=s`) is name-validated, opened read-only with `O_NOFOLLOW`, size-capped, and copied out
  (`crates/felis-daemon/src/graphics/image_decode.rs`). Fd passing is not expressible in the Kitty protocol (producers
  speak over the PTY, not the daemon socket), so the name is the handle; the containment argument is in
  [security-model.md](security-model.md) "Kitty graphics".

### Peer-UID verification

Source: [security-model.md](security-model.md) "Daemon IPC".

- Linux: `SO_PEERCRED` (`man 7 socket`) returns peer credentials on a connected `AF_UNIX` socket.
- macOS and the BSDs: `getpeereid(3)`, one contract for both.

These are the OS interfaces both sides of a local dial depend on: the daemon checks its peer on accept, and the dialer
checks the listener's uid after the connect and before the preface. felis takes `SO_PEERCRED` from `rustix`, which wraps
it safely, and calls `getpeereid(3)` through `libc` behind an audited `unsafe` site: `rustix` does not wrap the BSD
path, and a wrapper crate carried for one call is the heavier of the two costs.

### Fuzzing

Fuzzing owns totality over unbounded input length, which no bounded layer can claim: the parser, the Kitty graphics
command parser, and the IPC frame decoder all accept attacker-shaped bytes ([security-model.md](security-model.md)
"Parser robustness", "Kitty graphics"). The targets, the cadence and the acceptance bar are in
[testing.md](../reference/testing.md) "Fuzzing".

## Workspace, build, and platform facts

Workspace and platform facts live in [workspace.md](../reference/workspace.md); the crate-boundary record is in
[architecture/overview.md](architecture/overview.md) "Workspace: the crate-boundary decision record", and the evidence a
support claim rests on is in [testing.md](testing.md) "Why a support claim needs a passed gate". One decision sits on
top of them.

### Why the crate version carries no wire information

Every member sits at one `[workspace.package] version` and none is published, so no resolver reads a felis crate version
and no consumer can act on one. The wire contract is carried where a peer can act on it instead: `PROTOCOL_MINOR` and
the minor ledger, negotiated per connection ([workspace.md](../reference/workspace.md) "Versioning",
[ipc.md](../reference/ipc.md) "The minor ledger"). The alternative rule, pre-1.0 a wire-schema change bumps
`felis-protocol`'s crate minor, asks for a second ledger with no reader: two numbers to keep in step, one of which
nothing resolves.

_Revisit if_ `felis-protocol` is published for the cross-language reuse it is shaped for. A resolver is then a real
consumer, its version a real contract, and semver governs it, independently of the protocol minor.
