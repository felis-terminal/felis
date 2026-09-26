//! Shadow screen mirroring daemon grid state via `GridMsg` diffs.
//!
//! Uses [`felis_grid::ScreenBuffer`] instead of `felis_grid::Grid` because
//! parser state belongs exclusively to the daemon's parse loop.

use std::num::{NonZeroU16, NonZeroU32};

use felis_grid::{
    Cell, ClusterText, Grapheme, HyperlinkEntry, LinkText, ScreenBuffer, TableGc,
    wire::{RowCodecError, decode_row},
};
use felis_protocol::{
    kitty_keyboard::KittyKbdFlags,
    messages::{
        ClipboardWrite, CursorStyle, GridDims, GridMsg, MAX_GRID_COLS, MAX_GRID_ROWS,
        ModifyOtherKeys, MouseProtocol,
    },
};
use thiserror::Error;
use tracing::{debug, warn};

/// The daemon owns history and composes the browse viewport itself,
/// shipping the result as ordinary `RowDelta` rows, so nothing reads a
/// shadow-side scrollback; a default-capacity grid would still reserve
/// the whole retention window (~12.8 MB per 80-column window) for
/// history that cannot arrive.
const SHADOW_SCROLLBACK_ROWS: usize = 0;

/// Every variant is a value the daemon cannot produce, so the client
/// closes the attachment and lets reconnect + rehydrate recover
/// (`docs/reference/ipc.md` "Corruption").
#[derive(Debug, Error)]
pub enum ShadowError {
    #[error("row codec: {0}")]
    Row(#[from] RowCodecError),
    #[error("RowDelta row {row} is past the {MAX_GRID_ROWS}-row admitted bound")]
    RowIndex { row: u16 },
    #[error("scroll region {top}..={bottom} is outside the {rows}-row authoritative grid")]
    ScrollRegion { top: u16, bottom: u16, rows: u16 },
    #[error("scroll count {n_rows} is outside 1..={height} for the named region")]
    ScrollCount { n_rows: u16, height: u16 },
    #[error("cursor ({row}, {col}) is outside the {rows}x{cols} authoritative grid")]
    Cursor {
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
    },
    #[error("viewport {lines_from_bottom} is past its own maximum {max}")]
    Viewport { lines_from_bottom: u32, max: u32 },
    #[error("registry id 0 is reserved")]
    RegistryIdZero,
    #[error("registry entry {id} is past a published cap")]
    RegistryEntry { id: u32 },
    #[error("registry id {id} already holds an entry")]
    RegistryIdReused { id: u32 },
    #[error("a row names cluster {id}, which was never installed")]
    UnresolvedCluster { id: u32 },
    #[error("a row names hyperlink {id}, which was never installed")]
    UnresolvedHyperlink { id: u16 },
}

// Independent mode mirrors and dirty flags, not disjoint states of one
// machine, so `clippy::struct_excessive_bools`'s enum suggestion does
// not apply.
#[allow(clippy::struct_excessive_bools)]
pub struct ShadowScreen {
    screen: ScreenBuffer,
    title: Option<String>,
    title_dirty: bool,
    cwd: Option<String>,
    pointer_shape: Option<String>,
    /// The shadow fills its style and sizing registries the way the
    /// daemon's parse does (a pen per decoded run, a sizing entry per
    /// sized cell of every `RowDelta`), and a window, unlike a session,
    /// has no restart to clear them.
    table_gc: TableGc,
    kitty_kbd_flags: KittyKbdFlags,
    bracketed_paste: bool,
    alt_screen: bool,
    mouse_protocol: MouseProtocol,
    application_cursor: bool,
    modify_other_keys: ModifyOtherKeys,
    application_keypad: bool,
    /// Mirrored rather than gated on the client's own OS, so a
    /// non-Windows client attached to a Windows daemon honors it.
    win32_input_mode: bool,
    reverse_video: bool,
    /// `0` = live bottom; `N>0` = the top `min(N, rows)` visible rows
    /// come from retained scrollback.
    viewport: u32,
    /// Retained scrollback + live rows; starts at `rows`.
    viewport_max: u32,
    pending_clipboard_set: Option<ClipboardWrite>,
    rehydrating: bool,
    /// The last cursor the daemon stated, kept apart from the grid's
    /// own: an optimistic `local_resize` clamps that one, and the
    /// correction the daemon answers with carries no `CursorState` when
    /// its cursor has not moved.
    cursor: AuthoritativeCursor,
    /// Geometry as the daemon last announced it (`SessionToClientMsg::Attached`,
    /// `GridMsg::Size`), kept apart from the grid's own dimensions:
    /// [`Self::local_resize`] moves those ahead of the daemon honoring
    /// the resize, and the rehydrate burst carries no dimensions, so it
    /// must blank at the geometry the daemon composed it for.
    announced: GridDims,
}

impl ShadowScreen {
    /// `rows`/`cols` are what the daemon announced on
    /// `SessionToClientMsg::Attached`; the rehydrate burst blanks at them.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            screen: ScreenBuffer::with_scrollback(rows, cols, SHADOW_SCROLLBACK_ROWS),
            title: None,
            title_dirty: false,
            cwd: None,
            pointer_shape: None,
            table_gc: TableGc::new(),
            kitty_kbd_flags: KittyKbdFlags::empty(),
            bracketed_paste: false,
            alt_screen: false,
            mouse_protocol: MouseProtocol::Off,
            application_cursor: false,
            modify_other_keys: ModifyOtherKeys::Off,
            application_keypad: false,
            win32_input_mode: false,
            reverse_video: false,
            viewport: 0,
            viewport_max: u32::from(rows),
            pending_clipboard_set: None,
            rehydrating: false,
            cursor: AuthoritativeCursor::default(),
            announced: GridDims {
                rows,
                cols,
                pixel_w: 0,
                pixel_h: 0,
            },
        }
    }

    pub const fn take_pending_clipboard_set(&mut self) -> Option<ClipboardWrite> {
        self.pending_clipboard_set.take()
    }

    /// Top of the daemon's Kitty keyboard flag stack.
    #[must_use]
    pub const fn kitty_kbd_flags(&self) -> KittyKbdFlags {
        self.kitty_kbd_flags
    }

    /// `?2004`. The framing itself is the daemon's: it wraps every
    /// `InputMsg::Paste` against its own grid, so no client reads this
    /// to decide the wrapping. It gates REQ-804's multi-line-paste
    /// confirmation, which only appears with bracketed paste off.
    #[must_use]
    pub const fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// `?1049`.
    #[must_use]
    pub const fn alt_screen(&self) -> bool {
        self.alt_screen
    }

    /// Any of `?1000` / `?1002` / `?1003`.
    #[must_use]
    pub const fn mouse_protocol_active(&self) -> bool {
        !matches!(self.mouse_protocol, MouseProtocol::Off)
    }

    /// `?1` DECCKM.
    #[must_use]
    pub const fn application_cursor(&self) -> bool {
        self.application_cursor
    }

    /// xterm `modifyOtherKeys` level (REQ-506).
    #[must_use]
    pub const fn modify_other_keys(&self) -> ModifyOtherKeys {
        self.modify_other_keys
    }
    /// DECKPAM / DECKPNM.
    #[must_use]
    pub const fn application_keypad(&self) -> bool {
        self.application_keypad
    }

    /// `?9001` win32-input-mode.
    #[must_use]
    pub const fn win32_input_mode(&self) -> bool {
        self.win32_input_mode
    }

    /// `?5` DECSCNM.
    #[must_use]
    pub const fn reverse_video(&self) -> bool {
        self.reverse_video
    }

    /// `0` = live bottom; `N>0` = the visible window starts `N` rows
    /// above it.
    #[must_use]
    pub const fn viewport(&self) -> u32 {
        self.viewport
    }

    /// Retained scrollback rows + live rows.
    #[must_use]
    pub const fn viewport_max(&self) -> u32 {
        self.viewport_max
    }

    #[must_use]
    pub const fn viewport_at_bottom(&self) -> bool {
        self.viewport == 0
    }

    /// The daemon ships entries before any `RowDelta` that references
    /// them, so `None` means a buggy peer or mid-rehydrate.
    #[must_use]
    pub fn hyperlink(&self, id: NonZeroU16) -> Option<&HyperlinkEntry> {
        self.screen.hyperlink(id)
    }

    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn take_title_dirty(&mut self) -> Option<String> {
        if self.title_dirty {
            self.title_dirty = false;
            self.title.clone()
        } else {
            None
        }
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// CSS cursor keyword from `OSC 22`; `None` is the default arrow.
    #[must_use]
    pub fn pointer_shape(&self) -> Option<&str> {
        self.pointer_shape.as_deref()
    }

    #[must_use]
    pub const fn screen(&self) -> &ScreenBuffer {
        &self.screen
    }

    /// After a paint consumed the rows written since the last one.
    pub fn clear_damage(&mut self) {
        self.screen.clear_damage();
    }

    #[must_use]
    pub fn cluster_str(&self, id: NonZeroU32) -> Option<&str> {
        self.screen.cluster_str(id)
    }

    /// True between `RehydrateBegin` and `RehydrateEnd`.
    #[must_use]
    pub const fn is_rehydrating(&self) -> bool {
        self.rehydrating
    }

    /// The grid itself is left alone: a session switch keeps the
    /// outgoing session's cells on screen until the burst lands.
    pub const fn announce_dims(&mut self, rows: u16, cols: u16) {
        self.announced = GridDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        };
    }

    pub fn local_resize(&mut self, rows: u16, cols: u16) {
        self.screen.resize(rows, cols);
        self.place_cursor();
    }

    /// `ScreenBuffer::resize` clamps its cursor into the new geometry,
    /// so every geometry change re-places the daemon's coordinate: a
    /// resize the daemon overrules would otherwise leave the clamped
    /// position standing until the cursor next moves.
    const fn place_cursor(&mut self) {
        let c = self.cursor;
        let _ = self
            .screen
            .set_cursor_state(c.row, c.col, c.visible, c.style, c.blink);
    }

    /// Errors only on a malformed row body (a buggy peer).
    #[expect(
        clippy::match_same_arms,
        reason = "distinct GridMsg variants kept as separate arms for clarity even where the shadow treats them as no-ops"
    )]
    pub fn apply(&mut self, msg: &GridMsg) -> Result<(), ShadowError> {
        match msg {
            GridMsg::RehydrateBegin => {
                // The announced geometry, not the grid's: a `local_resize`
                // the daemon has not honored yet must not make the
                // burst's rows land short.
                self.screen = ScreenBuffer::with_scrollback(
                    self.announced.rows,
                    self.announced.cols,
                    SHADOW_SCROLLBACK_ROWS,
                );
                self.place_cursor();
                self.rehydrating = true;
            }
            GridMsg::RehydrateEnd => {
                self.rehydrating = false;
            }
            GridMsg::RowDelta { rows } => {
                for (row, packed_cells) in rows {
                    self.decode_and_write_row(*row, &packed_cells.0)?;
                }
            }
            GridMsg::CursorState {
                row,
                col,
                visible,
                style,
                blink,
            } => {
                self.set_cursor(*row, *col, *visible, *style, *blink)?;
            }
            GridMsg::Title { value } => {
                if self.title.as_deref() != Some(value.as_str()) {
                    self.title = Some(value.clone());
                    self.title_dirty = true;
                }
            }
            GridMsg::Cwd { value } => {
                self.cwd = Some(value.clone());
            }
            GridMsg::PointerShape { name } => {
                self.pointer_shape.clone_from(name);
            }
            // `ScrollToPrompt` resolves prompt marks daemon-side; nothing
            // reads a local mirror.
            GridMsg::PromptMark { .. } => {}
            // The client applies OSC 10/11/12 straight from this message
            // to the renderer.
            GridMsg::ThemeColor { .. } => {}
            // The OSC 4 / 104 override layer lives on the renderer beside
            // the configured palette; a copy here would be a second
            // palette for the shadow's cells to disagree with.
            GridMsg::PaletteColor { .. } | GridMsg::PaletteResetAll => {}
            // Pacing, not state: the marker says the cycle already
            // applied is complete.
            GridMsg::CycleEnd => {}
            GridMsg::KittyKbdFlags { flags } => {
                self.kitty_kbd_flags = *flags;
            }
            GridMsg::ClipboardSet { write } => {
                self.pending_clipboard_set = Some(write.clone());
            }
            GridMsg::Hyperlink { id, anchor, uri } => {
                // Ids arrive in reference order with gaps, and id 0 is
                // reserved. Every refusal below is a value the daemon's
                // own table could not hold, so it is corruption, not a
                // gap.
                let nz = NonZeroU16::new(*id).ok_or(ShadowError::RegistryIdZero)?;
                // An id sent twice redefines what cells already drew,
                // identical text included: the registry contract sends
                // each entry once per connection.
                if self.screen.hyperlink(nz).is_some() {
                    return Err(ShadowError::RegistryIdReused { id: u32::from(*id) });
                }
                let entry = link_entry(anchor.as_deref(), uri)
                    .ok_or_else(|| ShadowError::RegistryEntry { id: u32::from(*id) })?;
                if !self.screen.install_hyperlink(nz, entry) {
                    return Err(ShadowError::RegistryEntry { id: u32::from(*id) });
                }
            }
            GridMsg::Cluster { id, text } => {
                // Same rules as `Hyperlink`
                // (docs/explanation/data-model/grid-and-cells.md).
                let nz = NonZeroU32::new(*id).ok_or(ShadowError::RegistryIdZero)?;
                if self.screen.cluster_str(nz).is_some() {
                    return Err(ShadowError::RegistryIdReused { id: *id });
                }
                let text = ClusterText::new(text.as_str())
                    .ok_or(ShadowError::RegistryEntry { id: *id })?;
                if !self.screen.install_cluster(nz, text) {
                    return Err(ShadowError::RegistryEntry { id: *id });
                }
            }
            // The App reads the original `GridMsg` after `apply` to drive
            // window attention; a notification's content goes to
            // observers, not the shadow
            // (docs/reference/protocols/notifications.md).
            GridMsg::Attention { .. } => {}
            GridMsg::ModeFlags {
                bracketed_paste,
                alt_screen,
                mouse_protocol,
                application_cursor,
                modify_other_keys,
                application_keypad,
                win32_input_mode,
                reverse_video,
            } => {
                self.bracketed_paste = *bracketed_paste;
                self.alt_screen = *alt_screen;
                self.mouse_protocol = *mouse_protocol;
                self.application_cursor = *application_cursor;
                self.modify_other_keys = *modify_other_keys;
                self.application_keypad = *application_keypad;
                self.win32_input_mode = *win32_input_mode;
                self.reverse_video = *reverse_video;
            }
            GridMsg::ViewportState {
                lines_from_bottom,
                max,
            } => {
                if lines_from_bottom > max {
                    return Err(ShadowError::Viewport {
                        lines_from_bottom: *lines_from_bottom,
                        max: *max,
                    });
                }
                self.viewport = *lines_from_bottom;
                self.viewport_max = *max;
            }
            GridMsg::Scrolled {
                region_top,
                region_bottom,
                n_rows,
                direction,
            } => {
                // docs/reference/ipc.md: the blanked band lands as default
                // cells; the daemon follows with a `RowDelta` for any
                // blank carrying non-default attributes.
                self.scroll(*region_top, *region_bottom, *n_rows, *direction)?;
            }
            GridMsg::Size { dims } => {
                // Authoritative PTY dimensions (`docs/explanation/architecture/session-lifecycle.md`).
                // Overrules optimistic local resize; renderer letterboxes until
                // accompanying `RowDelta` replay arrives.
                self.announced = *dims;
                self.screen.resize(dims.rows, dims.cols);
                self.place_cursor();
            }
        }
        // Sweep after the message that grew the tables: no handle into
        // either survives an `apply`, since the renderer reads resolved
        // values and keys its glyph cache by the `Sizing` itself.
        self.table_gc.maybe_sweep(&mut self.screen);
        Ok(())
    }

    fn decode_and_write_row(&mut self, row: u16, packed_cells: &[u8]) -> Result<(), ShadowError> {
        // Before the decode, so a row index no geometry admits costs no
        // cell vector.
        if row >= MAX_GRID_ROWS {
            return Err(ShadowError::RowIndex { row });
        }
        let decoded = decode_row(packed_cells, self.screen.style_table_mut())?;
        self.check_row_handles(&decoded)?;
        self.write_row(row, &decoded);
        Ok(())
    }

    /// The daemon ships every registry entry a row names before that row
    /// (`docs/reference/ipc.md` "Registry delivery"), so an unresolved
    /// handle is corruption, not a gap the next message fills.
    fn check_row_handles(&self, decoded: &felis_grid::DecodedRow) -> Result<(), ShadowError> {
        for cell in &decoded.cells {
            if let Grapheme::Cluster(id) = cell.grapheme
                && self.screen.cluster_str(id).is_none()
            {
                return Err(ShadowError::UnresolvedCluster { id: id.get() });
            }
            if let Some(link) = cell.link
                && self.screen.hyperlink(link).is_none()
            {
                return Err(ShadowError::UnresolvedHyperlink { id: link.get() });
            }
        }
        Ok(())
    }

    fn write_row(&mut self, row: u16, decoded: &felis_grid::DecodedRow) {
        let cells = &decoded.cells;
        let sized_cells = &decoded.sized_cells;
        // Cold-start race: the daemon processes the window's first
        // `InputMsg::Resize` after the rehydrate burst, so its damage
        // burst at the new geometry lands on a shadow still at the
        // attach dims. Grow instead of dropping the rows.
        let want_rows = u16::try_from(usize::from(row).saturating_add(1))
            .unwrap_or(u16::MAX)
            .max(self.screen.rows());
        // `decode_row` refuses a wider row, so this never truncates.
        let want_cols = u16::try_from(cells.len())
            .unwrap_or(MAX_GRID_COLS)
            .min(MAX_GRID_COLS);
        if want_rows > self.screen.rows() || want_cols > self.screen.cols() {
            let new_cols = want_cols.max(self.screen.cols());
            self.screen.resize(want_rows, new_cols);
        }
        let cols = self.screen.cols();
        if cells.len() < usize::from(cols) {
            // Mid-shrink race: the daemon's last burst was at the old
            // wider count.
            warn!(
                row,
                got = cells.len(),
                expected = cols,
                "row narrower than shadow — padding"
            );
        }
        // One `copy_from_slice` plus one damage mark: per-cell `set_cell`
        // recomputes the `row_lookup` indirection and re-tests damage
        // per cell, which dominates the consume path on full-screen
        // repaints.
        if cells.len() == usize::from(cols) {
            self.screen.write_row_cells(row, cells);
        } else {
            // Mid-resize race. The wire cell is stored verbatim rather
            // than rebuilt through a synthetic escape burst and a fresh
            // `Parser`: that round-trip caps the consume rate below the
            // daemon's produce rate and cannot reconstruct
            // `Empty`/`Spacer`/`SizedSpacer` cells or the link slot.
            let limit = usize::from(cols).min(cells.len());
            for (col, cell) in cells.iter().take(limit).enumerate() {
                self.screen.set_cell(row, col as u16, *cell);
            }
            for col in limit..usize::from(cols) {
                self.screen.set_cell(row, col as u16, Cell::default());
            }
        }
        // OSC 66 side-band, stamped after the cell sweep because
        // `put_grapheme` clears prior sizing on re-print. Handles diverge
        // from the daemon's; the renderer reads through `cell_sizing`,
        // never by handle.
        for (col, sizing) in sized_cells {
            if let Some(handle) = self.screen.install_sizing(*sizing) {
                self.screen.set_cell_sizing(row, *col, Some(handle));
            }
        }
        // Triple-click stitching reads this bit to match the daemon's
        // search; the renderer never does, so it carries no damage.
        self.screen.set_soft_wrap(row, decoded.soft_wrap_continued);
    }

    const fn set_cursor(
        &mut self,
        row: u16,
        col: u16,
        visible: bool,
        style: CursorStyle,
        blink: bool,
    ) -> Result<(), ShadowError> {
        // Visibility does not relax the bound: a hidden cursor carries
        // the same authoritative coordinate a visible one does.
        if row >= self.announced.rows || col >= self.announced.cols {
            return Err(ShadowError::Cursor {
                row,
                col,
                rows: self.announced.rows,
                cols: self.announced.cols,
            });
        }
        // Recorded whether or not it fits the shadow's own dimensions:
        // an optimistic resize is the client's, and the daemon states
        // this coordinate once. A `Grid` setter, not re-encoded CUP /
        // DECTCEM / DECSCUSR: the apply path has no parser.
        self.cursor = AuthoritativeCursor {
            row,
            col,
            visible,
            style,
            blink,
        };
        self.place_cursor();
        Ok(())
    }

    fn scroll(
        &mut self,
        region_top: u16,
        region_bottom: u16,
        n_rows: u16,
        direction: felis_protocol::messages::ScrollDirection,
    ) -> Result<(), ShadowError> {
        if region_top > region_bottom || region_bottom >= self.announced.rows {
            return Err(ShadowError::ScrollRegion {
                top: region_top,
                bottom: region_bottom,
                rows: self.announced.rows,
            });
        }
        let height = region_bottom - region_top + 1;
        if n_rows == 0 || n_rows > height {
            return Err(ShadowError::ScrollCount { n_rows, height });
        }
        if !self
            .screen
            .apply_scroll_directive(region_top, region_bottom, n_rows, direction)
        {
            debug!(
                region_top,
                region_bottom, "scroll region outside the locally resized shadow"
            );
        }
        Ok(())
    }
}

/// The cursor as the daemon stated it, which outlives a shadow-local
/// resize that cannot hold the coordinate.
/// Its `Default` is a fresh `ScreenBuffer`'s cursor, so re-placing one
/// the daemon has not stated yet changes nothing.
#[derive(Clone, Copy)]
struct AuthoritativeCursor {
    row: u16,
    col: u16,
    visible: bool,
    style: CursorStyle,
    blink: bool,
}

impl Default for AuthoritativeCursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            style: CursorStyle::default(),
            blink: true,
        }
    }
}

/// `None` when either half is past [`LinkText::CAP`].
fn link_entry(anchor: Option<&str>, uri: &str) -> Option<HyperlinkEntry> {
    Some(HyperlinkEntry {
        id: match anchor {
            Some(anchor) => Some(LinkText::new(anchor)?),
            None => None,
        },
        uri: LinkText::new(uri)?,
    })
}

#[cfg(test)]
mod tests {
    use felis_grid::{
        AttrFlags, Attributes, Cell, Color, Grapheme, RowEncode, StyleId, StyleTable, encode_row,
    };
    use felis_protocol::RowPayload;
    use felis_protocol::kitty_text_sizing::{HAlign, Sizing, VAlign};
    use felis_protocol::messages::{AttentionSource, ClipboardSelection, ScrollDirection};

    use super::*;

    fn packed_row(cells: &[Cell], styles: &StyleTable) -> Vec<u8> {
        encode_row(
            RowEncode {
                cells,
                pad_to: cells.len(),
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            styles,
        )
        .unwrap()
    }

    /// Pins: the cell ring costs the viewport, not the daemon's
    /// retention window (a default-capacity `Grid` reserves ~12.8 MB at
    /// 24x80 for history the shadow can never receive).
    #[test]
    fn a_fresh_shadow_reserves_only_its_viewport() {
        let (rows, cols) = (24u16, 80u16);
        let shadow = ShadowScreen::new(rows, cols);
        let viewport_bytes = usize::from(rows) * usize::from(cols) * size_of::<Cell>();
        assert_eq!(viewport_bytes, 30_720);
        let reserved = shadow.screen().reserved_cell_bytes();
        assert!(
            reserved <= viewport_bytes * 2,
            "shadow reserved {reserved} bytes for a {viewport_bytes}-byte viewport",
        );
        assert_eq!(shadow.screen().scrollback_capacity(), 0);
    }

    /// Pins: a rehydrate, which rebuilds the grid, does not re-acquire
    /// the retention window either.
    #[test]
    fn a_rehydrated_shadow_reserves_only_its_viewport() {
        let (rows, cols) = (24u16, 80u16);
        let mut shadow = ShadowScreen::new(rows, cols);
        shadow.apply(&GridMsg::RehydrateBegin).unwrap();
        shadow.apply(&GridMsg::RehydrateEnd).unwrap();
        let viewport_bytes = usize::from(rows) * usize::from(cols) * size_of::<Cell>();
        assert!(
            shadow.screen().reserved_cell_bytes() <= viewport_bytes * 2,
            "rehydrate re-reserved the retention window",
        );
        assert_eq!(shadow.screen().scrollback_capacity(), 0);
    }

    /// Pins: `GridMsg::Size` resizes to the daemon's dims without a
    /// rehydrate, rolling back an optimistic `local_resize` the daemon
    /// overruled.
    #[test]
    fn grid_size_resizes_the_shadow_to_authoritative_dims() {
        let mut shadow = ShadowScreen::new(24, 80);
        shadow.local_resize(50, 200);
        shadow
            .apply(&GridMsg::Size {
                dims: GridDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            })
            .unwrap();
        assert_eq!(
            (shadow.screen().rows(), shadow.screen().cols()),
            (24, 80),
            "shadow must track the daemon's grid, not the window",
        );
    }

    /// Pins: an OSC 66 `s=2` run lands with both the cells and the
    /// `(col, Sizing)` side-band intact.
    #[test]
    fn row_delta_carries_osc_66_sizing_to_the_shadow() {
        let mut shadow = ShadowScreen::new(2, 4);
        let scale_two = Sizing::new(2, 0, 0, 0, VAlign::Top, HAlign::Left).unwrap();
        let row0 = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'A'),
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::SizedSpacer,
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Ascii(b'B'),
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::SizedSpacer,
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
        ];
        let row1 = vec![
            Cell {
                grapheme: Grapheme::SizedSpacer,
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            };
            4
        ];
        let sized_row0 = vec![
            (0, scale_two),
            (1, scale_two),
            (2, scale_two),
            (3, scale_two),
        ];
        let sized_row1 = sized_row0.clone();
        let packed_sized = |cells: &[Cell], sized: &[(u16, Sizing)]| {
            encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: sized,
                    soft_wrap_continued: false,
                },
                &StyleTable::new(),
            )
            .unwrap()
        };
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_sized(&row0, &sized_row0)))],
            })
            .unwrap();
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(1, RowPayload(packed_sized(&row1, &sized_row1)))],
            })
            .unwrap();
        assert_eq!(
            shadow.screen().cell(0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'A'),
        );
        assert_eq!(
            shadow.screen().cell(0, 2).unwrap().grapheme,
            Grapheme::Ascii(b'B'),
        );
        assert_eq!(
            shadow.screen().cell(0, 1).unwrap().grapheme,
            Grapheme::SizedSpacer,
        );
        assert_eq!(
            shadow.screen().cell(1, 0).unwrap().grapheme,
            Grapheme::SizedSpacer,
        );
        for r in 0..2 {
            for c in 0..4 {
                assert_eq!(
                    shadow.screen().cell_sizing(r, c).copied(),
                    Some(scale_two),
                    "cell ({r}, {c}) lost its OSC 66 sizing on the wire",
                );
            }
        }
    }

    /// Pins: `RehydrateBegin` blanks at the announced dims, not the
    /// grid's current ones.
    #[test]
    fn rehydrate_blanks_the_grid_at_the_announced_dims() {
        let mut shadow = ShadowScreen::new(2, 4);
        let cells = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'A'),
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
            Cell::default(),
            Cell::default(),
            Cell::default(),
        ];
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&cells, &StyleTable::new())))],
            })
            .unwrap();
        shadow.local_resize(5, 9);
        shadow.apply(&GridMsg::RehydrateBegin).unwrap();
        assert_eq!((shadow.screen().rows(), shadow.screen().cols()), (2, 4));
        assert_eq!(
            shadow.screen().cell(0, 0).unwrap().grapheme,
            Cell::default().grapheme,
            "rehydrate must blank the previous session's cells",
        );
        assert!(shadow.is_rehydrating());
        shadow.apply(&GridMsg::RehydrateEnd).unwrap();
        assert!(!shadow.is_rehydrating());
    }

    /// Pins: `announce_dims` and a mid-stream `GridMsg::Size` both set
    /// the dims the next rehydrate blanks at.
    #[test]
    fn rehydrate_follows_the_latest_announcement() {
        let mut shadow = ShadowScreen::new(2, 4);
        shadow.announce_dims(10, 20);
        shadow.apply(&GridMsg::RehydrateBegin).unwrap();
        assert_eq!((shadow.screen().rows(), shadow.screen().cols()), (10, 20));
        shadow
            .apply(&GridMsg::Size {
                dims: GridDims {
                    rows: 7,
                    cols: 15,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            })
            .unwrap();
        shadow.apply(&GridMsg::RehydrateBegin).unwrap();
        assert_eq!((shadow.screen().rows(), shadow.screen().cols()), (7, 15));
    }

    #[test]
    fn row_delta_writes_cells_into_the_shadow() {
        let mut shadow = ShadowScreen::new(1, 4);
        let mut styles = StyleTable::new();
        let bold = styles.intern(Attributes {
            fg: Color::Indexed(2),
            bg: Color::Default,
            flags: AttrFlags::BOLD,
            ..Attributes::default()
        });
        let cells = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'h'),
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            },
            Cell {
                grapheme: Grapheme::Ascii(b'i'),
                style: bold,
                link: None,
                sizing: None,
            },
            Cell::default(),
            Cell::default(),
        ];
        let body = packed_row(&cells, &styles);
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(body))],
            })
            .unwrap();
        assert_eq!(
            shadow.screen().cell(0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'h')
        );
        assert_eq!(
            shadow.screen().cell(0, 1).unwrap().grapheme,
            Grapheme::Ascii(b'i')
        );
        let style = shadow.screen().cell(0, 1).unwrap().style;
        assert!(shadow.screen().style(style).flags.contains(AttrFlags::BOLD));
    }

    #[test]
    fn row_delta_mirrors_the_soft_wrap_bit_both_ways() {
        // Pins: a later RowDelta with the bit clear clears the mirror; a
        // hard EL resets the bit without changing any cell the shadow
        // could infer it from.
        let mut shadow = ShadowScreen::new(2, 4);
        let cells: Vec<Cell> = (0..4).map(|_| Cell::default()).collect();
        for continued in [true, false] {
            shadow
                .apply(&GridMsg::RowDelta {
                    rows: vec![(
                        1,
                        RowPayload(
                            encode_row(
                                RowEncode {
                                    cells: &cells,
                                    pad_to: cells.len(),
                                    sized_cells: &[],
                                    soft_wrap_continued: continued,
                                },
                                &StyleTable::new(),
                            )
                            .unwrap(),
                        ),
                    )],
                })
                .unwrap();
            assert_eq!(shadow.screen().row_soft_wrap_continued(1), continued);
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// Auto-grow has one closed form inside the admitted bounds: the
        /// shadow grows to hold the row index and the row body, never
        /// past `MAX_GRID_ROWS` / `MAX_GRID_COLS` and never shrinking.
        /// Both directions of the resize race land here.
        #[test]
        fn a_row_delta_grows_the_shadow_to_the_closed_form(
            rows0 in 1_u16..=24,
            cols0 in 1_u16..=80,
            row in proptest::prop_oneof![0_u16..=120, 2040_u16..=2047],
            len in proptest::prop_oneof![0_usize..=120, 2040_usize..=2048],
        ) {
            let mut shadow = ShadowScreen::new(rows0, cols0);
            let cells: Vec<Cell> = (0..len).map(|_| Cell::default()).collect();
            let body = packed_row(&cells, &StyleTable::new());
            shadow
                .apply(&GridMsg::RowDelta {
                    rows: vec![(row, RowPayload(body))],
                })
                .expect("a row inside the admitted bounds is never an error");

            let cols = u16::try_from(len).unwrap_or(MAX_GRID_COLS);
            proptest::prop_assert_eq!(shadow.screen().rows(), rows0.max(row + 1));
            proptest::prop_assert_eq!(shadow.screen().cols(), cols0.max(cols));
        }
    }

    #[test]
    fn cursor_state_moves_the_cursor() {
        let mut shadow = ShadowScreen::new(5, 10);
        shadow
            .apply(&GridMsg::CursorState {
                row: 2,
                col: 7,
                visible: false,
                style: CursorStyle::default(),
                blink: true,
            })
            .unwrap();
        let cur = shadow.screen().cursor();
        assert_eq!(cur.row, 2);
        assert_eq!(cur.col, 7);
        assert!(!cur.visible);
    }

    /// Pins: a `RowDelta` does not move the cursor. The daemon sends a
    /// follow-up `CursorState` only when the cursor changed, so a mirror
    /// path that advanced it as a side effect would strand the displayed
    /// cursor at the right edge.
    #[test]
    fn row_delta_preserves_the_cursor_position() {
        let mut shadow = ShadowScreen::new(3, 10);
        shadow
            .apply(&GridMsg::CursorState {
                row: 1,
                col: 3,
                visible: true,
                style: CursorStyle::default(),
                blink: true,
            })
            .unwrap();
        let row1: Vec<Cell> = (0..10)
            .map(|i| Cell {
                grapheme: Grapheme::Ascii(b'A' + (i % 26) as u8),
                style: StyleId::DEFAULT,
                link: None,
                sizing: None,
            })
            .collect();
        let body = packed_row(&row1, &StyleTable::new());
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(1, RowPayload(body))],
            })
            .unwrap();
        let cur = shadow.screen().cursor();
        assert_eq!(cur.row, 1, "row stayed put");
        assert_eq!(cur.col, 3, "col restored to the pre-RowDelta value");
        assert!(!cur.pending_wrap, "pending_wrap not stuck after the sweep");
    }

    #[test]
    fn malformed_row_body_returns_a_codec_error() {
        let mut shadow = ShadowScreen::new(1, 4);
        let bad = vec![0xff, 0xff, 0xff];
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(bad))],
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::Row(_)));
    }

    #[test]
    fn title_message_updates_shadow_and_sets_dirty_once() {
        let mut shadow = ShadowScreen::new(1, 4);
        shadow
            .apply(&GridMsg::Title {
                value: "first".into(),
            })
            .unwrap();
        assert_eq!(shadow.title(), Some("first"));
        assert_eq!(shadow.take_title_dirty().as_deref(), Some("first"));
        shadow
            .apply(&GridMsg::Title {
                value: "first".into(),
            })
            .unwrap();
        assert!(shadow.take_title_dirty().is_none());
        shadow
            .apply(&GridMsg::Title {
                value: "second".into(),
            })
            .unwrap();
        assert_eq!(shadow.take_title_dirty().as_deref(), Some("second"));
    }

    /// One known cell and a drained title, so a no-op arm that blanks
    /// cells or flips the title dirty-flag fails the post-apply check.
    fn seeded_shadow_for_no_op_check() -> ShadowScreen {
        use felis_grid::Grapheme;
        let mut shadow = ShadowScreen::new(2, 4);
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(
                    0,
                    RowPayload(packed_row(
                        &[
                            Cell {
                                grapheme: Grapheme::Ascii(b'A'),
                                style: StyleId::DEFAULT,
                                link: None,
                                sizing: None,
                            },
                            Cell::default(),
                            Cell::default(),
                            Cell::default(),
                        ],
                        &StyleTable::new(),
                    )),
                )],
            })
            .unwrap();
        shadow
            .apply(&GridMsg::Title {
                value: "before".into(),
            })
            .unwrap();
        drop(shadow.take_title_dirty());
        shadow
    }

    fn assert_no_op_left_shadow_untouched(shadow: &mut ShadowScreen) {
        use felis_grid::Grapheme;
        assert_eq!(
            shadow.screen().cell(0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'A'),
            "cell content untouched",
        );
        assert!(
            shadow.take_title_dirty().is_none(),
            "title-dirty stayed clean — the no-op arm did not flip it",
        );
    }

    /// Pins: `PromptMark` (still shipped on rehydrate and live) applies
    /// as a no-op.
    #[test]
    fn prompt_mark_message_is_a_shadow_no_op_and_apply_succeeds() {
        use felis_protocol::messages::PromptKind;
        let mut shadow = seeded_shadow_for_no_op_check();
        shadow
            .apply(&GridMsg::PromptMark {
                line: 0,
                kind: PromptKind::PromptStart,
                exit_code: None,
            })
            .expect("PromptMark applies cleanly");
        assert_no_op_left_shadow_untouched(&mut shadow);
    }

    /// Pins: `ThemeColor` applies as a no-op.
    #[test]
    fn theme_color_message_is_a_shadow_no_op_and_apply_succeeds() {
        use felis_protocol::messages::{ThemeAction, ThemeChannel};
        let mut shadow = seeded_shadow_for_no_op_check();
        shadow
            .apply(&GridMsg::ThemeColor {
                channel: ThemeChannel::Foreground,
                action: ThemeAction::Set {
                    rgb: (0x10, 0x20, 0x30),
                },
            })
            .expect("ThemeColor applies cleanly");
        assert_no_op_left_shadow_untouched(&mut shadow);
    }

    #[test]
    fn cwd_message_updates_shadow() {
        let mut shadow = ShadowScreen::new(1, 4);
        shadow
            .apply(&GridMsg::Cwd {
                value: "file:///home".into(),
            })
            .unwrap();
        assert_eq!(shadow.cwd(), Some("file:///home"));
        shadow
            .apply(&GridMsg::Cwd {
                value: "file:///other".into(),
            })
            .unwrap();
        assert_eq!(shadow.cwd(), Some("file:///other"));
    }

    #[test]
    fn clipboard_set_message_queues_into_pending_replace_style() {
        let mut shadow = ShadowScreen::new(1, 1);
        shadow
            .apply(&GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::CLIPBOARD,
                    data: b"first".to_vec(),
                },
            })
            .unwrap();
        shadow
            .apply(&GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY,
                    data: b"second".to_vec(),
                },
            })
            .unwrap();
        let drained = shadow.take_pending_clipboard_set().unwrap();
        assert_eq!(
            drained.selection,
            ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY
        );
        assert_eq!(drained.data, b"second");
        assert!(shadow.take_pending_clipboard_set().is_none());
    }

    #[test]
    fn bell_message_is_a_shadow_no_op_and_apply_succeeds() {
        // Pins: Bell applies as a no-op and touches no cells or title.
        use felis_grid::Grapheme;
        let mut shadow = ShadowScreen::new(2, 4);
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(
                    0,
                    RowPayload(packed_row(
                        &[
                            Cell {
                                grapheme: Grapheme::Ascii(b'A'),
                                style: StyleId::DEFAULT,
                                link: None,
                                sizing: None,
                            },
                            Cell::default(),
                            Cell::default(),
                            Cell::default(),
                        ],
                        &StyleTable::new(),
                    )),
                )],
            })
            .unwrap();
        shadow
            .apply(&GridMsg::Title {
                value: "before".into(),
            })
            .unwrap();
        drop(shadow.take_title_dirty());
        shadow
            .apply(&GridMsg::Attention {
                source: AttentionSource::Bell,
            })
            .expect("Bell applies cleanly");
        assert_eq!(
            shadow.screen().cell(0, 0).unwrap().grapheme,
            Grapheme::Ascii(b'A'),
            "cell content untouched",
        );
        assert!(
            shadow.take_title_dirty().is_none(),
            "title-dirty stayed clean — Bell did not flip it",
        );
    }

    #[test]
    fn mode_flags_message_updates_shadow_mirror() {
        let mut shadow = ShadowScreen::new(1, 4);
        assert!(!shadow.bracketed_paste());
        assert!(!shadow.alt_screen());
        assert!(!shadow.mouse_protocol_active());
        assert!(!shadow.application_cursor());
        assert_eq!(shadow.modify_other_keys(), ModifyOtherKeys::Off);
        assert!(!shadow.application_keypad());
        assert!(!shadow.win32_input_mode());
        assert!(!shadow.reverse_video());
        shadow
            .apply(&GridMsg::ModeFlags {
                bracketed_paste: true,
                alt_screen: true,
                mouse_protocol: MouseProtocol::ButtonAndDrag,
                application_cursor: true,
                modify_other_keys: ModifyOtherKeys::Level2,
                application_keypad: true,
                win32_input_mode: true,
                reverse_video: true,
            })
            .unwrap();
        assert!(shadow.bracketed_paste());
        assert!(shadow.alt_screen());
        assert!(
            shadow.mouse_protocol_active(),
            "any reporting level must register as active"
        );
        assert!(shadow.application_cursor());
        assert_eq!(
            shadow.modify_other_keys(),
            ModifyOtherKeys::Level2,
            "the encoder reads this to disambiguate Shift+Enter"
        );
        assert!(
            shadow.application_keypad(),
            "the encoder reads this to switch the numpad to SS3"
        );
        assert!(
            shadow.win32_input_mode(),
            "the encoder reads this to switch to win32-input records"
        );
        assert!(
            shadow.reverse_video(),
            "the renderer reads this to XOR the screen-wide fg/bg swap"
        );
        // Pins: the next ModeFlags clears the mirror.
        shadow
            .apply(&GridMsg::ModeFlags {
                bracketed_paste: false,
                alt_screen: false,
                mouse_protocol: MouseProtocol::Off,
                application_cursor: false,
                modify_other_keys: ModifyOtherKeys::Off,
                application_keypad: false,
                win32_input_mode: false,
                reverse_video: false,
            })
            .unwrap();
        assert!(!shadow.bracketed_paste());
        assert!(!shadow.alt_screen());
        assert!(!shadow.mouse_protocol_active());
        assert!(!shadow.application_cursor());
        assert_eq!(shadow.modify_other_keys(), ModifyOtherKeys::Off);
        assert!(!shadow.application_keypad());
        assert!(!shadow.win32_input_mode());
        assert!(!shadow.reverse_video());
    }

    /// Pins: without the mirror `viewport_at_bottom` would always read
    /// true and a reattach into a paused-scrollback session would snap
    /// the user back to live.
    #[test]
    fn viewport_state_message_updates_shadow_mirror() {
        let mut shadow = ShadowScreen::new(24, 80);
        assert_eq!(shadow.viewport(), 0);
        assert_eq!(
            shadow.viewport_max(),
            24,
            "fresh shadow's max defaults to the live row count",
        );
        assert!(shadow.viewport_at_bottom());
        shadow
            .apply(&GridMsg::ViewportState {
                lines_from_bottom: 7,
                max: 1024,
            })
            .unwrap();
        assert_eq!(shadow.viewport(), 7);
        assert_eq!(shadow.viewport_max(), 1024);
        assert!(
            !shadow.viewport_at_bottom(),
            "viewport=7 must flip the at-bottom flag false",
        );
        shadow
            .apply(&GridMsg::ViewportState {
                lines_from_bottom: 0,
                max: 1024,
            })
            .unwrap();
        assert!(shadow.viewport_at_bottom());
    }

    #[test]
    fn kitty_kbd_flags_message_updates_shadow_mirror() {
        let mut shadow = ShadowScreen::new(1, 4);
        assert_eq!(shadow.kitty_kbd_flags(), KittyKbdFlags::empty());
        shadow
            .apply(&GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::DISAMBIGUATE,
            })
            .unwrap();
        assert_eq!(shadow.kitty_kbd_flags(), KittyKbdFlags::DISAMBIGUATE);
        shadow
            .apply(&GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::empty(),
            })
            .unwrap();
        assert_eq!(shadow.kitty_kbd_flags(), KittyKbdFlags::empty());
    }

    #[test]
    fn hyperlink_messages_register_in_id_order() {
        use std::num::NonZeroU16;
        let mut shadow = ShadowScreen::new(1, 4);
        shadow
            .apply(&GridMsg::Hyperlink {
                id: 1,
                anchor: Some("a".into()),
                uri: "https://e.test/1".into(),
            })
            .unwrap();
        shadow
            .apply(&GridMsg::Hyperlink {
                id: 2,
                anchor: None,
                uri: "https://e.test/2".into(),
            })
            .unwrap();
        let one = shadow.hyperlink(NonZeroU16::new(1).unwrap()).unwrap();
        let two = shadow.hyperlink(NonZeroU16::new(2).unwrap()).unwrap();
        assert_eq!(one.uri, "https://e.test/1");
        assert_eq!(one.id.as_ref().map(LinkText::as_str), Some("a"));
        assert_eq!(two.uri, "https://e.test/2");
        assert_eq!(two.id, None);
    }

    /// Pins: the admitted row index is the protocol-wide
    /// `MAX_GRID_ROWS`, not a separate shadow ceiling.
    #[test]
    fn the_last_admitted_row_is_accepted_and_one_past_it_is_refused() {
        let mut shadow = ShadowScreen::new(1, 1);
        let body = || RowPayload(packed_row(&[Cell::default()], &StyleTable::new()));
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(MAX_GRID_ROWS - 1, body())],
            })
            .expect("the last admitted row grows the shadow");
        assert_eq!(shadow.screen().rows(), MAX_GRID_ROWS);
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(MAX_GRID_ROWS, body())],
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::RowIndex { row } if row == MAX_GRID_ROWS));
    }

    #[test]
    fn a_row_wider_than_the_admitted_grid_is_refused_before_it_allocates() {
        let mut shadow = ShadowScreen::new(1, 4);
        let widest: Vec<Cell> = vec![Cell::default(); usize::from(MAX_GRID_COLS)];
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&widest, &StyleTable::new())))],
            })
            .expect("the widest admitted row is accepted");
        assert_eq!(shadow.screen().cols(), MAX_GRID_COLS);

        // `encode_row` refuses the over-wide row too, so the count is
        // patched into a valid body.
        let mut body = packed_row(&[Cell::default()], &StyleTable::new());
        body[2..4].copy_from_slice(&(MAX_GRID_COLS + 1).to_le_bytes());
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(body))],
            })
            .unwrap_err();
        assert!(matches!(
            err,
            ShadowError::Row(RowCodecError::OverLimit { field: "cols", .. })
        ));
    }

    #[test]
    fn a_sized_cell_outside_the_decoded_row_is_refused() {
        let mut shadow = ShadowScreen::new(1, 4);
        let sizing = Sizing::new(2, 0, 0, 0, VAlign::Top, HAlign::Left).unwrap();
        let cells = vec![Cell::default(); 4];
        let packed = |sized: &[(u16, Sizing)]| {
            RowPayload(
                encode_row(
                    RowEncode {
                        cells: &cells,
                        pad_to: cells.len(),
                        sized_cells: sized,
                        soft_wrap_continued: false,
                    },
                    &StyleTable::new(),
                )
                .unwrap(),
            )
        };
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, packed(&[(3, sizing)]))],
            })
            .expect("the row's last column may carry sizing");
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, packed(&[(4, sizing)]))],
            })
            .unwrap_err();
        assert!(matches!(
            err,
            ShadowError::Row(RowCodecError::SizedCellColumn { col: 4, .. })
        ));
    }

    #[test]
    fn scroll_regions_and_counts_outside_the_grid_are_refused() {
        let mut shadow = ShadowScreen::new(4, 4);
        let scrolled = |top, bottom, n_rows| GridMsg::Scrolled {
            region_top: top,
            region_bottom: bottom,
            n_rows,
            direction: ScrollDirection::Up,
        };
        shadow
            .apply(&scrolled(0, 3, 4))
            .expect("the whole grid scrolled by its own height");
        for (msg, what) in [
            (scrolled(0, 4, 1), "one row past the bottom"),
            (scrolled(2, 1, 1), "a reversed region"),
        ] {
            assert!(
                matches!(
                    shadow.apply(&msg).unwrap_err(),
                    ShadowError::ScrollRegion { .. }
                ),
                "{what} must be refused",
            );
        }
        for (msg, what) in [
            (scrolled(0, 3, 0), "a zero scroll count"),
            (scrolled(0, 3, 5), "a count past the region height"),
        ] {
            assert!(
                matches!(
                    shadow.apply(&msg).unwrap_err(),
                    ShadowError::ScrollCount { .. }
                ),
                "{what} must be refused",
            );
        }
    }

    /// Pins: an optimistic resize the daemon overrules leaves the
    /// authoritative cursor standing. The correction carries no
    /// `CursorState` when the daemon's own cursor has not moved, so a
    /// position clamped by the local shrink would otherwise be final.
    #[test]
    fn an_overruled_local_shrink_restores_the_authoritative_cursor() {
        for visible in [true, false] {
            let mut shadow = ShadowScreen::new(24, 80);
            shadow
                .apply(&GridMsg::CursorState {
                    row: 20,
                    col: 70,
                    visible,
                    style: CursorStyle::Bar,
                    blink: false,
                })
                .expect("the coordinate is inside the announced grid");
            shadow.local_resize(10, 20);

            shadow
                .apply(&GridMsg::Size {
                    dims: GridDims {
                        rows: 24,
                        cols: 80,
                        pixel_w: 0,
                        pixel_h: 0,
                    },
                })
                .expect("the daemon overrules the local resize");
            let cur = shadow.screen().cursor();
            assert_eq!((cur.row, cur.col), (20, 70), "visible = {visible}");
            assert_eq!(cur.visible, visible);
            assert_eq!(shadow.screen().cursor_style(), CursorStyle::Bar);
            assert!(!shadow.screen().cursor_blink());
        }
    }

    /// Pins: a coordinate the daemon states while the local shadow is
    /// smaller is kept, not dropped, and lands when the geometry can
    /// hold it.
    #[test]
    fn a_cursor_outside_a_locally_shrunk_shadow_lands_on_the_correction() {
        let mut shadow = ShadowScreen::new(24, 80);
        shadow.local_resize(10, 20);
        shadow
            .apply(&GridMsg::CursorState {
                row: 20,
                col: 70,
                visible: true,
                style: CursorStyle::default(),
                blink: true,
            })
            .expect("the coordinate is inside the announced grid");
        shadow
            .apply(&GridMsg::Size {
                dims: GridDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            })
            .expect("size apply");
        let cur = shadow.screen().cursor();
        assert_eq!((cur.row, cur.col), (20, 70));
        assert!(cur.visible);
    }

    /// Pins: visibility does not relax the coordinate bound.
    #[test]
    fn cursor_coordinates_outside_the_grid_are_refused_visible_or_not() {
        let cursor = |row, col, visible| GridMsg::CursorState {
            row,
            col,
            visible,
            style: CursorStyle::default(),
            blink: true,
        };
        let mut shadow = ShadowScreen::new(5, 10);
        shadow
            .apply(&cursor(4, 9, true))
            .expect("the bottom-right cell is inside the grid");
        for visible in [true, false] {
            for (row, col) in [(5, 0), (0, 10)] {
                assert!(
                    matches!(
                        shadow.apply(&cursor(row, col, visible)).unwrap_err(),
                        ShadowError::Cursor { .. }
                    ),
                    "({row}, {col}) is outside the grid whether or not the cursor is visible",
                );
            }
        }
    }

    /// Pins: the resize race keeps its tolerance in both directions. A
    /// burst composed at a larger geometry grows the shadow; one
    /// composed at a smaller geometry pads it.
    #[test]
    fn a_resize_race_still_applies_in_either_direction() {
        let mut shadow = ShadowScreen::new(2, 4);
        let wide = vec![Cell::default(); 8];
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(3, RowPayload(packed_row(&wide, &StyleTable::new())))],
            })
            .expect("a taller, wider burst grows the shadow");
        assert_eq!((shadow.screen().rows(), shadow.screen().cols()), (4, 8));

        let narrow = vec![Cell::default(); 2];
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&narrow, &StyleTable::new())))],
            })
            .expect("a narrower burst pads instead of failing");
        assert_eq!((shadow.screen().rows(), shadow.screen().cols()), (4, 8));
    }

    #[test]
    fn a_viewport_past_its_own_maximum_is_refused() {
        let mut shadow = ShadowScreen::new(24, 80);
        shadow
            .apply(&GridMsg::ViewportState {
                lines_from_bottom: 1024,
                max: 1024,
            })
            .expect("a viewport at its maximum is legal");
        let err = shadow
            .apply(&GridMsg::ViewportState {
                lines_from_bottom: 1025,
                max: 1024,
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::Viewport { .. }));
    }

    #[test]
    fn a_zero_registry_id_is_refused() {
        let mut shadow = ShadowScreen::new(1, 4);
        assert!(matches!(
            shadow
                .apply(&GridMsg::Hyperlink {
                    id: 0,
                    anchor: None,
                    uri: "https://e.test/1".into(),
                })
                .unwrap_err(),
            ShadowError::RegistryIdZero
        ));
        assert!(matches!(
            shadow
                .apply(&GridMsg::Cluster {
                    id: 0,
                    text: "e\u{0301}".into(),
                })
                .unwrap_err(),
            ShadowError::RegistryIdZero
        ));
    }

    #[test]
    fn an_over_length_registry_entry_is_refused() {
        let mut shadow = ShadowScreen::new(1, 4);
        assert!(matches!(
            shadow
                .apply(&GridMsg::Hyperlink {
                    id: 1,
                    anchor: None,
                    uri: format!("https://e.test/{}", "x".repeat(LinkText::CAP)),
                })
                .unwrap_err(),
            ShadowError::RegistryEntry { id: 1 }
        ));
        assert!(matches!(
            shadow
                .apply(&GridMsg::Cluster {
                    id: 1,
                    text: "x".repeat(ClusterText::CAP + 1),
                })
                .unwrap_err(),
            ShadowError::RegistryEntry { id: 1 }
        ));
    }

    /// Pins: a registry id is installed once per connection. A second
    /// entry under it redefines cells that already drew the first, and
    /// an identical re-send is refused by the same rule rather than
    /// carved out: neither is something the daemon sends.
    #[test]
    fn a_registry_id_the_peer_sends_twice_is_refused() {
        let mut shadow = ShadowScreen::new(1, 4);
        let hyperlink = |uri: &str| GridMsg::Hyperlink {
            id: 1,
            anchor: None,
            uri: uri.into(),
        };
        shadow.apply(&hyperlink("https://e.test/1")).expect("first");
        for (msg, what) in [
            (hyperlink("https://e.test/2"), "a conflicting entry"),
            (hyperlink("https://e.test/1"), "an identical re-send"),
        ] {
            assert!(
                matches!(
                    shadow.apply(&msg).unwrap_err(),
                    ShadowError::RegistryIdReused { id: 1 }
                ),
                "{what} must not redefine hyperlink 1",
            );
        }
        assert_eq!(
            shadow.hyperlink(NonZeroU16::new(1).unwrap()).unwrap().uri,
            "https://e.test/1",
            "the entry the rows were drawn against stands",
        );

        let cluster = |text: &str| GridMsg::Cluster {
            id: 1,
            text: text.into(),
        };
        shadow.apply(&cluster("e\u{0301}")).expect("first");
        for (msg, what) in [
            (cluster("a\u{0301}"), "a conflicting entry"),
            (cluster("e\u{0301}"), "an identical re-send"),
        ] {
            assert!(
                matches!(
                    shadow.apply(&msg).unwrap_err(),
                    ShadowError::RegistryIdReused { id: 1 }
                ),
                "{what} must not redefine cluster 1",
            );
        }
        assert_eq!(
            shadow.cluster_str(NonZeroU32::new(1).unwrap()),
            Some("e\u{0301}"),
        );
    }

    #[test]
    fn a_cluster_id_at_the_table_cap_installs_and_one_past_it_is_refused() {
        use felis_grid::CLUSTER_TABLE_CAP;

        let mut shadow = ShadowScreen::new(1, 4);
        let at_cap = u32::try_from(CLUSTER_TABLE_CAP).unwrap();
        shadow
            .apply(&GridMsg::Cluster {
                id: at_cap,
                text: "a".into(),
            })
            .expect("the last handle the table holds");
        assert_eq!(
            shadow.cluster_str(NonZeroU32::new(at_cap).unwrap()),
            Some("a")
        );
        let err = shadow
            .apply(&GridMsg::Cluster {
                id: at_cap + 1,
                text: "b".into(),
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::RegistryEntry { .. }));
    }

    /// Pins: installing by peer-assigned id skips the interner, where
    /// the daemon charges the `LinkTable` byte budget, so the install
    /// path must charge it too, and a peer past the budget is a peer
    /// the mirror cannot follow.
    #[test]
    fn a_flood_of_hyperlinks_from_a_peer_fails_at_the_table_budget() {
        use felis_grid::LINK_TABLE_BYTE_CAP;

        let mut shadow = ShadowScreen::new(1, 4);
        let uri = format!("https://e.test/{}", "u".repeat(LinkText::CAP - 20));
        // One id past what the budget holds, however the per-entry
        // charge rounds.
        let sent = u16::try_from(LINK_TABLE_BYTE_CAP / uri.len() + 2).unwrap();
        let mut refused = None;
        for id in 1..=sent {
            if let Err(err) = shadow.apply(&GridMsg::Hyperlink {
                id,
                anchor: None,
                uri: uri.clone(),
            }) {
                refused = Some((id, err));
                break;
            }
        }
        let (id, err) = refused.expect("the flood must not fit the budget");
        assert!(matches!(err, ShadowError::RegistryEntry { .. }));
        assert!(id > 1, "the entries below the budget must still install");
        assert!(shadow.hyperlink(NonZeroU16::new(1).unwrap()).is_some());
    }

    /// Pins: the registry-before-row order is an invariant, not a
    /// preference. A row naming an entry that never arrived ends the
    /// attachment instead of drawing a hole.
    #[test]
    fn a_row_naming_an_unarrived_registry_entry_ends_the_attachment() {
        let mut shadow = ShadowScreen::new(1, 2);
        let cluster = NonZeroU32::new(4).unwrap();
        let row = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'x'),
                ..Cell::default()
            },
            Cell {
                grapheme: Grapheme::Cluster(cluster),
                ..Cell::default()
            },
        ];
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&row, &StyleTable::new())))],
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::UnresolvedCluster { id: 4 }));

        let link = NonZeroU16::new(2).unwrap();
        let row = vec![
            Cell {
                grapheme: Grapheme::Ascii(b'x'),
                link: Some(link),
                ..Cell::default()
            },
            Cell::default(),
        ];
        let err = shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&row, &StyleTable::new())))],
            })
            .unwrap_err();
        assert!(matches!(err, ShadowError::UnresolvedHyperlink { id: 2 }));
    }

    /// Pins: sparse delivery stays legal. Holes at ids the row does not
    /// name are what "sparse" means.
    #[test]
    fn a_row_applies_when_its_entries_arrived_first_despite_unrelated_holes() {
        let mut shadow = ShadowScreen::new(1, 2);
        let cluster = NonZeroU32::new(4).unwrap();
        let link = NonZeroU16::new(2).unwrap();
        shadow
            .apply(&GridMsg::Cluster {
                id: cluster.get(),
                text: "e\u{0301}".into(),
            })
            .expect("cluster apply");
        shadow
            .apply(&GridMsg::Hyperlink {
                id: link.get(),
                anchor: None,
                uri: "https://e.test/2".into(),
            })
            .expect("hyperlink apply");
        let row = vec![
            Cell {
                grapheme: Grapheme::Cluster(cluster),
                link: Some(link),
                ..Cell::default()
            },
            Cell::default(),
        ];
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(packed_row(&row, &StyleTable::new())))],
            })
            .expect("ids 1..=3 are holes the row does not name");
        assert_eq!(shadow.cluster_str(cluster), Some("e\u{0301}"));
        assert_eq!(shadow.screen().cell(0, 0).unwrap().link, Some(link));
    }
}
