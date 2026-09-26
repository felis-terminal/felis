//! [`GridMsg`]: daemon→client grid diffs (kind 2), with the
//! cursor/prompt/theme vocabulary its variants carry.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

use super::GridDims;
use crate::kitty_keyboard::KittyKbdFlags;
use crate::row::RowPayload;

/// Grid-family messages ([`crate::MessageKind::Grid`], kind 2). Always
/// daemon → client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridMsg {
    /// Start of a fresh-attach rehydration; the client clears its
    /// shadow screen before consuming the `RowDelta` burst. Dimensions
    /// come from the preceding [`SessionToClientMsg::Attached`](super::SessionToClientMsg::Attached)
    /// or the latest [`Self::Size`].
    RehydrateBegin,
    /// End of the rehydration burst; subsequent messages are live diffs.
    RehydrateEnd,
    /// One cycle's worth of dirty-row cell data in a single frame.
    /// The layout of `packed_cells` is owned by `felis-grid` and
    /// specified by `docs/reference/row-codec.md`; a new row codec
    /// version is gated on the effective minor rather than a schema
    /// change.
    RowDelta {
        /// `(row, packed_cells)` pairs, zero-based. A later entry for
        /// the same `row` overrides an earlier one.
        rows: Vec<(u16, RowPayload)>,
    },
    /// The cell rows in `region_top ..= region_bottom` shifted by
    /// `n_rows` in `direction`; the shadow performs the same in-place
    /// shift. Pre-scroll dirty-row state rides `RowDelta` frames
    /// before this directive; new content past the scroll rides
    /// `RowDelta` frames after (`docs/reference/ipc.md`).
    Scrolled {
        /// Inclusive.
        region_top: u16,
        /// Inclusive.
        region_bottom: u16,
        /// Always ≥ 1.
        n_rows: u16,
        direction: ScrollDirection,
    },
    /// New grapheme-cluster registry entry (`docs/explanation/data-model/grid-and-cells.md`).
    ///
    /// Entries are shipped in reference order before any row referencing them.
    Cluster {
        /// 1-based; `0` is reserved.
        id: u32,
        /// Base char plus combining marks.
        text: String,
    },
    /// New hyperlink registry entry (`OSC 8`). Every entry a row
    /// references is shipped before that row, as for
    /// [`GridMsg::Cluster`].
    Hyperlink {
        /// 1-based; `0` is reserved (cells with no link).
        id: u16,
        /// `OSC 8 id=…`, when supplied.
        anchor: Option<String>,
        /// Control bytes already rejected on the daemon per
        /// `security-model.md`.
        uri: String,
    },
    CursorState {
        /// Zero-based.
        row: u16,
        /// Zero-based.
        col: u16,
        /// DECTCEM.
        visible: bool,
        /// `DECSCUSR` shape.
        style: CursorStyle,
        /// Whether the program asked for the blinking `DECSCUSR`
        /// variant; the client owns the blink animation.
        blink: bool,
    },
    /// Effective viewport after the daemon clamped the client's
    /// [`InputMsg::Viewport`](super::InputMsg::Viewport) request, shipped
    /// whenever the value or `max` changes.
    ViewportState {
        /// `0` is the live bottom; `N>0` means the top `min(N, rows)`
        /// rows of the stream come from the youngest scrollback.
        lines_from_bottom: u32,
        /// Retained scrollback rows + live row count.
        max: u32,
    },
    /// Authoritative grid dimensions changed mid-stream.
    ///
    /// Shipped when the active client resizes the PTY or to correct a client
    /// (`docs/explanation/architecture/session-lifecycle.md`).
    Size { dims: GridDims },
    /// `OSC 0` / `OSC 2`.
    Title {
        /// Control-byte-stripped at the parser side.
        value: String,
    },
    /// `OSC 7`, typically a `file://hostname/path` URL.
    Cwd {
        /// Control-byte-stripped at the parser side.
        value: String,
    },
    /// Shell semantic-prompt boundary (`OSC 133`).
    PromptMark {
        /// Absolute line the mark fired on
        /// (`docs/explanation/data-model/scrollback.md`): cumulative rows
        /// pushed into scrollback plus the cursor row at dispatch time,
        /// so it survives scrolling and resize.
        line: u64,
        kind: PromptKind,
        /// `Some(code)` for `OSC 133 ; D ; <code>`.
        exit_code: Option<u32>,
    },
    /// `OSC 10/11/12` (set) or `OSC 110/111/112` (reset).
    ThemeColor {
        channel: ThemeChannel,
        action: ThemeAction,
    },
    /// One indexed-palette slot changed (`OSC 4 ; idx ; spec`, or
    /// `OSC 104 ; idx`). An `OSC 4` carrying several pairs emits one
    /// message per pair.
    PaletteColor { index: u8, action: PaletteAction },
    /// Bare `OSC 104`: every index back to its xterm-256 baseline.
    /// Touches only the indexed palette.
    PaletteResetAll,
    /// `OSC 22 ; <css-cursor-name> ST` (kitty's pointer-shape
    /// extension). Unknown keywords fall back to the default.
    PointerShape {
        /// CSS cursor keyword, or `None` to reset to the default arrow.
        name: Option<String>,
    },
    /// Active Kitty keyboard flag bitmap, the top of the daemon's
    /// per-grid push/pop stack. Emitted on rehydrate (always, even `0`)
    /// and on change.
    KittyKbdFlags {
        /// Empty means the legacy encoding is active.
        flags: KittyKbdFlags,
    },
    /// Mode-bit mirror, shipped on every change and on rehydrate.
    ModeFlags {
        /// `?2004`.
        bracketed_paste: bool,
        /// `?1049`.
        alt_screen: bool,
        mouse_protocol: MouseProtocol,
        /// `?1` DECCKM.
        application_cursor: bool,
        /// `CSI > 4 ; Pv m`. When not `Off` the daemon's key encoder
        /// routes modified keys through `CSI keycode ; mod u` (REQ-506).
        modify_other_keys: ModifyOtherKeys,
        /// DECKPAM / DECKPNM.
        application_keypad: bool,
        /// `?9001`. When on, the daemon's key encoder emits
        /// win32-input-mode key records so `ConPTY` can rebuild
        /// `INPUT_RECORD`s; `PSReadLine` needs it to receive keystrokes.
        win32_input_mode: bool,
        /// `?5` DECSCNM. The renderer XORs this with each cell's SGR 7.
        reverse_video: bool,
    },
    /// The session wants the user's attention. The client's handler is
    /// `Window::request_user_attention` whatever the source; felis
    /// never draws a banner (`docs/reference/protocols/notifications.md`).
    Attention { source: AttentionSource },
    /// `OSC 52` clipboard write, carried to the initiating client only,
    /// never broadcast (`docs/reference/ipc.md`). Whether it reaches
    /// the OS clipboard is that client's `clipboard.osc_52` opt-in.
    ClipboardSet { write: ClipboardWrite },
    /// End of one pull-paced compose cycle: the client clears its
    /// outstanding pull here and paints the cycle as a whole
    /// (`docs/reference/ipc.md` "Grid (kind = 2)"). An empty cycle
    /// ships nothing at all, this marker included.
    CycleEnd,
}

const fn arm(name: &'static str) -> ArmMeta {
    ArmMeta::new(
        name,
        Direction::ToClient,
        CorrelationClass::Uncorrelated,
        ModeSet::ATTACHERS,
    )
}

impl Directed for GridMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm("Grid::RehydrateBegin"),
        arm("Grid::RehydrateEnd"),
        arm("Grid::RowDelta"),
        arm("Grid::Scrolled"),
        arm("Grid::Cluster"),
        arm("Grid::Hyperlink"),
        arm("Grid::CursorState"),
        arm("Grid::ViewportState"),
        arm("Grid::Size"),
        arm("Grid::Title"),
        arm("Grid::Cwd"),
        arm("Grid::PromptMark"),
        arm("Grid::ThemeColor"),
        arm("Grid::PointerShape"),
        arm("Grid::KittyKbdFlags"),
        arm("Grid::ModeFlags"),
        arm("Grid::Attention"),
        arm("Grid::ClipboardSet"),
        arm("Grid::PaletteColor"),
        arm("Grid::PaletteResetAll"),
        arm("Grid::CycleEnd"),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::RehydrateBegin => 0,
            Self::RehydrateEnd => 1,
            Self::RowDelta { .. } => 2,
            Self::Scrolled { .. } => 3,
            Self::Cluster { .. } => 4,
            Self::Hyperlink { .. } => 5,
            Self::CursorState { .. } => 6,
            Self::ViewportState { .. } => 7,
            Self::Size { .. } => 8,
            Self::Title { .. } => 9,
            Self::Cwd { .. } => 10,
            Self::PromptMark { .. } => 11,
            Self::ThemeColor { .. } => 12,
            Self::PointerShape { .. } => 13,
            Self::KittyKbdFlags { .. } => 14,
            Self::ModeFlags { .. } => 15,
            Self::Attention { .. } => 16,
            Self::ClipboardSet { .. } => 17,
            Self::PaletteColor { .. } => 18,
            Self::PaletteResetAll => 19,
            Self::CycleEnd => 20,
        }
    }
}

/// One `OSC 52` write request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardWrite {
    pub selection: ClipboardSelection,
    /// Decoded bytes (post-base64); the spec does not constrain charset.
    pub data: Vec<u8>,
}

/// What made a [`GridMsg::Attention`] fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionSource {
    /// One BEL event, coalesced from every `0x07` byte in the same
    /// chunk.
    Bell,
    /// A desktop notification (OSC 9 / 99 / 777) fired on this session;
    /// its content goes to observers via
    /// [`NotifyToClientMsg::Event`](super::NotifyToClientMsg::Event), not here.
    Notification,
}

/// Direction of a [`GridMsg::Scrolled`] shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollDirection {
    /// Rows shift toward lower row indices; the bottom band becomes
    /// blank (IND).
    Up,
    /// Rows shift toward higher row indices; the top band becomes
    /// blank (RI).
    Down,
}

/// Semantic-prompt boundary kind, per `OSC 133`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptKind {
    /// `A`.
    PromptStart,
    /// `B`.
    InputStart,
    /// `C`.
    OutputStart,
    /// `D`.
    CommandEnd,
}

/// Which theme channel an `OSC 10`/`11`/`12` (or `110`/`111`/`112`)
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChannel {
    /// `OSC 10`.
    Foreground,
    /// `OSC 11`.
    Background,
    /// `OSC 12`.
    Cursor,
}

/// What a [`GridMsg::ThemeColor`] does to its channel. Variants rather
/// than a present / absent color, so a truncated wire value fails the
/// decode instead of reading as a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeAction {
    /// `OSC 10/11/12`.
    Set { rgb: (u8, u8, u8) },
    /// `OSC 110/111/112`.
    Reset,
}

/// Cursor shape, set by `DECSCUSR` (`CSI Ps SP q`); the blink half of
/// the parameter rides [`GridMsg::CursorState::blink`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorStyle {
    /// DECSCUSR `0` / `1` / `2`.
    #[default]
    Block,
    /// DECSCUSR `3` / `4`.
    Underline,
    /// DECSCUSR `5` / `6`.
    Bar,
}

/// What a [`GridMsg::PaletteColor`] does to its slot. Variants rather
/// than a present / absent color, so a truncated wire value fails the
/// decode instead of reading as a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaletteAction {
    /// `OSC 4 ; idx ; spec`.
    Set { rgb: (u8, u8, u8) },
    /// `OSC 104 ; idx`.
    Reset,
}

/// Mouse-event reporting level. Mutually exclusive in xterm: the last
/// `?1XXXh` wins, and the matching `?1XXXl` turns reporting off.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseProtocol {
    #[default]
    Off,
    /// `?1000`.
    ButtonEvents,
    /// `?1002`.
    ButtonAndDrag,
    /// `?1003`.
    AnyMotion,
}

/// xterm `modifyOtherKeys` level (`CSI > 4 ; Pv m`). xterm defines
/// exactly three levels, so the parser clamps a higher `Pv` to
/// [`Self::Level2`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModifyOtherKeys {
    /// `Pv = 0`.
    #[default]
    Off,
    /// `Pv = 1`: escape modified keys that lack a legacy byte form.
    Level1,
    /// `Pv = 2`: escape every modified key.
    Level2,
}

bitflags! {
    /// Targets an `OSC 52` write request can name; xterm allows several
    /// at once (`cp`). Other selectors (`q`, `s`, cut buffers) are
    /// ignored.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ClipboardSelection: u8 {
        /// `c`.
        const CLIPBOARD = 1 << 0;
        /// `p`, X11 PRIMARY.
        const PRIMARY   = 1 << 1;
    }
}

// Serialized as the raw `u8` bits; `from_bits_truncate` drops unknown
// selectors.
impl Serialize for ClipboardSelection {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.bits().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ClipboardSelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_bits_truncate(u8::deserialize(deserializer)?))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    fn grid_cases() -> Vec<GridMsg> {
        vec![
            GridMsg::RehydrateBegin,
            GridMsg::RehydrateEnd,
            GridMsg::RowDelta {
                rows: vec![(5, RowPayload(vec![0xDE, 0xAD, 0xBE, 0xEF]))],
            },
            GridMsg::RowDelta { rows: vec![] },
            GridMsg::RowDelta {
                rows: vec![
                    (0, RowPayload(vec![0u8; 32])),
                    (5, RowPayload(vec![0xAB; 80])),
                ],
            },
            GridMsg::RowDelta {
                rows: (0..24)
                    .map(|r| (r as u16, RowPayload(vec![0xCD; 4 * 80])))
                    .collect(),
            },
            GridMsg::Scrolled {
                region_top: 0,
                region_bottom: 23,
                n_rows: 1,
                direction: ScrollDirection::Up,
            },
            GridMsg::Scrolled {
                region_top: 5,
                region_bottom: 18,
                n_rows: 7,
                direction: ScrollDirection::Down,
            },
            GridMsg::Scrolled {
                region_top: 0,
                region_bottom: u16::MAX,
                n_rows: u16::MAX,
                direction: ScrollDirection::Up,
            },
            GridMsg::Cluster {
                id: 1,
                text: "e\u{301}".into(),
            },
            GridMsg::Cluster {
                id: u32::MAX,
                text: "👩‍🔬".into(),
            },
            GridMsg::Hyperlink {
                id: 1,
                anchor: Some("anchor-1".into()),
                uri: "https://example.test/path".into(),
            },
            GridMsg::Hyperlink {
                id: 2,
                anchor: None,
                uri: "file:///etc/hosts".into(),
            },
            GridMsg::Hyperlink {
                id: u16::MAX,
                anchor: None,
                uri: "u".into(),
            },
            GridMsg::CursorState {
                row: 1,
                col: 2,
                visible: true,
                style: CursorStyle::Bar,
                blink: true,
            },
            GridMsg::ViewportState {
                lines_from_bottom: 0,
                max: 24,
            },
            GridMsg::ViewportState {
                lines_from_bottom: 9_999,
                max: 10_024,
            },
            GridMsg::ViewportState {
                lines_from_bottom: u32::MAX,
                max: u32::MAX,
            },
            GridMsg::Size {
                dims: GridDims {
                    rows: 30,
                    cols: 120,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            },
            GridMsg::Title {
                value: "felis · /tmp".into(),
            },
            GridMsg::Cwd {
                value: "file://localhost/home/user".into(),
            },
            GridMsg::PromptMark {
                line: 7,
                kind: PromptKind::PromptStart,
                exit_code: None,
            },
            GridMsg::PromptMark {
                line: 9,
                kind: PromptKind::CommandEnd,
                exit_code: Some(2),
            },
            GridMsg::ThemeColor {
                channel: ThemeChannel::Foreground,
                action: ThemeAction::Set {
                    rgb: (0xAB, 0xCD, 0xEF),
                },
            },
            GridMsg::ThemeColor {
                channel: ThemeChannel::Background,
                action: ThemeAction::Reset,
            },
            GridMsg::ThemeColor {
                channel: ThemeChannel::Cursor,
                action: ThemeAction::Set {
                    rgb: (0xFF, 0x00, 0x00),
                },
            },
            GridMsg::PaletteColor {
                index: 0,
                action: PaletteAction::Set {
                    rgb: (0x11, 0x22, 0x33),
                },
            },
            GridMsg::PaletteColor {
                index: u8::MAX,
                action: PaletteAction::Set {
                    rgb: (0xFF, 0xFF, 0xFF),
                },
            },
            GridMsg::PaletteColor {
                index: 7,
                action: PaletteAction::Reset,
            },
            GridMsg::PaletteResetAll,
            GridMsg::CycleEnd,
            GridMsg::PointerShape {
                name: Some("text".into()),
            },
            GridMsg::PointerShape { name: None },
            GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::empty(),
            },
            GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::DISAMBIGUATE,
            },
            GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT,
            },
            GridMsg::KittyKbdFlags {
                flags: KittyKbdFlags::all(),
            },
            GridMsg::ModeFlags {
                bracketed_paste: true,
                alt_screen: false,
                mouse_protocol: MouseProtocol::AnyMotion,
                application_cursor: true,
                modify_other_keys: ModifyOtherKeys::Off,
                application_keypad: true,
                win32_input_mode: false,
                reverse_video: false,
            },
            GridMsg::ModeFlags {
                bracketed_paste: false,
                alt_screen: true,
                mouse_protocol: MouseProtocol::Off,
                application_cursor: false,
                modify_other_keys: ModifyOtherKeys::Level2,
                application_keypad: false,
                win32_input_mode: true,
                reverse_video: true,
            },
            GridMsg::ModeFlags {
                bracketed_paste: false,
                alt_screen: false,
                mouse_protocol: MouseProtocol::ButtonEvents,
                application_cursor: false,
                modify_other_keys: ModifyOtherKeys::Level1,
                application_keypad: false,
                win32_input_mode: false,
                reverse_video: true,
            },
            GridMsg::ModeFlags {
                bracketed_paste: false,
                alt_screen: false,
                mouse_protocol: MouseProtocol::ButtonAndDrag,
                application_cursor: false,
                modify_other_keys: ModifyOtherKeys::Off,
                application_keypad: false,
                win32_input_mode: false,
                reverse_video: false,
            },
            GridMsg::Attention {
                source: AttentionSource::Bell,
            },
            GridMsg::Attention {
                source: AttentionSource::Notification,
            },
            GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::CLIPBOARD,
                    data: b"hello".to_vec(),
                },
            },
            GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY,
                    data: vec![],
                },
            },
            GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::empty(),
                    data: vec![],
                },
            },
        ]
    }

    #[test]
    fn grid_messages_round_trip() {
        for msg in grid_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn grid_cases_cover_every_variant() {
        assert_covers_every_arm(&grid_cases());
    }
}
