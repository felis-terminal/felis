---
name: add-escape-sequence
description:
  Implement a new escape-sequence / terminal-protocol behavior (CSI, OSC, DCS, APC, Kitty extensions) in felis — check
  the recorded stance first, then walk the parser→grid→daemon→client path, pin it with snapshots, and ratchet the
  esctest baseline. Use when adding or changing how felis parses or reacts to any escape sequence, when a producer
  (yazi, presenterm, a TUI) emits something felis ignores, or when an esctest/vttest failure needs a fix. Not for
  throughput work (perf-trace) or debugging existing behavior (producer-traffic-debug).
allowed-tools: Read Grep Edit Bash(just:*) Bash(cargo:*)
---

# Adding an escape-sequence behavior

## 1. The stance is probably already recorded

Before implementing, check in this order:

1. `docs/reference/protocols/support-matrix.md`: the verdict, per sequence. A **🚫** there (e.g. Sixel: permanent) ends
   the task; don't re-litigate it in code.
2. `docs/explanation/protocols/landscape.md`: for the families whose status rests on an argument, why it was taken and
   what would reverse it.
3. The protocol's docs: `docs/reference/protocols/<name>.md` (wire format, limits) and
   `docs/explanation/protocols/<name>.md` (deviations, rejected alternatives).
4. If the sequence is genuinely new scope, run `principle-check`. Note principle 4: felis parses escape sequences, never
   shell _content_.

## 2. Implementation path

Data flows one way; place each piece at its lowest correct layer:

- **Parse/dispatch** — `crates/felis-vt`: the DFA recognizes the sequence and dispatches a typed event to the sink.
  Kitty graphics and text-sizing each have a dispatch module here; the Kitty keyboard encoder lives in
  `crates/felis-daemon/src/serve/key_encode.rs`, its flag type in `felis-protocol`, and its mode state in `felis-grid`.
- **State** — `crates/felis-grid`: cells, modes, scrollback, image store, damage. Most sequences end here.
- **Daemon orchestration** — `crates/felis-daemon` only when the sequence needs session-level work (e.g. graphics APC
  bodies, notifications relay).
- **Client/render** — `crates/felis-client-core` (shadow screen) and `felis-render-wgpu` only if presentation changes.
  Daemon owns state, client owns pixels (principle 3); a sequence must not make the daemon decide presentation.
- Query/report sequences (DECRQM-style) reply on the PTY. Follow an existing reply's path through the grid's
  host-response queue.

## 3. Tests (in the established style; see `test-strategy`)

- **insta snapshots** are the conformance workhorse: `crates/felis-grid/tests/snapshot_csi.rs`,
  `snapshot_csi_handlers.rs`, `snapshot_osc.rs`; protocol-specific ones in `crates/felis-vt/tests/snapshot_kitty_*.rs`.
  Review with `cargo insta review`.
- Drive helpers: integration tests under `crates/felis-grid/tests/` use `common/mod.rs`'s `drive(rows, cols, bytes)` /
  `drive_take` / `drive_with(parser, grid, bytes)`; unit tests inside `felis-grid` use `src/test_support.rs`'s
  `drive(parser, grid, bytes)`. Reuse the helper of the layer you are in and never invent a new argument order.
- **Fuzz**: the parser's totality is owned by `fuzz/fuzz_targets/vt_parser.rs` / `grid_dispatch.rs`; add a seed with the
  new sequence to `fuzz/seeds/<target>/` and run `just fuzz-smoke`.
- **esctest ratchet**: if the change fixes esctest failures, raise `PASS_BASELINE` per
  `docs/reference/esctest-compatibility.md` "Invariant and baseline" (the gate lives in
  `crates/felis-pty/tests/esctest_smoke.rs`). The baseline only ratchets up: a change that would lower it is a
  regression to fix, not a baseline to edit down.
- Verify against a **real producer** (yazi, presenterm, timg, raw escapes) with the `producer-traffic-debug` skill; the
  logs, not a screenshot, are the signal.

## 4. Doc cascade

- `docs/reference/protocols/support-matrix.md`: flip the status.
- The protocol's reference doc: wire format facts, limits.
- The protocol's explanation doc: any deviation from the upstream spec and why.
- `docs/reference/protocols/vt-compliance.md`: when the sequence's behavioral facts or caveats change (its "Consumed
  without effect" table included).
- `docs/explanation/protocols/landscape.md`: the rationale for an admission decision, and only when it passes
  `doc-cascade`'s record gate ("Default to no record"), such as a reversal of a recorded decision or a new tolerated
  input (its "Admitting a tolerated input" rule). Otherwise the why goes in the commit body.
- `docs/reference/spec.md` if a REQ is affected.

Run the `doc-cascade` skill for the sweep; a protocol change typically touches all of the files above.
