---
title: VT / ANSI compliance design
sidebar:
  order: 2
---

This page records why felis's VT/ANSI compliance set is shaped the way it is: the target tool set, the sequences
consciously left out, and the versioning rule for the set itself. The supported-sequence lists and query replies live in
the [reference twin](../../reference/protocols/vt-compliance.md).

felis implements an opinionated subset of VT/ANSI escape sequences. The goal is "everything modern shells and TUIs use,
plus the Kitty extensions, and nothing that exists only for VT220 nostalgia."

The authoritative reference is XTerm Control Sequences (`ctlseqs`). The compliance set is felis's deviation list against
it: what is in, what is out, and what deserves clarification.

## Targets

- **Modern shells:** bash, zsh, fish, nushell, pwsh.
- **Modern TUIs:** neovim, helix, lazygit, btop, k9s, atuin.
- **Pagers and editors invoked from those:** less, fzf, ripgrep `--pretty`.
- **Image-bearing TUIs:** yazi, presenterm, mdcat (Kitty graphics path).

If a sequence is not used by any tool in this list, it is a candidate for removal.

## Conscious omissions

- **Status display lines** (DECSASD / DECSSDT): ignored. There is no second line area to select, and no page-length or
  lines-per-screen setting either (DECSLPP / DECSNLS): the daemon owns the row count. All four are parsed and dropped
  rather than stored: a value whose only reader is its own DECRQSS query is bookkeeping for no behavior, and the invalid
  reply is what tells a probing program to fall back. The same reasoning applies to the ANSI modes felis does not
  implement (KAM, SRM, …): they carry no soft state, and DECRQM answers them as unknown.
- **Per-row tab stops:** felis carries one global stops table per grid. Programs that need per-line tabs (rare outside
  VT420 + DEC's Page Memory) get the global table.
- **VT100 line-drawing graphics charset** in non-Unicode form: see the reference twin's "Charset handling". The Unicode
  equivalents are first-class.
- **DECCOLM** (80 ↔ 132 column switch): the in-grid side effects (ED 2 clear, cursor home, margin reset; gated on `?40`)
  are implemented, and DECNCSM suppresses the clear alone, at DECSCL level 5 or above. The actual column-count change is
  deferred: geometry is the daemon's. DECRQM reports the mode as modifiable, matching xterm.
- **XTWINOPS window manipulation** (`CSI 1–9 t`): accepted and ignored, a non-goal (see
  [`non-goals.md`](../non-goals.md)). The report codes and the title stack are implemented.
- **Colorimetric color specs** in the ChangeColor families felis acts on (`OSC 4 / 5 / 10–12`): `rgb:` and `#hex` are
  the committed color-spec subset, and X11's `CIELab`, `CIELuv`, `CIEXYZ`, `CIExyY`, `CIEuvY`, and `TekHVC` forms are
  refused, as is its `rgbi:` intensity form. Every colorimetric space needs its own matrix and gamma transform, and
  kitty implements none of the colorimetric forms either, so programs running under Kitty already carry the fallback.
  `rgbi:` is a normalized RGB triple and would cost only a scale, but felis refuses it for the same reason: the
  producers felis targets emit `#hex` or `rgb:`. esctest exercises these forms (21 cases, recorded as rejected in the
  [esctest compatibility reference](../../reference/esctest-compatibility.md)), but that is xterm's coverage of an xterm
  extension, not producer demand. _Revisit if_ a tool in the target set above ships a colorimetric spec on its default
  path.
- **DECARM** (auto-repeat): permanently reset in DECRQM; auto-repeat lives in the OS, not the terminal.

## Reply identity

The replies themselves live in the [reference twin](../../reference/protocols/vt-compliance.md) "Reporting and queries";
two of them are shaped by constraints recorded here. DA1 advertises a VT400-class terminal (`?64`) carrying xterm's
VT420 default feature mask verbatim (including bits for families felis parses but does not drive), because a trimmed
mask sends DA1-branching programs into legacy fallbacks; the argument and its _Revisit if_ live in
[terminal-identity.md](../architecture/terminal-identity.md). DA2's middle field is a fixed firmware number `400`,
deliberately independent of `CARGO_PKG_VERSION`: esctest's `DA2Tests` requires `314 ≤ Pv ≤ 999` _and_ a value that does
not track the build version, so a constant satisfies both. The wider identity argument (why `TERM=xterm-felis` rather
than impersonating kitty) is [terminal-identity.md](../architecture/terminal-identity.md).

## Versioning the compliance set

The compliance set is part of felis's _interface_, not its implementation: any added or removed sequence is a breaking
change, and the [support matrix](../../reference/protocols/support-matrix.md) records its status in the same change. The
rationale for it is recorded only when the change is an admission decision a contributor would plausibly re-propose
against and the reference facts alone do not show why that alternative fails; the record then goes to
[landscape.md](landscape.md), which holds the admission decisions. Any other addition or removal carries its why in the
commit body.
