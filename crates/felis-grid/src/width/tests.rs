use super::*;
use crate::{Grapheme, Grid};

#[test]
fn grapheme_width_matches_unicode_width() {
    use unicode_width::UnicodeWidthChar;
    let g = Grid::new(1, 4);
    for cp in 0..=0x0010_FFFF_u32 {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        assert_eq!(
            g.screen.grapheme_width(Grapheme::Char(c)),
            c.width().unwrap_or(0) as u8,
            "U+{cp:04X}"
        );
    }
}

#[test]
fn bulk_width_matches_grapheme_width() {
    // `table_width` stands in for `grapheme_width` in the batch loop: a
    // nonzero answer must equal it, and 0 must mean exactly "width 0 or
    // in `extends_previous_grapheme`'s fold set".
    let g = Grid::new(1, 4);
    let widths = bmp_widths();
    for cp in 0..=0x0010_FFFF_u32 {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        let bulk = table_width(widths, c);
        assert_eq!(bulk, bulk_width_or_defer(c), "U+{cp:04X} table diverges");
        let real = usize::from(g.screen.grapheme_width(Grapheme::Char(c)));
        if bulk == 0 {
            let is_fold_defer =
                matches!(c, '\u{1F3FB}'..='\u{1F3FF}') || matches!(c, '\u{1F1E6}'..='\u{1F1FF}');
            assert!(
                real == 0 || is_fold_defer,
                "U+{cp:04X} deferred but not a fold candidate"
            );
        } else {
            assert_eq!(bulk, real, "U+{cp:04X} width diverges");
        }
    }
}

#[test]
fn a_cluster_takes_at_most_two_cells() {
    assert_eq!(cluster_cell_width("e\u{301}"), 1);
    assert_eq!(cluster_cell_width("\u{1F469}\u{200D}\u{1F4BB}"), 2);
    assert_eq!(
        cluster_cell_width("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        2
    );
}

#[test]
fn a_cluster_of_zero_width_scalars_takes_no_cell() {
    assert_eq!(cluster_cell_width("\u{301}\u{302}"), 0);
}
