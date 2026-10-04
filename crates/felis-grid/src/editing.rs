//! CSI / cursor-movement / scrolling / editing operations on `Grid`.

use super::{
    AttrFlags, Attributes, Cell, Color, Cursor, CursorStyle, ErasedRange, Grapheme, Grid,
    MouseEncoding, MouseProtocol, PtyEffect, SavedCursor, ScrollDirection, ScrollOp, Sink, StyleId,
    SyncOutput, TITLE_STACK_LIMIT, UnderlineStyle, ascii_to_hex, copy_within_cells,
    default_tab_stops, encode_sgr_color, encode_sgr_underline_color, hex_to_ascii,
    known_modifiable_dec_mode, permanently_reset_ansi, permanently_reset_dec_mode,
    permanently_set_dec_mode, xtgettcap_value,
};
use crate::uax29::{is_extended_pictographic, is_grapheme_extend};

/// Fitzpatrick modifiers (U+1F3FB..=U+1F3FF) are UAX#29 Extend, yet
/// unicode-width reports them as width-2, so the width-0 fold gate
/// misses them.
pub(crate) const fn is_emoji_modifier(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

/// GB11's left side, `\p{ExtPict} Extend* ZWJ`. The fold admits any
/// zero-width scalar, ZWSP among them, which UAX#29 treats as a break,
/// so the walk reads Extend itself. A bidi override in the text is one
/// held pending from before the base, since one arriving after it
/// blocks the join through `PendingBidi::after_base`, so it is skipped.
fn ends_in_pictographic_joiner(cluster: &str) -> bool {
    let Some(rest) = cluster.strip_suffix('\u{200D}') else {
        return false;
    };
    rest.chars()
        .rev()
        .find(|&c| !(is_grapheme_extend(c) || crate::bidi::is_override(c)))
        .is_some_and(is_extended_pictographic)
}

/// Regional indicators (U+1F1E6..=U+1F1FF) pair into a flag (UAX#29
/// GB12/GB13) but are width-1 alone; see
/// [`Grid::prev_is_lone_regional_indicator`].
pub(crate) const fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

/// Bidi overrides that found no cell to fold backward into, waiting for
/// the next character printed (REQ-909). One override is enough for the
/// marker, so the queue is short; past it more overrides are dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PendingBidi {
    chars: [char; Self::CAP],
    len: u8,
    /// Set when an override arrives after the base, folded or not. In
    /// input order it is a UAX#29 break (GB4/GB5) ahead of any ZWJ, but
    /// folded it looks the same as one held here until the base, which
    /// is no break. Only a new base clears it.
    pub(crate) after_base: bool,
}

impl PendingBidi {
    const CAP: usize = 8;

    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Keeps `after_base`: a control that drops the queue leaves the
    /// break between the base and a later joiner in place.
    pub(crate) fn discard(&mut self) {
        *self = Self {
            after_base: self.after_base,
            ..Self::default()
        };
    }

    fn push(&mut self, c: char) {
        if let Some(slot) = self.chars.get_mut(usize::from(self.len)) {
            *slot = c;
            self.len += 1;
        }
    }

    fn as_slice(&self) -> &[char] {
        &self.chars[..usize::from(self.len)]
    }
}

#[derive(Clone, Copy)]
enum HDir {
    Left,
    Right,
}

/// `[Pt, Pl, Pb, Pr]` as they arrive on the wire: 1-based, inclusive,
/// with `0` meaning "default" ([`Grid::clamp_rectangle`] resolves it).
#[derive(Clone, Copy)]
pub(crate) struct RectParams {
    pt: u16,
    pl: u16,
    pb: u16,
    pr: u16,
}

impl RectParams {
    const UNSET: Self = Self {
        pt: 0,
        pl: 0,
        pb: 0,
        pr: 0,
    };
}

/// 0-based, inclusive on all four edges, origin-adjusted and clamped.
#[derive(Clone, Copy)]
pub(crate) struct GridRect {
    top: u16,
    left: u16,
    bottom: u16,
    right: u16,
}

/// DECOM offset applied to rectangle endpoints, in 0-based cells.
#[derive(Clone, Copy)]
struct RectOrigin {
    row: u16,
    col: u16,
}

/// Colors and the DECSCA / SPA protection bits are outside both
/// operations, so a selector that would touch them is dropped.
const RECT_ATTRS: AttrFlags = AttrFlags::BOLD
    .union(AttrFlags::FAINT)
    .union(AttrFlags::ITALIC)
    .union(AttrFlags::UNDERLINE)
    .union(AttrFlags::BLINK)
    .union(AttrFlags::REVERSE)
    .union(AttrFlags::CONCEAL)
    .union(AttrFlags::STRIKETHROUGH);

#[derive(Clone, Copy)]
struct AttrEdit {
    set: AttrFlags,
    clear: AttrFlags,
    underline_style: Option<UnderlineStyle>,
}

impl AttrEdit {
    /// `subparams` is already shifted to align with `params`. An
    /// empty tail reads as `0`, xterm's default, which turns every
    /// reachable attribute off.
    fn from_params(params: &[u16], subparams: u32) -> Self {
        let mut edit = Self {
            set: AttrFlags::empty(),
            clear: AttrFlags::empty(),
            underline_style: None,
        };
        if params.is_empty() {
            edit.clear = RECT_ATTRS;
            edit.underline_style = Some(UnderlineStyle::default());
            return edit;
        }
        let mut i = 0;
        while i < params.len() {
            let mut consumed = 1;
            match params[i] {
                0 => {
                    edit.set = AttrFlags::empty();
                    edit.clear = RECT_ATTRS;
                    edit.underline_style = Some(UnderlineStyle::default());
                }
                1 => edit.turn_on(AttrFlags::BOLD),
                2 => edit.turn_on(AttrFlags::FAINT),
                3 => edit.turn_on(AttrFlags::ITALIC),
                4 => {
                    if crate::sgr::next_is_subparam(subparams, i) {
                        consumed = 2;
                        let shape = params.get(i + 1).copied().unwrap_or(0);
                        if shape == 0 {
                            edit.turn_off(AttrFlags::UNDERLINE);
                            edit.underline_style = Some(UnderlineStyle::default());
                        } else {
                            edit.turn_on(AttrFlags::UNDERLINE);
                            edit.underline_style = Some(underline_shape(shape));
                        }
                    } else {
                        edit.turn_on(AttrFlags::UNDERLINE);
                        edit.underline_style = Some(UnderlineStyle::Single);
                    }
                }
                5 | 6 => edit.turn_on(AttrFlags::BLINK),
                7 => edit.turn_on(AttrFlags::REVERSE),
                8 => edit.turn_on(AttrFlags::CONCEAL),
                9 => edit.turn_on(AttrFlags::STRIKETHROUGH),
                21 => {
                    edit.turn_on(AttrFlags::UNDERLINE);
                    edit.underline_style = Some(UnderlineStyle::Double);
                }
                22 => edit.turn_off(AttrFlags::BOLD | AttrFlags::FAINT),
                23 => edit.turn_off(AttrFlags::ITALIC),
                24 => {
                    edit.turn_off(AttrFlags::UNDERLINE);
                    edit.underline_style = Some(UnderlineStyle::default());
                }
                25 => edit.turn_off(AttrFlags::BLINK),
                27 => edit.turn_off(AttrFlags::REVERSE),
                28 => edit.turn_off(AttrFlags::CONCEAL),
                29 => edit.turn_off(AttrFlags::STRIKETHROUGH),
                _ => {}
            }
            i += consumed;
        }
        edit
    }

    fn turn_on(&mut self, flags: AttrFlags) {
        self.set.insert(flags);
        self.clear.remove(flags);
    }

    fn turn_off(&mut self, flags: AttrFlags) {
        self.clear.insert(flags);
        self.set.remove(flags);
    }
}

const fn underline_shape(code: u16) -> UnderlineStyle {
    match code {
        2 => UnderlineStyle::Double,
        3 => UnderlineStyle::Curly,
        4 => UnderlineStyle::Dotted,
        5 => UnderlineStyle::Dashed,
        _ => UnderlineStyle::Single,
    }
}

/// An empty list, like an explicit `0`, reverses everything reachable.
fn reverse_attr_masks(params: &[u16], subparams: u32) -> Vec<AttrFlags> {
    if params.is_empty() {
        return vec![RECT_ATTRS];
    }
    let mut masks = Vec::with_capacity(params.len());
    let mut i = 0;
    while i < params.len() {
        let mut consumed = 1;
        let p = params[i];
        if p == 4 && crate::sgr::next_is_subparam(subparams, i) {
            consumed = 2;
        }
        let flags = match p {
            0 => RECT_ATTRS,
            1 => AttrFlags::BOLD,
            2 => AttrFlags::FAINT,
            22 => AttrFlags::BOLD.union(AttrFlags::FAINT),
            3 | 23 => AttrFlags::ITALIC,
            4 | 21 | 24 => AttrFlags::UNDERLINE,
            5 | 6 | 25 => AttrFlags::BLINK,
            7 | 27 => AttrFlags::REVERSE,
            8 | 28 => AttrFlags::CONCEAL,
            9 | 29 => AttrFlags::STRIKETHROUGH,
            _ => AttrFlags::empty(),
        };
        // One XOR per selector, not a union: xterm's ScrnMarkRectangle
        // applies the list one parameter at a time, so naming an
        // attribute twice cancels out.
        masks.push(flags);
        i += consumed;
    }
    masks
}

/// Not `fill(Cell::default())`: that stores the tag byte and the tail
/// around the undefined bytes between them, which keeps LLVM from
/// lowering the loop to a memset. Copying a static's bytes lowers to
/// memcpy.
pub(crate) fn blank_cells(cells: &mut [Cell]) {
    static BLANKS: [Cell; 64] = [Cell::BLANK; 64];
    for chunk in cells.chunks_mut(BLANKS.len()) {
        chunk.copy_from_slice(&BLANKS[..chunk.len()]);
    }
}

impl Grid {
    /// `false` when `g` wrote no cell of its own: it folded into a
    /// cluster, waits as a pending bidi override, or was dropped.
    pub(crate) fn put_grapheme(&mut self, g: Grapheme) -> bool {
        // Valid only for the single print right after a ZWJ fold;
        // taking it also clears a stale arm.
        let zwj_armed = std::mem::take(&mut self.zwj_pending);
        let width = self.screen.grapheme_width(g);
        // UAX#29 GB9 / GB11 extension folds into the previous cell's
        // cluster (mode 2027, REQ-602).
        if let Grapheme::Char(c) = g
            && self.extends_previous_grapheme(c, width, zwj_armed)
        {
            if crate::bidi::is_override(c) {
                // Set even when the fold is refused: the override still
                // came between the base and any later joiner.
                self.pending_bidi.after_base = true;
                if !self.append_combining_mark(c) {
                    self.pending_bidi.push(c);
                }
            } else {
                self.append_combining_mark(c);
            }
            return false;
        }
        // A 1-column grid cannot host a width-2 glyph: wrapping would
        // bounce forever.
        if width == 2 && self.screen.cols < 2 {
            return false;
        }
        // A wide glyph with no room wraps first; not xterm's strict
        // model, but every modern terminal (kitty, foot, wezterm) wraps
        // before squashing. Under DECLRMM with the cursor at or left of
        // `right_margin`, autowrap fires at that margin (esctest's
        // `test_DECSET_DECAWM_OnRespectsLeftRightMargin`).
        let print_right_edge = self.print_right_edge_for_cursor();
        let print_left_edge = self.print_left_edge_for_wrap();
        let needs_wrap = self.screen.cursor.pending_wrap
            || (width == 2 && self.screen.cursor.col > print_right_edge.saturating_sub(1));
        if needs_wrap {
            if self.autowrap {
                self.line_feed();
                self.screen.cursor.col = print_left_edge;
                self.screen.cursor.pending_wrap = false;
                // Record the autowrap so search / selection can stitch
                // the pair (docs/explanation/data-model/scrollback.md
                // "Soft wrap").
                self.screen.set_soft_wrap(self.screen.cursor.row, true);
            } else {
                self.screen.cursor.pending_wrap = false;
                if width == 2 && self.screen.cursor.col > print_right_edge.saturating_sub(1) {
                    self.screen.cursor.col = print_right_edge.saturating_sub(1);
                }
            }
        }
        // IRM runs before the wide-partner eviction so the eviction
        // only has to clean up partners the shift orphaned at the row
        // edges.
        if self.insert_mode {
            let cols = usize::from(self.screen.cols);
            // Under DECLRMM the shift truncates at the right margin
            // (esctest's `test_SM_IRM_TruncatesAtRightMargin`).
            let right_edge =
                if self.left_right_margin_mode && self.screen.cursor.col <= self.margins.right {
                    usize::from(self.margins.right) + 1
                } else {
                    cols
                };
            let row_start = self.screen.idx(self.screen.cursor.row, 0);
            let cursor = usize::from(self.screen.cursor.col);
            let n = usize::from(width);
            if cursor + n <= right_edge && cursor + n < right_edge {
                let row = self.screen.cursor.row;
                self.materialize_row_tail(row);
                for seam in [cursor, right_edge - n, right_edge] {
                    self.erase_pair_across(row, seam);
                }
                let src_start = row_start + cursor;
                let src_end = row_start + right_edge - n;
                let dst_start = row_start + cursor + n;
                if src_start < src_end {
                    copy_within_cells(&mut self.screen.cells, src_start..src_end, dst_start);
                    self.screen.occ_bump_phys(
                        row_start / cols,
                        u16::try_from(right_edge).unwrap_or(u16::MAX),
                    );
                }
            }
        }
        // Erase any foreign sized run before printing: Kitty text-sizing spec
        // requires erasing the entire character if any of its cells are modified.
        // Runs before `evict_wide_partner` so the latter sees post-clear cells.
        if self.screen.has_sized_cells {
            self.clear_foreign_sized_run(self.screen.cursor.row, self.screen.cursor.col);
            if width == 2 && self.screen.cursor.col + 1 < self.screen.cols {
                self.clear_foreign_sized_run(self.screen.cursor.row, self.screen.cursor.col + 1);
            }
        }
        // Otherwise typing over a wide glyph's right half leaves an
        // orphan Char with no Spacer and the renderer double-draws.
        let row = self.screen.cursor.row;
        // Resolve the physical row once: `idx()`'s lookup and a
        // `row_base / cols` division per glyph showed as +12% on the
        // unicode bench.
        let phys = self.screen.phys_row(row);
        let row_base = phys * usize::from(self.screen.cols);
        let col = usize::from(self.screen.cursor.col);
        let occ = usize::from(self.screen.occupancy[phys]);
        // Gap-blank a cursor addressed past the watermark before the
        // write raises it over a recycled scroll row's stale tail.
        if col > occ {
            self.fill_leading_gap(phys, col);
        }
        // A live wide glyph occupies columns inside `[0, occupancy)`,
        // so only a write there can orphan a half; at or past the
        // watermark the cell is blank or a clipped stale tenant.
        // Forward `cat` writes land exactly at the watermark, so this
        // elides both probes (~13% of the CJK parse).
        if col < occ {
            self.evict_wide_partner_at(row, row_base, col);
        }
        if width == 2 && col + 1 < occ {
            self.evict_wide_partner_at(row, row_base, col + 1);
        }
        let idx = row_base + col;
        // xterm scopes REP to the last graphic print, cleared on any
        // control dispatch.
        self.last_printed = Some(g);
        // The sizing stamp rides in the cell
        // (docs/explanation/data-model/grid-and-cells.md), so
        // re-printing over a sized cell clears its handle for free.
        self.screen.cells[idx] = Cell {
            grapheme: g,
            style: self.pen_style,
            link: self.current_link,
            sizing: self.current_sizing_handle,
        };
        if self.current_sizing_handle.is_some() {
            self.screen.has_sized_cells = true;
        }
        self.screen.occ_bump_phys(
            phys,
            self.screen.cursor.col.saturating_add(u16::from(width)),
        );
        self.screen.damage.mark(self.screen.cursor.row.into());
        self.pending_bidi.after_base = false;
        if !self.pending_bidi.is_empty() {
            let pending = std::mem::take(&mut self.pending_bidi);
            self.extend_cluster(idx, pending.as_slice());
        }
        if width == 2 {
            let spacer_idx = idx + 1;
            self.screen.cells[spacer_idx] = Cell {
                grapheme: Grapheme::Spacer,
                style: self.pen_style,
                link: self.current_link,
                sizing: self.current_sizing_handle,
            };
            // The just-printed wide glyph may have crossed the right
            // margin, so the next print wraps from there.
            let post_edge = self.print_right_edge_for_cursor();
            if self.screen.cursor.col + 1 >= post_edge {
                self.screen.cursor.col = post_edge;
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col += 2;
            }
        } else {
            let post_edge = self.print_right_edge_for_cursor();
            if self.screen.cursor.col >= post_edge {
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col += 1;
            }
        }
        true
    }

    /// Under DECLRMM with the cursor at or left of `right_margin`, that
    /// margin; otherwise the screen's right edge.
    pub(crate) const fn print_right_edge_for_cursor(&self) -> u16 {
        if self.left_right_margin_mode && self.screen.cursor.col <= self.margins.right {
            self.margins.right
        } else {
            self.screen.cols.saturating_sub(1)
        }
    }

    /// Always `left_margin` under DECLRMM: the autowrap path is by
    /// definition inside the margin, so `left_edge_for_line_start`'s
    /// origin-mode branch does not apply.
    pub(crate) const fn print_left_edge_for_wrap(&self) -> u16 {
        if self.left_right_margin_mode {
            self.margins.left
        } else {
            0
        }
    }

    /// `false` when the mark found no cell to join, or the cluster would
    /// pass its caps.
    pub(crate) fn append_combining_mark(&mut self, mark: char) -> bool {
        // Re-armed below only if this mark is itself a ZWJ.
        self.zwj_pending = false;
        let Some((row, owner_col)) = self.combining_owner() else {
            return false;
        };
        let owner_idx = self.screen.idx(row, owner_col);
        if !self.extend_cluster(owner_idx, &[mark]) {
            return false;
        }
        // Any cursor move clears the anchor, so a live one is the
        // cell just extended: REP must repeat the whole cluster.
        if self.last_printed.is_some() {
            self.last_printed = Some(self.screen.cells[owner_idx].grapheme);
        }
        // unicode-width reports base + VS16 (❤️) or a keycap
        // sequence (1️⃣) as 2 cells, but the base was placed narrow
        // before the selector arrived.
        self.widen_cluster_if_needed(row, owner_col);
        if mark == '\u{200D}' {
            self.zwj_pending = true;
        }
        true
    }

    /// Appends `marks` to the text of the cell at `idx`, which must be on
    /// the cursor row. Refused past `ClusterText::CAP` or at
    /// `CLUSTER_TABLE_CAP`: the cell keeps its text rather than being
    /// truncated.
    fn extend_cluster(&mut self, idx: usize, marks: &[char]) -> bool {
        // `mem::take` lends the scratch buffer out so the immutable
        // `cluster_str` borrow and the `&mut self` intern call don't
        // overlap; it is moved back on every return path so its grown
        // capacity persists across prints.
        let mut scratch = std::mem::take(&mut self.screen.cluster_table.scratch);
        scratch.clear();
        match self.screen.cells[idx].grapheme {
            Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => {
                self.screen.cluster_table.scratch = scratch;
                return false;
            }
            Grapheme::Ascii(b) => scratch.push(b as char),
            Grapheme::Char(c) => scratch.push(c),
            Grapheme::Cluster(existing) => {
                scratch.push_str(self.screen.cluster_str(existing).unwrap_or(""));
            }
        }
        scratch.extend(marks);
        let id = self.intern_cluster(&scratch);
        self.screen.cluster_table.scratch = scratch;
        let Some(id) = id else {
            return false;
        };
        self.screen.cells[idx].grapheme = Grapheme::Cluster(id);
        self.screen.damage.mark(self.screen.cursor.row.into());
        true
    }

    /// Four UAX#29 cases fold: a zero-width mark (GB9), an emoji
    /// modifier (Extend yet width-2, GB9), a pictographic after a
    /// pictographic and ZWJ (GB11), and the second regional indicator
    /// of a flag (GB12/GB13).
    fn extends_previous_grapheme(&self, c: char, width: u8, zwj_armed: bool) -> bool {
        width == 0
            || is_emoji_modifier(c)
            || (zwj_armed && self.zwj_continues_into_prev(c))
            || (is_regional_indicator(c) && self.prev_is_lone_regional_indicator())
    }

    /// A folded pair is a `Cluster`, not a bare `Char`, so a third
    /// indicator starts a new flag: the GB12/GB13 even/odd pairing. The
    /// one cluster that still counts as lone is an indicator carrying
    /// the bidi overrides that preceded it.
    fn prev_is_lone_regional_indicator(&self) -> bool {
        let Some((row, owner_col)) = self.combining_owner() else {
            return false;
        };
        match self.screen.cells[self.screen.idx(row, owner_col)].grapheme {
            Grapheme::Char(c) => is_regional_indicator(c),
            Grapheme::Cluster(id) => self.screen.cluster_str(id).is_some_and(|s| {
                let mut chars = s.chars();
                chars.next().is_some_and(is_regional_indicator)
                    && chars.all(crate::bidi::is_override)
            }),
            _ => false,
        }
    }

    /// A base placed at width 1 (❤, a keycap digit) becomes width 2
    /// once a variation selector / keycap mark joins it. No-op when the
    /// trailing cell is occupied, off-screen or past the right margin: a
    /// retroactive widen must neither clobber real content nor overflow
    /// the row.
    fn widen_cluster_if_needed(&mut self, row: u16, owner_col: u16) {
        let owner_idx = self.screen.idx(row, owner_col);
        if self
            .screen
            .grapheme_width(self.screen.cells[owner_idx].grapheme)
            != 2
        {
            return;
        }
        let next_col = owner_col + 1;
        if next_col >= self.screen.cols
            || (self.left_right_margin_mode && owner_col == self.margins.right)
        {
            return;
        }
        // Through `cell`, which reads a recycled row's stale tenant as
        // the blank it is.
        if self
            .screen
            .cell(row, next_col)
            .is_some_and(|c| !matches!(c.grapheme, Grapheme::Empty))
        {
            return;
        }
        let next_idx = self.screen.idx(row, next_col);
        let owner = self.screen.cells[owner_idx];
        self.screen.cells[next_idx] = Cell {
            grapheme: Grapheme::Spacer,
            style: owner.style,
            link: owner.link,
            sizing: owner.sizing,
        };
        self.screen.occ_bump_idx(next_idx);
        // Advance only when the cursor still parks right after the
        // base; a selector arriving after a cursor move widens in
        // place, matching "a combining mark does not move the cursor".
        if self.screen.cursor.col == next_col {
            let post_edge = self.print_right_edge_for_cursor();
            if next_col >= post_edge {
                self.screen.cursor.col = post_edge;
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col = next_col + 1;
            }
        }
    }

    /// After a width-1 print at the rightmost column the cursor parks
    /// there with `pending_wrap`, so the target is `cursor.col` itself;
    /// after a width-2 print the cursor lands past the Spacer, so a
    /// Spacer resolves to the owner one cell left.
    fn combining_owner(&self) -> Option<(u16, u16)> {
        let row = self.screen.cursor.row;
        let col = if self.screen.cursor.pending_wrap {
            self.screen.cursor.col
        } else if self.screen.cursor.col > 0 {
            self.screen.cursor.col - 1
        } else {
            return None;
        };
        // Past the watermark sits a recycled row's stale tenant, not a
        // cell anyone can see.
        if col >= self.screen.occupancy[self.screen.phys_row(row)] {
            return None;
        }
        let idx = self.screen.idx(row, col);
        let owner_col = if matches!(self.screen.cells[idx].grapheme, Grapheme::Spacer) && col > 0 {
            col - 1
        } else {
            col
        };
        Some((row, owner_col))
    }

    /// The base need not already be width-2: a width-1 dingbat base
    /// (❤️‍🔥, whose heart is width-1 until VS16 or the fire lands) is a
    /// valid target as long as the trailing cell is Spacer or Empty for
    /// `widen_cluster_if_needed` to claim, so the fold never clips a
    /// two-cell glyph or clobbers content to its right.
    fn zwj_continues_into_prev(&self, c: char) -> bool {
        if !is_extended_pictographic(c) || self.pending_bidi.after_base {
            return false;
        }
        let Some((row, owner_col)) = self.combining_owner() else {
            return false;
        };
        let Grapheme::Cluster(id) = self.screen.cells[self.screen.idx(row, owner_col)].grapheme
        else {
            return false;
        };
        if self
            .screen
            .cluster_str(id)
            .is_none_or(|s| !ends_in_pictographic_joiner(s))
        {
            return false;
        }
        owner_col + 1 < self.screen.cols
            && matches!(
                self.screen.cells[self.screen.idx(row, owner_col + 1)].grapheme,
                Grapheme::Spacer | Grapheme::Empty
            )
    }

    /// `row_base` is pre-resolved so `put_grapheme`'s two probes and
    /// its cell writes share one `row_lookup` round trip.
    pub(crate) fn evict_wide_partner_at(&mut self, row: u16, row_base: usize, col: usize) {
        let cols = usize::from(self.screen.cols);
        if col >= cols {
            return;
        }
        let idx = row_base + col;
        let needs_left_evict =
            matches!(self.screen.cells[idx].grapheme, Grapheme::Spacer) && col > 0;
        if needs_left_evict {
            self.screen.cells[idx - 1] = Cell::default();
            self.screen.damage.mark(row.into());
        } else if col + 1 < cols && matches!(self.screen.cells[idx + 1].grapheme, Grapheme::Spacer)
        {
            self.screen.cells[idx + 1] = Cell::default();
            self.screen.damage.mark(row.into());
        }
    }

    /// Blank the gap `[occ..col)` before raising watermark over a recycled tail.
    ///
    /// `#[inline(never)]`: keeping out of line lets callers in the parser loop
    /// hoist `self.screen.cells` accesses; inlining regresses throughput ~30%.
    #[inline(never)]
    pub(crate) fn fill_leading_gap(&mut self, phys: usize, col: usize) {
        let occ = usize::from(self.screen.occupancy[phys]);
        let cols = usize::from(self.screen.cols);
        let base = phys * cols;
        blank_cells(&mut self.screen.cells[base + occ..base + col]);
    }

    /// The cell-shifting editors copy spans that can reach past
    /// the watermark, where a recycled scroll row left undefined bytes;
    /// materializing the tail first keeps them from shifting a stale
    /// line into view.
    pub(crate) fn materialize_row_tail(&mut self, row: u16) {
        let phys = self.screen.phys_row(row);
        let cols = usize::from(self.screen.cols);
        let occ = usize::from(self.screen.occupancy[phys]).min(cols);
        if occ < cols {
            let start = phys * cols;
            self.screen.cells[start + occ..start + cols].fill(Cell::default());
            self.screen.occupancy[phys] = u16::try_from(cols).unwrap_or(u16::MAX);
        }
    }

    /// Widens `[from, to)` so it splits no wide pair. Past the watermark a
    /// recycled row's stale `Spacer` would pull in a live cell, so only
    /// cells below it are probed. A pair shares one protection state, so
    /// a protected-cell check over the result keeps or erases it whole.
    pub(crate) fn pair_aligned_span(&self, row: u16, from: usize, to: usize) -> (usize, usize) {
        let cols = usize::from(self.screen.cols);
        let phys = self.screen.phys_row(row);
        let occ = usize::from(self.screen.occupancy[phys]).min(cols);
        let base = phys * cols;
        let is_spacer =
            |col: usize| matches!(self.screen.cells[base + col].grapheme, Grapheme::Spacer);
        let mut from = from.min(cols);
        let mut to = to.clamp(from, cols);
        if from == to {
            return (from, to);
        }
        if from > 0 && from < occ && is_spacer(from) {
            from -= 1;
        }
        if to < occ && is_spacer(to) {
            to += 1;
        }
        (from, to)
    }

    /// Erases the pair, or the OSC 66 run, straddling the boundary
    /// between `col - 1` and `col`. A cell move runs this on each
    /// boundary it cuts first: a repair after the move could not tell a
    /// cut owner from a cluster whose widen was refused.
    pub(crate) fn erase_pair_across(&mut self, row: u16, col: usize) {
        let cols = usize::from(self.screen.cols);
        let phys = self.screen.phys_row(row);
        let occ = usize::from(self.screen.occupancy[phys]).min(cols);
        if col == 0 || col >= occ {
            return;
        }
        let idx = phys * cols + col;
        let cell = self.screen.cells[idx];
        if cell.sizing.is_some() {
            if let Some(block) = self.sized_block_at(row, u16::try_from(col).unwrap_or(u16::MAX))
                && usize::from(block.left) < col
            {
                self.clear_sized_block(block);
            }
        } else if matches!(cell.grapheme, Grapheme::Spacer) {
            self.screen.cells[idx - 1] = Cell::default();
            self.screen.cells[idx] = Cell::default();
            self.screen.damage.mark(row.into());
        }
    }

    /// Blank the cells `[from, to)` a shift vacated, exactly: the shift
    /// erased the pairs its seams cut, and the stale copies it left in
    /// the span would send a pair probe after a live cell.
    pub(crate) fn range_blank(&mut self, row: u16, from: usize, to: usize) {
        self.range_blank_inner(row, from, to, false);
    }

    /// Widens `[from, to)` so it splits no pair, and skips
    /// `ISO_PROTECTED` (SPA / EPA) cells only: DEC-protected cells
    /// (DECSCA) still blank, matching xterm's "regular erase respects
    /// only ECMA protection" (esctest's `test_ED_respectsISOProtection`
    /// et al.).
    pub(crate) fn range_blank_iso(&mut self, row: u16, from: usize, to: usize) {
        self.range_blank_inner(row, from, to, true);
    }

    pub(crate) fn range_blank_inner(
        &mut self,
        row: u16,
        mut from: usize,
        mut to: usize,
        erase: bool,
    ) {
        let iso_aware = erase && self.screen.style_table.has_iso_protected();
        let row_start = self.screen.idx(row, 0);
        let row_end = row_start + usize::from(self.screen.cols);
        from = from.max(row_start).min(row_end);
        to = to.max(from).min(row_end);
        if erase {
            let (from_col, to_col) = self.pair_aligned_span(row, from - row_start, to - row_start);
            from = row_start + from_col;
            to = row_start + to_col;
        }
        let blank = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: None,
            sizing: None,
        };
        for slot in &mut self.screen.cells[from..to] {
            if iso_aware
                && self
                    .screen
                    .style_table
                    .resolve(slot.style)
                    .flags
                    .contains(AttrFlags::ISO_PROTECTED)
            {
                continue;
            }
            *slot = blank;
        }
        // A pen-colored blank is non-default content, so the watermark
        // must cover it; an erase-to-row-end with the default pen
        // lowers it instead, unless iso-protected cells may have
        // survived inside the range.
        let phys = row_start / usize::from(self.screen.cols);
        if blank == Cell::default() {
            if !iso_aware && to == row_end {
                let new_end = u16::try_from(from - row_start).unwrap_or(u16::MAX);
                let o = &mut self.screen.occupancy[phys];
                if *o > new_end {
                    *o = new_end;
                }
            }
        } else {
            let end = u16::try_from(to - row_start).unwrap_or(u16::MAX);
            self.screen.occ_bump_phys(phys, end);
        }
        // A wholesale row blank resets the soft-wrap record: whatever
        // is printed next did not arrive via that autowrap. Partial
        // erases keep it.
        if from == row_start && to == row_end {
            self.screen.set_soft_wrap(row, false);
        }
    }

    pub(crate) fn line_feed(&mut self) {
        self.screen.cursor.pending_wrap = false;
        if self.screen.cursor.row == self.margins.bottom {
            // xterm suppresses IND / LF / VT / FF entirely (no scroll,
            // no advance) when the cursor is outside the DECSLRM band.
            if self.cursor_outside_left_right() {
                return;
            }
            self.scroll_region_up(1);
        } else if self.screen.cursor.row + 1 < self.screen.rows {
            self.screen.cursor.row += 1;
        }
    }

    pub(crate) fn reverse_index(&mut self) {
        self.screen.cursor.pending_wrap = false;
        if self.screen.cursor.row == self.margins.top {
            // Same DECSLRM gate as `line_feed`.
            if self.cursor_outside_left_right() {
                return;
            }
            self.scroll_region_down(1);
        } else if self.screen.cursor.row > 0 {
            self.screen.cursor.row -= 1;
        }
    }

    /// xterm suppresses the scroll arm of IND / RI / LF / VT / FF / NEL
    /// when the cursor is outside the DECSLRM band.
    pub(crate) const fn cursor_outside_left_right(&self) -> bool {
        if !self.left_right_margin_mode {
            return false;
        }
        self.screen.cursor.col < self.margins.left || self.screen.cursor.col > self.margins.right
    }

    /// Queue the directive for a band the caller has just rotated. The
    /// damage moves with the rows, so a row written before the scroll
    /// stays owed at its new position and the rows it did not touch
    /// ride the directive; the vacated band is marked because the
    /// shadow fills it with default blanks, not the pen's.
    fn queue_band_scroll(&mut self, top: u16, bottom: u16, n: usize, direction: ScrollDirection) {
        self.screen
            .damage
            .shift_band(usize::from(top), usize::from(bottom), n, direction);
        let scroll_seq = self.screen.next_scroll_seq();
        self.pty_effects.push_scrolled(
            ScrollOp {
                region_top: top,
                region_bottom: bottom,
                n_rows: u16::try_from(n).unwrap_or(u16::MAX),
                direction,
            },
            self.screen.geometry_gen(),
            scroll_seq,
        );
    }

    /// Top rows fall into scrollback only when the region is anchored
    /// at row 0 on the primary screen; a sub-region or the alt screen
    /// discards them (xterm / kitty) so alt-screen animations do not
    /// pollute history.
    pub(crate) fn scroll_region_up(&mut self, n: u16) {
        // A narrowed DECLRMM band cannot use the row rotation (it
        // reorders whole physical rows) and skips the scrollback push:
        // partial rows have no scrollback representation (xterm).
        if self.left_right_margin_mode {
            let (left, right_incl) = self.effective_left_right();
            if left != 0 || right_incl + 1 != self.screen.cols {
                self.scroll_subrect_up(n, left, right_incl);
                return;
            }
        }
        let n = n.max(1);
        let region_top = usize::from(self.margins.top);
        let region_bottom = usize::from(self.margins.bottom);
        let region_height = region_bottom - region_top + 1;
        let n = usize::from(n).min(region_height);
        let to_scrollback = self.margins.top == 0 && self.screen.saved_primary.is_none();
        let full_screen = region_top == 0 && region_bottom + 1 == usize::from(self.screen.rows);
        let blank = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: None,
            sizing: None,
        };
        if to_scrollback {
            // The row leaving the viewport top is already the youngest
            // history row in the same storage. This grows `history_len`
            // first, so the mark-prune below sees the current retained
            // depth.
            if full_screen {
                self.scroll_full_screen_into_history(n, &blank);
            } else {
                self.scroll_partial_top_into_history(n, region_bottom, &blank);
            }
            // Saturating: the u32::MAX clamp just evicts every
            // placement next drain.
            self.note_scrolled_into_scrollback(u32::try_from(n).unwrap_or(u32::MAX));
            self.scrollback_total_pushed = self
                .scrollback_total_pushed
                .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            self.prune_evicted_marks();
        } else {
            self.screen
                .rotate_region(region_top, region_bottom, n, ScrollDirection::Up, &blank);
        }
        self.queue_band_scroll(
            self.margins.top,
            self.margins.bottom,
            n,
            ScrollDirection::Up,
        );
    }

    /// Rows pushed past `scroll_bottom` are discarded; scrollback is
    /// only for upward scrolling.
    pub(crate) fn scroll_region_down(&mut self, n: u16) {
        if self.left_right_margin_mode {
            let (left, right_incl) = self.effective_left_right();
            if left != 0 || right_incl + 1 != self.screen.cols {
                self.scroll_subrect_down(n, left, right_incl);
                return;
            }
        }
        let n = n.max(1);
        let region_top = usize::from(self.margins.top);
        let region_bottom = usize::from(self.margins.bottom);
        let region_height = region_bottom - region_top + 1;
        let n = usize::from(n).min(region_height);
        let blank = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: None,
            sizing: None,
        };
        self.screen
            .rotate_region(region_top, region_bottom, n, ScrollDirection::Down, &blank);
        self.queue_band_scroll(
            self.margins.top,
            self.margins.bottom,
            n,
            ScrollDirection::Down,
        );
    }

    /// Sub-rectangle scroll for DECLRMM-active SU/SD/IL/DL; cells
    /// outside the rectangle are untouched (xterm under DECSLRM).
    /// Scrollback is not invoked: a partial-row push would corrupt the
    /// shape.
    pub(crate) fn scroll_subrect_range(
        &mut self,
        top: u16,
        bottom: u16,
        left: u16,
        right_incl: u16,
        n_rows: u16,
        direction: ScrollDirection,
    ) {
        let n_rows = n_rows.max(1);
        if top > bottom {
            return;
        }
        let region_height = bottom - top + 1;
        let n = n_rows.min(region_height);
        // The band copy reads source cells that can lie past a row's
        // watermark; materialize so no stale tail moves inside the band.
        for r in top..=bottom {
            self.materialize_row_tail(r);
            self.erase_pair_across(r, usize::from(left));
            self.erase_pair_across(r, usize::from(right_incl) + 1);
        }
        let blank = self.selective_blank_with_pen_bg();
        match direction {
            ScrollDirection::Up => {
                if bottom >= n + top {
                    for r in top..=(bottom - n) {
                        let src_r = r + n;
                        for c in left..=right_incl {
                            let src = self.screen.idx(src_r, c);
                            let dst = self.screen.idx(r, c);
                            self.screen.cells[dst] = self.screen.cells[src];
                        }
                    }
                }
                let first_blank_row = bottom + 1 - n;
                for r in first_blank_row..=bottom {
                    for c in left..=right_incl {
                        let idx = self.screen.idx(r, c);
                        self.screen.cells[idx] = blank;
                    }
                }
            }
            ScrollDirection::Down => {
                if top + n <= bottom {
                    for r in (top + n..=bottom).rev() {
                        let src_r = r - n;
                        for c in left..=right_incl {
                            let src = self.screen.idx(src_r, c);
                            let dst = self.screen.idx(r, c);
                            self.screen.cells[dst] = self.screen.cells[src];
                        }
                    }
                }
                let last_blank_row = top + n - 1;
                for r in top..=last_blank_row {
                    for c in left..=right_incl {
                        let idx = self.screen.idx(r, c);
                        self.screen.cells[idx] = blank;
                    }
                }
            }
        }
        for r in top..=bottom {
            self.screen.occ_bump_row(r, right_incl.saturating_add(1));
        }
        self.screen
            .damage
            .mark_range(top.into(), usize::from(bottom) + 1);
    }

    pub(crate) fn scroll_subrect_up(&mut self, n_rows: u16, left: u16, right_incl: u16) {
        self.scroll_subrect_range(
            self.margins.top,
            self.margins.bottom,
            left,
            right_incl,
            n_rows,
            ScrollDirection::Up,
        );
    }

    pub(crate) fn scroll_subrect_down(&mut self, n_rows: u16, left: u16, right_incl: u16) {
        self.scroll_subrect_range(
            self.margins.top,
            self.margins.bottom,
            left,
            right_incl,
            n_rows,
            ScrollDirection::Down,
        );
    }

    pub(crate) const fn carriage_return(&mut self) {
        self.screen.cursor.col = self.left_edge_for_line_start();
        self.screen.cursor.pending_wrap = false;
    }

    /// xterm: cursor at or right of `left_margin`, or origin mode on,
    /// parks at `left_margin`; otherwise col 0.
    pub(crate) const fn left_edge_for_line_start(&self) -> u16 {
        if self.left_right_margin_mode {
            if self.screen.cursor.col >= self.margins.left || self.origin_mode {
                self.margins.left
            } else {
                0
            }
        } else {
            0
        }
    }

    pub(crate) fn backspace(&mut self) {
        self.step_cursor_back(1);
    }

    /// xterm reverse-wrap gated on DECSET ?45 / ?1045 + DECAWM.
    ///
    /// Absorbs the first step if `pending_wrap`. `?1045` wraps unconditionally
    /// within margins; `?45` wraps only across autowrap continuations.
    pub(crate) fn step_cursor_back(&mut self, n: u16) {
        if n == 0 {
            return;
        }
        let had_pending_wrap = self.screen.cursor.pending_wrap;
        self.screen.cursor.pending_wrap = false;
        let extend = self.reverse_wrap_extend && self.autowrap;
        let inline = self.reverse_wrap_inline && self.autowrap;
        let mut remaining = n;
        if had_pending_wrap && (extend || inline) {
            remaining -= 1;
            if remaining == 0 {
                return;
            }
        }
        let new_col_on_wrap = || -> u16 {
            if self.left_right_margin_mode {
                self.margins.right
            } else {
                self.screen.cols.saturating_sub(1)
            }
        };
        for _ in 0..remaining {
            // A wrap step may have crossed into a row outside the
            // DECLRMM band.
            let left_edge =
                if self.left_right_margin_mode && self.screen.cursor.col >= self.margins.left {
                    self.margins.left
                } else {
                    0
                };
            if self.screen.cursor.col > left_edge {
                self.screen.cursor.col -= 1;
            } else if extend && self.screen.cursor.col == left_edge {
                let new_col = new_col_on_wrap();
                let new_row = if self.screen.cursor.row == self.margins.top {
                    self.margins.bottom
                } else if self.screen.cursor.row > 0 {
                    self.screen.cursor.row - 1
                } else {
                    self.screen.rows.saturating_sub(1)
                };
                self.screen.cursor.col = new_col;
                self.screen.cursor.row = new_row;
            } else if inline
                && self.screen.cursor.col == left_edge
                && self.screen.cursor.row > 0
                && self.screen.row_soft_wrap_continued(self.screen.cursor.row)
            {
                // A continuation bit on row 0 has no on-screen
                // predecessor (its head scrolled out), so the walk
                // stops rather than teleporting to the bottom.
                self.screen.cursor.col = new_col_on_wrap();
                self.screen.cursor.row -= 1;
            } else {
                break;
            }
        }
    }

    pub(crate) fn tab(&mut self) {
        // Under DECLRMM the edge collapses to `right_margin` (xterm's
        // `TabToNextStop` clamps at `rgt_marg`; esctest's
        // `test_DECSET_DECAWM_NoLineWrapOnTabWithLeftRightMargin`).
        // `?41` MoreFix: HT with `pending_wrap` set line-feeds first
        // (esctest's `test_DECSET_MoreFix`).
        if self.more_fix && self.screen.cursor.pending_wrap {
            self.line_feed();
            self.screen.cursor.col = 0;
            self.screen.cursor.pending_wrap = false;
            // MoreFix's implicit wrap is still an autowrap.
            self.screen.set_soft_wrap(self.screen.cursor.row, true);
            self.screen.damage.mark(self.screen.cursor.row.into());
        }
        let edge = usize::from(if self.left_right_margin_mode {
            self.margins.right
        } else {
            self.screen.cols.saturating_sub(1)
        });
        let start_col = usize::from(self.screen.cursor.col);
        let mut col = start_col;
        loop {
            col += 1;
            if col >= edge {
                col = edge;
                break;
            }
            if self.tab_stops.get(col).copied().unwrap_or(false) {
                break;
            }
        }
        if col != start_col {
            self.screen.cursor.col = col as u16;
            self.screen.cursor.pending_wrap = false;
        }
        // A TAB that did not move preserves `pending_wrap` so the
        // deferred wrap on the next print still fires.
    }

    pub(crate) fn hts(&mut self) {
        let col = usize::from(self.screen.cursor.col);
        if let Some(slot) = self.tab_stops.get_mut(col) {
            *slot = true;
        }
    }

    /// Mode 0 clears the cursor's column, 3 clears all; xterm ignores
    /// other modes.
    pub(crate) fn tbc(&mut self, mode: u16) {
        match mode {
            0 => {
                let col = usize::from(self.screen.cursor.col);
                if let Some(slot) = self.tab_stops.get_mut(col) {
                    *slot = false;
                }
            }
            3 => {
                self.tab_stops.fill(false);
            }
            _ => {}
        }
    }

    pub(crate) fn cuu(&mut self, n: u16) {
        let n = n.max(1);
        // VT220 §5.7: at or below the top margin CUU clamps to it;
        // above the region (DECOM off + CUP past it) it clamps to row 0.
        let floor = if self.screen.cursor.row >= self.margins.top {
            self.margins.top
        } else {
            0
        };
        self.screen.cursor.row = self.screen.cursor.row.saturating_sub(n).max(floor);
        self.screen.cursor.pending_wrap = false;
    }
    pub(crate) fn cud(&mut self, n: u16) {
        let n = n.max(1);
        // Mirror of `cuu`.
        let ceil = if self.screen.cursor.row <= self.margins.bottom {
            self.margins.bottom
        } else {
            self.screen.rows - 1
        };
        // Saturating: a CSI param near u16::MAX must not panic
        // (REQ-1200).
        self.screen.cursor.row = self.screen.cursor.row.saturating_add(n).min(ceil);
        self.screen.cursor.pending_wrap = false;
    }
    pub(crate) fn cuf(&mut self, n: u16) {
        let n = n.max(1);
        let right_edge =
            if self.left_right_margin_mode && self.screen.cursor.col <= self.margins.right {
                self.margins.right
            } else {
                self.screen.cols.saturating_sub(1)
            };
        self.screen.cursor.col = self.screen.cursor.col.saturating_add(n).min(right_edge);
        self.screen.cursor.pending_wrap = false;
    }
    pub(crate) fn cub(&mut self, n: u16) {
        self.step_cursor_back(n.max(1));
    }

    /// VT420: defaults Pl=1, Pr=cols; invalid ranges (Pl >= Pr) are
    /// rejected; setting moves the cursor home.
    pub(crate) fn decslrm(&mut self, pl1: u16, pr1: u16) {
        let cols = self.screen.cols;
        if cols == 0 {
            return;
        }
        let pl = if pl1 == 0 { 1 } else { pl1 };
        let pr = if pr1 == 0 { cols } else { pr1 };
        let left = pl.saturating_sub(1).min(cols - 1);
        let right = pr.saturating_sub(1).min(cols - 1);
        if left >= right {
            return;
        }
        self.margins.left = left;
        self.margins.right = right;
        // VT510: DECSLRM homes the cursor. With DECOM off, home is
        // (0, 0), not the band's left edge (esctest's
        // `test_DECSET_DECLRMM` expects an immediate write to start at
        // col 0); with DECOM on it is (scroll_top, left_margin).
        let (row, col) = if self.origin_mode {
            (self.margins.top, left)
        } else {
            (0, 0)
        };
        self.screen.cursor.row = row;
        self.screen.cursor.col = col;
        self.screen.cursor.pending_wrap = false;
    }
    /// 1-based (row, col) for CPR / DECXCPR. Under DECOM, coordinates outside
    /// the scroll margins fall back to absolute.
    // Clippy suggests nonexistent `margins.col` on this shape.
    #[allow(clippy::suspicious_operation_groupings)]
    pub(crate) const fn cursor_position_report(&self) -> (u16, u16) {
        let row = if self.origin_mode && self.screen.cursor.row >= self.margins.top {
            self.screen.cursor.row - self.margins.top + 1
        } else {
            self.screen.cursor.row + 1
        };
        let col = if self.origin_mode
            && self.left_right_margin_mode
            && self.screen.cursor.col >= self.margins.left
        {
            self.screen.cursor.col - self.margins.left + 1
        } else {
            self.screen.cursor.col + 1
        };
        (row, col)
    }

    pub(crate) fn cup(&mut self, row1: u16, col1: u16) {
        // With DECOM the row is relative to the region top and the
        // column to `left_margin` when DECLRMM is on (esctest's
        // `test_CUP_RespectsOriginMode`).
        let row0 = row1.max(1) - 1;
        let col0 = col1.max(1) - 1;
        let row = if self.origin_mode {
            self.margins
                .top
                .saturating_add(row0)
                .min(self.margins.bottom)
        } else {
            row0.min(self.screen.rows - 1)
        };
        let col = if self.origin_mode && self.left_right_margin_mode {
            self.margins
                .left
                .saturating_add(col0)
                .min(self.margins.right)
        } else {
            col0.min(self.screen.cols - 1)
        };
        self.screen.cursor.row = row;
        self.screen.cursor.col = col;
        self.screen.cursor.pending_wrap = false;
    }

    /// Column is relative to `left_margin` and clamped at
    /// `right_margin` when DECOM and DECLRMM are both on (esctest's
    /// `test_CHA_RespectsOriginMode`).
    pub(crate) fn cha(&mut self, col1: u16) {
        let col0 = col1.max(1) - 1;
        let col = if self.origin_mode && self.left_right_margin_mode {
            self.margins
                .left
                .saturating_add(col0)
                .min(self.margins.right)
        } else {
            col0.min(self.screen.cols - 1)
        };
        self.screen.cursor.col = col;
        self.screen.cursor.pending_wrap = false;
    }

    /// VPA ignores origin mode (xterm; esctest's
    /// `test_VPA_IgnoresOriginMode` pins the VT420 asymmetry with CUP).
    pub(crate) fn vpa(&mut self, row1: u16) {
        let row0 = row1.max(1) - 1;
        let row = row0.min(self.screen.rows - 1);
        self.screen.cursor.row = row;
        self.screen.cursor.pending_wrap = false;
    }

    pub(crate) fn ech(&mut self, n: u16) {
        let n = usize::from(n.max(1));
        let from = self
            .screen
            .idx(self.screen.cursor.row, self.screen.cursor.col);
        let row_end = self.screen.idx(self.screen.cursor.row, 0) + usize::from(self.screen.cols);
        let to = (from + n).min(row_end);
        self.range_blank_iso(self.screen.cursor.row, from, to);
        self.screen.damage.mark(self.screen.cursor.row.into());
    }

    /// xterm allows one slot per screen; a save overwrites.
    pub(crate) const fn decsc(&mut self) {
        self.saved_cursor = SavedCursor {
            cursor: self.screen.cursor,
            pen: self.pen,
            origin_mode: self.origin_mode,
        };
    }

    /// After `DECSTR` / boot the slot is [`SavedCursor::default`], so
    /// `DECRC` homes with default pen and DECOM off (esctest's
    /// `test_*_MoveToHomeWhenNotSaved`); restoring origin mode is what
    /// `test_SaveRestoreCursor_ResetsOriginMode` pins.
    pub(crate) fn decrc(&mut self) {
        let SavedCursor {
            mut cursor,
            pen,
            origin_mode,
        } = self.saved_cursor;
        // The screen may have shrunk since the save.
        if self.screen.rows > 0 && cursor.row >= self.screen.rows {
            cursor.row = self.screen.rows - 1;
        }
        if self.screen.cols > 0 && cursor.col >= self.screen.cols {
            cursor.col = self.screen.cols - 1;
        }
        self.screen.cursor = cursor;
        self.pen = pen;
        self.resync_pen_style();
        self.origin_mode = origin_mode;
    }

    /// `DECSTR` (`CSI ! p`) soft reset: programmable state to defaults,
    /// cells untouched (unlike `RIS`). `?1049` is not in the DECSTR
    /// reset set, so the alt-screen toggle survives.
    pub(crate) fn decstr(&mut self) {
        // VT510 homes the cursor, but xterm (and esctest's
        // `test_*_Reset`, which expects a post-DECSTR write at the
        // post-write column) leaves it put and only resets the
        // saved-cursor slot.
        self.screen.cursor_style = CursorStyle::default();
        self.screen.cursor_blink = true;
        self.c1_8bit = false;
        self.pen = Attributes::default();
        self.pen_style = StyleId::DEFAULT;
        self.saved_cursor = SavedCursor::default();
        self.bracketed_paste = false;
        self.application_cursor = false;
        self.focus_reporting = false;
        self.sync_output = SyncOutput::Off;
        self.mouse_protocol = MouseProtocol::Off;
        self.mouse_encoding = MouseEncoding::Default;
        self.margins.top = 0;
        self.margins.bottom = self.screen.rows - 1;
        self.origin_mode = false;
        self.autowrap = true;
        self.reverse_wrap_inline = false;
        self.reverse_wrap_extend = false;
        self.left_right_margin_mode = false;
        self.margins.left = 0;
        self.margins.right = self.screen.cols.saturating_sub(1);
        self.insert_mode = false;
        self.linefeed_newline_mode = false;
        self.tab_stops = default_tab_stops(self.screen.cols);
        self.reverse_video = false;
        self.application_keypad = false;
        // Kitty graphics: DECSTR wipes every placement, `C=1` included
        // (no soft-reset opt-out in the spec). DECSTR zeroes no cells;
        // this range is a placement-only signal.
        let last_row = self.screen.rows.saturating_sub(1);
        self.pty_effects.push(PtyEffect::Erased(ErasedRange {
            top: 0,
            bottom: last_row,
            force: true,
        }));
        self.screen.damage.mark_all();
    }

    pub(crate) fn el(&mut self, mode: u16) {
        let row_start = self.screen.idx(self.screen.cursor.row, 0);
        let cols = usize::from(self.screen.cols);
        let mut erased_some = false;
        match mode {
            0 => {
                let from = self
                    .screen
                    .idx(self.screen.cursor.row, self.screen.cursor.col);
                let to = row_start + cols;
                self.range_blank_iso(self.screen.cursor.row, from, to);
                erased_some = true;
            }
            1 => {
                let to = self
                    .screen
                    .idx(self.screen.cursor.row, self.screen.cursor.col)
                    + 1;
                self.range_blank_iso(self.screen.cursor.row, row_start, to);
                erased_some = true;
            }
            2 => {
                self.range_blank_iso(self.screen.cursor.row, row_start, row_start + cols);
                erased_some = true;
            }
            _ => {}
        }
        if erased_some {
            // Kitty graphics: a single-row range; the dispatcher's
            // intersection check covers the whole row regardless of
            // which columns EL touched.
            self.pty_effects.push(PtyEffect::Erased(ErasedRange {
                top: self.screen.cursor.row,
                bottom: self.screen.cursor.row,
                force: false,
            }));
        }
        self.screen.damage.mark(self.screen.cursor.row.into());
    }

    /// Outside `0..=6` is ignored (xterm).
    pub(crate) const fn decscusr(&mut self, ps: u16) {
        let style = match ps {
            0..=2 => CursorStyle::Block,
            3..=4 => CursorStyle::Underline,
            5..=6 => CursorStyle::Bar,
            _ => return,
        };
        // Odd Ps and 0 blink; forwarded via `GridMsg::CursorState.blink`
        // and echoed back by DECRQSS.
        self.screen.cursor_blink = matches!(ps, 0 | 1 | 3 | 5);
        self.screen.cursor_style = style;
    }

    pub(crate) fn ed(&mut self, mode: u16) {
        let cols = usize::from(self.screen.cols);
        let last_row = self.screen.rows.saturating_sub(1);
        match mode {
            0 => {
                // `el(0)` already pushed the cursor row's ErasedRange.
                self.el(0);
                let from_row = usize::from(self.screen.cursor.row) + 1;
                for r in from_row..usize::from(self.screen.rows) {
                    let logical = u16::try_from(r).unwrap_or(u16::MAX);
                    let start = self.screen.idx(logical, 0);
                    self.range_blank_iso(logical, start, start + cols);
                }
                if usize::from(self.screen.cursor.row) < usize::from(last_row) {
                    self.pty_effects.push(PtyEffect::Erased(ErasedRange {
                        top: self.screen.cursor.row + 1,
                        bottom: last_row,
                        force: false,
                    }));
                }
            }
            1 => {
                // `el(1)` already pushed the cursor row's ErasedRange.
                self.el(1);
                for r in 0..usize::from(self.screen.cursor.row) {
                    let logical = u16::try_from(r).unwrap_or(u16::MAX);
                    let start = self.screen.idx(logical, 0);
                    self.range_blank_iso(logical, start, start + cols);
                }
                if self.screen.cursor.row > 0 {
                    self.pty_effects.push(PtyEffect::Erased(ErasedRange {
                        top: 0,
                        bottom: self.screen.cursor.row - 1,
                        force: false,
                    }));
                }
            }
            2 => {
                // ED 2 is unconditional: esctest's `reset()` issues it,
                // and honoring ISO_PROTECTED here would leak an earlier
                // test's SPA stretch into the next (xterm: incremental
                // erases respect ECMA-48 protection, full erase does
                // not).
                let blank = Cell {
                    grapheme: Grapheme::Empty,
                    style: self.pen_style,
                    link: None,
                    sizing: None,
                };
                if blank == Cell::default() {
                    // Everything past the watermark is already `Cell::default()`,
                    // so blanking only the occupied prefix suffices.
                    for logical in 0..usize::from(self.screen.rows) {
                        let phys = self.screen.phys_row_at(logical);
                        let occ = usize::from(self.screen.occupancy[phys]);
                        // A stale soft-wrap bit on an already-blank row
                        // still has to ship (it rides the row's delta).
                        if occ == 0 && !self.screen.soft_wrap[phys] {
                            continue;
                        }
                        if occ > 0 {
                            let start = phys * cols;
                            self.screen.cells[start..start + occ].fill(blank);
                            self.screen.occupancy[phys] = 0;
                        }
                        self.screen.soft_wrap[phys] = false;
                        self.screen.damage.mark(logical);
                    }
                } else {
                    // BCE: every cell takes the pen's color, and the
                    // watermark pins at `cols` (a colored blank is not
                    // a default cell).
                    self.screen.cells.fill(blank);
                    self.screen.occupancy.fill(self.screen.cols);
                    self.screen.soft_wrap.fill(false);
                    self.screen.damage.mark_all();
                }
                // Drop the bulk-path hint so a `cat` right after a
                // `clear` takes the plaintext fast path.
                self.screen.has_sized_cells = false;
                // Kitty graphics: `force = false` so the `C=1`
                // exemption applies; RIS / DECSTR push a forced range.
                self.pty_effects.push(PtyEffect::Erased(ErasedRange {
                    top: 0,
                    bottom: last_row,
                    force: false,
                }));
                // Damage is settled above; the mark_all below would
                // ship the untouched rows as no-op RowDeltas.
                return;
            }
            3 => {
                // xterm's `CSI 3 J`: the live screen is not touched
                // (esctest's `test_ED_3`), so no damage either.
                self.drop_history();
                // With the ring emptied the retained floor is
                // `scrollback_total_pushed`, so the shared prune drops
                // exactly the scrollback-bound prefix.
                self.prune_evicted_marks();
                return;
            }
            _ => {}
        }
        self.screen.damage.mark_all();
    }

    /// No-op when the cursor is outside the scroll region (VT220); the
    /// cursor row stays, the column resets to the left edge.
    pub(crate) fn il(&mut self, n: u16) {
        if self.screen.cursor.row < self.margins.top || self.screen.cursor.row > self.margins.bottom
        {
            return;
        }
        if self.left_right_margin_mode {
            let (left, right_incl) = self.effective_left_right();
            if self.screen.cursor.col < left || self.screen.cursor.col > right_incl {
                return;
            }
            if left != 0 || right_incl + 1 != self.screen.cols {
                self.scroll_subrect_range(
                    self.screen.cursor.row,
                    self.margins.bottom,
                    left,
                    right_incl,
                    n,
                    ScrollDirection::Down,
                );
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                return;
            }
        }
        let n = n.max(1);
        let band_top = usize::from(self.screen.cursor.row);
        let band_bottom = usize::from(self.margins.bottom);
        let band_height = band_bottom - band_top + 1;
        let n = usize::from(n).min(band_height);
        let blank = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: None,
            sizing: None,
        };
        self.screen
            .rotate_region(band_top, band_bottom, n, ScrollDirection::Down, &blank);
        self.queue_band_scroll(
            self.screen.cursor.row,
            self.margins.bottom,
            n,
            ScrollDirection::Down,
        );
        self.screen.cursor.col = 0;
        self.screen.cursor.pending_wrap = false;
    }

    /// No-op when the cursor is outside the scroll region; rows below
    /// the region are untouched.
    pub(crate) fn dl(&mut self, n: u16) {
        if self.screen.cursor.row < self.margins.top || self.screen.cursor.row > self.margins.bottom
        {
            return;
        }
        if self.left_right_margin_mode {
            let (left, right_incl) = self.effective_left_right();
            if self.screen.cursor.col < left || self.screen.cursor.col > right_incl {
                return;
            }
            if left != 0 || right_incl + 1 != self.screen.cols {
                self.scroll_subrect_range(
                    self.screen.cursor.row,
                    self.margins.bottom,
                    left,
                    right_incl,
                    n,
                    ScrollDirection::Up,
                );
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                return;
            }
        }
        let n = n.max(1);
        let band_top = usize::from(self.screen.cursor.row);
        let band_bottom = usize::from(self.margins.bottom);
        let band_height = band_bottom - band_top + 1;
        let n = usize::from(n).min(band_height);
        let blank = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: None,
            sizing: None,
        };
        self.screen
            .rotate_region(band_top, band_bottom, n, ScrollDirection::Up, &blank);
        self.queue_band_scroll(
            self.screen.cursor.row,
            self.margins.bottom,
            n,
            ScrollDirection::Up,
        );
        self.screen.cursor.col = 0;
        self.screen.cursor.pending_wrap = false;
    }

    /// `DCH` (`CSI Pn P`). curses sliding-frame animations (`sl`,
    /// `asciiquarium`) scroll sprites via CUP + DCH every frame.
    pub(crate) fn dch(&mut self, n: u16) {
        // Under DECLRMM a cursor outside the margins is a no-op
        // (`test_DCH_DoesNothingOutsideLeftRightMargin`) and the shift
        // truncates at `right_margin`.
        let n = usize::from(n.max(1));
        let row_start = self.screen.idx(self.screen.cursor.row, 0);
        let cursor = usize::from(self.screen.cursor.col);
        let (left, right) = self.effective_left_right();
        let left = usize::from(left);
        let right_inclusive = usize::from(right);
        if cursor < left || cursor > right_inclusive {
            return;
        }
        let row = self.screen.cursor.row;
        self.materialize_row_tail(row);
        let edge = right_inclusive + 1;
        let n = n.min(edge - cursor);
        for seam in [cursor, cursor + n, edge] {
            self.erase_pair_across(row, seam);
        }
        let src_start = row_start + cursor + n;
        let src_end = row_start + edge;
        let dst_start = row_start + cursor;
        if src_start < src_end {
            copy_within_cells(&mut self.screen.cells, src_start..src_end, dst_start);
        }
        self.range_blank(
            self.screen.cursor.row,
            row_start + edge - n,
            row_start + edge,
        );
        self.screen.damage.mark(self.screen.cursor.row.into());
    }

    /// `ICH` (`CSI Pn @`), the inverse of `DCH`.
    pub(crate) fn ich(&mut self, n: u16) {
        // Mirror of DCH.
        let n = usize::from(n.max(1));
        let row_start = self.screen.idx(self.screen.cursor.row, 0);
        let cursor = usize::from(self.screen.cursor.col);
        let (left, right) = self.effective_left_right();
        let left = usize::from(left);
        let right_inclusive = usize::from(right);
        if cursor < left || cursor > right_inclusive {
            return;
        }
        let row = self.screen.cursor.row;
        self.materialize_row_tail(row);
        let edge = right_inclusive + 1;
        let n = n.min(edge - cursor);
        for seam in [cursor, edge - n, edge] {
            self.erase_pair_across(row, seam);
        }
        let src_start = row_start + cursor;
        let src_end = row_start + edge - n;
        let dst_start = row_start + cursor + n;
        if src_start < src_end {
            copy_within_cells(&mut self.screen.cells, src_start..src_end, dst_start);
            self.screen.occ_bump_row(
                self.screen.cursor.row,
                u16::try_from(edge).unwrap_or(u16::MAX),
            );
        }
        self.range_blank(
            self.screen.cursor.row,
            row_start + cursor,
            row_start + cursor + n,
        );
        self.screen.damage.mark(self.screen.cursor.row.into());
    }

    /// Unlike LF this runs regardless of cursor position.
    pub(crate) fn su(&mut self, n: u16) {
        self.scroll_region_up(n);
    }

    pub(crate) fn sd(&mut self, n: u16) {
        self.scroll_region_down(n);
    }

    /// `SL` (`CSI Pn SP @`). The shift spans the full row width
    /// (DECSLRM margins are not consulted); the cursor does not move.
    pub(crate) fn sl(&mut self, n: u16) {
        self.scroll_region_horizontal(n, HDir::Left);
    }

    /// `SR` (`CSI Pn SP A`), mirror of [`Self::sl`].
    pub(crate) fn sr(&mut self, n: u16) {
        self.scroll_region_horizontal(n, HDir::Right);
    }

    fn scroll_region_horizontal(&mut self, n: u16, dir: HDir) {
        let n = n.max(1);
        let cols = usize::from(self.screen.cols);
        let n = usize::from(n).min(cols);
        for r in self.margins.top..=self.margins.bottom {
            let row_start = self.screen.idx(r, 0);
            if n < cols {
                self.materialize_row_tail(r);
                self.erase_pair_across(
                    r,
                    match dir {
                        HDir::Left => n,
                        HDir::Right => cols - n,
                    },
                );
                match dir {
                    HDir::Left => {
                        let src_start = row_start + n;
                        let src_end = row_start + cols;
                        copy_within_cells(&mut self.screen.cells, src_start..src_end, row_start);
                    }
                    HDir::Right => {
                        let src_start = row_start;
                        let src_end = row_start + cols - n;
                        let dst_start = row_start + n;
                        copy_within_cells(&mut self.screen.cells, src_start..src_end, dst_start);
                    }
                }
            }
            match dir {
                HDir::Left => self.range_blank(r, row_start + cols - n, row_start + cols),
                HDir::Right => self.range_blank(r, row_start, row_start + n),
            }
            self.screen.damage.mark(r.into());
        }
    }

    /// VT220 §5.7: Pt < Pb required (else reset to full screen);
    /// bottoms past the screen clamp; the cursor moves to the region
    /// origin.
    pub(crate) fn decstbm(&mut self, pt: u16, pb: u16) {
        let pt = if pt == 0 { 1 } else { pt };
        let pb = if pb == 0 { self.screen.rows } else { pb };
        let top = (pt - 1).min(self.screen.rows - 1);
        let bottom = (pb - 1).min(self.screen.rows - 1);
        if top >= bottom {
            self.margins.top = 0;
            self.margins.bottom = self.screen.rows - 1;
        } else {
            self.margins.top = top;
            self.margins.bottom = bottom;
        }
        // The column resets to 0 regardless of DECOM; the row depends
        // on it.
        let row = if self.origin_mode {
            self.margins.top
        } else {
            0
        };
        self.screen.cursor.row = row;
        self.screen.cursor.col = 0;
        self.screen.cursor.pending_wrap = false;
    }

    /// Clamps at the active right edge (`right_margin` under DECLRMM),
    /// a hard ceiling regardless of the start column, like xterm
    /// (esctest's `test_CHT_IgnoresScrollingRegion`, a misnomer: CHT
    /// respects the LR margin but ignores the DECSTBM region).
    pub(crate) fn cht(&mut self, n: u16) {
        let n = n.max(1);
        let edge = usize::from(if self.left_right_margin_mode {
            self.margins.right
        } else {
            self.screen.cols.saturating_sub(1)
        });
        let mut col = usize::from(self.screen.cursor.col);
        for _ in 0..n {
            loop {
                col += 1;
                if col >= edge {
                    col = edge;
                    break;
                }
                if self.tab_stops.get(col).copied().unwrap_or(false) {
                    break;
                }
            }
            if col == edge {
                break;
            }
        }
        self.screen.cursor.col = col as u16;
        self.screen.cursor.pending_wrap = false;
    }

    /// Not clamped at the left margin under DECLRMM, unlike CHT at the
    /// right: xterm's asymmetry (esctest's `test_CBT_IgnoresRegion`
    /// starts inside the DECSLRM band and requires CBT to reach column
    /// 1).
    pub(crate) fn cbt(&mut self, n: u16) {
        let n = n.max(1);
        let mut col = usize::from(self.screen.cursor.col);
        for _ in 0..n {
            if col == 0 {
                break;
            }
            loop {
                col -= 1;
                if col == 0 {
                    break;
                }
                if self.tab_stops.get(col).copied().unwrap_or(false) {
                    break;
                }
            }
            if col == 0 {
                break;
            }
        }
        self.screen.cursor.col = col as u16;
        self.screen.cursor.pending_wrap = false;
    }

    /// XTGETTCAP: `DCS + q <hex>[ ; <hex> …] ST`; the reply mirrors
    /// input order, `1+r <hex> = <hexvalue>` per known cap and
    /// `0+r <hex>` per unknown, in one `DCS … ST`. Only a small subset
    /// is answered: enough for esctest's `GetIndexedColors()` to read
    /// the `Co` that aligns with `SPECIAL_COLOR_OSC4_OFFSET`.
    pub(crate) fn xtgettcap_reply(&mut self, body: &[u8]) {
        let mut parts: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for hex in body.split(|b| *b == b';') {
            let Some(name) = hex_to_ascii(hex) else {
                parts.push((hex.to_vec(), None));
                continue;
            };
            // `co` / `li` answer from the live grid, not the terminfo
            // default: a program that resized us and then probes
            // `tput cols` must read the current geometry.
            let value: Option<Vec<u8>> = match name.as_slice() {
                b"co" | b"cols" => Some(ascii_to_hex(self.screen.cols.to_string().as_bytes())),
                b"li" | b"lines" => Some(ascii_to_hex(self.screen.rows.to_string().as_bytes())),
                b"TN" | b"name" => self
                    .term_name
                    .as_deref()
                    .map(|v| ascii_to_hex(v.as_bytes())),
                _ => xtgettcap_value(&name).map(|v| ascii_to_hex(v.as_bytes())),
            };
            parts.push((hex.to_vec(), value));
        }
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(b"\x1bP");
        // Status byte: `1` iff every queried cap was known (xterm's
        // collapsed form).
        let all_known = parts.iter().all(|(_, v)| v.is_some());
        out.push(if all_known { b'1' } else { b'0' });
        out.extend_from_slice(b"+r");
        for (i, (hex, value)) in parts.iter().enumerate() {
            if i > 0 {
                out.push(b';');
            }
            out.extend_from_slice(hex);
            if let Some(v) = value {
                out.push(b'=');
                out.extend_from_slice(v);
            }
        }
        out.extend_from_slice(b"\x1b\\");
        self.enqueue_response(out);
    }

    /// xterm form `DCS Ps $ r D...D ST`, Ps=1 valid / Ps=0 invalid; an
    /// invalid reply tells a probing program the feature is
    /// unsupported so it falls back rather than blocking.
    pub(crate) fn decrqss_reply(&mut self, query: &[u8]) {
        let body: Option<Vec<u8>> = match query {
            b"m" => Some(self.decrqss_sgr_params()),
            // 1-based, mirroring the DECSTBM input form.
            b"r" => Some(
                format!(
                    "{};{}r",
                    self.margins.top.saturating_add(1),
                    self.margins.bottom.saturating_add(1)
                )
                .into_bytes(),
            ),
            b"\"q" => {
                let ps = i32::from(self.pen.flags.contains(AttrFlags::PROTECTED));
                Some(format!("{ps}\"q").into_bytes())
            }
            b"*x" => Some(format!("{}*x", self.dec_sace).into_bytes()),
            // Always 64 (VT420, our DA1). VT510: `level;1` = 7-bit,
            // else 8-bit.
            b"\"p" => {
                let bit = i32::from(!self.c1_8bit);
                Some(format!("64;{bit}\"p").into_bytes())
            }
            // 1-based inclusive. xterm reports them even when DECLRMM
            // is off: the margins persist in storage.
            b"s" => Some(
                format!(
                    "{};{}s",
                    self.margins.left.saturating_add(1),
                    self.margins.right.saturating_add(1)
                )
                .into_bytes(),
            ),
            b" q" => {
                let ps = match (self.screen.cursor_style, self.screen.cursor_blink) {
                    (CursorStyle::Block, true) => 1,
                    (CursorStyle::Block, false) => 2,
                    (CursorStyle::Underline, true) => 3,
                    (CursorStyle::Underline, false) => 4,
                    (CursorStyle::Bar, true) => 5,
                    (CursorStyle::Bar, false) => 6,
                };
                Some(format!("{ps} q").into_bytes())
            }
            _ => None,
        };
        let mut response = Vec::with_capacity(8 + body.as_ref().map_or(0, Vec::len));
        response.extend_from_slice(b"\x1bP");
        if let Some(payload) = body {
            response.push(b'1');
            response.extend_from_slice(b"$r");
            response.extend_from_slice(&payload);
        } else {
            response.push(b'0');
            response.extend_from_slice(b"$r");
        }
        response.extend_from_slice(b"\x1b\\");
        self.enqueue_response(response);
    }

    /// xterm prefixes a `0` ("reset all") so re-emitting the payload
    /// restores the pen regardless of prior state.
    pub(crate) fn decrqss_sgr_params(&self) -> Vec<u8> {
        let mut params: Vec<String> = vec!["0".into()];
        let flags = self.pen.flags;
        if flags.contains(AttrFlags::BOLD) {
            params.push("1".into());
        }
        if flags.contains(AttrFlags::FAINT) {
            params.push("2".into());
        }
        if flags.contains(AttrFlags::ITALIC) {
            params.push("3".into());
        }
        if flags.contains(AttrFlags::UNDERLINE) {
            match self.pen.underline_style {
                UnderlineStyle::Single => params.push("4".into()),
                UnderlineStyle::Double => params.push("21".into()),
                UnderlineStyle::Curly => params.push("4:3".into()),
                UnderlineStyle::Dotted => params.push("4:4".into()),
                UnderlineStyle::Dashed => params.push("4:5".into()),
            }
        }
        if flags.contains(AttrFlags::BLINK) {
            params.push("5".into());
        }
        if flags.contains(AttrFlags::REVERSE) {
            params.push("7".into());
        }
        if flags.contains(AttrFlags::CONCEAL) {
            params.push("8".into());
        }
        if flags.contains(AttrFlags::STRIKETHROUGH) {
            params.push("9".into());
        }
        if flags.contains(AttrFlags::OVERLINE) {
            params.push("53".into());
        }
        if let Some(s) = encode_sgr_color(self.pen.fg, 30, 38) {
            params.push(s);
        }
        if let Some(s) = encode_sgr_color(self.pen.bg, 40, 48) {
            params.push(s);
        }
        if let Some(s) = encode_sgr_underline_color(self.pen.underline_color) {
            params.push(s);
        }
        let mut out = params.join(";").into_bytes();
        out.push(b'm');
        out
    }

    /// Ps=1 protects subsequently printed cells; Ps=0 or 2 clears.
    /// Other Ps are accepted (xterm).
    pub(crate) fn decsca(&mut self, ps: u16) {
        match ps {
            1 => self.pen.flags.insert(AttrFlags::PROTECTED),
            0 | 2 => self.pen.flags.remove(AttrFlags::PROTECTED),
            _ => return,
        }
        self.resync_pen_style();
    }

    pub(crate) fn selective_blank(&mut self) -> Cell {
        let attrs = Attributes {
            fg: Color::Default,
            bg: self.pen.bg,
            underline_color: Color::Default,
            flags: AttrFlags::empty(),
            underline_style: UnderlineStyle::default(),
        };
        Cell {
            grapheme: Grapheme::Empty,
            style: self.screen.style_table.intern(attrs),
            link: None,
            sizing: None,
        }
    }

    /// Same Ps semantics as EL but PROTECTED cells survive.
    pub(crate) fn decsel(&mut self, ps: u16) {
        let cols = usize::from(self.screen.cols);
        let row = self.screen.cursor.row;
        let row_start = self.screen.idx(row, 0);
        let cursor = usize::from(self.screen.cursor.col);
        let (from, to) = match ps {
            0 => (cursor, cols),
            1 => (0, cursor + 1),
            2 => (0, cols),
            _ => return,
        };
        let (from, to) = self.pair_aligned_span(row, from, to);
        let blank = self.selective_blank();
        let protect = AttrFlags::PROTECTED | AttrFlags::ISO_PROTECTED;
        for col in from..to {
            let idx = row_start + col;
            if !self
                .screen
                .style_table
                .resolve(self.screen.cells[idx].style)
                .flags
                .intersects(protect)
            {
                self.screen.cells[idx] = blank;
            }
        }
        // The selective blank carries the pen background (BCE). Never
        // lowered here: protected cells may have survived inside.
        self.screen
            .occ_bump_row(row, u16::try_from(to).unwrap_or(u16::MAX));
        self.screen.damage.mark(row.into());
    }

    pub(crate) fn decsed(&mut self, ps: u16) {
        let cols = usize::from(self.screen.cols);
        let rows = usize::from(self.screen.rows);
        let cur_row = usize::from(self.screen.cursor.row);
        let cur_col = usize::from(self.screen.cursor.col);
        // The watermark is raised to `cols` on every row below, so no
        // stale tail may survive under it.
        for r in 0..self.screen.rows {
            self.materialize_row_tail(r);
        }
        let (tail_start, _) = self.pair_aligned_span(self.screen.cursor.row, cur_col, cols);
        let (_, head_end) = self.pair_aligned_span(self.screen.cursor.row, 0, cur_col + 1);
        let blank = self.selective_blank();
        let protect = AttrFlags::PROTECTED | AttrFlags::ISO_PROTECTED;
        let styles = &self.screen.style_table;
        let erase_range = |start: usize, end: usize, cells: &mut [Cell], blank: &Cell| {
            for cell in &mut cells[start..end] {
                if !styles.resolve(cell.style).flags.intersects(protect) {
                    *cell = *blank;
                }
            }
        };
        match ps {
            0 => {
                // Rows are addressed through `phys_row_at`: after a
                // scroll the physical order is rotated, so a bare
                // `logical * cols` would erase the wrong rows. (Mode 2
                // is rotation-agnostic.)
                let cur_base = self.screen.phys_row_at(cur_row) * cols;
                erase_range(
                    cur_base + tail_start,
                    cur_base + cols,
                    &mut self.screen.cells,
                    &blank,
                );
                for r in (cur_row + 1)..rows {
                    let rs = self.screen.phys_row_at(r) * cols;
                    erase_range(rs, rs + cols, &mut self.screen.cells, &blank);
                }
            }
            1 => {
                for r in 0..cur_row {
                    let rs = self.screen.phys_row_at(r) * cols;
                    erase_range(rs, rs + cols, &mut self.screen.cells, &blank);
                }
                let cur_base = self.screen.phys_row_at(cur_row) * cols;
                erase_range(
                    cur_base,
                    cur_base + head_end,
                    &mut self.screen.cells,
                    &blank,
                );
            }
            2 => {
                erase_range(0, rows * cols, &mut self.screen.cells, &blank);
            }
            _ => return,
        }
        // Conservative `cols` everywhere: the blank carries the pen
        // background, and protected cells may have survived anywhere.
        self.screen.occupancy.fill(self.screen.cols);
        self.screen.damage.mark_all();
    }

    fn rect_params(params: &[u16], offset: usize, defaults: RectParams) -> RectParams {
        RectParams {
            pt: params.get(offset).copied().unwrap_or(defaults.pt),
            pl: params.get(offset + 1).copied().unwrap_or(defaults.pl),
            pb: params.get(offset + 2).copied().unwrap_or(defaults.pb),
            pr: params.get(offset + 3).copied().unwrap_or(defaults.pr),
        }
    }

    /// DECOM offset for rectangle endpoints: 1-based input is relative
    /// to the region top when DECOM is set, and to `left_margin` when
    /// DECLRMM is also on (esctest's
    /// `fillRectangle_respectsOriginMode`,
    /// `test_DECCRA_respectsOriginMode`).
    const fn rect_origin(&self) -> RectOrigin {
        RectOrigin {
            row: if self.origin_mode {
                self.margins.top
            } else {
                0
            },
            col: if self.origin_mode && self.left_right_margin_mode {
                self.margins.left
            } else {
                0
            },
        }
    }

    /// `None` for `Pt > Pb` / `Pl > Pr` or a zero-extent clamp.
    /// Defaults: 0 → 1 for top/left (xterm's empty-param semantics),
    /// 0 → grid edge for bottom/right.
    pub(crate) fn clamp_rectangle(&self, rect: RectParams) -> Option<GridRect> {
        let origin = self.rect_origin();
        let pt = if rect.pt == 0 { 1 } else { rect.pt };
        let pl = if rect.pl == 0 { 1 } else { rect.pl };
        let pb = if rect.pb == 0 {
            self.screen.rows
        } else {
            rect.pb
        };
        let pr = if rect.pr == 0 {
            self.screen.cols
        } else {
            rect.pr
        };
        // Reject inverted rectangles before clamping: xterm treats them
        // as a no-op, and clamping first could rescue one into a
        // degenerate-but-valid range.
        if pt > pb || pl > pr {
            return None;
        }
        if self.screen.rows == 0 || self.screen.cols == 0 {
            return None;
        }
        let top = pt.saturating_sub(1).saturating_add(origin.row);
        let bottom = pb.saturating_sub(1).saturating_add(origin.row);
        let left = pl.saturating_sub(1).saturating_add(origin.col);
        let right = pr.saturating_sub(1).saturating_add(origin.col);
        let rect = GridRect {
            top: top.min(self.screen.rows - 1),
            left: left.min(self.screen.cols - 1),
            bottom: bottom.min(self.screen.rows - 1),
            right: right.min(self.screen.cols - 1),
        };
        if rect.top > rect.bottom || rect.left > rect.right {
            return None;
        }
        Some(rect)
    }

    /// Erases regardless of PROTECTED; margins do not apply (esctest's
    /// `test_DECERA_ignoresMargins`).
    pub(crate) fn decera(&mut self, params: &[u16]) {
        let rect = Self::rect_params(params, 0, RectParams::UNSET);
        let Some(rect) = self.clamp_rectangle(rect) else {
            return;
        };
        for r in rect.top..=rect.bottom {
            let (from, to) =
                self.pair_aligned_span(r, usize::from(rect.left), usize::from(rect.right) + 1);
            let row_start = self.screen.idx(r, 0);
            blank_cells(&mut self.screen.cells[row_start + from..row_start + to]);
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// DECERA that preserves PROTECTED cells.
    pub(crate) fn decsera(&mut self, params: &[u16]) {
        let rect = Self::rect_params(params, 0, RectParams::UNSET);
        let Some(rect) = self.clamp_rectangle(rect) else {
            return;
        };
        let blank = self.selective_blank();
        for r in rect.top..=rect.bottom {
            // Reads and re-watermarks past existing occupancy, so materialize first.
            self.materialize_row_tail(r);
            let (from, to) =
                self.pair_aligned_span(r, usize::from(rect.left), usize::from(rect.right) + 1);
            let row_start = self.screen.idx(r, 0);
            for idx in row_start + from..row_start + to {
                if !self
                    .screen
                    .style_table
                    .resolve(self.screen.cells[idx].style)
                    .flags
                    .contains(AttrFlags::PROTECTED)
                {
                    self.screen.cells[idx] = blank;
                }
            }
            self.screen.occ_bump_row(r, rect.right.saturating_add(1));
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// `Pch` is a Latin-1 codepoint restricted to printable
    /// (0x20..0x7E, 0xA0..0xFF) as in xterm; outside that the call is
    /// a no-op. Margins do not apply (esctest's
    /// `test_DECFRA_ignoresMargins`).
    pub(crate) fn decfra(&mut self, params: &[u16]) {
        let pch = params.first().copied().unwrap_or(0);
        let rect = Self::rect_params(params, 1, RectParams::UNSET);
        if !((0x20..=0x7E).contains(&pch) || (0xA0..=0xFF).contains(&pch)) {
            return;
        }
        let Some(rect) = self.clamp_rectangle(rect) else {
            return;
        };
        let grapheme = if pch <= 0x7E {
            Grapheme::Ascii(pch as u8)
        } else {
            Grapheme::Char(char::from_u32(u32::from(pch)).unwrap_or(' '))
        };
        for r in rect.top..=rect.bottom {
            // A fill whose left edge sits past current occupancy would
            // otherwise leave `[occ..left)` stale inside the new live
            // extent.
            self.materialize_row_tail(r);
            let row_base = self.screen.idx(r, 0);
            self.evict_wide_partner_at(r, row_base, usize::from(rect.left));
            self.evict_wide_partner_at(r, row_base, usize::from(rect.right));
            for c in rect.left..=rect.right {
                let idx = self.screen.idx(r, c);
                self.screen.cells[idx] = Cell {
                    grapheme,
                    style: self.pen_style,
                    link: self.current_link,
                    sizing: None,
                };
            }
            self.screen.occ_bump_row(r, rect.right.saturating_add(1));
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// `CSI Pt;Pl;Pb;Pr;Pp;Pt';Pl';Pp' $ v`. Page parameters are
    /// accepted but ignored (felis is single-page). Source and
    /// destination are both clipped to the grid; origin mode applies
    /// to both endpoints.
    pub(crate) fn deccra(&mut self, params: &[u16]) {
        let src = Self::rect_params(params, 0, RectParams::UNSET);
        let pt_dest = params.get(5).copied().unwrap_or(0);
        let pl_dest = params.get(6).copied().unwrap_or(0);
        let Some(src) = self.clamp_rectangle(src) else {
            return;
        };
        // The destination is a corner, not a rectangle, so only the
        // origin offset is shared with `clamp_rectangle`.
        let origin = self.rect_origin();
        let pt_dest = if pt_dest == 0 { 1 } else { pt_dest };
        let pl_dest = if pl_dest == 0 { 1 } else { pl_dest };
        let dest_top = pt_dest.saturating_sub(1).saturating_add(origin.row);
        let dest_left = pl_dest.saturating_sub(1).saturating_add(origin.col);
        if dest_top >= self.screen.rows || dest_left >= self.screen.cols {
            return;
        }
        let height = src.bottom - src.top + 1;
        let width = src.right - src.left + 1;
        let dest_bottom = (dest_top + height - 1).min(self.screen.rows - 1);
        let dest_right = (dest_left + width - 1).min(self.screen.cols - 1);
        let copy_rows = dest_bottom - dest_top + 1;
        let copy_cols = dest_right - dest_left + 1;
        // Both endpoints reason over `[0..cols)`, so the copy must
        // neither read nor leave a recycled-scroll stale tail.
        for r in src.top..src.top + copy_rows {
            self.materialize_row_tail(r);
        }
        for r in dest_top..dest_top + copy_rows {
            self.materialize_row_tail(r);
        }
        // Snapshot the source so a self-overlapping copy doesn't smear.
        let mut snapshot = Vec::with_capacity(usize::from(copy_rows) * usize::from(copy_cols));
        let src_end = src.left + copy_cols;
        for r in src.top..src.top + copy_rows {
            let row_at = snapshot.len();
            for c in src.left..src_end {
                let idx = self.screen.idx(r, c);
                let cell = self.screen.cells[idx];
                let cut = cell.sizing.is_some()
                    && !self
                        .sized_block_at(r, c)
                        .is_some_and(|b| b.within(src.top, src.left, copy_rows, copy_cols));
                snapshot.push(if cut { Cell::default() } else { cell });
            }
            // A pair the source edge cuts is not copied as a half.
            if matches!(snapshot[row_at].grapheme, Grapheme::Spacer) {
                snapshot[row_at] = Cell::default();
            }
            if src_end < self.screen.cols
                && matches!(
                    self.screen.cells[self.screen.idx(r, src_end)].grapheme,
                    Grapheme::Spacer
                )
                && let Some(last) = snapshot.last_mut()
            {
                *last = Cell::default();
            }
        }
        if self.screen.has_sized_cells {
            for r in dest_top..dest_top + copy_rows {
                for c in dest_left..dest_left + copy_cols {
                    if let Some(block) = self.sized_block_at(r, c)
                        && !block.within(dest_top, dest_left, copy_rows, copy_cols)
                    {
                        self.clear_sized_block(block);
                    }
                }
            }
        }
        let mut snap_iter = snapshot.into_iter();
        for r in dest_top..dest_top + copy_rows {
            self.erase_pair_across(r, usize::from(dest_left));
            self.erase_pair_across(r, usize::from(dest_right) + 1);
            for c in dest_left..dest_left + copy_cols {
                let idx = self.screen.idx(r, c);
                #[expect(
                    clippy::expect_used,
                    reason = "snapshot was sized to exactly copy_rows×copy_cols just above; exhaustion is a logic bug worth panicking on"
                )]
                let cell = snap_iter.next().expect("snapshot exhausted");
                self.screen.cells[idx] = cell;
            }
            self.screen.occ_bump_row(r, dest_right.saturating_add(1));
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// DECCARA (`CSI Pt;Pl;Pb;Pr;Ps... $ r`). Colors, hyperlinks and the
    /// two protection bits stay as they were; margins do not clip the
    /// area, and DECSACE picks the extent.
    pub(crate) fn deccara(&mut self, params: &[u16], subparams: u32) {
        let rect = Self::rect_params(params, 0, RectParams::UNSET);
        let Some(rect) = self.clamp_rectangle(rect) else {
            return;
        };
        let edit = AttrEdit::from_params(params.get(4..).unwrap_or(&[]), subparams >> 4);
        self.map_rect_attributes(rect, |attrs| {
            attrs.flags = (attrs.flags & !edit.clear) | edit.set;
            if let Some(style) = edit.underline_style {
                attrs.underline_style = style;
            }
        });
    }

    /// DECRARA (`CSI Pt;Pl;Pb;Pr;Ps... $ t`): the same selector list as
    /// DECCARA, toggled rather than assigned. An "off" spelling names
    /// the same attribute as its "on" twin, so `22` reverses bold just
    /// as `1` does.
    pub(crate) fn decrara(&mut self, params: &[u16], subparams: u32) {
        let rect = Self::rect_params(params, 0, RectParams::UNSET);
        let Some(rect) = self.clamp_rectangle(rect) else {
            return;
        };
        let masks = reverse_attr_masks(params.get(4..).unwrap_or(&[]), subparams >> 4);
        self.map_rect_attributes(rect, |attrs| {
            for mask in &masks {
                attrs.flags ^= *mask;
            }
        });
    }

    /// The tail is materialized first: the extent reaches past the
    /// occupancy watermark, where a recycled scroll row left undefined
    /// bytes that must not be read back as attributes.
    fn map_rect_attributes(&mut self, rect: GridRect, f: impl Fn(&mut Attributes)) {
        let stream = self.dec_sace != 2;
        let last_col = self.screen.cols.saturating_sub(1);
        for r in rect.top..=rect.bottom {
            let left = if stream && r != rect.top {
                0
            } else {
                rect.left
            };
            let right = if stream && r != rect.bottom {
                last_col
            } else {
                rect.right
            };
            self.materialize_row_tail(r);
            for c in left..=right {
                let idx = self.screen.idx(r, c);
                let mut attrs = *self
                    .screen
                    .style_table
                    .resolve(self.screen.cells[idx].style);
                f(&mut attrs);
                self.screen.cells[idx].style = self.screen.style_table.intern(attrs);
            }
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// xterm scopes the anchor to the most recent print; any control /
    /// CSI / OSC dispatch in between clears it.
    pub(crate) fn rep(&mut self, n: u16) {
        let n = n.max(1);
        let Some(g) = self.last_printed else {
            return;
        };
        // An ASCII anchor takes the `print_str` bulk path: identical
        // semantics, but the bulk loop hoists the per-cell work that
        // made `CSI 100 b` (kitty's csi benchmark) the grid's hottest
        // path.
        if let Grapheme::Ascii(b) = g {
            let chunk = [b; 64];
            let mut remaining = usize::from(n);
            while remaining > 0 {
                let take = remaining.min(chunk.len());
                self.print_str(&chunk[..take]);
                remaining -= take;
            }
            return;
        }
        // Through `put_grapheme` so wrap / IRM / damage / sizing match
        // a literal byte stream.
        for _ in 0..n {
            self.put_grapheme(g);
        }
    }

    /// DECIC (insert columns). Scoped to the DECSTBM region; no-op
    /// when the cursor is outside it.
    pub(crate) fn decic(&mut self, n: u16) {
        self.shift_columns_at_cursor(n, HDir::Right);
    }

    /// DECDC (delete columns), same scope as DECIC.
    pub(crate) fn decdc(&mut self, n: u16) {
        self.shift_columns_at_cursor(n, HDir::Left);
    }

    fn shift_columns_at_cursor(&mut self, n: u16, dir: HDir) {
        let n = n.max(1);
        let cur_row = self.screen.cursor.row;
        if !self.row_in_scroll_region(cur_row) {
            return;
        }
        let cur_col = self.screen.cursor.col;
        let (left, right_inclusive) = self.effective_left_right();
        // xterm: cursor outside the left-right margins is a no-op.
        if cur_col < left || cur_col > right_inclusive {
            return;
        }
        let edge = right_inclusive + 1;
        let region_top = self.margins.top;
        let region_bottom = self.margins.bottom;
        let blank = self.selective_blank_with_pen_bg();
        let n = n.min(edge - cur_col);
        let (cur, edge_u, n_u) = (usize::from(cur_col), usize::from(edge), usize::from(n));
        let seams = match dir {
            HDir::Right => [cur, edge_u - n_u, edge_u],
            HDir::Left => [cur, cur + n_u, edge_u],
        };
        for r in region_top..=region_bottom {
            self.materialize_row_tail(r);
            for seam in seams {
                self.erase_pair_across(r, seam);
            }
            match dir {
                HDir::Right => {
                    for c in (cur_col + n..edge).rev() {
                        let src = self.screen.idx(r, c - n);
                        let dst = self.screen.idx(r, c);
                        self.screen.cells[dst] = self.screen.cells[src];
                    }
                    for c in cur_col..cur_col + n {
                        let idx = self.screen.idx(r, c);
                        self.screen.cells[idx] = blank;
                    }
                }
                HDir::Left => {
                    for c in cur_col..edge - n {
                        let src = self.screen.idx(r, c + n);
                        let dst = self.screen.idx(r, c);
                        self.screen.cells[dst] = self.screen.cells[src];
                    }
                    for c in edge - n..edge {
                        let idx = self.screen.idx(r, c);
                        self.screen.cells[idx] = blank;
                    }
                }
            }
            self.screen.occ_bump_row(r, edge);
            self.screen.damage.mark(usize::from(r));
        }
    }

    /// DECFI: at the band's right margin the DECSTBM × DECSLRM
    /// rectangle scrolls left by one; right of the band it just
    /// advances (DEC STD 070, esctest's `test_DECFI_RightOfMargin`).
    pub(crate) fn decfi(&mut self) {
        self.index_at_margin(HDir::Left);
    }

    /// DECBI, mirror of DECFI at the left edge (esctest's
    /// `test_DECBI_Scrolls`).
    pub(crate) fn decbi(&mut self) {
        self.index_at_margin(HDir::Right);
    }

    fn index_at_margin(&mut self, dir: HDir) {
        let (left, right) = self.effective_left_right();
        let in_col_band = self.screen.cursor.col >= left && self.screen.cursor.col <= right;
        let in_row_band = self.row_in_scroll_region(self.screen.cursor.row);
        let at_margin = match dir {
            HDir::Left => self.screen.cursor.col == right,
            HDir::Right => self.screen.cursor.col == left,
        };
        if in_col_band && in_row_band && at_margin {
            let blank = self.selective_blank_with_pen_bg();
            let (l, r_incl) = (usize::from(left), usize::from(right));
            let seams = match dir {
                HDir::Left => [l, l + 1, r_incl + 1],
                HDir::Right => [l, r_incl, r_incl + 1],
            };
            for r in self.margins.top..=self.margins.bottom {
                self.materialize_row_tail(r);
                for seam in seams {
                    self.erase_pair_across(r, seam);
                }
                match dir {
                    HDir::Left => {
                        for c in left..right {
                            let src = self.screen.idx(r, c + 1);
                            let dst = self.screen.idx(r, c);
                            self.screen.cells[dst] = self.screen.cells[src];
                        }
                        let idx = self.screen.idx(r, right);
                        self.screen.cells[idx] = blank;
                    }
                    HDir::Right => {
                        for c in (left + 1..=right).rev() {
                            let src = self.screen.idx(r, c - 1);
                            let dst = self.screen.idx(r, c);
                            self.screen.cells[dst] = self.screen.cells[src];
                        }
                        let idx = self.screen.idx(r, left);
                        self.screen.cells[idx] = blank;
                    }
                }
                self.screen.occ_bump_row(r, right.saturating_add(1));
                self.screen.damage.mark(usize::from(r));
            }
            return;
        }
        match dir {
            HDir::Left => {
                if self.screen.cursor.col + 1 < self.screen.cols {
                    self.screen.cursor.col += 1;
                    self.screen.cursor.pending_wrap = false;
                }
            }
            HDir::Right => {
                if self.screen.cursor.col > 0 {
                    self.screen.cursor.col -= 1;
                    self.screen.cursor.pending_wrap = false;
                }
            }
        }
    }

    pub(crate) const fn row_in_scroll_region(&self, row: u16) -> bool {
        row >= self.margins.top && row <= self.margins.bottom
    }

    pub(crate) const fn effective_left_right(&self) -> (u16, u16) {
        if self.left_right_margin_mode {
            (self.margins.left, self.margins.right)
        } else {
            (0, self.screen.cols.saturating_sub(1))
        }
    }

    /// xterm's BCE for synthesized blanks (DECIC / DECDC / DECFI /
    /// DECBI).
    pub(crate) fn selective_blank_with_pen_bg(&mut self) -> Cell {
        let attrs = Attributes {
            fg: Color::Default,
            bg: self.pen.bg,
            underline_color: Color::Default,
            flags: AttrFlags::empty(),
            underline_style: UnderlineStyle::default(),
        };
        Cell {
            grapheme: Grapheme::Empty,
            style: self.screen.style_table.intern(attrs),
            link: None,
            sizing: None,
        }
    }

    /// `DECALN` (`ESC # 8`), vttest's alignment probe. xterm also
    /// drops every margin (esctest's `test_DECALN_ClearsMargins`).
    pub(crate) fn decaln(&mut self) {
        let filler = Cell {
            grapheme: Grapheme::Ascii(b'E'),
            style: StyleId::DEFAULT,
            link: None,
            sizing: None,
        };
        self.screen.cells.fill(filler);
        self.screen.occupancy.fill(self.screen.cols);
        self.screen.soft_wrap.fill(false);
        self.screen.cursor = Cursor::new();
        self.margins.top = 0;
        self.margins.bottom = self.screen.rows.saturating_sub(1);
        self.left_right_margin_mode = false;
        self.margins.left = 0;
        self.margins.right = self.screen.cols.saturating_sub(1);
        self.screen.damage.mark_all();
    }

    /// `CSI Ps t` xterm window manipulation. Only report-only codes
    /// answerable from grid state; side-effect codes (iconify, raise,
    /// resize) are the client's and are accepted-and-ignored here.
    pub(crate) fn xterm_window_op(&mut self, code: u16, sub: u16) {
        // `Pn >= 24` is DECSLPP, a page resize the daemon owns.
        if code >= 24 {
            return;
        }
        match code {
            // `CSI 11 t`: felis is always "shown".
            11 => self.enqueue_response(b"\x1b[1t".to_vec()),
            // `CSI 20 t` / `CSI 21 t`: `OSC L <icon> ST` / `OSC l
            // <title> ST`. An empty payload is still a valid reply, so
            // a probe can tell "supported" from "not supported".
            20 | 21 => {
                let (text, marker) = if code == 20 {
                    (self.icon_name.clone().unwrap_or_default(), b'L')
                } else {
                    (self.title.clone().unwrap_or_default(), b'l')
                };
                let mut response = Vec::with_capacity(8 + text.len());
                response.push(0x1b);
                response.push(b']');
                response.push(marker);
                response.extend_from_slice(text.as_bytes());
                response.push(0x1b);
                response.push(b'\\');
                self.enqueue_response(response);
            }
            // `CSI 22 ; Ps t` push / `CSI 23 ; Ps t` pop; Ps 0 = icon +
            // window, 1 = icon, 2 = window.
            22 => {
                if matches!(sub, 0..=2) {
                    if self.title_stack.len() == TITLE_STACK_LIMIT {
                        self.title_stack.remove(0);
                    }
                    self.title_stack
                        .push((self.title.clone(), self.icon_name.clone()));
                }
            }
            23 => {
                if matches!(sub, 0..=2)
                    && let Some((win, icon)) = self.title_stack.pop()
                {
                    if matches!(sub, 0 | 2) && self.title != win {
                        self.title = win;
                        self.mark_title_changed();
                    }
                    if matches!(sub, 0 | 1) {
                        self.icon_name = icon;
                    }
                }
            }
            // `CSI 13 t`: felis has no position to report (the
            // client's job); reply 0;0 so a probe gets a reply rather
            // than hanging. `sub` matters only with real coordinates.
            13 => self.enqueue_response(b"\x1b[3;0;0t".to_vec()),
            // `CSI 18 t` → `CSI 8 ; Pr ; Pc t`, text-area size in cells.
            18 => {
                let mut response = Vec::with_capacity(16);
                response.extend_from_slice(b"\x1b[8;");
                response.extend_from_slice(self.screen.rows.to_string().as_bytes());
                response.push(b';');
                response.extend_from_slice(self.screen.cols.to_string().as_bytes());
                response.push(b't');
                self.enqueue_response(response);
            }
            // `CSI 19 t` → `CSI 9 ; Pr ; Pc t`, root screen size: felis
            // has no screen larger than its terminal.
            19 => {
                let mut response = Vec::with_capacity(16);
                response.extend_from_slice(b"\x1b[9;");
                response.extend_from_slice(self.screen.rows.to_string().as_bytes());
                response.push(b';');
                response.extend_from_slice(self.screen.cols.to_string().as_bytes());
                response.push(b't');
                self.enqueue_response(response);
            }
            // 14/15/16 (pixel sizes) reply 0;0 so a probe does not
            // hang. Not for want of the numbers: they are one window's
            // fact and this grid is shared by every mirror of the
            // session (docs/reference/protocols/support-matrix.md).
            14 => self.enqueue_response(b"\x1b[4;0;0t".to_vec()),
            15 => self.enqueue_response(b"\x1b[5;0;0t".to_vec()),
            16 => self.enqueue_response(b"\x1b[6;0;0t".to_vec()),
            _ => {}
        }
    }

    /// `DECRQM` mode status report (Ps: 0 unknown, 1 set, 2 reset, 3 perm-set, 4 perm-reset).
    /// Priority: permanently set (Ps=3), functional modes, permanently reset set (Ps=4), DEC soft-state map (default 2),
    /// else 0. ANSI modes other than IRM and LNM report 0.
    pub(crate) fn decrqm(&mut self, mode: u16, private: bool) {
        let report = |state: bool| if state { 1u8 } else { 2u8 };
        let ps: u8 = if private {
            if permanently_set_dec_mode(mode) {
                3
            } else if let Some(state) = self.functional_dec_mode_state(mode) {
                report(state)
            } else if permanently_reset_dec_mode(mode) {
                // xterm reports these as Ps=4. esctest's DECRQM tests
                // carry `@knownBug(terminal="xterm")` expecting the
                // AssertEQ to fail; any other Ps makes the wrapper
                // raise "Should have failed".
                4
            } else if self.dec_mode_states.contains_key(&mode) || known_modifiable_dec_mode(mode) {
                report(self.dec_mode_states.get(&mode).copied().unwrap_or(false))
            } else {
                0
            }
        } else if mode == 4 {
            report(self.insert_mode)
        } else if mode == 20 {
            report(self.linefeed_newline_mode)
        } else if permanently_reset_ansi(mode) {
            4
        } else {
            0
        };
        let mut response = Vec::with_capacity(16);
        response.extend_from_slice(b"\x1b[");
        if private {
            response.push(b'?');
        }
        response.extend_from_slice(mode.to_string().as_bytes());
        response.push(b';');
        response.extend_from_slice(ps.to_string().as_bytes());
        response.extend_from_slice(b"$y");
        self.enqueue_response(response);
    }

    /// xterm `XTERM_SAVE` (`CSI ? Ps s`); unknown modes save `false`.
    pub(crate) fn xterm_save_modes(&mut self, params: &[u16]) {
        for &mode in params {
            let state = self
                .functional_dec_mode_state(mode)
                .or_else(|| self.dec_mode_states.get(&mode).copied())
                .unwrap_or(false);
            self.xterm_save_slots.insert(mode, state);
        }
    }

    /// xterm `XTERM_RESTORE` (`CSI ? Ps r`), replayed through
    /// `apply_dec_private_mode` so every functional side effect lands
    /// as for `DECSET`/`DECRST`. Never-written slots are skipped
    /// (xterm).
    pub(crate) fn xterm_restore_modes(&mut self, params: &[u16]) {
        for &mode in params {
            if let Some(&state) = self.xterm_save_slots.get(&mode) {
                self.apply_dec_private_mode(mode, state);
            }
        }
    }

    /// DECRQCRA: `[Pid, Pp, Pt, Pl, Pb, Pr]`, 1-based; missing
    /// trailing params take xterm's defaults (`Pt=1`, `Pl=1`,
    /// `Pb=rows`, `Pr=cols`). The page is unused (single page).
    pub(crate) fn decrqcra(&mut self, params: &[u16]) {
        let pid = params.first().copied().unwrap_or(0);
        let rect = Self::rect_params(
            params,
            2,
            RectParams {
                pt: 1,
                pl: 1,
                pb: self.screen.rows,
                pr: self.screen.cols,
            },
        );
        let checksum = self.compute_decrqcra_checksum(rect);
        // `DCS Pid ! ~ <hex> ST`: esctest's `escio.ReadDCS()` regex
        // matches these exact bytes.
        let mut out = Vec::with_capacity(20);
        out.extend_from_slice(b"\x1bP");
        out.extend_from_slice(pid.to_string().as_bytes());
        out.extend_from_slice(b"!~");
        out.extend_from_slice(format!("{checksum:04X}").as_bytes());
        out.extend_from_slice(b"\x1b\\");
        self.enqueue_response(out);
    }

    /// xterm's `charproc.c::do_dec_check_sum` (xterm-379): each cell
    /// contributes `-codepoint`, empty cells count as ASCII space
    /// (esctest's `AssertScreenCharsInRectEqual` maps "actual=32 ⇒
    /// expected=0"), and attributes subtract BOLD 1, UNDERLINE 2,
    /// REVERSE 4, BLINK 8, CONCEAL 32. The wire is 4 hex digits.
    pub(crate) fn compute_decrqcra_checksum(&self, rect: RectParams) -> u16 {
        if self.screen.rows == 0 || self.screen.cols == 0 {
            return 0;
        }
        // esctest's `test_DECSET_DECOM_DECRQCRA` queries Rect(1,1,1,1)
        // under DECOM and expects the band's top-left cell.
        let origin = self.rect_origin();
        let rect = GridRect {
            top: (rect.pt.max(1) - 1)
                .saturating_add(origin.row)
                .min(self.screen.rows - 1),
            left: (rect.pl.max(1) - 1)
                .saturating_add(origin.col)
                .min(self.screen.cols - 1),
            bottom: (rect.pb.max(1) - 1)
                .saturating_add(origin.row)
                .min(self.screen.rows - 1),
            right: (rect.pr.max(1) - 1)
                .saturating_add(origin.col)
                .min(self.screen.cols - 1),
        };
        if rect.top > rect.bottom || rect.left > rect.right {
            return 0;
        }
        let mut sum: i32 = 0;
        for r in rect.top..=rect.bottom {
            for c in rect.left..=rect.right {
                let Some(cell) = self.screen.cell(r, c) else {
                    continue;
                };
                // Per xterm, cluster cells contribute their first
                // codepoint (the rest are combining marks).
                let grapheme = cell.grapheme;
                let style = cell.style;
                let ch: u32 = match grapheme {
                    Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => u32::from(b' '),
                    Grapheme::Ascii(b) => u32::from(b),
                    Grapheme::Char(c) => u32::from(c),
                    Grapheme::Cluster(id) => self
                        .screen
                        .cluster_str(id)
                        .and_then(|s| s.chars().next())
                        .map_or_else(|| u32::from(b' '), u32::from),
                };
                #[expect(
                    clippy::cast_possible_wrap,
                    reason = "Unicode scalar ≤ 0x10FFFF never reaches the i32 sign bit"
                )]
                let ch = ch as i32;
                sum = sum.wrapping_sub(ch);
                let flags = self.screen.style(style).flags;
                if flags.contains(AttrFlags::BOLD) {
                    sum = sum.wrapping_sub(1);
                }
                if flags.contains(AttrFlags::UNDERLINE) {
                    sum = sum.wrapping_sub(2);
                }
                if flags.contains(AttrFlags::REVERSE) {
                    sum = sum.wrapping_sub(4);
                }
                if flags.contains(AttrFlags::BLINK) {
                    sum = sum.wrapping_sub(8);
                }
                if flags.contains(AttrFlags::CONCEAL) {
                    sum = sum.wrapping_sub(32);
                }
            }
        }
        sum as u16
    }

    /// `None` when the mode has no functional plumbing; the caller
    /// falls through to soft state.
    pub(crate) const fn functional_dec_mode_state(&self, mode: u16) -> Option<bool> {
        Some(match mode {
            1 => self.application_cursor,
            5 => self.reverse_video,
            6 => self.origin_mode,
            7 => self.autowrap,
            25 => self.screen.cursor.visible,
            41 => self.more_fix,
            45 => self.reverse_wrap_inline,
            1045 => self.reverse_wrap_extend,
            // Toggling ?69 off resets the margins in the DECSET arm,
            // so the field still reflects "what was last written".
            69 => self.left_right_margin_mode,
            1049 => self.screen.on_alternate_screen(),
            2004 => self.bracketed_paste,
            1004 => self.focus_reporting,
            2026 => self.synchronized_output(),
            // Mode 2027 (Grapheme Cluster Mode, Kitty / contour) is
            // always set: REQ-602 makes UAX#29 width an invariant with
            // no legacy path to leave. Advertising it lets notcurses /
            // neovim / fish skip their UAX#29-broken fallbacks.
            2027 => true,
            2031 => self.color_scheme_notify,
            2048 => self.in_band_resize_notify,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests;
