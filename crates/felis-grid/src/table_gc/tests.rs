use super::*;
use crate::{AttrFlags, Attributes, Color, Grapheme, Grid, UnderlineStyle};
use felis_vt::Parser;
use proptest::prelude::*;

#[test]
fn an_ordinary_session_is_never_swept() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(4, 20);
    let mut parser = Parser::default();
    parser.advance(&mut grid, b"\x1b[1;31mred\x1b[0m \x1b]66;s=2;A\x1b\\");

    let before = grid.clone();
    gc.maybe_sweep(&mut grid);

    assert!(before == grid, "nothing over threshold must be left alone");
    assert_eq!(gc.style_threshold(), STYLE_INIT);
    assert_eq!(gc.sizing_threshold(), SIZING_INIT);
}

#[test]
fn a_forced_sweep_compacts_below_the_threshold() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(4, 20);
    let mut parser = Parser::default();
    for i in 0..8 {
        parser.advance(&mut grid, format!("\x1b[H\x1b[38;5;{i}mX").as_bytes());
    }
    let before = grid.style_table_len();

    gc.sweep(&mut grid);

    assert!(
        grid.style_table_len() < before,
        "dead pens go even under the threshold: {before} -> {}",
        grid.style_table_len()
    );
    parser.advance(&mut grid, b"Y");
    let cell = grid.cell(0, 1).expect("cell in range");
    assert_eq!(
        grid.style(cell.style).fg,
        Color::Indexed(7),
        "the pen survives the forced sweep"
    );
}

#[test]
fn a_repaint_flood_is_swept_back_to_the_live_set() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(4, 20);
    let mut parser = Parser::default();
    for _ in 0..=SIZING_INIT {
        parser.advance(&mut grid, b"\x1b[H\x1b]66;s=2;A\x1b\\");
    }
    assert!(grid.sizing_count() > SIZING_INIT);

    gc.maybe_sweep(&mut grid);

    assert!(
        grid.sizing_count() < 8,
        "the live run is all that survives, got {}",
        grid.sizing_count()
    );
    assert_eq!(gc.sizing_threshold(), SIZING_INIT, "no need to raise it");
}

#[test]
fn a_truecolor_repaint_flood_is_swept_back_to_the_live_set() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(4, 20);
    let mut parser = Parser::default();
    for i in 0..=STYLE_INIT {
        let (r, g, b) = (i / 65536 % 256, i / 256 % 256, i % 256);
        parser.advance(
            &mut grid,
            format!("\x1b[H\x1b[38;2;{r};{g};{b}mX").as_bytes(),
        );
    }
    assert!(grid.style_table_len() > STYLE_INIT);

    gc.maybe_sweep(&mut grid);

    assert!(
        grid.style_table_len() < 8,
        "one live cell's pen is all that survives, got {}",
        grid.style_table_len()
    );
    assert_eq!(gc.style_threshold(), STYLE_INIT, "no need to raise it");
}

#[test]
fn a_full_scrollback_buys_headroom_the_live_set_alone_would_not() {
    let mut gc = TableGc::new();
    let (rows, cols) = (4u16, 20u16);
    let mut grid = Grid::new(rows, cols);
    let mut parser = Parser::default();
    for i in 0..(crate::DEFAULT_SCROLLBACK_ROWS + usize::from(rows)) {
        parser.advance(&mut grid, format!("r{i}\r\n").as_bytes());
    }
    for i in 0..(STYLE_INIT * 2) {
        let (r, g, b) = (i / 65536 % 256, i / 256 % 256, i % 256);
        // Repaint in place: a scrolling flood keeps its pens live in scrollback.
        parser.advance(
            &mut grid,
            format!("\x1b[H\x1b[38;2;{r};{g};{b}mX").as_bytes(),
        );
    }
    assert!(grid.style_table_len() > STYLE_INIT);

    gc.maybe_sweep(&mut grid);

    let live = grid.style_table_len();
    let swept_cells = (crate::DEFAULT_SCROLLBACK_ROWS + usize::from(rows)) * usize::from(cols);
    assert!(
        live * 2 < swept_cells / HEADROOM_DIVISOR,
        "the test is only meaningful while the live set cannot buy the headroom itself",
    );
    assert!(
        gc.style_threshold() >= live + swept_cells / HEADROOM_DIVISOR,
        "trigger must scale to the {swept_cells} cells swept, got {} for {live} live",
        gc.style_threshold(),
    );
}

/// The sizing ceiling sits below the handle space: a handle that cannot
/// be minted draws the run unsized.
#[test]
fn the_trigger_point_rises_only_on_a_sweep_that_reclaims_little() {
    assert_eq!(raise_if_still_full(10, 100, 1000), 100, "half reclaimed");
    assert_eq!(raise_if_still_full(51, 100, 1000), 200, "barely reclaimed");
    assert_eq!(
        raise_if_still_full(SIZING_MAX, SIZING_MAX, SIZING_MAX),
        SIZING_MAX
    );
    assert_eq!(
        raise_if_still_full(SIZING_MAX, SIZING_MAX / 2 + 1, SIZING_MAX),
        SIZING_MAX,
        "doubling is clamped, not skipped",
    );
    assert!(SIZING_MAX < u16::MAX as usize);
}

/// A scrollback of unique truecolor pens holds millions of live entries;
/// any cap pegs the trigger and fires the O(cells) sweep on every drain.
#[test]
fn the_style_raise_clears_the_live_set_uncapped() {
    let live = 1 << 21;
    assert_eq!(
        raise_past_live_set(live, live),
        live * 2,
        "a live set past any fixed cap must still lift the trigger clear",
    );
}

#[test]
fn the_style_headroom_pays_for_the_cells_the_sweep_scanned() {
    let live = 11_520; // one 60x192 screen of distinct pens
    let shallow = raise_past_live_set(live, live);
    let deep = raise_past_live_set(live, 1_930_000); // the same screen over a full 10k ring
    assert!(
        deep - live >= 1_930_000 / 32,
        "a sweep over a full ring must buy headroom proportional to it, got {}",
        deep - live,
    );
    assert!(
        deep > shallow,
        "deeper scrollback must sweep less often, not equally often",
    );
}

/// A flat fraction of the scan would sweep several times per frame on a
/// full screen of distinct pens.
#[test]
fn the_style_headroom_still_clears_a_live_set_that_fills_the_scan() {
    let live = 11_520;
    assert_eq!(raise_past_live_set(live, live), live * 2);
}

#[test]
fn the_style_raise_never_falls_below_the_initial_trigger_point() {
    assert!(raise_past_live_set(0, 0) >= STYLE_INIT);
    assert!(raise_past_live_set(10, 100) >= STYLE_INIT);
}

#[test]
fn a_wall_of_live_pens_stops_resweeping_after_one_futile_sweep() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(40, 110); // 4400 cells > STYLE_INIT
    let mut parser = Parser::default();
    for i in 0..(40 * 110) {
        let (r, g, b) = (i / 65536 % 256, i / 256 % 256, i % 256);
        parser.advance(
            &mut grid,
            format!(
                "\x1b[{};{}H\x1b[38;2;{r};{g};{b}mX",
                i / 110 + 1,
                i % 110 + 1
            )
            .as_bytes(),
        );
    }
    assert!(grid.style_table_len() > STYLE_INIT);

    gc.maybe_sweep(&mut grid);
    let live = grid.style_table_len();
    assert!(live > STYLE_INIT, "every pen is on a live cell");
    assert!(
        gc.style_threshold() >= live * 2,
        "trigger point must clear the live set, got {} for {live} live",
        gc.style_threshold()
    );

    let before = grid.clone();
    gc.maybe_sweep(&mut grid);
    assert!(before == grid, "second sweep must not run");
}

const RICH_PEN: &[u8] = b"\x1b[0m\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m\x1b[4:3m\x1b[58;2;7;8;9m\x1bV";

fn intern_unprinted_pens(parser: &mut Parser, grid: &mut Grid) {
    for i in 0..=STYLE_INIT {
        let (r, g, b) = (i / 65536 % 256, i / 256 % 256, i % 256);
        parser.advance(grid, format!("\x1b[48;2;{r};{g};{b}m").as_bytes());
    }
    assert!(grid.style_table_len() > STYLE_INIT);
}

fn sweep_into_a_compaction(gc: &mut TableGc, grid: &mut Grid) {
    let before = grid.style_table_len();
    gc.maybe_sweep(grid);
    assert!(
        grid.style_table_len() < before,
        "the sweep must compact for the test to mean anything"
    );
}

fn assert_printed_with_rich_pen(grid: &Grid, row: u16, col: u16) {
    let attrs = *grid.style(grid.cell(row, col).unwrap().style);
    assert_eq!(attrs.fg, Color::Rgb(1, 2, 3), "truecolor foreground");
    assert_eq!(attrs.bg, Color::Rgb(4, 5, 6), "truecolor background");
    assert_eq!(
        attrs.underline_color,
        Color::Rgb(7, 8, 9),
        "underline color"
    );
    assert_eq!(attrs.underline_style, UnderlineStyle::Curly);
    assert!(attrs.flags.contains(AttrFlags::UNDERLINE));
    assert!(attrs.flags.contains(AttrFlags::ISO_PROTECTED));
    assert!(
        grid.screen.style_table.has_iso_protected(),
        "EL must know a protected cell exists"
    );
}

fn assert_el_spares(parser: &mut Parser, grid: &mut Grid, row: u16, col: u16) {
    parser.advance(grid, format!("\x1b[{};1H\x1b[2K", row + 1).as_bytes());
    assert_eq!(grid.cell(row, col).unwrap().grapheme, Grapheme::Ascii(b'A'));
}

#[test]
fn text_printed_after_a_compacting_sweep_keeps_the_pen_set_before_it() {
    for alternate in [false, true] {
        let mut gc = TableGc::new();
        let mut grid = Grid::new(4, 20);
        let mut parser = Parser::default();
        if alternate {
            parser.advance(&mut grid, b"\x1b[?1049h");
        }
        intern_unprinted_pens(&mut parser, &mut grid);
        parser.advance(&mut grid, RICH_PEN);

        sweep_into_a_compaction(&mut gc, &mut grid);
        parser.advance(&mut grid, b"A");

        assert_eq!(grid.on_alternate_screen(), alternate);
        assert_printed_with_rich_pen(&grid, 0, 0);
        assert_el_spares(&mut parser, &mut grid, 0, 0);
    }
}

#[test]
fn a_pen_saved_by_decsc_before_a_compacting_sweep_is_restored_intact() {
    for alternate in [false, true] {
        let mut gc = TableGc::new();
        let mut grid = Grid::new(4, 20);
        let mut parser = Parser::default();
        if alternate {
            parser.advance(&mut grid, b"\x1b[?1049h");
        }
        parser.advance(&mut grid, RICH_PEN);
        parser.advance(&mut grid, b"\x1b7\x1bW\x1b[m");
        intern_unprinted_pens(&mut parser, &mut grid);

        sweep_into_a_compaction(&mut gc, &mut grid);
        parser.advance(&mut grid, b"\x1b8A");

        assert_printed_with_rich_pen(&grid, 0, 0);
        assert_el_spares(&mut parser, &mut grid, 0, 0);
    }
}

/// `?1049h` saves the primary's cursor; the sweep runs while the
/// alternate screen is up and the pen it saved is on no cell.
#[test]
fn the_primary_pen_saved_across_the_alternate_screen_survives_a_sweep_there() {
    let mut gc = TableGc::new();
    let mut grid = Grid::new(4, 20);
    let mut parser = Parser::default();
    parser.advance(&mut grid, RICH_PEN);
    parser.advance(&mut grid, b"\x1b[?1049h\x1bW\x1b[m");
    intern_unprinted_pens(&mut parser, &mut grid);

    sweep_into_a_compaction(&mut gc, &mut grid);
    parser.advance(&mut grid, b"\x1b[?1049lA");

    assert!(!grid.on_alternate_screen());
    assert_printed_with_rich_pen(&grid, 0, 0);
    assert_el_spares(&mut parser, &mut grid, 0, 0);
}

#[derive(Debug, Clone)]
enum Step {
    Pen(u8, u8, u8, bool),
    Print(u8),
    Sweep,
    SaveCursor,
    RestoreCursor,
    Alternate(bool),
    Home,
    Reset,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<bool>())
            .prop_map(|(r, g, b, p)| Step::Pen(r, g, b, p)),
        // A pen seen again after a compaction hits the intern cache.
        4 => (0u8..2, 0u8..2, 0u8..2, any::<bool>())
            .prop_map(|(r, g, b, p)| Step::Pen(r, g, b, p)),
        4 => (b'a'..=b'z').prop_map(Step::Print),
        2 => Just(Step::Sweep),
        1 => Just(Step::SaveCursor),
        1 => Just(Step::RestoreCursor),
        1 => any::<bool>().prop_map(Step::Alternate),
        1 => Just(Step::Home),
        1 => Just(Step::Reset),
    ]
}

fn bytes_of(step: &Step) -> Vec<u8> {
    match *step {
        Step::Pen(r, g, b, protected) => {
            let spa = if protected { "\x1bV" } else { "\x1bW" };
            format!("\x1b[0;38;2;{r};{g};{b};58;2;{b};{g};{r}m{spa}").into_bytes()
        }
        Step::Print(c) => vec![c],
        Step::Sweep => Vec::new(),
        Step::SaveCursor => b"\x1b7".to_vec(),
        Step::RestoreCursor => b"\x1b8".to_vec(),
        Step::Alternate(on) => if on { b"\x1b[?1049h" } else { b"\x1b[?1049l" }.to_vec(),
        Step::Home => b"\x1b[H".to_vec(),
        Step::Reset => b"\x1bc".to_vec(),
    }
}

fn resolved_screen(grid: &Grid) -> Vec<(Grapheme, Attributes)> {
    let mut out = Vec::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).unwrap();
            out.push((cell.grapheme, *grid.style(cell.style)));
        }
    }
    out
}

proptest! {
    /// A grid swept at arbitrary points must stay indistinguishable from
    /// one never swept.
    #[test]
    fn sweeps_are_invisible_to_what_is_printed_afterwards(
        steps in proptest::collection::vec(step(), 1..120),
    ) {
        let mut swept = Grid::new(3, 8);
        let mut unswept = Grid::new(3, 8);
        let (mut p1, mut p2) = (Parser::default(), Parser::default());
        for s in &steps {
            if matches!(s, Step::Sweep) {
                swept.sweep_styles(sealed::Token);
            }
            let bytes = bytes_of(s);
            p1.advance(&mut swept, &bytes);
            p2.advance(&mut unswept, &bytes);
            prop_assert_eq!(*swept.style(swept.pen_style), swept.pen());
            prop_assert_eq!(resolved_screen(&swept), resolved_screen(&unswept));
        }
        p1.advance(&mut swept, b"\x1b[?1049l\x1b8Z\x1b[H\x1b[2J");
        p2.advance(&mut unswept, b"\x1b[?1049l\x1b8Z\x1b[H\x1b[2J");
        prop_assert_eq!(resolved_screen(&swept), resolved_screen(&unswept));
    }
}
