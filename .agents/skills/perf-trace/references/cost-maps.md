# Cost maps: where each regime's time goes and what the next lever is

Read this before starting a new perf pass: every regime below has been measured and decomposed at least once, and
several "obvious" fixes are known duds. Numbers are from this box unless noted; the date on a heading is the last
measurement that confirmed the section. When the loop structure around a rejected optimization changes, re-measure it
(the regime-flip trap in `traps.md`) instead of trusting an earlier verdict. This file holds only the current map: what
each fix is and what it replaces belongs in `git log` and the decision records in `docs/explanation/`.

## Unicode print path (2026-09-25)

Kitten-shaped unicode payload at 35×106, parse-into-grid floor: ~490 MiB/s at load 8 (~2.56 G `instructions:u` over two
passes). `print_wide_str` holds most of the self time: the scalar `Chars` decode plus 16-byte cell stores; validation
(simdutf8) and width (the BMP table in `sink.rs`) barely register. Next lever, sized at ≤10%: a fused
validate-and-decode of the run to UTF-32, the shape ghostty uses. The transferable lesson from the mixed-script corpus:
it is multilingual with ASCII spaces between words, so a design that splits runs at every ASCII↔UTF-8 boundary
dispatches per word, not per line: read the corpus before optimizing for what its filename says. The parser's UTF-8 run
therefore swallows interior printable ASCII.

## CUP + ED 2 full repaint (2026-07)

The vim / lazygit / helix per-keystroke shape. Current: a default-pen ED 2 blanks only each row's occupancy-watermark
prefix and marks damage only on rows that actually changed; the BCE (colored-pen) path keeps the full vectorized fill.
`daemon_side/csi_redraw` 85 MiB/s, `multi_cycle` 160. kitten's benchmarks and vtebench barely lean on ED 2, so the
criterion case is the only guard; keep it.

## Plaintext `cat`: topology, and the pty wall (2026-08-20)

`cat` is scroll- and kernel-bound, not parse-bound: pure parser dispatch runs ~22 GiB/s (NoopSink), and parse-CPU
micro-opts have never moved `time cat`; only thread topology has.

Current topology: the PTY reader thread swaps buffers to a dedicated `felis-pty-parser` thread
(condvar-signal-only-when-parked), and on macOS `MasterReader::read` holds a 50 µs bounded spin. Blocked-write p50 0.53
ms/frame at 82×160; attached drain doom 253 / ascii 319 / scroll 117 / region 120 MiB/s. Rejected alternatives, all
measured on macOS:

- async `read` (kqueue readiness): 2 syscalls per chunk vs 1, slower (~1067 vs ~640 ms).
- split-only (no spin): re-pays the blocked reader's schedule-in wake per ~1 KiB chunk, 0.84 ms.
- spin-only (parse inline): contends the kernel tty lock against the writer, 0.65 ms. Neither half works alone.
- pacing the spin's retries (hint sweep 0–8192): flat 0–128, then monotonically worse (8192 → 10.7 ms/frame). Each
  `read(2)` is what wakes the writer, so polite retries stall the lockstep.
- QoS boost (`USER_INTERACTIVE`) on reader+parser threads: noise.

Locking: the parser thread and the async fan-out share the grid mutex; re-locking per ~1 KiB chunk ping-pongs it, so the
async effect-replay coalesces to a ~4 ms window (`DRAIN_COALESCE`). When two threads share the grid lock, measure the
end-to-end drain, because contention hides from a single-thread bench.

**The pty is the wall, and it is not felis's to move (2026-07-11).** Bare-pty bracket (Python `openpty` + fork `cat` +
`os.read` loop), same 150 MB file:

| path                                     | MB/s     |
| ---------------------------------------- | -------- |
| `cat > /dev/null` (regular file, no tty) | ~15000   |
| `cat \| cat` (pipe)                      | ~5000    |
| bare pty drain, cooked (OPOST/ONLCR)     | ~200     |
| bare pty drain, raw (`cfmakeraw` slave)  | ~234     |
| **felis `time cat`**                     | **~287** |

The pty imposes a ~20× kernel tax vs a pipe (line discipline, master/slave two-queue, small-buffer producer/consumer
ping-pong). Read-buffer size is a non-lever (1 KiB vs 1 MiB moved it <5%). felis already beats the naive raw drain, and
the one knob left (the child's termios, raw/OPOST +18%) is not felis's to force. Swapping the pty for a pipe is
impossible (the child needs a controlling tty). `time cat` is done: optimize the floor bench, never this number. The
residual gap to the 0.33 ms two-process spin floor is this machine's per-syscall cost in the daemon's process context;
treat further blocked-write work as a dud unless the kernel's ~1 KiB queue changes.

## ascii cell-write path (2026-07-11)

Pen interning is in (`Cell` 28 → 16 B, `Cell.style: StyleId` into a grid-owned `StyleTable`; write paths stamp a
constant `pen_style` id). Read-free ascii floor ~600 MiB/s (~592–622 over four runs), above ghostty's windowed 537. The
remaining windowed gap is the PTY read tax + async drain, not the cell write. The wide-partner probe runs only below the
row's watermark, so kitten's bare-`\n` staircase (every line past the watermark) is pure stores: ~430 MiB/s at the floor
under load 8, where the leading-gap blank and `rotate_region` are the next costs. csi at the floor is ~140 MiB/s under
load 6. EL / ED resolve each blanked cell's style for ISO protection only while the style table holds a protected entry
(`StyleTable::has_iso_protected`); without that gate the resolve is 9% of the csi floor's `instructions:u`.

Truecolor is the counter-workload: a fresh pen per cell makes `intern` run per cell where the inline pen was a plain
16-byte store. Shipped state: foldhash (`FixedState`). Floor ~242 MiB/s vs ~290 inline-pen (−14%; SipHash was −25%). ~4%
of that is structural `pen_style` machinery, ~12% the dedup probe. A last-hit cache is useless (consecutive truecolor
pens differ), and the direct-mapped pen cache in front of the dedup map misses on every cell here too; what holds the
truecolor floor level with it is the map's packed-word key (`PenKey`: two `u64` compares instead of the enum-wise
`Attributes` equality). The client shadow re-interns every run on decode, so a hash change pays on both sides.

`time cat` does not respond to anything in this family (pty-bound, above); measure at the floor bench.

## SGR flood / DOOM-fire (2026-08-21)

The DOOM regime: ~85% of frame bytes are `38;5`/`48;5` SGRs, nearly every cell's fg+bg flips per frame. Landed levers:
bulk CsiParam runs in `advance` (`Params::push_run`; guarded by the chunk-invariance test and the vt_parser fuzz target;
doom floor +38%) and the packed two-`u64` `Attributes` hash (+12%). 56 MiB doom payload, 137×35: floor ~289 MiB/s,
headless drain ~198, real window 0.25 s (220 MB/s) vs ghostty 0.30; the drain is a felis win ~1.2×.

`push_run` decodes in registers (open slot value and count held locally, one store per separator) and the bulk arm also
takes the first parameter byte in `CsiEntry` (equivalence pinned by a Kani proof against `push_digit`/`next_slot` and a
chunk-split proptest). Floor, 35×106 DOOM capture, instructions:u: doom −9.1% (218 → 241 MiB/s), truecolor −15% (297 →
345), csi −5% (143 → 154), sparse scroll flat (96.3 → 96.9, instructions +1.4%). Rejected shapes:

- SWAR digit decode (8-byte window, mask + multiply per parameter) is +7% instructions on doom: SGR parameters are 1–3
  digits, and the per-parameter setup outweighs the digits it replaces.
- Loading `values[0]` when the list is empty reads `clear`'s wide zeroing store and blocks store forwarding once per
  CSI; start from 0 (`len == 0` implies an empty slot).
- The arm's state test must stay one `CsiEntry | CsiParam` range check with the `:` exclusion as a trailing byte test;
  two state tests joined by `||` cost csi ~8% of cycles.

Pen interning under DOOM (2026-09-25, 35×106 floor, `instructions:u` over both floor runs, load 11–14): intern + hash +
equality was ~24% of the parse thread's cycles. DOOM's `38;5` and `48;5` arrive as separate SGRs, so the pen changes on
99.7% of SGRs (a skip-if-unchanged compare _added_ 2% instructions), but it cycles among ~500 pens: a 1024-slot
direct-mapped cache keyed by the packed words hits ~96% (256 slots: 78%; 4096 buys 0.1% more). With `PenKey`, over the
register `push_run`: doom 9409 → 8833 M instructions (−6.1%), 214 → 236 MiB/s; csi −2.6%; truecolor −2.2%, MiB/s flat;
ascii unchanged. Intern's share of doom cycles drops to ~15%; what remains is the slot load's latency and the three
`Color` packs, not the map.

Accepted cost, re-check on any `advance` edit: the sparse full-screen scroll floor reads −5% (~140 → ~134 MiB/s) with
the bulk arm present (across every structural variant tried and with the hash change reverted), showing it is
`advance`'s grown inlined body, not branch count and not the hash. The scroll _drain_ is flat (pull-pacing-bound with
40% floor headroom), so the dip is user-invisible; it is the recorded price of +55% on the SGR regime.

Style-registry sweeps are the other half of this regime: sweep trigger headroom is proportional to the _scan_ (÷16),
with a dense mark buffer and a no-op-sweep early out (rationale in `docs/explanation/data-model/grid-and-cells.md`
"Style interning"). termbench `sgr_fg_lines` at the pinned 192×60 field: 229 MB/s, first in the field (alacritty
172–180); `sgr_fg_bg_lines` 311. Reproducing a sweep-bound row needs a scrollback prefix: termbench runs its five
payloads in one process, so earlier payloads fill the ring before the SGR rows start; an isolated flood into a fresh
window measures the top line and looks fine.

Live DOOM fps is strictly serial, `1/(t_sim + t_blocked_write)`; at 82×160 felis runs ~666 fps (sim 0.95 ms, measured
with a per-write frame-replay driver on captured frames). A parked (windowless) session runs ~74 fps by design
(`parse_sink`'s 10 MiB/s parked pacing, not a drain defect). **Never compare terminals through a `script(1)` relay**:
the inner pty caps every terminal at ~430 fps regardless of speed; drive bare and read the HUD via `sessions capture`.
ghostty bare at 82×160 has not been measured, so the user-observed ~100 fps deficit is unconfirmed either way.

## Scrolling (2026-08-20)

The unified viewport-into-history ring is in: the live viewport is a window _into_ the history ring, so an evicted row
is never copied; decision record in `docs/explanation/data-model/scrollback.md` "Unified viewport-into-history ring". It
removed the `Scrollback::push_prefix` copy that was the entire primary-vs-alt gap (~9 ns/line: half cold destination
miss in the 38 MiB ring, half call/copy). Interior regions rotate as a band (see the same doc, "Interior scroll band"),
which records the two codegen traps (in-band wrap must stay `%`, not a conditional subtract, −8%; the materialize guard
must be an `#[inline]` shim over a `#[cold]` body, −9%).

Post-ring numbers, 137×35: sparse scroll floor ~140 MiB/s, ascii CRLF floor ~360, e2e sparse drain ~103 (+41%, wire
frames −30%), real window ~98 MB/s; region-scroll floor ~153 MiB/s, parity with full-screen.

Rejected / probe lessons:

- push_prefix's `head = (head+1) % cap` → compare-and-reset branch: −16% on the primary floor. The division was not the
  cost; the path family is as inlining/register-sensitive as `csi_param` and `print_str` (`traps.md`). Predates the
  ring; the lesson stands.
- A probe layered _on top of_ the structure it would replace under-measures it: a `lookup_base` added over the live
  `row_lookup` permutation read −1.1% and shelved the refactor, but _replacing_ the permutation sheds both the O(rows)
  rotate and the per-read indirection load (+8–11%). Probe the replacement, not the addition.

**Trap: `kitten __benchmark__ ascii` does not show scroll wins**: that stream is print-bound (few full-screen scrolls
per repetition), so ring vs pre-ring read parity within noise there while the scroll floors moved +28–65%. Measure
scroll with a CRLF-every-`cols` flood at the floor, or `flood-ab` in a real window, never kitten's `ascii`.

vtebench standings, 137×35, ms avg (felis / ghostty / kitty; bold = felis wins):

| benchmark                   | felis     | ghostty   | kitty |
| --------------------------- | --------- | --------- | ----- |
| scrolling_top_region        | 14.9 ±0.7 | 12.2 ±0.4 | 38.3  |
| scrolling_bottom_region     | 14.9 ±0.8 | 13.0 ±0.2 | 43.5  |
| scrolling\_\*\_small_region | 14.9–15.0 | 13.0      | 43.5  |
| scrolling                   | 15.2      | 11.1      | 91.3  |
| scrolling_fullscreen        | 21.2      | 16.0      | 171.2 |
| dense_cells                 | **5.0**   | 6.3       | 13.9  |
| medium_cells                | 6.2       | 4.9       | 17.9  |
| sync_medium_cells           | 6.9       | 4.9       | 27.8  |
| unicode                     | **3.0**   | 4.0       | 8.0   |

The remaining 1.1–1.4× scroll spread vs ghostty is the ordinary throughput band the project does not chase
(hold-the-line policy); kitty trails 3–8× on every scroll shape. Cross-terminal region numbers compare the same payload
only when the region is pinned explicitly: vtebench's setups read `tput lines`, which is empty when the terminfo entry
is in the 32-bit format Apple's ncurses 6.0 cannot read, and silently degrades regions to full-screen.

On Linux this family is ldisc-bound, not felis-bound (~32 bytes per PTY wake, drive at 12% of wall; a 50 µs
read-batching sleep probe made medium_cells 5× worse). Treat Linux scrolling deltas inside ±20 ms as parity unless
interleaved n≥3 says otherwise.

Linux PTY reader (2026-09-25). The reader thread, not the parser, is the larger daemon cost on vtebench: `scrolling`
arrives as ~13 bytes per `read(2)`, ~1.2 M reads/s, ~8 s of reader sys per 10 s. Do not batch those reads: output left
unread slows the producer's own kernel write path. A bare Python drain pausing 20–50 µs between reads (busy-wait or
sleep alike) took `scrolling` at 82×212 from ~160 to ~255 ms per sample with the writer at 100% sys either way, and a
felis reader that waited ≤50 µs while the parser was busy cut its CPU 93% (to ~900 B/read) but ran the harness scrolling
rows up to 2× slower. The Linux reader parks straight in `poll(2)`: the macOS spin gained no DOOM-fire fps there (five
interleaved pairs, 928 vs 930 mean) and cost 1.4–2.1 s of reader user time per 12 s, of which the extra `poll(2)` parks
give back ~0.4 s as sys.

## Cold window launch (2026-08-22)

The `startup` suite's felis (cold) bar carries the autospawn premium, and daemon boot is not it: `felis-daemon serve`
exec → socket connectable is ~4 ms (p50, 15 runs). `RetryPolicy::DAEMON_BOOT.initial_backoff` is 2 ms (attempt count
sized to keep the ~5 s ceiling); cold launch ~136 ms, premium over warm ~23 ms. The residual is the daemon's own first
attach (cold binary pages, first session) and is not chased. Measure with `felis --socket <tmp> -- true` in a loop,
killing the daemon matched on the socket between runs (`suites.suite_startup` does the same); measure bind time
separately by polling `connect()` at 1 ms; that is the number that says whether a wait constant is a wait or a sleep.

## Field standing (2026-08-20)

Like-for-like at 137×35 (felis release, ghostty nightly, kitty 0.42): kitten ascii 522.7 vs ghostty 401.4 MB/s, unicode
559.2 vs 324.5, csi 197.1 vs 135.5, long_escape_codes 609.5 vs 128.7; 150 MB live-window `cat` ~1.2× ahead (ascii 0.502
vs 0.601 s); termbench many_lines 225.4 vs 207.8, binary 164.9 vs 80.4; spawn→child-start 384 vs 446 ms with the daemon
autospawn included; every memory/CPU row (idle 88.3 vs 111.0 MB, flood CPU 0.18 vs 0.30 s). ghostty's one win is the
DOOM-fire payload drain, 0.302 vs 0.444 s (1.47×); the SGR-flood map above is the route to it. ghostty's ">2× faster
than any other leading fast terminal" claim does not hold against felis. Fresh numbers: `just bench-vs`.
