use super::*;

#[test]
fn da1_query_responds_with_xterm_vt420_default_feature_mask() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    // `CSI c` and `CSI 0 c` are equivalent.
    let reply = b"\x1b[?64;1;2;6;9;15;16;17;18;21;22;28;29c".to_vec();
    drive(&mut p, &mut g, b"\x1b[c");
    assert_eq!(responses(&mut g), vec![reply.clone()]);
    drive(&mut p, &mut g, b"\x1b[0c");
    assert_eq!(responses(&mut g), vec![reply]);
}

/// DECID is the legacy alias for DA1; terminfo probes expect the reply
/// byte-for-byte identical.
#[test]
fn decid_esc_z_replies_identically_to_da1() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1bZ");
    assert_eq!(responses(&mut g), vec![DA1_REPLY.to_vec()]);
}

/// Pp=41 per `docs/explanation/protocols/vt-compliance.md` "Reply
/// identity"; Pv=400 sits inside esctest's `[314, 999]` acceptance
/// range.
#[test]
fn da2_query_responds_with_vt420_model_and_in_range_version() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>c");
    assert_eq!(responses(&mut g), vec![b"\x1b[>41;400;0c".to_vec()]);
    // `CSI > 0 c` is the form esccmd's `DA2(0)` emits.
    drive(&mut p, &mut g, b"\x1b[>0c");
    assert_eq!(responses(&mut g), vec![b"\x1b[>41;400;0c".to_vec()]);
}

fn identified_grid() -> Grid {
    let mut g = Grid::new(1, 1);
    g.set_reply_identity(ReplyIdentity::new("felis 9.9.9", "felis-1"));
    g
}

#[test]
fn da3_replies_with_the_hex_of_the_host_unit_id() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    g.set_reply_identity(ReplyIdentity::new("host 1.0", "host-2"));
    drive(&mut p, &mut g, b"\x1b[=0c\x1b[=c");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1bP!|686f73742d32\x1b\\".to_vec(); 2]
    );
}

#[test]
fn da3_without_a_host_identity_replies_with_the_all_zero_unit_id() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[=c");
    assert_eq!(responses(&mut g), vec![b"\x1bP!|00000000\x1b\\".to_vec()]);
}

#[test]
fn xtversion_replies_with_the_host_string_not_the_grid_crate_version() {
    let mut p = Parser::new();
    let mut g = identified_grid();
    drive(&mut p, &mut g, b"\x1b[>q\x1b[>0q");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1bP>|felis 9.9.9\x1b\\".to_vec(); 2]
    );
}

#[test]
fn xtversion_without_a_host_identity_is_unanswered_while_da1_still_replies() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>q\x1b[c");
    let replies = responses(&mut g);
    assert_eq!(replies.len(), 1);
    assert!(replies[0].starts_with(b"\x1b[?64;"));
}

#[test]
fn ris_keeps_the_host_reply_identity() {
    let mut p = Parser::new();
    let mut g = identified_grid();
    drive(&mut p, &mut g, b"\x1bc\x1b[>q");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1bP>|felis 9.9.9\x1b\\".to_vec()]
    );
}

#[test]
fn reply_identity_refuses_bytes_that_could_break_the_dcs_frame() {
    assert!(ReplyIdentity::new("felis\x1b\\", "felis-1").is_none());
    assert!(ReplyIdentity::new("felis", "felis\u{9c}").is_none());
    assert!(ReplyIdentity::new("felis\u{e9}", "felis-1").is_none());
    assert!(ReplyIdentity::new("felis 0.3.0", "felis-1").is_some());
}

#[test]
fn dsr_5n_replies_ok() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[5n");
    assert_eq!(responses(&mut g), vec![b"\x1b[0n".to_vec()]);
}

#[test]
fn dsr_6n_replies_with_the_one_based_cursor_position() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[3;5H\x1b[6n");
    assert_eq!(responses(&mut g), vec![b"\x1b[3;5R".to_vec()]);
}

#[test]
fn multiple_queries_in_one_chunk_each_get_their_own_response() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[>1u\x1b[?u\x1b[>2u\x1b[?u");
    let responses = responses(&mut g);
    assert_eq!(responses, vec![b"\x1b[?1u".to_vec(), b"\x1b[?2u".to_vec()]);
}

#[test]
fn private_dsr_15_reports_no_printer() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?15n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?13n".to_vec()]);
}

#[test]
fn private_dsr_25_reports_udk_locked() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?25n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?21n".to_vec()]);
}

#[test]
fn private_dsr_26_reports_keyboard_status() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b[?26n");
    assert_eq!(responses(&mut g), vec![b"\x1b[?27;1;0;0n".to_vec()]);
}

/// `0` is the field default, so a deleted assignment arm would survive
/// a `0` round trip; this sets a non-default value.
#[test]
fn decrqss_decsace_round_trips_a_non_default_value() {
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b[2*x\x1bP$q*x\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1$r2*x\x1b\\");
}
