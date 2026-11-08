//! The two-tier scrollback against an uncompacted twin: compaction must
//! be unobservable through every reader, and no scan or clone may fault
//! a discarded page back in.

use felis_vt::Parser;
use proptest::prelude::*;

use crate::search::{SearchOptions, SearchQuery, SearchSource};
use crate::test_support::drive;
use crate::*;

#[derive(Debug, Clone)]
enum Op {
    Bytes(Vec<u8>),
    /// `count` lines of `width` columns in 256-color `color`; bare LF
    /// keeps the column, so the rows leave a staircase.
    Lines {
        count: u8,
        width: u8,
        color: u8,
        bare_lf: bool,
    },
    /// Scrolls with a colored pen, so BCE blanks reach history.
    BceScroll {
        color: u8,
        n: u8,
    },
    /// Scrolls a region anchored at the top but short of the bottom.
    PartialTop {
        bottom: u8,
        n: u8,
    },
    /// Colored ED 2, DECALN, DECCOLM under `?40`, DECSED 0/1/2: none may
    /// change a history row.
    WholeScreen(u8),
    /// On both grids, and on the compacted one also through the
    /// one-buffer-per-row oracle.
    Reflow {
        rows: u16,
        cols: u16,
    },
    /// A slice on the compacted grid alone, with the OS refusing every
    /// discard when `refuse` is set.
    Compact {
        max: usize,
        refuse: bool,
    },
    /// Both table sweeps, on both grids.
    Sweep,
    /// The compacted grid goes on as its clone.
    Clone,
    /// The compacted grid goes on as its restored state dump.
    Dump,
}

fn token() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "a",
        "b",
        "xyz",
        " ",
        "\r\n",
        "\n",
        "\r",
        "\t",
        "\x08",
        "端",
        "末",
        "👍",
        "e\u{301}",
        "\x1b[31m",
        "\x1b[38;5;200m",
        "\x1b[0m",
        "\x1b[44m\x1b[K",
        "\x1b[44m\x1b[2J",
        "\x1b[3J",
        "\x1b[5G",
        "\x1b[2A",
        "\x1b[H",
        "\x1b[3;2H",
        "\x1b]8;;http://x\x07",
        "\x1b]8;;\x07",
        "\x1b]66;s=2;AB\x07",
        "\x1b]66;s=2:w=3;A\x07",
        "\x1b]66;w=2;x\x07",
        "\x1b]133;A\x07",
        "\x1b]133;A;redraw=last\x07",
        "\x1b]133;A;redraw=0\x07",
        "\x1b]133;B\x07",
        "\x1b]133;C\x07",
        "\x1b]133;D;0\x07",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b[?7l",
        "\x1b[?7h",
    ])
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => prop::collection::vec(token(), 1..24).prop_map(|ts| Op::Bytes(ts.concat().into_bytes())),
        1 => prop::collection::vec(any::<u8>(), 1..32).prop_map(Op::Bytes),
        2 => (prop::sample::select(vec!["", ";redraw=last", ";redraw=0"]), token())
            .prop_map(|(redraw, prompt)| Op::Bytes(format!(
                "\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;A{redraw}\x07$ {prompt}{prompt}\x1b]133;B\x07"
            ).into_bytes())),
        4 => (1u8..12, 0u8..40, any::<u8>(), any::<bool>()).prop_map(|(count, width, color, bare_lf)| {
            Op::Lines { count, width, color, bare_lf }
        }),
        1 => (1u8..8, 1u8..4).prop_map(|(color, n)| Op::BceScroll { color, n }),
        1 => (0u8..4, 1u8..4).prop_map(|(bottom, n)| Op::PartialTop { bottom, n }),
        1 => (0u8..6).prop_map(Op::WholeScreen),
        2 => (1u16..=8, 1u16..=24).prop_map(|(rows, cols)| Op::Reflow { rows, cols }),
        4 => (0usize..=32, prop::bool::weighted(0.2))
            .prop_map(|(max, refuse)| Op::Compact { max, refuse }),
        1 => Just(Op::Sweep),
        1 => Just(Op::Clone),
        1 => Just(Op::Dump),
    ]
}

fn bytes_of(op: &Op, rows: u16) -> Vec<u8> {
    match op {
        Op::Bytes(bytes) => bytes.clone(),
        &Op::Lines {
            count,
            width,
            color,
            bare_lf,
        } => {
            let mut out = Vec::new();
            for i in 0..count {
                let text: String = (0..usize::from(width))
                    .map(|c| char::from(b'a' + ((c + usize::from(i)) % 26) as u8))
                    .collect();
                let end = if bare_lf { "\n" } else { "\r\n" };
                out.extend_from_slice(format!("\x1b[38;5;{color}m{text}\x1b[m{end}").as_bytes());
            }
            out
        }
        &Op::BceScroll { color, n } => format!(
            "\x1b[4{color}m\x1b[{rows};1H{}\x1b[m",
            "\n".repeat(n.into())
        )
        .into_bytes(),
        &Op::PartialTop { bottom, n } => {
            let bottom = (u16::from(bottom) + 1).min(rows.saturating_sub(1)).max(1);
            format!(
                "\x1b[1;{bottom}r\x1b[{bottom};1Hpt\x1b[45m{}\x1b[m\x1b[r",
                "\n".repeat(n.into())
            )
            .into_bytes()
        }
        &Op::WholeScreen(which) => match which {
            0 => b"\x1b[46m\x1b[2J\x1b[m".to_vec(),
            1 => b"\x1b#8".to_vec(),
            2 => b"\x1b[?40h\x1b[?3h\x1b[?3l\x1b[?40l".to_vec(),
            n => format!("\x1b[44m\x1b[?{}J\x1b[m", n - 3).into_bytes(),
        },
        Op::Reflow { .. } | Op::Compact { .. } | Op::Sweep | Op::Clone | Op::Dump => Vec::new(),
    }
}

/// Feeds `op` to both grids; compaction, clones and dumps reach `g`
/// alone.
fn apply(op: &Op, p: &mut Parser, g: &mut Grid, tp: &mut Parser, twin: &mut Grid) {
    match *op {
        Op::Compact { max, refuse } => {
            paged_cells::fail_discards(refuse);
            g.compact_history(max);
            paged_cells::fail_discards(false);
        }
        Op::Sweep => {
            for grid in [g, twin] {
                grid.gc_styles();
                grid.gc_sizings();
            }
        }
        Op::Reflow { rows, cols } => {
            let mut reference = g.clone();
            let expected = reference.reflow_reference(rows, cols);
            let remap = g.reflow(rows, cols);
            assert_eq!(remap, expected, "reflow remap against the oracle");
            assert!(*g == reference, "reflow against the oracle");
            assert_eq!(remap, twin.reflow(rows, cols), "reflow remap");
        }
        Op::Clone => {
            let copy = g.clone();
            assert!(copy == *g, "clone");
            *g = copy;
        }
        Op::Dump => {
            #[cfg(feature = "state-dump")]
            {
                let json = serde_json::to_vec(&*g).expect("dump");
                let restored: Grid = serde_json::from_slice(&json).expect("restore");
                restored.check_restored().expect("consistent");
                assert!(restored == *g, "restored dump");
                *g = restored;
            }
        }
        Op::WholeScreen(_) => {
            let before = history(g);
            let bytes = bytes_of(op, g.rows());
            drive(p, g, &bytes);
            drive(tp, twin, &bytes);
            assert_eq!(
                history(g),
                before,
                "a screen-scoped command changed history"
            );
        }
        _ => {
            let bytes = bytes_of(op, g.rows());
            drive(p, g, &bytes);
            drive(tp, twin, &bytes);
        }
    }
}

/// History rows with wrap bits, and each history row as `capture
/// --format ansi` renders it, resolved through the style table.
fn history(g: &Grid) -> Vec<(Vec<Cell>, bool, Vec<u8>)> {
    let sb = g.scrollback();
    (0..sb.len())
        .map(|i| {
            let row = sb.row(i).expect("row inside len");
            (
                row.to_vec(),
                sb.soft_wrap_continued(i).expect("wrap inside len"),
                row_ansi(row, g.cluster_table(), g.screen().style_table()),
            )
        })
        .collect()
}

fn hits(g: &Grid, source: SearchSource) -> Vec<SearchHit> {
    let query = SearchQuery::new(
        "[a-e]+",
        SearchOptions {
            regex: true,
            case_insensitive: false,
        },
    )
    .expect("query");
    g.search(&query, source).collect()
}

/// Everything a reader observes is the twin's.
fn assert_unobservable(g: &mut Grid, twin: &mut Grid) {
    assert!(g == twin, "state differs");
    assert_eq!(history(g), history(twin), "history");
    assert_eq!(
        g.rows_with_wrap(AltScreenRows::Include).collect::<Vec<_>>(),
        twin.rows_with_wrap(AltScreenRows::Include)
            .collect::<Vec<_>>()
    );
    for source in [SearchSource::Scrollback, SearchSource::Visible] {
        assert_eq!(hits(g, source), hits(twin, source), "search");
    }
    assert_eq!(g.prompt_marks(), twin.prompt_marks(), "marks");
    assert_eq!(g.prompt_marks_pruned(), twin.prompt_marks_pruned());
    for mark in g.prompt_marks() {
        assert_eq!(g.locate_line(mark.line), twin.locate_line(mark.line));
    }
    assert!(g.viewport_anchor() == twin.viewport_anchor(), "anchor");
    let depth = u32::try_from(g.scrollback().len()).unwrap();
    for viewport in [0, 1, depth / 2, depth] {
        for row in 0..g.rows() {
            assert_eq!(
                g.viewport_row(viewport, row).map(|v| v.cells.to_vec()),
                twin.viewport_row(viewport, row).map(|v| v.cells.to_vec()),
            );
        }
    }
    assert_eq!(g.take_pty_effects(), twin.take_pty_effects(), "effects");
    assert_tiers_consistent(g);
}

/// Cold and ring history share one cap, a row cannot be in both, the
/// ring's arrays still cover its window, no live row lies on a page that
/// was discarded and not reused since, the gap's reused prefix lies
/// inside the gap, and the cold index stays within twice the cap.
fn assert_tiers_consistent(g: &Grid) {
    let s = &g.screen;
    for (ring, cap, cells, gap) in std::iter::once((s.ring(), s.cap, &s.cells, s.gap)).chain(
        s.saved_primary
            .as_ref()
            .map(|p| (p.ring(), p.cap, &p.cells, p.gap)),
    ) {
        for rows in ring.live_rows() {
            assert!(
                !cells.has_discarded_page(rows.start * ring.cols..rows.end * ring.cols),
                "a live row in {rows:?} lies on a discarded page"
            );
            for phys in rows {
                assert!(
                    usize::from(ring.occupancy[phys]) <= ring.cols,
                    "occupancy past cols"
                );
            }
        }
        assert!(
            ring.cold.len() + ring.history_len <= cap,
            "history past cap"
        );
        assert!(ring.history_len + ring.rows <= ring.phys_cap);
        assert_eq!(ring.cells.len(), ring.phys_cap * ring.cols);
        assert!(
            ring.cold.is_empty() || ring.history_len >= 1,
            "a cold row with no ring history above it"
        );
        assert!(
            ring.cold.index_len() <= 2 * cap,
            "cold index past twice the cap"
        );
        let gap_len = ring.phys_cap - ring.history_len - ring.rows;
        assert!(
            gap.reused_prefix().is_none_or(|n| n < gap_len),
            "the reused prefix runs past the gap"
        );
    }
}

/// `count` lines of `width` columns, fed to both grids.
fn lines(
    count: usize,
    width: usize,
    p: &mut Parser,
    g: &mut Grid,
    tp: &mut Parser,
    twin: &mut Grid,
) {
    let line = format!("\x1b[32m{}\x1b[m\r\n", "w".repeat(width));
    let out = line.repeat(count);
    drive(p, g, out.as_bytes());
    drive(tp, twin, out.as_bytes());
}

/// The most pages a refill of `turned_live` rows may reuse: the pages
/// those rows span, one reuse-ahead span, and a straddling page at each
/// end of both.
fn reuse_bound(turned_live: usize, cols: u16) -> usize {
    let page = paged_cells::page_bytes();
    let row_bytes = usize::from(cols) * size_of::<Cell>();
    (turned_live * row_bytes).div_ceil(page) + compact::REUSE_AHEAD_BYTES.div_ceil(page) + 4
}

fn grids(rows: u16, cols: u16, cap: usize) -> (Parser, Grid, Parser, Grid) {
    (
        Parser::new(),
        Grid::with_scrollback(rows, cols, cap),
        Parser::new(),
        Grid::with_scrollback(rows, cols, cap),
    )
}

proptest! {
    /// Output of every kind, reflows, table sweeps, clones and state
    /// dumps, interleaved with compaction slices of any size (some refused
    /// by the OS) and run past the cap so the cold tier evicts: after every
    /// step the compacted grid equals its uncompacted twin, reads as it
    /// through every reader, and keeps the tiers consistent.
    #[test]
    fn a_compacted_grid_reads_as_its_uncompacted_twin(
        rows in 1u16..=8,
        cols in 1u16..=24,
        cap in 0usize..=24,
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(rows, cols, cap);
        for op in &ops {
            apply(op, &mut p, &mut g, &mut tp, &mut twin);
            assert_unobservable(&mut g, &mut twin);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// However slices, refills and reflows interleave and wherever the
    /// gap wraps, every live row stays on reused pages, and a burst
    /// reclaims only the pages its rows reach plus one reuse-ahead span.
    #[test]
    fn refill_reuses_only_the_pages_it_reaches(
        cols in 100u16..600,
        fill in 300usize..700,
        steps in prop::collection::vec((0usize..120, 0usize..400, any::<bool>()), 1..8),
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(3, cols, 300);
        let width = usize::from(cols) / 3;
        lines(fill, width, &mut p, &mut g, &mut tp, &mut twin);
        for (burst, slice, reflow) in steps {
            let cols = g.cols();
            g.compact_history(slice);
            assert_tiers_consistent(&g);
            let gap = g.screen.phys_cap - g.screen.history_len - usize::from(g.rows());
            let before = g.screen.cells.discarded_pages();
            lines(burst, width, &mut p, &mut g, &mut tp, &mut twin);
            assert_tiers_consistent(&g);
            prop_assert_eq!(history(&g), history(&twin));
            let reclaimed = before - g.screen.cells.discarded_pages();
            prop_assert!(
                reclaimed <= reuse_bound(burst.min(gap), cols),
                "{burst} rows into a {gap}-row gap reclaimed {reclaimed} pages"
            );
            if burst >= gap {
                prop_assert_eq!(g.screen.cells.discarded_pages(), 0);
            }
            if reflow {
                let remap = g.reflow(3, cols ^ 1);
                prop_assert_eq!(remap, twin.reflow(3, cols ^ 1));
                assert_tiers_consistent(&g);
            }
        }
        assert_unobservable(&mut g, &mut twin);
    }
}

#[test]
fn compaction_keeps_one_screen_of_history_in_the_ring() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 20, 50);
    apply(
        &Op::Lines {
            count: 30,
            width: 9,
            color: 1,
            bare_lf: false,
        },
        &mut p,
        &mut g,
        &mut tp,
        &mut twin,
    );
    assert_eq!(g.scrollback().len(), 27);
    assert!(!g.compact_history(10));
    assert!(!g.compact_history(10), "3 rows past the hot window remain");
    assert!(g.compact_history(10));
    assert_eq!(g.screen.history_len, 4);
    assert_eq!(g.screen.cold.len(), 23);
    assert!(g.compact_history(10), "a finished run has nothing left");
    assert_unobservable(&mut g, &mut twin);
}

/// Equality is what readers see: a compacted grid equals its
/// uncompacted twin, its clone (which copies live rows only and starts
/// with an intact gap) and its restored dump, mid-refill included.
#[test]
fn a_compacted_grid_equals_its_twin_its_clone_and_its_restored_dump() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 60, 200);
    lines(150, 41, &mut p, &mut g, &mut tp, &mut twin);
    for slice in [30, usize::MAX] {
        g.compact_history(slice);
        lines(3, 17, &mut p, &mut g, &mut tp, &mut twin);
        if slice == usize::MAX {
            assert!(
                !g.screen.gap.is_intact(),
                "the refill stopped inside the gap"
            );
        }
        assert!(g == twin, "compacted against the twin");
        assert!(g.clone() == g, "clone against its source");
        #[cfg(feature = "state-dump")]
        {
            let restored: Grid =
                serde_json::from_slice(&serde_json::to_vec(&g).expect("dump")).expect("restore");
            assert!(restored == g, "restored dump against its source");
        }
    }
}

/// A primary compacted while saved under `?1049h` survives a resize
/// under the alternate screen, a state dump and the re-wrap on leave,
/// then keeps scrolling in FIFO order, as its twin.
#[test]
fn a_saved_compacted_primary_survives_resize_dump_and_leave() {
    let (mut p, mut g, mut tp, mut twin) = grids(5, 16, 200);
    let prompt = b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07".to_vec();
    for op in [
        Op::Bytes(prompt.clone()),
        Op::Lines {
            count: 60,
            width: 23,
            color: 2,
            bare_lf: false,
        },
        Op::Bytes(prompt),
        Op::Bytes(b"\x1b[?1049halt\r\n".to_vec()),
        Op::Compact {
            max: usize::MAX,
            refuse: false,
        },
        Op::Reflow { rows: 4, cols: 9 },
        Op::Dump,
        Op::Reflow { rows: 5, cols: 16 },
        Op::Bytes(b"\x1b[?1049l".to_vec()),
        Op::Lines {
            count: 30,
            width: 12,
            color: 6,
            bare_lf: false,
        },
    ] {
        apply(&op, &mut p, &mut g, &mut tp, &mut twin);
        if matches!(op, Op::Compact { .. }) {
            let saved = g.screen.saved_primary.as_ref().expect("saved primary");
            assert!(
                !saved.cold.is_empty(),
                "the slice reached the saved primary"
            );
        }
        assert_unobservable(&mut g, &mut twin);
    }
}

/// A recycled vacant row whose pages were not discarded keeps a tail
/// naming pens a sweep has since reclaimed; the next sweep must not read
/// it.
#[test]
fn a_sweep_after_recycling_a_stale_vacant_row_reads_only_occupied_cells() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 12, 30);
    let mut out = Vec::new();
    for i in 0..40 {
        out.extend_from_slice(format!("\x1b[38;2;{i};1;1mlong-line-{i:02}\x1b[m\r\n").as_bytes());
    }
    out.extend_from_slice(&b"\x1b[32mb\x1b[m\r\n".repeat(40));
    drive(&mut p, &mut g, &out);
    drive(&mut tp, &mut twin, &out);
    paged_cells::fail_discards(true);
    assert!(g.compact_history(usize::MAX));
    paged_cells::fail_discards(false);
    let churn = b"\x1b[38;2;9;9;9mq\x1b[m\r\n";
    for _ in 0..3 {
        for grid in [&mut g, &mut twin] {
            grid.gc_styles();
            grid.gc_sizings();
        }
        drive(&mut p, &mut g, churn);
        drive(&mut tp, &mut twin, churn);
        assert_unobservable(&mut g, &mut twin);
    }
}

/// The shadow's sizing stamp lands on a recycled row whose tail past the
/// watermark still names a handle the sweep reclaimed and an install
/// reused; the compacted grid's row was discarded, the twin's was not.
#[test]
fn a_sizing_stamp_on_a_recycled_row_matches_the_twin() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 80, 30);
    let mut out = b"ab\x1b]66;w=2;W\x07\r\n".repeat(34);
    out.extend_from_slice(&b"x\r\n".repeat(34));
    drive(&mut p, &mut g, &out);
    drive(&mut tp, &mut twin, &out);
    assert!(g.compact_history(usize::MAX));
    for grid in [&mut g, &mut twin] {
        grid.gc_sizings();
    }
    for _ in 0..30 {
        let handles = [&mut g, &mut twin].map(|grid| {
            let handle = grid.install_sizing(Sizing::default()).expect("handle");
            drive_lf(grid);
            grid.set_cell_sizing(grid.rows() - 1, 2, Some(handle));
            grid.gc_sizings();
            handle
        });
        assert_eq!(handles[0], handles[1]);
        assert_unobservable(&mut g, &mut twin);
    }
}

fn drive_lf(grid: &mut Grid) {
    drive(&mut Parser::new(), grid, b"\r\n");
}

/// A discard the OS refuses changes nothing: the rows still move to the
/// cold tier, no page counts as discarded, and the ring stays usable.
#[test]
fn a_refused_discard_leaves_compaction_unobservable() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 200, 300);
    let fill = Op::Lines {
        count: 250,
        width: 150,
        color: 2,
        bare_lf: false,
    };
    apply(&fill, &mut p, &mut g, &mut tp, &mut twin);
    apply(
        &Op::Compact {
            max: usize::MAX,
            refuse: true,
        },
        &mut p,
        &mut g,
        &mut tp,
        &mut twin,
    );
    assert!(!g.screen.cold.is_empty());
    assert_eq!(g.screen.cells.discarded_pages(), 0);
    assert_unobservable(&mut g, &mut twin);
    apply(&fill, &mut p, &mut g, &mut tp, &mut twin);
    assert_unobservable(&mut g, &mut twin);
}

/// A cold row's chunk is recycled as the cap evicts it, so a ring kept
/// compacted while output streams through stops growing its cold tier
/// once history is at the cap.
#[test]
fn cold_eviction_holds_the_cap_while_output_streams() {
    let (mut p, mut g, mut tp, mut twin) = grids(4, 40, 100);
    let burst = Op::Lines {
        count: 30,
        width: 33,
        color: 4,
        bare_lf: false,
    };
    let mut settled = 0;
    for round in 0..20 {
        apply(&burst, &mut p, &mut g, &mut tp, &mut twin);
        g.compact_history(usize::MAX);
        assert_unobservable(&mut g, &mut twin);
        if round == 5 {
            settled = g.screen.cold.heap_bytes();
        }
    }
    assert_eq!(g.scrollback().len(), 100);
    assert!(g.screen.cold.heap_bytes() <= settled);
}

#[cfg(target_os = "linux")]
mod residency {
    use super::*;
    use crate::paged_cells::resident_pages;

    fn primary_pages(g: &Grid) -> usize {
        let cells = g
            .screen
            .saved_primary
            .as_ref()
            .map_or(&g.screen.cells, |saved| &saved.cells);
        resident_pages(cells)
    }

    /// A full 2,000-row ring at 200 columns, compacted to its hot window.
    fn compacted() -> (Parser, Grid, Parser, Grid) {
        let (mut p, mut g, mut tp, mut twin) = grids(24, 200, 2000);
        let fill = Op::Lines {
            count: 250,
            width: 60,
            color: 6,
            bare_lf: false,
        };
        for _ in 0..9 {
            apply(&fill, &mut p, &mut g, &mut tp, &mut twin);
        }
        assert_eq!(g.scrollback().len(), 2000);
        let full = resident_pages(&g.screen.cells);
        assert!(g.compact_history(usize::MAX));
        let after = resident_pages(&g.screen.cells);
        assert!(after * 10 < full, "{after} of {full} pages still resident");
        (p, g, tp, twin)
    }

    /// What `resident_cell_bytes` reports for the ring is what it keeps
    /// resident, to the page, full and compacted.
    #[test]
    fn resident_cell_bytes_match_what_the_ring_keeps_resident() {
        use crate::paged_cells::page_bytes;
        let ring_resident = |g: &Grid| g.resident_cell_bytes() - g.screen.cold.heap_bytes();
        let resident = |g: &Grid| primary_pages(g) * page_bytes();
        let (_, mut full, _, _) = grids(24, 200, 2000);
        let mut p = Parser::new();
        let lines = Op::Lines {
            count: 250,
            width: 9,
            color: 3,
            bare_lf: false,
        };
        for _ in 0..9 {
            drive(&mut p, &mut full, &bytes_of(&lines, 24));
        }
        assert!(ring_resident(&full).abs_diff(resident(&full)) <= page_bytes());
        let (_, g, _, _) = compacted();
        assert!(
            ring_resident(&g).abs_diff(resident(&g)) <= 2 * page_bytes(),
            "{} reported, {} resident",
            ring_resident(&g),
            resident(&g)
        );
    }

    /// Spec: scans and clones keep discarded pages discarded. Each walk
    /// that once read the whole physical ring runs with no compaction
    /// after it, and residency stays where compaction left it.
    #[test]
    fn scans_clones_and_screen_switches_leave_discarded_pages_alone() {
        let (mut p, mut g, mut tp, mut twin) = compacted();
        let low = primary_pages(&g);
        let slack = 2;
        g.gc_styles();
        g.gc_sizings();
        assert!(primary_pages(&g) <= low + slack, "table sweeps");
        let copy = g.clone();
        assert!(primary_pages(&g) <= low + slack, "clone source");
        assert!(resident_pages(&copy.screen.cells) <= low + slack, "clone");
        drop(copy);
        drive(&mut p, &mut g, b"\x1b[?1049h");
        drive(&mut tp, &mut twin, b"\x1b[?1049h");
        assert!(primary_pages(&g) <= low + slack, "alternate screen enter");
        g.gc_styles();
        assert!(
            primary_pages(&g) <= low + slack,
            "sweep under the alt screen"
        );
        drive(&mut p, &mut g, b"\x1b[?1049l");
        drive(&mut tp, &mut twin, b"\x1b[?1049l");
        assert!(primary_pages(&g) <= low + slack, "alternate screen leave");
        #[cfg(feature = "state-dump")]
        {
            drop(serde_json::to_vec(&g).expect("dump"));
            assert!(primary_pages(&g) <= low + slack, "state dump");
        }
        assert_unobservable(&mut g, &mut twin);
    }

    /// Spec: the screen-scoped erases touch only viewport pages.
    #[test]
    fn whole_screen_commands_leave_discarded_pages_alone() {
        let (mut p, mut g, mut tp, mut twin) = compacted();
        let low = primary_pages(&g);
        for which in 0..6 {
            apply(&Op::WholeScreen(which), &mut p, &mut g, &mut tp, &mut twin);
        }
        assert!(primary_pages(&g) <= low + 2);
        assert_unobservable(&mut g, &mut twin);
    }

    /// Output after compaction refills the gap from the viewport side;
    /// the next compaction discards it again, so residency returns to the
    /// hot window rather than ratcheting up.
    #[test]
    fn a_second_compaction_after_output_discards_the_refilled_gap() {
        let (mut p, mut g, mut tp, mut twin) = compacted();
        let low = primary_pages(&g);
        let burst = Op::Lines {
            count: 100,
            width: 60,
            color: 1,
            bare_lf: false,
        };
        apply(&burst, &mut p, &mut g, &mut tp, &mut twin);
        assert!(g.compact_history(usize::MAX));
        assert!(primary_pages(&g) <= low + 2);
        assert_unobservable(&mut g, &mut twin);
    }
}

/// Measurement, not a gate (`cargo nextest run --release -p felis-grid
/// --run-ignored only measure_ --no-capture`): what a full 54×192 ring
/// holds before and after a full compaction, for plain and rich output.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measurement"]
fn measure_compacted_residency() {
    use crate::paged_cells::{page_bytes, resident_pages};
    for (name, mean) in [("plain", 33usize), ("rich", 65)] {
        let mut p = Parser::new();
        let mut g = Grid::new(54, 192);
        let mut out = Vec::new();
        for i in 0..DEFAULT_SCROLLBACK_ROWS + 100 {
            let width = (i * 7919) % (2 * mean + 1);
            let text: String = (0..width)
                .map(|c| char::from(b'!' + (c % 90) as u8))
                .collect();
            out.extend_from_slice(format!("\x1b[3{}m{text}\x1b[m\r\n", i % 8).as_bytes());
        }
        drive(&mut p, &mut g, &out);
        let mib = |bytes: usize| bytes as f64 / f64::from(1 << 20);
        let before = resident_pages(&g.screen.cells) * page_bytes();
        let start = Instant::now();
        while !g.compact_history(1024) {}
        let took = start.elapsed();
        let ring = resident_pages(&g.screen.cells) * page_bytes();
        let cold = g.screen.cold.heap_bytes();
        println!(
            "54x192 {name}: ring {:.1} MiB -> ring {:.1} MiB + cold {:.1} MiB = {:.1} MiB ({took:?})",
            mib(before),
            mib(ring),
            mib(cold),
            mib(ring + cold),
        );
    }
}

/// Measurement, not a gate: the longest lock hold compaction adds, at the
/// largest admitted geometry with every history row full width.
#[test]
#[ignore = "measurement"]
fn measure_slice_latency_at_the_largest_geometry() {
    use felis_protocol::messages::{MAX_GRID_COLS, MAX_GRID_ROWS};
    let (rows, cols) = (MAX_GRID_ROWS, MAX_GRID_COLS);
    let mut p = Parser::new();
    let mut g = Grid::new(rows, cols);
    let line = format!("\x1b[44m{}\r\n", "x".repeat(usize::from(cols)));
    let out = line.repeat(DEFAULT_SCROLLBACK_ROWS + usize::from(rows));
    drive(&mut p, &mut g, out.as_bytes());
    for slice in [64usize, 256, 1024] {
        let start = Instant::now();
        g.compact_history(slice);
        println!("{rows}x{cols}: {slice}-row slice {:?}", start.elapsed());
    }
    let start = Instant::now();
    drive(&mut p, &mut g, line.as_bytes());
    println!(
        "{rows}x{cols}: first line feed into the gap {:?}",
        start.elapsed()
    );
    let start = Instant::now();
    g.compact_history(1);
    println!(
        "{rows}x{cols}: 1-row slice after the line feed {:?}",
        start.elapsed()
    );
}

/// `count` lines whose widths spread around `mean` columns, the shape
/// of shell output the measurements below use.
#[cfg(target_os = "macos")]
fn spread_lines(count: usize, mean: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..count {
        let width = (i * 7919) % (2 * mean + 1);
        let text: String = (0..width)
            .map(|c| char::from(b'!' + (c % 90) as u8))
            .collect();
        out.extend_from_slice(format!("\x1b[3{}m{text}\x1b[m\r\n", i % 8).as_bytes());
    }
    out
}

/// macOS counts a discarded page again from the `MADV_FREE_REUSE` that
/// takes it back, so this is where reusing more than the rows about to
/// turn live shows: a refill must grow the footprint with its rows.
#[cfg(target_os = "macos")]
mod footprint {
    use super::*;
    use crate::paged_cells::phys_footprint;

    const MIB: u64 = 1 << 20;

    #[test]
    fn a_refill_after_compaction_grows_the_footprint_by_its_rows_alone() {
        let (rows, cols) = (54u16, 192u16);
        let ring_bytes =
            (usize::from(rows) + DEFAULT_SCROLLBACK_ROWS) * usize::from(cols) * size_of::<Cell>();
        let ring_bytes = u64::try_from(ring_bytes).expect("small ring");
        let mut p = Parser::new();
        let mut g = Grid::new(rows, cols);
        let fill = spread_lines(DEFAULT_SCROLLBACK_ROWS + 100, 5);
        let one = spread_lines(1, 5);
        let hundred = spread_lines(100, 5);
        drive(&mut p, &mut g, &fill);
        let full = phys_footprint();
        while !g.compact_history(1024) {}
        let compacted = phys_footprint();
        assert!(
            compacted + ring_bytes * 3 / 4 < full,
            "compaction left {compacted} of {full} bytes"
        );
        drive(&mut p, &mut g, &one);
        let one_line = phys_footprint();
        assert!(
            one_line < compacted + MIB,
            "one line took the footprint from {compacted} to {one_line} bytes"
        );
        drive(&mut p, &mut g, &hundred);
        let row_bytes = u64::from(cols) * size_of::<Cell>() as u64;
        assert!(
            phys_footprint() < one_line + 100 * row_bytes + MIB,
            "a hundred lines reclaimed more than their rows"
        );
    }

    /// Measurement, not a gate (`cargo nextest run --release -p
    /// felis-grid --run-ignored only measure_ --no-capture`): the
    /// process footprint around compacting a full 54×192 ring and
    /// refilling it, for plain and rich output.
    #[test]
    #[ignore = "measurement"]
    fn measure_refill_footprint() {
        let mib = |bytes: u64| bytes as f64 / MIB as f64;
        // Each grid outlives the next one's measurement: a dropped cold
        // tier's heap would serve the next grid and hide its growth.
        let mut kept = Vec::new();
        for (name, mean) in [("plain", 33usize), ("rich", 65)] {
            let mut p = Parser::new();
            let mut g = Grid::new(54, 192);
            let fill = spread_lines(DEFAULT_SCROLLBACK_ROWS + 100, mean);
            let one = spread_lines(1, mean);
            let thousand = spread_lines(1000, mean);
            let start = phys_footprint();
            drive(&mut p, &mut g, &fill);
            let full = phys_footprint();
            while !g.compact_history(1024) {}
            let compacted = phys_footprint();
            drive(&mut p, &mut g, &one);
            let one_line = phys_footprint();
            drive(&mut p, &mut g, &thousand);
            let more = phys_footprint();
            drive(&mut p, &mut g, &fill);
            let refilled = phys_footprint();
            let [full, compacted, one_line, more, refilled] =
                [full, compacted, one_line, more, refilled].map(|b| mib(b.saturating_sub(start)));
            println!(
                "54x192 {name}: full {full:.1} MiB, compacted {compacted:.1}, +1 line \
                 {one_line:.1}, +1001 lines {more:.1}, refilled {refilled:.1}"
            );
            kept.push(g);
        }
    }
}
