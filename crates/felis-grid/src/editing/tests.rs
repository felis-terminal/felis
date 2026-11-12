use crate::test_support::{cluster_at, drive, drive_new, drive_take, reply_text, responses};
use crate::*;
use felis_vt::Parser;

mod clusters;
mod cursor_motion;
mod margins;
mod protection;
mod reports;
mod reverse_wrap;
mod ring_band;
mod saved_cursor;

pub(super) fn logical_row(g: &Grid, row: u16) -> String {
    (0..g.screen.cols)
        .map(|c| match g.cell(row, c).unwrap().grapheme {
            Grapheme::Ascii(b) => char::from(b),
            Grapheme::Empty => ' ',
            _ => '?',
        })
        .collect()
}
