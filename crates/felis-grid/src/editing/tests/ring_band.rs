use super::*;

/// `DECSED 1` addresses rows through the ring base: after a scroll the
/// physical order is rotated and `logical * cols` disagrees.
#[test]
fn decsed_1_addresses_rows_through_the_ring_base_after_scroll() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4;1H\n");
    assert_ne!(
        g.screen.base, 0,
        "scroll must advance the base, else this test cannot catch the bug"
    );
    for (row1, ch) in [(1u8, b'a'), (2, b'b'), (3, b'c'), (4, b'd')] {
        drive(&mut p, &mut g, format!("\x1b[{row1};1H").as_bytes());
        drive(&mut p, &mut g, &[ch, ch, ch, ch]);
    }
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[?1J");
    assert_eq!(logical_row(&g, 0), "    ", "logical row 0 fully erased");
    assert_eq!(
        logical_row(&g, 1),
        "  bb",
        "row 1 erased through the cursor"
    );
    assert_eq!(logical_row(&g, 2), "cccc", "row 2 untouched");
    assert_eq!(logical_row(&g, 3), "dddd", "row 3 untouched");
}

/// `DECSED 0` on a scrolled (rotated) grid.
#[test]
fn decsed_0_addresses_rows_through_the_ring_base_after_scroll() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4;1H\n");
    assert_ne!(g.screen.base, 0, "scroll must advance the base");
    for (row1, ch) in [(1u8, b'a'), (2, b'b'), (3, b'c'), (4, b'd')] {
        drive(&mut p, &mut g, format!("\x1b[{row1};1H").as_bytes());
        drive(&mut p, &mut g, &[ch, ch, ch, ch]);
    }
    drive(&mut p, &mut g, b"\x1b[2;2H\x1b[?0J");
    assert_eq!(
        logical_row(&g, 0),
        "aaaa",
        "row 0 untouched (before the cursor)"
    );
    assert_eq!(
        logical_row(&g, 1),
        "b   ",
        "row 1 erased from the cursor to EOL"
    );
    assert_eq!(logical_row(&g, 2), "    ", "row 2 fully erased");
    assert_eq!(logical_row(&g, 3), "    ", "row 3 fully erased");
}

/// An interior DECSTBM scroll is a band rotation: no cell content
/// moves physically, and every logical row reads back through the
/// band.
#[test]
fn interior_region_scroll_reads_back_through_the_band() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(g.screen.band_len, 3, "region scroll must arm the band");
    assert_eq!(g.screen.band_top, 1);
    assert_eq!(g.screen.band_rot, 1);
    assert_eq!(logical_row(&g, 0), "aaaa", "above the region: untouched");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(logical_row(&g, 3), "    ", "recycled region bottom blanks");
    assert_eq!(logical_row(&g, 4), "eeee", "below the region: untouched");
    drive(&mut p, &mut g, b"\x1b[4;1HXXXX\n");
    assert_eq!(g.screen.band_rot, 2);
    assert_eq!(logical_row(&g, 1), "dddd");
    assert_eq!(logical_row(&g, 2), "XXXX", "the print rode the rotation");
    assert_eq!(logical_row(&g, 3), "    ");
}

/// RI at the region top rotates the band back; equal up and down
/// scrolls cancel in `band_rot`.
#[test]
fn region_scroll_down_rotates_the_band_back() {
    let mut g = Grid::new(5, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[4;1H\n");
    drive(&mut p, &mut g, b"\x1b[2;1H\x1bM");
    assert_eq!(g.screen.band_rot, 0, "up then down cancels the rotation");
    assert_eq!(logical_row(&g, 1), "    ", "down-scroll blanks the top");
    assert_eq!(logical_row(&g, 2), "cccc");
    assert_eq!(logical_row(&g, 3), "dddd");
    assert_eq!(logical_row(&g, 4), "eeee");
}

/// Changing the scroll region while a band is rotated materializes
/// the active band first, or the two rotations would compose.
#[test]
fn region_change_materializes_the_previous_band() {
    let mut g = Grid::new(6, 4);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee\r\nffff",
    );
    drive(&mut p, &mut g, b"\x1b[2;5r\x1b[5;1H\n");
    assert_eq!(g.screen.band_rot, 1);
    drive(&mut p, &mut g, b"\x1b[3;1H\x1b[1L");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "    ", "IL's inserted blank");
    assert_eq!(logical_row(&g, 3), "dddd");
    assert_eq!(logical_row(&g, 4), "eeee", "the pre-IL blank fell off");
    assert_eq!(logical_row(&g, 5), "ffff");
}

/// A full-screen scroll into history after an interior band rotation
/// materializes first, so the evicted row is the logical top row.
#[test]
fn full_screen_scroll_after_interior_band_materializes_first() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    drive(&mut p, &mut g, b"\x1b[r\x1b[4;1H\n");
    assert_eq!(g.screen.band_len, 0, "the base bump must drop the band");
    assert_eq!(g.scrollback().len(), 1);
    let hist: Vec<Cell> = g.scrollback().row(0).unwrap().to_vec();
    let hist_text: String = hist
        .iter()
        .map(|c| match c.grapheme {
            Grapheme::Ascii(b) => char::from(b),
            _ => ' ',
        })
        .collect();
    assert_eq!(hist_text, "aaaa", "the logical top row was evicted");
    assert_eq!(logical_row(&g, 0), "cccc", "band content materialized");
    assert_eq!(logical_row(&g, 1), "    ");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(logical_row(&g, 3), "    ", "blank from the full scroll");
}

/// Full-screen RI with retained history takes the band path (a base
/// bump would resurrect history rows); history stays intact beneath.
#[test]
fn full_screen_ri_with_history_rotates_a_viewport_band() {
    let mut g = Grid::new(3, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd\r\neeee");
    assert_eq!(g.scrollback().len(), 2);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1bM");
    assert_eq!(g.screen.band_len, 3, "history forces the band path");
    assert_eq!(logical_row(&g, 0), "    ");
    assert_eq!(logical_row(&g, 1), "cccc");
    assert_eq!(logical_row(&g, 2), "dddd");
    assert_eq!(g.scrollback().len(), 2, "history untouched by the band");
    drive(&mut p, &mut g, b"\x1b[3;1H\n");
    assert_eq!(g.scrollback().len(), 3);
    assert_eq!(logical_row(&g, 0), "cccc");
    assert_eq!(logical_row(&g, 1), "dddd");
}

/// Entering the alt screen with a rotated band materializes the
/// primary first, since the snapshot records `base` but no band.
#[test]
fn alt_screen_roundtrip_preserves_band_rotated_content() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert_eq!(g.screen.band_len, 0, "the snapshot must not carry a band");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1HZZZZ\n\x1b[?1049l");
    assert_eq!(logical_row(&g, 0), "aaaa");
    assert_eq!(logical_row(&g, 1), "cccc", "rotation survived the trip");
    assert_eq!(logical_row(&g, 2), "    ");
    assert_eq!(logical_row(&g, 3), "dddd");
}

/// A resize with a rotated band keeps the logical content and drops
/// the band.
#[test]
fn resize_after_band_rotation_keeps_logical_content() {
    let mut g = Grid::new(4, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
    drive(&mut p, &mut g, b"\x1b[2;3r\x1b[3;1H\n");
    assert_ne!(g.screen.band_rot, 0);
    g.resize(4, 6);
    assert_eq!(g.screen.band_len, 0, "resize must drop the band");
    assert_eq!(logical_row(&g, 0), "aaaa  ");
    assert_eq!(logical_row(&g, 1), "cccc  ");
    assert_eq!(logical_row(&g, 2), "      ");
    assert_eq!(logical_row(&g, 3), "dddd  ");
}
