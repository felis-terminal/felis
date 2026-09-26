---
title: Feature baseline
sidebar:
  order: 6
---

[Maximalism inside, strictness at the edge](design.md#maximalism-inside-strictness-at-the-edge) commits felis to full
fidelity within the terminal screen and to declining the responsibilities beyond it. The inside half needs a concrete
line: which generation of terminal protocol felis implements, and what implementing it in full demands.

Per-sequence status is the [protocol support matrix](../reference/protocols/support-matrix.md); the capabilities beyond
the screen, with the reason each is declined, are the [non-goals](non-goals.md).

## Maximalism inside the terminal

The terminal a current TUI is written against is not the VT100 one. Editors distinguish `Ctrl+I` from `Tab`, file
managers draw images inline, presentation tools size text above one cell, and shells mark their own prompts. The
baseline is therefore what producers emit today rather than what the historical record contains: the Kitty keyboard,
graphics, and text-sizing protocols in full, grapheme segmentation and East Asian width as invariants rather than best
effort, and OSC 8 and OSC 133 as first-class instructions.

Full is the operative word, because these protocols are negotiated. A producer probes for a capability and commits to a
rendering path on the answer, so a terminal that answers the probe and then implements half the protocol is worse for
that producer than one that never answered: the fallback path it would otherwise have taken is closed to it. This is
what makes fidelity a correctness property here and not a feature count, and it is why principle 2 states the test as
"if this would look better in raw Kitty, felis has a bug".

Kitty is the fidelity benchmark for the protocols in scope because it defined them, which fixes the reference
implementation and the wire meaning in one place. That does not extend the baseline to everything Kitty does: the target
is the protocol set producers use, not the surface area of any one terminal.

## Diagnostics and performance budgets

Throughput and latency are separate budgets, and conflating them makes both unmeasurable. Bulk output is bound by parser
and ring-buffer throughput, not by the display rate: a multi-gigabyte log dump must not be paced by the compositor.
Interactive latency is bound by the frame: keypress to glyph stays within one frame budget (REQ-1002), which the
daemon's diff cycle and the client's render-on-dirty pacing (REQ-707) together have to hold. Published cross-terminal
measurements demonstrating these budgets are in [benchmarks.md](../reference/benchmarks.md).

Diagnostics are trace-driven from outside the process. The daemon emits its events through `tracing` (REQ-1100), which a
profiler or a log consumer reads after the fact, and felis draws no in-client performance overlay
([non-goals](non-goals.md#convenience-features-that-hide-complexity)).
