---
name: principle-check
description:
  Grade a proposed feature, scope change, or design decision against felis's four design principles before any code or
  doc work begins. Returns a per-principle verdict with the exact failing Test clause for any violation.
allowed-tools: Read
---

# Principle check

Use this when the user proposes a feature, asks "should felis support X?", or you are about to widen the project's
surface area. Run the proposal through the four principles in `docs/explanation/principles.md` and report which (if any)
it violates.

## Procedure

1. Read `docs/explanation/principles.md` if you do not already have its contents in context. That file is authoritative:
   quote its **Test:** clauses verbatim, do not paraphrase. (`docs/explanation/design.md` holds the rationale each
   principle points to.)
2. Restate the proposal in one sentence.
3. For each principle 1–4, answer:
   - **Pass** — the proposal does not interact with this principle.
   - **Fail** — quote the Test clause and explain the violation in one sentence.
   - **Tension** — passes the Test clause but trades against the principle's spirit; explain.
4. Summarize: **adopt as proposed**, **adopt with stated changes**, or **reject**.
5. If reject, point to where the rejection should be recorded (`non-goals.md`, a decision note in the owning design doc,
   or a closed-loop reply to the user).

## The four principles

The numbered titles below mirror `docs/explanation/principles.md`. The summary phrases are aides-memoires, not
authoritative; the file's Test clauses are. Principle 1 is broad: it folds in single-window, the "earn its place" bar,
and "no embedded evaluator; IPC is the extension surface" (each is a separate Test clause under it).

1. **Add only what earns its place.** One window holds exactly one shell. A capability enters felis only when no
   dedicated tool (shell, WM, external process) does it better _and_ a real consumer needs it. No embedded evaluator
   inside the felis process; the public, versioned IPC is the extension surface.
2. **Render everything, fast.** Modern protocols (Kitty graphics, text sizing, OSC 8) are first-class, not legacy
   curiosities, and speed comes from data structures (parser, shaper, atlas, grid-diff), not the language or GPU API.
3. **The daemon owns state, the client owns pixels.** Daemon owns process lifetime and raw state (PTY, grid,
   scrollback); client decides presentation (font, colors, cursor shape, shaping). State is portable; presentation is
   local. Closing the window must not kill the shell.
4. **Explicit over heuristic.** No URL detection, no content-type sniffing, no "smart" content-dependent defaults. If a
   feature parses shell _content_ (not escape sequences), it gets pushed out.

## Common failure patterns to check for

These have come up in this project's design conversations:

- **"Add a tab bar but only with one tab visible"** → fails 1 (the one-window-one-PTY Test clause).
- **"Allow a user-supplied script to react to OSC events"** → fails 1 (the embedded-evaluator Test clause; growing the
  IPC vocabulary or adding a typed action variant would _not_ fail it).
- **"Detect URLs and underline them"** → fails 4.
- **"Have the daemon pick the cursor color from the terminal theme"** → fails 3 (cursor shape/color is client-side
  presentation).
- **"Add Sixel because some legacy program emits it"** → fails 1 (a dedicated path, Kitty graphics, does it better and
  no consumer needs Sixel); recorded as a permanent reject in `non-goals.md` "Compatibility theater" and
  `reference/protocols/support-matrix.md` "Image protocols out of scope".
- **"Implement multi-user attach"** → fails 1; explicit non-goal beyond the v1 single-user SSH stdio path.

## Output format

Use this shape so the user can scan the verdict quickly:

```
Proposal: <one-sentence restatement>

1. Add only what earns its place       — Pass / Fail / Tension
2. Render everything, fast             — …
3. The daemon owns state, …            — …
4. Explicit over heuristic             — …

Verdict: adopt | adopt with changes | reject
Where to record: non-goals.md | owning design doc | reply only
```

For any **Fail**, quote the Test clause from `principles.md` exactly and reference the file. Principle 1 has three
distinct Test clauses: name which one fails.

## What this skill does _not_ do

- It does not author the design-doc decision note or the `non-goals.md` entry: edit the owning doc directly.
- It does not decide on its own; it surfaces the principle conflict so the user makes the call. If a principle conflict
  is genuinely contested, escalate to the user.
