---
title: esctest compatibility
sidebar:
  order: 13
---

The `esctest2` conformance harness, the baseline it ratchets, and what each remaining failure is.

## Harness configuration

The PTY integration test harness at `crates/felis-pty/tests/esctest_smoke.rs` executes Thomas Dickey's
[`esctest2`](https://invisible-island.net/esctest/) suite (79 modules, 568 cases) inside a felis-spawned PTY.

- **Suite flags**: `--expected-terminal=xterm` (iTerm2-specific cases count as known bugs rather than failures),
  `--max-vt-level=4` (matches the VT420-class DA1 reply), `--no-print-logs`, `--timeout=1`, a per-run `--logfile`, and
  `--xterm-reverse-wrap=383` (xterm's split reverse-wrap expectations). Both `--no-print-logs` and `--timeout=1` shape
  the pass count.
- **Environment**: The dev shell puts the `esctest` derivation (`dev/packages/esctest.nix`) on `PATH`; the derivation
  removes the window-op cases' waits for a window manager.
- **Invocation**: `cargo nextest run -p felis-pty --test esctest_smoke` (it fails without the `esctest` binary).
  `ESCTEST_BIN` names a binary to use instead of the one on `PATH`, and `ESCTEST_INCLUDE` passes an `--include=` pattern
  so a single module can be re-run while iterating.

## Invariant and baseline

The `PASS_BASELINE` constant in `esctest_smoke.rs` acts as a regression floor: any commit that flips esctest cases to
pass must raise the floor to the new pass count in the same commit. A drop below the floor fails CI.

The commit that raises the floor names the sequence behaviors it implemented and lists the newly passing case names.

## Case breakdown

The suite runs 568 cases under `--expected-terminal=xterm`:

| Category  | Count | Status   | Description                                                |
| --------- | ----- | -------- | ---------------------------------------------------------- |
| Passed    | 491   | Enforced | Meets the `PASS_BASELINE` CI regression floor.             |
| Known-bug | 43    | Ignored  | Tests marked `@knownBug` in esctest; logged but not gated. |
| Failed    | 34    | Tracked  | 3 deferred to daemon scope, 31 rejected non-goals.         |

## The 34 failing cases

### Deferred cases (3 tests)

- **Daemon-coordinated DECCOLM resize** (3 tests): `test_DECSET_DECCOLM`, `test_DECSET_Allow80To132`, and
  `test_RIS_ResetDECCOLM`. In-grid clear and cursor-home are implemented; window resizing requires daemon cell-count to
  pixel mapping.

### Rejected cases (31 tests)

- **Colorimetric color specs** (21 tests): `CIELab`, `CIELuv`, `CIEXYZ`, `CIExyY`, `CIEuvY`, and `TekHVC` variants
  across the ChangeColor families, plus the `RGBI` variants, refused per
  [vt-compliance.md](../explanation/protocols/vt-compliance.md) "Conscious omissions".
- **Reports of state felis does not keep** (6 tests): `test_DECRQSS_DECSASD`, `test_DECRQSS_DECSSDT`,
  `test_DECRQSS_DECSNLS`, `test_DECRQSS_DECSLPP`, `test_DECRQM_ANSI_KAM`, and `test_DECRQM_ANSI_SRM`, answered per
  [vt-compliance.md](protocols/vt-compliance.md#consumed-without-effect) "Consumed without effect".
- **Page length** (1 test): `test_XtermWinops_DECSLPP`. `CSI Pn t` with `Pn ≥ 24` is consumed without effect, because
  the daemon owns the row count ([vt-compliance.md](../explanation/protocols/vt-compliance.md) "Conscious omissions").
- **XtermWinops window manipulation** (3 tests): `test_XtermWinops` window position and iconify variants (`CSI 1 t`,
  `CSI 2 t`, `CSI 3 t`), a non-goal ([non-goals.md](../explanation/non-goals.md)).

Per-sequence support status across the whole protocol surface, esctest-exercised or not, is owned by the
[protocol support matrix](protocols/support-matrix.md); a status flip is edited there. Every sequence `esctest2`
exercises that is not accounted for above passes against the `PASS_BASELINE` floor.
