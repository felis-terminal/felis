use super::*;

#[test]
fn link_cells_emit_an_extra_underline_instance() {
    use crate::palette::LINK_UNDERLINE;
    let g = drive(
        1,
        4,
        b"\x1b[?25l\x1b]8;;https://example.com\x07ab\x1b]8;;\x07cd",
    );
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(
        buffers.rects.len(),
        6,
        "two link cells each add an underline"
    );
    assert_eq!(buffers.rects[1].color, LINK_UNDERLINE);
    assert_eq!(buffers.rects[1].size_px[1], LINK_UNDERLINE_PX);
    assert_eq!(buffers.rects[3].color, LINK_UNDERLINE);
    assert_eq!(buffers.rects[4].color, Theme::default().bg);
    assert_eq!(buffers.rects[5].color, Theme::default().bg);
}

/// REQ-910: a `javascript:` OSC 8 emits no `LINK_UNDERLINE`.
#[test]
fn disallowed_scheme_emits_no_link_underline_instance() {
    use crate::palette::LINK_UNDERLINE;
    let g = drive(
        1,
        6,
        b"\x1b[?25l\x1b]8;;javascript:alert(1)\x07evil\x1b]8;;\x07ok",
    );
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert!(
        buffers.rects.iter().all(|b| b.color != LINK_UNDERLINE),
        "javascript: scheme leaked past dispatch_osc_8 and produced a link underline",
    );
}

/// `https://good` then `javascript:bad`: the bad open clears the pen
/// rather than inheriting the safe URI.
#[test]
fn disallowed_scheme_after_safe_open_clears_underline_run() {
    use crate::palette::LINK_UNDERLINE;
    let g = drive(
        1,
        8,
        b"\x1b[?25l\x1b]8;;https://good\x07safe\x1b]8;;javascript:bad\x07evil\x1b]8;;\x07",
    );
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    let underline_count = buffers
        .rects
        .iter()
        .filter(|b| b.color == LINK_UNDERLINE)
        .count();
    assert_eq!(
        underline_count, 4,
        "underline run leaked past the javascript: pen-clear into `evil`",
    );
}

#[test]
fn link_underline_skipped_on_bidi_marker_cells() {
    // The bidi marker drops the link underline so the warning stays
    // unobstructed.
    use crate::palette::{BIDI_MARKER_BG, LINK_UNDERLINE};
    let g = drive(
        1,
        3,
        "\x1b[?25l\x1b]8;;https://example.test/\u{0007}A\u{202E}\x1b]8;;\u{0007}".as_bytes(),
    );
    let atlas = MockAtlas::full(slot());
    let buffers = build_instances(g.screen(), metrics(), &Theme::default(), &atlas);
    assert_eq!(buffers.rects[0].color, BIDI_MARKER_BG);
    assert!(
        buffers.rects.iter().all(|b| b.color != LINK_UNDERLINE),
        "link underline must not paint over bidi marker",
    );
}
