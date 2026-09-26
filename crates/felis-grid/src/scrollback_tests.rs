use crate::test_support::drive;
use crate::*;
use felis_vt::Parser;

/// REQ-605 publishes the number itself; every other test spells it
/// symbolically.
#[test]
fn default_scrollback_rows_is_ten_thousand() {
    assert_eq!(DEFAULT_SCROLLBACK_ROWS, 10_000);
}

/// Sums capacity, not length: the ring reserves its whole retention window
/// up front, and that reservation is what REQ-605a budgets.
fn held_cell_bytes(g: &Grid) -> usize {
    let saved: usize = [
        g.screen.saved_primary.as_ref(),
        g.screen.saved_alternate.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|s| s.cells.capacity() * size_of::<Cell>())
    .sum();
    g.reserved_cell_bytes() + saved
}

/// Primary ring plus one parked viewport, the state with the most cell
/// buffers alive at once.
const fn worst_case_held_cell_bytes(rows: usize, cols: usize) -> usize {
    (rows + DEFAULT_SCROLLBACK_ROWS + rows) * cols * size_of::<Cell>()
}

/// REQ-605a: `MAX_GRID_ROWS` and `MAX_GRID_COLS` fit `GRID_RESERVATION_BUDGET_BYTES`,
/// while doubling them exceeds budget
/// (`docs/explanation/architecture/session-lifecycle.md` "Geometry bounds").
const _: () = {
    use felis_protocol::messages::{GRID_RESERVATION_BUDGET_BYTES, MAX_GRID_COLS, MAX_GRID_ROWS};

    let rows = MAX_GRID_ROWS as usize;
    let cols = MAX_GRID_COLS as usize;
    assert!(worst_case_held_cell_bytes(rows, cols) <= GRID_RESERVATION_BUDGET_BYTES);
    assert!(worst_case_held_cell_bytes(rows * 2, cols * 2) > GRID_RESERVATION_BUDGET_BYTES);
};

/// The reservation the const budget above assumes is the one the grid
/// actually holds.
#[test]
fn the_held_cell_bytes_match_the_budgeted_worst_case() {
    let (rows, cols) = (24usize, 80usize);
    let ring = (rows + DEFAULT_SCROLLBACK_ROWS) * cols * size_of::<Cell>();
    let viewport = rows * cols * size_of::<Cell>();
    let mut p = Parser::new();
    let mut g = Grid::new(rows as u16, cols as u16);
    assert_eq!(held_cell_bytes(&g), ring);

    // `?47` preserves the alt buffer on leave, so it reaches the state with
    // the most buffers alive at once.
    drive(&mut p, &mut g, b"\x1b[?47h");
    assert_eq!(held_cell_bytes(&g), ring + viewport);
    drive(&mut p, &mut g, b"\x1b[?47l");
    assert_eq!(held_cell_bytes(&g), ring + viewport);
    assert_eq!(worst_case_held_cell_bytes(rows, cols), ring + viewport);
}

#[test]
fn line_feed_at_bottom_scrolls_into_scrollback() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3");
    assert_eq!(g.cursor().row, 1);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'r'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'2'));
    assert_eq!(g.cell(1, 3).unwrap().grapheme, Grapheme::Ascii(b'3'));
    assert_eq!(g.scrollback().len(), 1);
}

#[test]
fn scrollback_is_empty_reflects_pushed_rows() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    assert!(g.scrollback().is_empty());
    drive(&mut p, &mut g, b"a\r\nb\r\nc");
    assert!(!g.scrollback().is_empty());
    assert_eq!(g.scrollback().len(), 1);
}

/// The daemon relies on `cell_at_viewport(0, …)` being the live grid.
#[test]
fn cell_at_viewport_zero_matches_live_cell() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3");
    for r in 0..g.rows() {
        for c in 0..g.cols() {
            assert_eq!(
                g.cell_at_viewport(0, r, c).unwrap(),
                g.cell(r, c).unwrap().clone(),
                "viewport=0 must equal live cell at ({r},{c})"
            );
        }
    }
}

#[test]
fn cell_at_viewport_lifts_youngest_scrollback_to_top_band() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3\r\nrow4");
    assert_eq!(g.scrollback().len(), 2);
    assert_eq!(
        g.cell_at_viewport(1, 0, 0).unwrap().grapheme,
        Grapheme::Ascii(b'r')
    );
    assert_eq!(
        g.cell_at_viewport(1, 0, 3).unwrap().grapheme,
        Grapheme::Ascii(b'2'),
        "youngest scrollback row should sit at the top",
    );
    assert_eq!(
        g.cell_at_viewport(1, 1, 3).unwrap().grapheme,
        Grapheme::Ascii(b'3'),
        "live row 0 should slide down to visible row 1",
    );
    assert_eq!(
        g.cell_at_viewport(2, 0, 3).unwrap().grapheme,
        Grapheme::Ascii(b'1'),
    );
    assert_eq!(
        g.cell_at_viewport(2, 1, 3).unwrap().grapheme,
        Grapheme::Ascii(b'2'),
    );
}

#[test]
fn clamp_viewport_saturates_at_scrollback_depth() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3");
    assert_eq!(g.scrollback().len(), 1);
    assert_eq!(g.clamp_viewport(0), 0);
    assert_eq!(g.clamp_viewport(1), 1);
    assert_eq!(g.clamp_viewport(99), 1, "must clamp to scrollback len");
}

/// No scrollback on the alternate screen (xterm convention).
#[test]
fn clamp_viewport_returns_zero_on_alternate_screen() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"row1\r\nrow2\r\nrow3");
    assert_eq!(g.scrollback().len(), 1);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(g.on_alternate_screen());
    assert_eq!(g.clamp_viewport(99), 0);
    assert_eq!(
        g.cell_at_viewport(99, 0, 0),
        g.cell(0, 0).copied(),
        "alt screen forces viewport composition to live",
    );
}

/// Scrollback rows narrower than the current `cols` must read as blank past
/// their stored width.
#[test]
fn cell_at_viewport_pads_short_scrollback_rows_with_blanks() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 6);
    drive(&mut p, &mut g, b"abcdef\r\nghijkl\r\n");
    assert_eq!(g.scrollback().len(), 1);
    g.resize(2, 8);
    let top = g.cell_at_viewport(1, 0, 7).unwrap();
    assert!(
        top.is_blank(),
        "padded tail of a short scrollback row must be blank, got {:?}",
        top.grapheme,
    );
    assert_eq!(
        g.cell_at_viewport(1, 0, 0).unwrap().grapheme,
        Grapheme::Ascii(b'a'),
    );
    assert_eq!(
        g.cell_at_viewport(1, 0, 5).unwrap().grapheme,
        Grapheme::Ascii(b'f'),
    );
}

/// Flood rows reach scrollback clipped at the occupancy watermark.
#[test]
fn flood_scroll_rows_reach_scrollback_occupancy_clipped() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"y\r\ny\r\ny\r\ny");
    assert_eq!(g.scrollback().len(), 2);
    for i in 0..2 {
        // The tail past the watermark holds recycled bytes; exposing it would
        // leak stale content.
        let row = g.scrollback().row(i).unwrap();
        assert_eq!(row.len(), 1, "history rows clip at their live extent");
        assert_eq!(row[0].grapheme, Grapheme::Ascii(b'y'));
    }
}

/// BCE blanks past the printed content must reach scrollback with their
/// background intact.
#[test]
fn bce_blanks_reach_scrollback_with_background() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"AB\x1b[44m\x1b[K\x1b[m\r\nx\r\nx");
    assert_eq!(g.scrollback().len(), 1);
    let row = g.scrollback().row(0).unwrap();
    assert_eq!(row[0].grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(row[1].grapheme, Grapheme::Ascii(b'B'));
    for c in &row[2..4] {
        assert_eq!(
            g.style(c.style).bg,
            Color::Indexed(4),
            "BCE background must survive the scrollback push"
        );
    }
}

/// Cold-print floor probe for unified viewport-into-history ring
/// (`docs/explanation/data-model/scrollback.md`
/// "Unified viewport-into-history ring").
/// Compares warm print, unified ring print, and push-prefix copy.
/// Override via `FELIS_PROBE_{ROWS,COLS,CAP,OCC,LINES,ITERS}`.
#[test]
#[ignore = "measurement; run with --run-ignored all --no-capture"]
fn measure_cold_print_ring_probe() {
    use std::hint::black_box;
    use std::time::Instant;

    fn env(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    let rows = env("FELIS_PROBE_ROWS", 35);
    let cols = env("FELIS_PROBE_COLS", 137);
    let cap = env("FELIS_PROBE_CAP", 10_000);
    let occ = env("FELIS_PROBE_OCC", 1).min(cols);
    let lines = env("FELIS_PROBE_LINES", 8_000_000);
    let iters = env("FELIS_PROBE_ITERS", 9);
    let ring_rows = rows + cap;

    // In isolation the streaming history write is hidden by the prefetcher,
    // so the RFO half of the push never appears and A3-A1 falls far under
    // ~9 ns/line. This competitor churns a working set, identically in every
    // arm, so the streamed destinations actually miss; sweep `PRESSURE_KB`
    // until A3-A1 reproduces ~9.
    let pressure_kb = env("FELIS_PROBE_PRESSURE_KB", 0);
    let touches = env("FELIS_PROBE_TOUCHES", 8);
    let pw = (pressure_kb * 128).next_power_of_two().max(1);
    let pmask = pw - 1;

    // Not black_box'd: a black_box'd fill value manufactures a false
    // bottleneck. The escape is on the destination buffers.
    let src = vec![
        Cell {
            grapheme: Grapheme::Ascii(b'y'),
            ..Cell::default()
        };
        cols
    ];

    let mut live = vec![Cell::default(); rows * cols];
    let mut ring = vec![Cell::default(); ring_rows * cols];
    let mut hist = vec![Cell::default(); cap * cols];
    let mut pressure = vec![0u64; pw];
    // Pre-fault every page so the timed loops never take a soft fault.
    for buf in [&mut live, &mut ring, &mut hist] {
        buf.fill(Cell::default());
    }
    pressure.fill(0);

    let _ = black_box(live.as_ptr());
    let _ = black_box(ring.as_ptr());
    let _ = black_box(hist.as_ptr());
    let _ = black_box(pressure.as_ptr());

    let mut acc: u64 = 0;
    let mut pidx: usize = 0;
    macro_rules! churn {
        () => {
            if pressure_kb != 0 {
                for _ in 0..touches {
                    pidx = (pidx.wrapping_mul(2_654_435_761).wrapping_add(1)) & pmask;
                    acc = acc.wrapping_add(pressure[pidx]);
                    pressure[pidx] = acc;
                }
            }
        };
    }

    let ns_per_line = |secs: f64| secs / lines as f64 * 1.0e9;
    // The floor bench charges the `y\r\n` payload's 3 bytes per line.
    let mib_s = |secs: f64| (lines * 3) as f64 / (1024.0 * 1024.0) / secs;

    let mut a1 = Vec::with_capacity(iters);
    let mut a2 = Vec::with_capacity(iters);
    let mut a3 = Vec::with_capacity(iters);

    for _ in 0..iters {
        // A1: warm print.
        let mut base = 0usize;
        let t = Instant::now();
        for _ in 0..lines {
            churn!();
            let bottom = (base + rows - 1) % rows;
            let start = bottom * cols;
            live[start..start + occ].copy_from_slice(&src[..occ]);
            base = if base + 1 == rows { 0 } else { base + 1 };
        }
        a1.push(t.elapsed().as_secs_f64());
        black_box(&live[black_box(0)]);

        // A2: ring print.
        let mut base = 0usize;
        let t = Instant::now();
        for _ in 0..lines {
            churn!();
            let bottom = (base + rows - 1) % ring_rows;
            let start = bottom * cols;
            ring[start..start + occ].copy_from_slice(&src[..occ]);
            base = if base + 1 == ring_rows { 0 } else { base + 1 };
        }
        a2.push(t.elapsed().as_secs_f64());
        black_box(&ring[black_box(0)]);

        // A3: warm print plus cold history push.
        let mut base = 0usize;
        let mut head = 0usize;
        let t = Instant::now();
        for _ in 0..lines {
            churn!();
            let hstart = head * cols;
            hist[hstart..hstart + occ].copy_from_slice(&src[..occ]);
            head = if head + 1 == cap { 0 } else { head + 1 };
            let bottom = (base + rows - 1) % rows;
            let lstart = bottom * cols;
            live[lstart..lstart + occ].copy_from_slice(&src[..occ]);
            base = if base + 1 == rows { 0 } else { base + 1 };
        }
        a3.push(t.elapsed().as_secs_f64());
        black_box(&hist[black_box(0)]);
        black_box(&live[black_box(0)]);
    }
    black_box(acc);

    // Min over interleaved rounds, not mean: the least scheduler-perturbed
    // estimate.
    let best = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let (s1, s2, s3) = (best(&a1), best(&a2), best(&a3));

    eprintln!(
        "cold-print ring probe: rows={rows} cols={cols} cap={cap} occ={occ} \
         lines={lines} iters={iters} pressure_kb={pressure_kb} touches={touches}"
    );
    eprintln!(
        "  A1 warm-print (skip-push ceiling): {:.2} ns/line ({:.0} MiB/s-eq)",
        ns_per_line(s1),
        mib_s(s1),
    );
    eprintln!(
        "  A2 ring-print (reachable ceiling): {:.2} ns/line ({:.0} MiB/s-eq)",
        ns_per_line(s2),
        mib_s(s2),
    );
    eprintln!(
        "  A3 today (warm print + cold push): {:.2} ns/line ({:.0} MiB/s-eq)",
        ns_per_line(s3),
        mib_s(s3),
    );
    eprintln!(
        "  deltas: A3-A1={:.2} (push cost, validate vs doc ~9) | \
         A2-A1={:.2} (cold-miss into print) | A3-A2={:.2} (ring net saving)",
        ns_per_line(s3) - ns_per_line(s1),
        ns_per_line(s2) - ns_per_line(s1),
        ns_per_line(s3) - ns_per_line(s2),
    );
}

/// The ring's three parallel arrays, its modulus, and its occupied window
/// must agree: the alt-screen swap restores a whole ring geometry in one
/// move, and a `phys_cap` past the array length lets `base` walk off the end.
fn assert_ring_consistent(g: &Grid) {
    let cols = usize::from(g.screen.cols);
    assert_eq!(
        g.screen.cells.len(),
        g.screen.phys_cap * cols,
        "cells vs phys_cap"
    );
    assert_eq!(
        g.screen.soft_wrap.len(),
        g.screen.phys_cap,
        "soft_wrap vs phys_cap"
    );
    assert_eq!(
        g.screen.occupancy.len(),
        g.screen.phys_cap,
        "occupancy vs phys_cap"
    );
    assert!(
        g.screen.base < g.screen.phys_cap,
        "base {} vs phys_cap {}",
        g.screen.base,
        g.screen.phys_cap
    );
    assert!(
        g.screen.history_len + usize::from(g.screen.rows) <= g.screen.phys_cap,
        "occupied window {} + {} exceeds phys_cap {}",
        g.screen.history_len,
        g.screen.rows,
        g.screen.phys_cap
    );
    assert!(g.screen.history_len <= g.screen.cap, "history past cap");
}

fn surface_text(g: &Grid) -> (Vec<String>, Vec<String>) {
    let live = (0..g.screen.rows)
        .map(|r| row_text_trim(g.row_content(r).unwrap(), &ClusterTable::default()))
        .collect();
    let sb = (0..g.scrollback().len())
        .map(|i| row_text_trim(g.scrollback().row(i).unwrap(), &ClusterTable::default()))
        .collect();
    (live, sb)
}

fn driven_primary(rows: u16, cols: u16, lines: usize) -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(rows, cols);
    for i in 0..lines {
        drive(&mut p, &mut g, format!("line-{i:03}-tail\r\n").as_bytes());
    }
    (p, g)
}

#[test]
fn resize_under_the_alt_screen_restores_a_consistent_primary_ring() {
    // A `phys_cap` kept from the pre-resize ring lets `base` walk past arrays
    // trimmed to the new row count; the first eviction then indexes out of
    // bounds.
    let (mut p, mut g) = driven_primary(24, 80, 200);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    g.resize(82, 137);
    assert_ring_consistent(&g);
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_ring_consistent(&g);
    for i in 0..300 {
        drive(&mut p, &mut g, format!("after-{i}\r\n").as_bytes());
    }
    assert_ring_consistent(&g);
    assert_eq!(g.rows(), 82);
}

#[test]
fn resize_under_the_alt_screen_keeps_the_primary_scrollback() {
    // A trim reads viewport rows only, so trimming the saved snapshot would
    // empty the shell's scrollback under any TUI.
    let (mut p, mut g) = driven_primary(10, 20, 60);
    let before = g.scrollback().len();
    assert!(before > 0);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    g.resize(10, 40);
    drive(&mut p, &mut g, b"\x1b[?1049l");
    let (_, sb) = surface_text(&g);
    assert!(sb.iter().any(|row| row.contains("line-000-tail")));
    assert!(sb.iter().any(|row| row.contains("line-030-tail")));
}

#[test]
fn a_resize_under_the_alt_screen_matches_resizing_after_the_leave() {
    for (rows, cols) in [(40u16, 12u16), (6, 60), (12, 20)] {
        let (mut deferred_p, mut deferred) = driven_primary(12, 20, 40);
        drive(&mut deferred_p, &mut deferred, b"\x1b[?1049h");
        deferred.resize(rows, cols);
        drive(&mut deferred_p, &mut deferred, b"\x1b[?1049l");

        let (mut direct_p, mut direct) = driven_primary(12, 20, 40);
        drive(&mut direct_p, &mut direct, b"\x1b[?1049h");
        drive(&mut direct_p, &mut direct, b"\x1b[?1049l");
        direct.reflow(rows, cols);

        assert_eq!(
            surface_text(&deferred),
            surface_text(&direct),
            "geometry {rows}x{cols}"
        );
        assert_eq!(deferred.cursor(), direct.cursor(), "geometry {rows}x{cols}");
    }
}

#[test]
fn leaving_the_alt_screen_with_47l_clamps_the_kept_cursor() {
    // `?47l` keeps the alt screen's cursor rather than restoring the
    // primary's, so after a grow it can sit below the snapshot geometry.
    let (mut p, mut g) = driven_primary(6, 20, 20);
    drive(&mut p, &mut g, b"\x1b[?47h");
    g.resize(30, 20);
    drive(&mut p, &mut g, b"\x1b[29;10H");
    drive(&mut p, &mut g, b"\x1b[?47l");
    assert_ring_consistent(&g);
    assert!(g.cursor().row < g.rows());
    assert!(g.cursor().col < g.cols());
}

#[test]
fn zero_scrollback_reserves_only_the_viewport() {
    let (rows, cols) = (24u16, 80u16);
    let g = Grid::with_scrollback(rows, cols, 0);
    assert_eq!(g.scrollback_capacity(), 0);
    assert_eq!(
        g.reserved_cell_bytes(),
        usize::from(rows) * usize::from(cols) * size_of::<Cell>(),
    );
}

#[test]
fn zero_scrollback_drops_scrolled_rows_instead_of_retaining_them() {
    let mut p = Parser::new();
    let mut g = Grid::with_scrollback(2, 8, 0);
    let origin_phys_cap = g.screen.phys_cap;
    for i in 0..50 {
        drive(&mut p, &mut g, format!("row{i}\r\n").as_bytes());
    }
    assert_ring_consistent(&g);
    assert_eq!(
        g.screen.phys_cap, origin_phys_cap,
        "the ring grew at capacity 0"
    );
    assert_eq!(g.screen.history_len, 0);
    assert!(g.scrollback().is_empty());
    let (live, sb) = surface_text(&g);
    assert_eq!(live, vec!["row49".to_string(), String::new()]);
    assert_eq!(sb, Vec::<String>::new());
    // Prompt marks hang off the absolute-line origin, which must count every
    // dropped row.
    assert_eq!(g.scrollback_total_pushed(), 49);
}

/// Resize and the alt-screen round trip both rebuild the ring from `cap`.
#[test]
fn zero_scrollback_survives_resize_and_the_alt_screen() {
    let mut p = Parser::new();
    let mut g = Grid::with_scrollback(4, 8, 0);
    drive(&mut p, &mut g, b"a\r\nb\r\nc\r\nd\r\ne");
    g.resize(8, 20);
    assert_ring_consistent(&g);
    assert_eq!(g.scrollback_capacity(), 0);
    assert!(g.scrollback().is_empty());
    g.resize(2, 4);
    assert_ring_consistent(&g);
    assert_eq!(g.scrollback_capacity(), 0);

    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert_ring_consistent(&g);
    assert_eq!(g.scrollback_capacity(), 0);
    drive(&mut p, &mut g, b"alt\r\nalt2\r\nalt3");
    drive(&mut p, &mut g, b"\x1b[?1049l");
    assert_ring_consistent(&g);
    assert_eq!(
        g.scrollback_capacity(),
        0,
        "leaving the alt screen restored a retention window",
    );
    assert!(g.scrollback().is_empty());
}

/// Growth must extend the ring inside its original allocation
/// (`docs/explanation/data-model/scrollback.md`). Capacity alone would still
/// pass if `resize` reallocated into a bigger buffer, so the pointer is
/// asserted.
#[test]
fn the_ring_grows_without_ever_moving_its_cells() {
    let (rows, cols) = (4u16, 8u16);
    let mut p = Parser::new();
    let mut g = Grid::new(rows, cols);
    let origin = g.screen.cells.as_ptr();
    let reserved = g.screen.cells.capacity();
    assert!(
        reserved >= (usize::from(rows) + DEFAULT_SCROLLBACK_ROWS) * usize::from(cols),
        "the whole retention window must be reserved at construction, got {reserved}",
    );

    for i in 0..500 {
        drive(&mut p, &mut g, format!("line {i}\r\n").as_bytes());
    }
    assert!(
        g.screen.phys_cap > usize::from(rows),
        "the ring must have grown"
    );
    assert_eq!(
        g.screen.cells.as_ptr(),
        origin,
        "growth reallocated: the reservation was lost",
    );

    let history = g.scrollback();
    assert_eq!(history.len(), 500 - usize::from(rows) + 1);
    for (i, row) in history.iter().enumerate() {
        assert_eq!(
            row_text_trim(row, g.cluster_table()),
            format!("line {i}"),
            "history row {i} out of order after growth",
        );
    }
}

/// The client only ever calls `ScreenBuffer::resize`; it must land on the
/// same buffer as `Grid::resize`'s screen half.
#[test]
fn the_grid_resize_screen_half_matches_resizing_the_screen_alone() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 20);
    drive(&mut p, &mut g, b"\x1b[31mcolored\r\n");
    for i in 0..12 {
        drive(&mut p, &mut g, format!("line {i}\r\n").as_bytes());
    }
    let mut screen_alone = g.screen.clone();
    g.resize(9, 14);
    screen_alone.resize(9, 14);
    assert!(
        g.screen == screen_alone,
        "Grid::resize and ScreenBuffer::resize disagree on the screen",
    );
    let mut screen_alone = g.screen.clone();
    g.resize(3, 30);
    screen_alone.resize(3, 30);
    assert!(g.screen == screen_alone, "shrink diverged");
}

/// `pen_style` is the one interned handle held outside any cell, so a
/// compacting sweep must re-establish it.
#[test]
fn a_style_sweep_leaves_the_pen_memo_resolving_to_the_live_pen() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    for i in 0..300u32 {
        let seq = format!(
            "\x1b[38;2;{};{};{}mx\r",
            i % 256,
            (i / 4) % 256,
            (i / 16) % 256
        );
        drive(&mut p, &mut g, seq.as_bytes());
    }
    drive(&mut p, &mut g, b"\x1b[38;2;7;8;9m");
    let pen_before = g.pen();
    g.gc_styles();
    assert_eq!(
        *g.style(g.pen_style),
        pen_before,
        "the pen memo stopped resolving to the live pen after a sweep",
    );
}
