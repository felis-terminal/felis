---
name: test-strategy
description:
  Pick the right verification layer for felis — unit/proptest, fuzz, Kani bounded model-checking, or insta snapshots —
  and add the test/proof in the established style. Use when deciding what test to write for a function, when asked to
  "verify" / "prove" / "model-check" / "add a proof" / "is this worth a Kani harness", when weighing proptest vs Kani vs
  fuzz, when adding or running a Kani proof, or when reviewing whether a function is over- or under-tested. Covers the
  kernel-selection heuristic, the `#[cfg(kani)]` recipe, `just kani`, and — just as important — what NOT to verify with
  each tool.
compatibility:
  Kani layer is x86_64-linux only (see dev/packages/kani.nix, the packaging record). proptest / fuzz / insta run
  everywhere the dev shell does.
allowed-tools:
  Read Edit Bash(just kani:*) Bash(just test:*) Bash(just fuzz:*) Bash(cargo nextest:*) Bash(cargo kani:*) Bash(git
  log:*) Grep
---

# felis verification strategy

Use this when choosing how to test a function, when asked to "verify" / "prove" / "model-check" something, or when
adding a Kani proof. The governing decision is recorded in
[docs/explanation/testing.md "Which functions get a Kani proof"](../../../docs/explanation/testing.md), and
[docs/reference/testing.md "Kani proof inventory"](../../../docs/reference/testing.md) lists the proofs; this skill is
the operational how-to.

## The four layers and who owns what

felis is defense-in-depth, not pick-one. The layers cover _different_ parts of the input space at _different_ cadence:
they are complementary, not redundant.

| Layer               | Cadence / reach                  | Owns                                                                                                                              | Tool                                                   |
| ------------------- | -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------ |
| **unit + proptest** | every commit, every platform, ms | always-on regression tripwire; **unbounded** random inputs; readable spec                                                         | `cargo nextest` (`just test`)                          |
| **insta snapshots** | every commit                     | golden/behavioral output — conformance tables, SGR round-trips, dispatch wiring; catches _behavior_ drift a property can't phrase | `cargo insta` (review with `cargo insta review`)       |
| **fuzz**            | periodic, coverage-guided        | **unbounded-length** totality / no-panic of orchestration (the whole parser), deep paths                                          | `cargo fuzz` (`just fuzz <target>`, `just fuzz-smoke`) |
| **Kani**            | periodic, x86_64-linux           | **exhaustive** proof over a **bounded** domain of a small pure kernel                                                             | `cargo kani` (`just kani`)                             |

Key consequence: **Kani never retires a proptest.** proptest is the fast every-platform tripwire over unbounded inputs
and the guard for the window between heavy Kani runs (and for when the Kani package is temporarily unbuildable). Kani
adds a _top_ layer of exhaustiveness over small inputs; it does not subsume the layers below it.

These four cover the _input space_. **cargo-mutants** sits on a perpendicular axis, auditing whether the four layers
actually _bite_: the scope decision is in `docs/explanation/testing.md` and the recipes in `docs/reference/testing.md`
"Mutation testing"; this skill owns the triage (see "Auditing an existing suite" below).

## Is this function a Kani target?

The selection rule is a design decision and lives in `docs/explanation/testing.md` "Which functions get a Kani proof";
apply it, do not restate it. When Kani is not the answer, the right tool is a plain enumerating test (small domain), a
proptest (unbounded random), fuzz (unbounded totality of orchestration), or insta (behavioral output).

## Adding a Kani proof

1. **Place it in a `#[cfg(kani)] mod kani_proofs` in the same file as the target**, so it can read the target's private
   fields/fns (a child module sees the parent's privates). Put crate-root targets in
   `crates/felis-vt/src/kani_proofs.rs`.
2. **Gate everything `#[cfg(kani)]`**: normal builds, `cargo test`, and clippy never compile it. `cfg(kani)` is already
   registered in `[workspace.lints.rust]` (`check-cfg`), so it does not trip `unexpected_cfgs` under `-D warnings`. No
   per-crate change needed.
3. **Feed `kani::any()`**; bound arrays small (`[u8; 8]`, `[u8; 16]`). `kani::assume(...)` to constrain. Prove either an
   **equivalence** to a hand-written reference, an **invariant** (`assert!`), or just **no-panic** (call it and discard;
   Kani checks panics/overflow/OOB automatically). For string parsers, prove a byte-level kernel, not the `&str` front
   door: UTF-8 validation plus str-search primitives blow the SAT formula up past solver limits even for 15-byte inputs
   (the osc_color proof only converged after extracting `parse_x_color_bytes`).
4. **`#[kani::unwind(N)]`** must exceed the longest loop's trip count (including loops inside callees like
   `str::from_utf8`'s validation (~bytes) and slice `binary_search` (~log2 n)). If insufficient, Kani fails the run with
   an unwinding assertion; bump N. A passing run with unwinding assertions green means N is provably sufficient (sound).
   Derived/slice equality lowers to the builtin `memcmp`, whose loop sits OUTSIDE `#[kani::unwind]`'s reach; compare
   struct fields and slice elements with an explicit indexed loop instead.
5. **Run** `just kani` (verifies felis-vt + felis-protocol + felis-grid + felis-client-core, the last with
   `--no-default-features`; pass `--harness <name>` for one). x86_64-linux only.
6. **Commit** with the crate as scope (`vt: prove ...`), the body naming the property and what it guards.

```rust
// crates/<crate>/src/<file>.rs
#[cfg(kani)]
mod kani_proofs {
    use super::the_target;

    #[kani::proof]
    #[kani::unwind(17)]
    fn the_target_matches_its_reference() {
        let x: [u8; 16] = kani::any();
        let reference = /* simple oracle */;
        assert_eq!(the_target(&x), reference);
    }
}
```

## Reading Kani output

- `VERIFICATION:- SUCCESSFUL` per harness + `0 failures` is the goal.
- `Found ... unsupported constructs` (e.g. `Location::caller`, `handle_alloc_error`, `same_allocation`) are almost
  always on paths Kani proves **unreachable**: harmless when the run is `SUCCESSFUL`.
- A failure prints a **concrete counterexample**; re-run with
  `cargo kani --harness <name> -Z concrete-playback --concrete-playback=print` to get the exact input.

## Where the coverage record lives

The docs own the inventory; do not mirror it here:

- `docs/reference/testing.md` "Kani proof inventory": the current proofs, per crate and file.
- `docs/explanation/testing.md`: what is deliberately _not_ verified with Kani, and why.
- Forgejo issues: candidate kernels not yet proven.

One judgment note the inventory implies but does not state: heap-allocating round-trips (e.g. the base64
`encode`/`decode` pair) are **proptest**-owned (docs/explanation/testing.md "Deliberately not verified with Kani") even
when the decode side has Kani proofs; do not "complete" such a pair with a Kani harness.

## Test helpers & fixtures (the established style)

Two helper worlds exist and **cannot share code**: they are separate compilation units:

- **`src/` unit tests** reach a crate-root `#[cfg(test)] mod test_support` (for example, `felis-grid::test_support`) via
  `crate::test_support`.
- **Integration tests** keep their fixtures and helpers self-contained beside their test binaries; do not add helper
  crates.

Duplication _between_ these two worlds is expected, not a smell: a helper needed in both lives once in each. Do **not**
add a dev-only `felis-test-*` crate to bridge them (`docs/explanation/architecture/overview.md` "Rejected workspace
shapes" records why). Duplication _within_ one world is the smell: centralize it.

A test that asserts something about the **workspace** rather than about a crate goes in the `tests/` member, not in
whichever crate is convenient; today that is only the source-reference guard (`tests/doc_source_references.rs`).
Markdown links are `just docs-links` (lychee), and dependency bans are `cargo deny` plus the per-crate purity tests
(`crate_purity`, `no_render_deps`), each of which inspects its own crate's graph. A guard reaching above its own crate
root breaks when that crate is extracted, having never tested the crate hosting it. Tests that exercise one crate
against another are the opposite case and stay in the crate that owns the seam.

Conventions learned from the suites:

- **Canonical `drive` shapes** (felis-grid): `drive(rows, cols, bytes) -> Grid` (one-shot),
  `drive_take(rows, cols, bytes) -> (Grid, Vec<Vec<u8>>)` (one-shot + queued host responses),
  `drive_with(parser, grid, bytes)` (caller owns state across several bursts). Argument order is **always
  `(parser, grid, bytes)`**; `src/test_support::drive` matches it. Never invent a new order; the type checker catches a
  flip, but a reader shouldn't have to rely on that.
- **Independent-oracle rule.** A test helper that re-implements a production codec (the base64 `b64`/`naive_base64_*`
  encoders, the DECRQM reply parser) is deliberately _independent_ of production: a round-trip test must check
  production against a hand-written reference, never against itself. Consolidate duplicate _oracles_ into one per world,
  but never collapse an oracle into the production function it is meant to cross-check.
- **Drift is a latent gap.** A copied helper that falls behind (a `fmt_flags` missing the CONCEAL/OVERLINE letters its
  canonical twin emits) silently stops catching regressions in the missing dimension. When you find one, centralize
  rather than patch the copy.

## Auditing an existing suite

When sweeping for "redundant / missing" tests, **verify each claim against the code before acting**, because a survey
(human or agent) over-flags in both directions, and acting blind is how a cleanup introduces churn or deletes real
coverage:

- A flagged **gap** is often already covered elsewhere, such as by a test the survey didn't associate with the branch (a
  proptest, a codec suite in another crate, one test that walks several mirrors at once). Expect most flagged gaps to
  dissolve on inspection.
- A flagged **duplication** is often intentional: independent codec oracles (see above), purposeful test ergonomics (the
  `encode_no_app_cursor` / `encode_mok` wrappers each name a default-set and keep ~100 input tests readable; a "builder
  fixture" would be more code, less intent), or a split Rust's coherence rules force (you cannot add inherent methods to
  a shared fixture struct from another test crate, so per-file `State`/ `Harness` types with their own methods are
  correct, not duplicated).

Net: confirm-then-edit. The genuinely valuable finds are usually _drift_ (a copy fallen behind its twin) and _real_
missing-branch coverage, not the headline counts.

### Mutation testing (cargo-mutants)

The mechanical version of the audit above. It mutates the production code (`<`→`<=`, `+`→`*`, `&&`→`||`, delete a match
arm, replace a body with a default) and reruns the suite: a mutant the suite **catches** (some test fails) is "killed";
a mutant that **survives** (all tests still pass) is a hole: a branch no test pins, or a test whose oracle is too weak.
It grades the tests, not the code, so it finds gaps that clean lints and high test _counts_ hide. Run with
`just mutants` (scoped to grid / vt / protocol and routed through nextest by `.cargo/mutants.toml`, built under the
`mutants` cargo profile with four parallel jobs); standard workflow is `just mutants-pr` for the branch diff and
`just mutants-shard 0/8` (every shard) for a full sweep. Narrow to one function with `just mutants -F 'Grid::decsel'`.
`-F` is a regex over the full `--list` line (path included), so a _filename_ narrows to one file: `-F 'machine\.rs'`. Do
**not** reach for `--file`/`-f` to scope by file, because the `examine_globs` in `.cargo/mutants.toml` unions with
`--file`, so `-f <path>` silently widens back to the whole configured sweep instead of narrowing. `-F` is the only
filter that narrows here.

- **Cadence: periodic / on-demand, never a per-commit gate.** One build+test per mutant makes a full crate run
  minutes-to-hours, and the score never reaches 100% (see equivalent mutants). Treat survivors as a worklist, like
  fuzz/Kani findings, not a CI failure. Scoped to the pure boundary-logic crates on purpose; the async / GPU / OS-FFI
  crates pay poorly (slow suites, headless GPU can't run, equivalent-mutant noise).
- **Two failure modes it surfaces.** _Missing coverage_: a whole match arm is deletable with nothing failing. _Weak
  oracle_: a test that asserts a side condition (a protected cell survives) and never the main effect (the unprotected
  cells were blanked).
- **A third failure mode: dead code.** A survivor on an accessor often means nothing calls it. Check for callers before
  writing a test; the fix for an uncalled twin is deletion, and a test written to kill that mutant pins API nobody
  wanted.
- **Keep the workspace suite in play.** cargo-mutants runs the whole workspace per mutant by default;
  `--test-workspace false` is faster but reports as survivors every mutant whose only coverage lives in another crate (a
  `felis-grid` sweep policy driven from the daemon's pool and the client's shadow, say). Narrow with `-F`, not by
  dropping test scope.
- **Triage equivalent mutants.** Some survivors are behaviorally identical and _cannot_ be killed; for example,
  `PROTECTED | ISO_PROTECTED` → `^` is identical when the two flag bits are disjoint. Don't chase them; don't
  `#[mutants::skip]` the whole function to hide one (that drops coverage of its real mutants). Recognize and move on.
- **Turn it around to grade the _suite_: which tests are redundant.** By default nextest stops the run at the first
  failure, so a mutant's log names only some of its killers. `--cargo-test-arg --no-fail-fast` (plus
  `--cargo-test-arg --status-level=fail --cargo-test-arg --failure-output=never`, or the logs run to gigabytes) makes
  each `mutants.out/log/*.log` list every test that kills that mutant. A test is a deletion _candidate_ only when no
  mutant it kills dies by it alone; confirm the candidate against the code before acting (see "Auditing an existing
  suite"). Budget wall-clock: the full felis-grid sweep is ~5 h this way on a twelve-core host, felis-vt ~25 min,
  felis-protocol ~30 min.
- **Killing nothing does not mean useless.** Mutation testing cannot grade a guard over something it does not mutate:
  `crate_purity`, `no_render_deps`, the `*_cases_cover_every_variant` exhaustiveness tests, a `size_of` pin, a table
  cross-checked against `unicode-width`, a doc/schema sync check, or an early `return Err` cargo-mutants finds unviable.
  Read the test before believing its zero.
- **Scoping a sweep to one crate needs `--no-config`.** `-F 'crates/felis-vt/'` narrows the mutant list, but the
  config's `additional_cargo_args` still passes `--features felis-grid/json` to a `--package=felis-vt` test run, which
  cargo rejects outright, so the baseline fails before any mutant runs. Re-state the settings on the command line
  (`--no-config --test-tool nextest --profile mutants --exclude-re kani_proofs`) for a single-crate sweep.
- **Close the loop.** Fix a real survivor by adding/strengthening a test whose assertion the mutated code would fail,
  then rerun scoped (`just mutants -F '<fn>'`) and confirm it is now caught.
- **Watch the mtime trap when mutating by hand.** Restoring a file via `mv backup orig` keeps the backup's old mtime, so
  cargo may skip the rebuild and test a stale (still-mutated) binary. `touch` the file after a manual revert;
  cargo-mutants itself manages builds correctly.

## Cleanup / gotchas

- New `.rs` files with `#[cfg(kani)]` proofs must be **git-tracked/staged** before `nix develop`/`just kani`, because
  flakes only see tracked files.
- The Kani toolchain is pinned (`dev/packages/kani.nix`); if it breaks, the lower layers still guard everything. Don't
  make Kani the _only_ coverage of a kernel.
