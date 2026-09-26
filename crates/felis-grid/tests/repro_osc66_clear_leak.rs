//! An OSC 66 sized run must not leave a stale sizing handle behind after
//! ED/EL or a scroll (`docs/explanation/data-model/grid-and-cells.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grapheme;

mod common;
use common::drive;

#[test]
fn ed_clear_drops_stale_sizing_handles() {
    let g = drive(4, 12, b"\x1b]66;s=2;Hi\x07\x1b[2J");

    for r in 0..g.rows() {
        for c in 0..g.cols() {
            assert_eq!(
                g.cell(r, c).unwrap().grapheme,
                Grapheme::Empty,
                "cell ({r},{c}) should be blank after ED 2"
            );
            assert_eq!(
                g.cell_sizing_handle(r, c),
                None,
                "cell ({r},{c}) still carries a stale sizing handle after ED 2"
            );
        }
    }
}

#[test]
fn scrolled_in_plain_text_is_not_resized() {
    let g = drive(4, 12, b"\x1b]66;s=2;Hi\x07\x1b[2J\x1b[4;1HA\r\nB\r\nC\r\nD");

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'A'));
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::Ascii(b'B'));
    assert_eq!(
        g.cell_sizing_handle(0, 0),
        None,
        "plain 'A' scrolled into (0,0) inherited the cleared run's sizing handle"
    );
    assert_eq!(
        g.cell_sizing_handle(1, 0),
        None,
        "plain 'B' scrolled into (1,0) inherited the cleared run's sizing handle"
    );
}
