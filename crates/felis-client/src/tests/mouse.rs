use super::*;

mod click_streak {
    use super::*;

    #[test]
    fn click_streak_first_press_returns_one() {
        let mut s = ClickStreak::new();
        assert_eq!(s.record(Instant::now(), at(3, 5)), 1);
    }

    #[test]
    fn click_streak_cycles_one_two_three_one_when_all_in_window_same_cell() {
        let mut s = ClickStreak::new();
        let t0 = Instant::now();
        // The 4th press starts the cycle over (xterm / kitty / alacritty).
        assert_eq!(s.record(t0, at(1, 1)), 1);
        assert_eq!(s.record(t0 + Duration::from_millis(50), at(1, 1)), 2);
        assert_eq!(s.record(t0 + Duration::from_millis(100), at(1, 1)), 3);
        assert_eq!(s.record(t0 + Duration::from_millis(150), at(1, 1)), 1);
    }

    #[test]
    fn click_streak_resets_to_one_on_different_cell() {
        let mut s = ClickStreak::new();
        let t0 = Instant::now();
        assert_eq!(s.record(t0, at(1, 1)), 1);
        assert_eq!(s.record(t0 + Duration::from_millis(50), at(1, 1)), 2);
        assert_eq!(s.record(t0 + Duration::from_millis(60), at(5, 5)), 1);
    }

    #[test]
    fn click_streak_resets_to_one_after_window_expires() {
        let mut s = ClickStreak::new();
        let t0 = Instant::now();
        assert_eq!(s.record(t0, at(1, 1)), 1);
        assert_eq!(
            s.record(t0 + CLICK_INTERVAL + Duration::from_millis(1), at(1, 1)),
            1
        );
    }

    #[test]
    fn click_streak_reset_drops_state() {
        let mut s = ClickStreak::new();
        let t0 = Instant::now();
        assert_eq!(s.record(t0, at(1, 1)), 1);
        assert_eq!(s.record(t0 + Duration::from_millis(10), at(1, 1)), 2);
        s.reset();
        assert_eq!(s.record(t0 + Duration::from_millis(20), at(1, 1)), 1);
    }
}

mod mouse_input {
    use super::*;

    #[test]
    fn map_button_covers_left_right_middle_back_forward() {
        assert_eq!(map_button(WinitMouseButton::Left), Some(MouseButton::Left));
        assert_eq!(
            map_button(WinitMouseButton::Right),
            Some(MouseButton::Right)
        );
        assert_eq!(
            map_button(WinitMouseButton::Middle),
            Some(MouseButton::Middle)
        );
        assert_eq!(
            map_button(WinitMouseButton::Back),
            Some(MouseButton::Button8)
        );
        assert_eq!(
            map_button(WinitMouseButton::Forward),
            Some(MouseButton::Button9)
        );
    }

    #[test]
    fn map_button_only_passes_extra_buttons_8_through_11() {
        // xterm encodes 8..=11 and nothing else.
        assert_eq!(
            map_button(WinitMouseButton::Other(8)),
            Some(MouseButton::Button8)
        );
        assert_eq!(
            map_button(WinitMouseButton::Other(11)),
            Some(MouseButton::Button11)
        );
        assert_eq!(map_button(WinitMouseButton::Other(0)), None);
        assert_eq!(map_button(WinitMouseButton::Other(12)), None);
    }

    #[test]
    fn wheel_encoding_routes_by_grid_state() {
        assert_eq!(
            wheel_encoding_for(true, true),
            WheelEncoding::MouseButtons,
            "mouse-mode wins regardless of alt screen",
        );
        assert_eq!(wheel_encoding_for(true, false), WheelEncoding::MouseButtons);
        assert_eq!(
            wheel_encoding_for(false, true),
            WheelEncoding::ArrowKeys,
            "alt-screen pager UX wins when mouse-mode off",
        );
        assert_eq!(
            wheel_encoding_for(false, false),
            WheelEncoding::Scroll,
            "primary + no-mouse drives the scrollback viewport",
        );
    }

    /// Up and down are the CSI arrows whatever DECCKM says, and with
    /// Shift held Page Up / Page Down, the bytes `encode_named` sends
    /// for the keys themselves, so wheel and key never differ.
    #[test]
    fn wheel_arrow_bytes_encodes_each_delta_and_shift_state() {
        let cases: &[(&str, WheelDelta, bool, &[u8])] = &[
            (
                "one arrow per line tick up",
                WheelDelta::Lines(0.0, 3.0),
                false,
                b"\x1b[A\x1b[A\x1b[A",
            ),
            (
                "one arrow per line tick down",
                WheelDelta::Lines(0.0, -2.0),
                false,
                b"\x1b[B\x1b[B",
            ),
            // The way `wheel_buttons` counts a notch.
            (
                "a pixel delta down collapses to one arrow",
                WheelDelta::Pixels(0.0, -42.0),
                false,
                b"\x1b[B",
            ),
            (
                "a pixel delta up collapses to one arrow",
                WheelDelta::Pixels(0.0, 7.0),
                false,
                b"\x1b[A",
            ),
            // A pure horizontal scroll over a pager must not eat the wheel.
            (
                "a zero line delta is silent",
                WheelDelta::Lines(0.0, 0.0),
                false,
                b"",
            ),
            (
                "a horizontal-only pixel delta is silent",
                WheelDelta::Pixels(50.0, 0.0),
                false,
                b"",
            ),
            // No pager interprets `ESC [ C` / `ESC [ D` as scroll.
            (
                "the horizontal axis does not leak",
                WheelDelta::Lines(2.0, 1.0),
                false,
                b"\x1b[A",
            ),
            (
                "shift turns up into PageUp",
                WheelDelta::Lines(0.0, 1.0),
                true,
                b"\x1b[5~",
            ),
            (
                "shift turns down into PageDown",
                WheelDelta::Lines(0.0, -1.0),
                true,
                b"\x1b[6~",
            ),
            (
                "shift keeps one key per line tick",
                WheelDelta::Lines(0.0, 3.0),
                true,
                b"\x1b[5~\x1b[5~\x1b[5~",
            ),
            (
                "shift keeps a pixel delta at one key",
                WheelDelta::Pixels(0.0, -42.0),
                true,
                b"\x1b[6~",
            ),
        ];
        for (label, delta, shift_held, want) in cases {
            assert_eq!(wheel_arrow_bytes(*delta, *shift_held), *want, "{label}");
        }
    }

    #[test]
    fn wheel_zoom_delta_px_matches_browser_one_notch_one_pixel() {
        // Browser / kitty / alacritty convention: one notch = +-1 px.
        assert!((wheel_zoom_delta_px(WheelDelta::Lines(0.0, 1.0)) - 1.0).abs() < f32::EPSILON);
        assert!((wheel_zoom_delta_px(WheelDelta::Lines(0.0, -1.0)) + 1.0).abs() < f32::EPSILON);
        assert!((wheel_zoom_delta_px(WheelDelta::Lines(0.0, 3.0)) - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn wheel_stream_first_event_opens_a_stream() {
        assert!(wheel_starts_new_stream(None));
    }

    #[test]
    fn wheel_stream_inertia_inherits_the_opening_decision() {
        // Trackpad inertia arrives back-to-back with its gesture;
        // re-arming the latch there would let a Ctrl press during
        // momentum reclassify a scroll as a zoom.
        assert!(!wheel_starts_new_stream(Some(
            WHEEL_STREAM_IDLE.saturating_sub(Duration::from_millis(1))
        )));
        assert!(!wheel_starts_new_stream(Some(Duration::ZERO)));
    }

    #[test]
    fn wheel_stream_human_pause_opens_a_new_stream() {
        assert!(wheel_starts_new_stream(Some(WHEEL_STREAM_IDLE)));
        assert!(wheel_starts_new_stream(Some(
            WHEEL_STREAM_IDLE + Duration::from_millis(50)
        )));
    }

    #[test]
    fn wheel_zoom_delta_px_zero_motion_produces_no_zoom() {
        assert!(wheel_zoom_delta_px(WheelDelta::Lines(0.0, 0.0)).abs() < f32::EPSILON);
        assert!(wheel_zoom_delta_px(WheelDelta::Pixels(50.0, 0.0)).abs() < f32::EPSILON);
    }

    proptest::proptest! {
    // Bounded: winit never emits NaN / Inf, which would make the round()
    // rule meaningless.
    #[test]
    fn wheel_y_ticks_total_for_arbitrary_finite_line_delta(
        x in proptest::num::f32::NORMAL,
        y in -1024.0_f32..=1024.0_f32,
    ) {
        let ticks = wheel_y_ticks(WheelDelta::Lines(x, y));
        if y > 0.0 {
            proptest::prop_assert!(ticks > 0, "y={} should produce up-ticks, got {}", y, ticks);
        } else if y < 0.0 {
            proptest::prop_assert!(ticks < 0, "y={} should produce down-ticks, got {}", y, ticks);
        } else {
            proptest::prop_assert_eq!(ticks, 0, "zero y must drop horizontal motion");
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected_mag = f64::from(y).abs().round().max(if y == 0.0 { 0.0 } else { 1.0 }) as u32;
        proptest::prop_assert_eq!(ticks.unsigned_abs(), expected_mag);
    }

    #[test]
    fn wheel_y_ticks_pixel_delta_collapses_to_signum(
        px in -10_000.0_f64..=10_000.0_f64,
        py in -10_000.0_f64..=10_000.0_f64,
    ) {
        let ticks = wheel_y_ticks(WheelDelta::Pixels(px, py));
        let expected = if py > 0.0 { 1 } else if py < 0.0 { -1 } else { 0 };
        proptest::prop_assert_eq!(ticks, expected, "px={} py={}", px, py);
    }

    #[test]
    fn wheel_arrow_bytes_length_matches_tick_count_times_seq_len(
        y in -1024.0_f32..=1024.0_f32,
        shift_held in proptest::bool::ANY,
    ) {
        let bytes = wheel_arrow_bytes(WheelDelta::Lines(0.0, y), shift_held);
        let ticks = wheel_y_ticks(WheelDelta::Lines(0.0, y));
        let seq_len: usize = match (ticks > 0, shift_held) {
            (_, false) => 3,                 // ESC [ A   or   ESC [ B
            (_, true) => 4,                  // ESC [ 5 ~ or   ESC [ 6 ~
        };
        let expected_len = if ticks == 0 { 0 } else { ticks.unsigned_abs() as usize * seq_len };
        proptest::prop_assert_eq!(bytes.len(), expected_len);
    }

    // The multiplier is deliberately far from 1: a pixel delta that
    // honoured it would break conservation against the cell height.
    #[test]
    fn wheel_pixels_to_rows_conserves_every_pixel_of_a_gesture(
        cell_h in proptest::prop_oneof![proptest::strategy::Just(0.0_f64), 1.0_f64..=200.0_f64],
        deltas in proptest::collection::vec(-5_000.0_f64..=5_000.0_f64, 1..16),
    ) {
        let effective = cell_h.max(1.0);
        let mut accum = 0.0_f64;
        let mut rows_total = 0_i64;
        let mut pixels_total = 0.0_f64;
        for dy in deltas {
            let pending = accum + dy;
            let rows = wheel_pixels_to_rows(
                WheelDelta::Pixels(0.0, dy),
                cell_h,
                17.0,
                &mut accum,
            );
            rows_total += i64::from(rows);
            pixels_total += dy;
            proptest::prop_assert!(
                accum.abs() < effective + 1e-9,
                "residue {} holds a whole {}-px row",
                accum,
                effective,
            );
            proptest::prop_assert!(
                accum * pending >= 0.0,
                "residue {} overshot past {} pending px",
                accum,
                pending,
            );
        }
        #[allow(clippy::cast_precision_loss)]
        let reconstructed = (rows_total as f64).mul_add(effective, accum);
        let tolerance = 1e-6 * pixels_total.abs().max(1.0);
        proptest::prop_assert!(
            (reconstructed - pixels_total).abs() <= tolerance,
            "{} rows of {} px plus {} residue != {} px scrolled",
            rows_total,
            effective,
            accum,
            pixels_total,
        );
    }

    #[test]
    fn wheel_pixels_to_rows_line_delta_scales_and_drops_the_residue(
        cell_h in 1.0_f64..=200.0_f64,
        residue in -500.0_f64..=500.0_f64,
        y in -1024.0_f32..=1024.0_f32,
        multiplier in 0.1_f64..=10.0_f64,
    ) {
        let mut accum = residue;
        let rows = wheel_pixels_to_rows(
            WheelDelta::Lines(0.0, y),
            cell_h,
            multiplier,
            &mut accum,
        );
        #[allow(clippy::float_cmp)]
        {
            proptest::prop_assert_eq!(accum, 0.0, "a notch must discard trackpad residue");
        }
        let scaled = f64::from(y) * multiplier;
        proptest::prop_assert!(
            (f64::from(rows) - scaled).abs() <= 0.5,
            "{} rows is not {} lines rounded",
            rows,
            scaled,
        );
    }

    /// The pager and mouse-mode encodings must count one notch alike:
    /// `wheel_y_ticks` is the oracle for the vertical axis, and applying
    /// it to a transposed delta is the oracle for the horizontal one.
    #[test]
    fn wheel_buttons_counts_the_same_notches_as_wheel_y_ticks(
        x in -1024.0_f32..=1024.0_f32,
        y in -1024.0_f32..=1024.0_f32,
        pixel_delta in proptest::bool::ANY,
    ) {
        let (delta, transposed) = if pixel_delta {
            (
                WheelDelta::Pixels(f64::from(x), f64::from(y)),
                WheelDelta::Pixels(f64::from(y), f64::from(x)),
            )
        } else {
            (
                WheelDelta::Lines(x, y),
                WheelDelta::Lines(y, x),
            )
        };
        let y_ticks = wheel_y_ticks(delta);
        let x_ticks = wheel_y_ticks(transposed);
        let buttons = wheel_buttons(delta);

        let vertical = if y_ticks > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
        let horizontal = if x_ticks > 0 { MouseButton::WheelRight } else { MouseButton::WheelLeft };
        let mut expected = vec![vertical; y_ticks.unsigned_abs() as usize];
        expected.extend(std::iter::repeat_n(horizontal, x_ticks.unsigned_abs() as usize));
        proptest::prop_assert_eq!(buttons, expected);
    }

        }
}

mod mouse_position {
    use crate::app_methods::cell_for_position;

    use super::*;

    /// The winit adapter hands `x` to the column and `y` to the row, and
    /// the grid it clamps against is `dims`, not the cell size.
    #[test]
    fn cell_for_position_maps_x_to_col_and_y_to_row() {
        let dims = felis_protocol::messages::GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        };
        let pointer = PhysicalPosition::new(135.0, 61.0);
        assert_eq!(
            cell_for_position(pointer, (100.0, 0.0), [10, 20], dims),
            at(3, 3)
        );
        assert_eq!(
            cell_for_position(PhysicalPosition::new(1e6, 1e6), (0.0, 0.0), [10, 20], dims),
            at(23, 79)
        );
    }
}

#[test]
fn should_start_selection_xterm_modifier_table() {
    assert!(should_start_selection(false, false));
    assert!(should_start_selection(false, true));
    assert!(!should_start_selection(true, false));
    assert!(should_start_selection(true, true));
}

#[test]
fn held_button_release_reports_the_routing_its_press_took() {
    // A press forwarded to the program owes it a release; a press
    // grabbed for a selection owes it nothing.
    let mut held = HeldButtons::default();
    held.press(MouseButton::Left, PressRouting::Forwarded);
    held.press(MouseButton::Right, PressRouting::Grabbed);

    assert_eq!(
        held.release(MouseButton::Left),
        Some(PressRouting::Forwarded)
    );
    assert_eq!(
        held.release(MouseButton::Right),
        Some(PressRouting::Grabbed)
    );
    assert!(held.is_empty());
}

#[test]
fn held_button_release_without_a_press_is_unroutable() {
    // Presses the client swallows whole (Ctrl+Left hyperlink open,
    // Middle paste) are never recorded, so their release has no routing.
    let mut held = HeldButtons::default();
    assert_eq!(held.release(MouseButton::Middle), None);

    held.press(MouseButton::Left, PressRouting::Forwarded);
    assert!(held.release(MouseButton::Left).is_some());
    assert_eq!(held.release(MouseButton::Left), None);
}

#[test]
fn held_button_repeat_press_replaces_a_missed_release() {
    // A second press with no release means the release was missed (the
    // pointer left the window mid-hold); the stale routing must not
    // misroute the new press's release.
    let mut held = HeldButtons::default();
    held.press(MouseButton::Left, PressRouting::Grabbed);
    held.press(MouseButton::Left, PressRouting::Forwarded);
    assert_eq!(
        held.release(MouseButton::Left),
        Some(PressRouting::Forwarded)
    );
    assert!(held.is_empty());
}

#[test]
fn held_buttons_reports_the_newest_button_for_drag_attribution() {
    let mut held = HeldButtons::default();
    assert_eq!(held.last(), None);
    assert!(held.is_empty());

    held.press(MouseButton::Left, PressRouting::Forwarded);
    held.press(MouseButton::Right, PressRouting::Forwarded);
    assert_eq!(held.last(), Some(MouseButton::Right));
    assert!(held.is_held(MouseButton::Left));

    held.release(MouseButton::Right);
    assert_eq!(held.last(), Some(MouseButton::Left));
    assert!(!held.is_held(MouseButton::Right));

    held.press(MouseButton::Right, PressRouting::Forwarded);
    held.press(MouseButton::Left, PressRouting::Forwarded);
    assert_eq!(
        held.last(),
        Some(MouseButton::Left),
        "a repeat press becomes the newest"
    );
    held.release(MouseButton::Left);
    assert_eq!(held.last(), Some(MouseButton::Right));
}
