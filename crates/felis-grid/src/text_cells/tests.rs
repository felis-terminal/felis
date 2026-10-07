use felis_vt::Parser;
use proptest::prelude::*;

use super::text_cells;
use crate::editing::is_emoji_modifier;
use crate::{Grapheme, Grid, char_cell_width, push_cell_text};

fn laid_out(text: &str) -> Vec<(&str, u8)> {
    text_cells(text).map(|(r, w)| (&text[r], w)).collect()
}

/// The cells printing `text` into an empty row leaves, as (text, width).
fn printed(text: &str) -> Vec<(String, u8)> {
    let mut g = Grid::new(1, 512);
    Parser::new().advance(&mut g, text.as_bytes());
    let s = g.screen();
    let spacer = |c: u16| {
        s.cell(0, c)
            .is_some_and(|c| matches!(c.grapheme, Grapheme::Spacer))
    };
    (0..s.cursor().col)
        .filter(|&c| !spacer(c))
        .map(|c| {
            let mut out = String::new();
            push_cell_text(s.cell(0, c).expect("in range"), s.cluster_table(), &mut out);
            (out, if spacer(c + 1) { 2 } else { 1 })
        })
        .collect()
}

#[test]
fn clusters_take_the_cells_the_grid_gives_them() {
    assert_eq!(laid_out("ab"), [("a", 1), ("b", 1)]);
    assert_eq!(laid_out("字"), [("字", 2)]);
    assert_eq!(laid_out("e\u{301}x"), [("e\u{301}", 1), ("x", 1)]);
    assert_eq!(laid_out("❤\u{FE0F}"), [("❤\u{FE0F}", 2)]);
    assert_eq!(laid_out("👍🏻"), [("👍🏻", 2)]);
    assert_eq!(laid_out("👩\u{200D}💻"), [("👩\u{200D}💻", 2)]);
    assert_eq!(laid_out("👩\u{200D}字"), [("👩\u{200D}", 2), ("字", 2)]);
    assert_eq!(laid_out("🇯🇵🇯"), [("🇯🇵", 2), ("🇯", 1)]);
    assert_eq!(laid_out("1\u{FE0F}\u{20E3}"), [("1\u{FE0F}\u{20E3}", 2)]);
}

/// A bidi override folded after the base still lets a flag pair but
/// breaks a ZWJ sequence, on the print path and in the layout alike.
#[test]
fn an_override_after_the_base_folds_as_printing_does() {
    let cases: [(&str, &[(&str, u8)]); 2] = [
        ("🇯\u{202E}🇵", &[("🇯\u{202E}🇵", 2)]),
        (
            "👩\u{202E}\u{200D}💻",
            &[("👩\u{202E}\u{200D}", 2), ("💻", 2)],
        ),
    ];
    for (text, cells) in cases {
        assert_eq!(laid_out(text), cells, "{text:?}");
        let printed = printed(text);
        let printed: Vec<(&str, u8)> = printed.iter().map(|(s, w)| (s.as_str(), *w)).collect();
        assert_eq!(printed, cells, "{text:?}");
    }
}

/// The grid has no cell to fold a leading extender into and drops it;
/// an overlay keeps it visible.
#[test]
fn a_leading_extender_takes_its_own_cell() {
    assert_eq!(laid_out("\u{301}a"), [("\u{301}", 1), ("a", 1)]);
    assert_eq!(laid_out("🏻a"), [("🏻", 2), ("a", 1)]);
    assert_eq!(printed("🏻a"), [("a".to_owned(), 1)]);
}

/// The grid refuses to grow a cluster past its cap and drops the rest;
/// the cell count is the same, and an overlay keeps every mark.
#[test]
fn a_cluster_keeps_growing_past_the_grid_cap() {
    let text = format!("a{}b", "\u{301}".repeat(crate::ClusterText::CAP));
    assert_eq!(laid_out(&text), [(&text[..text.len() - 1], 1), ("b", 1)]);
    let widths: Vec<u8> = printed(&text).into_iter().map(|(_, w)| w).collect();
    assert_eq!(widths, [1, 1]);
}

const ALPHABET: &[char] = &[
    'a', 'Z', ' ', '1', '字', 'あ', 'क', '\u{301}', '\u{94D}', '\u{FE0F}', '\u{200D}', '\u{20E3}',
    '👍', '👩', '💻', '🔥', '❤', '🏻', '🏽', '🇯', '🇵', '\u{202E}',
];

proptest! {
    /// Under the cap and with a base first, the layout is the grid's.
    #[test]
    fn layout_matches_printing(
        chars in proptest::collection::vec(proptest::sample::select(ALPHABET), 1..24)
            .prop_filter("starts with a base", |cs| {
                char_cell_width(cs[0]) > 0 && !is_emoji_modifier(cs[0])
            }),
    ) {
        let text: String = chars.into_iter().collect();
        let expected = printed(&text);
        let got: Vec<(String, u8)> =
            laid_out(&text).into_iter().map(|(s, w)| (s.to_owned(), w)).collect();
        prop_assert_eq!(got, expected, "{:?}", text);
    }
}
