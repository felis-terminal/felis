//! OSC 8 hyperlink helpers: lookup, click, and hover discovery.

#![allow(unreachable_pub)]

#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};

use felis_client_core::{ActivationTarget, ShadowScreen};

use crate::GridPos;
use winit::{keyboard::ModifiersState, window::CursorIcon};

/// `None` for cells without a link or when the link table lacks the entry
/// (mid-rehydrate edge).
#[must_use]
pub fn url_at(shadow: &ShadowScreen, cell: GridPos) -> Option<&str> {
    let cell = shadow.screen().cell(cell.row, cell.col)?;
    let id = cell.link?;
    shadow.hyperlink(id).map(|entry| entry.uri.as_str())
}

/// Resolves the target activated by Ctrl+Left over `cell`, or `None`.
///
/// The single source of truth for the pointer icon, preview bar, and click handler.
/// Returns `None` if Ctrl is absent, the URI fails validation, or `preview_row_free`
/// is false (REQ-910 requires the preview to be visible before opening).
#[must_use]
pub fn activation_target(
    shadow: &ShadowScreen,
    cell: Option<GridPos>,
    mods: ModifiersState,
    preview_row_free: bool,
) -> Option<ActivationTarget> {
    if !mods.control_key() || !preview_row_free {
        return None;
    }
    let uri = url_at(shadow, cell?)?;
    match ActivationTarget::parse(uri) {
        Ok(target) => Some(target),
        Err(rejection) => {
            // The rejection carries no part of the URI
            // (`docs/explanation/security-model.md`
            // "OSC 8 hyperlinks and OSC 7 CWD").
            tracing::debug!(?rejection, "OSC 8 activation refused");
            None
        }
    }
}

/// `Pointer` while an activation is armed (the Ctrl+Click affordance),
/// overriding any program request; otherwise the program's `OSC 22` shape
/// applies (`None` ⇒ the default arrow), so program-side mouse reporting
/// (vim's `set mouse=a`, lazygit) keeps the arrow.
#[must_use]
pub fn pointer_icon(shadow: &ShadowScreen, activation_armed: bool) -> CursorIcon {
    if activation_armed {
        return CursorIcon::Pointer;
    }
    shadow
        .pointer_shape()
        .map_or(CursorIcon::Default, css_cursor_to_icon)
}

/// Whether an applied `GridMsg` can move the activation target under a stationary pointer.
///
/// Avoids allocating and re-parsing URIs on palette, title, or other messages that cannot
/// alter cells, hyperlinks, or pointer shape.
#[must_use]
pub const fn grid_msg_moves_hover_target(msg: &felis_protocol::messages::GridMsg) -> bool {
    use felis_protocol::messages::GridMsg as M;
    matches!(
        msg,
        M::RowDelta { .. }
            | M::Scrolled { .. }
            | M::Hyperlink { .. }
            | M::RehydrateBegin
            | M::RehydrateEnd
            | M::ViewportState { .. }
            | M::Size { .. }
            | M::PointerShape { .. }
    )
}

/// Verifies that the last presented frame displayed `target` before allowing activation.
///
/// `presented` is `None` when chrome bars or IME compositions claim the bottom row.
/// Because these states change without mouse events, clicks check the rendered frame
/// directly to honor REQ-910 ("no preview, no activation").
#[must_use]
pub fn activation_matches_preview(
    presented: Option<&ActivationTarget>,
    target: &ActivationTarget,
) -> bool {
    presented == Some(target)
}

/// The `OSC 22` payload is a CSS `cursor` keyword; an unknown-but-valid
/// keyword falls back to the default arrow rather than erroring, matching
/// every other terminal.
#[must_use]
fn css_cursor_to_icon(name: &str) -> CursorIcon {
    match name {
        "context-menu" => CursorIcon::ContextMenu,
        "help" => CursorIcon::Help,
        "pointer" => CursorIcon::Pointer,
        "progress" => CursorIcon::Progress,
        "wait" => CursorIcon::Wait,
        "cell" => CursorIcon::Cell,
        "crosshair" => CursorIcon::Crosshair,
        "text" => CursorIcon::Text,
        "vertical-text" => CursorIcon::VerticalText,
        "alias" => CursorIcon::Alias,
        "copy" => CursorIcon::Copy,
        "move" => CursorIcon::Move,
        "no-drop" => CursorIcon::NoDrop,
        "not-allowed" => CursorIcon::NotAllowed,
        "grab" => CursorIcon::Grab,
        "grabbing" => CursorIcon::Grabbing,
        "e-resize" => CursorIcon::EResize,
        "n-resize" => CursorIcon::NResize,
        "ne-resize" => CursorIcon::NeResize,
        "nw-resize" => CursorIcon::NwResize,
        "s-resize" => CursorIcon::SResize,
        "se-resize" => CursorIcon::SeResize,
        "sw-resize" => CursorIcon::SwResize,
        "w-resize" => CursorIcon::WResize,
        "ew-resize" => CursorIcon::EwResize,
        "ns-resize" => CursorIcon::NsResize,
        "nesw-resize" => CursorIcon::NeswResize,
        "nwse-resize" => CursorIcon::NwseResize,
        "col-resize" => CursorIcon::ColResize,
        "row-resize" => CursorIcon::RowResize,
        "all-scroll" => CursorIcon::AllScroll,
        "zoom-in" => CursorIcon::ZoomIn,
        "zoom-out" => CursorIcon::ZoomOut,
        // "default", "none" (no cell-model equivalent), and unknown keywords.
        _ => CursorIcon::Default,
    }
}

/// The argv shape the launcher hands to `Command`: a program name plus
/// the target as one whole argument, never a shell command line. Factored
/// out of `open_url` so the boundary is assertable in a unit test the way
/// `ShellExecuteArgs` pins the Windows shape below.
#[cfg(not(target_os = "windows"))]
fn launch_argv(target: &ActivationTarget) -> (&'static str, &str) {
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "macos")]
    let program = "open";
    (program, target.as_str())
}

/// Launches URLs via `xdg-open` (Linux) or `open` (macOS).
///
/// Waits on the spawned child in a background thread to reap zombies without
/// blocking the UI thread during browser startup.
#[cfg(not(target_os = "windows"))]
pub fn open_url(target: &ActivationTarget) -> std::io::Result<()> {
    let (program, arg) = launch_argv(target);
    let mut child = Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    drop(std::thread::spawn(move || {
        drop(child.wait());
    }));
    Ok(())
}

/// Passes URLs to Windows `ShellExecuteW` as an inert wide string.
///
/// Uses `ShellExecuteW` rather than `cmd /c start` to prevent shell injection via URI meta-characters.
/// Runs on a background thread to avoid blocking on registry resolution or handler cold starts.
#[cfg(target_os = "windows")]
pub fn open_url(target: &ActivationTarget) -> std::io::Result<()> {
    let args = ShellExecuteArgs::for_url(target)?;
    drop(std::thread::spawn(move || {
        if let Err(err) = args.execute() {
            tracing::warn!(?err, "the Windows shell declined the URL");
        }
    }));
    Ok(())
}

/// A value rather than three expressions at the call site so the hand-off
/// shape (URL as one whole argument, no command line) is assertable on
/// the hosts felis's CI has.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug)]
struct ShellExecuteArgs {
    /// `lpOperation`.
    verb: Vec<u16>,
    /// `lpFile`: the entire URL.
    file: Vec<u16>,
    /// `lpParameters`. Always `None`; modeled so its absence is assertable.
    parameters: Option<Vec<u16>>,
}

#[cfg(any(target_os = "windows", test))]
impl ShellExecuteArgs {
    fn for_url(target: &ActivationTarget) -> std::io::Result<Self> {
        Ok(Self {
            // The explicit "open" verb: a handler whose registered default
            // verb is "edit" or "print" would otherwise do that to the URL.
            verb: wide_z("open")?,
            // `wide_z`'s NUL check stays as defense in depth even though
            // `ActivationTarget::parse` already refuses one: this call
            // must stay safe against any future caller that skips parse.
            file: wide_z(target.as_str())?,
            parameters: None,
        })
    }
}

#[cfg(target_os = "windows")]
impl ShellExecuteArgs {
    /// Run the hand-off. Blocking, so it belongs off the UI thread.
    fn execute(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        };

        // Handlers may require an initialized COM apartment on this thread.
        // Initializing here leaves winit as sole owner of the UI thread apartment.
        // Only non-negative codes take a reference that requires uninitializing.

        // SAFETY: both are documented per-thread COM lifecycle calls
        // with no pointer arguments; null is the documented value for
        // the reserved one.
        #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
        let com = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        // SAFETY: `ShellExecuteW` reads the NUL-terminated UTF-16
        // buffers for the duration of the call and retains none; all
        // outlive it in `self`. The null `hwnd` / `lpdirectory`
        // arguments are the documented "no owner window" and "inherit
        // the current directory" cases, and a null `lpparameters` is
        // "no command line".
        #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
        let rc = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(),
                self.verb.as_ptr(),
                self.file.as_ptr(),
                self.parameters
                    .as_ref()
                    .map_or(std::ptr::null(), Vec::as_ptr),
                std::ptr::null(),
                windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            )
        };
        if com >= 0 {
            // SAFETY: balances the `CoInitializeEx` above, on the same
            // thread, and only when that call took a reference.
            #[allow(unsafe_code)] // against the workspace `unsafe_code = "deny"`.
            unsafe {
                CoUninitialize();
            }
        }
        // The documented success test is "greater than 32"; at or below it
        // the returned value is the error code and `GetLastError` is not
        // set, so `last_os_error` would report an unrelated errno.
        if rc.addr() > 32 {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "ShellExecuteW declined the URL (code {})",
                rc.addr()
            )))
        }
    }
}

/// An interior NUL is rejected rather than truncating: the URI would
/// otherwise reach the shell as its own prefix.
///
/// Compiled off Windows as well so the metacharacter suite runs on the CI
/// that exists.
#[cfg(any(target_os = "windows", test))]
fn wide_z(url: &str) -> std::io::Result<Vec<u16>> {
    if url.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "URL contains an interior NUL",
        ));
    }
    Ok(url.encode_utf16().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use felis_client_core::ActivationRejection;
    use felis_protocol::{RowPayload, messages::GridMsg};

    use super::*;

    /// How the daemon ships a plain row: unpadded, no OSC 66 band, clear
    /// wrap bit.
    fn packed_row(cells: &[felis_grid::Cell]) -> Vec<u8> {
        felis_grid::encode_row(
            felis_grid::RowEncode {
                cells,
                pad_to: cells.len(),
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            &felis_grid::StyleTable::new(),
        )
        .unwrap()
    }

    /// 1×5 `ShadowScreen` with `cell[0]` carrying OSC 8 link id 1 →
    /// `<https://example.com>`.
    fn shadow_with_link() -> ShadowScreen {
        shadow_with_link_uri("https://example.com")
    }

    /// As `shadow_with_link`, but with a caller-chosen URI, for building a
    /// link table entry that the grid's own C0/DEL filter lets through
    /// (`felis_grid::osc_dispatch::dispatch_osc_8`) but that
    /// `ActivationTarget::parse` refuses at the activation boundary.
    fn shadow_with_link_uri(uri: &str) -> ShadowScreen {
        let mut shadow = ShadowScreen::new(1, 5);
        shadow
            .apply(&GridMsg::Hyperlink {
                id: 1,
                anchor: None,
                uri: uri.to_owned(),
            })
            .unwrap();
        let mut cells: Vec<_> = (0..5).map(|_| felis_grid::Cell::default()).collect();
        cells[0].link = NonZeroU16::new(1);
        let body = packed_row(&cells);
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(body))],
            })
            .unwrap();
        shadow
    }

    const fn at(row: u16, col: u16) -> GridPos {
        GridPos { row, col }
    }

    fn ctrl() -> ModifiersState {
        let mut m = ModifiersState::empty();
        m |= ModifiersState::CONTROL;
        m
    }

    /// The composition `App::update_mouse_cursor_icon` performs: the icon
    /// is a function of the armed activation, never of the cell on its
    /// own. Asserting through it is what keeps the affordance and the
    /// activation from disagreeing.
    fn icon_for(
        shadow: &ShadowScreen,
        cell: Option<GridPos>,
        mods: ModifiersState,
        preview_row_free: bool,
    ) -> CursorIcon {
        pointer_icon(
            shadow,
            activation_target(shadow, cell, mods, preview_row_free).is_some(),
        )
    }

    #[test]
    fn pointer_icon_is_default_without_ctrl_held() {
        // Hover-only keeps the arrow; otherwise lazygit / vim users would
        // see Pointer wherever a shell printed an OSC 8 cell.
        let shadow = shadow_with_link();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 0)), ModifiersState::empty(), true),
            CursorIcon::Default
        );
    }

    #[test]
    fn pointer_icon_is_pointer_when_ctrl_hovers_a_link_cell() {
        let shadow = shadow_with_link();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 0)), ctrl(), true),
            CursorIcon::Pointer
        );
    }

    #[test]
    fn program_osc_22_shape_applies_without_ctrl() {
        let mut shadow = shadow_with_link();
        shadow
            .apply(&GridMsg::PointerShape {
                name: Some("text".to_owned()),
            })
            .unwrap();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 1)), ModifiersState::empty(), true),
            CursorIcon::Text
        );
    }

    #[test]
    fn ctrl_over_link_overrides_program_shape() {
        // The Ctrl+Click open is imminent.
        let mut shadow = shadow_with_link();
        shadow
            .apply(&GridMsg::PointerShape {
                name: Some("text".to_owned()),
            })
            .unwrap();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 0)), ctrl(), true),
            CursorIcon::Pointer
        );
    }

    #[test]
    fn osc_22_reset_returns_to_default_arrow() {
        let mut shadow = shadow_with_link();
        shadow
            .apply(&GridMsg::PointerShape {
                name: Some("wait".to_owned()),
            })
            .unwrap();
        shadow.apply(&GridMsg::PointerShape { name: None }).unwrap();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 1)), ModifiersState::empty(), true),
            CursorIcon::Default
        );
    }

    #[test]
    fn css_cursor_to_icon_maps_known_and_falls_back_for_unknown() {
        assert_eq!(css_cursor_to_icon("crosshair"), CursorIcon::Crosshair);
        assert_eq!(css_cursor_to_icon("ew-resize"), CursorIcon::EwResize);
        assert_eq!(css_cursor_to_icon("not-allowed"), CursorIcon::NotAllowed);
        // "none" has no cell-model equivalent.
        assert_eq!(css_cursor_to_icon("none"), CursorIcon::Default);
        assert_eq!(css_cursor_to_icon("made-up"), CursorIcon::Default);
    }

    #[test]
    fn pointer_icon_stays_default_when_the_stored_uri_fails_activation() {
        // The cursor icon must agree with `ActivationTarget::parse`'s refusal so the
        // pointer never promises a click that activation then refuses.
        let uri = "https://example.com/\u{202e}gpj.exe";
        let shadow = shadow_with_link_uri(uri);
        assert_eq!(url_at(&shadow, at(0, 0)), Some(uri));
        assert_eq!(
            ActivationTarget::parse(uri),
            Err(ActivationRejection::BidiControl)
        );
        assert_eq!(
            icon_for(&shadow, Some(at(0, 0)), ctrl(), true),
            CursorIcon::Default
        );
    }

    #[test]
    fn a_refused_uri_is_no_activation_so_the_press_takes_the_normal_route() {
        // `on_mouse_input` treats `None` as "not a link click": the press
        // reaches `held_buttons` and the program. Swallowing it instead
        // would let a producer blanket the screen with bidi-poisoned OSC 8
        // links and silently eat every Ctrl+Left a vim / lazygit user made.
        let shadow = shadow_with_link_uri("https://example.com/\u{202e}gpj.exe");
        assert_eq!(
            activation_target(&shadow, Some(at(0, 0)), ctrl(), true),
            None
        );
    }

    #[test]
    fn no_activation_while_another_bar_holds_the_preview_row() {
        // REQ-910 shows the preview *while the modifier is held*, so with
        // the row lost to a search bar, a confirmation, or an IME
        // composition there is nothing to activate: opening a target the
        // user was never shown is the spoofing the preview exists to stop.
        let shadow = shadow_with_link();
        assert!(activation_target(&shadow, Some(at(0, 0)), ctrl(), true).is_some());
        assert_eq!(
            activation_target(&shadow, Some(at(0, 0)), ctrl(), false),
            None
        );
        assert_eq!(
            icon_for(&shadow, Some(at(0, 0)), ctrl(), false),
            CursorIcon::Default
        );
    }

    #[test]
    fn only_messages_that_can_move_a_cell_or_the_link_table_recompute_the_hover() {
        use felis_protocol::messages::{PaletteAction, ThemeAction, ThemeChannel};

        assert!(grid_msg_moves_hover_target(&GridMsg::RowDelta {
            rows: Vec::new()
        }));
        assert!(grid_msg_moves_hover_target(&GridMsg::Hyperlink {
            id: 1,
            anchor: None,
            uri: "https://example.com".to_owned(),
        }));
        assert!(grid_msg_moves_hover_target(&GridMsg::PointerShape {
            name: None
        }));
        // A `cat` ships these in the same burst, so answering `true` here
        // would cost a parse allocation and a `set_cursor` per message.
        assert!(!grid_msg_moves_hover_target(&GridMsg::Title {
            value: "x".to_owned()
        }));
        assert!(!grid_msg_moves_hover_target(&GridMsg::PaletteColor {
            index: 1,
            action: PaletteAction::Reset,
        }));
        assert!(!grid_msg_moves_hover_target(&GridMsg::ThemeColor {
            channel: ThemeChannel::Foreground,
            action: ThemeAction::Reset,
        }));
    }

    #[test]
    fn pointer_icon_stays_default_on_ctrl_hover_over_plain_cell() {
        // Ctrl+Click would do nothing here, so the icon must not promise it.
        let shadow = shadow_with_link();
        assert_eq!(
            icon_for(&shadow, Some(at(0, 1)), ctrl(), true),
            CursorIcon::Default
        );
    }

    #[test]
    fn pointer_icon_stays_default_when_pointer_left_the_window() {
        // CursorLeft sets cursor_cell = None.
        let shadow = shadow_with_link();
        assert_eq!(icon_for(&shadow, None, ctrl(), true), CursorIcon::Default);
    }

    #[test]
    fn url_at_returns_none_when_cell_has_no_link() {
        let mut shadow = ShadowScreen::new(1, 5);
        let body = packed_row(
            &(0..5)
                .map(|_| felis_grid::Cell::default())
                .collect::<Vec<_>>(),
        );
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(body))],
            })
            .unwrap();
        assert_eq!(url_at(&shadow, at(0, 0)), None);
    }

    #[test]
    fn url_at_returns_uri_for_a_linked_cell() {
        let shadow = shadow_with_link();
        assert_eq!(url_at(&shadow, at(0, 0)), Some("https://example.com"));
        assert_eq!(url_at(&shadow, at(0, 1)), None);
    }

    /// Pins: a row naming an id the link table never received is
    /// refused, so no cell on screen can hold an unresolvable link.
    #[test]
    fn a_row_naming_an_unknown_link_id_never_reaches_a_cell() {
        let mut shadow = ShadowScreen::new(1, 5);
        let mut cells: Vec<_> = (0..5).map(|_| felis_grid::Cell::default()).collect();
        cells[0].link = NonZeroU16::new(99);
        let body = packed_row(&cells);
        shadow
            .apply(&GridMsg::RowDelta {
                rows: vec![(0, RowPayload(body))],
            })
            .expect_err("an unresolved link handle ends the attachment");
        assert_eq!(url_at(&shadow, at(0, 0)), None);
    }

    /// States that take the bottom row away (a search bar, confirmation,
    /// or IME composition) are entered and left with no pointer or modifier
    /// events, so a press checks what the last frame painted.
    #[test]
    fn activation_needs_the_target_the_last_frame_previewed() {
        let a = ActivationTarget::parse("https://example.com").unwrap();
        let b = ActivationTarget::parse("https://example.org").unwrap();
        assert!(activation_matches_preview(Some(&a), &a));
        assert!(
            !activation_matches_preview(None, &a),
            "a frame that previewed nothing arms nothing"
        );
        assert!(
            !activation_matches_preview(Some(&b), &a),
            "the previewed target is the only one the click may open"
        );
    }

    /// Asserts the terminator is present and is the only NUL.
    fn from_wide_z(buf: &[u16]) -> String {
        let (last, body) = buf.split_last().expect("buffer is never empty");
        assert_eq!(*last, 0, "buffer must be NUL-terminated");
        assert!(!body.contains(&0), "the terminator must be the only NUL");
        String::from_utf16(body).expect("round-trips through UTF-16")
    }

    /// Every character cmd.exe treats as syntax; all printable by a remote
    /// program inside an OSC 8 URI.
    const HOSTILE_URLS: &[&str] = &[
        "https://example.com/?a=1&b=2&calc.exe",
        "https://example.com/x|calc.exe",
        "https://example.com/x<in.txt",
        "https://example.com/x>out.txt",
        "https://example.com/x^&calc.exe",
        "https://example.com/x%COMSPEC%",
        r#"https://example.com/" & calc.exe & ""#,
        "https://example.com/a\nb",
    ];

    #[test]
    fn the_windows_handoff_carries_a_hostile_url_as_one_inert_file_argument() {
        // Pins that the URL becomes the whole `lpFile` and carries no command line
        // for cmd.exe to parse, preventing command injection.
        for url in HOSTILE_URLS {
            let Ok(target) = ActivationTarget::parse(url) else {
                // The one entry with a literal newline is a control
                // character: the activation boundary refuses it outright,
                // so it never reaches this hand-off to pin.
                assert_eq!(*url, "https://example.com/a\nb");
                continue;
            };
            let args = ShellExecuteArgs::for_url(&target).expect("a NUL-free URL encodes");
            assert_eq!(
                from_wide_z(&args.file),
                *url,
                "the URL must be lpFile, verbatim: no escaping, splitting, or dropping"
            );
            assert_eq!(
                from_wide_z(&args.verb),
                "open",
                "the operation must be the `open` verb, never a program to run"
            );
            assert!(
                args.parameters.is_none(),
                "a command line beside the URL is the shape that was removed"
            );
        }
    }

    #[test]
    fn the_newline_hostile_url_is_stopped_at_the_activation_boundary() {
        // The activation-time control-char check (`ActivationRejection`)
        // closes this vector before any launcher sees it, which is why
        // the hand-off test above skips it rather than pinning it.
        assert_eq!(
            ActivationTarget::parse("https://example.com/a\nb"),
            Err(ActivationRejection::ControlChar)
        );
    }

    #[test]
    fn wide_z_rejects_an_interior_nul() {
        // Defense in depth: `ActivationTarget::parse` already refuses a
        // NUL before `ShellExecuteArgs` ever sees one, but truncating at
        // the NUL here would still send the shell a different destination
        // than the cell the user clicked, so the low-level guard stays.
        let err = wide_z("https://example.com/\0evil").expect_err("interior NUL is rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn activation_target_parse_rejects_an_interior_nul_before_reaching_the_shell_handoff() {
        assert!(ActivationTarget::parse("https://example.com/\0evil").is_err());
    }

    #[test]
    fn the_windows_handoff_encodes_non_ascii_as_utf16() {
        // Surrogate pairs are two code units; a one-unit-per-char encoding
        // would corrupt an IDN or emoji URL.
        let target = ActivationTarget::parse("https://例え.jp/🐈").expect("non-ASCII URL parses");
        let args = ShellExecuteArgs::for_url(&target).expect("non-ASCII URL encodes");
        assert_eq!(from_wide_z(&args.file), "https://例え.jp/🐈");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn launch_argv_passes_the_target_as_one_verbatim_argument_to_the_platform_opener() {
        // Pins the Unix boundary the way `ShellExecuteArgs` pins Windows:
        // the target is the whole, single argument, never assembled into
        // a shell command line.
        let mut pinned = 0;
        for url in HOSTILE_URLS {
            let Ok(target) = ActivationTarget::parse(url) else {
                // Pinned rather than skipped silently: refusing `%` or `&`
                // in `ActivationTarget::parse` would otherwise empty this loop
                // and leave the test green while asserting nothing. The newline
                // entry is stopped upstream; see
                // `the_newline_hostile_url_is_stopped_at_the_activation_boundary`.
                assert_eq!(*url, "https://example.com/a\nb");
                continue;
            };
            let (program, arg) = launch_argv(&target);
            #[cfg(target_os = "linux")]
            assert_eq!(program, "xdg-open");
            #[cfg(target_os = "macos")]
            assert_eq!(program, "open");
            assert_eq!(arg, *url, "the target must be the whole argument, verbatim");
            pinned += 1;
        }
        assert_eq!(pinned, HOSTILE_URLS.len() - 1);
    }
}
