//! How a cell looks: its colors after reverse video, the cursor, the
//! bidi warning, selection and faint, and which decorations it carries.
//! Resolved per row of cells, so a client holding only decoded
//! scrollback rows resolves them the same way the cell painter does.

use felis_grid::{
    AttrFlags, Cell, ClusterTable, Color, CursorStyle, Grapheme, ScreenBuffer, StyleTable,
    UnderlineStyle, bidi,
};

use crate::{
    SelectionRange,
    palette::{
        BIDI_MARKER_BG, BIDI_MARKER_FG, LINK_UNDERLINE, ResolvedTheme, SELECTION_BG, resolve_bg,
        resolve_fg,
    },
};

/// Columns in `cells.len()..cols` read as [`Cell::BLANK`].
#[derive(Debug, Clone, Copy)]
pub struct RowCells<'a> {
    pub cells: &'a [Cell],
    pub cols: u16,
    pub styles: &'a StyleTable,
    pub clusters: &'a ClusterTable,
}

impl<'a> RowCells<'a> {
    /// The row's content up to its occupancy watermark, so the stale
    /// tail past it reads blank as [`ScreenBuffer::cell`] does.
    #[must_use]
    pub fn of_screen(screen: &'a ScreenBuffer, row: u16) -> Option<Self> {
        Some(Self {
            cells: screen.row_content(row)?,
            cols: screen.cols(),
            styles: screen.style_table(),
            clusters: screen.cluster_table(),
        })
    }

    #[must_use]
    pub fn cell(&self, col: u16) -> &'a Cell {
        self.cells.get(usize::from(col)).unwrap_or(&Cell::BLANK)
    }

    /// [`ScreenBuffer::char_span`] over this row.
    #[must_use]
    pub fn char_span(&self, col: u16) -> (u16, u16) {
        let is_spacer = |c: u16| c < self.cols && matches!(self.cell(c).grapheme, Grapheme::Spacer);
        if col > 0 && is_spacer(col) {
            (col - 1, col)
        } else if is_spacer(col.saturating_add(1)) {
            (col, col + 1)
        } else {
            (col, col)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreeditSpan {
    pub row: u16,
    pub col_start: u16,
    /// One past the last column the composition covers.
    pub col_end: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CursorPaint {
    cell: Option<(u16, u16)>,
    last_col: u16,
    style: CursorStyle,
}

impl CursorPaint {
    /// The row and first column of the character under the cursor.
    #[must_use]
    pub const fn cell(&self) -> Option<(u16, u16)> {
        self.cell
    }
}

/// The frame-wide inputs every row of a screen reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameState {
    pub cursor: CursorPaint,
    pub selection: Option<SelectionRange>,
    /// `?5` DECSCNM, xor'd against each cell's SGR 7.
    pub screen_reverse: bool,
    pub preedit: Option<PreeditSpan>,
    pub reserved_bottom_rows: u16,
    pub rows: u16,
}

impl FrameState {
    /// `pending_wrap` can bump the cursor column past the right edge for
    /// one print; the in-bounds check hides it then. Focus loss hides
    /// the cursor, and so does `viewport > 0` (Kitty / Alacritty's
    /// cursor-in-scrollback behavior): the live row has scrolled off the
    /// visible window.
    #[must_use]
    pub fn for_screen(
        screen: &ScreenBuffer,
        cursor_visible: bool,
        viewport: u32,
        selection: Option<SelectionRange>,
        screen_reverse: bool,
        preedit: Option<PreeditSpan>,
        reserved_bottom_rows: u16,
    ) -> Self {
        let cursor = screen.cursor();
        let in_bounds = cursor.row < screen.rows() && cursor.col < screen.cols();
        let (first, last) = screen.char_span(cursor.row, cursor.col);
        Self {
            cursor: CursorPaint {
                cell: (cursor.visible && in_bounds && cursor_visible && viewport == 0)
                    .then_some((cursor.row, first)),
                last_col: last,
                style: screen.cursor_style(),
            },
            selection,
            screen_reverse,
            preedit,
            reserved_bottom_rows,
            rows: screen.rows(),
        }
    }

    #[must_use]
    pub fn row(&self, row: u16, cells: &RowCells<'_>) -> RowFrame {
        RowFrame {
            cursor: match self.cursor.cell {
                Some((r, first)) if r == row => Some(RowCursor {
                    first,
                    last: self.cursor.last_col,
                    style: self.cursor.style,
                }),
                _ => None,
            },
            selection: selection_cols(self.selection, row, cells.cols),
            preedit: self
                .preedit
                .filter(|span| span.row == row)
                .map(|span| (span.col_start, span.col_end)),
            screen_reverse: self.screen_reverse,
            painted: row < self.rows.saturating_sub(self.reserved_bottom_rows),
        }
    }
}

/// One row's share of a [`FrameState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowFrame {
    pub cursor: Option<RowCursor>,
    /// Inclusive columns.
    pub selection: Option<(u16, u16)>,
    /// `[start, end)` columns.
    pub preedit: Option<(u16, u16)>,
    pub screen_reverse: bool,
    /// `false` for a row under the reserved bottom rows.
    pub painted: bool,
}

impl RowFrame {
    /// A row no frame overlay touches, such as a scrollback row.
    #[must_use]
    pub const fn plain(screen_reverse: bool) -> Self {
        Self {
            cursor: None,
            selection: None,
            preedit: None,
            screen_reverse,
            painted: true,
        }
    }
}

/// The columns of the character under the cursor, both halves of a
/// wide one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowCursor {
    pub first: u16,
    pub last: u16,
    pub style: CursorStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Paint,
    /// Under the pre-edit overlay, or on an unpainted row: the overlay
    /// is opaque, but a cell's glyph would draw through it in the later
    /// fg pass.
    Skip,
}

/// A `Block` cursor folds into the cell's fg and bg instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMarkerKind {
    Underline,
    Bar,
}

/// Colors are linear RGBA and are resolved under either disposition: a
/// glyph inking across cells takes each neighbor's fg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellAppearance {
    pub disposition: Disposition,
    pub fg: [f32; 4],
    pub bg: [f32; 4],
    /// `bg` is the theme background with no cell, cursor, selection or
    /// warning color over it.
    pub bg_is_ground: bool,
    /// CONCEAL (SGR 8) hides the glyph, and so does a wide character
    /// half under the pre-edit overlay, whose glyph inks across both
    /// halves. SECURITY: this must gate every fg draw.
    pub glyph_visible: bool,
    pub bidi_warning: bool,
    pub cursor_marker: Option<(CursorMarkerKind, [f32; 4])>,
    pub link_underline: Option<[f32; 4]>,
    pub underline: Option<(UnderlineStyle, [f32; 4])>,
    pub strikethrough: Option<[f32; 4]>,
    pub overline: Option<[f32; 4]>,
}

#[must_use]
pub fn resolve_cell(
    row: &RowCells<'_>,
    frame: &RowFrame,
    theme: &ResolvedTheme,
    col: u16,
) -> CellAppearance {
    let cell = row.cell(col);
    let attrs = row.styles.resolve(cell.style);
    let disposition = match frame.preedit {
        _ if !frame.painted => Disposition::Skip,
        Some((start, end)) if col >= start && col < end => Disposition::Skip,
        _ => Disposition::Paint,
    };
    let glyph_under_preedit = frame.preedit.is_some_and(|(start, end)| {
        let (first, last) = row.char_span(col);
        last >= start && first < end
    });
    // A wide character is painted as one: its Spacer takes the
    // lead's bidi warning, and the cursor or a selection on either
    // half covers both.
    let is_bidi = match cell.grapheme {
        Grapheme::Spacer if col > 0 => {
            cell_contains_bidi_override(row.cell(col - 1).grapheme, row.clusters)
        }
        g => cell_contains_bidi_override(g, row.clusters),
    };
    let (mut fg, mut bg) =
        resolve_pair(attrs.fg, attrs.bg, theme, attrs.flags, frame.screen_reverse);
    let cursor = frame
        .cursor
        .filter(|cursor| (cursor.first..=cursor.last).contains(&col));
    let mut cursor_marker =
        cursor.and_then(|cursor| apply_cursor_style(cursor, col, theme, &mut fg, &mut bg));
    let mut selected = false;
    // After the cursor swap so the security signal wins on the
    // cursor cell too (CVE-2021-42574).
    if is_bidi {
        bg = BIDI_MARKER_BG;
        fg = BIDI_MARKER_FG;
        // The marker would obstruct the warning.
        cursor_marker = None;
    } else if let Some((lo, hi)) = frame.selection {
        let (first, last) = row.char_span(col);
        selected = (lo..=hi).contains(&first) || (lo..=hi).contains(&last);
        if selected {
            bg = SELECTION_BG;
        }
    }
    // After cursor / bidi / selection so it dims the effective fg,
    // and never the bidi warning.
    let faint = attrs.flags.contains(AttrFlags::FAINT) && !is_bidi;
    if faint {
        fg = dim_toward_bg(fg, bg);
    }
    let block_cursor = cursor.is_some_and(|cursor| cursor.style == CursorStyle::Block);
    // The decoration pass draws after the glyph pass, so precedence
    // against the bg-pass overlays is enforced by suppression, not
    // z-order: a bidi cell drops every decoration, and the SGR
    // underline yields its slot to the link underline and the cursor
    // marker.
    let decorated = !is_bidi;
    let has_link = cell.link.is_some();
    CellAppearance {
        disposition,
        fg,
        bg,
        bg_is_ground: matches!(attrs.bg, Color::Default)
            && attrs.flags.contains(AttrFlags::REVERSE) == frame.screen_reverse
            && !block_cursor
            && !is_bidi
            && !selected,
        glyph_visible: !attrs.flags.contains(AttrFlags::CONCEAL) && !glyph_under_preedit,
        bidi_warning: is_bidi,
        cursor_marker,
        link_underline: (decorated && has_link).then_some(LINK_UNDERLINE),
        underline: (decorated
            && attrs.flags.contains(AttrFlags::UNDERLINE)
            && !has_link
            && cursor.is_none())
        .then(|| {
            (
                attrs.underline_style,
                underline_deco_color(attrs.underline_color, fg, bg, theme, faint),
            )
        }),
        strikethrough: (decorated && attrs.flags.contains(AttrFlags::STRIKETHROUGH)).then_some(fg),
        overline: (decorated && attrs.flags.contains(AttrFlags::OVERLINE)).then_some(fg),
    }
}

/// Replaces `out` with columns `0..row.cols`.
pub fn resolve_row(
    row: &RowCells<'_>,
    frame: &RowFrame,
    theme: &ResolvedTheme,
    out: &mut Vec<CellAppearance>,
) {
    out.clear();
    out.extend((0..row.cols).map(|col| resolve_cell(row, frame, theme, col)));
}

fn apply_cursor_style(
    cursor: RowCursor,
    col: u16,
    theme: &ResolvedTheme,
    fg_color: &mut [f32; 4],
    bg_color: &mut [f32; 4],
) -> Option<(CursorMarkerKind, [f32; 4])> {
    let original_bg = *bg_color;
    let cursor_color = theme.cursor.unwrap_or(*fg_color);
    match cursor.style {
        CursorStyle::Block => {
            // With `theme.cursor`: bg = cursor color, glyph = original
            // cell bg for contrast. Without: reverse-video swap.
            if theme.cursor.is_some() {
                *bg_color = cursor_color;
                *fg_color = original_bg;
            } else {
                std::mem::swap(fg_color, bg_color);
            }
            None
        }
        CursorStyle::Underline => Some((CursorMarkerKind::Underline, cursor_color)),
        CursorStyle::Bar if cursor.first != col => None,
        CursorStyle::Bar => Some((CursorMarkerKind::Bar, cursor_color)),
    }
}

/// The selected columns of `row`, inclusive and clipped to `cols`. Mirrors
/// `Selection::contains` in `felis-client-core` but stays local so this
/// crate takes no UI-state dep (docs/explanation/architecture/overview.md
/// "Workspace: the crate-boundary decision record").
const fn selection_cols(
    selection: Option<SelectionRange>,
    row: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    let Some(sel) = selection else {
        return None;
    };
    let (sr, sc) = sel.start;
    let (er, ec) = sel.end;
    if row < sr || row > er {
        return None;
    }
    let lo = if sel.rectangle || row == sr { sc } else { 0 };
    let hi = if sel.rectangle || row == er {
        ec
    } else {
        u16::MAX
    };
    let last = cols.saturating_sub(1);
    let hi = if hi < last { hi } else { last };
    if lo > hi { None } else { Some((lo, hi)) }
}

/// The nine UAX #9 explicit-formatting characters. Cluster cells walk
/// every codepoint so a base plus combining RLO surfaces.
pub(crate) fn cell_contains_bidi_override(grapheme: Grapheme, clusters: &ClusterTable) -> bool {
    match grapheme {
        Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer | Grapheme::Ascii(_) => false,
        Grapheme::Char(c) => bidi::is_override(c),
        // (docs/explanation/data-model/grid-and-cells.md "Cluster
        // interning"); an unresolved handle carries no override.
        Grapheme::Cluster(id) => clusters
            .get(id)
            .is_some_and(|s| bidi::iter_contains_override(s.chars())),
    }
}

/// The XOR makes `?5` compose with SGR 7 the way every modern terminal
/// treats it. Both halves swap the *resolved* colors, so an indexed or
/// direct-RGB pair swaps too.
fn resolve_pair(
    fg: Color,
    bg: Color,
    theme: &ResolvedTheme,
    flags: AttrFlags,
    screen_reverse: bool,
) -> ([f32; 4], [f32; 4]) {
    if flags.contains(AttrFlags::REVERSE) == screen_reverse {
        (resolve_fg(fg, theme), resolve_bg(bg, theme))
    } else {
        (resolve_bg(bg, theme), resolve_fg(fg, theme))
    }
}

/// A 50/50 mix roughly halves perceived brightness on a dark theme,
/// matching kitty's dim intensity.
const FAINT_FG_WEIGHT: f32 = 0.5;

/// Mixes toward the *cell* bg, not black, so a dimmed glyph stays
/// legible on a light or colored background; alpha is untouched.
pub(crate) fn dim_toward_bg(fg: [f32; 4], bg: [f32; 4]) -> [f32; 4] {
    let w = FAINT_FG_WEIGHT;
    [
        fg[0].mul_add(w, bg[0] * (1.0 - w)),
        fg[1].mul_add(w, bg[1] * (1.0 - w)),
        fg[2].mul_add(w, bg[2] * (1.0 - w)),
        fg[3],
    ]
}

/// An explicit SGR 58 color is dimmed too when the cell is faint so the
/// whole cell reads at one intensity.
fn underline_deco_color(
    underline_color: Color,
    fg_color: [f32; 4],
    bg_color: [f32; 4],
    theme: &ResolvedTheme,
    faint: bool,
) -> [f32; 4] {
    match underline_color {
        Color::Default => fg_color,
        other => {
            let base = resolve_fg(other, theme);
            if faint {
                dim_toward_bg(base, bg_color)
            } else {
                base
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::unwrap_used)]

    use felis_grid::{Grid, RowEncode, decode_row, encode_row};
    use felis_vt::Parser;
    use proptest::prelude::*;

    use super::*;
    use crate::palette::Theme;

    fn drive(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
        let mut g = Grid::new(rows, cols);
        Parser::new().advance(&mut g, bytes);
        g
    }

    fn theme() -> ResolvedTheme {
        ResolvedTheme::new(&Theme::default())
    }

    fn live_frame(g: &Grid, row: u16) -> RowFrame {
        let state = FrameState::for_screen(g.screen(), true, 0, None, false, None, 0);
        state.row(row, &RowCells::of_screen(g.screen(), row).unwrap())
    }

    fn resolve_at(g: &Grid, frame: &RowFrame, row: u16, col: u16) -> CellAppearance {
        resolve_cell(
            &RowCells::of_screen(g.screen(), row).unwrap(),
            frame,
            &theme(),
            col,
        )
    }

    #[test]
    fn selection_cols_is_none_on_every_row_when_nothing_is_selected() {
        for row in 0..4u16 {
            assert_eq!(selection_cols(None, row, 4), None);
        }
    }

    proptest! {
        /// `selection_cols` mirrors
        /// `felis_client_core::Selection::contains`; the crates cannot
        /// depend on each other, so each is pinned against the same
        /// stated predicates.
        #[test]
        fn selection_cols_matches_its_stated_ranges(
            rectangle: bool,
            ar in 0u16..30,
            ac in 0u16..30,
            er in 0u16..30,
            ec in 0u16..30,
            r  in 0u16..30,
            c  in 0u16..30,
        ) {
            // `SelectionRange` arrives normalized (client-core's
            // `Selection::range` does it).
            let (start, end) = if rectangle {
                ((ar.min(er), ac.min(ec)), (ar.max(er), ac.max(ec)))
            } else if (ar, ac) <= (er, ec) {
                ((ar, ac), (er, ec))
            } else {
                ((er, ec), (ar, ac))
            };
            let range = SelectionRange { start, end, rectangle };

            let want = if rectangle {
                r >= start.0 && r <= end.0 && c >= start.1 && c <= end.1
            } else {
                let scalar = |(r, c): (u16, u16)| u32::from(r) * 1_000 + u32::from(c);
                scalar(start) <= scalar((r, c)) && scalar((r, c)) <= scalar(end)
            };
            let got = selection_cols(Some(range), r, 30).is_some_and(|(lo, hi)| lo <= c && c <= hi);
            prop_assert_eq!(got, want);
        }
    }

    const TOKENS: &[&str] = &[
        "a",
        "b",
        " ",
        "漢",
        "\u{202E}",
        "e\u{301}",
        "\x1b[7m",
        "\x1b[2m",
        "\x1b[8m",
        "\x1b[9m",
        "\x1b[53m",
        "\x1b[4:3m",
        "\x1b[58;5;3m",
        "\x1b[38;2;1;2;3m",
        "\x1b[48;5;9m",
        "\x1b[m",
        "\x1b]8;;https://example.com\x1b\\",
        "\x1b]8;;\x1b\\",
        "\r\n",
        "\x1b[K",
        "\x1b[2K",
        "\x1b[H",
    ];

    proptest! {
        /// A scrollback row reaches a client only as a packed row decoded
        /// into the client's own style table; it must resolve exactly as
        /// the live row it was encoded from.
        #[test]
        fn a_decoded_row_resolves_like_the_live_row_it_came_from(
            input in proptest::collection::vec(proptest::sample::select(TOKENS), 0..80),
            screen_reverse: bool,
            cursor_visible: bool,
            sel in (0u16..4, 0u16..12, 0u16..4, 0u16..12, any::<bool>()),
        ) {
            let g = drive(4, 12, input.concat().as_bytes());
            let screen = g.screen();
            let (sr, sc, er, ec, rectangle) = sel;
            let selection = Some(SelectionRange {
                start: (sr.min(er), sc.min(ec)),
                end: (sr.max(er), sc.max(ec)),
                rectangle,
            });
            let state = FrameState::for_screen(
                screen, cursor_visible, 0, selection, screen_reverse, None, 0,
            );
            let theme = theme();
            let (mut want, mut got) = (Vec::new(), Vec::new());
            for r in 0..screen.rows() {
                let live = RowCells::of_screen(screen, r).unwrap();
                let bytes = encode_row(
                    RowEncode {
                        cells: live.cells,
                        pad_to: usize::from(screen.cols()),
                        sized_cells: &[],
                        soft_wrap_continued: false,
                    },
                    screen.style_table(),
                )
                .unwrap();
                let mut styles = StyleTable::new();
                let decoded = decode_row(&bytes, &mut styles).unwrap();
                let scrollback = RowCells {
                    cells: &decoded.cells,
                    cols: screen.cols(),
                    styles: &styles,
                    clusters: screen.cluster_table(),
                };
                for frame in [RowFrame::plain(screen_reverse), state.row(r, &live)] {
                    resolve_row(&live, &frame, &theme, &mut want);
                    resolve_row(&scrollback, &frame, &theme, &mut got);
                    prop_assert_eq!(&got, &want, "row {}", r);
                }
            }
        }
    }

    #[test]
    fn bidi_warning_drops_every_marker_and_decoration_and_wins_over_a_block_cursor() {
        for style in ["2", "4", "6"] {
            let input = format!(
                "\x1b]8;;https://example.com\x1b\\\x1b[4;9;53mA\u{202E}\x1b[m\x1b]8;;\x1b\\\x1b[1;1H\x1b[{style} q"
            );
            let g = drive(1, 4, input.as_bytes());
            let look = resolve_at(&g, &live_frame(&g, 0), 0, 0);
            assert!(look.bidi_warning, "style {style}");
            assert_eq!((look.fg, look.bg), (BIDI_MARKER_FG, BIDI_MARKER_BG));
            assert_eq!(look.cursor_marker, None);
            assert_eq!(look.link_underline, None);
            assert_eq!(look.underline, None);
            assert_eq!(look.strikethrough, None);
            assert_eq!(look.overline, None);
            assert!(!look.bg_is_ground);
        }
    }

    #[test]
    fn faint_dims_an_explicit_underline_color_but_never_the_bidi_warning() {
        let theme = theme();
        let g = drive(1, 4, b"\x1b[2;4;58;5;1mA");
        let look = resolve_at(&g, &RowFrame::plain(false), 0, 0);
        let red = resolve_fg(Color::Indexed(1), &theme);
        assert_eq!(look.fg, dim_toward_bg(theme.fg, theme.bg));
        assert_eq!(
            look.underline,
            Some((UnderlineStyle::Single, dim_toward_bg(red, theme.bg)))
        );

        let g = drive(1, 4, "\x1b[2mA\u{202E}".as_bytes());
        let look = resolve_at(&g, &RowFrame::plain(false), 0, 0);
        assert_eq!(look.fg, BIDI_MARKER_FG);
    }

    #[test]
    fn a_bar_cursor_marks_only_the_lead_of_a_wide_character() {
        for cursor_col in [1, 2] {
            let g = drive(1, 4, format!("漢\x1b[1;{cursor_col}H\x1b[6 q").as_bytes());
            let frame = live_frame(&g, 0);
            let color = theme().fg;
            assert_eq!(
                resolve_at(&g, &frame, 0, 0).cursor_marker,
                Some((CursorMarkerKind::Bar, color)),
                "cursor on column {cursor_col}"
            );
            assert_eq!(resolve_at(&g, &frame, 0, 1).cursor_marker, None);
        }
    }

    #[test]
    fn a_cursor_of_any_style_suppresses_the_sgr_underline() {
        for style in ["2", "4", "6"] {
            let g = drive(1, 4, format!("\x1b[4mAB\x1b[1;1H\x1b[{style} q").as_bytes());
            let frame = live_frame(&g, 0);
            assert_eq!(
                resolve_at(&g, &frame, 0, 0).underline,
                None,
                "style {style}"
            );
            assert!(resolve_at(&g, &frame, 0, 1).underline.is_some());
        }
    }

    #[test]
    fn a_column_under_the_preedit_is_skipped_with_its_colors_resolved() {
        let g = drive(1, 4, b"\x1b[31;42mABCD");
        let frame = RowFrame {
            preedit: Some((1, 3)),
            ..RowFrame::plain(false)
        };
        for col in 0..4 {
            let look = resolve_at(&g, &frame, 0, col);
            let plain = resolve_at(&g, &RowFrame::plain(false), 0, col);
            let want = if (1..3).contains(&col) {
                Disposition::Skip
            } else {
                Disposition::Paint
            };
            assert_eq!(look.disposition, want, "col {col}");
            assert_eq!((look.fg, look.bg), (plain.fg, plain.bg), "col {col}");
        }
    }

    #[test]
    fn the_stale_tail_past_the_occupancy_watermark_reads_blank() {
        let g = drive(2, 4, b"\x1b[2;1H\x1b[41mXYZ\x1b[m\x1b[T");
        assert!(
            matches!(
                g.screen().row_cells(0).unwrap()[0].grapheme,
                Grapheme::Ascii(b'X')
            ),
            "the scroll recycles the old row behind a zero watermark"
        );
        let theme = theme();
        let look = resolve_at(&g, &RowFrame::plain(false), 0, 0);
        assert_eq!((look.fg, look.bg), (theme.fg, theme.bg));
        assert!(look.bg_is_ground);
    }

    #[test]
    fn a_slice_shorter_than_the_row_pads_with_blank_cells() {
        let g = drive(1, 2, b"\x1b[41mXY");
        let screen = g.screen();
        let row = RowCells {
            cols: 5,
            ..RowCells::of_screen(screen, 0).unwrap()
        };
        let theme = theme();
        let mut out = Vec::new();
        resolve_row(&row, &RowFrame::plain(true), &theme, &mut out);
        assert_eq!(out.len(), 5);
        for look in &out[2..] {
            assert_eq!((look.fg, look.bg), (theme.bg, theme.fg));
            assert!(!look.bg_is_ground, "screen reverse paints the bg");
        }
    }
}
