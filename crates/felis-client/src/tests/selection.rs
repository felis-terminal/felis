use felis_protocol::messages::MouseProtocol;

use super::*;

const LIVE: (u32, Option<ViewportAnchor>) = (0, None);

const fn browsing(offset: u32, pushed: u64) -> (u32, Option<ViewportAnchor>) {
    (
        offset,
        Some(ViewportAnchor {
            pushed,
            generation: 0,
        }),
    )
}

fn state((offset, anchor): (u32, Option<ViewportAnchor>)) -> GridMsg {
    GridMsg::ViewportState {
        lines_from_bottom: offset,
        max: 1_000,
        anchor,
    }
}

#[test]
fn output_growing_a_browsed_offset_keeps_the_selection() {
    assert!(!viewport_message_enters_scrollback(
        &state(browsing(7, 13)),
        browsing(5, 11),
    ));
}

#[test]
fn a_browsed_view_that_moves_off_its_lines_clears_the_selection() {
    assert!(viewport_message_enters_scrollback(
        &state(browsing(6, 11)),
        browsing(5, 11),
    ));
    assert!(viewport_message_enters_scrollback(
        &state(browsing(5, 13)),
        browsing(5, 11),
    ));
    let resized = (
        7,
        Some(ViewportAnchor {
            pushed: 13,
            generation: 1,
        }),
    );
    assert!(viewport_message_enters_scrollback(
        &state(resized),
        browsing(5, 11),
    ));
}

#[test]
fn viewport_state_into_scrollback_clears_selection() {
    // Pins: the highlight must not outlive the cells it was anchored to.
    assert!(viewport_message_enters_scrollback(
        &GridMsg::ViewportState {
            lines_from_bottom: 1,
            max: 100,
            anchor: None,
        },
        LIVE,
    ));
    assert!(viewport_message_enters_scrollback(
        &GridMsg::ViewportState {
            lines_from_bottom: 9_999,
            max: 10_024,
            anchor: None,
        },
        LIVE,
    ));
}

#[test]
fn leaving_a_browsed_view_for_live_clears_the_selection() {
    assert!(viewport_message_enters_scrollback(
        &state(LIVE),
        browsing(5, 11),
    ));
}

#[test]
fn viewport_state_at_bottom_keeps_selection() {
    assert!(!viewport_message_enters_scrollback(
        &GridMsg::ViewportState {
            lines_from_bottom: 0,
            max: 24,
            anchor: None,
        },
        LIVE,
    ));
}

#[test]
fn unrelated_grid_messages_never_request_a_selection_clear() {
    assert!(!viewport_message_enters_scrollback(
        &GridMsg::Attention {
            source: felis_protocol::messages::AttentionSource::Bell,
        },
        LIVE,
    ));
    assert!(!viewport_message_enters_scrollback(
        &GridMsg::CursorState {
            row: 0,
            col: 0,
            visible: true,
            style: felis_protocol::messages::CursorStyle::default(),
            blink: true,
        },
        LIVE,
    ));
}

#[test]
fn a_press_the_program_consumes_still_dismisses_the_selection() {
    // Pins: a forwarded press still counts as click-elsewhere, or the
    // highlight has no gesture left that can clear it.
    assert!(forwarded_press_dismisses_selection(MouseButton::Left));
    assert!(forwarded_press_dismisses_selection(MouseButton::Right));
    // Middle is paste PRIMARY, independent of selection state (xterm).
    assert!(!forwarded_press_dismisses_selection(MouseButton::Middle));
    assert!(!forwarded_press_dismisses_selection(MouseButton::Button8));
}

#[test]
fn entering_or_leaving_the_alt_screen_clears_the_selection() {
    // A `?1049` swap replaces every visible cell, in both directions.
    let msg = mode_flags(false, true, MouseProtocol::Off);
    assert!(grid_change_invalidates_selection(&msg, false, true, LIVE));
    assert!(grid_change_invalidates_selection(&msg, true, false, LIVE));
}

#[test]
fn a_mode_flip_that_leaves_the_screen_alone_keeps_the_selection() {
    // `ModeFlags` carries every bit at once, so a TUI enabling mouse
    // reporting after taking the alt screen re-sends the same
    // `alt_screen`; only the transition may clear.
    assert!(!grid_change_invalidates_selection(
        &mode_flags(false, true, MouseProtocol::AnyMotion),
        true,
        true,
        LIVE,
    ));
    assert!(!grid_change_invalidates_selection(
        &mode_flags(false, false, MouseProtocol::ButtonEvents),
        false,
        false,
        LIVE,
    ));
}

#[test]
fn a_scroll_into_scrollback_clears_the_selection_on_either_screen() {
    let scrolled = GridMsg::ViewportState {
        lines_from_bottom: 3,
        max: 100,
        anchor: None,
    };
    assert!(grid_change_invalidates_selection(
        &scrolled, false, false, LIVE
    ));
    assert!(grid_change_invalidates_selection(
        &scrolled, true, true, LIVE
    ));
    let at_bottom = GridMsg::ViewportState {
        lines_from_bottom: 0,
        max: 100,
        anchor: None,
    };
    assert!(!grid_change_invalidates_selection(
        &at_bottom, false, false, LIVE
    ));
}
