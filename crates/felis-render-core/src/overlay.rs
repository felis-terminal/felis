//! Frame overlays the client composes over the grid: IME pre-edit,
//! scrollback search, and the bottom-row bars.

/// Uncommitted IME composition text painted at 0-based `anchor` `(row, col)`.
///
/// Stops at the row's right edge without wrapping. `cursor` is the active-segment
/// byte range reported by winit.
#[derive(Debug, Clone)]
pub struct PreeditOverlay {
    pub anchor: (u16, u16),
    pub text: String,
    pub cursor: Option<(usize, usize)>,
}

/// Scrollback-search overlay with hit highlights and a bottom-row status bar.
///
/// The bar occludes the bottom row's cells. Re-derived on redraw and painted
/// until the renderer's `set_search_overlay` passes `None`.
#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    /// Empty suppresses the bar; highlights still paint.
    pub label: String,
    /// One entry per `col_span` of a `SearchToClientMsg::Match`, already mapped
    /// through the viewport by the caller. The renderer clips
    /// out-of-range rows / cols so a viewport edit racing the paint
    /// cannot panic.
    pub visible_hits: Vec<SearchHitSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHitSpan {
    /// Composed-view row (0 = top of visible screen; `rows - 1` = bottom).
    pub row: u16,
    /// Inclusive column range start.
    pub col_start: u16,
    /// Exclusive column range end.
    pub col_end: u16,
    /// The renderer does not enforce uniqueness.
    pub is_current: bool,
}

/// Same chrome surface as the search bar (the caller reserves the
/// bottom row identically), with
/// [`CONFIRM_BAR_BG`](crate::palette::CONFIRM_BAR_BG) so a destructive
/// prompt never reads as a search.
#[derive(Debug, Clone, Default)]
pub struct ConfirmOverlay {
    pub label: String,
}

/// The Ctrl-hover OSC 8 activation preview (`docs/explanation/security-model.md`
/// "OSC 8 hyperlinks and OSC 7 CWD"): `text` is already
/// `ActivationTarget::preview`'s output, so it carries no control or
/// bidi codepoint by construction.
#[derive(Debug, Clone, Default)]
pub struct LinkPreviewOverlay {
    pub text: String,
}
