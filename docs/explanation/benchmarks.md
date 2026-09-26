---
title: Benchmark-harness design
sidebar:
  order: 11
---

Why the cross-terminal benchmark harness (`tools/bench/crossterm.py`) is shaped the way it is, why the workloads and
instruments were chosen, and what makes comparative measurements valid. The normative harness specification, suite
definitions, and published results are in the reference twin, [benchmarks.md](../reference/benchmarks.md); in-tree
performance regression gating (Criterion) is in [reference/testing.md](../reference/testing.md#performance-benchmarks)
and [explanation/testing.md](testing.md).

The page groups the harness decisions by the question each answers: what kind of tool the harness is, which workloads
and instruments it borrows, which legs it lets into a report, and what a bar is allowed to claim.

## What the harness is

The harness is written in **Python, not shell**, for the reason the Criterion driver is
([testing.md](testing.md#performance-regression-gating)): every input and output of `tools/bench/crossterm.py` (suite
results, `meta.json`) is JSON.

### The cross-terminal comparison is a local report, never a gate

`tools/bench/crossterm.py` launches real windows in every terminal of the field, so its numbers are only valid on an
idle desktop whose window manager the harness can place a window through, conditions CI cannot offer. It answers a
different question from the Criterion gate ("where does felis sit in the field" versus "did felis get slower"), and only
the second question can be answered automatically.

The report uses charts rather than a table because a field of up to seven terminals across eight suites is read by eye,
and the payloads within a suite differ by more than an order of magnitude, so a shared axis flattens the fast ones into
slivers.

_Revisit if_ a dedicated, otherwise-idle benchmark machine appears; the resume markers make an unattended nightly run
possible, only the environment is missing.

### Repetition is what makes two bars comparable

Only running the field again measures whether felis's bar differs from the bar beside it, which is what `ROUNDS` buys.
The spread each suite measures is within one leg (hyperfine's runs, vtebench's samples, Typometer's characters), while
the quantity that decides the comparison is between-leg: the same terminal re-launched an hour later, on a machine whose
page cache, thermal state and compositor have all moved.

The order rotates per round rather than shuffling under a recorded seed, because whatever drifts over an hour lands on
positions: a rotation moves every terminal each round, while a shuffle can leave one where it was.

A two-decimal ratio taken from one leg each side is a claim the next run can reverse, so a ratio is printed only where
the two ranges are apart. A dispersion estimate over those rounds, a MAD or a standard deviation, is rejected: three
points do not support one, and the range is the honest statement of what the run saw.

_Revisit if_ a publishable run grows past the handful of rounds a desktop has time for, where a range starts tracking
the worst round rather than the spread.

## Workloads and instruments

### The workloads answer to what a reader already believes

`cat`, `kittenbench` and `doom-fire` reproduce the shapes of the published numbers on the pinned field instead of
measuring workloads of felis's own choosing. A reader arrives holding those numbers: ghostty's `time cat` over a 150 MB
file and its DOOM-fire frame rate are the ones in circulation.

`cat` is the weakest of the three and stays. It stops when the last byte reaches the pty, so an eagerly-reading terminal
looks fast, and felis (whose daemon owns the read) is exactly that shape. What keeps it honest is running beside suites
that stop later: three workloads stopping at three different places make a suspicious `cat` number legible instead of
load-bearing.

### The DOOM-fire rate costs a patch, and only that patch

The app drives the terminal directly, and the patch adds only a deadline and a stderr count: the fire, the frame and the
rate are upstream's. Upstream blocks on a keystroke, runs until Ctrl-C, and paints its frame rate on screen.

Each way around the patch fails:

- **Screen-scraping the counter.** It needs a facility only felis has, so it cannot cover the field.
- **Relaying the app through a harness-owned pty.** It inserts a copy between the app and the terminal under test, the
  thing being measured.
- **Replaying a recorded byte stream.** It removes the app's own CPU cost, which is most of what caps the frame rate.

_Revisit if_ upstream grows a headless or fixed-length mode; the patch is then a rebase burden with nothing left to buy.

### Input latency is borrowed, not invented

The suite borrows Typometer and adds the entry point upstream lacks (`tools/bench/typometer/`). Every published
terminal-latency figure of the last decade was taken with Typometer, and a home-grown loop would have to be driven
through something only felis exposes, so it could not cover the field at all. The suite earns its place beside the
throughput suites by answering the question they cannot: felis's daemon/client split is exactly the shape that could be
slow here and fast everywhere else.

One consequence bounds what the chart can claim whatever the platform: the number includes the compositor, the display
and the measurement itself, so the bars are comparable with each other and not with zero.

#### The Linux half needs a tool of its own

One instrument cannot cover both platforms. Typometer injects and reads the screen through X11 while the Linux field is
Wayland, so there is nothing to borrow there and the Linux half carries the same method in a tool of its own
(`tools/bench/wl-latency/`). Running the field under Xwayland so that Typometer itself could be borrowed is rejected: it
drops foot, which is Wayland-only, and it measures the X11 path nobody uses on a Wayland desktop.

#### The instrument measures itself first

Such a tool may not measure terminals before it has measured itself. Calibration injects into its own surface, whose key
handler flips a pixel, and the instrument earns the field only if its own p95 − p5 spread stays under 5 ms, the smallest
between-terminal gap the chart is asked to resolve. Flipping that pixel directly, without a key, would be the cheaper
calibration and is rejected too: it measures the capture half alone, so it could pass while injection jitter dominates
the terminal bars.

Both halves clear the gate on a display-paced output (injection p50 0.12 ms, capture p95 − p5 1.34 ms over 300
injections). The same calibration on the same host without a display reads p95 − p5 41 ms, which is the nested output's
fallback redraw rather than the instrument, because a screencopy frame arrives when the compositor next composites and a
nested compositor has no display pacing it. That is the one bound the Linux bars carry beyond the shared one, and the
chart states it.

#### The timing loop follows upstream, with two Wayland exceptions

The timing loop follows upstream's step for step, since a rewritten one would not be comparable with any Typometer
figure in circulation. Wayland forces two exceptions:

- **Each key is released as soon as it is pressed** rather than held until its glyph shows. A Wayland client repeats a
  held key itself, so a glyph slower than the seat's repeat delay would type a second one into the next cell.
- **Each screen state is polled for** rather than read once at a fixed moment. A terminal that presents a partly drawn
  frame and completes it later (ghostty tip under NVIDIA, 0.6 to 1.8 s) otherwise fails as a missing pattern or a block
  cursor instead of producing the slow number it is.

Only the detector differs otherwise, because a terminal under a freshly cleared `cat` is one flat color: a mark is a
pixel that departs from it, and the pattern is the evenly spaced run of them on a single row. That search runs per row
rather than over the image, because a whole-screen diff is not a quiet picture, since the previous leg's window is still
closing.

_Revisit if_ a second Linux compositor enters the field, at which point `wl-latency`'s niri-only focus check needs a
second implementation; or if a felis-only latency regression ever appears here, at which point the headless
`profile_echo_latency` probe (`crates/felis-daemon/tests/`) is the one that can bisect it, since this suite cannot see
inside the pipeline.

### Startup ends at the first window, not at the process exit

The end point is whatever the platform itself reports: a mapped toplevel under niri, which requires a committed buffer,
and an on-screen window on macOS, which does not prove drawn content; the chart says which.

Timing a launch of `true` until it exits is the established shape (hyperfine over `<terminal> -e true`) and is rejected:
it is a process lifetime, and part of the field shows no window during it at all (foot exits without mapping one,
wezterm on macOS never orders its window on screen), so those bars time a teardown.

The startup suite skips the WM pin and the grid read-back the other suites rely on, because the pin's handshake would
sit inside the timed launch.

_Revisit if_ macOS exposes a first-frame signal for another process's window.

## Which legs reach the report

### A leg taken while the display cannot present is refused, not flagged

The leg's artifacts are moved out of the report's reach, kept on disk for diagnosis. Behind the lock screen or with the
display asleep, macOS presents no frame, and a window that cannot present is not an idle version of one that can:
felis's client then paints nothing and retries on a backoff timer ([pipeline.md](rendering/pipeline.md) "Demand-driven
emission"), so every throughput, CPU and memory figure describes a window that draws no frame. A note beside such a bar
would still chart it on the same axis as the presenting ones.

Waking the display from the harness is already done for a whole run (`caffeinate -u` on macOS) and cannot unlock a
screen, so the check has to stand on its own. On Linux only a locker that reports to logind is seen, and display sleep
is not read at all; those halves are recorded as unknown rather than as presenting.

_Revisit if_ niri exposes output power state or its lock over IPC.

### A leg's artifacts reach the report only as one completed attempt

Each attempt writes into its own directory and is promoted whole. No process a leg started ever holds a path in the
directory the report reads, and what lands is a copy taken before the leg's processes are stopped, which no surviving
writer can change.

The alternative a reader reaches for is to clean up where each failure happens: clear a leg's stale rows before a retry,
stop its processes before moving its files, wait for every writer before a refusal is set aside. Each of those leaves a
window: a retry that raises before rewriting its rows, a process that ignores SIGTERM, a child forked after the tree was
read, a daemon-parented wrapper outside the launch's tree. The attempt directory closes all of them at once.

Stopping the processes is then about the next leg's machine, not about the numbers, so a process that escapes the walk
costs a `LINGERING` line and never a wrong bar.

_Revisit if_ a suite has to write outside its attempt directory while a leg runs.

## What a bar measures

### Resource use is sampled over the workload, not read from the kernel's peaks

The peak is the largest of the samples taken during the measured pass. Both kernels keep a high-water mark, and it is
the obvious peak to read, but on macOS the only one is the lifetime maximum, which for every GUI terminal is its 280 to
440 MB startup, so it would chart the launch rather than the workload.

The window opens on a handshake rather than on a marker the sampler notices later, because a fast terminal drains a
small payload in less time than the process set takes to resolve and would otherwise chart as zero CPU.

CPU is what is charged to the terminal's own processes, so a GPU terminal is charged less of its drawing than a CPU
renderer; the chart says so rather than estimating the driver's share.

### Pin workload, never strategy

The harness pins what the workload needs and leaves each terminal's performance strategy alone. Stock scrollback caps
span an order of magnitude and cursor blink differs out of the box, so leaving them unpinned would chart configurations,
not terminals. Frame pacing, renderer backends and damage tracking are each project's shipped performance decisions, so
equalizing them would measure a configuration nobody runs.

felis sets the scrollback number by being the one terminal that cannot move (`DEFAULT_SCROLLBACK_ROWS`, no config key).
Where a pin cannot be applied honestly (ghostty's byte-only cap), it is recorded as absent rather than approximated:
inventing a byte budget from an assumed per-cell cost would be a pin that looks applied and is not, which is strictly
worse because the chart lines the bars up either way.

The grid gets the same treatment from the other side: the window manager is the one party that overrides every
terminal's request, so the harness asks it directly instead of asking each terminal. Every leg's grid is then read back
from inside the terminal rather than assumed, because a pin nothing checks is a comment and a cell count alone cannot
see a font pin that did not apply.

_Revisit if_ felis gains an initial-size surface (the probe could go), or ghostty documents a stable cell cost.

#### Not a nested compositor

Running the field inside a nested compositor (`niri --config bench.kdl`, everything floating) would need no desktop
detection at all and would reproduce across hosts. It is rejected because the render-bound suites would then measure a
compositor nobody runs, a nested compositor presenting through the host's Wayland surface so that its frame pacing is
the host's and its own at once.

_Revisit if_ runtime resizing on the host compositor proves unreliable, or a second Linux compositor enters the field:
one nested config then beats one pin module per desktop.

### The memory suite plots the platform's own accounting, not RSS

Each platform is read through its own ledger, and every leg names the ledger it was read through, because two runs taken
under different accountings do not belong on one axis.

RSS bills every shared page in full to each process mapping it, and felis is the one leg in the field that is two
processes: under RSS it pays twice for libc, the GPU driver and the mmap'd font, and the daemon/client split shows up as
a cost the chart cannot tell from a real one. On macOS RSS also drops whatever the memory compressor took, which makes
an idle bar partly a record of what else the machine was doing.

`vmmap`'s footprint line is the alternative on macOS and needs a task port a notarized terminal's hardened runtime
refuses, which excludes exactly the field being measured.

No further decomposition is asserted, not heap vs. mapping vs. atlas and not "resident plus compressed" for
`phys_footprint`, because a ledger that counts IOKit mappings, page tables and purgeable memory and subtracts
alternate-accounted memory survives no two-word paraphrase the harness could check.

_Revisit if_ a per-process GPU accounting arrives that covers the whole field (Linux has one in DRM `fdinfo`; macOS has
no equivalent, and half a field is not a comparison).

### felis's own row is attributed to the binary, not the checkout

Both revisions are recorded and their disagreement written out as a sentence, because the one attribution every reader
will attempt is the one that would silently be wrong. `--felis` may name a `nix build` result or a stale
`target/release` binary; either is a perfectly good thing to measure and a very bad thing to file under `git describe`.

_Revisit if_ the build stops embedding a hash (a tarball release falls back to the checkout, the honest answer only
because there is nothing better to say).
