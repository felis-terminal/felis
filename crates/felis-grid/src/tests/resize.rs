use super::*;

#[test]
fn resize_keeps_prompt_marks_with_absolute_lines_intact() {
    // docs/explanation/data-model/scrollback.md: marks carry absolute
    // lines a resize cannot invalidate.
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(5, 4);
    drive(&mut p, &mut g, b"\x1b[1;1H\x1b]133;A\x07"); // line 0
    drive(&mut p, &mut g, b"\x1b[5;1H\x1b]133;A\x07"); // line 4
    assert_eq!(g.prompt_marks().len(), 2);
    g.resize(3, 4);
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0].line, 0);
    assert_eq!(marks[1].line, 4);
    assert_eq!(marks[0].kind, PromptKind::PromptStart);
}

#[test]
fn resize_preserves_top_left_quadrant() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 6);
    drive(&mut p, &mut g, b"abcdef\r\nghijkl\r\nmnopqr");
    g.resize(2, 4);
    assert_eq!(g.rows(), 2);
    assert_eq!(g.cols(), 4);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Ascii(b'd'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'g'));
}

/// A narrowing resize must trim the preserved `?47` alt-screen snapshot
/// too: an untrimmed wide buffer reinstalled into the narrow grid reads
/// every row after the first from the wrong column offset.
#[test]
fn resize_trims_the_preserved_alt_snapshot_not_just_the_primary() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[?47h");
    drive(&mut p, &mut g, b"\x1b[1;1HA\x1b[2;1HB");
    drive(&mut p, &mut g, b"\x1b[?47l");
    g.resize(4, 8);
    drive(&mut p, &mut g, b"\x1b[?47h");
    assert_eq!(g.rows(), 4);
    assert_eq!(g.cols(), 8);
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(
        g.cell(1, 0).unwrap().grapheme,
        Grapheme::Ascii(b'B'),
        "row 1 must show the alt buffer's row 1, not a mis-strided read"
    );
}

/// A grow-from-default resize must extend the implicit full-screen
/// scroll region, or `/bin/sh` (which never emits DECSTBM) keeps
/// line-feeding within the initial region and the grown bottom stays blank.
#[test]
fn resize_grow_extends_implicit_full_screen_scroll_region() {
    let mut p = Parser::new();
    // The daemon's cold-start shape: pool spawns 24×80, the client ships
    // Resize{82, 137} after the window appears.
    let mut g = Grid::new(24, 80);
    g.resize(82, 137);
    drive(&mut p, &mut g, b"\x1b[H");
    for _ in 0..30 {
        drive(&mut p, &mut g, b"\n");
    }
    drive(&mut p, &mut g, b"X");
    assert_eq!(
        g.cursor().row,
        30,
        "line-feed must advance into rows beyond the old bound",
    );
    assert_eq!(
        g.cell(30, 0).unwrap().grapheme,
        Grapheme::Ascii(b'X'),
        "stamp must land in the post-grow scroll region",
    );
}

/// An explicitly pinned narrow scroll region survives a resize that
/// still fits it; pagers in a resized window would otherwise mis-scroll.
#[test]
fn resize_preserves_explicit_narrow_scroll_region_when_it_still_fits() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    drive(&mut p, &mut g, b"\x1b[6;11r");
    g.resize(40, 120);
    drive(&mut p, &mut g, b"\x1b[11;1H");
    drive(&mut p, &mut g, b"keep\n");
    assert_eq!(
        g.cursor().row,
        10,
        "explicit DECSTBM band must survive a resize that still fits it",
    );
}
