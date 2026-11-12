use super::*;
use crate::test_support::{cluster_fit_px_at, walk_pinned};

fn slot_after_ensure(idx: &mut GlyphIndex, c: char, sizing: SizingKey) -> GlyphSlot {
    let h = idx.cell_metrics().height;
    idx.ensure(c, h, sizing);
    idx.slot(c, h, sizing).expect("placed")
}

#[test]
fn a_grown_cell_shifts_font_glyphs_by_the_left_pad() {
    let plain = slot_after_ensure(
        &mut GlyphIndex::new(stack(), 14.0, nz(256)),
        'M',
        SizingKey::default(),
    );
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256)).with_cell_scale([1.5, 1.5]);
    let pad = idx.cell_metrics().pad;
    assert!(pad[0] > 0 && pad[1] > 0);
    let grown = slot_after_ensure(&mut idx, 'M', SizingKey::default());
    assert!(!grown.fills_cell);
    assert_eq!(grown.offset_px[0], plain.offset_px[0] + pad[0]);
    assert_eq!(
        grown.offset_px[1], plain.offset_px[1],
        "the baseline moves, the bearing does not"
    );
    assert_eq!(grown.size_px, plain.size_px);
    let s2 = SizingKey::new(2, 0, 0, 0, 0, 0);
    let plain_s2 = slot_after_ensure(&mut GlyphIndex::new(stack(), 14.0, nz(256)), 'M', s2);
    let grown_s2 = slot_after_ensure(&mut idx, 'M', s2);
    assert_eq!(grown_s2.offset_px[0], plain_s2.offset_px[0] + 2 * pad[0]);
}

#[test]
fn box_drawing_fills_the_scaled_cell_without_a_pad() {
    for scale in [[1.5, 1.3], [0.6, 0.7]] {
        let mut idx = GlyphIndex::new(stack(), 14.0, nz(256)).with_cell_scale(scale);
        let m = idx.cell_metrics();
        let slot = slot_after_ensure(&mut idx, '█', SizingKey::default());
        assert!(slot.fills_cell, "{scale:?}");
        assert_eq!(slot.size_px, [m.width, m.height], "{scale:?}");
        assert_eq!(slot.offset_px[0], 0, "{scale:?}");
    }
}

#[test]
fn a_shrunk_cell_never_fits_primary_ascii() {
    let font = Arc::new(Font::load_test_font());
    let mut idx =
        GlyphIndex::new(FontStack::new(font.clone()), 14.0, nz(512)).with_cell_scale([0.5, 0.5]);
    for c in '!'..='~' {
        assert!(
            idx.fitted_bitmap(&font, c, SizingKey::default()).is_none(),
            "{c:?}"
        );
    }
}

#[test]
fn fractional_scale_rasterizes_below_default_for_sub_cell_shrink() {
    // `s=1, n=1, d=2` is effective scale 0.5, so the bitmap must be
    // smaller than the default.
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(512));
    let h = idx.cell_metrics().height;
    let default_sk = SizingKey::default();
    let frac_half_sk = SizingKey::new(1, 0, 1, 2, 0, 0);
    let scale_2_sk = SizingKey::new(2, 0, 0, 0, 0, 0);
    assert!(idx.ensure('A', h, default_sk));
    assert!(idx.ensure('A', h, frac_half_sk));
    assert!(idx.ensure('A', h, scale_2_sk));
    let small = idx.slot('A', h, frac_half_sk).expect("fractional slot");
    let mid = idx.slot('A', h, default_sk).expect("default slot");
    let large = idx.slot('A', h, scale_2_sk).expect("integer scale=2 slot");
    // Hinted metrics are not strictly proportional; compare
    // the ordering, not exact ratios.
    assert!(
        small.size_px[1] < mid.size_px[1],
        "0.5× bitmap height ({}) must be below default ({})",
        small.size_px[1],
        mid.size_px[1],
    );
    assert!(
        large.size_px[1] > mid.size_px[1],
        "2× bitmap height ({}) must exceed default ({})",
        large.size_px[1],
        mid.size_px[1],
    );
}

#[test]
fn primary_face_ascii_is_never_fitted_at_any_osc66_scale() {
    let font = Arc::new(Font::load_test_font());
    for px in [13.0, 18.67, 24.0, 29.33] {
        let mut idx = GlyphIndex::new(FontStack::new(font.clone()), px, nz(512));
        for s in 1..=7 {
            let sk = SizingKey::new(s, 0, 0, 0, 0, 0);
            for c in '!'..='~' {
                assert!(
                    idx.fitted_bitmap(&font, c, sk).is_none(),
                    "{c:?} at {px} px, s={s}"
                );
            }
        }
    }
}

/// Width-1 chars the pinned emoji and symbol faces draw wider than a
/// Monaspace cell.
const OVERWIDE: [char; 6] = [
    '\u{263A}',
    '\u{2764}',
    '\u{2654}',
    '\u{26F5}',
    '\u{2B50}',
    '\u{1F30D}',
];

/// The ink fills the block along its binding axis and is centred on
/// both, so a fitted glyph reads as large as the cell allows.
#[test]
fn a_fallback_glyph_wider_than_its_cell_fills_and_centres_in_it() {
    let stack = FontStack::pinned_test_stack();
    let mut idx = GlyphIndex::new(stack, 18.0, nz(1024));
    let m = idx.cell_metrics();
    let (cw, ch, ascent) = (i64::from(m.width), i64::from(m.height), i64::from(m.ascent));
    let mut fitted = 0;
    for c in OVERWIDE
        .into_iter()
        .filter(|&c| felis_grid::char_cell_width(c) == 1)
    {
        let font = idx.stack().resolve(c, FontStyle::REGULAR).clone();
        let Some(bitmap) = idx.fitted_bitmap(&font, c, SizingKey::default()) else {
            continue;
        };
        fitted += 1;
        let (w, h) = (i64::from(bitmap.width()), i64::from(bitmap.height()));
        let (left, top) = (i64::from(bitmap.left), i64::from(bitmap.top));
        let right = cw - left - w;
        let (above, below) = (ascent - top, ch - (ascent - top) - h);
        assert!(
            left >= 0 && right >= -1,
            "{c:?} inside the cell: left {left}, right {right}"
        );
        assert!(
            above >= -1 && below >= -1,
            "{c:?} inside the cell: above {above}, below {below}"
        );
        assert!(
            (left - right).abs() <= 1,
            "{c:?} centred: left {left}, right {right}"
        );
        assert!(
            (above - below).abs() <= 1,
            "{c:?} centred: above {above}, below {below}"
        );
        assert!(
            w >= cw - 2 || h >= ch - 2,
            "{c:?} fills an axis: {w}x{h} in {cw}x{ch}"
        );
    }
    assert!(
        fitted > 0,
        "no candidate overruns its cell in the pinned set"
    );
}

/// An OSC 66 block is `w` (or the char's width) cells at the integer
/// `s`, so a glyph between one and two cells wide fits `w=2` and a
/// halved glyph in an `s=2` block, and is fitted only for `w=1`.
#[test]
fn osc66_width_and_fractional_scale_set_the_fitted_block() {
    let stack = FontStack::pinned_test_stack();
    let px = 18.0;
    let mut idx = GlyphIndex::new(stack, px, nz(1024));
    let m = idx.cell_metrics();
    let cw = m.width as f32;
    let (c, font) = OVERWIDE
        .into_iter()
        .filter(|&c| felis_grid::char_cell_width(c) == 1)
        .map(|c| (c, idx.stack().resolve(c, FontStyle::REGULAR).clone()))
        .find(|(c, font)| {
            font.advance_px(*c, px)
                .is_some_and(|a| a > cw + 1.0 && a < 2.0 * cw)
        })
        .expect("the pinned set has a width-1 glyph between one and two cells wide");
    let w1 = SizingKey::new(1, 1, 0, 0, 0, 0);
    let w2 = SizingKey::new(1, 2, 0, 0, 0, 0);
    let half_in_s2 = SizingKey::new(2, 0, 1, 2, 0, 0);
    assert!(idx.fitted_bitmap(&font, c, w1).is_some(), "w=1 fits {c:?}");
    assert!(idx.fitted_bitmap(&font, c, w2).is_none(), "w=2 holds {c:?}");
    assert!(
        idx.fitted_bitmap(&font, c, half_in_s2).is_none(),
        "s=2 n=1 d=2 holds {c:?}"
    );
}

/// A star the symbol faces cover is still drawn in color, as an
/// emoji-presentation scalar.
#[test]
fn an_emoji_presentation_char_rasterizes_in_color() {
    let stack = FontStack::pinned_test_stack();
    let mut idx = GlyphIndex::new(stack, 18.0, nz(1024));
    let h = idx.cell_metrics().height;
    idx.ensure('\u{2B50}', h, SizingKey::default());
    let slot = idx
        .slot('\u{2B50}', h, SizingKey::default())
        .expect("a star slot");
    assert!(slot.is_color);
}

/// A one-row grid fed `bytes` and walked against the pinned set.
/// Clusters whose advance stays within their cells keep the native
/// path: a ligated flag, a widened VS16 emoji, a keycap, a combining
/// mark, and one the face attaches at its anchor with no precomposed
/// glyph.
#[test]
fn a_cluster_within_its_cells_is_not_fitted() {
    for text in [
        "\u{1F1EF}\u{1F1F5}",
        "\u{2764}\u{FE0F}",
        "#\u{FE0F}\u{20E3}",
        "e\u{301}",
        "q\u{301}",
    ] {
        let p = walk_pinned(text.as_bytes(), 4);
        assert_eq!(cluster_fit_px_at(&p, 0), 0, "{text:?}");
    }
}

/// The fit is measured once per cluster: a later walk reads the memo,
/// so a repainted prompt does not rasterize its clusters again.
#[test]
fn a_fitted_cluster_is_measured_once() {
    let mut p = walk_pinned("\u{1F408}\u{200D}\u{1F408}".as_bytes(), 4);
    assert_eq!(p.index.cluster_fits.len(), 1);
    let first = cluster_fit_px_at(&p, 0);
    p.index.cluster_fits.values_mut().for_each(|px| *px += 1);
    let mut frame = ShapeFrame::empty();
    p.index.walked = None;
    GridWalker::default().walk(
        &mut p.index,
        p.grid.screen(),
        &mut Shaper::new(),
        &[],
        &mut frame,
    );
    p.frame = frame;
    assert_eq!(
        cluster_fit_px_at(&p, 0),
        first + 1,
        "the walk read the memo"
    );
}

/// A font-supplied `│` lands a glyph-sized bitmap shorter than the
/// cell, so `size_px[1] == cell_height` only holds on the synthetic
/// path.
#[test]
fn vertical_box_drawing_bitmap_exactly_fills_the_cell() {
    let mut idx = GlyphIndex::new(stack(), 14.0, nz(256));
    let metrics = idx.cell_metrics();
    let sk = SizingKey::default();
    assert!(idx.ensure('│', metrics.height, sk));
    let slot = idx.slot('│', metrics.height, sk).expect("│ slot");
    assert_eq!(
        slot.size_px,
        [metrics.width, metrics.height],
        "synthetic │ must fill the cell so adjacent rows connect",
    );
    let ascent_i32 = i32::try_from(metrics.ascent).expect("ascent fits in i32");
    assert_eq!(
        slot.offset_px,
        [0, ascent_i32],
        "left=0 + top=ascent pins the bitmap at the cell's top-left",
    );
}
