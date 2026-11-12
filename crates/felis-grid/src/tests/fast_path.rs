use super::*;

/// `Sink::print_utf8_run` must be byte-for-byte equivalent to a
/// per-byte decode regardless of chunking; one byte per `advance`
/// forces the pending-drain path at every sequence boundary.
#[test]
fn bulk_utf8_run_is_chunking_invariant() {
    let input = "\u{202E}你好😀aé e\u{0301}f\r\n\u{2066}ab".as_bytes();

    let mut whole_p = Parser::new();
    let mut whole_g = Grid::new(3, 16);
    drive(&mut whole_p, &mut whole_g, input);

    let mut split_p = Parser::new();
    let mut split_g = Grid::new(3, 16);
    for &b in input {
        split_p.advance(&mut split_g, &[b]);
    }

    for r in 0..whole_g.rows() {
        for c in 0..whole_g.cols() {
            assert_eq!(
                whole_g.cell(r, c),
                split_g.cell(r, c),
                "cell ({r}, {c}) diverged between whole and per-byte chunking"
            );
        }
    }
}

/// A recycled alt-screen row keeps its previous tenant's cells past the
/// watermark; an ASCII run landing there must not read them as live.
#[test]
fn ascii_run_past_the_watermark_ignores_a_stale_spacer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, "\x1b[?1049hx中\n\réé\x1b[mb".as_bytes());
    let row: Vec<_> = (0..4).map(|c| g.cell(0, c).unwrap().grapheme).collect();
    assert_eq!(
        row,
        [
            Grapheme::Char('é'),
            Grapheme::Char('é'),
            Grapheme::Ascii(b'b'),
            Grapheme::Empty,
        ]
    );
}

// A default-pen scroll recycles a physical row by dropping its
// watermark and leaving unreferenced bytes in place
// (docs/explanation/data-model/grid-and-cells.md "Occupancy
// watermark"); no reader or writer may expose that stale tail.

/// Rows `AAAA`, `BBBB`, `CCCC`, then one bottom line feed: the recycled
/// bottom row's physical slot still holds `AAAA`.
fn scrolled_grid_with_stale_tail() -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(
        &mut p,
        &mut g,
        b"\x1b[1;1HAAAAAAAAAA\x1b[2;1HBBBBBBBBBB\x1b[3;1HCCCCCCCCCC",
    );
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    (p, g)
}

#[test]
fn recycled_row_reads_blank_past_the_watermark() {
    let (_p, g) = scrolled_grid_with_stale_tail();
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'C'));
    for c in 0..10 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "recycled row col {c} must read blank, not the stale 'A'"
        );
    }
    assert_eq!(g.row_content(2).unwrap().len(), 0, "watermark is 0");
}

/// `print_str`'s cold gap fork blanks the leading gap before writing.
#[test]
fn bulk_run_past_watermark_blanks_the_leading_gap() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    drive(&mut p, &mut g, b"\x1b[3;6HZZZ");
    for c in 0..5 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "gap col {c} must be blanked, not stale 'A'"
        );
    }
    for c in 5..8 {
        assert_eq!(g.cell(2, c).unwrap().grapheme, Grapheme::Ascii(b'Z'));
    }
    for c in 8..10 {
        assert!(g.cell(2, c).unwrap().is_blank(), "past-run col {c} clipped");
    }
    assert_eq!(g.row_content(2).unwrap().len(), 8);
}

/// The per-glyph path (`put_grapheme`) blanks the same leading gap.
#[test]
fn single_glyph_past_watermark_blanks_the_leading_gap() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    // "é" routes through the per-byte decoder, not `print_str`.
    drive(&mut p, &mut g, "\x1b[3;6Hé".as_bytes());
    for c in 0..5 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "gap col {c} must be blanked, not stale 'A'"
        );
    }
    assert_eq!(g.cell(2, 5).unwrap().grapheme, Grapheme::Char('é'));
    for c in 6..10 {
        assert!(g.cell(2, c).unwrap().is_blank());
    }
}

/// The watermark bumps only to the run's end; `cell` clips the rest.
#[test]
fn fast_path_write_clips_the_stale_tail_past_the_run() {
    let (mut p, mut g) = scrolled_grid_with_stale_tail();
    drive(&mut p, &mut g, b"HI");
    assert_eq!(g.cell(2, 0).unwrap().grapheme, Grapheme::Ascii(b'H'));
    assert_eq!(g.cell(2, 1).unwrap().grapheme, Grapheme::Ascii(b'I'));
    for c in 2..10 {
        assert!(
            g.cell(2, c).unwrap().is_blank(),
            "col {c} past the run must clip the stale 'A'"
        );
    }
}

/// BCE is live content: the recycled row materializes every cell with
/// the pen background and pins the watermark at `cols`.
#[test]
fn bce_scroll_materializes_the_full_row_with_pen_background() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(
        &mut p,
        &mut g,
        b"\x1b[44m\x1b[1;1HAAAAAAAAAA\x1b[2;1HBBBBBBBBBB\x1b[3;1HCCCCCCCCCC",
    );
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    for c in 0..10 {
        let cell = *g.cell(2, c).unwrap();
        assert_eq!(
            g.style(cell.style).bg,
            Color::Indexed(4),
            "BCE row col {c} carries the pen background"
        );
        assert_eq!(cell.grapheme, Grapheme::Empty);
    }
    assert_eq!(
        g.row_content(2).unwrap().len(),
        10,
        "BCE pins the watermark at cols"
    );
}
