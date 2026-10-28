---
title: Testing
sidebar:
  order: 8
---

felis's verification stack: the test layers, their targets and acceptance bars, the commands that run them, and the CI
workflows that gate them. The strategy behind the layers is in [testing.md](../explanation/testing.md).

## Layer overview

| Layer                  | Purpose                                       | Source artifact / tool                                         |
| ---------------------- | --------------------------------------------- | -------------------------------------------------------------- |
| Principle invariants   | Encode the "Test:" lines from `principles.md` | [principles.md](../explanation/principles.md)                  |
| Workspace guards       | Invariants of the workspace itself            | Rust `#[test]`, `lychee`, and `cargo-deny`                     |
| Tool tests             | The repository tools under `tools/`           | pytest                                                         |
| Unit tests             | Component-internal correctness                | Rust `#[test]`                                                 |
| Property tests         | Algebraic invariants of parser, grid, reflow  | proptest                                                       |
| Fuzzing                | Robustness against hostile bytes              | cargo-fuzz over libFuzzer                                      |
| Bounded model-checking | Exhaustive proof of SWAR / arithmetic kernels | Kani                                                           |
| Snapshot tests         | Serialized output for representative inputs   | insta                                                          |
| Conformance tests      | VT100-VT520 + xterm sequence behavior         | vttest + esctest2                                              |
| Protocol conformance   | Kitty graphics / sizing / keyboard wire specs | Upstream Kitty specs                                           |
| Integration tests      | End-to-end daemon <-> client                  | Real PTY + spawned shell                                       |
| Performance benchmarks | Hot-loop throughput and latency               | Criterion, gungraun (Valgrind callgrind)                       |
| Mutation testing       | Whether the other layers bite                 | cargo-mutants                                                  |
| Visual regression      | Renderer correctness over time                | Out of scope for v1 ([explanation](../explanation/testing.md)) |

## Principle invariants

Source: [principles.md](../explanation/principles.md).

Every principle in `principles.md` ends with one or more Test clauses. Each row pairs a Test clause with the mechanism
that enforces it.

| Principle | Test clause (from `principles.md`)                                                                                                                                              | Enforced by                                                                                                         |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| 1         | Any feature that lets one window display more than one PTY at the same time is rejected.                                                                                        | Architecture: a session owns one PTY and a client attaches to one session id.                                       |
| 1         | If a dedicated tool at the shell, WM, or external-process layer does a feature as well or better, or if no real consumer needs it yet, the terminal does not implement it.      | Review: the `principle-check` skill.                                                                                |
| 1         | Any proposal that introduces a runtime evaluator inside the felis process is rejected; proposals that grow the IPC vocabulary or add typed variants to the action enum are not. | `cargo deny check bans` rejects the scripting hosts listed in `deny.toml`.                                          |
| 2         | If "this would look better in raw Kitty" is ever true for the same input, felis has a bug, not a feature gap.                                                                   | Snapshot and property tests of the parsers and the grid (`crates/felis-grid/tests/it/`).                            |
| 2         | Any optimization PR that does not show a profiler trace pointing at parser, shaper, atlas, or grid-diff code is suspect.                                                        | Criterion baselines (`just bench-all`).                                                                             |
| 3         | `pkill felis-client` followed by relaunch must not affect the running shell or its scrollback.                                                                                  | The detach/reattach test in `crates/felis-client-core/src/connector/tests.rs`.                                      |
| 3         | If changing a client config requires a daemon restart, the boundary has been crossed.                                                                                           | `tools/dep_bans.py` under `just deny` keeps `felis-grid` free of font and GPU dependencies; reload is client-local. |
| 4         | If a feature's behavior depends on parsing the _content_ of the shell output (rather than escape sequences), it is a heuristic and gets pushed out.                             | Review: the `principle-check` skill.                                                                                |

## Workspace guards

`tests/doc_source_references.rs`, in the `tests/` member ([workspace.md](workspace.md)), asserts that every bare
citation of a Rust source file under `crates/` resolves, so a path cited after a rename fails
`cargo nextest run --workspace`. Markdown links are checked separately by `just docs-links`, which runs `lychee` without
network access.

## Tool tests

The Python tools under `tools/` (release and wire-compatibility gates, bench orchestrators, prose and dependency checks)
are tested by pytest, which collects every `*_test.py` beside the tool it tests. The suites are hermetic: git
repositories are throwaway and forges are local fakes. `just test-tools` runs them all, and `pr.yml` runs that recipe on
every pull request.

## Unit tests

Unit tests pin the fixed points the design docs commit to: parser dispatch against the Williams DFA
(<https://vt100.net/emu/dec_ansi_parser>), UTF-8 chunk boundaries, SGR-pen incrementality, reflow round-trips, damage
granularity, image eviction order, cluster compositing, sparse registry installs, grid admission, and the connection
driver's state machine. They sit beside the code they test, and the owning explanation page states each contract.
`just test` runs them with the rest of the workspace suite.

## Property tests

Tool: proptest (<https://github.com/proptest-rs/proptest>).

Property tests state algebraic invariants over arbitrary input: parser and driver totality (any bytes produce
well-formed events or a typed error, never a panic), grid bounds, reflow stability, OSC payload safety, damage union
associativity, and geometry admission. By convention they live in a crate's integration-test tree
(`crates/<crate>/tests/it/proptest_*.rs`) and run under `just test`.

## Fuzzing

Tool: cargo-fuzz (<https://github.com/rust-fuzz/cargo-fuzz>) over libFuzzer (<https://llvm.org/docs/LibFuzzer.html>).
The targets live under `fuzz/fuzz_targets/`, a separate workspace; `just fuzz-list` enumerates them.

### Acceptance criteria

Two bars run at two cadences. The qualification bar comes from [security-model.md](../explanation/security-model.md): a
target passes a 24-hour run with zero panics, zero OOMs, and bounded peak memory. That run is manual, on a machine that
can spare a day per target.

CI enforces the tripwire below it. `fuzz.yml` runs `just fuzz-smoke` (10,000 executions per target) on pushes to `main`
and pull requests targeting it, skipping documentation-only changes, and `just fuzz-long` nightly (600 seconds per
target, then corpus minification). A finding at either cadence fails the run; a clean nightly is not a qualification.

### Running fuzz targets

| Task                              | Command                     |
| --------------------------------- | --------------------------- |
| Smoke pass across all targets     | `just fuzz-smoke`           |
| Long-run with corpus minification | `just fuzz-long`            |
| Run a specific target             | `just fuzz <target> [args]` |
| Prime working corpus from seeds   | `fuzz/seed-corpus.sh`       |

The recipes build without a sanitizer, so a panic or a failed debug assertion is the finding; `FUZZ_SANITIZER` selects
one (`address`, for instance) and `FUZZ_JOBS` sets how many targets `just fuzz-long` runs at once.

### Curated seed corpus

`fuzz/seeds/<target>/` stores bug-trail seeds: minimal raw byte inputs surfacing edge cases or fixed regressions.
`fuzz/seed-corpus.sh` copies them into `fuzz/corpus/<target>/`. To add a seed, minimize the byte sequence, write it to
`fuzz/seeds/<target>/<name>.bin`, and re-run the script.

## Snapshot tests

Tool: insta (<https://github.com/mitsuhiko/insta>).

A snapshot test serializes an output (a grid after a CSI, OSC, or Kitty sequence, a reflow, a rehydration, a recorded
PTY replay, a CLI help page) and compares it against a checked-in `.snap` file.

```
just snapshot-test     # run tests and stage changes
just snapshot-review   # interactive cargo insta review
just snapshot-accept   # accept all staged snapshots
```

The renderer's cell-instance fixtures are raw bytes rather than insta snapshots: they pin the bg, fg, and decoration
instance buffers a set of small screens paint into, and `just golden` rewrites them.

## Conformance tests

### vttest

Source: <https://invisible-island.net/vttest/vttest.html>.

`crates/felis-pty/tests/vttest_smoke.rs` drives vttest's VT100-VT520 and xterm scenarios through a felis PTY and asserts
the round-trips against the grid. Deliberate non-conformances recorded in
[vt-compliance.md](../explanation/protocols/vt-compliance.md) (charset switching is a no-op, DECDLD soft characters are
unsupported, DECCOLM does not resize) are excluded from failure criteria. The esctest2 suite, its ratcheted baseline,
and its known failures are [esctest-compatibility.md](esctest-compatibility.md).

### Kitty protocol conformance

The graphics (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>), text sizing
(<https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>), and keyboard
(<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>) protocols are tested against the upstream Kitty specifications;
the per-feature verdict is [protocols/support-matrix.md](protocols/support-matrix.md).

## Integration tests

Integration tests run the daemon against real children on a real PTY: shells, pagers and editors, captured producer byte
streams, detach and reattach, the `felis` verbs' machine output, and shell completions under real interpreters. The
tests that drive an external tool (sshd and ssh, `less`, `nvim`, vttest and esctest, fish and zsh) need the dev shell's
tools and fail without them. They compile on Unix only, so the native Windows job does not run them. The Windows
in-place upgrade tests are the exception: the native Windows job runs them, each rerunning itself at Medium integrity so
that the daemon uses the embedded ConPTY host.

REQ-1206 ([spec.md](spec.md)) states the exit criteria for this layer, and part of it is met by hand rather than by a
test: an editor or file-manager session driven end to end through the GUI client. The client's window state machines are
unit-tested without a window or a daemon, through the felis-owned event, input and window-effect types the winit adapter
translates to; the client as a whole is covered by the headless frontend smoke ("CI shape" below), which drives one
window, one keystroke and one rendered frame.

## Machine-surface schemas

The committed JSON Schema documents (the CLI bundle, [cli.md](cli.md) "JSON Schema"; the config schema; the JSON
protocol, [json-protocol.md](json-protocol.md)) are generated from the serde types under each crate's `schema` feature.
`just schema` rewrites them; without `UPDATE_SCHEMA` the same tests assert byte equality, and CI's `--all-features` run
is where that assertion fires. No published JSON protocol object may forbid additional properties, since a closed object
would make an optional field a later writer of the same version adds a break; a test enforces it.

The CLI bundle is validated against the objects the CLI test suites really parse, and must reject the fixtures under
`crates/felis-cli/tests/fixtures/schema-invalid/`. The golden JSON protocol messages carry what a schema cannot state;
`just golden` regenerates them from a live run.

## Performance benchmarks

Tools: criterion (<https://docs.rs/criterion>) for wall-clock time, and gungraun (<https://docs.rs/gungraun>) for
instruction counts under Valgrind's callgrind.

`tools/bench/criterion.py` discovers every `[[bench]]` target that calls `criterion_main!` from `cargo metadata`, so
adding one to a crate's `Cargo.toml` is all it takes to enter `just bench-all`.

### Running benchmarks

| Task                              | Command                                                                                             |
| --------------------------------- | --------------------------------------------------------------------------------------------------- |
| Headline end-to-end throughput    | `just bench`                                                                                        |
| Run all Criterion targets         | `just bench-all` (`--quick` for fast runs)                                                          |
| Generate Markdown report          | `just bench-report`                                                                                 |
| Check regression against baseline | `just bench --save-baseline before`, modify code, `just bench --baseline before`, `just bench-gate` |
| Instruction-count gate            | `just bench-gate-ir [base]` (base revision, `origin/main` by default)                               |
| Orchestrator and gate tests       | `just test-tools tools/bench`                                                                       |

`just bench-gate` fails if a benchmark's mean wall-clock change exceeds the threshold (15% by default). It has no CI
caller; the pull-request gate is the instruction-count gate below.

### Instruction-count gate

`bench.yml` runs `tools/bench/gungraun.py gate` on every pull request against its base commit. It counts the
instructions (callgrind's `Ir`) of the `gungraun_gate` bench in `felis-client-core`, which shares its workloads with
`end_to_end_throughput`, and:

- fails when any benchmark's `Ir` exceeds the base's by more than 2%;
- passes a regression when the pull request carries the `perf-regression-accepted` label, by convention with a PR
  comment stating why the cost is accepted;
- fails when the bench run breaks or a benchmark the base ran is missing from the head run, label or not;
- skips when the base revision has no `gungraun_gate` bench.

The gate runs on Linux only. The dev shell provides `valgrind` and `gungraun-runner`, and the runner must be the exact
version of the `gungraun` crate in `Cargo.lock`: `dev/packages/gungraun-runner.nix` pins it, and evaluating the dev
shell fails when the two disagree.

## Damage-tracking correctness

Source: [damage-tracking.md](../explanation/rendering/damage-tracking.md).

The invariant is **no underdraw**: every cell whose visible content changed must reach the client next paint. Overdraw
is permitted. Three harnesses hold it, one per layer:

- **Grid**: `crates/felis-grid/tests/it/damage_correctness.rs` applies the queued scroll directives to the prior screen
  and requires every row outside the dirty set to equal the grid's.
- **Daemon to client**: a property test in `crates/felis-daemon/src/serve/tests.rs` requires every subscriber's shadow
  screen to hold the grid's cells, cursor, and soft-wrap bits after each compose.
- **Renderer**: property tests in `crates/felis-render-wgpu/src/row_cache.rs` and
  `crates/felis-render-core/src/glyphs/walk_tests.rs` require incremental frames and the populate walk to equal a full
  rebuild.

## Security tests

The threats these tests verify, and the property each defense must hold, are
[security-model.md](../explanation/security-model.md); the standing audits are [security-audits.md](security-audits.md).

## Kani proofs

Tool: Kani (<https://model-checking.github.io/kani/>).

Kani model-checks self-contained SWAR and arithmetic kernels (UTF-8 and LEB128 decoding, the printable-run scan, CSI
parameter accumulation, frame headers, base64, placeholder and geometry arithmetic) for absence of panics and assertion
violations across bounded inputs. Each proof is a `#[kani::proof]` harness under `#[cfg(kani)]` beside its kernel.

```
just kani                     # run all proofs in the Kani dev shell
just kani --harness <name>    # run a specific proof
just kani-update              # bump dev/packages/kani/default.nix to the latest Kani release
```

`.forgejo/workflows/kani.yml` reruns every proof weekly, and on a PR that changes `dev/packages/kani/`.

## Mutation testing

Tool: cargo-mutants (<https://mutants.rs/>).

cargo-mutants mutates the production code and reruns the suite: a mutant no test fails on is a branch nothing pins, or a
test whose oracle is too weak. `.cargo/mutants.toml` scopes the sweep to `felis-grid`, `felis-vt` and `felis-protocol`,
routes it through nextest under the `mutants` cargo profile, and excludes `kani_proofs` and the committed prost codegen.

| Task                        | Command                  |
| --------------------------- | ------------------------ |
| Scoped sweep                | `just mutants`           |
| Mutants touched by the diff | `just mutants-pr [base]` |
| One shard of the full sweep | `just mutants-shard 0/8` |

`-F <regex>` narrows a run to one function or file. Runs are periodic and on demand; no workflow gates on the result,
and the triage procedure is in the `test-strategy` skill.

## Contributor environment variables

These variables steer tests and harnesses in a checkout. None is part of the product surface, and no compatibility
promise covers any of them (the runtime environment a user may set is [terminal-identity.md](terminal-identity.md)). The
dev shell already exports the ones a normal `just check` needs. The knobs of `#[ignore]`d measurement and diagnostic
probes, and of the bench scripts, are documented where they are read.

| Variable                    | Effect                                                                                                                                                                                                                             |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `FELIS_TEST_FONT_DIR`       | Directory holding the pinned test fonts. Font-dependent tests load their faces from it alone and panic when it is unset; they are compiled out on Windows, whose CI runs outside the dev shell. `dev/flake-module.nix` exports it. |
| `FELIS_SMOKE_MARKER`        | A non-empty value puts the client in frontend-smoke mode: type the marker, read it back from the rendered frame, exit. `just smoke` and `just smoke-headless` set it.                                                              |
| `UPDATE_SCHEMA`             | Rewrite the committed JSON schemas instead of asserting byte equality. `just schema` sets it.                                                                                                                                      |
| `UPDATE_GOLDEN`             | Rewrite the golden JSON protocol messages and cell-instance fixtures from a live run. `just golden` sets it.                                                                                                                       |
| `VTTEST_BIN`, `ESCTEST_BIN` | Absolute path to the conformance binary, which must exist; without it the harness resolves the name on `PATH`.                                                                                                                     |
| `FELIS_SYSTEMD_TESTS`       | `1` runs the systemd hand-off tests against the live user manager (a transient unit per test, on its own socket path); without it they return at once.                                                                             |
| `FELIS_PROSE_CHECK_SKIP`    | `1` makes the prose gate exit zero, for a commit whose prose is checked another way.                                                                                                                                               |

## CI shape

CI runs on Forgejo Actions (<https://forgejo.org/docs/latest/user/actions/>) with configuration under
`.forgejo/workflows/`. The one workflow under `.github/workflows/`, `release-mirror.yml`, gates nothing: it mirrors a
published Forgejo release to GitHub, crates.io, and the Homebrew tap ([workspace.md](workspace.md) "Release gate").

### Workflows

| Workflow      | Triggers                  | Purpose                                                                                                        |
| ------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `pr.yml`      | Push, PR                  | `nix flake check`, the Cargo suite (clippy, nextest, `just deny`, tool tests), the wire check, Linux smoke.    |
| `windows.yml` | Push, PR                  | Windows clippy, nextest, headless smoke, and package artifact.                                                 |
| `darwin.yml`  | Push, PR                  | aarch64-darwin package and release-archive build, with the archive's manifest, signature, and smoke checks.    |
| `fuzz.yml`    | Push, PR, Nightly, Manual | Fuzz smoke on push and PR; the nightly long run with corpus minification.                                      |
| `kani.yml`    | Weekly, PR, Manual        | Every Kani proof; on a PR, only when `dev/packages/kani/` changes.                                             |
| `bench.yml`   | PR, PR label change       | Instruction-count regression gate against the PR's base commit.                                                |
| `release.yml` | Tag (`v*`)                | The release gate on the tag's revision, then the release assets ([workspace.md](workspace.md) "Release gate"). |

### Frontend smoke test

The Linux and Windows pipelines run a client smoke test: the client connects to a daemon, types an input marker,
verifies terminal echo, and reads back the rendered GPU frame. On Linux (`just smoke-headless`) it runs under Xvfb on
software Vulkan (lavapipe), where the job also runs `just test-gpu`, the renderer tests that need a real adapter. A pull
request drives the debug Cargo build; `main` and tags drive the shipped build.

### Wire compatibility gates

Changes to `felis.proto` must preserve wire compatibility, verified by seven checks:

| Gate                                 | Runs in                                             | Guarantees                                                                                                        |
| ------------------------------------ | --------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `buf breaking` (`WIRE_JSON`)         | CI wire check, `just proto-compat`, pre-commit hook | Field numbers, types, oneof membership, and enum names stay compatible; field options are outside its comparison. |
| Release baseline                     | CI wire check on a tag                              | A tag is measured against the `felis.proto` of the newest published final release; an unreadable answer fails.    |
| Minor-ledger sync test               | CI Cargo suite, `just test`                         | [ipc.md](ipc.md#the-minor-ledger)'s ledger and the schema agree on what each minor introduced.                    |
| Send-gate tests                      | CI Cargo suite, `just test`                         | `FrameWriter` refuses an addition from a minor the connection does not define, and survives the refusal.          |
| Correlation-gate tests               | CI Cargo suite, `just test`                         | An arm goes out only with the envelope and id kind its correlation class requires.                                |
| Row-codec and carrier golden vectors | CI Cargo suite, `just test`                         | Opaque `packed_cells` and preface byte representations stay fixed ([row-codec.md](row-codec.md)).                 |
| Schema metadata generation           | `schema-meta` pre-commit hook, `just proto`         | The committed `src/schema_meta.rs` is what `tools/proto/schema_meta.py` generates from the schema's options.      |

Pre-release breaking changes are acknowledged in `crates/felis-protocol/proto/BREAKING.md` by recording the base commit
SHA (`base: <sha>`).
