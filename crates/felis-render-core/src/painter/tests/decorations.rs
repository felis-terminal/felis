use super::*;

fn deco_for(sgr: &[u8]) -> Vec<Decoration> {
    let mut bytes = sgr.to_vec();
    bytes.push(b'A');
    let g = drive(1, 4, &bytes);
    let atlas = MockAtlas::full(slot());
    build_instances(g.screen(), metrics(), &Theme::default(), &atlas).decos
}

#[test]
fn single_underline_emits_one_solid_line_in_the_descender_band() {
    let deco = deco_for(b"\x1b[4m");
    assert_eq!(deco.len(), 1);
    let d = deco[0];
    assert_eq!(d.kind, DecoKind::Solid);
    assert_eq!(d.size_px[0], metrics().width as f32, "spans the cell width");
    assert_eq!(
        d.color,
        Theme::default().fg,
        "default underline color is the cell fg"
    );
    let ascent = metrics().ascent as f32;
    let ch = metrics().height as f32;
    assert!(
        d.origin_px[1] >= ascent && d.origin_px[1] <= ch - d.thickness_px,
        "underline sits in the descender band, got y={}",
        d.origin_px[1],
    );
}

#[test]
fn double_underline_emits_two_stacked_lines() {
    let deco = deco_for(b"\x1b[21m");
    assert_eq!(deco.len(), 2);
    assert!(deco.iter().all(|d| d.kind == DecoKind::Solid));
    assert_ne!(
        deco[0].origin_px[1], deco[1].origin_px[1],
        "two distinct rows"
    );
}

#[test]
fn curly_dotted_dashed_underlines_pick_the_procedural_kinds() {
    assert_eq!(deco_for(b"\x1b[4:3m")[0].kind, DecoKind::Curly);
    assert_eq!(deco_for(b"\x1b[4:4m")[0].kind, DecoKind::Dotted);
    assert_eq!(deco_for(b"\x1b[4:5m")[0].kind, DecoKind::Dashed);
    let curly = deco_for(b"\x1b[4:3m")[0];
    assert!(
        curly.size_px[1] > curly.thickness_px,
        "curly band must exceed the thickness so the wave fits",
    );
    let dotted = deco_for(b"\x1b[4:4m")[0];
    assert_eq!(
        dotted.size_px[1], dotted.thickness_px,
        "dotted stays one line tall"
    );
    assert!(
        dotted.period_px > 0.0,
        "a dotted pattern needs a non-zero period"
    );
}

#[test]
fn underline_color_uses_sgr_58_when_set() {
    let deco = deco_for(b"\x1b[4;58:2::255:0:0m");
    assert_eq!(deco.len(), 1);
    let want = crate::palette::srgb_to_linear_rgba([255, 0, 0], 1.0);
    assert_eq!(
        deco[0].color, want,
        "SGR 58 underline color must be honored"
    );
    assert_ne!(deco[0].color, Theme::default().fg);
}

#[test]
fn strikethrough_emits_a_mid_cell_line_in_the_fg_color() {
    let deco = deco_for(b"\x1b[9m");
    assert_eq!(deco.len(), 1);
    let d = deco[0];
    assert_eq!(d.kind, DecoKind::Solid);
    assert_eq!(d.color, Theme::default().fg);
    let ascent = metrics().ascent as f32;
    assert!(
        d.origin_px[1] < ascent,
        "strike crosses the glyph body, above the baseline"
    );
}

#[test]
fn overline_emits_a_line_at_the_cell_top() {
    let deco = deco_for(b"\x1b[53m");
    assert_eq!(deco.len(), 1);
    assert_eq!(deco[0].kind, DecoKind::Solid);
    assert_eq!(deco[0].origin_px[1], 0.0, "overline seats at the cell top");
    assert_eq!(deco[0].color, Theme::default().fg);
}

#[test]
fn faint_dims_the_glyph_toward_the_background() {
    let theme = Theme::default();
    let atlas = MockAtlas::full(slot());
    let plain = build_instances(drive(1, 4, b"A").screen(), metrics(), &theme, &atlas);
    let faint = build_instances(drive(1, 4, b"\x1b[2mA").screen(), metrics(), &theme, &atlas);
    assert_eq!(plain.glyphs.len(), 1);
    assert_eq!(faint.glyphs.len(), 1);
    assert_eq!(
        plain.glyphs[0].color, theme.fg,
        "baseline glyph paints the theme fg"
    );
    assert_eq!(
        faint.glyphs[0].color,
        dim_toward_bg(theme.fg, theme.bg),
        "faint glyph mixes the fg toward the bg",
    );
    assert_ne!(
        faint.glyphs[0].color, theme.fg,
        "faint must actually change the color"
    );
}

#[test]
fn conceal_hides_the_glyph_but_keeps_the_background() {
    let theme = Theme::default();
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(drive(1, 4, b"\x1b[8mA").screen(), metrics(), &theme, &atlas);
    assert!(
        buffers.glyphs.is_empty(),
        "concealed glyph must leave no ink"
    );
    assert!(
        !buffers.rects.is_empty(),
        "the cell background must still paint"
    );
}

#[test]
fn conceal_hides_the_glyph_yet_keeps_the_underline() {
    // Conceal suppresses the glyph, not the cell's decorations.
    let deco = deco_for(b"\x1b[4;8m");
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(
        drive(1, 4, b"\x1b[4;8mA").screen(),
        metrics(),
        &Theme::default(),
        &atlas,
    );
    assert!(buffers.glyphs.is_empty(), "conceal still hides the glyph");
    assert_eq!(deco.len(), 1, "the underline survives conceal");
}

#[test]
fn a_link_cell_yields_the_underline_slot_but_keeps_strikethrough() {
    use std::num::NonZeroU16;
    let mut g = Grid::new(1, 4);
    let style = g.style_table_mut().intern(felis_grid::Attributes {
        flags: AttrFlags::UNDERLINE | AttrFlags::STRIKETHROUGH,
        ..felis_grid::Attributes::default()
    });
    g.set_cell(
        0,
        0,
        Cell {
            grapheme: Grapheme::Ascii(b'A'),
            style,
            link: NonZeroU16::new(1),
            sizing: None,
        },
    );
    let atlas = MockAtlas::full(slot());
    let deco = build_instances(g.screen(), metrics(), &Theme::default(), &atlas).decos;
    assert_eq!(
        deco.len(),
        1,
        "link suppresses the SGR underline but not the strike"
    );
    assert!(
        deco[0].origin_px[1] < metrics().ascent as f32,
        "the surviving decoration is the strikethrough",
    );
}

#[test]
fn a_bidi_override_cell_suppresses_every_decoration() {
    let mut g = Grid::new(1, 4);
    let style = g.style_table_mut().intern(felis_grid::Attributes {
        flags: AttrFlags::UNDERLINE | AttrFlags::STRIKETHROUGH | AttrFlags::OVERLINE,
        ..felis_grid::Attributes::default()
    });
    g.set_cell(
        0,
        0,
        Cell {
            grapheme: Grapheme::Char('\u{202e}'), // RIGHT-TO-LEFT OVERRIDE
            style,
            link: None,
            sizing: None,
        },
    );
    let atlas = MockAtlas::full(slot());
    let deco = build_instances(g.screen(), metrics(), &Theme::default(), &atlas).decos;
    assert!(
        deco.is_empty(),
        "a bidi-override cell drops every decoration"
    );
}
