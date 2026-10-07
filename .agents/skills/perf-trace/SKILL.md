---
name: perf-trace
description:
  Trace-driven throughput profiling for felis using `kitten __benchmark__` plus samply flamegraphs. Use when asked to
  measure or improve felis's parser/grid throughput (ascii, unicode, csi, long_escape_codes), benchmark felis against
  kitty, find a hot path in the daemon, or read a CPU profile of the parse loop. Covers driving the winit/Wayland app
  unattended, discarding the focus-warmup sample, decomposing cost with criterion micro-benches (pure dispatch vs decode
  vs full grid), capturing a samply profile of the daemon despite paranoid/mlock/attach/hand-off gotchas, and
  symbolicating raw addresses with jq + addr2line.
license: same as the felis repository
compatibility:
  Linux + Wayland (niri) or macOS host with felis built; needs kitten (kitty), samply, jq, addr2line (Linux) / python3
  (macOS). samply on Linux needs kernel.perf_event_paranoid <= 1.
metadata:
  author: felis
  version: "2.0"
allowed-tools:
  Bash(./target/release/felis:*) Bash(.agents/skills/perf-trace/scripts/*) Bash(python3 tools/bench/*) Bash(samply:*)
  Bash(jq:*) Bash(addr2line:*) Bash(cargo bench:*) Bash(cargo nextest:*) Bash(just bench:*) Bash(kitten:*) Bash(nix
  run:*) Read Edit
---

# Trace-driven performance analysis for felis

Use this when the goal is "make felis faster at X" and you want numbers and a profile to drive it, not guesswork.

The loop is: **measure → reference → decompose → profile → fix → re-measure.** Each stage has a tool; don't skip to
"fix" without a number that says where the time is.

Before starting a pass, read two reference files:

- `references/cost-maps.md`: where every measured regime's time goes, which levers are sized, which fixes are known
  duds. A new pass starts from these numbers, not from scratch.
- `references/traps.md`: measurement traps that each cost a day (bench-vs-real divergence, alias poison in the print
  loops, the ONLCR staircase). Read it before trusting any micro-bench and before touching `Parser::advance` or a print
  loop.

For the cross-terminal suites (vtebench, termbench-pro, cat, kittenbench, doom-fire, startup, memory, latency, payload
capture) read `references/harnesses.md` before running anything. They live in the repo, not in this skill:
`tools/bench/suites.py` over `tools/bench/field.py`, driven by `tools/bench/crossterm.py`.

To run the whole field in one go (every suite, one results root, charts at the end in ~1-1.5 h):

```
just bench-vs                        # or a subset: cat kittenbench doom-fire
just bench-vs-report <results-root>  # re-chart without re-measuring
just bench-vs-check-field            # 2 min: is the field launchable and pinned?
```

`bench-vs` enters `.#bench` itself, which is what pins the field (kitty, alacritty, wezterm, ghostty/foot, vtebench,
termbench-pro, hyperfine, kitten, DOOM-fire, Typometer on macOS, wl-latency on Linux). Calling
`tools/bench/crossterm.py run` by hand measures the host's own terminals instead and marks them as such.

`latency` types into the focused window, and needs two macOS permission grants there or niri here; read the `latency`
section of `references/harnesses.md` before running it, and leave the desktop alone while it does.

`field.py` is what makes the suites comparable: one pristine config, one font family, the point size converted to
felis's physical pixels, and one grid, probed from felis once, then verified from inside every terminal. The
orchestrator adds a resolved field (nothing is looked up inside a suite), provenance in `meta.json` (machine, GPU, power
state, every tool's version and origin), `report.md` and `png/`. Reach for
`python3 tools/bench/suites.py <suite> --results <dir>` when iterating on one suite, and for `bench-vs` when the
question is "where does felis sit in the field".

## 0. What the benchmark actually measures

`kitten __benchmark__` floods the TTY and times how long the terminal takes to parse it (it suppresses rendering via
synchronized output, DECSET 2026, which felis honors). So in felis it measures the **daemon's parse path**
(`Parser::advance` → `Grid` sink), not the renderer. Benchmarks: `ascii`, `unicode`, `csi`, `long_escape_codes`,
`images`. Throughput is reported as MB/s.

## 1. Measure: the kittenbench suite, felis-only

```
python3 tools/bench/suites.py kittenbench --results /tmp/kb --terminals felis
grep -a MB/s /tmp/kb/felis.kitten
```

`--terminals felis` runs only the felis leg (the dev build via `--felis-bin`, default `./target/release/felis`), so this
is the fast iteration loop; drop the flag to measure the whole field. The suite bakes in the quirks that cost real time
if you hand-roll the drive: a discarded focus-warmup run (the first benchmark after the window maps is ~5x slow), one
benchmark per invocation (`images` can hang on a graphics reply), no `timeout` (kitten hangs when not the foreground
process group), and a fresh private socket per run (a reused socket connects to a stale daemon and measures the old
build). Repetitions and the benchmark list are knobs: `--param KITTEN_REPS=80 --param KITTEN_BENCHMARKS="ascii csi"`.

## 2. Reference: measure kitty too

Run the same on stock kitty for headroom (`kitty --title ref sh -c '<wrapper>'`). The gap to kitty tells you which
benchmark has room. Compare like-for-like on the same machine.

## 3. Decompose: criterion micro-benches (no profiler needed)

Before reaching for a profiler, split the cost with in-process benches. This is fast (~8 s/run), deterministic, and is
the A/B harness for any fix:

- **Pure dispatch:** feed the stream to a `NoopSink` (`felis-vt` `parser_throughput` bench). Measures only the state
  machine.
- **Decode only:** a sink that runs `utf8::Decoder` + width but writes no cell.
- **Full grid:** `Parser::advance` into a real `Grid` (`felis-grid/benches/unicode_throughput.rs`). The delta from
  decode-only is the grid-mutation cost.

```
cargo bench -p felis-grid --bench unicode_throughput
cargo bench -p felis-vt   --bench parser_throughput
```

Bare `cargo bench` is for this exploratory decomposition **only**. Any number you will compare across runs (a baseline,
a regression gate, the headline figure) goes through the orchestrator: `just bench` / `just bench-all` /
`just bench-gate` (the same flags as the `bench.yml` CI gate). Hand-rolled Criterion invocations are how two baselines
stop being comparable.

Generate Unicode samples from code-point ranges in the bench itself; **never paste a third-party corpus** (e.g. kitty's
GPL benchmark text) into the repo, even for a bench.

## 4. Profile: samply

When you need to see which function dominates inside a stage. Linux:

```
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid   # once per boot
.agents/skills/perf-trace/scripts/samply-profile.sh unicode 16
.agents/skills/perf-trace/scripts/symbolize.sh /tmp/felis_prof.json.gz
```

The scripts exist because the profiler fights you: samply needs `perf_event_paranoid <= 1`; attach mode (`--pid`) dies
on the mlock limit, so the profiled process must be launched under samply. On Linux `samply-profile.sh` launches
`felis-daemon serve --socket` under samply and points the client at that socket; an auto-spawned daemon would land
outside samply's process tree (the `isolated-daemon` skill's "Start the daemon yourself"). The saved JSON has raw
`0x...` frames, which `symbolize.sh` resolves with addr2line against the non-stripped binary.

To profile an arbitrary byte shape instead of kitten's streams, put it in a file and use
`scripts/samply-macos.sh flood <payload>` (cat inside the profiled felis), with `scripts/flood-ab-macos.sh <payload>` as
the paired end-to-end A/B (raw vs DECSET 2026; the delta isolates the render-live pipeline from the parse path).

Reading the result:

- Pick the thread that owns the parse: `felis-pty-parser` (the `felis-pty-reader` thread only swaps buffers to it; see
  `references/cost-maps.md`).
- **The hottest leaf is often libc and a red herring.** Walk `stackTable.prefix` up to the first felis frame before
  believing any libc/libsystem leaf; on macOS, `__psynch_cvsignal` as a parse leaf means "this loop signals another
  thread per iteration".
- One function spans several addresses; merge duplicate rows.

## 5. Fix, then re-measure end-to-end

Iterate against the criterion bench (fast), then **confirm with the real-daemon harness**: a criterion win can hide
plumbing that doesn't move end-to-end (see the `Parser::advance` trap in `references/traps.md` for a win that inverted).
Add a criterion case that guards the win, and a correctness test if the fix touches semantics (e.g. chunk-boundary
invariance for a bulk decode).

## macOS variant

The workflow is identical; the plumbing differs. Measurement goes through the same `suites.py` invocation; profiling
goes through `samply-macos.sh <workload>` (`kitten <bench>` / `flood <payload>` / `tb`), with `flood-ab-macos.sh` as the
end-to-end A/B:

- **samply**: no paranoid/mlock gates. It occasionally dies with `couldn't create root TaskProfiler … InvalidAddress`;
  re-run.
- **Symbolication**: addr2line doesn't do Mach-O. The scripts pass `--unstable-presymbolicate` for a `.syms.json`
  sidecar; aggregate with `scripts/prof-top.py <profile> <syms> <thread-substr>` (python3 via
  `nix run nixpkgs#python3`).

Single-thread floor and full-daemon drain for any payload file (the two ends that bracket every end-to-end number):

```sh
FELIS_BENCH_FILE=/abs/path/payload \
FELIS_BENCH_ROWS=35 FELIS_BENCH_COLS=137 \
  cargo nextest run --release -p felis-daemon \
  -E 'test(measure_parse_into_grid_floor) or test(measure_attached_cat_drain)' \
  --run-ignored all --no-capture
```

The rows/cols must match the payload's capture geometry, and the payload's newlines must match what the terminal
actually receives (ONLCR: `\r\n`); both mismatches silently measure a different regime (`references/traps.md`).

## Cleanup (every run)

Daemons detach and persist. The bundled scripts run on a private daemon and tear it down on exit through the
`isolated-daemon` skill's helper; a run you start by hand uses the same helper, never a `pkill`.
