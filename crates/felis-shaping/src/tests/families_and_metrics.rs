use super::*;

#[test]
fn families_are_listed_in_case_blind_order() {
    let mut families = ["noto Sans", "Liberation Mono", "DejaVu Sans", "dejavu Sans"]
        .map(|name| InstalledFamily::new(name.to_owned()));
    sort_ignoring_case(&mut families);
    assert_eq!(
        families.map(|family| family.name),
        ["DejaVu Sans", "dejavu Sans", "Liberation Mono", "noto Sans"]
    );
}

#[test]
fn a_family_is_monospace_only_when_every_face_is() {
    let regular = FaceStyle {
        italic: false,
        weight: 400,
    };
    let mut family = InstalledFamily::new("Mixed".to_owned());
    family.add_face(true, regular);
    assert!(family.monospace);
    family.add_face(false, regular);
    assert!(!family.monospace);
    assert_eq!(family.styles, [regular]);
}

#[test]
fn from_scaled_ceils_ascent_and_descent_and_rounds_advance_and_leading() {
    // (case, (width, ascent, descent, leading), (width, height, ascent))
    let cases = [
        // DejaVu Sans Mono at 10 pt / 96 dpi: hhea ascender 1901, descender
        // -483, lineGap 0 at upem 2048 scales to 12.376 / 3.145 / 0, which
        // foot and kitty both size as a 17 px cell with the baseline at 13.
        (
            "fractional ascent and descent each ceil",
            (8.0, 12.376, 3.145, 0.0),
            (8, 17, 13),
        ),
        (
            "whole ascent and descent do not grow the cell",
            (8.0, 13.0, 4.0, 0.0),
            (8, 17, 13),
        ),
        (
            "leading below the half pixel rounds down",
            (8.0, 12.0, 4.0, 2.4),
            (8, 18, 12),
        ),
        (
            "leading past the half pixel rounds up",
            (8.0, 12.0, 4.0, 2.6),
            (8, 19, 12),
        ),
        // DejaVu Sans Mono's `M` at 14 pt / 96 dpi.
        (
            "11.24 px advance yields an 11 px cell",
            (11.24, 12.0, 4.0, 0.0),
            (11, 16, 12),
        ),
        // Menlo's `M` at 14 pt / 72 dpi.
        (
            "8.43 px advance yields an 8 px cell",
            (8.43, 12.0, 4.0, 0.0),
            (8, 16, 12),
        ),
        (
            "advance past the half pixel rounds up",
            (16.86, 12.0, 4.0, 0.0),
            (17, 16, 12),
        ),
        (
            "whole pixel advance keeps its width",
            (7.0, 12.0, 4.0, 0.0),
            (7, 16, 12),
        ),
        (
            "sub-half-pixel advance still yields one pixel",
            (0.3, 12.0, 4.0, 0.0),
            (1, 16, 12),
        ),
        (
            "degenerate metrics still yield a one pixel cell",
            (0.0, 0.0, 0.0, 0.0),
            (1, 1, 0),
        ),
    ];
    for (case, (width, ascent, descent, leading), expected) in cases {
        let metrics = CellMetrics::from_scaled(width, ascent, descent, leading);
        assert_eq!(
            (metrics.width, metrics.height, metrics.ascent),
            expected,
            "{case}"
        );
    }
}

#[test]
fn cell_scale_resizes_the_cell_and_centers_the_glyph_box_in_it() {
    let base = CellMetrics::unscaled(10, 20, 15);
    // (case, scale, (width, height, ascent), pad, trailing pad)
    let cases = [
        (
            "unit scale keeps the cell",
            [1.0, 1.0],
            (10, 20, 15),
            [0, 0],
            [0, 0],
        ),
        (
            "an odd growth puts the extra pixel below",
            [1.0, 1.15],
            (10, 23, 16),
            [0, 1],
            [0, 2],
        ),
        (
            "an even growth splits evenly and lowers the baseline",
            [1.2, 1.2],
            (12, 24, 17),
            [1, 2],
            [1, 2],
        ),
        (
            "a shrink pads negatively and raises the baseline",
            [0.5, 0.85],
            (5, 17, 13),
            [-3, -2],
            [-2, -1],
        ),
    ];
    for (case, scale, cell, pad, trailing) in cases {
        let m = base.with_cell_scale(scale);
        assert_eq!((m.width, m.height, m.ascent), cell, "{case}");
        assert_eq!(m.glyph_box, [10, 20], "{case}");
        assert_eq!(m.pad, pad, "{case}");
        assert_eq!(m.trailing_pad(), trailing, "{case}");
    }
}

#[test]
fn cell_scale_applies_to_the_font_cell_not_a_scaled_one() {
    let base = CellMetrics::unscaled(10, 20, 15);
    assert_eq!(
        base.with_cell_scale([1.5, 1.5]).with_cell_scale([1.2, 1.2]),
        base.with_cell_scale([1.2, 1.2]),
    );
}

/// `effective_scale` is `s × n/d`, or `s` alone without a
/// fractional part.
#[test]
fn sizing_key_effective_scale_honors_fractional_n_over_d() {
    assert!((SizingKey::new(1, 0, 1, 2, 0, 0).effective_scale() - 0.5).abs() < 1e-5);
    assert!((SizingKey::new(2, 0, 1, 2, 0, 0).effective_scale() - 1.0).abs() < 1e-5);
    assert!((SizingKey::new(1, 0, 0, 0, 0, 0).effective_scale() - 1.0).abs() < 1e-5);
    assert!((SizingKey::new(7, 0, 14, 15, 0, 0).effective_scale() - 6.533_333_3).abs() < 1e-5);
    assert!((SizingKey::default().effective_scale() - 1.0).abs() < 1e-5);
}

#[test]
fn font_style_index_roundtrips() {
    for i in 0..4 {
        assert_eq!(FontStyle::from_index(i).index(), i);
    }
    assert_eq!(FontStyle::REGULAR, FontStyle::default());
    assert_eq!(FontStyle::REGULAR.index(), 0);
}
