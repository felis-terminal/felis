use felis_client_core::config::MAX_FONT_SIZE_LOGICAL_PX;

use super::*;

const fn at(row: u16, col: u16) -> GridPos {
    GridPos { row, col }
}

mod cli {
    use super::*;

    /// Pins: a clap upgrade must not silently drop the flag.
    #[test]
    fn cli_parses_trace_perf_flag() {
        let cli = Cli::try_parse_from(["felis", "--trace-perf"]).expect("parses with --trace-perf");
        assert!(cli.trace_perf);
        let default = Cli::try_parse_from(["felis"]).expect("parses bare felis");
        assert!(!default.trace_perf);
    }

    /// Pins: args after the program (including a `-c` string with
    /// spaces) survive as separate argv entries, never re-split or
    /// shell-evaluated.
    #[test]
    fn cli_captures_trailing_command_after_double_dash() {
        let cli = Cli::try_parse_from(["felis", "--", "htop"]).expect("parses felis -- htop");
        assert!(cli.cmd.is_none());
        assert_eq!(cli.command, vec!["htop"]);

        let cli = Cli::try_parse_from(["felis", "--", "bash", "-c", "ls; exec bash"])
            .expect("parses felis -- bash -c '...'");
        assert_eq!(cli.command, vec!["bash", "-c", "ls; exec bash"]);

        let bare = Cli::try_parse_from(["felis"]).expect("parses bare felis");
        assert_eq!(bare.command, Vec::<String>::new());
    }

    #[test]
    fn cli_flags_before_double_dash_do_not_leak_into_command() {
        let cli = Cli::try_parse_from(["felis", "--socket", "/tmp/s.sock", "--", "ls"])
            .expect("parses felis --socket P -- ls");
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/tmp/s.sock"))
        );
        assert_eq!(cli.command, vec!["ls"]);
    }

    /// Pins: an unknown bare token is a hard error, not a command to
    /// exec; the front-door `felis` answers a typo the same way.
    #[test]
    fn cli_bare_token_without_double_dash_is_a_subcommand_not_a_command() {
        assert!(Cli::try_parse_from(["felis-client", "definitely-not-a-subcommand"]).is_err());
        let cli = Cli::try_parse_from(["felis-client", "attach", "1a2b"]).expect("parses attach");
        assert!(matches!(cli.cmd, Some(ClientCmd::Attach { .. })));
        assert_eq!(cli.command, Vec::<String>::new());
    }
}

#[test]
fn perf_trace_directive_includes_hot_path_targets() {
    let perf = log_filter_directive(true);
    assert!(perf.contains("felis::redraw=trace"), "{perf}");
    assert!(perf.contains("felis::pull=trace"), "{perf}");
    let normal = log_filter_directive(false);
    assert!(
        !normal.contains("=trace"),
        "default must not enable trace: {normal}"
    );
    assert!(normal.contains("felis=debug"));
}

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

mod wheel {
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
        // Pins the precedence: mouse-mode over alt-screen over the
        // primary-screen scroll fallback.
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

    #[test]
    fn wheel_arrow_bytes_line_delta_emits_csi_arrow_per_tick() {
        let three_up = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, 3.0), false);
        assert_eq!(three_up, b"\x1b[A\x1b[A\x1b[A".to_vec());
        let two_down = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, -2.0), false);
        assert_eq!(two_down, b"\x1b[B\x1b[B".to_vec());
    }

    #[test]
    fn wheel_arrow_bytes_pixel_delta_collapses_to_one_arrow() {
        // Pins: per-pixel deltas collapse to one arrow, the way
        // `wheel_buttons` counts a notch.
        let down = wheel_arrow_bytes(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -42.0)),
            false,
        );
        assert_eq!(down, b"\x1b[B".to_vec());
        let up = wheel_arrow_bytes(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 7.0)),
            false,
        );
        assert_eq!(up, b"\x1b[A".to_vec());
    }

    #[test]
    fn wheel_arrow_bytes_zero_delta_is_silent() {
        // A pure horizontal scroll over a pager must not eat the wheel.
        assert_eq!(
            wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, 0.0), false),
            Vec::<u8>::new()
        );
        assert_eq!(
            wheel_arrow_bytes(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(50.0, 0.0)),
                false
            ),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn wheel_arrow_bytes_ignores_horizontal_axis() {
        // No pager interprets `ESC [ C` / `ESC [ D` as scroll.
        let diag = wheel_arrow_bytes(MouseScrollDelta::LineDelta(2.0, 1.0), false);
        assert_eq!(diag, b"\x1b[A".to_vec(), "horizontal axis must not leak");
    }

    #[test]
    fn wheel_arrow_bytes_uses_csi_form_not_app_cursor_form() {
        // Pins: `ESC [ A` regardless of DECCKM, like `encode_named`, so
        // wheel-up and ArrowUp never differ.
        let one = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, 1.0), false);
        assert_eq!(one, b"\x1b[A".to_vec());
        assert_ne!(one, b"\x1bOA".to_vec());
    }

    #[test]
    fn wheel_arrow_bytes_shift_held_emits_pageup_pagedown() {
        // Matches `encode_named`'s PageUp / PageDown bytes.
        let pgup = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, 1.0), true);
        assert_eq!(pgup, b"\x1b[5~".to_vec());
        let pgdn = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, -1.0), true);
        assert_eq!(pgdn, b"\x1b[6~".to_vec());
    }

    #[test]
    fn wheel_arrow_bytes_shift_held_count_matches_unshifted() {
        let three_pgup = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, 3.0), true);
        assert_eq!(three_pgup, b"\x1b[5~\x1b[5~\x1b[5~".to_vec());
    }

    #[test]
    fn wheel_arrow_bytes_shift_held_pixel_delta_still_collapses_to_one() {
        let down = wheel_arrow_bytes(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -42.0)),
            true,
        );
        assert_eq!(down, b"\x1b[6~".to_vec());
    }

    #[test]
    fn clamped_scroll_multiplier_keeps_the_wheel_alive() {
        use felis_client_core::config::MouseConfig;
        // Direction is the delta's sign, not the multiplier's.
        let zero = MouseConfig {
            scroll_multiplier: 0.0,
        };
        assert!(zero.clamped_scroll_multiplier() > 0.0);
        let negative = MouseConfig {
            scroll_multiplier: -5.0,
        };
        assert!(negative.clamped_scroll_multiplier() > 0.0);
        let sane = MouseConfig {
            scroll_multiplier: 3.0,
        };
        assert!((sane.clamped_scroll_multiplier() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn wheel_zoom_delta_px_matches_browser_one_notch_one_pixel() {
        // Browser / kitty / alacritty convention: one notch = +-1 px.
        assert!(
            (wheel_zoom_delta_px(MouseScrollDelta::LineDelta(0.0, 1.0)) - 1.0).abs() < f32::EPSILON
        );
        assert!(
            (wheel_zoom_delta_px(MouseScrollDelta::LineDelta(0.0, -1.0)) + 1.0).abs()
                < f32::EPSILON
        );
        assert!(
            (wheel_zoom_delta_px(MouseScrollDelta::LineDelta(0.0, 3.0)) - 3.0).abs() < f32::EPSILON
        );
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
        assert!(wheel_zoom_delta_px(MouseScrollDelta::LineDelta(0.0, 0.0)).abs() < f32::EPSILON);
        assert!(
            wheel_zoom_delta_px(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                50.0, 0.0
            )))
            .abs()
                < f32::EPSILON
        );
    }

    proptest::proptest! {
    // Bounded: winit never emits NaN / Inf, which would make the round()
    // rule meaningless.
    #[test]
    fn wheel_y_ticks_total_for_arbitrary_finite_line_delta(
        x in proptest::num::f32::NORMAL,
        y in -1024.0_f32..=1024.0_f32,
    ) {
        let ticks = wheel_y_ticks(MouseScrollDelta::LineDelta(x, y));
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
        let ticks = wheel_y_ticks(MouseScrollDelta::PixelDelta(
            PhysicalPosition::new(px, py),
        ));
        let expected = if py > 0.0 { 1 } else if py < 0.0 { -1 } else { 0 };
        proptest::prop_assert_eq!(ticks, expected, "px={} py={}", px, py);
    }

    #[test]
    fn wheel_arrow_bytes_length_matches_tick_count_times_seq_len(
        y in -1024.0_f32..=1024.0_f32,
        shift_held in proptest::bool::ANY,
    ) {
        let bytes = wheel_arrow_bytes(MouseScrollDelta::LineDelta(0.0, y), shift_held);
        let ticks = wheel_y_ticks(MouseScrollDelta::LineDelta(0.0, y));
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
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, dy)),
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
            MouseScrollDelta::LineDelta(0.0, y),
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
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(f64::from(x), f64::from(y))),
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(f64::from(y), f64::from(x))),
            )
        } else {
            (
                MouseScrollDelta::LineDelta(x, y),
                MouseScrollDelta::LineDelta(y, x),
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

#[test]
fn winit_state_reaches_the_wire_through_one_conversion() {
    // The only winit -> wire path: `winit_keys::mods`, then `Modifiers::to_input_mods`.
    use felis_protocol::messages::InputMods;

    let mut state = ModifiersState::empty();
    state |= ModifiersState::SHIFT;
    state |= ModifiersState::CONTROL;
    let mods = winit_keys::mods(state).to_input_mods();
    assert!(mods.contains(InputMods::SHIFT));
    assert!(mods.contains(InputMods::CTRL));
    assert!(!mods.contains(InputMods::ALT));
}

#[test]
fn ime_cursor_area_anchors_at_cell_origin() {
    let (x, y, w, h) = ime_cursor_area_pixels(0, 0, 8, 16);
    assert_eq!((x, y, w, h), (0, 0, 8, 16));
}

#[test]
fn ime_cursor_area_scales_by_cell_metrics() {
    // Platform IMEs lean on this; a one-pixel drift puts the popup over
    // the wrong character.
    let (x, y, w, h) = ime_cursor_area_pixels(10, 3, 8, 16);
    assert_eq!((x, y, w, h), (80, 48, 8, 16));
}

#[test]
fn ime_cursor_area_handles_large_grid_dimensions() {
    let (x, _, _, _) = ime_cursor_area_pixels(1000, 0, 8, 16);
    assert_eq!(x, 8000);
}
#[test]
fn search_ui_starts_off() {
    let ui = SearchUi::default();
    assert_eq!(ui.mode, SearchUiMode::Off);
    assert_eq!(ui.query, "");
    assert!(ui.matches.is_empty());
    assert!(ui.current.is_none());
    assert!(!ui.streaming);
    assert!(ui.error.is_none());
}
#[test]
fn format_search_label_renders_each_search_state() {
    let active = |query: &str| SearchUi {
        mode: SearchUiMode::Active,
        query: query.into(),
        ..SearchUi::default()
    };
    let cases = [
        (
            "composing appends cursor marker",
            SearchUi {
                mode: SearchUiMode::Composing,
                query: "foo".into(),
                ..SearchUi::default()
            },
            "find: foo_",
        ),
        (
            "active streaming shows ellipsis count",
            SearchUi {
                streaming: true,
                matches: vec![dummy_hit(-1); 3],
                ..active("foo")
            },
            "find: foo  (3…)",
        ),
        (
            "active with no matches says so",
            active("foo"),
            "find: foo  no matches",
        ),
        (
            "active with current shows position",
            SearchUi {
                matches: vec![dummy_hit(-1), dummy_hit(-3), dummy_hit(-5)],
                current: Some(1),
                ..active("foo")
            },
            "find: foo  2/3",
        ),
        // A daemon-side error wins over a partial stream's count.
        (
            "active error overrides count",
            SearchUi {
                error: Some("invalid regex".into()),
                ..active("(")
            },
            "find: (  error: invalid regex",
        ),
    ];
    for (case, ui, expected) in cases {
        assert_eq!(format_search_label_for(&ui), expected, "{case}");
    }
}

/// The boundary of the window's paste admission (REQ-105a), which every
/// producer of an `InputMsg::Paste` shares: the clipboard, a file drop,
/// and the pipe chord's `paste` sink, whose region the daemon admits up
/// to twice `MAX_PASTE_BYTES`, so it is the one that can reach the cap
/// without a human holding a 16 MiB clipboard.
#[test]
fn a_paste_at_the_limit_is_admitted_and_one_past_it_names_its_size() {
    use felis_protocol::messages::MAX_PASTE_BYTES;

    assert_eq!(app_methods::paste_refusal(MAX_PASTE_BYTES), None);
    let notice =
        app_methods::paste_refusal(MAX_PASTE_BYTES + 1).expect("one byte past the cap is refused");
    assert!(
        notice.contains(&(MAX_PASTE_BYTES + 1).to_string())
            && notice.contains(&MAX_PASTE_BYTES.to_string()),
        "the notice names both numbers, got {notice}",
    );
}

/// Pins: a query the client refuses to send leaves no stream behind and
/// says why. `commit_search_query` runs this before it opens a stream, so
/// a bar in this state means the daemon was never told about a stream id
/// the client had already burned, a mismatch it answers by dropping
/// the connection.
#[test]
fn refused_search_reports_the_limit_and_holds_no_stream() {
    use felis_protocol::codec::WireCodec;
    use felis_protocol::messages::{MAX_SEARCH_PATTERN_BYTES, SearchOptions, SearchToDaemonMsg};

    let query = "x".repeat(MAX_SEARCH_PATTERN_BYTES + 1);
    let err = SearchToDaemonMsg::Query {
        query: query.clone(),
        options: SearchOptions::default(),
    }
    .validate()
    .expect_err("a query over the pattern limit is refused before the wire");

    let mut ui = SearchUi {
        mode: SearchUiMode::Composing,
        query,
        streaming: true,
        matches: vec![dummy_hit(-1)],
        current: Some(0),
        stream: None,
        error: None,
    };
    app_methods::refuse_search(&mut ui, err.to_string());

    assert_eq!(ui.mode, SearchUiMode::Active);
    assert!(!ui.streaming);
    assert!(ui.stream.is_none());
    assert!(ui.matches.is_empty());
    assert!(ui.current.is_none());
    assert!(
        format_search_label_for(&ui).contains("error: wire field Search::Query.query"),
        "the bar names the refused field, not just an empty result"
    );
}

fn dummy_hit(line_index: i64) -> SearchHitRecord {
    SearchHitRecord {
        line_index,
        col_spans: vec![ColSpan {
            line_index,
            col_start: 0,
            col_end: 1,
        }],
    }
}

use crate::app_methods::format_search_label as format_search_label_for;

mod search_navigation {
    use crate::app_methods::{next_search_index, search_hit_anchor_line, visible_hit_spans};

    use super::*;

    /// The soft-wrap-stitched shape `advance_search_hit` anchors on: one
    /// segment per row, starting `first_seg_offset` rows below the
    /// logical line's top.
    fn stitched_hit(line_index: i64, first_seg_offset: i64, segments: u8) -> SearchHitRecord {
        SearchHitRecord {
            line_index,
            col_spans: (0..i64::from(segments))
                .map(|i| ColSpan {
                    line_index: line_index + first_seg_offset + i,
                    col_start: 2,
                    col_end: 6,
                })
                .collect(),
        }
    }

    proptest::proptest! {
        /// `n` and `N` are inverse steps of one cycle over the whole hit
        /// list, and `matches` is newest-first, so a first `n` opens at
        /// the youngest hit and a first `N` at the oldest.
        #[test]
        fn next_search_index_walks_one_cycle_in_both_directions(
            len in 1_usize..=64,
            offset in 0_usize..=64,
        ) {
            let start = offset % len;
            proptest::prop_assert_eq!(next_search_index(None, len, SearchDirection::Older), Some(0));
            proptest::prop_assert_eq!(
                next_search_index(None, len, SearchDirection::Newer),
                Some(len - 1),
            );
            for (forward, back) in [
                (SearchDirection::Older, SearchDirection::Newer),
                (SearchDirection::Newer, SearchDirection::Older),
            ] {
                let mut visited = Vec::with_capacity(len);
                let mut current = Some(start);
                for _ in 0..len {
                    let next = next_search_index(current, len, forward);
                    proptest::prop_assert_eq!(next_search_index(next, len, back), current);
                    current = next;
                    visited.push(next.expect("a non-empty list always has a next hit"));
                }
                proptest::prop_assert_eq!(current, Some(start), "the walk must close the cycle");
                visited.sort_unstable();
                visited.dedup();
                proptest::prop_assert_eq!(visited.len(), len, "every hit must be reachable once");
            }
        }

        #[test]
        fn next_search_index_has_nothing_to_pick_without_matches(
            current in proptest::option::of(0_usize..=64),
            newer in proptest::bool::ANY,
        ) {
            let direction = if newer { SearchDirection::Newer } else { SearchDirection::Older };
            proptest::prop_assert_eq!(next_search_index(current, 0, direction), None);
        }
    }

    /// Pins: the jump centers on the first painted segment; the logical
    /// line's top may be a row where nothing is highlighted.
    #[test]
    fn search_hit_anchor_line_prefers_the_first_painted_segment() {
        assert_eq!(search_hit_anchor_line(&stitched_hit(-40, 3, 2)), -37);
        assert_eq!(
            search_hit_anchor_line(&SearchHitRecord {
                line_index: -40,
                col_spans: Vec::new(),
            }),
            -40
        );
    }

    #[test]
    fn visible_hit_spans_project_each_segment_and_tag_the_current_hit() {
        let matches = [stitched_hit(-5, 0, 2), stitched_hit(-2, 0, 1)];
        // `composed_row = line_index + K`.
        let spans = visible_hit_spans(&matches, Some(1), 5, 24);
        assert_eq!(spans.len(), 3);
        assert_eq!(
            spans.iter().map(|s| s.row).collect::<Vec<_>>(),
            vec![0, 1, 3]
        );
        assert_eq!(
            spans.iter().map(|s| s.is_current).collect::<Vec<_>>(),
            vec![false, false, true],
        );
        assert_eq!((spans[0].col_start, spans[0].col_end), (2, 6));
        let none_current = visible_hit_spans(&matches, None, 5, 24);
        assert!(none_current.iter().all(|s| !s.is_current));
    }

    /// Pins: off-screen segments are dropped, not clamped; a clamped row
    /// would paint a highlight over unrelated text.
    #[test]
    fn visible_hit_spans_drop_offscreen_segments_and_keep_the_visible_ones() {
        let spans = visible_hit_spans(&[stitched_hit(-6, 0, 2)], Some(0), 5, 24);
        assert_eq!(spans.len(), 1, "only the on-screen segment paints");
        assert_eq!(spans[0].row, 0);
        assert_eq!(
            visible_hit_spans(&[stitched_hit(-100, 0, 1)], Some(0), 5, 24),
            Vec::<SearchHitSpan>::new()
        );
    }

    /// Pins: an empty column range never reaches the renderer as a
    /// zero-width quad.
    #[test]
    fn visible_hit_spans_skip_empty_column_ranges() {
        let empty = SearchHitRecord {
            line_index: -1,
            col_spans: vec![
                ColSpan {
                    line_index: -1,
                    col_start: 4,
                    col_end: 4,
                },
                ColSpan {
                    line_index: -1,
                    col_start: 6,
                    col_end: 5,
                },
            ],
        };
        assert_eq!(
            visible_hit_spans(&[empty], Some(0), 5, 24),
            Vec::<SearchHitSpan>::new()
        );
    }
}

mod mouse_position {
    use crate::app_methods::{cell_for_position, pixel_for_position};

    use super::*;

    const DIMS: felis_protocol::messages::GridDims = felis_protocol::messages::GridDims {
        rows: 24,
        cols: 80,
        pixel_w: 0,
        pixel_h: 0,
    };

    fn pos(x: f64, y: f64) -> PhysicalPosition<f64> {
        PhysicalPosition::new(x, y)
    }

    /// Why `reflow` re-derives the hovered cell: font zooms or DPI changes
    /// move the grid under a motionless pointer without winit motion events.
    /// Caching the cell from `CursorMoved` would target the wrong glyph or hyperlink.
    #[test]
    fn a_metrics_change_moves_the_cell_under_a_motionless_pointer() {
        let pointer = pos(35.0, 61.0);
        assert_eq!(
            cell_for_position(pointer, (0.0, 0.0), 10, 20, DIMS),
            at(3, 3)
        );
        assert_eq!(
            cell_for_position(pointer, (0.0, 0.0), 20, 40, DIMS),
            at(1, 1),
            "a zoom to double-size cells puts a different cell under the same pixel"
        );
    }

    proptest::proptest! {
        /// Every pixel of a cell names that cell once the letterbox
        /// origin comes off, and a zero-sized metric floors to one pixel
        /// so a pre-resize renderer still names a cell the grid has.
        #[test]
        fn cell_for_position_names_the_cell_holding_the_pixel(
            rows in 1_u16..=200,
            cols in 1_u16..=200,
            cell_w in 0_u32..=64,
            cell_h in 0_u32..=64,
            origin_x in -500.0_f32..=500.0,
            origin_y in -500.0_f32..=500.0,
            row_offset in 0_u32..=200,
            col_offset in 0_u32..=200,
            within_x in 0_u32..=64,
            within_y in 0_u32..=64,
            frac_x in 0.0_f64..0.999,
            frac_y in 0.0_f64..0.999,
        ) {
            let (cw, ch) = (cell_w.max(1), cell_h.max(1));
            let (row, col) = (row_offset % u32::from(rows), col_offset % u32::from(cols));
            let dims = felis_protocol::messages::GridDims { rows, cols, pixel_w: 0, pixel_h: 0 };
            let x = f64::from(origin_x) + f64::from(col * cw + within_x % cw) + frac_x;
            let y = f64::from(origin_y) + f64::from(row * ch + within_y % ch) + frac_y;
            let cell = cell_for_position(pos(x, y), (origin_x, origin_y), cell_w, cell_h, dims);
            proptest::prop_assert_eq!(cell, at(row as u16, col as u16));
        }

        /// Off the block in either direction, the nearest edge cell: a
        /// stray event mid-resize still names a cell the grid has.
        #[test]
        fn cell_for_position_clamps_to_the_nearest_edge_cell(
            rows in 1_u16..=200,
            cols in 1_u16..=200,
            cell_w in 1_u32..=64,
            cell_h in 1_u32..=64,
            origin_x in -500.0_f32..=500.0,
            origin_y in -500.0_f32..=500.0,
            before in 0.0_f64..=10_000.0,
            past in 0.0_f64..=10_000.0,
        ) {
            let dims = felis_protocol::messages::GridDims { rows, cols, pixel_w: 0, pixel_h: 0 };
            let origin = (origin_x, origin_y);
            let above = pos(f64::from(origin_x) - before, f64::from(origin_y) - before);
            proptest::prop_assert_eq!(
                cell_for_position(above, origin, cell_w, cell_h, dims),
                at(0, 0),
            );
            let beyond = pos(
                f64::from(origin_x) + f64::from(u32::from(cols) * cell_w) + past,
                f64::from(origin_y) + f64::from(u32::from(rows) * cell_h) + past,
            );
            proptest::prop_assert_eq!(
                cell_for_position(beyond, origin, cell_w, cell_h, dims),
                at(rows - 1, cols - 1),
            );
        }
    }

    /// Pins: `?1016` reports 1-based pixels relative to the content block.
    #[test]
    fn pixel_for_position_reports_one_based_content_relative_pixels() {
        assert_eq!(pixel_for_position(pos(0.0, 0.0), (0.0, 0.0)), (1, 1));
        assert_eq!(pixel_for_position(pos(11.7, 25.2), (0.0, 0.0)), (12, 26));
        assert_eq!(
            pixel_for_position(pos(111.7, 65.2), (100.0, 40.0)),
            (12, 26)
        );
        assert_eq!(pixel_for_position(pos(10.0, 10.0), (100.0, 40.0)), (1, 1));
    }

    /// The pair rides the wire as `u16`.
    #[test]
    fn pixel_for_position_saturates_at_the_wire_limit() {
        assert_eq!(
            pixel_for_position(pos(f64::from(u32::MAX), 100_000.0), (0.0, 0.0)),
            (u16::MAX, u16::MAX),
        );
    }
}

#[test]
fn window_attributes_keep_native_chrome_when_decorations_on() {
    let attrs = window_attributes("felis".to_owned(), true, false);
    assert!(attrs.decorations);
    assert_eq!(attrs.title, "felis");
    assert!(!attrs.transparent);
}

#[test]
fn window_attributes_flag_transparent_surface_when_requested() {
    // Without it the OS composites the window opaque and the renderer's
    // bg alpha is silently ignored.
    let attrs = window_attributes("felis".to_owned(), true, true);
    assert!(attrs.transparent);
}

#[test]
fn window_attributes_keep_rounded_corners_on_macos_when_chromeless() {
    // winit gives OS rounded corners and shadow only to a `Titled`
    // window and selects `Borderless` whenever `decorations` is false,
    // so on macOS the flag stays `true` and the titlebar is emptied
    // instead.
    let attrs = window_attributes("felis".to_owned(), false, false);
    if cfg!(target_os = "macos") {
        assert!(attrs.decorations);
    } else {
        assert!(!attrs.decorations);
    }
}

#[test]
fn the_configured_baseline_routes_font_size_through_the_sanitizer() {
    // Pins the integration: the baseline goes through
    // `sanitized_font_size_logical_px`, not `cfg.font.size_px` raw.
    let oversize = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(200.0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        configured_font_size_logical_px(&oversize),
        Some(MAX_FONT_SIZE_LOGICAL_PX),
        "200 px config size must clamp to MAX",
    );

    let nan = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(f32::NAN),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(
        configured_font_size_logical_px(&nan).is_none(),
        "NaN must drop to None",
    );
}

#[test]
fn renderer_config_from_leaves_the_font_size_to_the_window() {
    // The renderer takes a physical size, and the scale factor only
    // exists once the window does; filling it from the logical value
    // rasterized glyphs at half size on Retina.
    let cfg = config::EffectiveConfig {
        font: config::FontConfig {
            size_px: Some(15.0),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(renderer_config_from(&cfg).font_size_physical_px.is_none());
}

#[test]
fn renderer_config_from_default_config_is_all_none() {
    // Pins: no config file leaves every override slot empty, so the
    // renderer keeps its built-in defaults.
    let rc = renderer_config_from(&config::EffectiveConfig::default());
    assert!(rc.font_family.is_none());
    assert!(rc.font_size_physical_px.is_none());
    assert_eq!(rc.font_fallbacks, Vec::<FaceSpec>::new());
    assert!(rc.theme_fg.is_none());
    assert!(rc.theme_bg.is_none());
    assert!(rc.theme_cursor.is_none());
    assert!(rc.theme_palette.is_empty());
}

#[test]
#[allow(clippy::float_cmp)]
fn css_shorthand_theme_color_falls_back_to_the_defaults_end_to_end() {
    // Pins end to end (config -> RendererConfig -> Theme): `#abc` yields
    // the built-in color, and the rejection must survive every hop or
    // the painted color would not match the reported theme.
    let short = config::EffectiveConfig {
        theme: config::ThemeConfig {
            foreground: Some("#abc".into()),
            background: Some("#def".into()),
            ..Default::default()
        },
        cursor: config::CursorConfig {
            color: Some("#fff".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let theme = Renderer::theme_from_config(&renderer_config_from(&short));
    let default = felis_render_wgpu::palette::Theme::default();
    assert_eq!(theme.fg, default.fg, "shorthand fg keeps the default");
    assert_eq!(theme.bg, default.bg, "shorthand bg keeps the default");
    assert_eq!(
        theme.cursor, None,
        "shorthand cursor color falls back to reverse video",
    );
}

#[test]
fn renderer_config_from_full_config_carries_every_field_through() {
    let cfg = config::EffectiveConfig {
        font: config::FontConfig {
            family: Some("JetBrains Mono".to_owned()),
            size_px: Some(15.0),
            fallback: vec![
                config::FontStyleConfig {
                    family: Some("Noto Sans CJK JP".to_owned()),
                    features: None,
                },
                config::FontStyleConfig {
                    family: Some("Noto Color Emoji".to_owned()),
                    features: Some(Vec::new()),
                },
            ],
            features: Vec::new(),
            bold: config::FontStyleConfig {
                family: Some("Iosevka Bold".to_owned()),
                features: Some(vec!["ss01".to_owned()]),
            },
            italic: config::FontStyleConfig {
                family: Some("Victor Mono".to_owned()),
                features: None,
            },
            bold_italic: config::FontStyleConfig::default(),
        },
        cursor: config::CursorConfig {
            color: Some("#ffaa00".to_owned()),
            ..Default::default()
        },
        theme: config::ThemeConfig {
            foreground: Some("#cdcdcd".to_owned()),
            background: Some("#101010".to_owned()),
            palette: config::PaletteConfig {
                red: Some("#ff5555".to_owned()),
                bright_blue: Some("#5555ff".to_owned()),
                indexed: std::collections::BTreeMap::from([(
                    "16".to_owned(),
                    "#d08770".to_owned(),
                )]),
                ..Default::default()
            },
        },
        ..Default::default()
    };
    let rc = renderer_config_from(&cfg);
    assert_eq!(rc.font_family.as_deref(), Some("JetBrains Mono"));
    // Inherit (no features) vs opt-out (empty list) must survive the mapping.
    assert_eq!(
        rc.font_fallbacks,
        vec![
            FaceSpec::named("Noto Sans CJK JP"),
            FaceSpec {
                family: Some("Noto Color Emoji".to_owned()),
                features: Some(Vec::new()),
            },
        ],
    );
    assert_eq!(rc.font_bold.family.as_deref(), Some("Iosevka Bold"));
    assert_eq!(rc.font_bold.features, Some(vec!["ss01".to_owned()]));
    assert_eq!(rc.font_italic.family.as_deref(), Some("Victor Mono"));
    assert_eq!(rc.font_italic.features, None);
    assert_eq!(rc.font_bold_italic.family, None);
    assert_eq!(rc.theme_fg.as_deref(), Some("#cdcdcd"));
    assert_eq!(rc.theme_bg.as_deref(), Some("#101010"));
    assert_eq!(rc.theme_cursor.as_deref(), Some("#ffaa00"));
    // `PaletteConfig::into_overrides` lays out by SGR index: slot 1 is
    // red, 12 is bright_blue, and `indexed` carries cube slot 16.
    assert_eq!(
        rc.theme_palette.get(&1).map(String::as_str),
        Some("#ff5555")
    );
    assert_eq!(
        rc.theme_palette.get(&12).map(String::as_str),
        Some("#5555ff")
    );
    assert_eq!(
        rc.theme_palette.get(&16).map(String::as_str),
        Some("#d08770")
    );
    // Untouched slots are absent; the renderer keeps the xterm baseline.
    assert!(!rc.theme_palette.contains_key(&0), "slot 0 stays at xterm");
    assert!(
        !rc.theme_palette.contains_key(&15),
        "slot 15 stays at xterm"
    );
}

mod font_differs {
    use super::*;

    #[test]
    fn a_size_only_edit_takes_the_size_only_reload_path() {
        // `App::reload_config` picks its renderer call from these
        // predicates; a size-only edit must land on `reload_font_size`,
        // since the full `reload_font` rescans fontdb. The size is not
        // on `RendererConfig`, so the invariant spans both inputs the
        // reload reads.
        let base = config::EffectiveConfig {
            font: config::FontConfig {
                family: Some("JetBrains Mono".to_owned()),
                size_px: Some(14.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bigger = base.clone();
        bigger.font.size_px = Some(16.0);

        assert_ne!(
            configured_font_size_logical_px(&bigger),
            configured_font_size_logical_px(&base),
            "the reload's `size_changed` arm must see the edit",
        );
        let (base_rc, bigger_rc) = (renderer_config_from(&base), renderer_config_from(&bigger));
        assert!(
            !font_stack_inputs_differ(&base_rc, &bigger_rc),
            "size-only edit must take the fast path",
        );
        assert!(
            !font_features_differ(&base_rc, &bigger_rc),
            "size-only edit must not invalidate for features",
        );
        assert!(
            !font_settings_differ(&base_rc, &bigger_rc),
            "the size no longer rides RendererConfig, so nothing there changed",
        );
    }

    #[test]
    fn font_stack_inputs_differ_isolates_stack_edits() {
        // Pins the split between the slow path (`reload_font` rescans
        // fontdb) and the fast paths that keep the `FontStack`.
        let base = RendererConfig {
            font_family: Some("JetBrains Mono".to_owned()),
            font_fallbacks: vec![FaceSpec::named("Noto Sans CJK JP")],
            ..Default::default()
        };
        let mut family = base.clone();
        family.font_family = Some("Hack".to_owned());
        assert!(font_settings_differ(&base, &family));
        assert!(font_stack_inputs_differ(&base, &family));
        let mut fallback = base.clone();
        fallback.font_fallbacks = vec![FaceSpec::named("Noto Color Emoji")];
        assert!(font_settings_differ(&base, &fallback));
        assert!(font_stack_inputs_differ(&base, &fallback));
        let mut features = base.clone();
        features.font_features = vec!["calt".to_owned()];
        assert!(font_settings_differ(&base, &features));
        assert!(
            !font_stack_inputs_differ(&base, &features),
            "features-only edit must take the shape-time fast path",
        );
        // A new styled face needs the fontdb rescan like a base-family change.
        let mut bold = base.clone();
        bold.font_bold = FaceSpec::named("Iosevka");
        assert!(font_settings_differ(&base, &bold));
        assert!(font_stack_inputs_differ(&base, &bold));
        assert!(!font_settings_differ(&base, &base));
        assert!(!font_stack_inputs_differ(&base, &base));
    }

    #[test]
    fn font_settings_differ_ignores_theme_changes() {
        // A pure color change must not take the atlas-rebuild slow path.
        let base = RendererConfig {
            font_family: Some("JetBrains Mono".to_owned()),
            theme_fg: Some("#ffffff".to_owned()),
            ..Default::default()
        };
        let mut other = base.clone();
        other.theme_fg = Some("#000000".to_owned());
        other.theme_bg = Some("#101010".to_owned());
        other.theme_cursor = Some("#ff00ff".to_owned());
        assert!(!font_settings_differ(&base, &other));
    }
}

#[test]
fn reported_theme_differs_tracks_only_the_reported_colors() {
    // The reload path re-sends `ConfigureTheme` off this predicate, and
    // the frame carries fg / bg / cursor only: a stale one leaves OSC
    // 10/11/12 answering the attach-time color; a palette- or
    // opacity-only edit would resend identical colors.
    let base = RendererConfig {
        theme_fg: Some("#ffffff".to_owned()),
        theme_bg: Some("#000000".to_owned()),
        theme_cursor: Some("#ff00ff".to_owned()),
        ..Default::default()
    };
    assert!(!reported_theme_differs(&base, &base));

    let mut fg = base.clone();
    fg.theme_fg = Some("#eeeeee".to_owned());
    assert!(reported_theme_differs(&base, &fg));

    let mut bg = base.clone();
    bg.theme_bg = Some("#101010".to_owned());
    assert!(reported_theme_differs(&base, &bg));

    let mut cursor = base.clone();
    cursor.theme_cursor = Some("#00ff00".to_owned());
    assert!(reported_theme_differs(&base, &cursor));

    let mut palette_only = base.clone();
    palette_only.theme_palette.insert(1, "#ff0000".to_owned());
    assert!(!reported_theme_differs(&base, &palette_only));

    let mut opacity_only = base.clone();
    opacity_only.background_opacity = Some(0.8);
    assert!(!reported_theme_differs(&base, &opacity_only));
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
fn held_button_repeat_press_becomes_the_newest_for_drag_attribution() {
    let mut held = HeldButtons::default();
    held.press(MouseButton::Left, PressRouting::Forwarded);
    held.press(MouseButton::Right, PressRouting::Forwarded);
    held.press(MouseButton::Left, PressRouting::Forwarded);
    assert_eq!(held.last(), Some(MouseButton::Left));
    held.release(MouseButton::Left);
    assert_eq!(held.last(), Some(MouseButton::Right));
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
}

mod selection_invalidation {
    use felis_protocol::messages::{ModifyOtherKeys, MouseProtocol};

    use super::*;

    #[test]
    fn viewport_state_into_scrollback_clears_selection() {
        // Pins: the highlight must not outlive the cells it was anchored to.
        assert!(viewport_message_enters_scrollback(
            &GridMsg::ViewportState {
                lines_from_bottom: 1,
                max: 100,
            }
        ));
        assert!(viewport_message_enters_scrollback(
            &GridMsg::ViewportState {
                lines_from_bottom: 9_999,
                max: 10_024,
            }
        ));
    }

    #[test]
    fn viewport_state_at_bottom_keeps_selection() {
        assert!(!viewport_message_enters_scrollback(
            &GridMsg::ViewportState {
                lines_from_bottom: 0,
                max: 24,
            }
        ));
    }

    #[test]
    fn unrelated_grid_messages_never_request_a_selection_clear() {
        assert!(!viewport_message_enters_scrollback(&GridMsg::Attention {
            source: felis_protocol::messages::AttentionSource::Bell,
        }));
        assert!(!viewport_message_enters_scrollback(&GridMsg::CursorState {
            row: 0,
            col: 0,
            visible: true,
            style: felis_protocol::messages::CursorStyle::default(),
            blink: true,
        }));
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
        let msg = mode_flags(true, MouseProtocol::Off);
        assert!(grid_change_invalidates_selection(&msg, false, true));
        assert!(grid_change_invalidates_selection(&msg, true, false));
    }

    #[test]
    fn a_mode_flip_that_leaves_the_screen_alone_keeps_the_selection() {
        // `ModeFlags` carries every bit at once, so a TUI enabling mouse
        // reporting after taking the alt screen re-sends the same
        // `alt_screen`; only the transition may clear.
        assert!(!grid_change_invalidates_selection(
            &mode_flags(true, MouseProtocol::AnyMotion),
            true,
            true
        ));
        assert!(!grid_change_invalidates_selection(
            &mode_flags(false, MouseProtocol::ButtonEvents),
            false,
            false
        ));
    }

    #[test]
    fn a_scroll_into_scrollback_clears_the_selection_on_either_screen() {
        let scrolled = GridMsg::ViewportState {
            lines_from_bottom: 3,
            max: 100,
        };
        assert!(grid_change_invalidates_selection(&scrolled, false, false));
        assert!(grid_change_invalidates_selection(&scrolled, true, true));
        let at_bottom = GridMsg::ViewportState {
            lines_from_bottom: 0,
            max: 100,
        };
        assert!(!grid_change_invalidates_selection(&at_bottom, false, false));
    }

    const fn mode_flags(alt_screen: bool, mouse_protocol: MouseProtocol) -> GridMsg {
        GridMsg::ModeFlags {
            bracketed_paste: false,
            alt_screen,
            mouse_protocol,
            application_cursor: false,
            modify_other_keys: ModifyOtherKeys::Off,
            application_keypad: false,
            win32_input_mode: false,
            reverse_video: false,
        }
    }
}

#[test]
fn ime_commit_forwards_japanese_text_as_utf8_bytes() {
    let bytes = ime_commit_bytes(&Ime::Commit("こんにちは".to_owned()))
        .expect("non-empty commit must forward bytes");
    assert_eq!(bytes, "こんにちは".as_bytes());
}

#[test]
fn empty_ime_commit_is_dropped() {
    // Some IMEs send an empty Commit on cancel.
    assert_eq!(ime_commit_bytes(&Ime::Commit(String::new())), None);
}

#[test]
fn preedit_from_ime_anchors_at_cursor_cell_with_text() {
    let overlay =
        preedit_from_ime(&Ime::Preedit("こん".to_owned(), Some((6, 6))), at(3, 5)).unwrap();
    assert_eq!(overlay.anchor, (3, 5));
    assert_eq!(overlay.text, "こん");
    assert_eq!(overlay.cursor, Some((6, 6)));
}

#[test]
fn preedit_from_ime_empty_string_clears_overlay() {
    // Platform IMEs send an empty Preedit when composition ends without commit.
    assert!(preedit_from_ime(&Ime::Preedit(String::new(), None), at(0, 0)).is_none());
}

#[test]
fn preedit_from_ime_commit_event_clears_overlay() {
    assert!(preedit_from_ime(&Ime::Commit("hello".to_owned()), at(0, 0)).is_none());
    assert!(preedit_from_ime(&Ime::Enabled, at(0, 0)).is_none());
    assert!(preedit_from_ime(&Ime::Disabled, at(0, 0)).is_none());
}

#[test]
fn ime_preedit_enabled_disabled_carry_no_bytes_today() {
    // Pre-edit text rides the renderer overlay, never the PTY.
    assert_eq!(ime_commit_bytes(&Ime::Enabled), None);
    assert_eq!(ime_commit_bytes(&Ime::Disabled), None);
    assert_eq!(
        ime_commit_bytes(&Ime::Preedit("こん".to_owned(), Some((2, 2)))),
        None,
    );
}

mod daemon_closed {
    use super::*;

    #[test]
    fn daemon_closed_on_the_live_connection_reconnects_when_nothing_is_in_flight() {
        assert_eq!(
            daemon_closed_outcome(3, 3, false, &PipeState::Idle),
            ClosedOutcome::Reconnect,
        );
    }

    #[test]
    fn daemon_closed_on_the_live_connection_defers_to_an_in_flight_switch() {
        // A landing started before the dying session's connection
        // reached EOF, so that EOF arrives on the still-current
        // generation.
        assert_eq!(
            daemon_closed_outcome(3, 3, true, &PipeState::Idle),
            ClosedOutcome::AwaitSwitch,
        );
    }

    #[test]
    fn daemon_closed_while_attached_to_a_transient_returns_to_the_origin() {
        assert_eq!(
            daemon_closed_outcome(
                3,
                3,
                false,
                &PipeState::Active {
                    viewport: 12,
                    region: None,
                    source: test_place("/tmp/a.sock", 0xaaa),
                },
            ),
            ClosedOutcome::PipeReturn { viewport: 12 },
        );
    }

    #[test]
    fn superseded_connection_eof_never_closes_the_window() {
        // A superseded connection's reader can post `DaemonClosed` after a subsequent
        // switch completes. When returning to Idle, the generation stamp distinguishes
        // that stale EOF from the live daemon dying.
        for &switch_in_flight in &[true, false] {
            for pipe_state in &[
                PipeState::Idle,
                PipeState::Awaiting {
                    target: felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
                    viewport: 12,
                },
                PipeState::Active {
                    viewport: 12,
                    region: None,
                    source: test_place("/tmp/a.sock", 0xaaa),
                },
                PipeState::Returning {
                    viewport: 12,
                    expected: test_place("/tmp/a.sock", 0xaaa),
                },
            ] {
                assert_eq!(
                    daemon_closed_outcome(1, 2, switch_in_flight, pipe_state),
                    ClosedOutcome::Ignore,
                    "stale EOF must not be read as the live connection's",
                );
            }
        }
    }

    #[test]
    fn a_transient_landing_leaves_the_handoff_state_intact() {
        // Only the return may consume the state: consuming it on the way
        // in unlinks the region file under the command still reading it,
        // drops the parked origin, and lets a second chord fire inside
        // the live transient.
        let dir = std::env::temp_dir().join(format!("felis-pipe-landing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("region.txt");
        std::fs::write(&path, b"region\n").unwrap();
        let mut state = PipeState::Active {
            viewport: 12,
            region: Some(StagedRegion::adopt(path.clone())),
            source: test_place("/tmp/a.sock", 0xaaa),
        };
        assert!(
            state
                .settle_returning(&test_place("/tmp/a.sock", 1), false)
                .is_none()
        );
        assert!(matches!(
            state,
            PipeState::Active {
                viewport: 12,
                region: Some(_),
                ..
            },
        ));
        assert!(path.exists(), "the staged region must survive the landing");

        drop(state);
        assert!(!path.exists(), "the return path's drop is what unlinks it");
        std::fs::remove_dir(&dir).unwrap();
    }

    /// Only the exact place the viewport belongs to gets it back:
    /// restoring on an older trail entry would paste one session's
    /// scroll position onto another's grid.
    #[test]
    fn a_return_landing_restores_the_viewport_only_at_the_expected_place() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: 12,
            expected: expected.clone(),
        };
        assert_eq!(state.settle_returning(&expected, false), Some(12));
        assert!(matches!(state, PipeState::Idle));

        let mut state = PipeState::Returning {
            viewport: 12,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/b.sock", 0xaaa), false),
            None,
            "the same id on another daemon is another place"
        );
        assert!(
            matches!(state, PipeState::Idle),
            "settling on a real place ends the handoff, sent or not"
        );
    }

    /// A pipe/run chain that hops transient to transient is still one
    /// visit, so the handoff outlives every landing inside it and the
    /// return still restores the viewport at the far end.
    #[test]
    fn a_handoff_survives_the_landings_inside_a_transient_chain() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: 12,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/local.sock", 0x1), true),
            None
        );
        assert!(
            matches!(state, PipeState::Returning { .. }),
            "a landing the chain's own re-point made keeps the handoff parked"
        );
        assert_eq!(state.settle_returning(&expected, false), Some(12));
        assert!(matches!(state, PipeState::Idle));
    }

    /// The guard a nested pipe/run chord asks: only a parked return
    /// owes anything, and the visit that owes it is the one a second
    /// visit would overwrite.
    #[test]
    fn a_visit_owes_a_return_from_its_landing_until_the_return_lands() {
        assert!(
            PipeState::Returning {
                viewport: 12,
                expected: test_place("/tmp/a.sock", 0xaaa),
            }
            .owes_a_return()
        );
        assert!(!PipeState::Idle.owes_a_return());
        assert!(
            PipeState::Active {
                viewport: 12,
                region: None,
                source: test_place("/tmp/a.sock", 0xaaa),
            }
            .owes_a_return(),
            "a live transient owes its return before it has started it"
        );
        assert!(
            !PipeState::Awaiting {
                target: felis_client_core::PipeTarget::Clipboard,
                viewport: 12,
            }
            .owes_a_return()
        );
    }

    /// A continuation that failed or never started leaves nothing to
    /// wait for, so the handoff is dropped rather than left for an
    /// unrelated later switch to that place to inherit.
    #[test]
    fn a_handoff_with_nothing_left_to_continue_it_is_dropped() {
        let mut state = PipeState::Returning {
            viewport: 12,
            expected: test_place("/tmp/a.sock", 0xaaa),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/b.sock", 0xbbb), false),
            None
        );
        assert!(
            matches!(state, PipeState::Idle),
            "a settled unwind keeps no handoff"
        );
    }

    /// An intent the daemon already reported delivered is a
    /// continuation, so the unwind has not settled and the handoff is
    /// not that landing's to drop.
    #[test]
    fn a_handoff_survives_a_landing_with_a_continuation_still_parked() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: 12,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/c.sock", 0x9), true),
            None
        );
        assert!(matches!(state, PipeState::Returning { .. }));
        assert_eq!(state.settle_returning(&expected, false), Some(12));
    }

    #[test]
    fn region_data_that_answers_no_request_leaves_a_live_transient_alone() {
        // A region reply carries no sink of its own, so it is only
        // answerable while `Awaiting`.
        let mut state = PipeState::Active {
            viewport: 12,
            region: None,
            source: test_place("/tmp/a.sock", 0xaaa),
        };
        assert!(state.take_awaiting().is_none());
        assert!(matches!(state, PipeState::Active { viewport: 12, .. }));

        let mut state = PipeState::Awaiting {
            target: felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
            viewport: 7,
        };
        let (target, viewport) = state.take_awaiting().expect("the awaited sink comes back");
        assert_eq!(
            target,
            felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
        );
        assert_eq!(viewport, 7);
        assert!(matches!(state, PipeState::Idle));
    }

    #[test]
    fn the_first_connection_is_live_before_any_switch() {
        // `drive` stamps `FIRST_CONN_GEN` onto the initial pump and seeds
        // `App::conn_gen` with the same value.
        assert_eq!(
            daemon_closed_outcome(FIRST_CONN_GEN, FIRST_CONN_GEN, false, &PipeState::Idle),
            ClosedOutcome::Reconnect,
        );
    }
}

/// What a shell exit does to a landing already in flight
/// (`AppEvent::SessionExited`: newest intent wins).
mod switch_state_carries_the_exit {
    use super::*;

    /// The failure handler tests the retry intent before the missing
    /// session, so a landing left with its retry would re-fetch a roster
    /// on the connection the exit tore down and stay busy on a dead grid
    /// forever.
    #[test]
    fn an_in_flight_landings_retry_is_dropped_with_the_session() {
        let mut state = SwitchState::InFlight {
            session_gone: false,
            retry: Some(SwitchDirection::Next),
        };
        assert!(state.carry_exit());
        assert_eq!(
            state,
            SwitchState::InFlight {
                session_gone: true,
                retry: None,
            },
        );
    }

    /// The caller closes the window instead.
    #[test]
    fn an_idle_window_carries_nothing() {
        let mut state = SwitchState::Idle;
        assert!(!state.carry_exit());
        assert_eq!(state, SwitchState::Idle);
    }
}

/// Issue #22: a window takes the exit ladder when its shell exits, and
/// re-dials the same session when the transport drops under it.
mod lifecycle {
    use super::*;

    /// The two halves the handler joins, pinned separately: the
    /// classifier's verdict on a close that arrives mid-landing, and
    /// the latch it then sets. Driving `App::on_daemon_closed` itself
    /// needs a live winit event loop (Forgejo issue #150).
    #[test]
    fn a_close_during_a_switch_classifies_as_await_switch_and_the_lost_latch_round_trips() {
        let (outgoing, _rx) = outgoing_channel();
        assert!(!outgoing.is_lost());
        assert_eq!(
            daemon_closed_outcome(FIRST_CONN_GEN, FIRST_CONN_GEN, true, &PipeState::Idle),
            ClosedOutcome::AwaitSwitch,
        );
        outgoing.mark_lost();
        assert!(
            outgoing.is_lost(),
            "the failed landing decides on this bit, not on the queue's depth"
        );
    }

    /// Pins the criterion "terminal states are visible": each reason
    /// names a remedy; the nonzero status is the return type.
    #[test]
    fn every_terminal_reconnect_state_names_a_remedy() {
        for reason in [ExitReason::Refused, ExitReason::RetriesExhausted] {
            assert_ne!(reason.headline(), "");
            assert!(
                reason.remedy().contains("felis "),
                "{reason:?} must name the command that recovers it"
            );
        }
        // docs/reference/cli.md "Exit codes": a connection this build
        // could not use is a failure to ask.
        assert_eq!(ExitReason::Refused.code().get(), 2);
        assert_eq!(ExitReason::RetriesExhausted.code().get(), 2);
    }

    /// A session that ended is not a terminal state: the window unwinds
    /// its trail, and a spent ladder closes on `0` like any shell exit.
    #[test]
    fn only_a_connection_the_window_cannot_use_ends_it() {
        let eof = || DialError::Create(felis_client_core::ConnectError::EofBeforeWelcome);
        let gone = felis_client_core::ReconnectError::SessionGone(eof());
        assert_eq!(ExitReason::from_reconnect(&gone), None);

        let refused = felis_client_core::ReconnectError::Refused(eof());
        assert_eq!(
            ExitReason::from_reconnect(&refused),
            Some(ExitReason::Refused)
        );

        let spent = felis_client_core::ReconnectError::Exhausted {
            attempts: 6,
            source: eof(),
        };
        assert_eq!(
            ExitReason::from_reconnect(&spent),
            Some(ExitReason::RetriesExhausted)
        );
    }

    /// Pins the criterion "retry bounds are visible": the indicator is
    /// on the one surface a window with no frames can still repaint.
    #[test]
    fn the_disconnected_window_says_so_in_its_title() {
        assert_eq!(
            app_methods::window_title_for(None, "zsh: ~/work", true),
            "zsh: ~/work \u{2014} disconnected"
        );
        assert_eq!(
            app_methods::window_title_for(Some("[work]"), "zsh", false),
            "[work] zsh"
        );
    }
}

/// A [`Place`](exit_ladder::Place) on a named local socket.
fn test_place(socket: &str, session_id: u128) -> exit_ladder::Place {
    exit_ladder::Place {
        reconnector: Reconnector {
            carrier: Carrier::Local(PathBuf::from(socket).into()),
            offer: Offer::window(true),
        },
        session_id,
    }
}
