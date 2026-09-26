---
title: Testing strategy
sidebar:
  order: 10
---

How felis verifies correctness, conformance, robustness, and performance, and why the verification stack has the layers
it has. The layer inventory itself (targets, acceptance bars, commands, CI wiring, the Kani proof inventory) is the
reference twin, [reference/testing.md](../reference/testing.md); this page owns the strategy and the decision record.

## Why the layers exist

No test layer exists on taste: each is grounded in either an existing design-doc invariant or an upstream specification
/ tool whose contract felis's tests check against. The stack runs from broad-and-sampled to narrow-and-exhaustive; each
layer exists because the one above it cannot make its guarantee, and which layer is which is the reference twin's
inventory ([reference/testing.md](../reference/testing.md) "Layer overview"). Above all of them sit the principle
invariants: the "Test:" lines in [principles.md](principles.md) are the highest-level test specification, and everything
below must preserve them.

Two of the layers are chosen for a reason that inventory cannot carry. Property tests earn the parser surfaces through
proptest's shrink behavior, which reduces a 4 KiB pathological sequence to a 7-byte trigger. Fuzzing attacks the same
surfaces with hostile rather than strategy-shaped bytes, and its acceptance bar (a clean 24-hour run) is set by
[security-model.md](security-model.md) "Parser robustness", because the parser, the Kitty graphics command parser, and
the IPC frame decoder all accept attacker-shaped input.

One tool sits on an axis perpendicular to all of them. cargo-mutants covers no new inputs; it mutates the production
code and asks whether the suite notices, which grades the tests rather than the code. That makes it a meta-tool and not
a fifth layer, and it stays a periodic worklist rather than a gate: one build-and-test per mutant makes a sweep cost
hours, and the score cannot reach 100% because some mutants are behaviorally identical to the code they replace. A
survivor is a question about one test, never a number to chase. The sweep is scoped to the boundary-logic crates because
the async, GPU and OS-FFI crates pay poorly: slow suites, a GPU that cannot run headless, and equivalent-mutant noise.

One layer the inventory marks out of scope is out for a decided reason. Pixel-level visual regression is not attempted:
a pixel snapshot is brittle across OS font-rasterization changes, and the damage harness
([reference/testing.md](../reference/testing.md#damage-tracking-correctness)) already enforces pixel-equivalent output
by diffing against a full redraw. _Revisit if_ cell-level snapshots plus the damage diff miss a renderer regression.

## Peer conformance practices

The choices below come from how the peer terminals (wezterm, alacritty, ghostty, foot, xterm, contour, kitty, Mintty,
iTerm2) verify their escape-sequence layer.

**Adopted**

- **esctest2 as a CI ratchet** ([reference/esctest-compatibility.md](../reference/esctest-compatibility.md)). Among the
  peers, only xterm's and iTerm2's maintainers run it at all, both manually, outside CI (iTerm2's maintainer authored
  it). felis is alone in gating on it, which is also why the ratchet is one-way.
- **vttest as hand-picked scenario replays, not a gate** (`vttest_smoke.rs`, 27 scenarios). Both kitty's and wezterm's
  maintainers judge vttest human-required on record
  ([kitty #6331](https://github.com/kovidgoyal/kitty/discussions/6331),
  [wezterm #133](https://github.com/wez/wezterm/issues/133)), and the peer in-process unit cultures grew partly to
  replace it, so felis does not gate on what it cannot automate honestly.
- **Layered fuzz targets on crate boundaries** (`just fuzz*`), after ghostty's three-target AFL++ shape.
- **Recording-based RefTest fixtures**, after alacritty's shape (`crates/felis-grid/tests/ref_recordings.rs` replays
  `crates/felis-grid/tests/ref/<scenario>/` `recording.bin` + `size.json` into a `Parser` + `Grid` and pins the screen
  with insta): anything a human can demonstrate interactively becomes a deterministic regression. Alacritty's
  `config.json` is dropped (the felis vt layer is configless by design) and its bespoke `expected.snap` format is
  replaced by the workspace's existing insta review flow. The repository ships no tool that captures a new scenario.

**Rejected**

- **Visual screenshot-diff harnesses** (xterm's eyeball `vttests/`, reference-image suites). The human-inspection
  dependency is exactly what makes vttest ungateable; recording fixtures cover the same ground with byte comparison.
- **Inline-snapshot macros for grids** (contour's Catch2 fleet, wezterm's `k9::snapshot!`). Inline literal updates break
  down past a few lines of grid state; insta's reviewed files are the fit.
- **One-test-per-CSI unit files** (iTerm2's per-parser-layer XCTest split). The external ratchet scales where per-CSI
  ceremony does not; the marginal value over the existing hand-written grid tests is small.
- **An encode → parse → encode property over every CSI/OSC handler.** Mooted by the code's shape: the VT layer is
  decode-only, and the two response encoders that do exist (Kitty graphics, text sizing) already carry parse-back
  proptests (`proptest_kitty_graphics.rs`, `proptest_kitty_text_sizing.rs`).

## Why sampling is not enough for the SWAR kernels

Where proptest samples a strategy and fuzzing samples an input space, Kani proves the absence of panics and the truth of
`assert!`s for _all_ inputs within a bounded range. felis uses it for small, self-contained SWAR / arithmetic kernels
where a single off-by-one in a bitmask or a wrapping add is the whole bug: the parser hot paths and the protocol
wire-format encoders/decoders.

`felis-vt` is the trust root for every byte that arrives from a child program: a panic or a wrong answer in the parser
corrupts grid state for attacker-controlled input (see the preamble of `crates/felis-vt/tests/proptest_parser.rs`). The
`State::Ground` hot path coalesces printable runs with hand-written SWAR (SIMD-within-a-register) bit tricks
(`scan_printable_run`, `scan_mixed_print_run`, and the masks that drive them), built on the classic `hasless`/`haszero`
subtract-and-mask predicates from Bit Twiddling Hacks (<https://graphics.stanford.edu/~seander/bithacks.html>), whose
correctness is alignment- and borrow-sensitive across lanes.

Randomized testing samples that space poorly: the proptest `scan_printable_run_matches_scalar` only ever places **one**
offending byte in printable filler, so it never exercises borrow interaction between two non-printable bytes. The Kani
proof against `nonprintable_mask` disproves the tempting invariant ("`0x80` set in _every_ non-printable byte"): the
counterexample `[0x7F, 0x7E, 0xFF×6]` shows a low `0x7F` (DEL) borrowing into lane 1 and falsely flagging the printable
`0x7E`. The code is correct (only the _lowest_ set lane is read, via `trailing_zeros()`), but that contract is wrong,
and no sampling layer catches it.

The SWAR scanners are therefore proven equivalent to their scalar references for every input up to the bounded window,
while the proptest stays as the fast always-on check. The live proof list is the
[Kani proof inventory](../reference/testing.md#kani-proof-inventory) in the reference twin.

The layer reaches x86_64-linux only, from a dedicated `.#kani` dev shell: Kani's compiler pins one exact nightly and
borrows its precompiled sysroot from an upstream release tarball, and only that platform's tarball is wired up. The
packaging record is `dev/packages/kani.nix`, the file it governs.

Because `cfg(kani)` code compiles under no other layer (not under `cargo test`, not under clippy), a kernel edit can
invalidate a proof silently. The proofs are therefore re-checked by a weekly scheduled CI job plus manual dispatch
(`kani.yml`), not per PR: CBMC's runtime would dominate the per-PR pipeline, and nightly buys nothing over weekly for a
proof set that moves far more slowly than the parser it guards.

_Revisit if_ the inventory grows enough that a full run outgrows the weekly window (then split per-crate jobs or gate by
changed paths).

## Which functions get a Kani proof

Kani fits a kernel: the computational core of an algorithm with the orchestration stripped away. A function is a
candidate only when all of these hold:

- **Pure**: the output depends on the inputs and a small bounded state, with no I/O, no global mutation, and ideally no
  allocation.
- **Narrow interface**: bytes, integers, or a `char` in; a value or an `Option` out, so `kani::any()` can feed it
  directly.
- **Dense computation**: arithmetic, bit-twiddling, table lookups, fixed-size buffer work, a codec. One wrong bit or
  branch is a real bug that sampling misses (the `nonprintable_mask` borrow case above).
- **Bounded loops**, so CBMC can unwind them exhaustively.
- **A domain a plain test cannot enumerate.** `bidi::is_override` iterates all of `0..=char::MAX` and the placeholder
  rank tables enumerate every rank; a loop that covers the domain already proves it, and a model checker adds nothing.

The rejected shape is proving orchestration: large state times branching, unbounded input length, code that drives a
sink or mutates buffers. Fuzzing owns that (the next section), and Kani does not retire a proptest either way: the
proptest is the every-platform tripwire and the guard for the window between weekly Kani runs, so a kernel never has
Kani as its only coverage. Dense work routed through an external crate (`serde`, `prost`, `miniz_oxide`) is out as well:
the generic blowup exceeds solver limits, and the fuzz targets own those round-trips.

## Deliberately not verified with Kani

- **Whole-parser totality.** Verifying `Parser::advance` over arbitrary input would force a bounded
  `#[kani::unwind(n)]`, making it _weaker_ than the unbounded `vt_parser` fuzz target for length coverage, while the
  branchy DFA makes CBMC blow up. Full-parser totality stays with fuzzing.
- **Heap-allocating round-trips.** The base64 `decode(encode(x)) == x` property bit-blasts past every solver tried; it
  lives as a proptest instead, and only the two cheap base64 safety proofs stay under Kani.
- **The allocating half of `Chord::parse`.** Its byte-level kernels are proven: the `+` split, the name lookups, and the
  F-key index accumulator. The `String`-building layer above them (the character key, the token named in each error) is
  orchestration whose only computation is the kernels', so the `chord_parser` fuzz target and the parser proptests own
  it.
- **I/O-bound and state-machine code**, where the bound explosion outweighs the payoff over the proptest/fuzz layers.

## Performance regression gating

How Criterion benchmark targets are run, what they pin, and what fails the gate is the reference twin's job
([reference/testing.md](../reference/testing.md#performance-benchmarks)); this section records why the regression gate
is shaped the way it is.

- **Python, not shell.** `tools/bench/criterion.py` is stdlib-only Python (`python3` pinned in the dev shell) rather
  than shell: every input and output is JSON (`cargo metadata`, Criterion's `estimates.json` / `benchmark.json`), and a
  shell version needs jq + awk for a fraction of the job.
- **A pinned test font.** Font-dependent benches load the devshell-pinned probe font rather than a system probe, the
  same reasoning as the shaping feature tests (`dev/flake-module.nix` `testFonts`): a system probe makes the numbers
  machine-dependent (a different face is different GSUB/rasterization work, so two hosts' baselines never compare) and
  panics on a font-less CI runner.
- **The regression gate measures change against baseline, not other terminals.** `bench.yml` gates pull requests on
  `just bench-gate`, failing if the mean change against the PR's base commit exceeds the threshold
  ([reference/testing.md](../reference/testing.md) "Running benchmarks"). The gate answers strictly whether felis has
  regressed against its own previous commits on identical hardware. Comparative performance evaluation against peer
  terminals is a local report rather than a gate, and lives in [benchmarks.md](benchmarks.md).

## Shell overlays under the real interpreters

The completion tests drive the generated fish and zsh overlays through the interpreter itself, against a `felis`
stand-in that records its argv. The overlays (`crates/felis-cli/src/cli_completions.rs`) are shell script held in Rust
strings, and the contract they carry is about what the shell does on a `<TAB>`: which tokens reach the hidden
`__complete-sessions` helper, and which lines yield no call at all because the slot's daemon is an SSH destination.

A string assertion on the overlay ("contains `--socket)`", "does not contain `--host|--socket`") pins the text, not that
behavior. A `case` arm can match the right token and still forward it through a variable the assertion never sees, and a
quoting slip breaks the function without changing any substring. The string assertions stay alongside as the tripwire
that runs without either shell.

The two shells are driven differently. fish exposes its completion engine directly (`complete -C '<line>'` hands the
line to `commandline` as an interactive `<TAB>` would); zsh has no equivalent, so the test sources the script under
`zsh -f`, stubs `compdef` and `_describe`, and calls the helpers with `words` set as the completion system would.

Each test skips rather than fails when its shell is off `PATH`, because a contributor's machine need not carry both
shells; the dev shell does, so CI never takes the skip.

## Why a support claim needs a passed gate

The build matrix separates _committed_ from _supported_, and lets neither word do the other's work. Committed is a
roadmap category: it says the work is planned, and it may be written down. Supported is a claim about evidence, and the
strong form of that evidence is a runtime gate: the test suite on the real target plus a frontend smoke that drives the
packaged artifact through a real window and GPU surface (each target's standing is in
[workspace.md](../reference/workspace.md) "Build and platform matrix"). A user reading "supported" is being told someone
ran it; a claim that rests on a green cross-compilation is not that.

Four consequences follow, each chosen deliberately.

### The cache is a substituter; the release is the claim

A release is what a user installs on the project's word, so its page and archives require _that tag's_ gate to have
passed, not the target's gate to exist: a gate that is defined and wired but has not run green publishes nothing.

The binary cache is not gated: the derivation is content-addressed, so a revision the cache lacks still builds to the
same store path on the user's machine, and holding a push back withholds only speed, never the revision. The push
therefore runs where the package builds (`pr.yml` `linux-smoke` and `darwin.yml` `darwin-build` on `main`,
`release.yml`'s build jobs on a tag), and it names the store path it built rather than arming a post-build hook, which
pushes nothing when a re-run finds the path already in the store. A pull request never pushes, since its code is not
trusted with the cache's credential.

Rejected: a publish job per target behind that revision's smoke, which gated nothing a user could not build from the
same revision and cost a runner slot and a second evaluation per `main` revision.

_Revisit if_ the cache becomes an install channel of its own, such as a profile or channel that points users at the
newest cached revision rather than at a flake reference they chose.

### A pull request smokes the debug build

A pull request publishes nothing, so its smoke only has to catch the client's own regressions; the packaged-artifact
smoke that the claim rests on runs on every `main` revision and tag. The shipped build costs a release compile of every
workspace crate per commit (the Nix package also stamps the revision, so even a commit that touches no crate misses its
cache), while the debug binaries are one link away from what the suite already compiled.

_Revisit if_ a `main` run's package smoke fails after its pull request passed.

### A source build is not an artifact

The flake may keep exposing an ungated target; a Nix user who builds `aarch64-linux` from source gets a build the docs
describe as untested. Withholding the expression instead would remove the only way anyone could produce the evidence.

### A tag rebuilds rather than promotes

`release.yml` runs the whole gate again on the tag's own revision, and `linux-smoke` refuses to push unless the built
binary's own identity agrees with the tag.

Promotion is rejected. A release could name the store path the tagged commit's own `main` run already pushed and attach
that, but promotion cannot be gated: a Forgejo `needs:` does not reach across workflow files, so a `release.yml` job can
only look up a cache entry by hash and trust it. A tag is also a mutable pointer, and it can be pushed at any commit:
one that never saw a green run, one whose run went red, one on a branch nobody merged.

The rebuild is cheap where it would hurt: the derivation is content-addressed, so a tag build whose inputs match the
`main` build produces the same store path and the cache push is a no-op. The attached archives are relocated copies of
that same store path rather than a second build, so the cache stays the reproducibility story and the tarballs are only
what a host without Nix can unpack. What the second run pays for is runner time, which a release spends once.

_Revisit if_ Forgejo gains cross-workflow gating, which would make promotion an option worth re-costing.

### Claims name exact targets

Claims name exact targets: `x86_64-linux`, not "Linux". Bare "Linux" covers musl, i686 and 32-bit ARM, and bare "macOS"
would cover `x86_64-darwin`, which is outside the matrix (no package, no coverage, no user); the vagueness would be
doing the claiming.

Windows is the case that shows the gate is only half of it: a green suite and smoke do not complete the claim alone, and
the other half is `release.yml` attaching the zip to the release page, so that a user has a download that outlives a
run's artifact retention. `aarch64-linux` has neither half, and its claim opens on its first green run, not on this
page.

### Maintainer use, for `aarch64-darwin`

One target carries the claim on a weaker class of evidence. `aarch64-darwin` has no window gate: `darwin.yml` builds it
and runs the release archive's headless checks (its manifest, its signature, and sessions spawned through the bundle),
but `nix/package.nix` sets `doCheck = false` there, and no suite or frontend smoke runs on the `aarch64-darwin` runner.
What stands in is the maintainer's daily use of that build on the platform's own hardware, across the revisions that
reach `main`. That witnesses the whole stack a smoke would drive, over hours rather than one scripted minute: the Metal
surface on a real GPU, the window server, the PTY, the input path.

What it does not witness is everything a machine would re-check. No frame is compared against an expected one, no CI run
reproduces the result, and the suite does not run on the target at all, so a regression only Cargo tests would catch
reaches the cache. The evidence is also one person's report about the revisions that person happened to run, and a
reader of a green run log cannot find it. The cache push runs in `darwin-build` after the build succeeds, so the cache
never carries a path whose build failed. _Revisit if_ a darwin frontend smoke lands, which retires this class of
evidence and makes the row read like `x86_64-linux`'s.
