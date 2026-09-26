---
name: extend-ipc
description:
  Extend felis's IPC surface — a new message family, field, handshake flag, or `felis sessions` verb — without breaking
  the versioning contract or the felis-protocol purity rule. Use when adding/changing anything in felis-protocol, wiring
  a daemon handler or client-core connector call, adding a CLI verb or its `--format` machine output, or when a change
  must stay compatible with older daemons/clients. IPC is felis's one extension surface (principle 1), so most feature
  requests that survive principle-check land here.
allowed-tools: Read Grep Edit Bash(just:*) Bash(cargo:*)
---

# Extending the IPC surface

The public, versioned IPC is felis's **only** extension surface: no embedded evaluator, no hooks, no plugin API. Verbs
and messages carry typed data (`SendString { text }`), never expressions or callbacks. If the proposal smells like
scripting, run `principle-check` first.

## 0. Classify a new verb before wiring it

A machine-output verb or bridge operation starts from its class: Point, Point-diagnostic, Stream, or Exempt, chosen
against `docs/explanation/architecture/control-surfaces.md` "Shared conventions" → "Machine output". Write the class
into the proposal, and for a Stream also what its clean terminal's `count` counts and which failures end it in the
failing terminal. A proposal missing either is not ready to review; send it back rather than letting the implementation
pick.

## 1. Read the contract before touching the wire

- `docs/reference/ipc.md`: the normative wire spec: stream/frame/ application layers, message families, handshake,
  versioning rules.
- `crates/felis-protocol/proto/felis.proto`: the machine-readable authority for the schema itself: the families, their
  supporting types, and the `FrameKind` numbering. A message change starts here.
- `docs/reference/row-codec.md`: the one field the schema does not describe: the `packed_cells` bytes, spec'd
  language-neutrally with golden vectors.
- `docs/explanation/architecture/ipc.md`: why it looks like this; its **rejected alternatives** are load-bearing (a
  "missing" feature is usually a rejected one), and what never crosses the wire at all is
  `docs/explanation/architecture/overview.md` "Responsibility table".
- `docs/reference/cli.md`: the CLI contract for verbs: the `--format human|json|jsonl` classification, the object shapes
  each class writes, and the exit-code meanings.

## 2. The compatibility rules (do not improvise)

The argument is in `docs/explanation/architecture/ipc.md`; these are the rules it yields.

- A new oneof arm carries a `(felis.v1.arm)` field option spelling its routing row (direction, correlation, modes,
  phases, since), matching the `ArmMeta` entry added beside it; `the_schema_declares_the_same_arm_table` is the only
  gate on that row, since `buf breaking` does not read options.
- Fields and oneof arms are **appended** with the next field number. A family with arms in both directions is two
  wrappers (`<Family>ToDaemonMsg`, `<Family>ToClientMsg`): a new arm joins the wrapper for its direction and takes the
  next number free across both, never a number the other wrapper uses. An addition bumps `PROTOCOL_MINOR`
  (`crates/felis-protocol/src/preface.rs` and the pinned declaration in `felis.proto`'s header) and is gated on the
  connection's effective minor (the min of the two peers) before a sender may use it.
- Renumbering, retyping, or reinterpreting an existing field is a semantic break: bump `PROTOCOL_MAJOR`; the preface
  then refuses the older half outright.
- `--format` output shapes are append-only too; their epoch is the `"v"` every object carries.
- Exit codes are fixed; `docs/reference/cli.md` "Exit codes" owns their meanings.
- An _optimization_ is negotiated in `Hello`, never by sniffing: today's one is `Hello.pull_paced`, and the next that a
  real peer declines earns a `ConnectionMode`, not a second bool. A gated _surface_ earns a `ConnectionMode` too
  (`crates/felis-protocol/src/caps.rs`).

## 3. Purity rule for felis-protocol

`felis-protocol` is the cross-language reuse surface: **no tokio, no OS-specific code, no I/O**.
`docs/reference/security-audits.md` has a standing "felis-protocol purity" checklist; run it over your diff. Non-Rust
clients (felis.el) generate their codec straight from `felis.proto` via a `protoc-gen-*`; there is no in-repo wire
manifest to regenerate. Framing/handshake constants a client can't get from the schema (frame header, the frozen preface
layout, keyboard bits) are documented in `docs/reference/ipc.md`.

## 4. Wiring path

A new message/verb typically touches, in dependency order:

1. `crates/felis-protocol`: three files move together for any message change:
   - `src/messages/<family>.rs`: the domain type (source of truth for shapes), one module per family; vocabulary shared
     across families and the flat re-exports live in `src/messages.rs`. Add the new case to the wrapper's `*_cases()`
     corpus, because it is the **single** round-trip corpus (the codec round-trip also exercises the `convert/` impls),
     and the `*_cover_every_variant` guard fails until the case exists;
   - `proto/felis.proto`: the wire schema: mirror the change with the next free field number (numbers are frozen as
     declared, not derived from the Rust variant order), then regenerate the committed codegen with `just proto` (CI
     diffs it against the schema, so a proto edit that skips regeneration cannot merge), then run `just proto-compat`:
     `buf breaking` against the merge-base with `origin/main`, the same comparison the `proto-compat` CI job makes. For
     an intended pre-release break, add a `base: <40-hex sha of that merge-base>` line to
     `crates/felis-protocol/proto/BREAKING.md` with the why; the rule the gate applies is in `docs/reference/testing.md`
     "Wire compatibility gates". A minor bump also needs its row in `PROTOCOL_MINOR`'s ledger (`docs/reference/ipc.md`)
     and a matching `MINOR_LEDGER` entry in `preface.rs`, or the sync test fails;
   - `src/convert/<family>.rs`: the domain↔wire mapping: infallible `From<domain>`, fallible `TryFrom<wire>`. Narrow
     every proto-widened integer through `narrow()` (never a bare `as` cast), reject `_UNSPECIFIED` enum sentinels. No
     per-family test corpus lives here: only targeted decode-rejection tests that need a hand-built wire value (bad
     enum, out-of-range narrow). Plus `frame.rs`/`codec.rs` if the frame layer changes, `caps.rs` if it adds a
     connection mode.
2. `crates/felis-daemon`: the handler on the session pool / IPC server.
3. `crates/felis-client-core/src/connector/requests.rs` (and `roster.rs` / `pull.rs` where relevant): the headless
   client call.
4. `crates/felis-cli`: the verb, its `--format` output (declare the class from step 0 by flattening `PointFormat` or
   `StreamFormat`, and emit through the `Reporter`), and completions. A bridge operation for a Stream verb also joins
   the `DaemonOp::is_streaming` match in `cli_bridge/admission.rs`;
   `the_bridge_streams_exactly_the_operations_whose_verb_is_a_stream` fails until the two agree. Any change to
   `cli_output.rs` or the bridge's request grammar means running `just schema`: the published bundle under
   `crates/felis-cli/schemas/` is generated from those types and CI fails on a stale copy.

## 5. Tests

- Add the case to the wrapper's `*_cases()` corpus (see "Wiring path" above); it is the round-trip test for message
  families.
- `fuzz/fuzz_targets/ipc_frame.rs` owns frame-decode totality and `ipc_body.rs` owns body-decode totality (prost/JSON +
  `convert/`); drop a seed for the new message into `fuzz/seeds/ipc_body/` (encoded body bytes) and run
  `just fuzz-smoke`.
- Rehydration/snapshot coverage: `crates/felis-client-core/tests/rehydration_snapshot.rs` if the shadow-grid state is
  affected.
- Version-skew behavior: assert the old-daemon path degrades to exit code `2` (or the documented fallback), not a panic.

## 6. Doc cascade (wider than usual)

- `docs/reference/ipc.md` and `docs/reference/cli.md` (normative).
- `docs/explanation/architecture/ipc.md` if a design decision was made.
- `docs/reference/control-surfaces.md` if the verb adds a knob.
- Affected `docs/how-to/` recipes and the two tutorials.
- **`skills/felis/SKILL.md`** — the shipped agent-facing skill documents the verbs, `--format` shapes, and the three
  guarantees; a verb change that skips it ships a stale skill to every user.

Run the `doc-cascade` skill for the grep sweep.
