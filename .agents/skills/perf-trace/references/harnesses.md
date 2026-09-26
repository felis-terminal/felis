# Cross-terminal harnesses: what each suite bakes in

Read this before running or debugging the cross-terminal suite. The suites live in the repo, in `tools/bench/suites.py`
(one function each: `suite_vtebench`, `suite_termbench`, `suite_cat`, `suite_kittenbench`, `suite_doom_fire`,
`suite_startup`, `suite_memory`, `suite_latency`), on top of `tools/bench/field.py`; `flood-ab-macos.sh` stays in this
skill. Each function's docstring documents its own quirks; this file holds the shared environment traps and the recipes
that live outside any one suite.

`field.py` is the piece to read first. It owns the _condition_ every terminal is held to (pristine config, one font
family, the point size converted to a pixel `font.size_px` for felis, the grid, the scrollback depth, a cursor that does
not blink), and every suite launches through it, so no two suites can drift apart. `check_field.py`
(`just bench-vs-check-field`) is the two-minute check that the field still launches and lands on the pinned grid.

The rule for adding a pin: **pin the workload, never the strategy.** Anything that changes how much work there is (grid,
font, scrollback depth, a blinking cursor) has to be equalized or the bars are meaningless. Anything that is a
terminal's own answer to that work (`repaint_delay`, `max_fps`, vsync, renderer backend, ghostty tip's scrollback
compression) stays exactly as shipped; equalizing it measures a configuration nobody runs.

## The front door

`tools/bench/crossterm.py` (`just bench-vs`) drives every suite into one results root and renders `report.md` plus a PNG
per suite (matplotlib, so re-charting needs the bench shell). It resolves the whole field once: every terminal and tool
comes from `nix develop .#bench`, else PATH; it probes felis's grid once, and calls the suites in-process with that
`Field`. It keeps the `.done` resume semantics (re-run with `--out <root>` to continue a crashed pass), and lets one leg
(a missing tool, a terminal that will not launch) skip without taking the rest of the field down.

`--param ROUNDS=3` measures the whole field three times, rotating the leg order each round: the artifacts then sit under
`<suite>/round-<n>/`, the resume marker keys on the round as well as the terminal, and the charts draw the between-round
range instead of the within-leg spread. Resuming a root under a different `ROUNDS` is refused before the first window
opens, so pass the same value the root was measured with.

To drive one suite alone: `python3 tools/bench/suites.py vtebench --results <dir>`.

The charts read the suite artifacts directly (`*.dat`, `tb`'s JSON, hyperfine's JSON, `*.mem.json`, `*.startup.json`,
`*.res.json`), so `just bench-vs-report <root>` re-charts any results dir, including one produced by running a harness
by hand. That recipe enters `.#bench` even though it launches no terminal: the charts are matplotlib, which only that
shell carries. Parsers, scales and the provenance block are pinned by `tools/bench/crossterm_test.py`
(`just bench-vs-selftest`): change an artifact's shape and update both.

**`just bench-vs` enters `nix develop .#bench` for you.** That shell (`dev/bench/devshell.nix`) is the only way the
field is reproducible; invoking `crossterm.py run` by hand outside it takes every terminal from the host, and the report
marks each tool `unpinned` (with its sha256) instead of `pinned`. The same shell is where `tb` works at all on darwin
(nixpkgs' install_name is broken; the shell hands out a wrapped one), and where vtebench's workload definitions are
found (`FELIS_BENCH_VTEBENCH_BENCHMARKS`: nixpkgs installs them outside the binary, and vtebench's default
`./benchmarks` only exists in a source checkout).

`--tool NAME=PATH` names a binary for one entry in the field and swaps it. `ghostty-tip` is a second ghostty bar beside
the release, launched through the identical recipe, so a release-vs-tip delta is one chart. On Linux the bench shell
pins it to the upstream commit in `dev/flake.lock` (`FELIS_BENCH_GHOSTTY_TIP`), which lock maintenance advances; on
macOS, or for a tip newer than the lock, `fetch_ghostty_tip.py` (`just bench-vs-fetch-tip`) gets the build: a flake
build on Linux, a zip download on macOS, where it also strips `com.apple.quarantine`; without that, App Translocation
re-executes the bundle elsewhere and the leg never finishes. Either way the row is marked unpinned and its version
string recorded; `docs/reference/benchmarks.md` carries the rest. A full-field run refuses to start without both ghostty
builds: on macOS fetch the tip first; on Linux a missing one usually means the shell's store path was garbage-collected,
and re-entering the shell fixes it. `--terminals` lifts the requirement for a narrowed run.

Everything below still applies: the front door does not remove a single trap, it only stops you from re-typing every
invocation.

## Shared shape

All suites share, through `field.py`: the pinned condition (pristine config, one font family, points converted to a
pixel `font.size_px` for felis, the grid probed once by `suites.probe_grid`, 10k rows of scrollback, no cursor blink),
the window pin (`tools/bench/wm/`, one module per desktop: niri places the window through `niri msg action`, the macOS
tilers refuse the run, everything else leaves the size flags to it), quit-on-close options, and `.done` marker resume
(re-invoke with the same results dir after a crash; already-done terminals are skipped). Every leg reads its grid back
from inside the terminal, in cells and in `ws_xpixel`/`ws_ypixel`, and records a mismatch: the pin is a request, not a
guarantee. A leg the pin could not place gets `stty rows cols` from the wrapper instead and is recorded as `tty-only`.

- **paneru force-resizes window heights**, silently defeating every terminal's size flag (width survives). Stop it first
  (`paneru stop`, launchd-managed; `paneru start` to restore). The guard is `pgrep -x`, matching the process _name_:
  `pgrep -f 'paneru launch'` matches a command line paneru does not have, so it passes while paneru is up.
- **felis logs to STDOUT, not stderr.** Its tracing subscriber writes to stdout (felis-client main.rs), so a
  `>/dev/null` on the client discards the warning that says the pinned font was not installed, which is the only signal
  that felis alone fell back to another face.
- **felis's own config has to be pinned like every other terminal's.** `FelisRun.start` pins a throwaway `HOME` (plus
  `XDG_CONFIG_HOME` for Linux), because `directories::ProjectDirs` has no env override on macOS. The root
  `--config PATH` (docs/reference/cli.md "Global options") would also pin it; the harness does not use it. The key is
  `font.size_px`, in logical pixels: a point is one on macOS and 4/3 of one under the Wayland convention the rest of the
  field converts against, so the pin is converted off macOS. Verify a font-pin change by probing two sizes and watching
  the grid halve. A wrong key name is silent in the numbers, because felis runs its 14px default and the field is pinned
  to _that_ grid; it shows up only as `unknown key ignored` in the client's stdout log.
- **kitty and ghostty linger after their last window closes**; without `macos_quit_when_last_window_closed` /
  `--quit-after-last-window-closed` a sequential driver hangs forever. ghostty never exits even with the flag when its
  command exits instantly, which is why every suite, `suite_startup` included, terminates its launch itself instead of
  waiting for it to exit; `check_field.py` still reports "runs" and "exits" separately.
- **Two terminals cannot raise their own window.** ghostty 1.3.1 opens no window _at all_ until its application is
  activated (the launch sits there, the wrapper never runs, and the leg burns its whole timeout), and wezterm's window
  stays behind everything, because a binary started outside LaunchServices does not take focus on macOS. This is not the
  typing suite's problem alone: measured, ghostty timed out in vtebench and termbench too, so `run_leg` raises _every_
  leg before waiting on its marker. `field.raise_window` activates through System Events (`lsappinfo setfront` is
  refused with -54 for a caller that is not already frontmost) and returns at once when the launch's process tree
  already holds the front, which is the common case.
- **ghostty restores saved windows instead of honoring `-e`.** After any non-clean exit (a killed driver, a stray
  instance) ghostty's window-state restore takes over the next launch: the `-e` command silently never runs, the size
  flags lose to the saved geometry, and a re-launch can open several restored windows that each re-run a recorded
  wrapper. Two concurrent benchmark legs contaminated a ghostty `cat` drain 2x this way. Every ghostty invocation needs
  `--window-save-state=never` (the scripts carry it), and a single-window run should be verified (a `$$`-stamped start
  line in the wrapper log) before trusting the numbers.
- **ghostty's first-ever launch is slow** (cold app-bundle start) and can look like a failed spawn. A _freshly
  downloaded_ bundle is worse: Gatekeeper scans it on first launch. Measured here: a just-unpacked ghostty tip took 20s,
  and on the same loaded pass every already-warm terminal timed out at 30s. `check_field.py` waits 60s for this reason;
  re-run before believing a "runs: NO". Killing the driver does NOT kill an already-mapped terminal; sweep strays before
  re-running or two suites race and contaminate each other.
- **`ps` may be nix procps, not Apple's.** procps `ps` lacks the macOS entitlement for `rss`/`cputime` and spews its
  keyword list to stdout (`2>/dev/null` doesn't save you). Hardcode `/bin/ps` in anything that samples CPU time on
  macOS, and parse `cputime` at centisecond resolution: whole seconds read as 0 for sub-second floods. procps has no
  such resolution at all, so on Linux CPU time comes from `utime` + `stime` in `/proc/<pid>/stat`.

## vtebench

vtebench complements `kitten __benchmark__`: no DECSET 2026, so it measures PTY-read backpressure with the renderer
live, and its scrolling workloads expose costs the parse-only benchmark hides. Payloads scale with the tty grid (`tput`
in each benchmark script), so all terminals must run at the same rows × cols. Results land in `<results>/<term>.dat`,
which `crossterm.py` charts. To run a subset, point `-b` at a directory of symlinks to the wanted `benchmarks/*`
entries.

Linux specifics:

- **Run it under `nix develop .#bench -c`.** Outside a dev shell the nix-built felis-client dies at startup ("The
  wayland library could not be loaded"), no window maps, and the session drains at the daemon's detached budgeted pace
  (~10 MB/s). vtebench still prints plausible-looking numbers >10x off; a felis "natural grid" of 24x80 is the tell. The
  script aborts when the client dies early.
- **niri tiles every window**, so the size flags never hold; `wm/niri.py` floats each window and resizes it against the
  grid the tty reports back. `niri msg --json outputs` is where the scale comes from, and an unreadable one refuses the
  run rather than converting through a guess.

Two daemon-side probe recipes that pair with it (temporary patches, never committed): a per-wake stats counter in
`run_session`'s read arm (bytes/drive-µs/gap-µs per 64 wakes at info level) splits parse cost from waiting; launching
`felis-daemon serve --socket` manually captures its stderr without the GUI redirecting it. Set `SHELL=<wrapper>` on the
client invocation, not the daemon: a local session's shell comes from the environment the client sends, and the daemon's
own `SHELL` is only the fallback.

## cat / kittenbench / doom-fire

The three suites that reproduce ghostty's published comparison. Traps specific to them:

- **`cat` needs `--show-output`.** hyperfine sends a command's output to /dev/null by default, which would measure `cat`
  reading a file and never hand the terminal a byte. Each payload is named with `-n <kind>` so the chart reads `ascii`,
  not a store path.
- **`cat` stops at the pty, not at the parse.** A terminal that reads eagerly into an unparsed buffer finishes early. Do
  not report a `cat` win without the `kittenbench` bar beside it.
- **The payloads are generated, not captured.** `payloads.py` builds them from a fixed seed into
  `target/bench-payloads/` (150 MB x 3 = 450 MB, reused across runs). Delete that directory to force a rebuild; a file
  of the wrong size is rebuilt on its own.
- **kitten writes results to stdout and the payload to /dev/tty**, so `>> file` captures the numbers without taking the
  workload off the terminal. It hangs outside the foreground process group, so it is never wrapped in `timeout`, and
  `images` is left out of the default set because it waits on a graphics reply. One benchmark per invocation, after a
  discarded warm-up pass.
- **DOOM-fire runs the patched build** from `.#bench` (`doom-fire-bench.patch`): `DOOM_BENCH_SECS=<n>` skips the
  keystroke prompts and the 120x22 minimum check, exits on a deadline, and prints
  `doom-fire frames ... secs ... fps ... bytes_avg ...` to stderr. Without that variable the binary is upstream's demo
  and will block forever. Its frame is sized from the tty, so its bars are only as comparable as the grid pin.
- **The app's counter is a cumulative average**, so warm-up frames cannot be discarded within a run; the suite runs a
  short pass and throws the whole thing away instead.

## termbench-pro / startup / memory

- `suite_termbench`: contour's `tb` (nixpkgs `termbench-pro`), five fixed-payload categories as JSON MB/s. The nixpkgs
  darwin package has a broken install_name, so a bare store `tb` dies at startup; `.#bench` exports a wrapper with
  `DYLD_LIBRARY_PATH` set, which is what `TB_BIN` points at. Any hand-rolled `tb` invocation outside that shell still
  needs the variable.
- `suite_startup`: launch to first window, watched by `tools/bench/firstwindow.py` (niri event stream on Linux,
  `CGWindowListCopyWindowInfo` on macOS); the launch is terminated once its window is seen. felis gets cold (bench
  daemon killed per launch) and warm bars; the daemon-autospawn premium is the delta. Linux needs niri: under another
  compositor the suite skips itself.
- Resource sampling (`tools/bench/usage.py`): the throughput suites wrap their measured pass in `usage.wrap`, and
  `run_leg(..., sample=True)` writes `<name>.res.json`. To sanity-check a sampler by hand, compare its `cpu_s` against
  `ps -o cputime` and its macOS footprint against `footprint -p <pid>`; kitty's `kitten __atexit__` child is counted as
  kitty (`field.HELPERS`).
- `suite_memory`: what a terminal holds when idle and after flooding 200k lines through scrollback, plus the CPU between
  the two marks (a difference, so startup stays out of a flood column). Not RSS: `field.memory_sample` reads
  `phys_footprint` through `proc_pid_rusage` on macOS and PSS from `smaps_rollup` on Linux, because RSS bills every
  shared page to each process mapping it and felis is the one leg that is two processes. GPU-side memory is outside
  both. Scrollback pins to felis's fixed `DEFAULT_SCROLLBACK_ROWS` (10k, no config key): felis cannot move, so it sets
  the number. ghostty is the one that can get away: 1.3.1 caps by bytes only, while tip renamed that key and added
  `scrollback-limit-lines`, so `field.py` probes `+show-config --default` and pins the line cap where it exists. A build
  without it keeps its default and lands in `SCROLLBACK_UNPINNED` in `meta.json`, which the memory chart prints. felis
  is sampled split (client / daemon), plus detached-daemon residency and idle CPU after the window closes.

Flamegraphs: `samply-macos.sh tb` profiles the daemon under a `tb` loop; `prof-flame.py <profile> <syms> <thread>` emits
collapsed stacks for `inferno-flamegraph` (nixpkgs `inferno`).

## latency

`suite_latency` is the odd one out: it measures from _outside_ the terminal, and it is the one suite with a different
instrument per platform. Both press a key and poll the screen until the pixel where the glyph lands changes color, so
the bar carries compositor and display as well as terminal. macOS uses `typometer` (built headless by `.#bench` from
`tools/bench/typometer/Main.java`) through the OS event API; Linux uses `wl-latency --measure` (also built by `.#bench`)
through `zwp_virtual_keyboard_v1` and `zwlr_screencopy_manager_v1`. Bars taken by different instruments do not belong on
one axis, and the chart names which one it drew. The macOS traps are unlike every other suite's:

- **Three grants, all on the wrong process.** Screen Recording, Accessibility and control of System Events are checked
  against the application the run was started _from_, not against `java` or `osascript`. Grant them to that terminal, or
  every leg dies with `Cannot detect the reference pattern.`, which reads like a rendering bug and is a permission
  denial.
- **The raise is load-bearing twice.** Every suite needs it to open ghostty's window at all (see "Shared shape"); this
  one also needs the window that is typed into to be the frontmost one, so the driver re-raises and then _verifies_
  rather than trusting the activation.
- **The display sleeps and then the Mac locks.** This is the trap that costs a whole run: the suite asks for an
  untouched desktop, and an untouched Mac parks its display on the `pmset displaysleep` timer and locks. Every remaining
  leg then fails, because no window can be in front of the lock screen. `suite_latency` holds `caffeinate -d -i -u`
  (which also wakes a parked display) for its duration; when debugging a leg by hand, hold one yourself.
- **Every path in a wrapper must be absolute.** The wrapper runs inside the terminal under test, and wezterm starts its
  shell in the user's home instead of inheriting the driver's working directory; a relative results root makes its
  `touch` fail with "No such file or directory" and the leg times out while every other terminal passes. `crossterm.py`
  resolves `--out` for this reason.
- **The window-manager guard is re-checked per leg here.** paneru can come up mid-run (a login, a launchd restart). As
  measured: a run that passed the start-of-run guard had four legs force-resized (width kept, heights 79/85 against the
  pinned 37), and the leg being typed into when it happened died with `Previously undetected block cursor found`, which
  reads like a cursor bug and is a resize.
- **Do not touch the keyboard.** Keystrokes follow focus. Each leg re-checks through `lsappinfo` that its own window is
  frontmost and skips itself otherwise, so the worst case is a missing bar; but a notification or a Spotlight window
  costs whatever leg it lands on.
- **Block cursor aborts the run.** Either instrument needs the pixel it watches to start at the background color, so the
  wrapper sets a steady bar with `CSI 6 q`. felis has no cursor-shape config key (shape belongs to the program), which
  is why this pin is an escape sequence rather than a launch flag. This one is not macOS-only.
- **9 pt is too small to measure.** The pattern detection diffs two screen captures and smooths with a 2 px radius, so
  in the field's pinned 9 pt cell the five dots merge and every leg aborts. Measured on this field: 9 and 10 fail, 12
  and 16 work. `suite_latency` therefore runs at `LATENCY_FONT_PT` (14) and re-probes felis's grid at that size (it is
  the one suite that does not inherit `FONT_PT`).
- **The floor is the machine's.** The published figures include the compositor, the display and the instrument
  (`docs/reference/benchmarks.md` "Latency"), so read the bars against each other. A felis-only regression is better
  chased with the headless `profile_echo_latency` probe in `crates/felis-daemon/tests/real_app_harness.rs`, which can
  see inside the pipeline. The Linux leg has its own three:

- **The instrument has to pass its own calibration first.** `tools/bench/wl-latency/` is its own crate, not a workspace
  member (build it by hand with `cargo build --release --manifest-path tools/bench/wl-latency/Cargo.toml`). `--globals`
  dumps what the compositor offers; `--calibrate` times the tool against its own surface and is the gate the suite rests
  on (p95 − p5 under 5 ms). Run it on a host with a real display: under a nested niri the samples are the nested
  output's redraw clock, roughly a second wide, and the gate fails for a reason that is not the instrument.
- **niri only, by the focus check.** `--measure` takes `--window-id` and refuses to inject unless
  `niri msg focused-window` reports that id, re-checked before every keystroke. The suite gets the id from the window
  pin's `focus_for_typing`; a desktop whose pin cannot answer skips the leg.
- **Every sample is quantized by one output frame.** A screencopy frame arrives when the compositor next composites, so
  on a 60 Hz output nothing below ~16 ms of resolution is there to be read. Bars separate well above that; do not read a
  1 ms difference between two of them.

## Capturing a real app's byte stream as a payload

For a workload only a real program produces (DOOM-fire, a TUI), run it once under `script(1)` with the pty pinned
(`stty rows R cols C` inside the wrapped command, because the app sizes its frames from the pty), slice the steady-state
portion out of the recording (warm-up frames can be 3x smaller), and replay the file through flood-ab / the floor bench
/ other terminals. Two rules:

- **A captured payload only replays at its capture geometry.** The same bytes in a shorter window turn cursor-home
  repaints into a scroll storm and can invert a cross-terminal comparison. The floor bench takes
  `FELIS_BENCH_ROWS`/`FELIS_BENCH_COLS` to match.
- **An app's own on-screen fps counter may be a cumulative average** (DOOM-fire's is: it starts >2000 on tiny warm-up
  frames and decays for minutes), so eyeballed readings are inflated by run length. Reconstruct instantaneous fps from
  consecutive counter values (`T(n) = n/fps_n`) or compare drain seconds directly.
