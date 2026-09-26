//! Kitty Unicode-placeholder cell resolution, shared by every renderer
//! (`docs/reference/protocols/kitty-graphics.md`
//! "Unicode-placeholder placement").

use felis_protocol::kitty_graphics::placeholder::{PLACEHOLDER, Rank, decode_placement};

use crate::{Color, Grapheme, Grid, ScreenBuffer, images::ImageId};

/// The `U+10EEEE` cell at `(row, col)` shows tile `(tile_row, tile_col)`
/// of image `image_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceholderCell {
    pub row: u16,
    pub col: u16,
    pub image_id: ImageId,
    pub tile_row: u32,
    pub tile_col: u32,
}

impl Grid {}

fn placeholder_text(grapheme: Grapheme, screen: &ScreenBuffer) -> Option<String> {
    match grapheme {
        Grapheme::Char(c) if c == PLACEHOLDER => Some(c.to_string()),
        Grapheme::Cluster(id) => {
            let s = screen.cluster_str(id)?;
            s.starts_with(PLACEHOLDER).then(|| s.to_string())
        }
        _ => None,
    }
}

const fn fg_rgb24(color: Color) -> Option<u32> {
    match color {
        Color::Rgb(r, g, b) => Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32),
        _ => None,
    }
}

impl ScreenBuffer {
    /// A cell that omits the id inherits it from the previous placeholder
    /// cell on the row; an omitted column auto-increments from the
    /// previous cell of the same image; an omitted row inherits. A
    /// non-placeholder cell, or a missing id with no run to inherit
    /// from, breaks the run.
    #[must_use]
    pub fn placeholder_cells(&self) -> Vec<PlaceholderCell> {
        let mut out = Vec::new();
        for row in 0..self.rows() {
            let mut prev: Option<(ImageId, u32, u32)> = None;
            for col in 0..self.cols() {
                let Some(cell) = self.cell(row, col) else {
                    prev = None;
                    continue;
                };
                let Some(text) = placeholder_text(cell.grapheme, self) else {
                    prev = None;
                    continue;
                };
                let Some(decoded) = decode_placement(&text) else {
                    prev = None;
                    continue;
                };
                let fg24 = fg_rgb24(self.style(cell.style).fg);
                let image_id = match (decoded.image_id_msb, fg24) {
                    (Some(msb), Some(low)) => ImageId((msb.get() << 24) | low),
                    (None, Some(low)) => ImageId(low),
                    (_, None) => {
                        let Some((pid, _, _)) = prev else {
                            prev = None;
                            continue;
                        };
                        pid
                    }
                };
                let same_run = prev.filter(|p| p.0 == image_id);
                let tile_row = decoded
                    .row
                    .map(Rank::get)
                    .or_else(|| same_run.map(|p| p.1))
                    .unwrap_or(0);
                let tile_col = decoded
                    .col
                    .map(Rank::get)
                    .or_else(|| same_run.map(|p| p.2.saturating_add(1)))
                    .unwrap_or(0);
                prev = Some((image_id, tile_row, tile_col));
                out.push(PlaceholderCell {
                    row,
                    col,
                    image_id,
                    tile_row,
                    tile_col,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::drive;
    use felis_vt::Parser;

    #[test]
    pub(crate) fn id_comes_from_fg_and_column_auto_increments_across_a_run() {
        let mut g = Grid::new(1, 8);
        let mut p = Parser::new();
        drive(
            &mut p,
            &mut g,
            "\u{1b}[38;2;0;0;5m\u{10eeee}\u{10eeee}".as_bytes(),
        );
        let cells = g.placeholder_cells();
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells[0],
            PlaceholderCell {
                row: 0,
                col: 0,
                image_id: ImageId(5),
                tile_row: 0,
                tile_col: 0
            }
        );
        assert_eq!(
            cells[1],
            PlaceholderCell {
                row: 0,
                col: 1,
                image_id: ImageId(5),
                tile_row: 0,
                tile_col: 1
            }
        );
    }

    #[test]
    pub(crate) fn non_placeholder_cells_yield_nothing() {
        let mut g = Grid::new(1, 8);
        let mut p = Parser::new();
        drive(&mut p, &mut g, b"plain text");
        assert_eq!(g.placeholder_cells(), Vec::<PlaceholderCell>::new());
    }

    use felis_protocol::kitty_graphics::placeholder::diacritic_for;

    /// The sentinel plus diacritic ranks in (row, col, msb) order.
    pub(crate) fn cell_bytes(diacritic_ranks: &[u32]) -> String {
        let mut s = String::from('\u{10eeee}');
        for &r in diacritic_ranks {
            s.push(diacritic_for(Rank::new(r).unwrap()));
        }
        s
    }

    #[test]
    pub(crate) fn third_diacritic_supplies_the_high_byte_of_the_id() {
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        let mut bytes = String::from("\u{1b}[38;2;0;0;5m");
        bytes.push_str(&cell_bytes(&[0, 0, 7]));
        drive(&mut p, &mut g, bytes.as_bytes());
        let cells = g.placeholder_cells();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].image_id, ImageId((7 << 24) | 5));
    }

    #[test]
    pub(crate) fn explicit_row_is_used_and_inherited_across_the_run() {
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        let mut bytes = String::from("\u{1b}[38;2;0;0;9m");
        bytes.push_str(&cell_bytes(&[2, 0]));
        bytes.push('\u{10eeee}');
        drive(&mut p, &mut g, bytes.as_bytes());
        let cells = g.placeholder_cells();
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].tile_row, cells[0].tile_col), (2, 0));
        assert_eq!((cells[1].tile_row, cells[1].tile_col), (2, 1));
    }

    #[test]
    pub(crate) fn an_id_change_breaks_the_run_and_resets_the_tile_column() {
        let mut g = Grid::new(1, 6);
        let mut p = Parser::new();
        let mut bytes = String::from("\u{1b}[38;2;0;0;5m\u{10eeee}");
        bytes.push_str("\u{1b}[38;2;0;0;6m\u{10eeee}");
        drive(&mut p, &mut g, bytes.as_bytes());
        let cells = g.placeholder_cells();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].image_id, ImageId(5));
        assert_eq!(cells[1].image_id, ImageId(6));
        assert_eq!(cells[1].tile_col, 0);
    }

    #[test]
    pub(crate) fn no_id_and_no_run_skips_the_cell() {
        let mut g = Grid::new(1, 4);
        let mut p = Parser::new();
        let mut bytes = String::from('\u{10eeee}');
        bytes.push('\u{10eeee}');
        drive(&mut p, &mut g, bytes.as_bytes());
        assert_eq!(g.placeholder_cells(), Vec::<PlaceholderCell>::new());
    }

    /// The trailing cell keeps the id from the persisting pen, but its
    /// tile column restarts.
    #[test]
    pub(crate) fn a_gap_cell_resets_the_run_so_tile_column_restarts() {
        let mut g = Grid::new(1, 6);
        let mut p = Parser::new();
        let mut bytes = String::from("\u{1b}[38;2;0;0;5m\u{10eeee}");
        bytes.push('X');
        bytes.push('\u{10eeee}');
        drive(&mut p, &mut g, bytes.as_bytes());
        let cells = g.placeholder_cells();
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].col, cells[0].tile_col), (0, 0));
        assert_eq!(
            (cells[1].col, cells[1].tile_col),
            (2, 0),
            "the gap must reset the run, restarting the tile column at 0"
        );
    }
}
