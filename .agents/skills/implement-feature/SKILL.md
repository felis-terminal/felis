---
name: implement-feature
description:
  The front-door workflow for landing any change in felis — classify the task, read the owning docs first, pick the
  crate by the dependency direction, run the right gates (just check, schema/contract regeneration), then cascade the
  docs. Use when starting an issue, bug fix, feature, or refactor; when unsure which crate owns a change, which design
  doc governs it, or which checks must pass before committing. Routes to principle-check, test-strategy, doc-cascade,
  add-config-key, extend-ipc, and add-escape-sequence for the specialized steps.
allowed-tools: Read Grep Bash(just:*) Bash(cargo:*) Bash(nix:*) Bash(git:*) Edit Write
---

# Implementing a change in felis

Docs first, code second: the design docs are the source of truth for every feature decision, and most implementation
questions ("where does this live?", "was this already rejected?") are answered there faster than by reading code.

## 0. Classify the task and route

| Task shape                                             | Use                                                                                                 |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| New feature / scope change / "should felis support X?" | `principle-check` skill **first** — before any code or doc work                                     |
| Adding or changing a config key                        | `add-config-key` skill                                                                              |
| New IPC message, `felis sessions` verb, or CLI surface | `extend-ipc` skill (a new verb picks its Point/Stream/Exempt class first)                           |
| New escape-sequence / protocol behavior                | `add-escape-sequence` skill                                                                         |
| Choosing what test/proof to write                      | `test-strategy` skill                                                                               |
| Throughput / performance work                          | `perf-trace` skill                                                                                  |
| Rendering or Kitty-protocol debugging                  | `producer-traffic-debug` (producer traffic / logs) / `felis-macos-gui-debug` (macOS pixels & input) |
| Doc-only change                                        | `doc-cascade` skill                                                                                 |

Anything else (plain bug fix, refactor, test work): continue below.

## 1. Read the owning docs before writing code

The reading order is `AGENTS.md` "Reading order before editing anything" and the crate map is
`docs/reference/workspace.md` "Crate map"; follow them rather than reading code first. Two things the map does not say:

- Open work is tracked in Forgejo issues; if the task is there, it usually states the design constraints and blockers
  already.
- Where a subject has both reference facts and an explanation doc (protocols, IPC, testing), read both. A rejected
  alternative you did not know about is the most expensive thing to rediscover in review; for protocol work the verdict
  is in `docs/reference/protocols/support-matrix.md` and `docs/explanation/protocols/landscape.md` holds the rationale.

## 2. Pick the crate

The dependency direction is one-way (`docs/reference/workspace.md` "Dependency direction"); a change that needs an arrow
reversed is in the wrong crate. Two review-enforced rules beyond that:

- A new crate is a new repo-split seam: argue it against `docs/explanation/architecture/overview.md` "Workspace: the
  crate-boundary decision record", whose "Rejected workspace shapes" rule out `felis-test-*` helper crates. The one
  member outside `crates/` is `tests/`, which hosts guards on workspace-wide invariants and takes no new kinds of test.
- The daemon owns state, the client owns presentation (principle 3): a knob that changes _pixels_ belongs client-side.

## 3. Dev environment

`nix develop` (or direnv) is assumed by everything below because it provides the nightly toolchain, runtime libs for
wgpu/winit, the `xterm-felis` terminfo (`TERMINFO_DIRS`), prepends `./target/debug` to `PATH`, and installs the
pre-commit hook. Special shells: `.#msrv` (stable check at the `Cargo.toml` `rust-version`), `.#windows` (cross),
`.#kani` (model checking, x86_64-linux).

Run it locally: `cargo build` first, since `cargo run -p felis-client` builds only the client and the client auto-spawns
the `felis-daemon` binary beside it in `target/debug` (falling back to `PATH`); `felis sessions spawn/send/capture` (see
the `felis` skill) drives a session headlessly, against a private daemon from the `isolated-daemon` skill whenever the
run must not touch the user's sessions; `FELIS_STARTUP_EXIT_MS=<n>` makes the GUI client exit on its own for automated
runs. An auto-spawned daemon's stderr goes to `/dev/null` (or the journal when systemd starts it); its log lines are
teed to `daemon.log` (`docs/reference/cli.md` "Log files"; `RUST_LOG` respected, default `info,felis_daemon=debug`).

## 4. Implement and verify

- Choose the test layer with the `test-strategy` skill (unit/proptest vs insta vs fuzz vs Kani); add tests in the
  established style.
- Comments: `doc-prose` "Code comments". Default to none; `just prose-check` rejects blocks over five lines and history
  phrasing.
- Fast loop: `cargo nextest run -p <crate>`; full local gate before commit: `just check`, whose contents are listed in
  `CONTRIBUTING.md` "Development environment"; it mirrors CI (`.forgejo/workflows/pr.yml`). The `proto-compat` half
  needs `origin/main` fetched (it compares `felis.proto` with the merge-base; `just proto-compat <rev>` names a base
  explicitly).
- Generated artifacts must be regenerated, never hand-edited:
  - config structs, CLI `--format` output, bridge grammar, or felis-json types changed → `just schema` and commit every
    rewritten `*.schema.json` (`crates/felis-client-core/felis-config.schema.json`, `crates/felis-cli/schemas/`,
    `crates/felis-grid/schemas/`); a test fails on staleness under `--all-features`.
  - bridge conversations or felis-json frames changed → `just golden`.
  - `felis.proto` changed → `just proto` (the pre-commit hook fails on a stale generated copy); see the `extend-ipc`
    skill.
- Nontrivial behavior changes: drive the real binary once (step 3), not just the tests.

## 5. Docs and commit

- Run the `doc-cascade` skill: update the reference doc, grep the changed term across `docs/`. Record a decision in the
  owning explanation doc only when it passes doc-cascade's "Default to no record" gate; everything else about _why_ goes
  in the commit body.
- Commit per `CONTRIBUTING.md` (Scoped Commits, body explains the _why_). One logical change per commit.

## Trip hazards

- The default dev shell is **nightly** (cargo-fuzz needs it) but the build must stay on the MSRV (`Cargo.toml`
  `rust-version`): do not use nightly-only features; `just test-msrv` is the local check for the CI gate.
- `--all-features` is not optional in test runs: the schema guard only runs under it (why `just test` passes it).
- `.pre-commit-config.yaml` is a Nix-store symlink managed by git-hooks.nix (never edit it; the hooks live in
  `dev/flake-module.nix`).
- New files must be git-tracked (at least staged) before any flake-mediated command (`just kani`, `nix build`) can see
  them.
- The GPU path cannot run headless; renderer changes need a real session (see producer-traffic-debug /
  felis-macos-gui-debug).
- Windows compiles and lints locally through the mingw cross pass (`just check-windows`); ConPTY / named-pipe behavior
  runs only on CI's `x86_64-windows` runner, never on this machine.
