use crate::test_support::{drive, responses};
use crate::*;
use felis_protocol::messages::ThemeChannel;
use felis_vt::Parser;

#[test]
fn osc_8_stamps_current_link_onto_subsequent_prints() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 6);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;id=foo;https://example.test/\x07ab\x1b]8;;\x07c",
    );
    let id = g.cell(0, 0).unwrap().link.unwrap();
    assert_eq!(g.cell(0, 1).unwrap().link, Some(id));
    assert_eq!(g.cell(0, 2).unwrap().link, None);
    let entry = g.hyperlink(id).unwrap();
    assert_eq!(entry.id.as_ref().map(LinkText::as_str), Some("foo"));
    assert_eq!(entry.uri, "https://example.test/");
}

/// `ansi::row_ansi_with` indexes the bulk table to re-emit a row's
/// anchors; an accessor off by one silently re-emits a row's links
/// pointing at each other's targets.
#[test]
fn the_bulk_hyperlink_table_indexes_as_a_cells_handle_less_one() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;;https://example.test/a\x07a\x1b]8;;https://example.test/b\x07b",
    );
    let table = g.hyperlink_table();
    assert_eq!(table.len(), 2);
    for col in 0..2 {
        let id = g.cell(0, col).unwrap().link.unwrap();
        assert_eq!(
            table.get(id).unwrap(),
            g.hyperlink(id).unwrap(),
            "column {col}"
        );
    }
    let handle = |n| NonZeroU16::new(n).expect("nonzero");
    assert_eq!(table.get(handle(1)).unwrap().uri, "https://example.test/a");
    assert_eq!(table.get(handle(2)).unwrap().uri, "https://example.test/b");
}

#[test]
fn osc_22_sets_pointer_shape_and_flags_dirty() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;pointer\x07");
    assert_eq!(g.pointer_shape(), Some("pointer"));
    assert_eq!(
        g.take_pointer_shape_dirty(),
        Some(Some("pointer".to_owned()))
    );
    assert_eq!(g.take_pointer_shape_dirty(), None, "dirty clears on take");
}

#[test]
fn osc_22_empty_body_resets_to_default_arrow() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    assert_eq!(g.pointer_shape(), Some("text"));
    assert_eq!(g.take_pointer_shape_dirty(), Some(Some("text".to_owned())));
    drive(&mut p, &mut g, b"\x1b]22;\x07");
    assert_eq!(g.pointer_shape(), None, "empty body resets to default");
    assert_eq!(g.take_pointer_shape_dirty(), Some(None), "reset is shipped");
}

#[test]
fn osc_22_rejects_invalid_keyword_keeping_current_shape() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;crosshair\x07");
    assert_eq!(
        g.take_pointer_shape_dirty(),
        Some(Some("crosshair".to_owned()))
    );
    // Junk must not clobber the valid earlier request.
    drive(&mut p, &mut g, b"\x1b]22;Bogus Shape!\x07");
    assert_eq!(g.pointer_shape(), Some("crosshair"));
    assert_eq!(g.take_pointer_shape_dirty(), None, "invalid → no change");
}

#[test]
fn osc_22_push_and_pop_restore_the_prior_shape() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    drive(&mut p, &mut g, b"\x1b]22;>wait\x07");
    assert_eq!(
        g.pointer_shape(),
        Some("wait"),
        "push makes the new shape live"
    );
    drive(&mut p, &mut g, b"\x1b]22;<\x07");
    assert_eq!(
        g.pointer_shape(),
        Some("text"),
        "pop restores the pushed shape"
    );
}

#[test]
fn osc_22_pop_on_empty_stack_resets_to_default() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    // kitty's behavior: pop on an empty stack resets to the default.
    drive(&mut p, &mut g, b"\x1b]22;<\x07");
    assert_eq!(g.pointer_shape(), None, "pop with an empty stack resets");
}

#[test]
fn osc_22_takes_the_first_valid_shape_of_a_fallback_list() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    // A comma list is kitty's fallback form; the client resolves
    // recognition.
    drive(&mut p, &mut g, b"\x1b]22;grab,progress\x07");
    assert_eq!(g.pointer_shape(), Some("grab"));
    drive(&mut p, &mut g, b"\x1b]22;BAD!,pointer\x07");
    assert_eq!(g.pointer_shape(), Some("pointer"));
}

#[test]
fn osc_8_dedupes_same_id_and_uri_to_one_table_row() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 6);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;id=k;https://u\x07x\x1b]8;;\x07\x1b]8;id=k;https://u\x07y",
    );
    assert_eq!(g.hyperlink_count(), 1);
    let id_x = g.cell(0, 0).unwrap().link.unwrap();
    let id_y = g.cell(0, 1).unwrap().link.unwrap();
    assert_eq!(id_x, id_y);
}

#[test]
fn osc_8_dedupe_truth_table_remaining_corners() {
    // Identity is "id pair matches AND uri pair matches"; every other
    // combination produces a fresh row.

    // Corner 1: (Some(k), https://u_a) + (Some(k), https://u_b) → distinct.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;id=k;https://u_a\x07a\x1b]8;;\x07\x1b]8;id=k;https://u_b\x07b",
    );
    assert_eq!(
        g.hyperlink_count(),
        2,
        "same id + distinct uris → distinct rows"
    );
    assert_ne!(
        g.cell(0, 0).unwrap().link.unwrap(),
        g.cell(0, 1).unwrap().link.unwrap(),
    );

    // Corner 2: (None, https://u) + (None, https://u) → dedup.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;;https://u\x07a\x1b]8;;\x07\x1b]8;;https://u\x07b",
    );
    assert_eq!(g.hyperlink_count(), 1, "None + same uri → dedup");
    assert_eq!(
        g.cell(0, 0).unwrap().link.unwrap(),
        g.cell(0, 1).unwrap().link.unwrap(),
    );

    // Corner 3: (None, https://u) + (Some(k), https://u) → distinct.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;;https://u\x07a\x1b]8;;\x07\x1b]8;id=k;https://u\x07b",
    );
    assert_eq!(
        g.hyperlink_count(),
        2,
        "None vs Some(k) for same uri → distinct rows",
    );
    assert_ne!(
        g.cell(0, 0).unwrap().link.unwrap(),
        g.cell(0, 1).unwrap().link.unwrap(),
    );
}

#[test]
fn osc_8_same_uri_different_id_gets_distinct_ids() {
    // Per the xterm / foot convention `id=` labels the anchor;
    // hover-grouping keyed on anchor id must not collapse independent
    // anchors.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;id=k1;https://same\x07a\x1b]8;;\x07\x1b]8;id=k2;https://same\x07b",
    );
    assert_eq!(g.hyperlink_count(), 2, "different ids → distinct rows");
    let a = g.cell(0, 0).unwrap().link.unwrap();
    let b = g.cell(0, 1).unwrap().link.unwrap();
    assert_ne!(a, b);
    assert_eq!(g.hyperlink(a).unwrap().uri, "https://same");
    assert_eq!(g.hyperlink(b).unwrap().uri, "https://same");
}

#[test]
fn osc_8_distinct_uris_get_distinct_ids() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]8;;https://u1\x07a\x1b]8;;\x07\x1b]8;;https://u2\x07b",
    );
    assert_eq!(g.hyperlink_count(), 2);
    let a = g.cell(0, 0).unwrap().link.unwrap();
    let b = g.cell(0, 1).unwrap().link.unwrap();
    assert_ne!(a, b);
}

#[test]
fn osc_8_link_table_caps_at_u16_max_to_bound_memory_growth() {
    // `Cell.link` is `NonZeroU16`, so `LinkTable::intern` caps before
    // push. Driven through the parser to keep the whole OSC 8 path
    // honest; affordable because dedup is a hash lookup.
    let mut g = Grid::new(1, 1);
    let mut p = Parser::new();
    let cap = u16::MAX as usize;
    for i in 0..cap {
        let bytes = format!("\x1b]8;;https://u{i}\x07X\x1b]8;;\x07");
        p.advance(&mut g, bytes.as_bytes());
    }
    assert_eq!(g.hyperlink_count(), cap);
    p.advance(&mut g, b"\x1b]8;;https://overflow\x07X\x1b]8;;\x07");
    assert_eq!(
        g.hyperlink_count(),
        cap,
        "table must not grow past u16::MAX",
    );
}

#[test]
fn osc_8_with_uri_containing_semicolons_round_trips() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    // OSC 8 spends its separators on the code and the params section,
    // so the URI keeps its own.
    drive(&mut p, &mut g, b"\x1b]8;;https://e.test/?a=1;b=2\x07X");
    let id = g.cell(0, 0).unwrap().link.unwrap();
    assert_eq!(g.hyperlink(id).unwrap().uri, "https://e.test/?a=1;b=2");
}

#[test]
fn osc_8_with_japanese_uri_round_trips_after_0x9c_fix() {
    // The byte 0x9C inside `作` (E4 BD 9C) truncates the URI if the
    // parser honors it as C1 ST.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        "\x1b]8;;file:///home/u/作業/index\x07X".as_bytes(),
    );
    let id = g.cell(0, 0).unwrap().link.unwrap();
    assert_eq!(g.hyperlink(id).unwrap().uri, "file:///home/u/作業/index");
}

#[test]
fn osc_8_empty_uri_clears_current_link() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]8;;https://u\x07\x1b]8;;\x07X");
    assert_eq!(g.cell(0, 0).unwrap().link, None);
}

#[test]
fn osc_8_rejects_control_bytes_in_params_or_uri() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]8;\x01id=k;https://u\x07X");
    assert_eq!(g.hyperlink_count(), 0);
    assert_eq!(g.cell(0, 0).unwrap().link, None);

    let mut g = Grid::new(1, 4);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]8;;https://u\x01ri\x07X");
    assert_eq!(g.hyperlink_count(), 0);
}

/// REQ-910 negative (`security-model.md`): a positive-only allowlist,
/// so `xdg-open` is never left to guess.
#[test]
fn osc_8_rejects_disallowed_and_schemeless_uris() {
    for uri in [
        "javascript:alert(1)",
        "data:text/html,<x>",
        "vbscript:msgbox(1)",
        "chrome-extension://abcdef/x",
        "example.test/no-scheme",
    ] {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        let bytes = format!("\x1b]8;;{uri}\x07X\x1b]8;;\x07");
        p.advance(&mut g, bytes.as_bytes());
        assert_eq!(
            g.hyperlink_count(),
            0,
            "disallowed scheme {uri:?} leaked into the link table",
        );
        assert_eq!(
            g.cell(0, 0).unwrap().link,
            None,
            "disallowed scheme {uri:?} stamped a cell link",
        );
    }
}

/// Trailing cells must not inherit the prior safe URI
/// (confusion-attack class).
#[test]
fn osc_8_disallowed_scheme_mid_stream_clears_the_pen() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    p.advance(
        &mut g,
        b"\x1b]8;;https://good\x07a\x1b]8;;javascript:bad\x07b\x1b]8;;\x07",
    );
    assert!(g.cell(0, 0).unwrap().link.is_some());
    // Written while the javascript: open should have failed.
    assert_eq!(g.cell(0, 1).unwrap().link, None);
}

#[test]
fn osc_8_rejected_params_or_uri_mid_stream_clears_the_pen() {
    for reject in [
        &b"\x1b]8;\x01id=k;https://bad\x07"[..],
        &b"\x1b]8;;https://bad\x01uri\x07"[..],
        &b"\x1b]8;;https://bad\xffuri\x07"[..],
    ] {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        drive(&mut p, &mut g, b"\x1b]8;;https://good\x07a");
        drive(&mut p, &mut g, reject);
        drive(&mut p, &mut g, b"b");
        assert!(g.cell(0, 0).unwrap().link.is_some());
        assert_eq!(
            g.cell(0, 1).unwrap().link,
            None,
            "{reject:?} left the previous link on the pen",
        );
    }
}

#[test]
fn can_or_sub_cancelling_an_osc_8_leaves_the_open_link_on_the_pen() {
    for cancel in [0x18_u8, 0x1A] {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        drive(&mut p, &mut g, b"\x1b]8;;https://good\x07a");
        drive(&mut p, &mut g, &[b'\x1b', b']', b'8', b';', cancel]);
        drive(&mut p, &mut g, b"b");
        let open = g.cell(0, 0).unwrap().link;
        assert!(open.is_some());
        assert_eq!(
            g.cell(0, 1).unwrap().link,
            open,
            "cancel byte {cancel:#04x}"
        );
    }
}

/// A second subscriber attaching later resolves the same handles as the
/// first.
#[test]
fn resolving_a_registry_id_does_not_consume_it() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 8);
    drive(&mut p, &mut g, b"\x1b]8;;https://u1\x07a");
    let link = g.cell(0, 0).expect("cell").link.expect("linked");
    assert_eq!(g.hyperlink(link).expect("entry").uri.as_str(), "https://u1");
    let cluster = g.intern_cluster("e\u{0301}").expect("intern");

    drive(&mut p, &mut g, b"\x1b]8;;https://u2\x07b");
    assert_eq!(g.hyperlink_count(), 2);
    assert_eq!(g.hyperlink(link).expect("entry").uri.as_str(), "https://u1");
    assert_eq!(g.cluster_str(cluster), Some("e\u{0301}"));
}

#[test]
fn osc_2_sets_window_title() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;hello-felis\x1b\\");
    assert_eq!(g.title(), Some("hello-felis"));
    assert_eq!(g.take_title_dirty().as_deref(), Some("hello-felis"));
    assert!(g.take_title_dirty().is_none(), "second call clears flag");
}

#[test]
fn osc_0_also_sets_the_window_title() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]0;via-osc-0\x07");
    assert_eq!(g.title(), Some("via-osc-0"));
}

#[test]
fn osc_1_sets_the_icon_name_but_not_the_title() {
    // The icon name is observable through `CSI 20 t` (reported as
    // `OSC L … ST`).
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]1;icon-only\x07");
    assert_eq!(g.title(), None);
    drive(&mut p, &mut g, b"\x1b[20t");
    let reply = responses(&mut g).into_iter().next().unwrap();
    assert_eq!(String::from_utf8(reply).unwrap(), "\x1b]Licon-only\x1b\\");
}

#[test]
fn osc_0_sets_the_icon_name_as_well_as_the_title() {
    // The title half is pinned above; the icon half is observable
    // through `CSI 20 t`.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]0;both\x07\x1b[20t");
    let reply = responses(&mut g).into_iter().next().unwrap();
    assert_eq!(String::from_utf8(reply).unwrap(), "\x1b]Lboth\x1b\\");
}

#[test]
fn osc_title_aborted_by_can_does_not_install_partial_value() {
    // `docs/explanation/security-model.md`: a payload truncated by CAN
    // (0x18) or SUB (0x1A) must not install its partial value.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;original\x07");
    assert_eq!(g.title(), Some("original"));
    drive(&mut p, &mut g, b"\x1b]2;half-\x18");
    assert_eq!(g.title(), Some("original"));
    drive(&mut p, &mut g, b"\x1b]2;rogue\x1a");
    assert_eq!(g.title(), Some("original"));
}

#[test]
fn osc_title_only_marks_dirty_when_value_changes() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]2;same\x07");
    assert!(g.take_title_dirty().is_some());
    drive(&mut p, &mut g, b"\x1b]2;same\x07");
    assert!(g.take_title_dirty().is_none());
    drive(&mut p, &mut g, b"\x1b]2;changed\x07");
    assert_eq!(g.take_title_dirty().as_deref(), Some("changed"));
}

#[test]
fn osc_2_invalid_utf8_is_rejected() {
    // `sanitize_osc_str` returns `None` on `from_utf8` failure; a "lossy
    // from_utf8" refactor (replacement-char title) must surface here.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    // 0xC0 is invalid as a UTF-8 leading byte.
    drive(&mut p, &mut g, b"\x1b]2;valid\xc0tail\x07");
    assert_eq!(g.title(), None);
}

#[test]
fn osc_7_records_cwd_url() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(
        &mut p,
        &mut g,
        b"\x1b]7;file://localhost/home/user/proj\x1b\\",
    );
    assert_eq!(g.cwd(), Some("file://localhost/home/user/proj"));
    assert_eq!(
        g.take_cwd_dirty().as_deref(),
        Some("file://localhost/home/user/proj"),
    );
    assert!(g.take_cwd_dirty().is_none(), "second take clears");
}

#[test]
fn osc_7_with_control_byte_is_rejected() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]7;file://localhost/oops\x01tail\x07");
    assert_eq!(g.cwd(), None);
}

#[test]
fn osc_7_idempotent_on_same_value() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    drive(&mut p, &mut g, b"\x1b]7;file:///a\x07");
    assert!(g.take_cwd_dirty().is_some());
    drive(&mut p, &mut g, b"\x1b]7;file:///a\x07");
    assert!(g.take_cwd_dirty().is_none());
    drive(&mut p, &mut g, b"\x1b]7;file:///b\x07");
    assert_eq!(g.take_cwd_dirty().as_deref(), Some("file:///b"));
}

#[test]
fn osc_133_a_records_a_prompt_start_mark_at_cursor_line() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    // With no scrolling yet the absolute line equals the cursor row
    // (docs/explanation/data-model/scrollback.md).
    drive(&mut p, &mut g, b"\x1b[3;1H");
    drive(&mut p, &mut g, b"\x1b]133;A\x07");
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].line, 2);
    assert_eq!(marks[0].kind, PromptKind::PromptStart);
    assert_eq!(marks[0].exit_code, None);
}

#[test]
fn osc_133_d_with_exit_code_is_parsed() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b]133;D;42\x07");
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].kind, PromptKind::CommandEnd);
    assert_eq!(marks[0].exit_code, Some(42));
}

/// A later bare `D` must yield `None` rather than fall back to an older
/// command's code.
#[test]
fn last_command_exit_reads_only_the_youngest_d_mark() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    assert_eq!(g.last_command_exit(), None, "no marks yet");
    drive(&mut p, &mut g, b"\x1b]133;D;42\x07\r\n");
    assert_eq!(g.last_command_exit(), Some(42));
    drive(&mut p, &mut g, b"\x1b]133;A\x07\r\n");
    assert_eq!(g.last_command_exit(), Some(42));
    drive(&mut p, &mut g, b"\x1b]133;D\x07\r\n");
    assert_eq!(g.last_command_exit(), None, "bare D shadows the older 42");
}

#[test]
fn osc_133_unknown_kind_is_ignored() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b]133;Q\x07");
    assert_eq!(g.prompt_marks(), []);
}

/// Four prompt-start marks at absolute lines 0..=3 with 3 rows scrolled
/// into scrollback, so the marks span the scrollback/screen boundary
/// (`docs/explanation/data-model/scrollback.md` "Prompt marks (OSC
/// 133)").
fn grid_with_four_prompts() -> Grid {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    // A@0; LF; A@1; LF; A@2; LF→scroll; A@3; LF→scroll; LF→scroll.
    drive(
        &mut p,
        &mut g,
        b"\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\r\n",
    );
    assert_eq!(g.scrollback_total_pushed(), 3);
    let lines: Vec<u64> = g
        .prompt_marks()
        .iter()
        .filter(|m| m.kind == PromptKind::PromptStart)
        .map(|m| m.line)
        .collect();
    assert_eq!(lines, vec![0, 1, 2, 3]);
    g
}

#[test]
fn prompt_jump_previous_walks_up_through_scrollback() {
    use felis_protocol::messages::PromptJump;
    let g = grid_with_four_prompts();
    // Each Previous lands the next prompt up at the top composed row
    // (viewport = tp - line).
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), Some(1)); // → line 2 at top
    assert_eq!(g.prompt_jump_target(1, PromptJump::Previous), Some(2)); // → line 1
    assert_eq!(g.prompt_jump_target(2, PromptJump::Previous), Some(3)); // → line 0
    // At the oldest prompt: hard clamp, no wrap.
    assert_eq!(g.prompt_jump_target(3, PromptJump::Previous), None);
}

#[test]
fn prompt_jump_next_walks_down_toward_the_live_bottom() {
    use felis_protocol::messages::PromptJump;
    let g = grid_with_four_prompts();
    assert_eq!(g.prompt_jump_target(3, PromptJump::Next), Some(2)); // → line 1
    assert_eq!(g.prompt_jump_target(2, PromptJump::Next), Some(1)); // → line 2
    // The last prompt sits on the live screen, so the viewport clamps
    // to 0.
    assert_eq!(g.prompt_jump_target(1, PromptJump::Next), Some(0));
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}

#[test]
fn prompt_jump_is_disabled_on_the_alternate_screen() {
    use felis_protocol::messages::PromptJump;
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(&mut p, &mut g, b"\x1b]133;A\x07\r\n\x1b]133;A\x07");
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(g.on_alternate_screen());
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), None);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}

#[test]
fn prompt_jump_with_no_marks_is_a_no_op() {
    use felis_protocol::messages::PromptJump;
    let g = Grid::new(3, 10);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), None);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}
#[test]
fn osc9_notification_relays_message_as_body() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;Build finished\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title, None);
    assert_eq!(got[0].body, "Build finished");
    assert_eq!(
        g.take_notifications(),
        Vec::<felis_vt::notification::Notification>::new()
    );
}

#[test]
fn conemu_osc9_4_progress_is_not_a_notification() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    // ConEmu progress is window chrome.
    drive(&mut p, &mut g, b"\x1b]9;4;50\x07");
    assert_eq!(
        g.take_notifications(),
        Vec::<felis_vt::notification::Notification>::new()
    );
}

/// The `ConEmu` filter fires only on the 3+-segment `9 ; <digit> ; …`
/// shape; a `params.len() >= 3 && …` guard flipped to `||` would
/// swallow this two-segment message.
#[test]
fn osc9_single_digit_message_is_relayed_not_filtered() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;5\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "5");
}

/// `ConEmu` chrome requires a single-digit first segment; the inner
/// `p.len() == 1 && digit` guard flipped to `||` would drop this.
#[test]
fn osc9_three_segments_with_non_digit_first_is_relayed() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;X;y\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "X;y");
}

#[test]
fn osc777_notify_relays_title_and_body() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]777;notify;Title;Body text\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Title"));
    assert_eq!(got[0].body, "Body text");
}

#[test]
fn osc99_single_shot_relays_title() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;;Hello\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Hello"));
    assert_eq!(got[0].body, "");
}

#[test]
fn osc99_chunks_reassemble_across_advances() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    // Each chunk in a separate advance, sharing i=abc.
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=0:p=title;Part one\x1b\\");
    assert!(
        g.take_notifications().is_empty(),
        "an unfinished chunk must not relay"
    );
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=0:p=body;the body\x1b\\");
    assert_eq!(
        g.take_notifications(),
        Vec::<felis_vt::notification::Notification>::new()
    );
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=1:p=body; more\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Part one"));
    assert_eq!(got[0].body, "the body more");
    assert_eq!(got[0].id.as_deref(), Some("abc"));
}

#[test]
fn osc99_chunk_at_the_specs_full_encoded_payload_size_reassembles() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    // "YWFh" is Base64 for "aaa", so 1024 groups is exactly the
    // notification spec's 4096-byte per-chunk encoded payload; the
    // `99;i=…:d=0:e=1:p=title;` prefix rides on top of it inside the
    // parser's OSC buffer.
    let encoded = "YWFh".repeat(1024);
    drive(
        &mut p,
        &mut g,
        format!("\x1b]99;i=big:d=0:e=1:p=title;{encoded}\x1b\\").as_bytes(),
    );
    assert_eq!(
        g.take_notifications(),
        Vec::<felis_vt::notification::Notification>::new()
    );
    drive(&mut p, &mut g, b"\x1b]99;i=big:d=1:e=1:p=title;\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("a".repeat(3072).as_str()));
}

#[test]
fn osc99_support_query_gets_a_truthful_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;i=7:p=?;\x1b\\");
    // The reply goes on the response channel, not the notification
    // outbox.
    assert_eq!(
        g.take_notifications(),
        Vec::<felis_vt::notification::Notification>::new()
    );
    let replies = responses(&mut g);
    assert_eq!(replies.len(), 1);
    let reply = String::from_utf8(replies[0].clone()).unwrap();
    // Advertises title/body + the three urgencies; omits actions (a)
    // and close events (c).
    assert_eq!(reply, "\x1b]99;i=7:p=?;p=title,body:u=0,1,2\x1b\\");
}

#[test]
fn osc_11_query_reports_the_client_configured_background() {
    // A light/dark detector (neovim, modern CLIs) queries `OSC 11 ; ?`;
    // an xterm-white reply on felis's dark surface picks a light
    // colorscheme.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(&mut p, &mut g, b"\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:0d0d/0d0d/1212\x1b\\".to_vec()]);
}

#[test]
fn osc_10_and_12_queries_report_configured_fg_and_cursor() {
    // OSC 10 (fg) and OSC 12 (cursor) share OSC 11's latent bug.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Foreground, Some((0xE5, 0xE5, 0xE5)));
    g.set_theme_config_default(ThemeChannel::Cursor, Some((0xFF, 0xAA, 0x00)));
    drive(&mut p, &mut g, b"\x1b]10;?\x07\x1b]12;?\x07");
    let replies = responses(&mut g);
    assert_eq!(
        replies,
        vec![
            b"\x1b]10;rgb:e5e5/e5e5/e5e5\x1b\\".to_vec(),
            b"\x1b]12;rgb:ffff/aaaa/0000\x1b\\".to_vec(),
        ]
    );
}

#[test]
fn runtime_osc_set_wins_over_the_configured_default() {
    // A colorscheme-toggling shell function must still be able to
    // change the reported background.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(&mut p, &mut g, b"\x1b]11;#abcdef\x07\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:abab/cdcd/efef\x1b\\".to_vec()]);
}

#[test]
fn osc_111_reset_falls_back_to_the_configured_default_not_xterm_white() {
    // `OSC 111` reverts to this terminal's default: the client's
    // configured background once one is attached, not xterm's white.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_theme_config_default(ThemeChannel::Background, Some((0x0D, 0x0D, 0x12)));
    drive(
        &mut p,
        &mut g,
        b"\x1b]11;#ffffff\x07\x1b]111\x07\x1b]11;?\x07",
    );
    let replies = responses(&mut g);
    assert_eq!(
        replies.last().unwrap(),
        &b"\x1b]11;rgb:0d0d/0d0d/1212\x1b\\".to_vec()
    );
}

#[test]
fn osc_11_query_without_a_configured_default_keeps_the_xterm_baseline() {
    // The esctest path drives a bare grid; `test_ResetSpecialColor_Dynamic`
    // pins the xterm baseline (white bg).
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]11;?\x07");
    let replies = responses(&mut g);
    assert_eq!(replies, vec![b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".to_vec()]);
}

/// A mutant returning the constant `1` or leaking the table fails the
/// second assertion.
#[test]
fn cluster_count_and_table_track_interned_entries() {
    let mut g = Grid::new(1, 8);
    assert_eq!(g.cluster_count(), 0);
    let id1 = g.intern_cluster("e\u{0301}").expect("intern");
    let id2 = g.intern_cluster("o\u{0308}").expect("intern");
    assert_eq!(g.cluster_count(), 2);
    let table = g.cluster_table();
    assert_eq!(table.len(), 2);
    assert_eq!(table.get(id1), Some("e\u{0301}"));
    assert_eq!(table.get(id2), Some("o\u{0308}"));
}

/// Installing at a sparse id spans the table to `id` and leaves the ids
/// below it absent: a visible-first rehydrate ships the entries its
/// rows name, so the skipped ids are entries still in flight.
#[test]
fn install_cluster_places_text_at_the_daemon_assigned_id() {
    use std::num::NonZeroU32;
    let mut g = Grid::new(1, 8);
    let id = NonZeroU32::new(3).unwrap();
    g.install_cluster(id, ClusterText::new("zz").expect("under cap"));
    assert_eq!(g.cluster_count(), 3, "the table spans to the installed id");
    assert_eq!(g.cluster_str(id), Some("zz"));
    assert_eq!(g.cluster_str(NonZeroU32::new(1).unwrap()), None);
    assert_eq!(g.cluster_str(NonZeroU32::new(2).unwrap()), None);
    g.install_cluster(
        NonZeroU32::new(2).unwrap(),
        ClusterText::new("mid").expect("under cap"),
    );
    assert_eq!(g.cluster_count(), 3);
    assert_eq!(g.cluster_str(NonZeroU32::new(2).unwrap()), Some("mid"));
    assert_eq!(g.cluster_str(NonZeroU32::new(1).unwrap()), None);
}

/// The `payload.len() < 2` guard returns before indexing `payload[1]`;
/// flipped to `>` a one-segment payload indexes out of bounds.
#[test]
fn osc_52_single_segment_is_a_noop() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c\x07");
    assert!(g.take_pending_clipboard_set().is_none());
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
}

/// The tolerated OSC families leave nothing a program can observe: no
/// reply, no title change, no theme override
/// (`docs/reference/protocols/vt-compliance.md` "Consumed without
/// effect").
#[test]
fn tolerated_osc_families_leave_no_state_and_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]0;kept\x07");

    for seq in [
        &b"\x1b]13;rgb:ff/00/00\x07"[..],
        b"\x1b]17;#abc\x07",
        b"\x1b]19;?\x07",
        b"\x1b]633;A\x07",
        b"\x1b]1337;CurrentDir=/tmp\x07",
    ] {
        drive(&mut p, &mut g, seq);
        assert!(responses(&mut g).is_empty(), "reply to {seq:?}");
    }

    assert_eq!(g.title(), Some("kept"));
    for channel in [
        ThemeChannel::Foreground,
        ThemeChannel::Background,
        ThemeChannel::Cursor,
    ] {
        assert_eq!(g.theme_override(channel), None);
    }
}

/// A refused color spec (colorimetric space or `rgbi:`) is dropped at
/// the dispatch layer: the addressed palette slot keeps the value it
/// already had and no reply goes out, even with a `?` query in the
/// same sequence
/// (`docs/reference/protocols/vt-compliance.md` "OSC").
#[test]
fn osc_4_and_5_refused_spec_keeps_the_slot_and_sends_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b]4;1;#aabbcc\x1b\\\x1b]5;0;#112233\x1b\\",
    );
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());

    for seq in [
        &b"\x1b]4;1;CIELab:1/1/1\x1b\\"[..],
        b"\x1b]4;1;rgbi:1.0/0.5/0.0\x1b\\",
        b"\x1b]4;1;TekHVC:0/0/0\x1b\\",
        b"\x1b]5;0;CIEXYZ:0.1/0.2/0.3\x1b\\",
    ] {
        drive(&mut p, &mut g, seq);
        assert!(responses(&mut g).is_empty(), "reply to {seq:?}");
    }

    assert_eq!(
        g.palette_overrides().collect::<Vec<_>>(),
        vec![(1, (0xaa, 0xbb, 0xcc))]
    );
    drive(&mut p, &mut g, b"\x1b]4;1;?\x1b\\\x1b]5;0;?\x1b\\");
    assert_eq!(
        responses(&mut g),
        vec![
            b"\x1b]4;1;rgb:aaaa/bbbb/cccc\x1b\\".to_vec(),
            b"\x1b]5;0;rgb:1111/2222/3333\x1b\\".to_vec(),
        ]
    );
}

/// The same refusal on the dynamic colors: OSC 10-12 keep the current
/// override and answer nothing when the spec is colorimetric.
#[test]
fn osc_10_refused_spec_keeps_the_override_and_sends_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]10;#aabbcc\x1b\\");

    drive(
        &mut p,
        &mut g,
        b"\x1b]10;CIELuv:1/1/1\x1b\\\x1b]11;rgbi:0/0/0\x1b\\",
    );
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(
        g.theme_override(ThemeChannel::Foreground),
        Some((0xaa, 0xbb, 0xcc))
    );
    assert_eq!(g.theme_override(ThemeChannel::Background), None);
}
