//! OSC 66 byte streams shaped like presenterm / yazi output, pinned at
//! the grid level (graphemes, sizing handles, cursor rest position).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;

use crate::common;
use common::drive;

fn render(grid: &Grid) -> String {
    let mut out = String::new();
    common::write_sizing_rows(&mut out, grid, '_', '·');
    out
}

#[test]
fn presenterm_style_slide_title_then_body() {
    // Each title lands where CUP parked the cursor; the plain print
    // after the run lands past the block extent.
    let bytes = b"\x1b]66;s=2;Hello\x07\x1b[3;1H\x1b]66;s=2;World\x07\x1b[5;1HWelcome!";
    let g = drive(6, 16, bytes);
    insta::assert_snapshot!(render(&g), @r#"
    row 0: "H·e·l·l·o·      " sizings="1111111111......"
    row 1: "··········      " sizings="1111111111......"
    row 2: "W·o·r·l·d·      " sizings="2222222222......"
    row 3: "··········      " sizings="2222222222......"
    row 4: "Welcome!        " sizings="................"
    row 5: "                " sizings="................"
    "#);
}

#[test]
fn yazi_style_icon_grid_with_w_override() {
    // `w=2` with no `s` gives a 2x1 block per icon.
    let bytes = b"\x1b]66;w=2;\xee\x82\xa0\x07 file_one\r\n\
                  \x1b]66;w=2;\xee\x82\xa1\x07 file_two\r\n\
                  \x1b]66;w=2;\xee\x82\xa2\x07 file_three";
    let g = drive(3, 16, bytes);
    insta::assert_snapshot!(render(&g), @r#"
    row 0: "\u{e0a0}· file_one     " sizings="11.............."
    row 1: "\u{e0a1}· file_two     " sizings="22.............."
    row 2: "\u{e0a2}· file_three   " sizings="33.............."
    "#);
}

#[test]
fn presenterm_centered_title_with_h_alignment_metadata() {
    // `h=2` is recorded on the sizing handle for the renderer; it does
    // not move the block on screen.
    let bytes = b"\x1b[2;5H\x1b]66;s=2:h=2;Hi\x07";
    let g = drive(3, 16, bytes);
    insta::assert_snapshot!(render(&g), @r#"
    row 0: "                " sizings="................"
    row 1: "    H·i·        " sizings="....1111........"
    row 2: "    ····        " sizings="....1111........"
    "#);
    let sizing = g.cell_sizing(1, 4).expect("primary sized cell");
    assert_eq!(sizing.halign(), felis_grid::HAlign::Center);
    assert_eq!(sizing.scale(), 2);
}
