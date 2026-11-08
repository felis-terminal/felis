//! The two-tier scrollback against an uncompacted twin: compaction must
//! be unobservable through every reader, and no scan or clone may fault
//! a discarded page back in.

use felis_vt::Parser;
use proptest::prelude::*;

use crate::search::{SearchOptions, SearchQuery, SearchSource};
use crate::test_support::{drive, normalized};
use crate::*;

#[derive(Debug, Clone)]
enum Op {
    /// `count` lines of `width` columns in color `color`; bare LF keeps
    /// the column, so the rows leave a staircase.
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
    /// An OSC 133 prompt, command and output.
    Prompt,
    /// An OSC 66 run, which interns a sizing handle.
    Sized,
    /// Colored ED 2, DECALN, DECCOLM under `?40`, DECSED 0/1/2.
    WholeScreen(u8),
    Ed3,
    Alt(bool),
    Reflow {
        rows: u8,
        cols: u8,
    },
    /// Both table sweeps, on both grids.
    Sweep,
    Compact(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1u8..12, 0u8..40, 0u8..8, any::<bool>()).prop_map(|(count, width, color, bare_lf)| {
            Op::Lines { count, width, color, bare_lf }
        }),
        1 => (1u8..8, 1u8..4).prop_map(|(color, n)| Op::BceScroll { color, n }),
        1 => (0u8..4, 1u8..4).prop_map(|(bottom, n)| Op::PartialTop { bottom, n }),
        1 => Just(Op::Prompt),
        1 => Just(Op::Sized),
        1 => (0u8..6).prop_map(Op::WholeScreen),
        1 => Just(Op::Ed3),
        1 => any::<bool>().prop_map(Op::Alt),
        1 => (2u8..8, 4u8..24).prop_map(|(rows, cols)| Op::Reflow { rows, cols }),
        1 => Just(Op::Sweep),
        4 => (0u8..12).prop_map(Op::Compact),
    ]
}

fn bytes_of(op: &Op, rows: u16) -> Vec<u8> {
    match *op {
        Op::Lines {
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
                out.extend_from_slice(format!("\x1b[3{color}m{text}\x1b[m{end}").as_bytes());
            }
            out
        }
        Op::BceScroll { color, n } => format!(
            "\x1b[4{color}m\x1b[{rows};1H{}\x1b[m",
            "\n".repeat(n.into())
        )
        .into_bytes(),
        Op::PartialTop { bottom, n } => {
            let bottom = (u16::from(bottom) + 1).min(rows.saturating_sub(1)).max(1);
            format!(
                "\x1b[1;{bottom}r\x1b[{bottom};1Hpt\x1b[45m{}\x1b[m\x1b[r",
                "\n".repeat(n.into())
            )
            .into_bytes()
        }
        Op::Prompt => {
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07".to_vec()
        }
        Op::Sized => b"\x1b]66;s=2;W\x07\r\n\r\n".to_vec(),
        Op::WholeScreen(which) => match which {
            0 => b"\x1b[46m\x1b[2J\x1b[m".to_vec(),
            1 => b"\x1b#8".to_vec(),
            2 => b"\x1b[?40h\x1b[?3h\x1b[?3l\x1b[?40l".to_vec(),
            n => format!("\x1b[44m\x1b[?{}J\x1b[m", n - 3).into_bytes(),
        },
        Op::Ed3 => b"\x1b[3J".to_vec(),
        Op::Alt(on) => {
            if on {
                b"\x1b[?1049halt\r\n".to_vec()
            } else {
                b"\x1b[?1049l".to_vec()
            }
        }
        Op::Reflow { .. } | Op::Sweep | Op::Compact(_) => Vec::new(),
    }
}

/// Feeds `op` to both grids, compaction to `g` alone.
fn apply(op: &Op, p: &mut Parser, g: &mut Grid, tp: &mut Parser, twin: &mut Grid) {
    match *op {
        Op::Compact(max) => {
            g.compact_history(usize::from(max));
        }
        Op::Sweep => {
            for grid in [g, twin] {
                grid.gc_styles();
                grid.gc_sizings();
            }
        }
        Op::Reflow { rows, cols } => {
            let remap = g.reflow(rows.into(), cols.into());
            assert_eq!(remap, twin.reflow(rows.into(), cols.into()), "reflow remap");
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
    assert!(normalized(g) == normalized(twin), "state differs");
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

/// Cold and ring history share one cap, a row cannot be in both, and the
/// ring's arrays still cover its window.
fn assert_tiers_consistent(g: &Grid) {
    let s = &g.screen;
    for (ring, cap) in std::iter::once((s.ring(), s.cap))
        .chain(s.saved_primary.as_ref().map(|p| (p.ring(), p.cap)))
    {
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
    }
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
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Output, BCE blanks, wraps, partial-top scrolls, prompt marks,
    /// screen-scoped erases, ED 3, the alternate screen and reflows,
    /// interleaved with compaction slices of any size and run past the
    /// cap so the cold tier evicts: after every step the compacted grid
    /// reads exactly as its uncompacted twin.
    #[test]
    fn compaction_is_unobservable(
        rows in 2u16..6,
        cols in 4u16..20,
        cap in 0usize..24,
        ops in prop::collection::vec(op(), 1..40),
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(rows, cols, cap);
        for op in &ops {
            apply(op, &mut p, &mut g, &mut tp, &mut twin);
            assert_unobservable(&mut g, &mut twin);
        }
    }

    /// Rows recycled out of the vacated gap read as the twin's, whatever
    /// slice boundary the compaction stopped at and whatever blank the
    /// recycle writes.
    #[test]
    fn recycling_after_compaction_reads_as_the_twin(
        slices in prop::collection::vec(1u8..9, 1..6),
        refill in prop::collection::vec(op(), 1..20),
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(4, 12, 30);
        let fill = Op::Lines { count: 40, width: 17, color: 3, bare_lf: false };
        apply(&fill, &mut p, &mut g, &mut tp, &mut twin);
        for slice in slices {
            g.compact_history(slice.into());
            assert_unobservable(&mut g, &mut twin);
        }
        for op in refill.iter().filter(|op| !matches!(op, Op::Compact(_))) {
            apply(op, &mut p, &mut g, &mut tp, &mut twin);
            assert_unobservable(&mut g, &mut twin);
        }
    }

    /// W → W' → W with no eviction, on the live primary and on one saved
    /// under `?1049h` and resized before the restore: rows, cursor,
    /// marks and the placement remaps match the twin's (REQ-604, REQ-407).
    #[test]
    fn reflow_round_trips_match_the_twin(
        narrow in 4u16..16,
        lines in 1u8..20,
        saved in any::<bool>(),
        slice in 1u8..40,
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(5, 16, 200);
        for op in [
            Op::Prompt,
            Op::Lines { count: lines, width: 23, color: 2, bare_lf: false },
            Op::Sized,
            Op::Prompt,
        ] {
            apply(&op, &mut p, &mut g, &mut tp, &mut twin);
        }
        g.compact_history(slice.into());
        assert_unobservable(&mut g, &mut twin);
        if saved {
            apply(&Op::Alt(true), &mut p, &mut g, &mut tp, &mut twin);
            g.compact_history(usize::MAX);
        }
        for step in [Op::Reflow { rows: 5, cols: narrow as u8 }, Op::Reflow { rows: 5, cols: 16 }] {
            apply(&step, &mut p, &mut g, &mut tp, &mut twin);
            assert_unobservable(&mut g, &mut twin);
        }
        if saved {
            apply(&Op::Alt(false), &mut p, &mut g, &mut tp, &mut twin);
            assert_unobservable(&mut g, &mut twin);
        }
        prop_assert_eq!(g.cursor(), twin.cursor());
    }

    /// Cold rows carrying distinct pens and sizing handles keep them
    /// across both table sweeps, which then compact the tables under
    /// new output.
    #[test]
    fn table_sweeps_keep_cold_rows_attributes_and_sizing(
        colors in prop::collection::vec(0u8..255, 1..12),
        slice in 1u8..30,
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(3, 12, 40);
        let mut out = Vec::new();
        for (i, color) in colors.iter().enumerate() {
            out.extend_from_slice(format!("\x1b[38;5;{color}mc{i}\x1b[m").as_bytes());
            out.extend_from_slice(b"\x1b]66;s=2;S\x07\r\n\r\n");
        }
        out.extend_from_slice(b"\r\n\r\n\r\n\r\n");
        drive(&mut p, &mut g, &out);
        drive(&mut tp, &mut twin, &out);
        g.compact_history(slice.into());
        prop_assert!(!g.screen.cold.is_empty());
        let churn = b"\x1b[38;2;1;2;3mx\x1b[38;2;4;5;6my\x1b[m\x1b]66;s=3;Z\x07\r\n";
        drive(&mut p, &mut g, churn);
        drive(&mut tp, &mut twin, churn);
        for grid in [&mut g, &mut twin] {
            grid.gc_styles();
            grid.gc_sizings();
        }
        prop_assert_eq!(resolved(&g), resolved(&twin));
        assert_tiers_consistent(&g);
    }

    /// The commands that once wrote the whole physical ring leave both
    /// tiers of history alone at any ring base and slice boundary.
    #[test]
    fn whole_screen_commands_leave_compacted_history_alone(
        fill in 10u8..60,
        slice in 1u8..40,
        which in prop::collection::vec(0u8..6, 1..6),
    ) {
        let (mut p, mut g, mut tp, mut twin) = grids(4, 10, 30);
        apply(&Op::Lines { count: fill, width: 13, color: 5, bare_lf: false },
            &mut p, &mut g, &mut tp, &mut twin);
        g.compact_history(slice.into());
        let before = history(&g);
        for w in which {
            apply(&Op::WholeScreen(w), &mut p, &mut g, &mut tp, &mut twin);
            prop_assert_eq!(history(&g), before.clone());
            assert_unobservable(&mut g, &mut twin);
        }
    }
}

/// Each history and live cell as its pen and sizing resolve, so two
/// grids whose tables were swept differently still compare.
fn resolved(g: &Grid) -> Vec<Vec<(Grapheme, Attributes, Option<Sizing>)>> {
    g.rows_with_wrap(AltScreenRows::Include)
        .map(|(cells, _)| {
            cells
                .iter()
                .map(|c| {
                    (
                        c.grapheme,
                        *g.style(c.style),
                        c.sizing.and_then(|h| g.sizing_by_handle(h)).copied(),
                    )
                })
                .collect()
        })
        .collect()
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

/// A compacted grid carried across the state dump, live and with the
/// primary saved under the alternate screen, reads as its twin and keeps
/// scrolling in FIFO order after the restore.
#[cfg(feature = "state-dump")]
#[test]
fn a_compacted_grid_survives_the_state_dump() {
    for saved in [false, true] {
        let (mut p, mut g, mut tp, mut twin) = grids(4, 30, 40);
        for op in [
            Op::Prompt,
            Op::Lines {
                count: 50,
                width: 37,
                color: 3,
                bare_lf: false,
            },
            Op::Sized,
            Op::Alt(saved),
        ] {
            apply(&op, &mut p, &mut g, &mut tp, &mut twin);
        }
        g.compact_history(25);
        let json = serde_json::to_vec(&g).expect("dump");
        let mut restored: Grid = serde_json::from_slice(&json).expect("restore");
        restored.check_restored().expect("consistent");
        assert_unobservable(&mut restored, &mut twin);
        for op in [
            Op::Alt(false),
            Op::Lines {
                count: 30,
                width: 12,
                color: 6,
                bare_lf: false,
            },
        ] {
            apply(&op, &mut p, &mut restored, &mut tp, &mut twin);
            assert_unobservable(&mut restored, &mut twin);
        }
    }
}

/// A discard the OS refuses changes nothing: the rows still move to the
/// cold tier and the ring stays usable.
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
    paged_cells::fail_discards(true);
    let done = g.compact_history(usize::MAX);
    paged_cells::fail_discards(false);
    assert!(done);
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
        "{rows}x{cols}: 1-row slice re-discarding the whole gap {:?}",
        start.elapsed()
    );
}
