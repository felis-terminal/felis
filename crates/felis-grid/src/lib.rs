//! `felis-grid`: cells, cursor, damage tracker, scrollback, image store.

#![cfg_attr(not(test), forbid(unsafe_code))]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::{NonZeroU16, NonZeroU32},
    time::{Duration, Instant},
};

pub use felis_protocol::kitty_keyboard::KittyKbdFlags;
pub use felis_protocol::kitty_text_sizing::{HAlign, Sizing, VAlign};
pub use felis_protocol::messages::CursorStyle;
pub use felis_protocol::messages::ScrollDirection;
pub use felis_protocol::messages::{
    ClipboardSelection, ClipboardWrite, ModifyOtherKeys, MouseProtocol,
};
use felis_protocol::messages::{PromptJump, PromptKind, ThemeChannel};
use felis_vt::Sink;
/// Re-exported so `felis-render-wgpu` can flag Trojan-Source characters
/// without depending on `felis-vt` (`docs/explanation/architecture/overview.md`
/// "Workspace: the crate-boundary decision record").
pub use felis_vt::bidi;
use serde::{Deserialize, Serialize};

/// 1-based registry index handed out by [`Grid::install_sizing`]; the
/// wire layer packs 0 as "default sizing".
pub type SizingHandle = NonZeroU16;

/// Text of one interned grapheme cluster, bounded to [`Self::CAP`] bytes.
///
/// Follows UAX #15 Stream-Safe bounds to prevent unbounded memory growth;
/// text exceeding the cap stops folding rather than being truncated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClusterText(Box<str>);

impl Default for ClusterText {
    fn default() -> Self {
        Self(Box::from(""))
    }
}

impl ClusterText {
    /// Longest cluster that may be interned, in UTF-8 bytes.
    pub const CAP: usize = 128;

    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        (text.len() <= Self::CAP).then(|| Self(text.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ClusterText {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for ClusterText {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for ClusterText {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl std::fmt::Display for ClusterText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptMark {
    /// Absolute line the mark fired on: `scrollback_total_pushed +
    /// cursor.row` at OSC 133 time. Survives scrolling and resize where
    /// a screen row would not; map back to a current location with
    /// [`Grid::locate_line`]. See docs/explanation/data-model/scrollback.md.
    pub line: u64,
    pub kind: PromptKind,
    /// Only set for `OSC 133 ; D ; <code>`.
    pub exit_code: Option<u32>,
}

/// The `redraw` option of the latest `OSC 133 ; A`, a kitty extension:
/// how much of the prompt the shell repaints after a resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptRedraw {
    Full,
    /// `redraw=last` (Ghostty): bash repaints only the cursor's line.
    LastLine,
    /// `redraw=0`.
    Never,
}

/// What the primary screen's `OSC 133` marks say about the prompt a
/// resize may blank. Marks sent on the alternate screen belong to a
/// program inside a TUI and never reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellPrompt {
    /// Set by the first `C`. A shell that marks prompts but never
    /// command starts leaves a running command's output looking like
    /// part of the prompt.
    marks_commands: bool,
    /// Between a `C` and the next `D`. An `A` then comes from a program
    /// the command runs (a nested shell, or one `exec` replaced the
    /// marking shell with), which may not mark its own commands.
    command_open: bool,
    /// Between an `A` or `B` and the next `C` or `D`.
    at_prompt: bool,
    redraw: PromptRedraw,
    /// Absolute ordinal (see [`Grid::prompt_marks_pruned`]) of the
    /// youngest `A` that is not `k=s`, when it fired at column 0: a
    /// prompt sharing its first row with output that lacked a final
    /// newline cannot be blanked without erasing that output.
    start: Option<u64>,
}

impl ShellPrompt {
    const fn new() -> Self {
        Self {
            marks_commands: false,
            command_open: false,
            at_prompt: false,
            redraw: PromptRedraw::Full,
            start: None,
        }
    }

    fn observe(&mut self, mark: &Osc133, ordinal: u64, at_column_0: bool) {
        match mark.kind {
            PromptKind::PromptStart if !mark.secondary => {
                // Every prompt re-declares the option, as in kitty, so a
                // `redraw=0` shell exec'ing into another shell leaves no
                // opt-out behind. A `k=s` line inherits its prompt's.
                self.redraw = mark.redraw.unwrap_or(PromptRedraw::Full);
                self.start = (at_column_0 && !self.command_open).then_some(ordinal);
                self.at_prompt = true;
            }
            PromptKind::PromptStart | PromptKind::InputStart => self.at_prompt = true,
            PromptKind::OutputStart => {
                self.marks_commands = true;
                self.command_open = true;
                self.at_prompt = false;
            }
            PromptKind::CommandEnd => {
                self.command_open = false;
                self.at_prompt = false;
            }
        }
    }
}

/// Where an absolute prompt-mark line currently sits
/// (docs/explanation/data-model/scrollback.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkLocation {
    /// On the live screen at this 0-based row.
    Screen(u16),
    /// In scrollback at this index (0 = oldest retained row), the same
    /// indexing [`ScrollbackView::row`] takes.
    Scrollback(usize),
    Evicted,
}

/// Rows the parser erased on the live region. The Kitty graphics
/// dispatcher drops any placement whose cells intersect (the spec's
/// auto-deletion rule for placements without `C=1`); `force` (RIS /
/// DECSTR) wipes even `C=1` placements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErasedRange {
    /// First erased row (inclusive).
    pub top: u16,
    /// Last erased row (inclusive).
    pub bottom: u16,
    pub force: bool,
}

/// One scroll-region shift recorded on the grid for
/// docs/reference/ipc.md scroll-aware emission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollOp {
    /// First row of the scrolled region (inclusive).
    pub region_top: u16,
    /// Last row of the scrolled region (inclusive).
    pub region_bottom: u16,
    /// Number of rows the region shifted; always ≥ 1.
    pub n_rows: u16,
    /// `Up` for line-feed past the bottom margin and `IND`; `Down`
    /// for `RI` and `\n` past the top in DECOM.
    pub direction: ScrollDirection,
}

/// Edge-triggered alt-screen toggle. The daemon saves / restores the
/// Kitty graphics placement table on it (placements are per-screen, as
/// in Kitty). A redundant `?1049h`/`?1049l` queues nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenSwitch {
    /// `?1049h`.
    EnteredAlternate,
    /// `?1049l`.
    LeftAlternate,
}

/// One parser side-effect, kept in byte-stream order.
///
/// Kept in a single queue because draining kind-by-kind inverts event order
/// within PTY bursts, breaking producer sequencing expectations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEffect {
    /// Host-bound reply bytes the daemon writes to the PTY.
    Response(Vec<u8>),
    /// One buffered Kitty graphics APC body for the daemon's
    /// dispatcher (`docs/explanation/protocols/kitty-graphics.md` "Dispatcher
    /// architecture").
    Apc(ApcBody),
    /// A row range erased by ED / EL; the dispatcher evicts
    /// intersecting placements.
    Erased(ErasedRange),
    /// Rows the primary screen pushed into scrollback. Adjacent scrolls
    /// coalesce into one entry.
    ScrolledIntoScrollback(u32),
    /// Rows a full-screen scroll on the alternate screen dropped off its
    /// top: placements move up with the text and leave with it.
    AltScreenScrolled(u32),
    /// RIS, after the forced erase that clears the live placements: the
    /// primary screen saved while on the alternate one goes too.
    HardReset,
    /// A whole-row shift of a band (docs/reference/ipc.md `Scrolled`),
    /// with the grid's damage already moved along with the rows.
    /// `geometry_gen` names the grid generation; `first_seq..=last_seq`
    /// is the span of the scroll order the entry covers, more than one
    /// step once adjacent shifts of the band have coalesced.
    Scrolled {
        op: ScrollOp,
        geometry_gen: u64,
        first_seq: u64,
        last_seq: u64,
    },
    /// Edge-triggered alt-screen toggle; see [`ScreenSwitch`].
    ScreenSwitch(ScreenSwitch),
    /// Primary screen re-wrapped from inside the byte stream (`?1049l` deferred resize).
    ///
    /// Queued rather than returned so it applies after [`ScreenSwitch::LeftAlternate`]
    /// restores saved primary placements (REQ-604).
    PrimaryReflowed(ReflowRemap),
    /// `?2004l` while bracketed paste was on. The daemon logs a
    /// warning on drain (REQ-902); edge-triggered so a reset storm
    /// doesn't flood the log.
    BracketedPasteDisabled,
}

/// Grid → ANSI/SGR re-encoding for the pipe action, `felis sessions
/// capture --ansi`, and the host-terminal clients.
pub mod ansi;
mod cluster_table;
mod damage;
mod editing;
pub mod images;
/// `felis-json` v1: the named, versioned JSON form of a frame body
/// for the consumers that re-expose or record a session as JSON.
#[cfg(feature = "json")]
pub mod json_v1;
mod link_table;
mod modes;
pub mod mouse;
mod osc52;
mod osc_color;
mod osc_dispatch;
mod placeholder_resolve;
pub mod pty_effects;
mod screen;
pub mod search;
mod sgr;
mod sink;
mod style_table;
mod table_gc;
mod text_cells;
mod text_sizing;
mod uax29;
pub mod wire;
mod xtgettcap;

pub use ansi::{
    AnsiCaps, RowAnsiOptions, logical_line_spans, logical_lines, push_cell_text, row_ansi,
    row_ansi_with, row_text_trim, sgr_set,
};
pub use cluster_table::{CLUSTER_TABLE_CAP, ClusterTable};
pub use damage::Damage;
pub use link_table::{LINK_TABLE_BYTE_CAP, LinkTable, LinkText};
pub use modes::{
    KNOWN_MODIFIABLE_DEC_MODES, PERMANENTLY_RESET_ANSI_MODES, PERMANENTLY_RESET_DEC_MODES,
    PERMANENTLY_SET_DEC_MODES,
};
use modes::{
    known_modifiable_dec_mode, permanently_reset_ansi, permanently_reset_dec_mode,
    permanently_set_dec_mode,
};
pub use mouse::encode_mouse;
use osc_color::{format_osc_color_response, parse_x_color};
use osc52::{format_osc_52_response, parse_osc_52_selection};
pub use placeholder_resolve::PlaceholderCell;
pub use pty_effects::{APC_OUTBOX_CAP, PtyEffectQueue};
use screen::SavedScreen;
pub use screen::ScreenBuffer;
pub use search::{SearchCursor, SearchHit, SearchOptions, SearchQuery, SearchQueryError, row_text};
pub use sgr::{AttrFlags, Attributes, Color, UnderlineStyle};
pub use style_table::{StyleId, StyleTable};
pub use table_gc::{Sweepable, TableGc};
pub use text_cells::{TextCells, text_cells};
pub use wire::{DecodedRow, RowCodecError, RowEncode, decode_row, encode_row};
use xtgettcap::{ascii_to_hex, hex_to_ascii, xtgettcap_value};

pub const DEFAULT_SCROLLBACK_ROWS: usize = 10_000;

/// Must stay `Copy`: the scroll / scrollback / IL-DL bulk paths rely on
/// `copy_within` / `slice::fill` (~12x over per-element `clone`), which
/// is why cluster text lives in the grid's `cluster_table` rather than
/// in [`Grapheme::Cluster`] (`docs/explanation/data-model/grid-and-cells.md`
/// "Cluster interning").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub grapheme: Grapheme,
    /// Interned pen; resolve via [`Grid::style`]. Grid-local: the wire
    /// ships resolved [`Attributes`] (docs/explanation/data-model/grid-and-cells.md
    /// "Style interning").
    pub style: StyleId,
    /// `OSC 8` hyperlink handle into the grid's link table.
    pub link: Option<NonZeroU16>,
    /// `OSC 66` text-sizing handle into the grid's `sizing_table`.
    pub sizing: Option<SizingHandle>,
}

/// Reserves capacity upfront so [`Grid::grow_ring`] never reallocates.
///
/// Growing stepwise leaves the allocator holding freed intermediate buffers
/// unreturned to the OS; unfaulted reserved pages avoid resident memory cost.
fn ring_cells(len: usize, ring_rows: usize, cols: usize) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(ring_rows.saturating_mul(cols).max(len));
    cells.resize(len, Cell::default());
    cells
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            grapheme: Grapheme::Empty,
            style: StyleId::DEFAULT,
            link: None,
            sizing: None,
        }
    }
}

/// Returned by occupancy-clipping accessors ([`Grid::cell`]) for columns
/// past a row's watermark, where the backing store holds stale bytes.
static BLANK_CELL: Cell = Cell::BLANK;

impl Cell {
    pub const BLANK: Self = Self {
        grapheme: Grapheme::Empty,
        style: StyleId::DEFAULT,
        link: None,
        sizing: None,
    };

    /// A linked space counts as non-blank. The bare id compare is sound
    /// because interning pins the default pen at [`StyleId::DEFAULT`],
    /// so a colored blank (BCE) can never alias it.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        matches!(self.grapheme, Grapheme::Empty)
            && self.style == StyleId::DEFAULT
            && self.link.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grapheme {
    Empty,
    /// 7-bit printable ASCII (`0x20..=0x7E`); no allocation.
    Ascii(u8),
    Char(char),
    /// 1-based handle into the grid's interned `cluster_table`
    /// (`docs/explanation/data-model/grid-and-cells.md` "Cluster interning").
    Cluster(NonZeroU32),
    /// Right half of a double-wide glyph stored in the previous cell;
    /// the renderer skips it.
    Spacer,
    /// Continuation cell of an OSC 66 sized run (REQ-603), carrying the
    /// primary's sizing handle. Unlike `Spacer`, the owner can be
    /// `cw × w × s` cells away and `s - 1` rows up. The bg pass still
    /// emits its quad.
    SizedSpacer,
}

/// `col` is always strictly less than the grid's column count; the xterm
/// "last column flag" is [`Self::pending_wrap`]: a print in the rightmost
/// column sets it and the next print wraps before drawing, any explicit
/// cursor movement clears it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
    pub visible: bool,
    pub pending_wrap: bool,
}

impl Cursor {
    const fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            pending_wrap: false,
        }
    }
}

/// State captured by DECSC / SCOSC / DECSET 1048. Includes origin mode
/// because xterm restores it too (esctest's
/// `test_SaveRestoreCursor_ResetsOriginMode`). `Default` is a fresh
/// terminal so a `DECRC` after `DECSTR` returns to the upper-left.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SavedCursor {
    cursor: Cursor,
    pen: Attributes,
    origin_mode: bool,
}

/// Where a visible row under a lifted viewport sources its content.
enum ViewportRow {
    Live(u16),
    Scrollback(usize),
}

/// One composed-view row, resolved through the viewport seam exactly
/// once. See [`Grid::viewport_row`].
pub struct ViewportRowView<'a> {
    /// The row's cells in column order, occupancy-clipped: columns
    /// from `cells.len()` to the grid width are [`Cell::BLANK`].
    pub cells: &'a [Cell],
    pub soft_wrap_continued: bool,
    /// Sparse `(col, Sizing)` OSC 66 band; always empty for a scrollback row.
    pub sized_cells: Vec<(u16, Sizing)>,
}

/// The scrolling region's four margins, 0-based inclusive. The vertical
/// pair is DECSTBM; the horizontal pair is DECSLRM, consulted only while
/// DECLRMM (`?69`) is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Margins {
    /// DECSTBM top margin (0-based, inclusive). Default `0`. Rows
    /// Rows scrolled off the top reach scrollback only when `top == 0`
    /// (xterm: a program-defined sub-region does not pollute history).
    top: u16,
    /// An invalid `CSI r` (top ≥ bottom, or bottom past the screen)
    /// resets to full screen.
    bottom: u16,
    left: u16,
    right: u16,
}

impl Margins {
    const fn new(rows: u16, cols: u16) -> Self {
        Self {
            top: 0,
            bottom: rows - 1,
            left: 0,
            right: cols.saturating_sub(1),
        }
    }
}

/// Kitty keyboard progressive-enhancement protocol state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct KittyKbd {
    /// Top entry is the active flag bitmap; empty means legacy encoding.
    /// Pushes past [`KITTY_KBD_STACK_LIMIT`] evict the bottom entry per
    /// the spec.
    stack: Vec<KittyKbdFlags>,
    /// Active flags as last reported via `take_kitty_kbd_dirty`.
    last_emitted: KittyKbdFlags,
    dirty: bool,
}

/// `?2026` (BSU/ESU) synchronized-output state. The deadline is `None`
/// until the first `synchronized_output_deadline(now)` observation after
/// BSU, so the parser path reads no wall clock (REQ-1004). One enum
/// rather than a mode bool plus a deadline field so a stale deadline
/// cannot survive behind a cleared flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SyncOutput {
    #[default]
    Off,
    On {
        /// Anchored on first observation; a duplicate BSU does not move
        /// it, so spamming `?2026h` cannot pin emission past 150 ms.
        deadline: Option<Instant>,
    },
}

/// Implements [`Sink`] so the VT parser can drive it directly.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, PartialEq, Eq)]
pub struct Grid {
    /// Exactly the surface the wire mirrors into an attached window
    /// ([`ScreenBuffer`]); everything else on `Grid` is parser state no
    /// client receives.
    screen: ScreenBuffer,
    /// S7C1T / S8C1T: responses use 8-bit C1 bytes when set.
    c1_8bit: bool,
    /// DECSCL level, 1..=5, default 4 (matches our DA1). DECRQM requires
    /// `>= 3`, DECSLRM `>= 4` (esctest pins both). DECSTR preserves the
    /// level per the VT510 spec.
    conformance_level: u8,
    pen: Attributes,
    /// Invariant: `style_table.resolve(pen_style) == pen`. Kept eager
    /// rather than lazy so equal grids hold equal ids and the derived
    /// `PartialEq` stays meaningful.
    pen_style: StyleId,
    /// `?2004`; the daemon wraps client paste bytes while set.
    bracketed_paste: bool,
    /// `?1` DECCKM, mirrored to the client via [`ModeSnapshot`].
    application_cursor: bool,
    /// xterm `modifyOtherKeys` level (`CSI > 4 ; Pv m`), mirrored via
    /// [`ModeSnapshot`] so the keyboard encoder emits
    /// `CSI keycode ; mod u` (REQ-506).
    modify_other_keys: ModifyOtherKeys,
    /// `?1004`; the daemon forwards focus changes while set.
    focus_reporting: bool,
    /// `?2026` (BSU/ESU); see [`SyncOutput`].
    sync_output: SyncOutput,
    margins: Margins,
    /// DECOM (`?6`). Preserved across DECSC / DECRC; reset by DECSTR / RIS.
    origin_mode: bool,
    /// DECAWM (`?7`). When clear, prints at the last column overwrite
    /// the same cell with no advance (vttest's right-margin test).
    autowrap: bool,
    /// IRM (`CSI 4 h/l`). Reset by DECSTR.
    insert_mode: bool,
    /// LNM (`CSI 20 h/l`): LF / VT / FF perform a CR first.
    linefeed_newline_mode: bool,
    /// One entry per column. Preserved across DECSC / DECRC but rebuilt
    /// by DECSTR / RIS / resize so it never lags the column count.
    tab_stops: Vec<bool>,
    /// `?5` DECSCNM, mirrored via [`ModeSnapshot`]; the renderer XORs it
    /// with each cell's SGR 7. Screen-global: survives an alt-screen
    /// switch, only DECSTR clears it.
    reverse_video: bool,
    /// `Some` between `dcs_hook` and `dcs_unhook`.
    dcs: Option<DcsState>,
    /// Last graphic grapheme printed, for REP (`CSI Pn b`); cleared by
    /// any control / CSI / OSC dispatch so an REP after CUP repeats
    /// nothing.
    last_printed: Option<Grapheme>,
    /// Armed when the previous print folded a ZWJ into a cell's cluster,
    /// so the following base joins that cluster; the C0 controls that
    /// move the cursor (BS, HT, LF, VT, FF, CR) disarm it.
    zwj_pending: bool,
    /// Kept only while nothing but SGR, OSC or BEL separates the
    /// overrides from the next print, so the marker cannot land on a
    /// character somewhere else.
    pending_bidi: editing::PendingBidi,
    /// Mirrors the daemon's graphics reassembler over the APC bodies
    /// this grid admits, so it knows which body completes a command.
    graphics_tracker: felis_vt::kitty_graphics::ReassemblyTracker,
    /// Set by an admitted APC that completes a cursor-moving placement;
    /// taken by [`felis_vt::Sink::take_yield`].
    apc_yield: bool,
    /// `ESC =` / `ESC >` DECKPAM / DECKPNM, mirrored via [`ModeSnapshot`].
    application_keypad: bool,
    /// `?9001` win32-input-mode, requested by `ConPTY` on startup and by
    /// `PSReadLine`; mirrored via [`ModeSnapshot`] so the key encoder
    /// emits `CSI Vk;Sc;Uc;Kd;Cs;Rc _` records.
    win32_input_mode: bool,
    /// `OSC 0` / `OSC 2`.
    title: Option<String>,
    /// `OSC 1`; `OSC 0` sets both. Separate from the title because
    /// vttest's title-stack tests check the two channels independently.
    icon_name: Option<String>,
    /// xterm title stack (`CSI 22 / 23 t`): (title, icon name) per
    /// entry. Capped at [`TITLE_STACK_LIMIT`], FIFO eviction.
    title_stack: Vec<(Option<String>, Option<String>)>,
    title_dirty: bool,
    /// Counts title changes. A second consumer cannot share
    /// [`Self::title_dirty`]: the facet push takes that flag, and a take
    /// while nobody is attached would hide the change from the daemon's
    /// session-roster mirror.
    title_epoch: u64,
    /// `OSC 7` working directory (a `file://hostname/path` URL).
    cwd: Option<String>,
    cwd_dirty: bool,
    /// Counts cwd changes; see [`Self::title_epoch`].
    cwd_epoch: u64,
    /// `OSC 22` pointer shape (kitty extension); `None` is the default
    /// arrow. Carries no damage.
    pointer_shape: Option<String>,
    /// Shapes saved below the current one by `OSC 22 ; > name`;
    /// `OSC 22 ; <` pops. Bounded (see [`Self::push_pointer_shape`]).
    pointer_shape_stack: Vec<Option<String>>,
    pointer_shape_dirty: bool,
    /// `DECSET 2031`. The daemon writes the `CSI ? 997 ; Ps n` report to
    /// the PTY only while this is on (kitty / ghostty / contour
    /// convention).
    color_scheme_notify: bool,
    /// `DECSET 2048`. The daemon writes the
    /// `CSI 48 ; rows ; cols ; h ; w t` report to the PTY only while
    /// this is on, the set's own answer included.
    in_band_resize_notify: bool,
    /// Bumped by every `DECRST 2048`. The daemon dedups its reports by
    /// geometry, and a reset coalesced with a re-set inside one parse
    /// burst leaves no other trace that the promised answer is owed.
    resize_notify_epoch: u32,
    /// Last OS color preference the client reported (`true` = dark);
    /// `DSR ? 996 n` before any report answers with the light default.
    os_dark: Option<bool>,
    /// `OSC 133` marks, oldest first, front-pruned once a mark falls
    /// below the retained-scrollback floor; the daemon's per-connection
    /// cursors translate through [`Self::prompt_marks_pruned`]
    /// (docs/explanation/data-model/scrollback.md).
    prompt_marks: Vec<PromptMark>,
    /// Marks drained from the front of `prompt_marks` over the session's
    /// life: the absolute ordinal of `prompt_marks[i]` is
    /// `prompt_marks_pruned + i`. Saturating.
    prompt_marks_pruned: u64,
    shell_prompt: ShellPrompt,
    /// Cumulative rows ever pushed into scrollback: the absolute-line
    /// origin for [`PromptMark`] (docs/explanation/data-model/scrollback.md).
    /// Never drained (contrast [`Self::scrolled_into_scrollback`]).
    /// Saturating.
    scrollback_total_pushed: u64,
    /// `XTERM_SAVE` / `XTERM_RESTORE` (`CSI ? Ps s` / `CSI ? Ps r`)
    /// slots per private mode; RESTORE on a never-written slot is a
    /// no-op.
    xterm_save_slots: HashMap<u16, bool>,
    /// `DECSACE` (`CSI Ps * x`): `2` selects the rectangle extent for
    /// DECCARA / DECRARA, anything else the stream. DECRQSS round-trips
    /// the raw value (esctest's `test_DECRQSS_DECSACE`).
    dec_sace: u16,
    /// DECLRMM (`?69`). When reset, `CSI s` falls through to SCOSC and
    /// the horizontal margins are ignored.
    left_right_margin_mode: bool,
    /// `?41` xterm `MoreFix`: HT with `pending_wrap` engaged performs an
    /// implicit linefeed first (esctest's `test_DECSET_MoreFix`).
    more_fix: bool,
    /// `?45` `ReverseWrapInline`. xterm (patch 380+) limits it to
    /// wrapped lines: a backward move crosses the left edge only when
    /// the current row is an autowrap continuation, and never around
    /// the top margin. Requires DECAWM.
    reverse_wrap_inline: bool,
    /// `?1045` `ReverseWrapExtend`: unconditional reverse-wraparound,
    /// and at `scroll_top` the cursor wraps to `scroll_bottom`. Requires
    /// DECAWM.
    reverse_wrap_extend: bool,
    /// `?40` `Allow80To132`: when reset (xterm default) DECCOLM is a
    /// no-op; when set, DECCOLM clears the screen, homes the cursor, and
    /// resets margins. felis does not honor the column-count change
    /// (that needs the daemon-coordinated resize).
    allow_80_to_132: bool,
    /// `?95` DECNCSM. Functional only at Level 5+, where a DECCOLM
    /// toggle skips the ED 2 step; at Level 4 it is soft state DECRQM
    /// reports.
    dec_ncsm: bool,
    /// `OSC 10/11/12` overrides, `[fg, bg, cursor]` indexed by
    /// `ThemeChannel as usize`; `None` means "renderer default".
    theme_overrides: [Option<(u8, u8, u8)>; 3],
    theme_dirty: [bool; 3],
    /// Attached client's configured `[fg, bg, cursor]`, set on attach.
    /// Consulted by `OSC 10/11/12 ; ?` before xterm fallback so background
    /// detectors see actual surface colors. Not VT state (not saved/reset).
    theme_config_defaults: [Option<(u8, u8, u8)>; 3],
    /// `OSC 4` overrides, sparse over [`default_palette_color`]. Answers
    /// OSC 4 queries (REQ-203) and, mirrored via `GridMsg::PaletteColor`,
    /// is what the renderer resolves `Color::Indexed` against. A
    /// `BTreeMap` so [`Self::palette_overrides`] and
    /// [`Self::take_palette_dirty`] agree on order without sorting.
    palette_overrides: BTreeMap<u8, (u8, u8, u8)>,
    palette_dirty: BTreeSet<u8>,
    /// A whole-table reset (`OSC 104` bare, DECSCL, RIS) not shipped
    /// yet. Emitted ahead of the per-index deltas; arming it clears
    /// [`Self::palette_dirty`], since everything queued before the
    /// reset is subsumed by it.
    palette_reset_all_pending: bool,
    /// DEC private modes with no functional effect that must still
    /// round-trip DECSET/DECRESET/DECRQM; absent means "reset".
    dec_mode_states: HashMap<u16, bool, foldhash::fast::FixedState>,
    /// xterm special colors (OSC 5 / OSC 4 with `idx >= 256`): 0=bold,
    /// 1=underline, 2=blink, 3=reverse, 4=italic. `None` queries echo
    /// `default_special_color` so reset-then-query round-trips.
    special_color_overrides: [Option<(u8, u8, u8)>; SPECIAL_COLOR_COUNT],
    mouse_protocol: MouseProtocol,
    /// `?1006` SGR / `?1016` SGR-pixels / default X10. `?1005` and
    /// `?1015` are accepted as no-ops per
    /// `docs/reference/protocols/support-matrix.md` "Mouse and focus".
    mouse_encoding: MouseEncoding,
    /// DECSC / SCOSC / DECSET 1048 slot. Non-`Option`: a restore with no
    /// prior save returns to (0, 0) with default pen and origin mode off
    /// (esctest's `test_*_MoveToHomeWhenNotSaved`); DECSTR resets it to
    /// [`SavedCursor::default`].
    saved_cursor: SavedCursor,
    /// The inactive screen's slot, swapped with [`Self::saved_cursor`]
    /// on alt-screen enter / leave (esctest's
    /// `test_SaveRestoreCursor_AltVsMain`).
    inactive_saved_cursor: SavedCursor,
    /// `OSC 8` link applied to printed cells.
    current_link: Option<NonZeroU16>,
    /// Active OSC 66 sizing context, applied to printed cells.
    current_sizing_handle: Option<SizingHandle>,
    kitty_kbd: KittyKbd,
    /// Parser side-effects in byte-stream order since the last
    /// `take_pty_effects` drain; see [`PtyEffect`]. APC entries are
    /// capped at [`APC_OUTBOX_CAP`] per drain: overflow drops and flags
    /// the bell. The cap lives in [`PtyEffectQueue`] so no path here can
    /// add an APC that escapes it.
    pty_effects: PtyEffectQueue,
    /// BEL since the last `take_bell_pending` drain; the daemon ferries
    /// it as `GridMsg::Attention`. Coalesced.
    bell_pending: bool,
    /// OSC 9 / 99 / 777 notifications awaiting relay
    /// (`docs/reference/protocols/notifications.md`). Capped at
    /// `NOTIFY_OUTBOX_CAP` per drain; further ones drop with BEL flagged.
    notifications: Vec<felis_vt::notification::Notification>,
    /// In-flight OSC 99 multi-chunk reassembly keyed by `i=`. Bounded
    /// (`NOTIFY_REASSEMBLY_*`) so a producer that never sends `d=1`
    /// cannot leak.
    osc99_pending: HashMap<String, PartialNotification>,
    /// Latest `OSC 52` write for the daemon to forward. Replaced, not
    /// queued: programs that set the clipboard repeatedly want only the
    /// last write to land.
    pending_clipboard_set: Option<ClipboardWrite>,
    /// Last value stored under the clipboard selection (`c`). `?`
    /// queries answer from here, not the OS clipboard, so a program
    /// cannot read data the user did not put there via OSC 52 (tmux's
    /// `set-clipboard external` model).
    clipboard_cache_clipboard: Option<Vec<u8>>,
    /// Same for the primary selection (`p`).
    clipboard_cache_primary: Option<Vec<u8>>,
    /// Held here so a multi-byte sequence keeps state across `print`
    /// calls.
    utf8: felis_vt::utf8::Decoder,
    /// The `TERM` the host stamped on the child; XTGETTCAP `TN` answers
    /// it, and reports the cap unknown while it is unset.
    term_name: Option<Box<str>>,
}

/// Matches the depth the spec's reference implementation enforces.
pub const KITTY_KBD_STACK_LIMIT: usize = 32;

/// 32 covers tmux-in-screen-in-shell nesting.
pub const TITLE_STACK_LIMIT: usize = 32;

/// DCS body cap. DECRQSS bodies are 1-3 bytes, but an XTGETTCAP body is
/// a `;`-joined list of hex cap names and a multi-cap probe runs well
/// past a dozen bytes; 512 fits any realistic probe while bounding the
/// buffer (REQ-903 / REQ-904). Past the cap the query is treated as
/// invalid.
const DCS_BUFFER_LIMIT: usize = 512;

/// Cap on decoded notifications between `take_notifications` drains
/// (`docs/reference/protocols/notifications.md`); past it the
/// notification drops and the bell flags.
pub const NOTIFY_OUTBOX_CAP: usize = 64;

/// Cap on distinct in-flight OSC 99 reassembly ids; once full, new ids
/// drop until a `d=1` clears room.
const NOTIFY_REASSEMBLY_IDS: usize = 32;

/// Cap on reassembled bytes (title + body) per OSC 99 notification;
/// past it the in-flight entry drops.
const NOTIFY_REASSEMBLY_BYTES: usize = 64 * 1024;

/// One in-flight OSC 99 notification
/// (`docs/reference/protocols/notifications.md`). Title and body
/// accumulate separately because a producer interleaves `p=title` /
/// `p=body` chunks under one `i=`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PartialNotification {
    pub title: String,
    pub body: String,
    /// Latest non-absent `u=` urgency seen on any chunk.
    pub urgency: felis_protocol::messages::Urgency,
}

/// One buffered APC body with the cursor position when delivered.
///
/// Anchors placements at this captured position rather than the live cursor,
/// since producers may restore cursor position (DECRC) before APC drainage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApcBody {
    pub body: Vec<u8>,
    /// Cursor row (0-based) when the parser delivered the body.
    pub cursor_row: u16,
    /// Cursor column (0-based) when the parser delivered the body.
    pub cursor_col: u16,
}

/// xterm special colors: bold, underline, blink, reverse, italic.
/// Addressable via `OSC 5 ; idx` and via `OSC 4 ; idx + Co` where `Co`
/// is terminfo's indexed-color count (esctest's
/// `ChangeSpecialColorTests`).
const SPECIAL_COLOR_COUNT: usize = 5;

/// xterm's `Co` for the default 256-color palette: `OSC 4` indices
/// `256..=260` alias `special_color_overrides[0..=4]`.
const SPECIAL_COLOR_OSC4_OFFSET: u16 = 256;

/// DA1 reply; see the `CSI c` arm for the feature bits. Shared with
/// `ESC Z` (DECID) so the two answers stay byte-identical, as terminfo
/// probes sending DECID instead of DA1 expect.
const DA1_REPLY: &[u8] = b"\x1b[?64;1;2;6;9;15;16;17;18;21;22;28;29c";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DcsState {
    kind: DcsKind,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum DcsKind {
    /// A DCS shape felis does not service; the body still counts
    /// toward the cap.
    #[default]
    Other,
    /// `DCS $ q <body> ST`: Request Status String.
    Decrqss,
    /// `DCS + q <hex>; … ST`; reply is `DCS 1+r <hex>=<hexvalue> [; …] ST`
    /// (or `0+r <hex>` for unknown caps).
    Xtgettcap,
}

/// Masks before the cast: a plain `u8::try_from` discards the low
/// byte's known bits whenever a producer parks nonsense in the high
/// bits.
fn kitty_kbd_known_flags(raw: u16) -> KittyKbdFlags {
    KittyKbdFlags::from_bits_truncate((raw & u16::from(KittyKbdFlags::all().bits())) as u8)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlinkEntry {
    /// `id=…` parameter (xterm's "anchor"); `None` when the producer
    /// supplied none.
    pub id: Option<LinkText>,
    /// Sanitized `URI` payload (control bytes rejected per `security-model.md`).
    pub uri: LinkText,
}

/// Mode bits the client mirrors via `GridMsg::ModeFlags`; the daemon
/// diffs against the last-shipped value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "a flat mirror of independent terminal mode bits; a bitflags newtype would obscure the per-mode field names the daemon reads"
)]
pub struct ModeSnapshot {
    /// `?2004` bracketed paste mirror.
    pub bracketed_paste: bool,
    /// `?1049` alt-screen mirror.
    pub alt_screen: bool,
    /// Mouse-event reporting level mirror (`?1000`/`?1002`/`?1003`).
    pub mouse_protocol: MouseProtocol,
    /// `?1` DECCKM; the key encoder emits SS3 arrows while set.
    pub application_cursor: bool,
    /// xterm `modifyOtherKeys` level; the key encoder routes
    /// modified keys through `CSI keycode ; mod u` (REQ-506).
    pub modify_other_keys: ModifyOtherKeys,
    /// DECKPAM / DECKPNM; the key encoder emits SS3 keypad
    /// sequences while set.
    pub application_keypad: bool,
    /// `?9001` win32-input-mode; the key encoder emits
    /// `CSI Vk;Sc;Uc;Kd;Cs;Rc _` records for `ConPTY`.
    pub win32_input_mode: bool,
    /// `?5` DECSCNM; the renderer swaps every cell's effective fg / bg
    /// (xor'd with SGR 7) plus the default-background fill.
    pub reverse_video: bool,
}

/// One diff cycle's worth of indexed-palette movement, as
/// [`Grid::take_palette_dirty`] hands it to the daemon.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PaletteDeltas {
    /// A whole-table reset fired. Ship it before [`Self::entries`]: a
    /// reset that arrived first must not undo a later set.
    pub reset_all: bool,
    /// Changed indices, ascending.
    pub entries: Vec<PaletteEntry>,
}

/// One index's new state inside a [`PaletteDeltas`] batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    pub index: u8,
    /// `Some(rgb)` for an `OSC 4` set, `None` for a per-index reset:
    /// matching the encoding [`Grid::take_theme_dirty`] uses for its channel.
    pub rgb: Option<(u8, u8, u8)>,
}

/// Mouse-event encoding selected via `?1006` / `?1016`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    /// `\e[M Cb Cx Cy`, each byte biased by `0x20`; fragile beyond
    /// column 223.
    #[default]
    Default,
    /// `?1006`: SGR (`\e[<Cb;Cx;Cy M` for press, `m` for release).
    Sgr,
    /// `?1016`: SGR with pixel coordinates instead of cells.
    SgrPixels,
}

impl Grid {
    /// Fresh grid of the given dimensions, both axes clamped to 1,
    /// retaining [`DEFAULT_SCROLLBACK_ROWS`] of history.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::with_scrollback(rows, cols, DEFAULT_SCROLLBACK_ROWS)
    }

    /// `0` retains nothing: rows leaving the viewport top are dropped.
    ///
    /// Client shadow mirrors use 0 because they read history from the daemon,
    /// avoiding redundant address space reservations per window.
    #[must_use]
    pub fn with_scrollback(rows: u16, cols: u16, scrollback_rows: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            screen: ScreenBuffer::with_scrollback(rows, cols, scrollback_rows),
            c1_8bit: false,
            conformance_level: 4,
            pen: Attributes::default(),
            pen_style: StyleId::DEFAULT,
            bracketed_paste: false,
            application_cursor: false,
            modify_other_keys: ModifyOtherKeys::Off,
            focus_reporting: false,
            sync_output: SyncOutput::Off,
            margins: Margins::new(rows, cols),
            origin_mode: false,
            autowrap: true,
            insert_mode: false,
            linefeed_newline_mode: false,
            tab_stops: default_tab_stops(cols),
            reverse_video: false,
            application_keypad: false,
            win32_input_mode: false,
            last_printed: None,
            zwj_pending: false,
            pending_bidi: editing::PendingBidi::default(),
            graphics_tracker: felis_vt::kitty_graphics::ReassemblyTracker::new(),
            apc_yield: false,
            dcs: None,
            title: None,
            icon_name: None,
            title_stack: Vec::new(),
            title_dirty: false,
            title_epoch: 0,
            cwd: None,
            cwd_dirty: false,
            cwd_epoch: 0,
            pointer_shape: None,
            pointer_shape_stack: Vec::new(),
            pointer_shape_dirty: false,
            color_scheme_notify: false,
            in_band_resize_notify: false,
            resize_notify_epoch: 0,
            os_dark: None,
            prompt_marks: Vec::new(),
            prompt_marks_pruned: 0,
            shell_prompt: ShellPrompt::new(),
            scrollback_total_pushed: 0,
            xterm_save_slots: HashMap::new(),
            dec_sace: 0,
            left_right_margin_mode: false,
            more_fix: false,
            reverse_wrap_inline: false,
            reverse_wrap_extend: false,
            allow_80_to_132: false,
            dec_ncsm: false,
            theme_overrides: [None; 3],
            theme_dirty: [false; 3],
            theme_config_defaults: [None; 3],
            palette_overrides: BTreeMap::new(),
            palette_dirty: BTreeSet::new(),
            palette_reset_all_pending: false,
            special_color_overrides: [None; SPECIAL_COLOR_COUNT],
            dec_mode_states: HashMap::default(),
            mouse_protocol: MouseProtocol::Off,
            mouse_encoding: MouseEncoding::Default,
            saved_cursor: SavedCursor::default(),
            inactive_saved_cursor: SavedCursor::default(),
            current_link: None,
            current_sizing_handle: None,
            kitty_kbd: KittyKbd::default(),
            pty_effects: PtyEffectQueue::default(),
            pending_clipboard_set: None,
            bell_pending: false,
            notifications: Vec::new(),
            osc99_pending: HashMap::new(),
            clipboard_cache_clipboard: None,
            clipboard_cache_primary: None,
            utf8: felis_vt::utf8::Decoder::new(),
            term_name: None,
        }
    }

    /// Forwarded by the daemon as `GridMsg::ClipboardSet`.
    pub const fn take_pending_clipboard_set(&mut self) -> Option<ClipboardWrite> {
        self.pending_clipboard_set.take()
    }

    /// The only drain for parser side-effects; the per-kind extraction
    /// shims live behind `#[cfg(test)]` (`test_support`) so an
    /// order-losing shape cannot grow production call sites.
    #[must_use]
    pub fn take_pty_effects(&mut self) -> Vec<PtyEffect> {
        let mut effects = self.pty_effects.take();
        if self.c1_8bit {
            // Applied at drain time so a mid-burst S8C1T affects the
            // whole drain.
            for effect in &mut effects {
                if let PtyEffect::Response(bytes) = effect {
                    *bytes = transform_c1(bytes);
                }
            }
        }
        effects
    }

    pub const fn take_bell_pending(&mut self) -> bool {
        let pending = self.bell_pending;
        self.bell_pending = false;
        pending
    }

    /// Every OSC 9 / 99 / 777 notification decoded since the last call
    /// (`docs/reference/protocols/notifications.md`); incomplete OSC 99
    /// chunks stay in the reassembly map.
    pub fn take_notifications(&mut self) -> Vec<felis_vt::notification::Notification> {
        std::mem::take(&mut self.notifications)
    }

    fn enqueue_response(&mut self, bytes: Vec<u8>) {
        self.pty_effects.push(PtyEffect::Response(bytes));
    }

    /// Coalesces with an immediately-preceding scroll; a non-scroll
    /// effect in between ends the run, which is exactly the ordering
    /// the unified queue preserves.
    pub(crate) fn note_scrolled_into_scrollback(&mut self, n: u32) {
        self.pty_effects.push_scroll(n);
    }

    #[must_use]
    pub fn kitty_kbd_flags(&self) -> KittyKbdFlags {
        self.kitty_kbd.stack.last().copied().unwrap_or_default()
    }

    #[must_use]
    pub const fn kitty_kbd_stack_depth(&self) -> usize {
        self.kitty_kbd.stack.len()
    }

    pub fn take_kitty_kbd_dirty(&mut self) -> Option<KittyKbdFlags> {
        if self.kitty_kbd.dirty {
            self.kitty_kbd.dirty = false;
            self.kitty_kbd.last_emitted = self.kitty_kbd_flags();
            Some(self.kitty_kbd.last_emitted)
        } else {
            None
        }
    }

    fn kitty_kbd_after_mutation(&mut self) {
        let cur = self.kitty_kbd_flags();
        if cur != self.kitty_kbd.last_emitted {
            self.kitty_kbd.dirty = true;
        }
    }

    fn kitty_kbd_push(&mut self, raw: u16) {
        let flags = kitty_kbd_known_flags(raw);
        if self.kitty_kbd.stack.len() == KITTY_KBD_STACK_LIMIT {
            self.kitty_kbd.stack.remove(0);
        }
        self.kitty_kbd.stack.push(flags);
        self.kitty_kbd_after_mutation();
    }

    fn kitty_kbd_pop(&mut self, count: u16) {
        let n = usize::from(count.max(1));
        let new_len = self.kitty_kbd.stack.len().saturating_sub(n);
        self.kitty_kbd.stack.truncate(new_len);
        self.kitty_kbd_after_mutation();
    }

    /// `CSI = flags ; mode u`: 1 = replace, 2 = OR-in, 3 = AND-NOT.
    fn kitty_kbd_apply(&mut self, raw_flags: u16, mode: u16) {
        let flags = kitty_kbd_known_flags(raw_flags);
        let cur = self.kitty_kbd_flags();
        let new = match mode {
            // Mode 0 means "use default 1" per the spec.
            0 | 1 => flags,
            2 => cur | flags,
            3 => cur & !flags,
            _ => return,
        };
        if let Some(top) = self.kitty_kbd.stack.last_mut() {
            *top = new;
        } else if mode != 3 {
            self.kitty_kbd.stack.push(new);
        }
        self.kitty_kbd_after_mutation();
    }

    #[must_use]
    pub const fn theme_override(&self, channel: ThemeChannel) -> Option<(u8, u8, u8)> {
        self.theme_overrides[channel as usize]
    }

    /// Host state, not parsed VT state: survives RIS.
    pub fn set_term_name(&mut self, name: &str) {
        self.term_name = Some(name.into());
    }

    #[must_use]
    pub fn term_name(&self) -> Option<&str> {
        self.term_name.as_deref()
    }

    /// Client state, not parsed VT state: untouched by DECSC / reset
    /// and never emits a `GridMsg::ThemeColor`.
    pub const fn set_theme_config_default(
        &mut self,
        channel: ThemeChannel,
        rgb: Option<(u8, u8, u8)>,
    ) {
        self.theme_config_defaults[channel as usize] = rgb;
    }

    #[must_use]
    pub const fn theme_config_default(&self, channel: ThemeChannel) -> Option<(u8, u8, u8)> {
        self.theme_config_defaults[channel as usize]
    }

    /// `Option<Option<…>>`: the outer level distinguishes "nothing
    /// changed" from "changed to a value" / "changed to reset", the
    /// latter two being `GridMsg::ThemeColor`'s `Set` and `Reset`
    /// actions.
    #[allow(clippy::option_option)]
    pub const fn take_theme_dirty(
        &mut self,
        channel: ThemeChannel,
    ) -> Option<Option<(u8, u8, u8)>> {
        let idx = channel as usize;
        if self.theme_dirty[idx] {
            self.theme_dirty[idx] = false;
            Some(self.theme_overrides[idx])
        } else {
            None
        }
    }

    /// Ascending by index; the daemon replays these on rehydrate.
    pub fn palette_overrides(&self) -> impl Iterator<Item = (u8, (u8, u8, u8))> + '_ {
        self.palette_overrides.iter().map(|(i, rgb)| (*i, *rgb))
    }

    pub fn take_palette_dirty(&mut self) -> PaletteDeltas {
        let entries = std::mem::take(&mut self.palette_dirty)
            .into_iter()
            .map(|index| PaletteEntry {
                index,
                rgb: self.palette_overrides.get(&index).copied(),
            })
            .collect();
        PaletteDeltas {
            reset_all: std::mem::take(&mut self.palette_reset_all_pending),
            entries,
        }
    }

    /// Oldest first. The daemon's per-connection cursor is an absolute
    /// ordinal; subtract [`Self::prompt_marks_pruned`] to index this
    /// front-pruned slice.
    #[must_use]
    pub fn prompt_marks(&self) -> &[PromptMark] {
        &self.prompt_marks
    }

    #[must_use]
    pub const fn prompt_marks_pruned(&self) -> u64 {
        self.prompt_marks_pruned
    }

    /// `None` when no `D` mark is retained or the youngest one carried
    /// no code: an older mark's code describes a different command.
    #[must_use]
    pub fn last_command_exit(&self) -> Option<u32> {
        self.prompt_marks
            .iter()
            .rev()
            .find(|m| m.kind == PromptKind::CommandEnd)
            .and_then(|m| m.exit_code)
    }

    /// Marks are appended in absolute-line order, so the evicted set is
    /// a prefix; an out-of-order mark left behind is harmless because
    /// `locate_line` / `prompt_jump_target` filter on the retained
    /// window.
    fn prune_evicted_marks(&mut self) {
        let oldest_retained = self
            .scrollback_total_pushed
            .saturating_sub(u64::try_from(self.screen.scrollback().len()).unwrap_or(u64::MAX));
        let keep_from = self
            .prompt_marks
            .iter()
            .position(|m| m.line >= oldest_retained)
            .unwrap_or(self.prompt_marks.len());
        if keep_from > 0 {
            self.prompt_marks.drain(..keep_from);
            self.prompt_marks_pruned = self
                .prompt_marks_pruned
                .saturating_add(u64::try_from(keep_from).unwrap_or(u64::MAX));
        }
    }

    #[must_use]
    pub const fn scrollback_total_pushed(&self) -> u64 {
        self.scrollback_total_pushed
    }

    /// Resolve a [`PromptMark::line`] to its current location
    /// (`docs/explanation/data-model/scrollback.md` "Prompt marks (OSC 133)").
    /// The live screen occupies absolute lines
    /// `[scrollback_total_pushed, scrollback_total_pushed + rows)`; the
    /// retained scrollback occupies the `scrollback.len()` lines below.
    #[must_use]
    pub fn locate_line(&self, line: u64) -> MarkLocation {
        let screen_base = self.scrollback_total_pushed;
        if line >= screen_base {
            return MarkLocation::Screen(u16::try_from(line - screen_base).unwrap_or(u16::MAX));
        }
        let sb_len = u64::try_from(self.screen.scrollback().len()).unwrap_or(u64::MAX);
        if line >= screen_base - sb_len {
            MarkLocation::Scrollback(usize::try_from(line - (screen_base - sb_len)).unwrap_or(0))
        } else {
            MarkLocation::Evicted
        }
    }

    /// `current_viewport` is the connection's present offset (0 = live
    /// bottom); the chosen prompt-start mark aligns to the top composed
    /// row. `None` on the alternate screen (kitty's `is_main_linebuf`
    /// guard), when no retained prompt lies in that direction, or when
    /// the jump would not move the viewport (hard clamp, no wrap).
    #[must_use]
    pub fn prompt_jump_target(&self, current_viewport: u32, direction: PromptJump) -> Option<u32> {
        if self.screen.on_alternate_screen() {
            return None;
        }
        let total = self.scrollback_total_pushed;
        let sb_len = u64::try_from(self.screen.scrollback().len()).unwrap_or(u64::MAX);
        // Top composed row = total_pushed - viewport
        // (docs/explanation/data-model/scrollback.md); `oldest` is the
        // youngest line a jump can land on without showing a
        // non-prompt row.
        let top = total.saturating_sub(u64::from(current_viewport));
        let oldest = total.saturating_sub(sb_len);
        let is_prompt = |m: &&PromptMark| {
            m.kind == PromptKind::PromptStart
                && m.line >= oldest
                && m.line < total + u64::from(self.screen.rows)
        };
        let target_line = match direction {
            PromptJump::Previous => self
                .prompt_marks
                .iter()
                .filter(|m| is_prompt(m) && m.line < top)
                .map(|m| m.line)
                .max()?,
            PromptJump::Next => self
                .prompt_marks
                .iter()
                .filter(|m| is_prompt(m) && m.line > top)
                .map(|m| m.line)
                .min()?,
        };
        let target = u32::try_from(total.saturating_sub(target_line)).unwrap_or(u32::MAX);
        let clamped = self.screen.clamp_viewport(target);
        (clamped != current_viewport).then_some(clamped)
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
    pub const fn title_epoch(&self) -> u64 {
        self.title_epoch
    }

    pub(crate) const fn mark_title_changed(&mut self) {
        self.title_dirty = true;
        self.title_epoch = self.title_epoch.wrapping_add(1);
    }

    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn take_cwd_dirty(&mut self) -> Option<String> {
        if self.cwd_dirty {
            self.cwd_dirty = false;
            self.cwd.clone()
        } else {
            None
        }
    }

    #[must_use]
    pub const fn cwd_epoch(&self) -> u64 {
        self.cwd_epoch
    }

    pub(crate) const fn mark_cwd_changed(&mut self) {
        self.cwd_dirty = true;
        self.cwd_epoch = self.cwd_epoch.wrapping_add(1);
    }

    #[must_use]
    pub fn pointer_shape(&self) -> Option<&str> {
        self.pointer_shape.as_deref()
    }

    pub fn set_pointer_shape(&mut self, shape: Option<String>) {
        if self.pointer_shape != shape {
            self.pointer_shape = shape;
            self.pointer_shape_dirty = true;
        }
    }

    /// At the cap the oldest saved shape is dropped.
    const POINTER_SHAPE_STACK_CAP: usize = 16;

    /// `OSC 22 ; > name` (kitty pointer-shape stack push).
    pub fn push_pointer_shape(&mut self, shape: Option<String>) {
        if self.pointer_shape_stack.len() >= Self::POINTER_SHAPE_STACK_CAP {
            self.pointer_shape_stack.remove(0);
        }
        self.pointer_shape_stack.push(self.pointer_shape.clone());
        self.set_pointer_shape(shape);
    }

    /// `OSC 22 ; <`. Popping an empty stack resets to the default arrow
    /// (kitty's behavior).
    pub fn pop_pointer_shape(&mut self) {
        let restored = self.pointer_shape_stack.pop().unwrap_or(None);
        self.set_pointer_shape(restored);
    }

    #[allow(
        clippy::option_option,
        reason = "outer = dirty, inner = the new value incl. an explicit reset (None) the daemon must ship"
    )]
    pub fn take_pointer_shape_dirty(&mut self) -> Option<Option<String>> {
        if self.pointer_shape_dirty {
            self.pointer_shape_dirty = false;
            Some(self.pointer_shape.clone())
        } else {
            None
        }
    }

    #[must_use]
    pub const fn color_scheme_notify(&self) -> bool {
        self.color_scheme_notify
    }

    /// The preference the daemon's candidate chain resolved to: one
    /// attached window's report, not necessarily the last reporter.
    pub const fn set_os_dark(&mut self, dark: bool) {
        self.os_dark = Some(dark);
    }

    /// `None` when no client has reported one. The daemon uses it to
    /// tell an effective change (which owes the program a `DECSET 2031`
    /// notification) from a re-resolution landing on the same answer.
    #[must_use]
    pub const fn os_dark(&self) -> Option<bool> {
        self.os_dark
    }

    /// Called by the daemon whenever its chain resolves to no reporting
    /// window, the last detach included: a parked session must not keep
    /// answering `DSR ? 996 n` with a departed window's OS setting.
    pub const fn clear_os_dark(&mut self) {
        self.os_dark = None;
    }

    /// `CSI ? 997 ; Ps n` (`Ps = 1` dark, `2` light), shared by the
    /// `DECSET 2031` notification and the `DSR ? 996 n` reply. Unknown
    /// answers light, the default most programs assume.
    #[must_use]
    pub fn color_scheme_report_bytes(&self) -> Vec<u8> {
        let ps = if self.os_dark == Some(true) {
            b'1'
        } else {
            b'2'
        };
        vec![0x1b, b'[', b'?', b'9', b'9', b'7', b';', ps, b'n']
    }

    #[must_use]
    pub const fn in_band_resize_notify(&self) -> bool {
        self.in_band_resize_notify
    }

    #[must_use]
    pub const fn resize_notify_epoch(&self) -> u32 {
        self.resize_notify_epoch
    }

    /// `CSI 48 ; rows ; cols ; height_px ; width_px t`, the report the
    /// daemon writes for the `DECSET 2048` set itself and for every later
    /// geometry change. The pixel axes carry the `0` stub `CSI 14 t`
    /// answers (docs/reference/protocols/vt-compliance.md).
    #[must_use]
    pub fn resize_notify_report_bytes(&self) -> Vec<u8> {
        format!("\x1b[48;{};{};0;0t", self.screen.rows, self.screen.cols).into_bytes()
    }

    #[must_use]
    pub const fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    #[must_use]
    pub const fn focus_reporting(&self) -> bool {
        self.focus_reporting
    }

    #[must_use]
    pub const fn synchronized_output(&self) -> bool {
        matches!(self.sync_output, SyncOutput::On { .. })
    }

    /// Fallback timeout for synchronized output (REQ-1004); the Kitty /
    /// contour de-facto 150 ms.
    pub const SYNC_OUTPUT_TIMEOUT: Duration = Duration::from_millis(150);

    /// Stamps the deadline lazily on the first call after BSU so the
    /// parser path reads no clock; later calls return the anchored
    /// value, and a duplicate `?2026 h` never slides it forward
    /// (otherwise a producer could defeat the fallback by spamming BSU).
    pub fn synchronized_output_deadline(&mut self, now: Instant) -> Option<Instant> {
        let SyncOutput::On { deadline } = &mut self.sync_output else {
            return None;
        };
        Some(*deadline.get_or_insert(now + Self::SYNC_OUTPUT_TIMEOUT))
    }

    /// Polled by the daemon's render-emission loop. Past the deadline
    /// the fallback path clears the mode flag so a fresh `?2026 h`
    /// re-anchors a fresh window; `false` tells the daemon to re-poll
    /// at or after `synchronized_output_deadline(now)`.
    pub fn ready_to_present(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.synchronized_output_deadline(now) else {
            return true;
        };
        if now >= deadline {
            self.set_synchronized_output(false);
            true
        } else {
            false
        }
    }

    /// The duplicate-BSU arm keeps the anchored deadline: re-anchoring
    /// would let a producer defeat the 150 ms fallback by spamming
    /// `?2026h`.
    const fn set_synchronized_output(&mut self, on: bool) {
        self.sync_output = match (self.sync_output, on) {
            (SyncOutput::On { deadline }, true) => SyncOutput::On { deadline },
            (SyncOutput::Off, true) => SyncOutput::On { deadline: None },
            (_, false) => SyncOutput::Off,
        };
    }

    #[must_use]
    pub const fn reverse_video(&self) -> bool {
        self.reverse_video
    }

    #[must_use]
    pub const fn application_keypad(&self) -> bool {
        self.application_keypad
    }

    #[must_use]
    pub const fn autowrap(&self) -> bool {
        self.autowrap
    }

    #[must_use]
    pub const fn origin_mode(&self) -> bool {
        self.origin_mode
    }

    #[must_use]
    pub const fn insert_mode(&self) -> bool {
        self.insert_mode
    }

    #[cfg(test)]
    pub(crate) const fn left_margin_for_test(&self) -> u16 {
        self.margins.left
    }

    #[cfg(test)]
    pub(crate) const fn right_margin_for_test(&self) -> u16 {
        self.margins.right
    }

    #[must_use]
    pub const fn c1_8bit(&self) -> bool {
        self.c1_8bit
    }

    #[must_use]
    pub const fn mouse_protocol(&self) -> MouseProtocol {
        self.mouse_protocol
    }

    #[must_use]
    pub const fn mode_snapshot(&self) -> ModeSnapshot {
        ModeSnapshot {
            bracketed_paste: self.bracketed_paste,
            alt_screen: self.screen.saved_primary.is_some(),
            mouse_protocol: self.mouse_protocol,
            application_cursor: self.application_cursor,
            modify_other_keys: self.modify_other_keys,
            application_keypad: self.application_keypad,
            win32_input_mode: self.win32_input_mode,
            reverse_video: self.reverse_video,
        }
    }

    #[must_use]
    pub const fn mouse_encoding(&self) -> MouseEncoding {
        self.mouse_encoding
    }

    fn set_mouse_protocol(&mut self, on: bool, level: MouseProtocol) {
        if on {
            self.mouse_protocol = level;
        } else if self.mouse_protocol == level {
            self.mouse_protocol = MouseProtocol::Off;
        }
    }

    fn set_mouse_encoding(&mut self, on: bool, encoding: MouseEncoding) {
        if on {
            self.mouse_encoding = encoding;
        } else if self.mouse_encoding == encoding {
            self.mouse_encoding = MouseEncoding::Default;
        }
    }

    /// `home_cursor` is `?1049h` (cursor to (0,0)) vs `?47h` / `?1047h`.
    /// Idempotent, and skips the screen-switch event when already on
    /// alt so the daemon-side placement save/restore stays
    /// edge-triggered.
    fn enter_alternate(&mut self, home_cursor: bool) {
        if self.screen.saved_primary.is_some() {
            return;
        }
        // `SavedScreen` records `base` but no band, and its restore /
        // resize helpers reason in raw ring order, so a snapshot must
        // carry no band rotation.
        self.screen.materialize_band();
        // Only `?47` re-entry restores prior alt cells; `?1047` / `?1049`
        // never populate `saved_alternate` on leave.
        let restored_alt = self.screen.saved_alternate.take();
        let blank_cells = || -> Vec<Cell> {
            vec![Cell::default(); usize::from(self.screen.rows) * usize::from(self.screen.cols)]
        };
        let blank_wrap =
            || -> Box<[bool]> { vec![false; usize::from(self.screen.rows)].into_boxed_slice() };
        let blank_occ =
            || -> Box<[u16]> { vec![0u16; usize::from(self.screen.rows)].into_boxed_slice() };
        // The alt screen carries no scrollback (xterm): `phys_cap == rows`,
        // `history_len == 0`, `cap == 0`.
        let (
            new_cells,
            new_base,
            new_wrap,
            new_occ,
            new_phys_cap,
            new_history_len,
            new_cap,
            new_cursor_pen,
        ) = if let Some(alt) = restored_alt {
            (
                alt.cells,
                alt.base,
                alt.soft_wrap,
                alt.occupancy,
                alt.phys_cap,
                alt.history_len,
                alt.cap,
                Some((alt.cursor, alt.pen)),
            )
        } else {
            (
                blank_cells(),
                0,
                blank_wrap(),
                blank_occ(),
                usize::from(self.screen.rows),
                0,
                0,
                None,
            )
        };
        let saved_cells = std::mem::replace(&mut self.screen.cells, new_cells);
        // Per-cell OSC 66 sizing rides inside the swapped `cells`
        // (docs/explanation/data-model/grid-and-cells.md), so no
        // side-table needs swapping.
        let saved_base = std::mem::replace(&mut self.screen.base, new_base);
        let saved_wrap = std::mem::replace(&mut self.screen.soft_wrap, new_wrap);
        let saved_occ = std::mem::replace(&mut self.screen.occupancy, new_occ);
        let saved_phys_cap = std::mem::replace(&mut self.screen.phys_cap, new_phys_cap);
        let saved_history_len = std::mem::replace(&mut self.screen.history_len, new_history_len);
        let saved_cap = std::mem::replace(&mut self.screen.cap, new_cap);
        self.screen.saved_primary = Some(SavedScreen {
            cells: saved_cells,
            rows: self.screen.rows,
            cols: self.screen.cols,
            base: saved_base,
            soft_wrap: saved_wrap,
            occupancy: saved_occ,
            phys_cap: saved_phys_cap,
            history_len: saved_history_len,
            cap: saved_cap,
            cursor: self.screen.cursor,
            pen: self.pen,
        });
        self.screen.refresh_has_sized_cells();
        std::mem::swap(&mut self.saved_cursor, &mut self.inactive_saved_cursor);
        if let Some((alt_cursor, alt_pen)) = new_cursor_pen {
            self.screen.cursor = alt_cursor;
            self.pen = alt_pen;
        } else {
            self.pen = Attributes::default();
        }
        self.resync_pen_style();
        if home_cursor {
            self.screen.cursor = Cursor::new();
        }
        self.screen.damage.mark_all();
        self.pty_effects
            .push(PtyEffect::ScreenSwitch(ScreenSwitch::EnteredAlternate));
    }

    /// `restore_cursor` is `?1049l` vs `?47l` / `?1047l`. `preserve_alt`
    /// snapshots the alt buffer so a later `?47h` re-enters with the
    /// same scribble (esctest's `test_DECSET_ALTBUF`); `?1047` / `?1049`
    /// pass `false`. No-op (no event) when already on the primary.
    fn leave_alternate(&mut self, restore_cursor: bool, preserve_alt: bool) {
        let Some(saved) = self.screen.saved_primary.take() else {
            return;
        };
        // Same contract as `enter_alternate`: no band rotation in a
        // snapshot.
        self.screen.materialize_band();
        // The primary comes back at its snapshot geometry and is
        // re-wrapped onto the alt screen's geometry below.
        let target_rows = self.screen.rows;
        let target_cols = self.screen.cols;
        if preserve_alt {
            self.screen.saved_alternate = Some(SavedScreen {
                cells: std::mem::replace(&mut self.screen.cells, saved.cells),
                rows: std::mem::replace(&mut self.screen.rows, saved.rows),
                cols: std::mem::replace(&mut self.screen.cols, saved.cols),
                base: std::mem::replace(&mut self.screen.base, saved.base),
                soft_wrap: std::mem::replace(&mut self.screen.soft_wrap, saved.soft_wrap),
                occupancy: std::mem::replace(&mut self.screen.occupancy, saved.occupancy),
                phys_cap: std::mem::replace(&mut self.screen.phys_cap, saved.phys_cap),
                history_len: std::mem::replace(&mut self.screen.history_len, saved.history_len),
                cap: std::mem::replace(&mut self.screen.cap, saved.cap),
                cursor: self.screen.cursor,
                pen: self.pen,
            });
        } else {
            self.screen.saved_alternate = None;
            self.screen.cells = saved.cells;
            // Dimensions included: a resize under the alt screen moves
            // the live dimensions past the snapshot's.
            self.screen.rows = saved.rows;
            self.screen.cols = saved.cols;
            self.screen.base = saved.base;
            self.screen.soft_wrap = saved.soft_wrap;
            self.screen.occupancy = saved.occupancy;
            self.screen.phys_cap = saved.phys_cap;
            self.screen.history_len = saved.history_len;
            self.screen.cap = saved.cap;
        }
        self.screen.refresh_has_sized_cells();
        if restore_cursor {
            self.screen.cursor = saved.cursor;
        }
        self.pen = saved.pen;
        self.resync_pen_style();
        std::mem::swap(&mut self.saved_cursor, &mut self.inactive_saved_cursor);
        let remap = self.reflow_restored_primary(target_rows, target_cols);
        self.screen.damage.mark_all();
        self.pty_effects
            .push(PtyEffect::ScreenSwitch(ScreenSwitch::LeftAlternate));
        // After the switch effect: the remap must see the restored
        // primary placements.
        if let Some(remap) = remap {
            self.pty_effects.push(PtyEffect::PrimaryReflowed(remap));
        }
    }

    /// Reflows the restored primary screen to current dimensions.
    ///
    /// Primary re-wrap waits until exiting the alt screen, collapsing resize
    /// bursts into a single pass (`docs/explanation/data-model/scrollback.md`).
    fn reflow_restored_primary(
        &mut self,
        target_rows: u16,
        target_cols: u16,
    ) -> Option<ReflowRemap> {
        if target_rows == self.screen.rows && target_cols == self.screen.cols {
            return None;
        }
        // The damage tracker follows the live row count; `reflow` sizes
        // it back.
        self.screen.damage.resize(usize::from(self.screen.rows));
        // `?47l` / `?1047l` keep the alt screen's cursor, which can sit
        // past the restored geometry; clamp before the re-wrap reads
        // its row as a live row.
        self.screen.cursor.row = self.screen.cursor.row.min(self.screen.rows - 1);
        self.screen.cursor.col = self.screen.cursor.col.min(self.screen.cols - 1);
        self.reflow(target_rows, target_cols)
    }

    /// Moves the cursor past a Kitty image placement as kitty's
    /// `screen_handle_graphics_command` does; the rule is the kitty-graphics
    /// reference, "Placement parameters". Zero on an axis moves nothing.
    pub fn advance_cursor_after_image_placement(
        &mut self,
        rows: u16,
        cols: u16,
        no_cursor_move: bool,
    ) {
        if no_cursor_move || (rows == 0 && cols == 0) {
            return;
        }
        let mut row = u32::from(self.screen.cursor.row) + u32::from(rows.saturating_sub(1));
        let mut col = u32::from(self.screen.cursor.col) + u32::from(cols);
        let top = u32::from(self.margins.top);
        let bottom = u32::from(self.margins.bottom);
        let clamp_to_region = self.origin_mode && (top..=bottom).contains(&row);
        if col >= u32::from(self.screen.cols) {
            col = 0;
            row += 1;
        }
        if row > bottom {
            self.scroll_region_up(u16::try_from(row - bottom).unwrap_or(u16::MAX));
        }
        let (min_row, max_row) = if clamp_to_region {
            (top, bottom)
        } else {
            (0, u32::from(self.screen.rows.saturating_sub(1)))
        };
        let last_col = self.screen.cols.saturating_sub(1);
        self.screen.cursor.row = u16::try_from(row.clamp(min_row, max_row)).unwrap_or(u16::MAX);
        self.screen.cursor.col = u16::try_from(col).unwrap_or(u16::MAX).min(last_col);
        self.screen.cursor.pending_wrap = false;
    }

    #[must_use]
    pub const fn pen(&self) -> Attributes {
        self.pen
    }

    #[must_use]
    pub const fn style_table_len(&self) -> usize {
        self.screen.style_table_len()
    }

    /// The one choke point every SGR / pen-restore site funnels
    /// through, so no write path re-interns per cell.
    fn resync_pen_style(&mut self) {
        self.pen_style = self.screen.style_table.intern(self.pen);
    }

    #[must_use]
    pub const fn damage(&self) -> &Damage {
        &self.screen.damage
    }

    pub const fn damage_mut(&mut self) -> &mut Damage {
        &mut self.screen.damage
    }

    /// Trims and pads: a resize neither pushes to scrollback nor evicts
    /// history. Saved screen snapshots are trimmed in lockstep so a
    /// later switch restores matching dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.screen.rows && cols == self.screen.cols {
            return;
        }
        self.pending_bidi.discard();
        // Read before the screen moves: an implicit full-screen region
        // must re-track the new edges, and the test is against the old
        // geometry. Otherwise a 24→82 grow leaves the bottom margin at
        // 23 and every /bin/sh line-feed scrolls only the top 24 rows.
        let was_full_vertical =
            self.margins.top == 0 && self.margins.bottom + 1 == self.screen.rows;
        let was_full_horizontal =
            self.margins.left == 0 && self.margins.right + 1 == self.screen.cols;
        self.screen.resize(rows, cols);
        // Prompt marks are not pruned here: they carry absolute lines
        // and a resize neither pushes to scrollback nor evicts history
        // (docs/explanation/data-model/scrollback.md).
        self.finish_geometry_change(rows, cols, was_full_vertical, was_full_horizontal);
    }

    /// The parser-owned half of a geometry change; the screen's half is
    /// [`ScreenBuffer::retrack_geometry`].
    fn finish_geometry_change(
        &mut self,
        rows: u16,
        cols: u16,
        was_full_vertical: bool,
        was_full_horizontal: bool,
    ) {
        // An implicit full-screen region re-tracks the edges so /bin/sh
        // (never emits DECSTBM) keeps full-screen scrolling after a
        // SIGWINCH; a user-pinned band that still fits survives.
        if was_full_vertical
            || self.margins.bottom >= rows
            || self.margins.top >= rows - 1
            || self.margins.top >= self.margins.bottom
        {
            self.margins.top = 0;
            self.margins.bottom = rows - 1;
        }
        // Same rule for the DECLRMM margins.
        if was_full_horizontal
            || self.margins.right >= cols
            || self.margins.left >= cols.saturating_sub(1)
            || self.margins.left >= self.margins.right
        {
            self.margins.left = 0;
            self.margins.right = cols.saturating_sub(1);
        }
        // Newly-revealed columns get the every-8 default so a widen
        // leaves no hole on the right for HT walks.
        let mut new_stops = default_tab_stops(cols);
        for (col, slot) in new_stops.iter_mut().enumerate() {
            if let Some(prev) = self.tab_stops.get(col) {
                *slot = *prev;
            }
        }
        self.tab_stops = new_stops;
    }

    /// Re-wrap the primary screen at `rows × cols` (REQ-604,
    /// `docs/explanation/data-model/grid-and-cells.md` "Reflow on
    /// resize").
    /// Preserves the cursor's logical position; the alternate screen uses
    /// [`Self::resize`] instead.
    pub fn reflow(&mut self, rows: u16, cols: u16) -> Option<ReflowRemap> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.screen.rows && cols == self.screen.cols {
            return None;
        }
        if self.screen.on_alternate_screen() {
            self.resize(rows, cols);
            return None;
        }
        self.pending_bidi.discard();

        // Read before any field changes; see `resize`.
        let was_full_vertical =
            self.margins.top == 0 && self.margins.bottom + 1 == self.screen.rows;
        let was_full_horizontal =
            self.margins.left == 0 && self.margins.right + 1 == self.screen.cols;

        // Every logical line's new absolute line is measured from the
        // oldest retained combined row, so a surviving prompt mark
        // keeps a valid location (D1).
        let sb_len = self.screen.scrollback().len();
        let base_abs = self
            .scrollback_total_pushed
            .saturating_sub(u64::try_from(sb_len).unwrap_or(u64::MAX));

        let old_rows = self.screen.rows;
        let old_cols = self.screen.cols;
        let old_cursor_col = self.screen.cursor.col;
        // Re-wrapping a prompt the shell is about to repaint leaves a
        // stale copy: zsh climbs back by the row count it drew, which a
        // re-wrap changes. Kitty and Ghostty blank it the same way.
        let blank = self.prompt_redraw_rows();
        // Trailing blank live rows are screen padding, not history:
        // reflowing them would anchor blank rows to the bottom and
        // shove real content (and the prompt) into scrollback. Keep
        // every row up to the last non-blank one and the cursor's row.
        let mut live_extent = usize::from(self.screen.cursor.row) + 1;
        for r in (0..old_rows).rev() {
            if blank.as_ref().is_some_and(|b| b.contains(&r)) {
                continue;
            }
            let row = self.screen.row_content(r).unwrap_or(&[]);
            if row.iter().any(|c| *c != Cell::default()) {
                live_extent = live_extent.max(usize::from(r) + 1);
                break;
            }
        }
        let mut combined: Vec<Vec<Cell>> = Vec::with_capacity(sb_len + live_extent);
        let mut continued: Vec<bool> = Vec::with_capacity(sb_len + live_extent);
        let old_cols_usize = usize::from(old_cols);
        for (i, (cells, cont)) in self
            .screen
            .rows_with_wrap(AltScreenRows::Include)
            .take(sb_len + live_extent)
            .enumerate()
        {
            let screen_row = i.checked_sub(sb_len).and_then(|r| u16::try_from(r).ok());
            if screen_row.is_some_and(|r| blank.as_ref().is_some_and(|b| b.contains(&r))) {
                combined.push(vec![Cell::default(); old_cols_usize]);
                continued.push(false);
                continue;
            }
            let blank_end = blank.as_ref().map(|b| b.end);
            let cont = cont && (blank_end.is_none() || screen_row != blank_end);
            let mut row = cells.to_vec();
            if i < sb_len {
                // Pad history rows back to full width: the walk clips
                // at the occupancy watermark, but the rewrap
                // reconstructs bare-LF column carries from trailing
                // positional blanks, so a clipped row would collapse a
                // staircase to column 0.
                row.resize(old_cols_usize, Cell::default());
            }
            combined.push(row);
            continued.push(cont);
        }

        let spans = logical_line_spans(&continued);
        let mut line_of_row: Vec<usize> = vec![0; combined.len()];
        for (li, &(start, end)) in spans.iter().enumerate() {
            for slot in &mut line_of_row[start..=end] {
                *slot = li;
            }
        }

        let cursor_combined = sb_len + usize::from(self.screen.cursor.row);
        let cursor_line = line_of_row.get(cursor_combined).copied().unwrap_or(0);
        let cursor_run_index = {
            let (start, end) = spans[cursor_line];
            let mut j = 0usize;
            for k in start..cursor_combined {
                let contribution = if k < end {
                    self.nontail_contribution(&combined[k], &combined[k + 1])
                        .len()
                } else {
                    combined[k].len()
                };
                j += contribution;
            }
            j + usize::from(self.screen.cursor.col)
        };

        // For image-placement anchors (REQ-604), track where each
        // pre-reflow row's first cell lands, so an anchor
        // mid-logical-line remaps to its own row, not the line start.
        let mut out_rows: Vec<Vec<Cell>> = Vec::with_capacity(combined.len());
        let mut out_continued: Vec<bool> = Vec::with_capacity(combined.len());
        let mut new_line_start: Vec<usize> = Vec::with_capacity(spans.len());
        let mut new_row_of_old: Vec<usize> = Vec::with_capacity(combined.len());
        let mut row_starts: Vec<usize> = Vec::new();
        let mut cursor_phys = (0usize, 0usize);
        for (li, &(start, end)) in spans.iter().enumerate() {
            new_line_start.push(out_rows.len());
            let run = self.stitch_logical_line(&combined, start, end);
            let line_first = out_rows.len();
            let target = (li == cursor_line).then_some(cursor_run_index);
            row_starts.clear();
            if let Some((lr, lc)) = self.rewrap_run(
                &run,
                cols,
                &mut out_rows,
                &mut out_continued,
                target,
                &mut row_starts,
            ) {
                cursor_phys = (line_first + lr, lc);
            }
            let mut off = 0usize;
            for k in start..=end {
                let within = row_starts.partition_point(|&s| s <= off).saturating_sub(1);
                new_row_of_old.push(line_first + within);
                off += if k < end {
                    self.nontail_contribution(&combined[k], &combined[k + 1])
                        .len()
                } else {
                    combined[k].len()
                };
            }
        }
        let total_new = out_rows.len();

        // Bottom-anchor the live window to the newest content: the last
        // `rows` physical rows are live, everything above spills into
        // scrollback. A cursor left above the window clamps to the top
        // row rather than dropping off-screen.
        let win_start = total_new.saturating_sub(usize::from(rows));

        // `install_ring` drops the oldest past `cap` (D2: a narrowing
        // that overflows the ring loses the oldest lines).
        let cols_usize = usize::from(cols);
        let rows_usize = usize::from(rows);
        let history: Vec<(Vec<Cell>, bool)> = (0..win_start)
            .map(|phys| {
                let row = &out_rows[phys];
                let occ = row
                    .iter()
                    .rposition(|c| *c != Cell::default())
                    .map_or(0, |i| i + 1);
                (row[..occ].to_vec(), out_continued[phys])
            })
            .collect();
        let mut viewport_cells = vec![Cell::default(); rows_usize * cols_usize];
        let mut viewport_soft_wrap = vec![false; rows_usize];
        for r in 0..rows_usize {
            let phys = win_start + r;
            if phys < total_new {
                viewport_cells[r * cols_usize..(r + 1) * cols_usize]
                    .copy_from_slice(&out_rows[phys]);
                viewport_soft_wrap[r] = out_continued[phys];
            }
        }
        self.screen
            .install_ring(rows, cols, &history, &viewport_cells, &viewport_soft_wrap);

        let cursor_row = cursor_phys.0.saturating_sub(win_start).min(rows_usize - 1);
        self.screen.cursor.row = u16::try_from(cursor_row).unwrap_or(rows - 1);
        self.screen.cursor.col = u16::try_from(cursor_phys.1)
            .unwrap_or(cols - 1)
            .min(cols - 1);
        if blank.is_some() {
            self.screen.cursor.col = old_cursor_col.min(cols - 1);
        }
        self.screen.cursor.pending_wrap = false;

        // Remap prompt marks onto their logical line's new first
        // physical row (D1); a mark whose line evicted falls below the
        // retained floor and `prune_evicted_marks` drops it.
        self.scrollback_total_pushed =
            base_abs.saturating_add(u64::try_from(win_start).unwrap_or(u64::MAX));
        for mark in &mut self.prompt_marks {
            let Some(j) = mark.line.checked_sub(base_abs) else {
                continue;
            };
            let Ok(j) = usize::try_from(j) else { continue };
            if let Some(&li) = line_of_row.get(j) {
                mark.line =
                    base_abs.saturating_add(u64::try_from(new_line_start[li]).unwrap_or(u64::MAX));
            }
        }
        self.prune_evicted_marks();

        self.screen.retrack_geometry(rows, cols);
        self.finish_geometry_change(rows, cols, was_full_vertical, was_full_horizontal);

        Some(ReflowRemap {
            old_sb_len: sb_len,
            new_win_start: win_start,
            new_total: total_new,
            new_sb_len: self.screen.scrollback().len(),
            new_row_of_old,
            cleared: blank.map(|b| sb_len + usize::from(b.start)..sb_len + usize::from(b.end)),
        })
    }

    /// The screen rows a resize blanks: from the first row of the prompt
    /// the cursor sits in to the bottom (only the cursor's row under
    /// `redraw=last`), or `None` outside a prompt or when the shell opted
    /// out.
    fn prompt_redraw_rows(&self) -> Option<std::ops::Range<u16>> {
        let prompt = &self.shell_prompt;
        if !prompt.marks_commands || !prompt.at_prompt || prompt.redraw == PromptRedraw::Never {
            return None;
        }
        let idx = usize::try_from(prompt.start?.checked_sub(self.prompt_marks_pruned)?).ok()?;
        let cursor_row = self.screen.cursor.row;
        let start_row = match self.locate_line(self.prompt_marks.get(idx)?.line) {
            MarkLocation::Screen(row) if row <= cursor_row => row,
            _ => return None,
        };
        Some(match prompt.redraw {
            PromptRedraw::LastLine => cursor_row..cursor_row + 1,
            _ => start_row..self.screen.rows,
        })
    }

    /// A continuation row contributes every cell except the unfillable
    /// slot a wide-glyph wrap leaves at the right edge, dropped so that
    /// glyph re-flows onto its own boundary. That slot is the only
    /// reason a continuation row ends short; a row that is entirely
    /// `Empty` is a bare-LF column carry and rides through whole.
    fn nontail_contribution<'a>(&self, row: &'a [Cell], next_row: &[Cell]) -> &'a [Cell] {
        let next_starts_wide = next_row
            .first()
            .is_some_and(|c| self.screen.grapheme_width(c.grapheme) == 2);
        let has_content = row.iter().any(|c| !matches!(c.grapheme, Grapheme::Empty));
        let ends_blank = row
            .last()
            .is_some_and(|c| matches!(c.grapheme, Grapheme::Empty));
        if next_starts_wide && has_content && ends_blank {
            &row[..row.len() - 1]
        } else {
            row
        }
    }

    /// The run's trailing never-written padding (`Cell::default()`) is
    /// trimmed; a colored BCE blank or a wide-glyph `Spacer` is not
    /// `default`, so it rides through.
    fn stitch_logical_line(&self, combined: &[Vec<Cell>], start: usize, end: usize) -> Vec<Cell> {
        let mut run: Vec<Cell> = Vec::new();
        for k in start..=end {
            if k < end {
                run.extend_from_slice(self.nontail_contribution(&combined[k], &combined[k + 1]));
            } else {
                run.extend_from_slice(&combined[k]);
            }
        }
        while run.last().is_some_and(|c| *c == Cell::default()) {
            run.pop();
        }
        run
    }

    /// Always emits at least one row so a blank logical line survives.
    /// A width-2 glyph never straddles the wrap boundary: the row is
    /// flushed with the unfillable slot blank, matching the VT sink's
    /// autowrap. `row_starts` receives the run index each emitted row
    /// begins at (first always `0`).
    fn rewrap_run(
        &self,
        run: &[Cell],
        cols: u16,
        out_rows: &mut Vec<Vec<Cell>>,
        out_continued: &mut Vec<bool>,
        cursor_target: Option<usize>,
        row_starts: &mut Vec<usize>,
    ) -> Option<(usize, usize)> {
        let cols_usize = usize::from(cols);
        let mut cur: Vec<Cell> = Vec::with_capacity(cols_usize);
        let mut rows_pushed = 0usize;
        let mut first = true;
        let mut cursor_pos = None;
        let mut i = 0usize;
        row_starts.push(0);
        while i < run.len() {
            let cell = run[i];
            let has_spacer = i + 1 < run.len() && matches!(run[i + 1].grapheme, Grapheme::Spacer);
            // A width-2 cluster whose widen was refused sat in one cell
            // and keeps one, or the rest of its line would shift right.
            if has_spacer && self.screen.grapheme_width(cell.grapheme) == 2 {
                // A 1-column grid cannot host a wide glyph; drop it
                // (with its spacer) rather than loop forever, matching
                // `put_grapheme`.
                if cols_usize < 2 {
                    i += 2;
                    continue;
                }
                if cur.len() + 2 > cols_usize {
                    pad_and_push(&mut cur, cols_usize, out_rows, out_continued, &mut first);
                    rows_pushed += 1;
                    row_starts.push(i);
                }
                if cursor_target == Some(i) {
                    cursor_pos = Some((rows_pushed, cur.len()));
                }
                if cursor_target == Some(i + 1) {
                    cursor_pos = Some((rows_pushed, cur.len() + 1));
                }
                cur.push(cell);
                cur.push(run[i + 1]);
                i += 2;
            } else {
                if cur.len() + 1 > cols_usize {
                    pad_and_push(&mut cur, cols_usize, out_rows, out_continued, &mut first);
                    rows_pushed += 1;
                    row_starts.push(i);
                }
                if cursor_target == Some(i) {
                    cursor_pos = Some((rows_pushed, cur.len()));
                }
                cur.push(cell);
                i += 1;
            }
        }
        let last_col = cur.len();
        pad_and_push(&mut cur, cols_usize, out_rows, out_continued, &mut first);
        if cursor_target.is_some() && cursor_pos.is_none() {
            cursor_pos = Some((rows_pushed, last_col.min(cols_usize.saturating_sub(1))));
        }
        cursor_pos
    }

    /// Double the physical ring toward `rows + cap`, re-laying it out
    /// canonically: history at `[0, history_len)`, viewport after it,
    /// base at `history_len`. Growth is lazy so an idle session never
    /// allocates past its live viewport (the resident-memory
    /// requirement, `docs/explanation/data-model/scrollback.md`).
    fn grow_ring(&mut self) {
        let cols = usize::from(self.screen.cols);
        let rows = usize::from(self.screen.rows);
        let ring_rows = rows + self.screen.cap;
        // `history_len + rows + 1 <= ring_rows` because this is only
        // called while `history_len < cap`.
        let new_cap =
            (self.screen.phys_cap * 2).clamp(self.screen.history_len + rows + 1, ring_rows);
        // The one caller grows only a ring that is exactly full, which
        // is what makes the relayout a rotation: every physical row is
        // live, so canonical order is one `rotate_left` away.
        debug_assert_eq!(self.screen.history_len + rows, self.screen.phys_cap);
        self.screen.materialize_band();
        let oldest = (self.screen.base + self.screen.phys_cap - self.screen.history_len)
            % self.screen.phys_cap;
        self.screen.cells.rotate_left(oldest * cols);
        self.screen.soft_wrap.rotate_left(oldest);
        self.screen.occupancy.rotate_left(oldest);
        // Within the reservation `ring_cells` took, so no byte moves.
        self.screen.cells.resize(new_cap * cols, Cell::default());
        let mut soft_wrap = vec![false; new_cap].into_boxed_slice();
        let mut occupancy = vec![0u16; new_cap].into_boxed_slice();
        soft_wrap[..self.screen.phys_cap].copy_from_slice(&self.screen.soft_wrap);
        occupancy[..self.screen.phys_cap].copy_from_slice(&self.screen.occupancy);
        self.screen.soft_wrap = soft_wrap;
        self.screen.occupancy = occupancy;
        self.screen.base = self.screen.history_len;
        self.screen.phys_cap = new_cap;
        self.screen.band_len = 0;
        self.screen.band_rot = 0;
    }

    /// Evicts the top `n` rows into history in place: the base advances
    /// and the row it leaves behind becomes history-youngest for free.
    /// Once history is at `cap` the recycle-blank of the bottom row is
    /// the eviction of the oldest row.
    fn scroll_full_screen_into_history(&mut self, n: usize, blank_with: &Cell) {
        // The base bumps do not commute with an active scroll band (see
        // `rotate_region`), and the row leaving the viewport top must be
        // the physically-at-`base` row for the in-place eviction to hold.
        self.screen.materialize_band();
        let cols = usize::from(self.screen.cols);
        let rows = usize::from(self.screen.rows);
        let blank_is_default = *blank_with == Cell::default();
        for _ in 0..n {
            // Grow before the base moves, or the recycle-blank below
            // would clobber the row about to become history-youngest.
            if self.screen.history_len < self.screen.cap
                && self.screen.history_len + rows == self.screen.phys_cap
            {
                self.grow_ring();
            }
            self.screen.base = if self.screen.base + 1 >= self.screen.phys_cap {
                0
            } else {
                self.screen.base + 1
            };
            self.screen.history_len = (self.screen.history_len + 1).min(self.screen.cap);
            let bottom = self.screen.phys_row_at(rows - 1);
            let start = bottom * cols;
            if blank_is_default {
                self.screen.occupancy[bottom] = 0;
            } else {
                // BCE: a colored blank is live content, pinned at `cols`.
                self.screen.cells[start..start + cols].fill(*blank_with);
                self.screen.occupancy[bottom] = u16::try_from(cols).unwrap_or(u16::MAX);
            }
            self.screen.soft_wrap[bottom] = false;
        }
    }

    /// The top rows go through the O(1) full-screen eviction, which
    /// over-scrolls the rows below the band; those are copied back down
    /// and the band's vacated bottom re-blanked.
    fn scroll_partial_top_into_history(
        &mut self,
        n: usize,
        region_bottom: usize,
        blank_with: &Cell,
    ) {
        let cols = usize::from(self.screen.cols);
        let rows = usize::from(self.screen.rows);
        self.scroll_full_screen_into_history(n, blank_with);
        // Reverse order handles the overlap. Occupancy-clipped like
        // `rotate_region`: `(occupancy, prefix)` defines the row, so the
        // stale tail behind a short copy is unobservable.
        for r in ((region_bottom + 1)..rows).rev() {
            let sp = self.screen.phys_row_at(r - n);
            let dp = self.screen.phys_row_at(r);
            let occ = usize::from(self.screen.occupancy[sp]);
            self.screen
                .cells
                .copy_within(sp * cols..sp * cols + occ, dp * cols);
            self.screen.soft_wrap[dp] = self.screen.soft_wrap[sp];
            self.screen.occupancy[dp] = self.screen.occupancy[sp];
        }
        let blank_is_default = *blank_with == Cell::default();
        for r in (region_bottom + 1 - n)..=region_bottom {
            let phys = self.screen.phys_row_at(r);
            let start = phys * cols;
            if blank_is_default {
                self.screen.occupancy[phys] = 0;
            } else {
                self.screen.cells[start..start + cols].fill(*blank_with);
                self.screen.occupancy[phys] = u16::try_from(cols).unwrap_or(u16::MAX);
            }
            self.screen.soft_wrap[phys] = false;
        }
    }

    /// The `CSI 3 J` path: relays the live viewport into a bare
    /// `rows * cols` ring, preserving the visible screen exactly.
    fn drop_history(&mut self) {
        if self.screen.history_len == 0
            && self.screen.base == 0
            && self.screen.phys_cap == usize::from(self.screen.rows)
        {
            return;
        }
        let cols = usize::from(self.screen.cols);
        let rows = usize::from(self.screen.rows);
        let mut cells = ring_cells(rows * cols, rows + self.screen.cap, cols);
        let mut soft_wrap = vec![false; rows].into_boxed_slice();
        let mut occupancy = vec![0u16; rows].into_boxed_slice();
        for r in 0..rows {
            let src = self.screen.phys_row_at(r);
            cells[r * cols..r * cols + cols]
                .copy_from_slice(&self.screen.cells[src * cols..src * cols + cols]);
            soft_wrap[r] = self.screen.soft_wrap[src];
            occupancy[r] = self.screen.occupancy[src];
        }
        self.screen.cells = cells;
        self.screen.soft_wrap = soft_wrap;
        self.screen.occupancy = occupancy;
        self.screen.base = 0;
        self.screen.phys_cap = rows;
        self.screen.history_len = 0;
        self.screen.band_len = 0;
        self.screen.band_rot = 0;
    }
}

/// History occupies `[base - history_len, base)` in ring order; the
/// offset is non-negative because `history_len + rows` never exceeds
/// `phys_cap`.
const fn hist_phys_of(base: usize, phys_cap: usize, history_len: usize, idx: usize) -> usize {
    let p = base + phys_cap - history_len + idx;
    if p >= phys_cap { p - phys_cap } else { p }
}

/// Whether a [`Grid::rows_with_wrap`] walk takes in the live rows while
/// the alternate screen holds the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AltScreenRows {
    Include,
    /// Stop at the scrollback half while the alt screen is up: alt
    /// content continues nothing in the saved primary's history.
    Skip,
}

/// A read view over retained scrollback, oldest to newest
/// (docs/explanation/data-model/scrollback.md). Rows are
/// occupancy-clipped: a history row's tail past the watermark is
/// undefined, so the view hands back only the live prefix.
pub struct ScrollbackView<'a> {
    cells: &'a [Cell],
    soft_wrap: &'a [bool],
    occupancy: &'a [u16],
    base: usize,
    phys_cap: usize,
    history_len: usize,
    cols: usize,
}

impl<'a> ScrollbackView<'a> {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.history_len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.history_len == 0
    }

    /// History row `idx_from_oldest`, occupancy-clipped, or `None` past
    /// the retained ring.
    #[must_use]
    pub fn row(&self, idx_from_oldest: usize) -> Option<&'a [Cell]> {
        if idx_from_oldest >= self.history_len {
            return None;
        }
        let phys = hist_phys_of(self.base, self.phys_cap, self.history_len, idx_from_oldest);
        let start = phys * self.cols;
        let occ = usize::from(self.occupancy[phys]).min(self.cols);
        Some(&self.cells[start..start + occ])
    }

    #[must_use]
    pub const fn soft_wrap_continued(&self, idx_from_oldest: usize) -> Option<bool> {
        if idx_from_oldest >= self.history_len {
            return None;
        }
        let phys = hist_phys_of(self.base, self.phys_cap, self.history_len, idx_from_oldest);
        Some(self.soft_wrap[phys])
    }

    /// Iterate history rows oldest to newest, occupancy-clipped.
    pub fn iter(&self) -> impl Iterator<Item = &'a [Cell]> {
        let this = ScrollbackView { ..*self };
        (0..self.history_len).map(move |i| this.row(i).unwrap_or(&[]))
    }
}

/// Only the four shapes felis emits (CSI / OSC / DCS / ST) are mapped;
/// other ESC sequences pass through so a literal ESC inside a title
/// payload survives.
fn transform_c1(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() {
            let mapped = match bytes[i + 1] {
                b'[' => Some(0x9B),
                b']' => Some(0x9D),
                b'P' => Some(0x90),
                b'\\' => Some(0x9C),
                _ => None,
            };
            if let Some(b) = mapped {
                out.push(b);
                i += 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// `None` for [`Color::Default`]: DECRQSS omits the parameter entirely
/// when the pen carries the default.
fn encode_sgr_color(c: Color, base8: u8, base_extended: u8) -> Option<String> {
    match c {
        Color::Default => None,
        Color::Indexed(n) if n < 8 => Some(format!("{}", base8 + n)),
        Color::Indexed(n) if n < 16 => Some(format!("{}", base8 + 60 + (n - 8))),
        Color::Indexed(n) => Some(format!("{base_extended};5;{n}")),
        Color::Rgb(r, g, b) => Some(format!("{base_extended};2;{r};{g};{b}")),
    }
}

/// SGR 58 has no 16-color short form; xterm reports it in the colon
/// sub-param shape (`58:5:n`, `58:2::r:g:b` with an empty color-space
/// slot), which `apply_sgr` parses back so the reply round-trips.
fn encode_sgr_underline_color(c: Color) -> Option<String> {
    match c {
        Color::Default => None,
        Color::Indexed(n) => Some(format!("58:5:{n}")),
        Color::Rgb(r, g, b) => Some(format!("58:2::{r}:{g}:{b}")),
    }
}

/// DEC standard: stops every 8 columns from column 8; column 0 is
/// never a stop.
fn default_tab_stops(cols: u16) -> Vec<bool> {
    let cols = usize::from(cols);
    let mut stops = vec![false; cols];
    let mut c = 8;
    while c < cols {
        stops[c] = true;
        c += 8;
    }
    stops
}

fn compact_saved_into_logical_order(saved: &mut SavedScreen) {
    if saved.base == 0 {
        return;
    }
    let cols = usize::from(saved.cols);
    saved.cells.rotate_left(saved.base * cols);
    saved.soft_wrap.rotate_left(saved.base);
    saved.occupancy.rotate_left(saved.base);
    saved.base = 0;
}

/// Trim the inactive alternate snapshot to `dst_rows × dst_cols`.
/// Uses snapshot source geometry rather than live grid dimensions.
/// Alt-screen only: the alt buffer is a bare viewport ring (`cap == 0`),
/// so trim/pad is used; saved primary reflows instead.
fn resize_saved_screen(saved: &mut SavedScreen, dst_rows: u16, dst_cols: u16) {
    debug_assert_eq!(saved.history_len, 0, "the alt snapshot holds no history");
    debug_assert_eq!(
        saved.phys_cap,
        usize::from(saved.rows),
        "the alt snapshot's ring is a bare viewport"
    );
    let (src_rows, src_cols) = (saved.rows, saved.cols);
    // `trim_cells` reads row-identity layout.
    compact_saved_into_logical_order(saved);
    saved.cells = trim_cells(
        &saved.cells,
        src_rows,
        src_cols,
        &saved.occupancy,
        dst_rows,
        dst_cols,
    );
    saved.soft_wrap = trim_soft_wrap(&saved.soft_wrap, dst_rows);
    saved.cursor.row = saved.cursor.row.min(dst_rows - 1);
    saved.cursor.col = saved.cursor.col.min(dst_cols - 1);
    saved.cursor.pending_wrap = false;
    // `trim_cells` lays rows out flat, so the modulus is the new
    // row count; a stale `phys_cap` would let the restored buffer's
    // `base` walk past the shorter arrays.
    saved.base = 0;
    saved.rows = dst_rows;
    saved.cols = dst_cols;
    saved.phys_cap = usize::from(dst_rows);
    // Trimmed rows lost their watermark provenance; `dst_cols` is the
    // always-safe upper bound and self-heals as rows are recycled.
    saved.occupancy = vec![dst_cols; usize::from(dst_rows)].into_boxed_slice();
}

fn trim_soft_wrap(src: &[bool], dst_rows: u16) -> Box<[bool]> {
    let mut dst = vec![false; usize::from(dst_rows)].into_boxed_slice();
    let keep = src.len().min(dst.len());
    dst[..keep].copy_from_slice(&src[..keep]);
    dst
}

fn pad_and_push(
    cur: &mut Vec<Cell>,
    cols: usize,
    out_rows: &mut Vec<Vec<Cell>>,
    out_continued: &mut Vec<bool>,
    first: &mut bool,
) {
    cur.resize(cols, Cell::default());
    out_rows.push(std::mem::take(cur));
    out_continued.push(!*first);
    *first = false;
}

/// Row-anchor translation for one [`Grid::reflow`] (REQ-604). Kitty
/// direct placements anchor to a signed grid row (`1` = top live row,
/// `0` = youngest scrollback line, negative = older history,
/// [`images::CellPos`]) and live daemon-side; the placement owner
/// replays each anchor through [`Self::remap_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflowRemap {
    /// Scrollback length before the reflow: converts a signed anchor
    /// row into an index over the pre-reflow combined surface.
    old_sb_len: usize,
    /// First post-reflow physical row inside the live window.
    new_win_start: usize,
    /// Total post-reflow physical rows (history + live content).
    new_total: usize,
    /// Scrollback length after the reflow, post-eviction: the
    /// retention horizon for remapped anchors.
    new_sb_len: usize,
    /// Pre-reflow combined row index → post-reflow physical row of
    /// that row's first cell.
    new_row_of_old: Vec<usize>,
    /// Pre-reflow combined rows of a prompt blanked for the shell to
    /// repaint; anchors there go with the prompt.
    cleared: Option<std::ops::Range<usize>>,
}

impl ReflowRemap {
    /// `None` means the anchor's line left the retained scrollback; the
    /// owner should evict it as [`images::Placements::shift_up`] does.
    #[must_use]
    pub fn remap_row(&self, old_row: i32) -> Option<i32> {
        let old_sb = i64::try_from(self.old_sb_len).unwrap_or(i64::MAX);
        let j = usize::try_from(i64::from(old_row) + old_sb - 1).ok()?;
        if self.cleared.as_ref().is_some_and(|c| c.contains(&j)) {
            return None;
        }
        // Rows below the pre-reflow content extent are screen padding
        // that reflow regenerates; an anchor there keeps its distance
        // below the last content row.
        let nj = self
            .new_row_of_old
            .get(j)
            .copied()
            .unwrap_or_else(|| self.new_total.saturating_add(j - self.new_row_of_old.len()));
        let new_anchor = i64::try_from(nj).unwrap_or(i64::MAX)
            - i64::try_from(self.new_win_start).unwrap_or(i64::MAX)
            + 1;
        let horizon = 1i64 - i64::try_from(self.new_sb_len).unwrap_or(i64::MAX);
        if new_anchor < horizon {
            return None;
        }
        i32::try_from(new_anchor).ok()
    }
}

fn trim_cells(
    src: &[Cell],
    src_rows: u16,
    src_cols: u16,
    src_occ: &[u16],
    dst_rows: u16,
    dst_cols: u16,
) -> Vec<Cell> {
    let mut dst = vec![Cell::default(); usize::from(dst_rows) * usize::from(dst_cols)];
    for r in 0..src_rows.min(dst_rows) {
        // `occupancy` is authoritative: the source tail past the
        // watermark holds undefined bytes. Requires `src` in
        // logical-row order so `src_occ[r]` indexes the same row.
        let occ = src_occ.get(usize::from(r)).copied().unwrap_or(src_cols);
        let live = src_cols.min(dst_cols).min(occ);
        for c in 0..live {
            let src_idx = usize::from(r) * usize::from(src_cols) + usize::from(c);
            let dst_idx = usize::from(r) * usize::from(dst_cols) + usize::from(c);
            dst[dst_idx] = src[src_idx];
        }
        if live == dst_cols && dst_cols < occ.min(src_cols) {
            let cut = usize::from(r) * usize::from(src_cols) + usize::from(dst_cols);
            // The discard sweep after the resize takes a sized pair,
            // block and all.
            if matches!(src[cut].grapheme, Grapheme::Spacer) && src[cut].sizing.is_none() {
                dst[usize::from(r) * usize::from(dst_cols) + usize::from(dst_cols) - 1] =
                    Cell::default();
            }
        }
    }
    dst
}

fn copy_within_cells(cells: &mut [Cell], src: std::ops::Range<usize>, dst_start: usize) {
    cells.copy_within(src, dst_start);
}

/// Public so callers outside this crate (the renderer's pre-edit
/// overlay) reuse the same tables: `felis-grid` is the single source
/// of truth for cell widths.
#[must_use]
pub fn char_cell_width(c: char) -> u8 {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0) as u8
}

impl Grid {}

/// xterm defines `id` as the only standard key; unknown keys are
/// ignored.
fn parse_osc_8_id(section: &[u8]) -> Option<String> {
    for kv in section.split(|b| *b == b':') {
        if let Some(rest) = kv.strip_prefix(b"id=") {
            return std::str::from_utf8(rest).ok().map(str::to_owned);
        }
    }
    None
}

/// Leading/trailing whitespace and `+` are rejected (`str::parse`
/// would accept them); the parser side already stripped C0 controls.
fn parse_palette_index(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Out-of-range indices drop, as in xterm.
const fn special_slot_from_osc5(idx: u16) -> Option<usize> {
    if (idx as usize) < SPECIAL_COLOR_COUNT {
        Some(idx as usize)
    } else {
        None
    }
}

const fn special_slot_from_osc4_alias(idx: u16) -> Option<usize> {
    let off = SPECIAL_COLOR_OSC4_OFFSET;
    if idx >= off && (idx - off) < SPECIAL_COLOR_COUNT as u16 {
        Some((idx - off) as usize)
    } else {
        None
    }
}

/// Default xterm-256color palette entry: 0..=7 classic ANSI (`0xCD` /
/// `0xE5`), 8..=15 bright, 16..=231 the 6×6×6 cube over
/// `[0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF]`, 232..=255 grayscale
/// `0x08 + 10n`.
#[must_use]
pub const fn default_palette_color(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (0, 0, 0),
        1 => (0xCD, 0, 0),
        2 => (0, 0xCD, 0),
        3 => (0xCD, 0xCD, 0),
        4 => (0, 0, 0xEE),
        5 => (0xCD, 0, 0xCD),
        6 => (0, 0xCD, 0xCD),
        7 => (0xE5, 0xE5, 0xE5),
        8 => (0x7F, 0x7F, 0x7F),
        9 => (0xFF, 0, 0),
        10 => (0, 0xFF, 0),
        11 => (0xFF, 0xFF, 0),
        12 => (0x5C, 0x5C, 0xFF),
        13 => (0xFF, 0, 0xFF),
        14 => (0, 0xFF, 0xFF),
        15 => (0xFF, 0xFF, 0xFF),
        16..=231 => {
            let n = idx - 16;
            let r = cube_axis(n / 36);
            let g = cube_axis((n / 6) % 6);
            let b = cube_axis(n % 6);
            (r, g, b)
        }
        232..=255 => {
            let level = 8 + (idx - 232) * 10;
            (level, level, level)
        }
    }
}

const fn cube_axis(i: u8) -> u8 {
    match i {
        0 => 0x00,
        1 => 0x5F,
        2 => 0x87,
        3 => 0xAF,
        4 => 0xD7,
        _ => 0xFF,
    }
}

/// xterm's default X resources: black foreground / cursor on white
/// background. esctest's `reset()` pins this via `OSC 10 ; #000` /
/// `OSC 11 ; #ffffff`, so `OSC 110` must round-trip to the same pair.
/// The renderer reads its own theme, not this constant.
#[must_use]
pub const fn default_dynamic_color(channel: ThemeChannel) -> (u8, u8, u8) {
    match channel {
        ThemeChannel::Background => (0xFF, 0xFF, 0xFF),
        ThemeChannel::Foreground | ThemeChannel::Cursor => (0, 0, 0),
    }
}

/// xterm leaves the special colors unset at startup (the attribute
/// falls through to fg/bg); `(0, 0, 0)` is the deterministic stand-in
/// so a reset-then-query round-trips. Rendering never consults it.
#[must_use]
pub const fn default_special_color() -> (u8, u8, u8) {
    (0, 0, 0)
}

struct Osc133 {
    kind: PromptKind,
    /// `D ; <code>` only.
    exit_code: Option<u32>,
    /// `A` only; `None` when the option is absent or unrecognized.
    redraw: Option<PromptRedraw>,
    /// `A ; k=s`: a continuation line, not the prompt's first.
    secondary: bool,
}

fn parse_osc_133(text: &str) -> Option<Osc133> {
    let mut parts = text.split(';');
    let kind_letter = parts.next()?;
    let kind = match kind_letter.chars().next()? {
        'A' => PromptKind::PromptStart,
        'B' => PromptKind::InputStart,
        'C' => PromptKind::OutputStart,
        'D' => PromptKind::CommandEnd,
        _ => return None,
    };
    let mut parsed = Osc133 {
        kind,
        exit_code: None,
        redraw: None,
        secondary: false,
    };
    match kind {
        PromptKind::CommandEnd => {
            parsed.exit_code = parts.next().and_then(|s| s.parse::<u32>().ok());
        }
        PromptKind::PromptStart => {
            for opt in parts {
                match opt.split_once('=') {
                    Some(("redraw", "0")) => parsed.redraw = Some(PromptRedraw::Never),
                    Some(("redraw", "1")) => parsed.redraw = Some(PromptRedraw::Full),
                    Some(("redraw", "last")) => parsed.redraw = Some(PromptRedraw::LastLine),
                    Some(("k", "s")) => parsed.secondary = true,
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Some(parsed)
}

/// Per `docs/explanation/security-model.md` "OSC 8 hyperlinks and OSC 7
/// CWD" a C0 / DEL byte truncates the sequence; felis drops the whole
/// dispatch. UTF-8 continuations are 0x80+, so the filter touches only
/// C0 + DEL.
fn sanitize_osc_str(body: &[u8]) -> Option<&str> {
    if body.iter().any(|b| *b < 0x20 || *b == 0x7F) {
        return None;
    }
    core::str::from_utf8(body).ok()
}

/// REQ-910 / `docs/explanation/security-model.md` "OSC 8 hyperlinks and
/// OSC 7 CWD": the URI scheme must be one of `http`, `https`, `mailto`,
/// `file`.
/// Comparison is ASCII case-insensitive per RFC 3986 §3.1.
fn osc8_scheme_allowed(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    ["http", "https", "mailto", "file"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

#[cfg(test)]
mod scrollback_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

impl Grid {
    #[must_use]
    pub fn apply_scroll_directive(
        &mut self,
        region_top: u16,
        region_bottom: u16,
        n_rows: u16,
        direction: ScrollDirection,
    ) -> bool {
        self.screen
            .apply_scroll_directive(region_top, region_bottom, n_rows, direction)
    }

    /// See [`ScreenBuffer::geometry_gen`].
    #[must_use]
    pub const fn geometry_gen(&self) -> u64 {
        self.screen.geometry_gen()
    }

    /// See [`ScreenBuffer::scroll_seq`].
    #[must_use]
    pub const fn scroll_seq(&self) -> u64 {
        self.screen.scroll_seq()
    }

    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.screen.cell(row, col)
    }

    #[must_use]
    pub fn cell_at_viewport(&self, viewport: u32, row: u16, col: u16) -> Option<Cell> {
        self.screen.cell_at_viewport(viewport, row, col)
    }

    #[must_use]
    pub fn cell_sizing(&self, row: u16, col: u16) -> Option<&Sizing> {
        self.screen.cell_sizing(row, col)
    }

    #[must_use]
    pub fn cell_sizing_handle(&self, row: u16, col: u16) -> Option<SizingHandle> {
        self.screen.cell_sizing_handle(row, col)
    }

    #[must_use]
    pub fn clamp_viewport(&self, requested: u32) -> u32 {
        self.screen.clamp_viewport(requested)
    }

    #[must_use]
    pub const fn cluster_count(&self) -> usize {
        self.screen.cluster_count()
    }

    #[must_use]
    pub fn cluster_str(&self, id: NonZeroU32) -> Option<&str> {
        self.screen.cluster_str(id)
    }

    #[must_use]
    pub const fn cluster_table(&self) -> &ClusterTable {
        self.screen.cluster_table()
    }

    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.screen.cols()
    }

    #[must_use]
    pub const fn cursor(&self) -> Cursor {
        self.screen.cursor()
    }

    #[must_use]
    pub const fn cursor_blink(&self) -> bool {
        self.screen.cursor_blink()
    }

    #[must_use]
    pub const fn cursor_style(&self) -> CursorStyle {
        self.screen.cursor_style()
    }

    pub fn install_cluster(&mut self, id: NonZeroU32, text: ClusterText) -> bool {
        self.screen.install_cluster(id, text)
    }

    pub fn install_sizing(&mut self, sizing: Sizing) -> Option<SizingHandle> {
        self.screen.install_sizing(sizing)
    }

    #[must_use]
    pub fn logical_line_bounds(&self, row: u16) -> (u16, u16) {
        self.screen.logical_line_bounds(row)
    }

    #[must_use]
    pub const fn on_alternate_screen(&self) -> bool {
        self.screen.on_alternate_screen()
    }

    #[must_use]
    pub fn placeholder_cells(&self) -> Vec<PlaceholderCell> {
        self.screen.placeholder_cells()
    }

    #[must_use]
    pub const fn reserved_cell_bytes(&self) -> usize {
        self.screen.reserved_cell_bytes()
    }

    #[must_use]
    pub fn row_cells(&self, row: u16) -> Option<&[Cell]> {
        self.screen.row_cells(row)
    }

    #[must_use]
    pub fn row_content(&self, row: u16) -> Option<&[Cell]> {
        self.screen.row_content(row)
    }

    #[must_use]
    pub fn row_sized_cells(&self, row: u16) -> Vec<(u16, Sizing)> {
        self.screen.row_sized_cells(row)
    }

    #[must_use]
    pub fn row_soft_wrap_continued(&self, row: u16) -> bool {
        self.screen.row_soft_wrap_continued(row)
    }

    #[must_use]
    pub fn row_view(&self, row: u16) -> ViewportRowView<'_> {
        self.screen.row_view(row)
    }

    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.screen.rows()
    }

    pub fn rows_with_wrap(&self, alt: AltScreenRows) -> impl Iterator<Item = (&[Cell], bool)> {
        self.screen.rows_with_wrap(alt)
    }

    #[must_use]
    pub fn scrollback(&self) -> ScrollbackView<'_> {
        self.screen.scrollback()
    }

    #[must_use]
    pub const fn scrollback_capacity(&self) -> usize {
        self.screen.scrollback_capacity()
    }

    pub fn set_cell(&mut self, row: u16, col: u16, cell: Cell) {
        self.screen.set_cell(row, col, cell);
    }

    pub fn set_cell_sizing(&mut self, row: u16, col: u16, handle: Option<SizingHandle>) {
        self.screen.set_cell_sizing(row, col, handle);
    }

    #[must_use]
    pub const fn set_cursor_state(
        &mut self,
        row: u16,
        col: u16,
        visible: bool,
        style: CursorStyle,
        blink: bool,
    ) -> bool {
        self.screen
            .set_cursor_state(row, col, visible, style, blink)
    }

    pub fn set_soft_wrap(&mut self, row: u16, continued: bool) {
        self.screen.set_soft_wrap(row, continued);
    }

    #[must_use]
    pub fn sizing_by_handle(&self, handle: SizingHandle) -> Option<&Sizing> {
        self.screen.sizing_by_handle(handle)
    }

    #[must_use]
    pub const fn sizing_count(&self) -> usize {
        self.screen.sizing_count()
    }

    #[must_use]
    pub fn style(&self, id: StyleId) -> &Attributes {
        self.screen.style(id)
    }

    #[must_use]
    pub const fn style_table(&self) -> &StyleTable {
        self.screen.style_table()
    }

    pub const fn style_table_mut(&mut self) -> &mut StyleTable {
        self.screen.style_table_mut()
    }

    #[must_use]
    pub fn viewport_row(&self, viewport: u32, row: u16) -> Option<ViewportRowView<'_>> {
        self.screen.viewport_row(viewport, row)
    }

    pub fn write_row_cells(&mut self, row: u16, cells: &[Cell]) -> bool {
        self.screen.write_row_cells(row, cells)
    }
}

impl Grid {
    /// `None` for ids past the table, which happens briefly during
    /// reattach when a `Hyperlink` message has not yet streamed.
    #[must_use]
    pub fn hyperlink(&self, id: NonZeroU16) -> Option<&HyperlinkEntry> {
        self.screen.hyperlink(id)
    }

    /// The daemon's own table is gapless, so this is its interned count.
    #[must_use]
    pub const fn hyperlink_count(&self) -> usize {
        self.screen.hyperlink_count()
    }

    #[must_use]
    pub const fn hyperlink_table(&self) -> &LinkTable {
        self.screen.hyperlink_table()
    }
}

/// Each sweep re-establishes the one handle the parser holds outside
/// the cells, which is why neither is a bare delegate.
impl Grid {
    /// Returns the cells scanned, which the sweep policy uses for its
    /// next trigger point.
    pub fn gc_styles(&mut self) -> usize {
        let (scanned, compacted) = self.screen.gc_styles();
        if compacted {
            // The pen memo's id was not necessarily on any cell, so
            // re-intern rather than remap.
            self.resync_pen_style();
        }
        scanned
    }

    pub fn gc_sizings(&mut self) {
        let remap = self.screen.gc_sizings();
        self.current_sizing_handle = self
            .current_sizing_handle
            .and_then(|h| remap.get(h.get() as usize - 1).copied().flatten());
    }
}

impl Grid {
    #[must_use]
    pub const fn screen(&self) -> &ScreenBuffer {
        &self.screen
    }
}
