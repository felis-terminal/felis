use super::*;

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
