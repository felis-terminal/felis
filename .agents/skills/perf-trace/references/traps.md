# Micro-bench traps: each cost a day

Read this before trusting any micro-bench result, and before touching `Parser::advance`, the print loops, or anything
the profiler says is hot. The pattern across all of them: an isolated measurement that looks decisive but encodes a
structure the real code doesn't have (or vice versa).

- **`black_box` can manufacture a bottleneck.** A standalone `slice::fill(black_box(cell))` probe measured 12x slower
  than memcpy and "confirmed" a fill hypothesis; but in-tree, with the blank value loop-invariant and inlined, the same
  fill was fine; the criterion bench on the real `Grid` showed no change and the real benchmark none either. Decompose
  with the real structure before trusting an isolated probe.
- **Chained small memmoves lose to a store loop.** Doubling `copy_within` to blank a row (log2 memmove calls) was ~10%
  worse end-to-end on csi than the compiler's per-cell store loop at typical erase widths. Call overhead dominates under
  ~2 KiB spans.
- **`Parser::advance`'s dispatch loop punishes any extra shape.** A CsiParam run fast path was tried in five shapes:
  inlining the params mutation into the loop cost csi_heavy ~50% (register allocation); an `#[inline(never)]` consumer
  fixed that and won sgr_dense −15% / csi_heavy −5% on the NoopSink criterion bench, yet still lost ~16% on
  termbench-pro's SGR categories in the real daemon, where `advance` inlines into the Grid sink instead. The `mixed`
  stream regressed +7–10% in every shape including a guard-free jump table; it tracks `advance`'s code size, not branch
  order. Reverted; the chunk-invariance test (`csi_param_chunked_advance_matches_per_byte_dispatch`) and the `sgr_dense`
  bench case remain for the next attempt. A NoopSink parser win must be re-proven end-to-end BEFORE any cleanup pass,
  and on this box that means interleaved A/B runs (keep both builds' binaries, alternate `FELIS_BIN`, n≥3); single
  termbench runs swing ±25% whenever the user's VMs are awake.
- **A `self.cells` write/call before `print_str`'s bulk loop poisons it.** The loop's throughput needs the compiler to
  hoist the `Vec` data pointer and elide bounds checks across the whole run; any `self.cells` write or opaque
  `&mut self` call preceding it in the same function body defeats that at compile time, even on a branch the run never
  takes at runtime (ascii 536 → 366 MiB/s). Read-only probes of other fields (occupancy, row_lookup) are free.
  `#[inline(never)]` on the whole `print_str`, or splitting the loop into its own function, does NOT recover it: the
  loop must stay inlined AND call-free. Fix shape: detect the rare case with a read-only test, then fork it to a cold
  `#[inline(never)]` copy behind an early `return`, so the fast loop's body has no preceding write. This is how the
  zero-copy scroll's leading-gap blank is placed.
- **A `&self` method call _between_ a loop's `self.cells` stores is the same poison.** `print_wide_str`'s batch loop
  calling `self.grapheme_width(..)` per glyph capped the CJK floor at 187 MiB/s: the compiler must assume the call reads
  the cells just stored, so nothing hoists. Replacing the call with a free function on `char` (`bulk_width_or_defer`)
  bought +18% in one step. Suspect this whenever a hoisted-loop rewrite lands far under its throwaway ceiling
  experiment.
- **Self-time in a store loop is not removable time, and duds flip after a regime change.** With the method call still
  in place, the profiler showed width lookup at ~24% self, yet hardcoding the width gained 0% (ILP absorbed it under the
  stores). After the alias fix put the lookup on the loop's serial dependency chain, the same width fast-path broadening
  that had measured +1.5% (a recorded dud) returned +9%. Re-run rejected optimizations when the loop structure around
  them changes; a prior measurement encodes a prior regime.
- **A `-p felis-cli -p felis-client` rebuild is not a perf A/B.** The parse loop, the grid and the registry sweeps all
  live in `felis-daemon`, which neither `-p` builds; the window then runs the _previous_ daemon and both legs measure
  the same code. A 3.4× daemon-side win read as flat in three separate window harnesses before this was spotted. Build
  the whole workspace, or A/B through `nix build .#felis`, which cannot half-build.
- **Timing `cat` measures the pty write, not the drain.** felis reads ahead of its parser, so `time cat 32MB` returned
  at 200 MiB/s while the parse thread was still working (the same early return the cross-terminal report's `cat` chart
  warns about). Any hand-rolled flood must end in a `\033[6n` whose reply the driver reads, which is what makes
  `kitten __benchmark__` the harness that waits.
- **The kitten ascii benchmark staircases (bare `\n`); real `cat` does not.** `kitten __benchmark__ ascii` writes bare
  `\n`, and felis `line_feed` keeps the column, so every line lands past the watermark on a recycled row: 99% of
  `print_str` calls gap (instrument with an `AtomicU64` in `print_str`; the bench harness sends daemon stderr to
  `/dev/null`, so patch the launch line to a logfile to read it). This is the worst case for any
  recycle-and-fix-on-write scheme. Real `cat` through a tty gets `\n`→`\r\n` from ONLCR, restarts at column 0, and never
  gaps. The same trap inverts for hand-generated floor payloads: bytes written to a _file_ bypass ONLCR, so generate
  `\r\n` explicitly or the floor bench measures the staircase path (8 vs 93 MiB/s on the scroll flood). Measure real
  `time cat` (GUI harness, ONLCR path) too, and split debug from release: the scroll memset shows up 2x in the
  parse-bound debug build but is invisible in the display-bound release build.
