---
title: Design principles
sidebar:
  order: 3
---

These rules turn the [design values](design.md) into acceptance criteria for changes to felis: each states an invariant
and supplies concrete **Tests** for rejecting a violation, and the scope exclusions they imply are enumerated in
[non-goals](non-goals.md). Convenience does not override them.

## 1. Add only what earns its place

A felis window holds exactly one shell, always. A capability enters felis only when no dedicated tool does it better and
a real consumer needs it. Delegated responsibilities stay with the window manager, shell, or external process, including
when the felis project supplies a companion utility. The felis process hosts no embedded evaluator; public, versioned
IPC is the extension surface.

Rationale: [A drawn boundary](design.md#a-drawn-boundary) and
[Composition across a process boundary](design.md#composition-across-a-process-boundary).

**Tests:**

- Any feature that lets one window display more than one PTY at the same time is rejected.
- If a dedicated tool at the shell, WM, or external-process layer does a feature as well or better, or if no real
  consumer needs it yet, the terminal does not implement it.
- Any proposal that introduces a runtime evaluator inside the felis process is rejected; proposals that grow the IPC
  vocabulary or add typed variants to the action enum are not.

## 2. Render everything, fast

felis treats modern terminal protocols as first-class rendering responsibilities. Fidelity and throughput both matter;
performance changes must address measured costs in the implementation, not rely on the language or GPU API's reputation.
The [feature baseline](feature-baseline.md) defines the protocol scope for the fidelity test.

Rationale: [Maximalism inside, strictness at the edge](design.md#maximalism-inside-strictness-at-the-edge) and
[Algorithms, not languages](design.md#algorithms-not-languages).

**Tests:**

- If "this would look better in raw Kitty" is ever true for the same input, felis has a bug, not a feature gap.
- Any optimization PR that does not show a profiler trace pointing at parser, shaper, atlas, or grid-diff code is
  suspect.

## 3. The daemon owns state, the client owns pixels

The daemon owns process lifetime and raw terminal state: the PTY, grid, scrollback, and protocol-level objects. The
client owns presentation: fonts, colors, cursor preferences, and shaping. Closing a client must not end the session;
changing presentation must not require a daemon restart.

Rationale: [Mechanism, not policy](design.md#mechanism-not-policy) and
[Correct by construction, not by replay](design.md#correct-by-construction-not-by-replay).

**Tests:**

- `pkill felis-client` followed by relaunch must not affect the running shell or its scrollback.
- If changing a client config requires a daemon restart, the boundary has been crossed.

## 4. Explicit over heuristic

Configuration is explicit. felis interprets escape sequences as instructions, never the content of shell output. A
clickable hyperlink requires OSC 8; URL-shaped text alone does not request one.

Rationale: [Explicit, never heuristic](design.md#explicit-never-heuristic).

**Test:** if a feature's behavior depends on parsing the _content_ of the shell output (rather than escape sequences),
it is a heuristic and gets pushed out.
