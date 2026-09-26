---
title: Testing
sidebar:
  order: 8
---

felis's verification stack: the test layers, their targets and acceptance bars, the commands that run them, and the CI
workflows that gate them. The strategy behind the layers is in [testing.md](../explanation/testing.md).

## Layer overview

| Layer                  | Purpose                                       | Source artifact / tool                                         |
| ---------------------- | --------------------------------------------- | -------------------------------------------------------------- |
| Principle invariants   | Encode the "Test:" lines from `principles.md` | [principles.md](../explanation/principles.md)                  |
| Workspace guards       | Invariants of the workspace itself            | Rust `#[test]`, `lychee`, and `cargo-deny`                     |
| Unit tests             | Component-internal correctness                | Rust `#[test]`                                                 |
| Property tests         | Algebraic invariants of parser, grid, reflow  | proptest                                                       |
| Fuzzing                | Robustness against hostile bytes              | cargo-fuzz over libFuzzer                                      |
| Bounded model-checking | Exhaustive proof of SWAR / arithmetic kernels | Kani                                                           |
| Snapshot tests         | Serialized output for representative inputs   | insta                                                          |
| Conformance tests      | VT100-VT520 + xterm sequence behavior         | vttest + esctest2                                              |
| Protocol conformance   | Kitty graphics / sizing / keyboard wire specs | Upstream Kitty specs                                           |
| Integration tests      | End-to-end daemon <-> client                  | Real PTY + spawned shell                                       |
| Performance benchmarks | Hot-loop throughput and latency               | Criterion                                                      |
| Mutation testing       | Whether the other layers bite                 | cargo-mutants                                                  |
| Visual regression      | Renderer correctness over time                | Out of scope for v1 ([explanation](../explanation/testing.md)) |

## Principle invariants

Source: [principles.md](../explanation/principles.md).

Every principle in `principles.md` ends with one or more Test clauses. These testable invariants form the highest-level
test specification. Each row pairs a Test clause with the concrete artifact that enforces it.

| Principle | Test clause (from `principles.md`)                                                                                                                                              | Enforced by                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1         | Any feature that lets one window display more than one PTY at the same time is rejected.                                                                                        | Architectural: `pool::Session` owns one PTY triple; client `Connection` attaches to one `SessionId`. Exercised by `crates/felis-client-core/src/connector/tests.rs::killed_client_reattaches_and_observes_prior_output`.                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| 1         | If a dedicated tool at the shell, WM, or external-process layer does a feature as well or better, or if no real consumer needs it yet, the terminal does not implement it.      | Review-time: guarded by the `/principle-check` skill.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| 1         | Any proposal that introduces a runtime evaluator inside the felis process is rejected; proposals that grow the IPC vocabulary or add typed variants to the action enum are not. | Workspace check: `cargo deny check bans` rejects embeddable scripting hosts listed in `deny.toml`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 2         | If "this would look better in raw Kitty" is ever true for the same input, felis has a bug, not a feature gap.                                                                   | Snapshot + property tests: `crates/felis-grid/tests/snapshot_csi.rs`, `crates/felis-grid/tests/snapshot_csi_handlers.rs`, `crates/felis-grid/tests/snapshot_osc.rs`, `crates/felis-grid/tests/proptest_osc.rs`, `crates/felis-grid/tests/proptest_osc_52.rs`, `crates/felis-grid/tests/proptest_sync_output.rs`, `crates/felis-grid/tests/proptest_mouse.rs`, `crates/felis-vt/tests/snapshot_kitty_graphics.rs`, `crates/felis-vt/tests/snapshot_kitty_text_sizing.rs`, `crates/felis-vt/tests/proptest_kitty_graphics.rs`, `crates/felis-vt/tests/proptest_kitty_text_sizing.rs`, `crates/felis-vt/tests/proptest_placeholder.rs`, `crates/felis-vt/tests/proptest_parser.rs`. |
| 2         | Any optimization PR that does not show a profiler trace pointing at parser, shaper, atlas, or grid-diff code is suspect.                                                        | Criterion baselines: `crates/felis-vt/benches/parser_throughput.rs`, `crates/felis-shaping/benches/shape_cache.rs`, `crates/felis-render-wgpu/benches/atlas.rs`, `crates/felis-grid/benches/damage_merge.rs`, `crates/felis-protocol/benches/ipc_throughput.rs`.                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 3         | `pkill felis-client` followed by relaunch must not affect the running shell or its scrollback.                                                                                  | Detach/reattach exit gate: `crates/felis-client-core/src/connector/tests.rs::killed_client_reattaches_and_observes_prior_output`. Rehydration snapshot: `crates/felis-client-core/tests/rehydration_snapshot.rs`. Image replay: `serve::tests::rehydrate_replays_persisted_images_and_placements`.                                                                                                                                                                                                                                                                                                                                                                               |
| 3         | If changing a client config requires a daemon restart, the boundary has been crossed.                                                                                           | Architectural: `felis-grid` has zero font or GPU dependencies, pinned by `crates/felis-grid/tests/no_render_deps.rs`. The reload path is client-local: `felis-client-core` config watcher tests and `felis-client` reload tests.                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 4         | If a feature's behavior depends on parsing the _content_ of the shell output (rather than escape sequences), it is a heuristic and gets pushed out.                             | Review-time: guarded by the `/principle-check` skill.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |

## Workspace guards

One test asserts a property of the workspace itself rather than of any crate in it. It lives in the `tests/` member
([workspace.md](workspace.md)).

| Guard                            | Pins                                                                |
| -------------------------------- | ------------------------------------------------------------------- |
| `tests/doc_source_references.rs` | Every bare citation of a Rust source file under `crates/` resolves. |

Markdown links are checked separately by `just docs-links`, which runs `lychee` without network access. A source path
cited after a rename fails `cargo nextest run --workspace`.

## Unit tests

Source: design docs; standard Rust `#[test]`.

Specific fixed-points the design docs commit to that map cleanly to unit tests:

- **Parser dispatch** matches the Williams DFA documented transitions for every state and byte-range pair
  (<https://vt100.net/emu/dec_ansi_parser>; [implementation.md](../explanation/implementation.md)).
- **UTF-8 boundary handling**: chunked input split mid-codepoint produces the same grapheme stream as unsplit input
  ([implementation.md](../explanation/implementation.md)).
- **Cell ASCII fast path**: single-codepoint, no-combiner, single-cell-width inputs do not allocate a side table
  ([grid-and-cells.md](../explanation/data-model/grid-and-cells.md)).
- **Cluster compositing**: a multi-codepoint cluster emits one instance per shaped glyph, a combining mark stacked on
  its base by the shaper GPOS offset (`cluster_cell_emits_one_instance_per_glyph_stacked_by_gpos`), and a base plus
  combining mark shapes to at least the base glyph without panicking
  (`combining_mark_cluster_shapes_without_panicking`); an OSC 66 sized cluster scales the pen advance and GPOS by the
  glyph scale (`sized_cluster_scales_pen_and_gpos_by_glyph_scale`); a single-glyph cluster is placed identically to the
  equivalent `Char` cell (`single_glyph_cluster_matches_char_placement`); a glyph id resolved against two faces keys on
  the shaping face `font_id` (`glyph_id_slots_key_on_font_id_so_faces_do_not_collide`)
  ([text-shaping.md](../explanation/rendering/text-shaping.md)).
- **SGR-pen incrementality**: applying SGR resets equals re-emitting the full cleared state
  ([implementation.md](../explanation/implementation.md)).
- **Damage granularity**: full-screen events (resize, alt-screen switch) call `mark_all()` so every row ships; partial
  updates mark and drain rows individually with no collapse threshold
  ([damage-tracking.md](../explanation/rendering/damage-tracking.md)).
- **Reflow round-trip**: `Grid::reflow` from W to W' and back to W restores original logical line breaks while no
  scrollback eviction fires (`reflow_soft_wrap_narrow_then_widen_round_trips` and cursor, prompt-mark, wide-glyph cases
  in `felis-grid`; [grid-and-cells.md](../explanation/data-model/grid-and-cells.md)).
- **Image lifecycle**: an image with refcount 0 is evicted before a new arrival exceeds the per-session byte cap
  ([image-store.md](../explanation/data-model/image-store.md)).
- **Connection-driver state machine**: `crates/felis-transport/src/driver/tests.rs` pins phase, direction, and
  correlation contracts: sequence numbers advancing independently, stream classification, items racing a cancel being
  dropped rather than fatal, one terminal per stream, stream bounds refusing requests instead of connections, and one
  connection death leaving others untouched ([ipc.md](ipc.md)).
- **Sparse registry installs**: a client table told about id 100 before 1-99 distinguishes the gap from an entry and
  lets low ids backfill slots (`installing_a_high_id_first_leaves_a_gap_that_a_later_low_id_backfills` in `felis-grid`
  `link_table` and `cluster_table` tests). A row naming an entry that never arrived ends the attachment instead
  (`a_row_naming_an_unarrived_registry_entry_ends_the_attachment` in `felis-client-core` shadow tests;
  [grid-and-cells.md](../explanation/data-model/grid-and-cells.md)).
- **Grid admission**: every daemon-impossible coordinate, count, and registry value is refused before the shadow changes
  a cell, at the exact bound and one past it, while both directions of a resize race still apply (`felis-client-core`
  `shadow.rs` tests; [ipc.md](ipc.md) "Grid admission"). One refusal costs the attachment alone: the session and its
  other subscribers keep running (`an_attachment_that_closes_leaves_the_session_and_its_peers_running` in `felis-daemon`
  `serve/session_task.rs`). The send side is pinned against the same rule: a resize retires queued scroll directives and
  a directive drained from the previous geometry downgrades to a row replay
  (`a_resize_retires_a_pending_scroll_directive`, `a_scroll_drained_after_a_resize_downgrades_to_a_row_replay`), and the
  shadow holds the authoritative cursor across an overruled optimistic resize
  (`an_overruled_local_shrink_restores_the_authoritative_cursor`), and a resize answers an idle mirror's armed pull
  (`a_resize_answers_an_idle_mirrors_armed_pull_exactly_once`, which also pins that the replay is shipped once and the
  mirror's next clean pull finds nothing). A registry id the peer sends twice, conflicting or identical, is refused
  (`a_registry_id_the_peer_sends_twice_is_refused`).
- **Visible-first rehydrate**: the attach burst ships only clusters its visible rows name, the skipped tail backfills on
  later cycles without manufacturing frames on quiet cycles, and `Ops` subscribers receive no registry entries
  (`felis-daemon` `serve/tests.rs`). Sent-set algebra is pinned in `serve/registry_sync.rs`.

## Property tests

Tool: proptest (<https://github.com/proptest-rs/proptest>).

Properties checked by proptest:

- **Parser totality**: for any byte sequence, the parser produces a well-formed event sequence and never panics
  ([security-model.md](../explanation/security-model.md)).
- **Grid bounds invariant**: cursor row, cursor col, and every emitted cell coordinate are strictly within grid bounds
  ([grid-and-cells.md](../explanation/data-model/grid-and-cells.md)).
- **Reflow stability**: for plain-text grid states, width round-trips under the scrollback cap restore the starting grid
  (`reflow_width_round_trip_restores_plain_text`). Arbitrary resizes, erases, and alt-screen toggles maintain structural
  invariants (`reflow_keeps_invariants_on_arbitrary_input`, `leaving_the_alt_screen_after_a_resize_keeps_scrolling`;
  `crates/felis-grid/tests/proptest_grid.rs`).
- **OSC payload safety**: titles and hyperlinks reject control bytes such as `\n`, `\r`, or `\x1b`
  ([security-model.md](../explanation/security-model.md)).
- **Damage union associativity**: `(a U b) U c == a U (b U c)`
  ([damage-tracking.md](../explanation/rendering/damage-tracking.md)).
- **Geometry admission**: create geometries stay inside REQ-605a budgets, out-of-range axes are refused, and resize
  clamping is idempotent (`crates/felis-protocol/tests/proptest_geometry.rs`, `felis-grid`
  `the_geometry_bounds_fit_the_held_cell_budget`).
- **Driver totality**: arbitrary frame sequences make progress or close with typed errors without panicking; stream
  tables stay consistent (`crates/felis-transport/tests/proptest_driver.rs`).

## Fuzzing

Tool: cargo-fuzz (<https://github.com/rust-fuzz/cargo-fuzz>) over libFuzzer (<https://llvm.org/docs/LibFuzzer.html>).

### Targets

Ten targets live under `fuzz/fuzz_targets/`, discoverable via `cargo fuzz list`:

- **`vt_parser`**: arbitrary byte sequences fed to the VT parser.
- **`kitty_graphics`**: payload bytes for APC G... ST sequences.
- **`ipc_frame`**: Unix socket framing layer (header and length caps).
- **`ipc_body`**: framed protobuf payloads and domain conversion.
- **`row_codec`**: row wire decoding and canonical re-encoding round-trips ([row-codec.md](row-codec.md)).
- Additional targets: `grid_dispatch`, `kitty_text_sizing`, `sync_output`, `scrollback_search`, and `chord_parser`.

### Acceptance criteria

Two bars run at two cadences. The qualification bar comes from [security-model.md](../explanation/security-model.md): a
target passes a 24-hour run with zero panics, zero OOMs, and bounded peak memory. That run is manual, on a machine that
can spare a day per target.

CI enforces the tripwire below it. `fuzz.yml` runs `just fuzz-smoke` (10,000 executions per target) on pushes to `main`
and pull requests targeting it, skipping documentation-only changes, and `just fuzz-long` nightly, which is 600 seconds
per target followed by a corpus minification pass. A finding at either cadence fails the run; a clean nightly is not a
qualification.

### Running fuzz targets

| Task                              | Command                   |
| --------------------------------- | ------------------------- |
| Smoke pass across all targets     | `just fuzz-smoke`         |
| Long-run with corpus minification | `just fuzz-long`          |
| Run a specific target             | `cargo fuzz run <target>` |
| Prime working corpus from seeds   | `fuzz/seed-corpus.sh`     |

### Curated seed corpus

`fuzz/seeds/<target>/` stores bug-trail seeds: minimal raw byte inputs surfacing edge cases or fixed regressions.
Running `fuzz/seed-corpus.sh` copies these into `fuzz/corpus/<target>/`.

To add a seed: minimize the byte sequence, write it to `fuzz/seeds/<target>/<name>.bin`, and re-run
`fuzz/seed-corpus.sh`.

## Snapshot tests

Tool: insta (<https://github.com/mitsuhiko/insta>).

A snapshot test serializes an output and compares it against a checked-in `.snap` file.

Categories covered:

- **Per-CSI and OSC sequences**: supported sequences in [protocols/support-matrix.md](protocols/support-matrix.md).
- **Kitty graphics**: command combinations listed in [protocols/kitty-graphics.md](protocols/kitty-graphics.md).
- **Reflow scenarios**: pre- and post-resize states for wrapped lines, sized-text spans, and image placements.
- **Rehydration**: pre-detach and post-attach grid and image state.
- **Recorded PTY replays**: `crates/felis-grid/tests/ref_recordings.rs` replays
  `crates/felis-grid/tests/ref/<scenario>/` (`recording.bin` + `size.json`).
- **CLI help pages**: `crates/felis-cli/src/tests.rs` renders every visible help page into
  `crates/felis-cli/src/snapshots/felis__tests__every_visible_help_page_is_snapshotted.snap`.

Snapshot workflow:

```
just snapshot-test     # run tests and stage changes
just snapshot-review   # interactive cargo insta review
just snapshot-accept   # accept all staged snapshots
```

## Conformance tests

### vttest

Source: <https://invisible-island.net/vttest/vttest.html>.

Reference tool for VT100-VT520 and xterm conformance.

`crates/felis-pty/tests/vttest_smoke.rs` automates 27 active scenario round-trips against the grid; seven further
harnesses are `#[ignore]`d diagnostic probes that dump a menu screen rather than assert anything. Deliberate
non-conformances recorded in [vt-compliance.md](../explanation/protocols/vt-compliance.md) (charset switching is a
no-op, DECDLD soft characters are unsupported, DECCOLM does not resize) are excluded from failure criteria.

### Kitty protocol conformance

Tests verify conformance against upstream specifications:

- **Graphics protocol** (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>): chunk sizes, transmission methods
  (`t=d`, `t=f`, `t=t`, `t=s`), formats (`f=24`, `f=32`, `f=100`), zlib compression (`o=z`), response framing, and
  Unicode placeholder mapping.
- **Text sizing protocol** (<https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>): OSC 66 metadata fields (`s`, `w`,
  `n`, `d`, `v`, `h`) and clamping.
- **Keyboard protocol** (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>): progressive enhancement flags, modifier
  bitfields, functional key codes (PUA 57344-63743), and legacy exceptions.

## Integration tests

Permanent end-to-end regression specifications:

- **Real-binary sessions**: `crates/felis-daemon/tests/real_app_harness.rs` runs the daemon over an in-memory carrier
  with a real child on the PTY and mirrors the `GridMsg` stream into a `ShadowScreen`. `/bin/sh` drives the burst,
  escape-barrage and pull-pacing cases; `less` pages long stdin; `nvim` enters the alt screen. The cases that need
  `less`, `nvim`, or coreutils resolve them on `PATH` and skip when absent, so a machine missing one loses that case
  rather than the run; the shell-driven cases assume a POSIX `/bin/sh`. The file also holds `#[ignore]`d measurement
  probes ("Contributor environment variables" below).
- **Detach/attach persistence**: closing the client window keeps the shell alive; a new client re-hydrates the visible
  screen, and scrollback stays in the daemon for later viewport requests. Tested in
  `crates/felis-client-core/src/connector/tests.rs` (`killed_client_reattaches_and_observes_prior_output`).
- **Producer byte streams**: what a graphics or text-sizing producer emits is pinned as bytes rather than by launching
  the application: yazi's `KgpOld` preview burst in `crates/felis-daemon/tests/yazi_kgp_old_direct_placement.rs`, OSC 66
  streams shaped like presenterm's and yazi's output in `crates/felis-grid/tests/e2e_osc_66_producers.rs`, and captured
  PTY recordings replayed by `crates/felis-grid/tests/ref_recordings.rs` ("Snapshot tests" above).
- **Bridge lifecycle**: `crates/felis-cli/tests/cli_bridge.rs` drives the `felis bridge` binary with piped stdio against
  a daemon, verifying correlation, error framing, cancel cleanup, and stream exit.
- **Shell completion overlays**: `crates/felis-cli/src/cli_completions.rs` tests generated fish and zsh completion
  scripts under real interpreters.

REQ-1206 ([spec.md](spec.md)) states the exit criteria for this layer, and part of it is met by hand rather than by a
test: an editor or file-manager session driven end to end through the GUI client. The automated coverage of the client
itself is the headless frontend smoke ("CI shape" below), which drives one window, one keystroke and one rendered frame.

## Machine-surface schemas

The two published JSON Schema documents (`crates/felis-cli/schemas/`, [cli.md](cli.md) "JSON Schema") are generated from
the serde types by `crates/felis-cli/src/cli_schema.rs`, which is compiled only under the crate's `schema` feature.
`just schema` rewrites both files, the config schema, and the `felis-json` v1 schema; without `UPDATE_SCHEMA` the same
tests assert byte equality, and CI's `--all-features` run is where that assertion fires.

`felis-json` v1 ([ipc.md](ipc.md) "Structural session JSON") publishes
`crates/felis-grid/schemas/felis-json-v1.schema.json`, generated by `crates/felis-grid/src/json_v1/schema.rs` under
`felis-grid`'s own `schema` feature. Two tests hold it: `the_felis_json_schema_is_up_to_date` is the staleness
assertion, and `no_published_object_forbids_additional_properties` enforces the format's compatibility rule, since a
closed object would make a later v1 writer's optional field a break. The golden frames under
`crates/felis-grid/tests/golden/json-v1/` are the positive fixtures, rewritten by `just golden`.

The bundle is checked from three directions:

- Every object the `cli_sessions.rs`, `cli_bridge.rs`, `cli_config.rs`, `cli_daemon.rs`, `cli_doctor.rs` and
  `cli_version.rs` suites parse is validated against it, so the objects the binary really writes are the positive
  fixtures.
- `crates/felis-cli/tests/fixtures/schema-invalid/` holds what the bundle must reject: `out-*.json` name the class they
  claim and carry a `repaired` twin that must validate, so a rejection cannot be credited to the wrong defect;
  `req-*.json` are whole bridge request lines. `tests/schema_fixtures.rs` checks the rejection, and `cli_bridge.rs`
  feeds the request half to a live bridge, which must answer `malformed_request`. An `above-u64` / `above-i64` fixture
  sits at the closest value a validator comparing as `f64` can still reject, not at the arithmetic maximum plus one:
  `schema_fixtures.rs` pins that gap separately ([cli.md](cli.md) "JSON Schema").
- `crates/felis-cli/tests/fixtures/bridge/*.jsonl` are golden conversations: `in` lines are written to a bridge's stdin,
  `out` lines are its stdout. They carry the lifecycle properties JSON Schema cannot state: exactly one terminal per id,
  nothing after it. Session ids, timestamps, and prose are masked and replies are compared per request id, because
  interleaving across ids is the bridge's prerogative; `just golden` regenerates them from a live run.

## Performance benchmarks

Tool: criterion (<https://docs.rs/criterion>).

### Targets

Every `[[bench]]` entry in the workspace is a Criterion target, and `tools/bench/criterion.py` discovers them from
`cargo metadata` rather than a list, so adding one to a crate's `Cargo.toml` is all it takes to enter `just bench-all`
and the nightly snapshot.

| Target                  | Crate               | Measures                                                                                    |
| ----------------------- | ------------------- | ------------------------------------------------------------------------------------------- |
| `parser_throughput`     | `felis-vt`          | Parser cost alone, without the grid or the dispatcher.                                      |
| `csi_dispatch`          | `felis-grid`        | CSI-heavy grid throughput, the `kitten __benchmark__ csi` shape.                            |
| `unicode_throughput`    | `felis-grid`        | Print path against a real `Grid` over synthesized code-point ranges.                        |
| `scroll_region`         | `felis-grid`        | `Grid::scroll_region_up` at 24x80: whole grid, partial region, `yes` flood.                 |
| `damage_merge`          | `felis-grid`        | The row-granularity damage walk `compose_diffs` performs every frame.                       |
| `ipc_throughput`        | `felis-protocol`    | Body encode, frame wrap, frame decode, body decode for three body shapes.                   |
| `socket_write`          | `felis-transport`   | Framed writes over a real `UnixStream`, buffered against unbuffered.                        |
| `end_to_end_throughput` | `felis-client-core` | PTY parse to shadow apply, reported in input bytes for comparison with `parser_throughput`. |
| `client_consume`        | `felis-client-core` | The client's frame-read, decode and `ShadowScreen::apply` ceiling.                          |
| `shape_cache`           | `felis-shaping`     | `ShapeCache::get_or_insert` hit against miss.                                               |
| `shape_run`             | `felis-shaping`     | `Shaper::shape_run` with a warm shaper against a fresh one per iteration.                   |
| `atlas`                 | `felis-render-wgpu` | `GlyphIndex::ensure` on an atlas hit against a miss.                                        |
| `shape_memo`            | `felis-render-wgpu` | `GlyphIndex::shape_run_cached` on a memo hit against the miss that runs `shape_run`.        |
| `video_frame`           | `felis-render-wgpu` | `image_atlas::write_rgba` per-frame RGB-to-RGBA expansion.                                  |

`socket_write` declares `required-features = ["test-util"]` because the raw frame write sits behind the authorization
boundary in every other build; the orchestrator reads the requirement from the manifest and passes the feature.

### Running benchmarks

`tools/bench/criterion.py` coordinates Criterion benchmarks:

| Task                              | Command                                                                                             |
| --------------------------------- | --------------------------------------------------------------------------------------------------- |
| Headline end-to-end throughput    | `just bench`                                                                                        |
| Run all Criterion targets         | `just bench-all` (`--quick` for fast runs)                                                          |
| Generate Markdown report          | `just bench-report`                                                                                 |
| Check regression against baseline | `just bench --save-baseline before`, modify code, `just bench --baseline before`, `just bench-gate` |
| Orchestrator self-tests           | `just bench-selftest`                                                                               |

The gate fails if the mean change exceeds the threshold (15% by default).

## Damage-tracking correctness

Source: [damage-tracking.md](../explanation/rendering/damage-tracking.md).

The invariant is **no underdraw**: every cell whose visible content changed must reach the client next paint. Overdraw
is permitted.

The grid-layer test harness lives in `crates/felis-grid/tests/damage_correctness.rs`. It applies the queued
`PtyEffect::Scrolled` directives to the prior screen and requires every row outside `Damage::dirty_rows()` to equal the
grid's.

Covered scenarios:

- Single-cell SGR and glyph mutations.
- Single-row and multi-row scroll pushes.
- Cursor motion leaves damage empty, as fixed cases and as a property over random motion sequences.
- Synchronized output bracket release.
- Random shell sequence property tests (`shell_byte_strategy`), scroll margins, `IL` / `DL` and background-colored
  blanks included.

The daemon-to-client harness is `every_mirror_ends_each_compose_holding_the_grids_cells` in
`crates/felis-daemon/src/serve/tests.rs`: random output (scroll margins, `IL` / `DL`, DECSLRM, the alternate screen,
scrollback clears, cursor motion), drain cadences, resizes, scrollback browsing and a separate pull cadence for each of
two subscribers, after which every subscriber that composed at the live view must hold the grid's cells, cursor and
soft-wrap bits in a real `ShadowScreen`. Each row entry is also checked against the 0.1.0 client's row write, which
skips cells matching the storage past a row's watermark.

The client-side harness is the `incremental_frames_match_a_full_rebuild` property test in
`crates/felis-render-wgpu/src/row_cache.rs`: random shadow writes, scroll directives, resizes, cursor moves and changes
to every frame-wide paint input except a non-empty shape frame, up to three per frame, under buffer limits that force
both slotted and packed rows, after each frame of which the renderer's cached row instances must equal a full rebuild.
Its populate-walk twin is `walking_the_marked_rows_matches_a_full_walk` in
`crates/felis-render-wgpu/src/glyphs/walk_tests.rs`: random writes, clusters, sized cells, scroll directives, resizes,
style sweeps and overlay priming, with and without shaping features and under atlases small enough to recycle, after
each of which the kept shape results must equal a from-scratch walk and the atlas must already hold every glyph a full
walk primes.

## Security tests

Source: [security-model.md](../explanation/security-model.md).

Key threat model verifications:

- **Output-to-input injection, identity queries**: queries (DA1, DA2, DA3, XTVERSION, XTGETTCAP) return static
  identities independent of prior terminal output; XTGETTCAP `TN` answers the `TERM` fixed when the session was spawned.
- **Output-to-input injection, reflective queries**: queries return only sanitized, session-local state
  (`crates/felis-grid/tests/proptest_osc_palette.rs`, `crates/felis-grid/tests/proptest_osc_52.rs`).
- **OSC payload sanitization**: control characters (`\n`, `\r`, `\x1b`) in titles or hyperlinks truncate or reject the
  sequence.
- **Image decoder budgets**: decompressed pixel counts and dimensions exceeding configured budgets abort decode and emit
  bell errors.
- **Symlink traversal**: file reads open the final path component with `O_NOFOLLOW` (`t=t` through `openat` under a
  pinned parent).
- **Peer authentication**: peer UID is checked via socket credentials before greeting.
- **Frame length ceiling**: frames exceeding configured caps tear down connections without allocation. Pinned in
  `crates/felis-protocol/src/frame.rs` and fuzz targets.
- **Per-operation limits**: REQ-105a payload bounds are checked in `crates/felis-protocol/src/messages/limits.rs`.
- **Receiver-side claim bounds**: `crates/felis-client-core/tests/hostile_claims.rs` verifies maximal scalar claims
  decode under 1 MiB peak heap under DHAT.
- **Connection isolation**: corrupt frames terminate only the offending connection
  (`a_corrupt_frame_kills_its_own_connection_and_nothing_else` in `felis-daemon` `serve/tests.rs`).
- **Bidirectional override visibility**: Trojan Source characters (U+202A-U+202E, U+2066-U+2069) render visible
  replacement markers.

Standing audits are documented in [security-audits.md](security-audits.md).

## Kani proof inventory

Tool: Kani (<https://model-checking.github.io/kani/>).

Kani model-checks self-contained SWAR and arithmetic kernels for absence of panics and assertion violations across
bounded inputs.

Inventory (28 proofs across 11 files):

- **`felis-vt`**: UTF-8 boundary decoding (`utf8.rs`), printable-run SWAR scan and CSI parameter accumulator
  (`kani_proofs.rs`), Kitty text sizing field parsing (`kitty_text_sizing.rs`), Kitty graphics parsing
  (`kitty_graphics.rs`).
- **`felis-protocol`**: base64 decoding safety (`base64.rs`), frame header decoding (`frame.rs`), Unicode placeholder
  arithmetic (`kitty_graphics/placeholder.rs`), announced geometry admission (`messages.rs`).
- **`felis-grid`**: X11 color parsing (`osc_color.rs`), LEB128 and UTF-8 wire decoding kernels (`wire.rs`).
- **`felis-client-core`**: chord-string splitting, ASCII-case-insensitive key-name lookup, and F-key index arithmetic
  (`keymap/chord.rs`).

### Running proofs

Proofs run via the dedicated Kani dev shell:

```
just kani                     # run all proofs
just kani --harness <name>    # run a specific proof
```

Drift is checked weekly in CI via `.forgejo/workflows/kani.yml`.

## Mutation testing

Tool: cargo-mutants (<https://mutants.rs/>).

cargo-mutants mutates the production code and reruns the suite: a mutant no test fails on is a branch nothing pins, or a
test whose oracle is too weak. `.cargo/mutants.toml` scopes the sweep to `felis-grid`, `felis-vt` and `felis-protocol`,
routes it through nextest under the `mutants` cargo profile, and excludes `kani_proofs` and the committed prost codegen.

| Task                        | Command                  |
| --------------------------- | ------------------------ |
| Scoped sweep                | `just mutants`           |
| Mutants touched by the diff | `just mutants-pr [base]` |
| One shard of the full sweep | `just mutants-shard 0/8` |

`-F <regex>` narrows a run to one function or file. Runs are periodic and on demand; no workflow gates on the result,
and the triage procedure is in the `test-strategy` skill.

## Contributor environment variables

These variables steer tests, probes, and harnesses in a checkout. None is part of the product surface: the three the
client binary itself reads are measurement hooks, and no compatibility promise covers any of them (the runtime
environment a user may set is [terminal-identity.md](terminal-identity.md)). The dev shell already exports the ones a
normal `just check` needs.

| Variable                                                                                    | Read by                                                                                        | Effect                                                                                                                                                                   |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `FELIS_TEST_FONT_DIR`                                                                       | `felis-shaping`, `felis-render-wgpu` `glyphs.rs`                                               | Directory holding the pinned probe font. Font-dependent tests and benches skip when it is unset and panic when it is set but wrong. `dev/flake-module.nix` exports it.   |
| `FELIS_SMOKE_MARKER`                                                                        | `felis-client` `smoke.rs`                                                                      | A non-empty value puts the client in frontend-smoke mode: type the marker, read it back from the rendered frame, exit. `just smoke` and `just smoke-headless` set it.    |
| `FELIS_SMOKE_TIMEOUT_MS`                                                                    | `felis-client` `smoke.rs`                                                                      | Smoke deadline in milliseconds; 90,000 by default, sized for a cold Windows runner.                                                                                      |
| `FELIS_STARTUP_EXIT_MS`                                                                     | `felis-client` `event_handler.rs`                                                              | Exit this many milliseconds after the renderer is ready, so a non-interactive run captures warm RSS and the `dhat-heap` dump.                                            |
| `UPDATE_SCHEMA`                                                                             | `felis-cli` `cli_schema.rs`, `felis-client-core` `config.rs`, `felis-grid` `json_v1/schema.rs` | Rewrite the committed JSON schemas instead of asserting byte equality. `just schema` sets it.                                                                            |
| `UPDATE_GOLDEN`                                                                             | `felis-cli` `tests/cli_bridge.rs`, `felis-grid` `tests/json_v1.rs`                             | Rewrite the golden bridge conversations from the live run, and the golden `felis-json` v1 frames. `just golden` sets it.                                                 |
| `VTTEST_BIN`, `ESCTEST_BIN`                                                                 | `felis-pty` `tests/vttest_smoke.rs`, `tests/esctest_smoke.rs`                                  | Absolute path to the conformance binary; without it the harness resolves the name on `PATH` and skips when absent.                                                       |
| `ESCTEST_INCLUDE`                                                                           | `felis-pty` `tests/esctest_smoke.rs`                                                           | Pattern passed to esctest's `--include`, to run one case group instead of the full suite.                                                                                |
| `VTTEST_MENU`, `VTTEST_STEPS`, `VTTEST_PATH`, `VTTEST_OUTER`, `VTTEST_INNER`, `VTTEST_LEAF` | `felis-pty` `tests/vttest_smoke.rs`                                                            | Menu coordinates for the `#[ignore]`d diagnostic probes that dump a vttest screen.                                                                                       |
| `FELIS_CAT_FILE`, `FELIS_CAT_ITERS`                                                         | `felis-daemon` `tests/real_app_harness.rs`                                                     | Payload and repeat count for the `#[ignore]`d cat-throughput probes; without the file they skip.                                                                         |
| `FELIS_PULL_MS`, `FELIS_ECHO_PULL`                                                          | `felis-daemon` `tests/real_app_harness.rs`                                                     | Pull-pacing period in milliseconds (16 is a 60 Hz client, 0 pulls continuously) and whether the echo-latency probe paces at all.                                         |
| `FELIS_BENCH_FILE`, `FELIS_BENCH_ROWS`, `FELIS_BENCH_COLS`                                  | `felis-daemon` `parse_sink.rs`, `serve/tests.rs`                                               | Payload and its capture geometry for the `#[ignore]`d parse-floor and attached-drain measurements; a payload replayed at the wrong grid becomes a wrap-and-scroll storm. |
| `FELIS_PROBE_*`                                                                             | `felis-grid` `scrollback_tests.rs`                                                             | Geometry, cap, occupancy, line count, iterations and cache pressure for the `#[ignore]`d cold-print ring probe.                                                          |
| `THRESHOLD`, `ALLOWLIST`, `CRITERION_ROOT`                                                  | `tools/bench/criterion.py`                                                                     | Regression fraction that fails the gate, newline-separated benchmark ids exempt from it, and the Criterion output root. `bench.yml` sets `THRESHOLD` and `ALLOWLIST`.    |
| `FELIS_SYSTEMD_TESTS`                                                                       | `felis-cli` `tests/cli_systemd_handoff.rs`                                                     | `1` runs the systemd hand-off tests against the live user manager (a transient unit per test, on its own socket path); without it they return at once.                   |
| `FELIS_PROSE_CHECK_SKIP`                                                                    | `tools/prose_check.py`                                                                         | `1` makes the prose gate exit zero, for a commit whose prose is checked another way.                                                                                     |

## CI shape

CI runs on Forgejo Actions (<https://forgejo.org/docs/latest/user/actions/>) with configuration under
`.forgejo/workflows/`. The one workflow under `.github/workflows/`, `release-mirror.yml`, gates nothing: it copies a
published Forgejo release to the GitHub mirror and opens the Homebrew tap's version bump ([workspace.md](workspace.md)
"Release gate").

### Workflows

| Workflow      | Triggers                  | Key jobs and gates                                                                                                                                                                                                                       |
| ------------- | ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pr.yml`      | Push, PR                  | `nix flake check` (formatting, lints, MSRV, doc checks), `build` (clippy, nextest, cargo deny, wasm32 portable-core clippy, `just selftest`), `proto-compat` (wire checks), Linux headless smoke (`smoke-headless`), Linux package push. |
| `windows.yml` | Push, PR                  | Windows cross-clippy (MinGW), Windows nextest, Windows headless smoke, Windows package artifact.                                                                                                                                         |
| `darwin.yml`  | Push, PR                  | macOS build check on aarch64-darwin (`nix build .#felis .#felis-dist`, the package and the release archive) and, on `main`, a cache push of the package.                                                                                 |
| `fuzz.yml`    | Push, PR, Nightly, Manual | Smoke fuzzer run (push and PR) and a 10-minute-per-target nightly run with corpus minification.                                                                                                                                          |
| `kani.yml`    | Weekly, Manual            | Weekly full verification of in-tree Kani proofs.                                                                                                                                                                                         |
| `bench.yml`   | PR, Nightly, Manual       | Criterion regression gate against the PR's base commit and nightly full snapshot.                                                                                                                                                        |
| `release.yml` | Tag (`v*`)                | Verifies tag revision, reruns the Linux, macOS and Windows chains on it, pushes both Nix targets to the cache, and attaches the Linux and macOS archives, the Windows zip, the config schema and the proto to the release.               |

### Frontend smoke test

The Linux and Windows CI pipelines run a client smoke test. The client connects to a daemon, types an input marker,
verifies terminal echo, and reads back the rendered GPU frame. On Linux (`just smoke-headless`) it runs under a virtual
display (Xvfb) on software Vulkan (lavapipe); on Windows it runs `felis-client.exe` in the runner's desktop session on
the native graphics backend. A pull request drives the debug Cargo build; `main` and tags drive the shipped build (the
Nix package on Linux, the release build on Windows). The Linux job also runs `just test-gpu`: the `felis-render-wgpu`
tests that read pixels back from an offscreen frame, which are `#[ignore]`d in the default run because the noop backend
the other renderer tests use never executes a pass.

### Persistent build directory

Cargo jobs on the Linux and Windows runners share one build directory per repository, outside the job workspace:
`$HOME/.cache/forgejo-ci/<owner>/<repo>/target` on Linux and `D:\forgejo-ci\<owner>\<repo>\target` on the Windows
guest's state disk. The `cargo-env` action claims it at the start of a job, and `cargo-env/done`, the job's last step
under `if: always()`, releases the claim. A job that never reaches that step (cancelled, past its job timeout, or cut
off by a runner or host restart) leaves its claim behind.

The next job rebuilds the directory from scratch when any of these holds:

- a job that has ended, or started more than 6 hours ago, left its claim behind;
- the directory is more than 7 days old, or carries no record of when it was built;
- the toolchain (`rustc -vV`), `Cargo.lock`, or a `CARGO_PROFILE_*` override differs from the ones it was built with, on
  any event but a pull request;
- on Windows, D: has less than 8 GB free.

While another running job holds a claim, the rebuild is deferred to the next job that finds the directory unclaimed.
Each job's log names the reason for a rebuild and, at the end, the directory's size and the PDB bytes the job wrote.
Windows builds the dev profile with `CARGO_PROFILE_DEV_DEBUG=line-tables-only`.

### Wire compatibility gates

Changes to `felis.proto` must preserve wire compatibility, verified by seven checks:

| Gate                                 | Execution point                                      | Coverage                                                                                                                                                                                                                                                                                                                                        |
| ------------------------------------ | ---------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `buf breaking` (`WIRE_JSON`)         | _proto-compat_, `just proto-compat`, pre-commit hook | Field numbers, types, oneof membership, enum names. Field options are outside its comparison, so an arm's routing row is checked by the arm-table sync test alone.                                                                                                                                                                              |
| Release baseline                     | _proto-compat_ on a tag                              | Which schema a tag is measured against: the `felis.proto` at the tag of the forge's newest published final release, read out of the checkout's history. While no final release is published, a tag answers to no baseline. An unreachable or unparseable answer, or a release tag missing from the clone, fails the run rather than passing it. |
| Minor-ledger sync test               | _build_, `just test`                                 | `PROTOCOL_MINOR` matches ledger in [ipc.md](ipc.md#the-minor-ledger), and every arm, field, closed-enum value and row-codec version the send gate authorizes is named in its row — and every identifier a row names is authorized from that minor.                                                                                              |
| Send-gate tests                      | _build_, `just test`                                 | A synthetic addition one minor past this build is refused by `FrameWriter`, the writer survives the refusal, and the same body goes out on a connection that defines the minor.                                                                                                                                                                 |
| Correlation-gate tests               | _build_, `just test`                                 | The encoder refuses a correlated arm sent with no envelope, an uncorrelated arm sent with one, and an id of the wrong kind for the class in either direction; each honest pairing goes out through `FrameWriter`, and the client queue refuses a mismatch of its own.                                                                           |
| Row-codec and carrier golden vectors | _build_, `just test`                                 | Opaque `packed_cells` and preface byte representations ([row-codec.md](row-codec.md)).                                                                                                                                                                                                                                                          |
| Arm-table sync test                  | _build_, `just test`                                 | Proto `(felis.v1.arm)` field options match Rust `ArmMeta` declarations ([ipc.md](ipc.md#the-arm-table)).                                                                                                                                                                                                                                        |

Pre-release breaking changes are acknowledged in `crates/felis-protocol/proto/BREAKING.md` by recording the base commit
SHA (`base: <sha>`).
