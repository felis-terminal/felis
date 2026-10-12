use super::limits::{
    APP_MAX, ID_DEPTH_MAX, ID_MAX, ID_SEGMENT_MAX, KEY_MAX, MSG_DECODED_MAX, MSG_ENCODED_MAX,
    SEQUENCE_MAX, TITLE_DECODED_MAX, TITLE_ENCODED_MAX,
};
use super::{DropReason, Field, Kind, Osc7501, Report, State, Terminator, parse_osc7501};

/// Independent of `felis_protocol::base64`, which the decoder uses.
fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

fn b64_unpadded(bytes: &[u8]) -> String {
    b64(bytes).trim_end_matches('=').to_owned()
}

fn parse(payload: &str) -> Osc7501 {
    parse_osc7501(format!("7501;{payload}").as_bytes(), Terminator::St).unwrap()
}

#[track_caller]
fn report(payload: &str) -> Report {
    match parse(payload) {
        Osc7501::Report(r) => r,
        other => panic!("expected a report for {payload:?}, got {other:?}"),
    }
}

#[track_caller]
fn dropped(payload: &str) -> DropReason {
    match parse(payload) {
        Osc7501::Dropped(reason) => reason,
        other => panic!("expected a drop for {payload:?}, got {other:?}"),
    }
}

fn root(state: State) -> Report {
    Report {
        id: String::new(),
        state,
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
    }
}

/// A payload whose `ESC ] … ST` sequence is exactly `wire_len` bytes.
fn payload_with_wire_len(wire_len: usize, terminator: Terminator) -> String {
    let overhead = 2 + "7501;".len() + if terminator == Terminator::Bel { 1 } else { 2 };
    let head = "state=idle:pad=";
    format!("{head}{}", "a".repeat(wire_len - overhead - head.len()))
}

#[test]
fn b64_oracle_matches_rfc4648_vectors() {
    assert_eq!(b64(b""), "");
    assert_eq!(b64(b"f"), "Zg==");
    assert_eq!(b64(b"fo"), "Zm8=");
    assert_eq!(b64(b"foo"), "Zm9v");
    assert_eq!(b64(b"foobar"), "Zm9vYmFy");
}

// --- Framing -------------------------------------------------------------

#[test]
fn other_codes_are_not_ours() {
    for body in [
        &b"750;state=idle"[..],
        b"75010;state=idle",
        b"7501x;state=idle",
        b"",
        b"99;",
    ] {
        assert_eq!(parse_osc7501(body, Terminator::St), None, "{body:?}");
    }
}

#[test]
fn bare_code_without_payload_has_no_state() {
    assert_eq!(
        parse_osc7501(b"7501", Terminator::St),
        Some(Osc7501::Dropped(DropReason::MissingState))
    );
    assert_eq!(dropped(""), DropReason::MissingState);
}

#[test]
fn question_mark_payload_is_a_query() {
    assert_eq!(parse("?"), Osc7501::Query);
    assert_eq!(parse(" \t? "), Osc7501::Query);
}

#[test]
fn question_mark_followed_by_pairs_is_a_report_not_a_query() {
    assert_eq!(report("?:state=idle"), root(State::Idle));
}

#[test]
fn sequence_length_cap_counts_introducer_and_terminator() {
    for terminator in [Terminator::St, Terminator::Bel] {
        let at_cap = payload_with_wire_len(SEQUENCE_MAX, terminator);
        let over = payload_with_wire_len(SEQUENCE_MAX + 1, terminator);
        assert!(
            matches!(
                parse_osc7501(format!("7501;{at_cap}").as_bytes(), terminator),
                Some(Osc7501::Report(_))
            ),
            "{terminator:?} at cap"
        );
        assert_eq!(
            parse_osc7501(format!("7501;{over}").as_bytes(), terminator),
            Some(Osc7501::Dropped(DropReason::TooLong)),
            "{terminator:?} over cap"
        );
    }
}

#[test]
fn bel_terminated_sequence_admits_one_more_body_byte_than_st() {
    let payload = payload_with_wire_len(SEQUENCE_MAX, Terminator::Bel);
    let body = format!("7501;{payload}");
    assert!(matches!(
        parse_osc7501(body.as_bytes(), Terminator::Bel),
        Some(Osc7501::Report(_))
    ));
    assert_eq!(
        parse_osc7501(body.as_bytes(), Terminator::St),
        Some(Osc7501::Dropped(DropReason::TooLong))
    );
}

#[test]
fn over_long_query_is_dropped() {
    let body = format!("7501;{}?", " ".repeat(SEQUENCE_MAX));
    assert_eq!(
        parse_osc7501(body.as_bytes(), Terminator::St),
        Some(Osc7501::Dropped(DropReason::TooLong))
    );
}

// --- Spec examples ---------------------------------------------------------

#[test]
fn spec_introduction_example() {
    let r = report(
        "state=blocked:kind=permission:app=terraform:\
         msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/",
    );
    assert_eq!(
        r,
        Report {
            kind: Some(Kind::Permission),
            app: Some("terraform".into()),
            msg: Some("Apply 3 to add, 1 to change, 0 to destroy?".into()),
            ..root(State::Blocked)
        }
    );
}

#[test]
fn spec_one_program_example() {
    let reports = [
        "state=working:app=brew:msg=SW5zdGFsbGluZyB1cGRhdGVz",
        "state=blocked:kind=auth:app=brew:msg=UGFzc3dvcmQgcmVxdWlyZWQgdG8gaW5zdGFsbCB1cGRhdGVz",
        "state=done:app=brew:msg=VXBncmFkZWQgMTIgcGFja2FnZXM=",
    ]
    .map(report);
    let brew = |state, kind, msg: &str| Report {
        kind,
        app: Some("brew".into()),
        msg: Some(msg.into()),
        ..root(state)
    };
    assert_eq!(
        reports,
        [
            brew(State::Working, None, "Installing updates"),
            brew(
                State::Blocked,
                Some(Kind::Auth),
                "Password required to install updates"
            ),
            brew(State::Done, None, "Upgraded 12 packages"),
        ]
    );
}

#[test]
fn spec_several_records_example() {
    assert_eq!(
        report("state=working:id=us-east:title=VVMgRWFzdA==:progress=40:msg=UHVzaGluZyBpbWFnZQ=="),
        Report {
            id: "us-east".into(),
            progress: Some(40),
            title: Some("US East".into()),
            msg: Some("Pushing image".into()),
            ..root(State::Working)
        }
    );
    assert_eq!(
        report(
            "state=blocked:kind=permission:id=eu-west:title=RVUgV2VzdA==:\
             msg=QXBwcm92ZSBkZXBsb3kgdG8gZXUtd2VzdCAocHJvZHVjdGlvbik/"
        ),
        Report {
            id: "eu-west".into(),
            kind: Some(Kind::Permission),
            title: Some("EU West".into()),
            msg: Some("Approve deploy to eu-west (production)?".into()),
            ..root(State::Blocked)
        }
    );
    assert_eq!(
        parse("state=clear:id=us-east"),
        Osc7501::Clear {
            id: "us-east".into()
        }
    );
}

// --- Rule 0: whitespace ------------------------------------------------------

#[test]
fn whitespace_around_keys_and_values_is_trimmed() {
    assert_eq!(
        report(" state = blocked\t:\tkind =auth : app= cargo "),
        Report {
            kind: Some(Kind::Auth),
            app: Some("cargo".into()),
            ..root(State::Blocked)
        }
    );
}

#[test]
fn whitespace_inside_a_value_makes_the_pair_malformed() {
    assert_eq!(dropped("state=work ing"), DropReason::MissingState);
    assert_eq!(report("state=idle:app=car go").app, None);
}

// --- Rule 1: malformed pairs, unknown keys, last wins -------------------------

#[test]
fn malformed_pairs_are_skipped() {
    let expected = Report {
        app: Some("cargo".into()),
        ..root(State::Idle)
    };
    for payload in [
        "state=idle:app=cargo:noequals",
        "state=idle::app=cargo",
        "state=idle:=x:app=cargo",
        "state=idle:App=x:app=cargo",
        "state=idle:ap1=x:app=cargo",
        "state=idle:app=cargo:app=bad!",
        "state=idle:app=cargo:app=caf\u{e9}",
    ] {
        assert_eq!(report(payload), expected, "{payload:?}");
    }
}

#[test]
fn a_known_key_spelled_in_uppercase_is_not_that_key() {
    assert_eq!(dropped("STATE=idle"), DropReason::MissingState);
}

#[test]
fn unknown_keys_are_ignored() {
    assert_eq!(
        report("future=x:state=working:zzz=1,2/3+4.5_6-7=8"),
        root(State::Working)
    );
}

#[test]
fn key_length_cap_applies_to_every_key() {
    let at_cap = "k".repeat(KEY_MAX);
    let over = "k".repeat(KEY_MAX + 1);
    assert_eq!(report(&format!("{at_cap}=x:state=idle")), root(State::Idle));
    assert_eq!(
        dropped(&format!("{over}=x:state=idle")),
        DropReason::KeyTooLong
    );
    // Over the cap wins over malformed: a limit is checked first.
    assert_eq!(
        dropped(&format!("{}=x:state=idle", "K".repeat(KEY_MAX + 1))),
        DropReason::KeyTooLong
    );
    assert_eq!(
        dropped(&format!("state=idle:{over}=b@d")),
        DropReason::KeyTooLong
    );
}

#[test]
fn the_last_well_formed_occurrence_wins() {
    assert_eq!(report("state=done:state=working"), root(State::Working));
    assert_eq!(
        dropped("state=working:state=bogus"),
        DropReason::UnknownState
    );
    assert_eq!(report("state=working:state=bad!"), root(State::Working));
    assert_eq!(report("state=idle:id=a:id=b").id, "b");
    assert_eq!(
        report("state=idle:msg=Zmlyc3Q=:msg=c2Vjb25k")
            .msg
            .as_deref(),
        Some("second")
    );
}

// --- Rule 2: soft fields fall back to absent ---------------------------------

#[test]
fn kind_is_kept_only_with_blocked() {
    for kind in Kind::ALL {
        let r = report(&format!("state=blocked:kind={}", kind.as_str()));
        assert_eq!(r.kind, Some(kind));
    }
    for state in ["idle", "working", "done", "error"] {
        assert_eq!(
            report(&format!("state={state}:kind=auth")).kind,
            None,
            "{state}"
        );
    }
}

#[test]
fn unrecognized_kind_is_absent() {
    for kind in ["other", "Auth", "", "auth,question"] {
        assert_eq!(
            report(&format!("state=blocked:kind={kind}")).kind,
            None,
            "{kind:?}"
        );
    }
}

#[test]
fn progress_is_kept_only_with_working_or_blocked() {
    assert_eq!(report("state=working:progress=40").progress, Some(40));
    assert_eq!(report("state=blocked:progress=40").progress, Some(40));
    for state in ["idle", "done", "error"] {
        assert_eq!(
            report(&format!("state={state}:progress=40")).progress,
            None,
            "{state}"
        );
    }
}

#[test]
fn progress_range_is_zero_to_one_hundred() {
    for (value, expected) in [
        ("0", Some(0)),
        ("100", Some(100)),
        ("007", Some(7)),
        ("101", None),
        ("255", None),
        ("256", None),
        ("99999999999999999999999", None),
        ("", None),
        ("+5", None),
        ("-0", None),
        ("1.0", None),
        ("4O", None),
    ] {
        assert_eq!(
            report(&format!("state=working:progress={value}")).progress,
            expected,
            "{value:?}"
        );
    }
}

#[test]
fn app_outside_its_charset_is_absent() {
    for app in ["", "a,b", "a/b", "a=b"] {
        assert_eq!(
            report(&format!("state=idle:app={app}")).app,
            None,
            "{app:?}"
        );
    }
    assert_eq!(
        report("state=idle:app=claude-code_1.2+x").app.as_deref(),
        Some("claude-code_1.2+x")
    );
}

#[test]
fn app_length_cap_drops_the_report() {
    let at_cap = "a".repeat(APP_MAX);
    assert_eq!(
        report(&format!("state=idle:app={at_cap}")).app,
        Some(at_cap)
    );
    assert_eq!(
        dropped(&format!("state=idle:app={}", "a".repeat(APP_MAX + 1))),
        DropReason::FieldTooLong(Field::App)
    );
    // Over the cap even though the charset would only make it absent.
    assert_eq!(
        dropped(&format!("state=idle:app={}", ",".repeat(APP_MAX + 1))),
        DropReason::FieldTooLong(Field::App)
    );
}

// --- Rule 3: state and id gate the whole report -------------------------------

#[test]
fn every_known_state_is_accepted() {
    for state in State::ALL {
        assert_eq!(report(&format!("state={}", state.as_str())), root(state));
    }
}

#[test]
fn missing_or_unknown_state_drops_the_report() {
    assert_eq!(dropped("app=cargo"), DropReason::MissingState);
    assert_eq!(dropped("state=paused"), DropReason::UnknownState);
    assert_eq!(dropped("state="), DropReason::UnknownState);
    assert_eq!(dropped("state=Idle"), DropReason::UnknownState);
}

#[test]
fn absent_id_addresses_the_root_record() {
    assert_eq!(report("state=idle").id, "");
}

#[test]
fn id_grammar_accepts_nested_segments() {
    assert_eq!(
        report("state=idle:id=build/test.unit+1_x-y").id,
        "build/test.unit+1_x-y"
    );
}

#[test]
fn id_that_fails_the_grammar_drops_instead_of_falling_back_to_root() {
    for id in [
        "",
        "/",
        "a/",
        "/a",
        "a//b",
        "a,b",
        "a=b",
        "a b",
        "a@b",
        "caf\u{e9}",
    ] {
        assert_eq!(
            dropped(&format!("state=idle:id={id}")),
            DropReason::BadId,
            "{id:?}"
        );
    }
}

#[test]
fn id_limits_drop_the_report() {
    let seg = "s".repeat(ID_SEGMENT_MAX);
    assert_eq!(report(&format!("state=idle:id={seg}")).id, seg);
    assert_eq!(
        dropped(&format!("state=idle:id={}", "s".repeat(ID_SEGMENT_MAX + 1))),
        DropReason::FieldTooLong(Field::Id)
    );

    let deepest = ["d"; ID_DEPTH_MAX].join("/");
    assert_eq!(report(&format!("state=idle:id={deepest}")).id, deepest);
    assert_eq!(
        dropped(&format!("state=idle:id={deepest}/d")),
        DropReason::FieldTooLong(Field::Id)
    );

    // Four full segments plus separators overshoot the total, so the last
    // segment is shortened to land exactly on it.
    let last = ID_MAX - 3 * (ID_SEGMENT_MAX + 1);
    let longest = format!("{seg}/{seg}/{seg}/{}", "t".repeat(last));
    assert_eq!(longest.len(), ID_MAX);
    assert_eq!(report(&format!("state=idle:id={longest}")).id, longest);
    assert_eq!(
        dropped(&format!("state=idle:id={longest}t")),
        DropReason::FieldTooLong(Field::Id)
    );
}

#[test]
fn every_id_occurrence_must_be_valid() {
    assert_eq!(dropped("state=idle:id=ok:id=b@d"), DropReason::BadId);
    assert_eq!(dropped("state=idle:id=b@d:id=ok"), DropReason::BadId);
    assert_eq!(dropped("state=idle:id=a//b:id=ok"), DropReason::BadId);
}

#[test]
fn a_malformed_id_pair_is_not_skipped() {
    assert_eq!(dropped("state=idle:id=a b"), DropReason::BadId);
    assert_eq!(dropped("state=idle:id=caf\u{e9}"), DropReason::BadId);
}

#[test]
fn an_id_pair_without_equals_has_no_key_and_is_skipped() {
    assert_eq!(report("state=idle:id"), root(State::Idle));
}

// --- Rule 4: limits, base64 and control characters drop the whole report -----

#[test]
fn msg_encoded_and_decoded_caps() {
    let largest = "m".repeat(MSG_DECODED_MAX);
    let encoded = b64(largest.as_bytes());
    assert_eq!(encoded.len(), MSG_ENCODED_MAX);
    assert_eq!(
        report(&format!("state=idle:msg={encoded}")).msg,
        Some(largest)
    );

    // Unpadded, one byte more still fits the encoded cap.
    let decoded_over = b64(&[b'm'; MSG_DECODED_MAX + 1]);
    assert_eq!(decoded_over.len(), MSG_ENCODED_MAX);
    assert_eq!(
        dropped(&format!("state=idle:msg={decoded_over}")),
        DropReason::FieldTooLong(Field::Msg)
    );

    assert_eq!(
        dropped(&format!("state=idle:msg={encoded}A")),
        DropReason::FieldTooLong(Field::Msg)
    );
}

#[test]
fn msg_encoded_cap_is_checked_before_decoding() {
    let invalid = "A".repeat(MSG_ENCODED_MAX + 1);
    assert_eq!(
        dropped(&format!("state=idle:msg={invalid}")),
        DropReason::FieldTooLong(Field::Msg)
    );
}

#[test]
fn title_encoded_cap() {
    let largest = "t".repeat(TITLE_DECODED_MAX);
    let encoded = b64(largest.as_bytes());
    assert_eq!(encoded.len(), TITLE_ENCODED_MAX);
    assert_eq!(
        report(&format!("state=idle:title={encoded}")).title,
        Some(largest)
    );
    assert_eq!(
        dropped(&format!("state=idle:title={encoded}A")),
        DropReason::FieldTooLong(Field::Title)
    );
}

#[test]
fn base64_padding_is_optional() {
    for encoded in [
        b64(b"ok?"),
        b64(b"ok"),
        b64_unpadded(b"ok"),
        b64_unpadded(b"o"),
    ] {
        assert!(
            report(&format!("state=idle:msg={encoded}")).msg.is_some(),
            "{encoded}"
        );
    }
}

#[test]
fn empty_free_text_is_present_and_empty() {
    let r = report("state=idle:msg=:title=");
    assert_eq!(r.msg.as_deref(), Some(""));
    assert_eq!(r.title.as_deref(), Some(""));
}

#[test]
fn undecodable_base64_drops_the_report() {
    for (key, field) in [("msg", Field::Msg), ("title", Field::Title)] {
        // `-` and `_` are the URL-safe alphabet, which the spec does not admit;
        // `Zg=` is a half-padded tail.
        for value in ["a", "abcde", "ab=c", "a-_b", "Zg="] {
            let payload = format!("state=idle:{key}={value}");
            assert_eq!(dropped(&payload), DropReason::BadBase64(field), "{payload}");
        }
    }
}

#[test]
fn text_that_is_not_utf8_drops_the_report() {
    for bytes in [&[0xFF][..], &[0x80], &[0xC0, 0x80], &[b'o', 0xE4, 0xBD]] {
        for (key, field) in [("msg", Field::Msg), ("title", Field::Title)] {
            assert_eq!(
                dropped(&format!("state=idle:{key}={}", b64(bytes))),
                DropReason::NotUtf8(field),
                "{key} {bytes:?}"
            );
        }
    }
}

#[test]
fn control_characters_drop_the_report_and_their_neighbors_do_not() {
    for c in ['\u{0}', '\t', '\n', '\u{1F}', '\u{7F}', '\u{80}', '\u{9F}'] {
        let encoded = b64(format!("a{c}b").as_bytes());
        for (key, field) in [("msg", Field::Msg), ("title", Field::Title)] {
            assert_eq!(
                dropped(&format!("state=idle:{key}={encoded}")),
                DropReason::ControlChar(field),
                "{key} {c:?}"
            );
        }
    }
    for c in [' ', '~', '\u{A0}', '\u{202E}', '\u{200B}'] {
        let text = format!("a{c}b");
        let encoded = b64(text.as_bytes());
        assert_eq!(
            report(&format!("state=idle:msg={encoded}")).msg,
            Some(text),
            "{c:?}"
        );
    }
}

#[test]
fn an_earlier_bad_occurrence_drops_even_when_a_later_one_is_valid() {
    let ok = b64(b"ok");
    for (payload, reason) in [
        (
            format!("state=idle:app={}:app=ok", "a".repeat(APP_MAX + 1)),
            DropReason::FieldTooLong(Field::App),
        ),
        (
            format!("state=idle:msg=a:msg={ok}"),
            DropReason::BadBase64(Field::Msg),
        ),
        (
            format!("state=idle:msg={}:msg={ok}", b64(b"a\nb")),
            DropReason::ControlChar(Field::Msg),
        ),
        (
            format!("state=idle:msg={}:msg={ok}", b64(&[0xFF])),
            DropReason::NotUtf8(Field::Msg),
        ),
        (
            format!(
                "state=idle:title={}:title={ok}",
                "A".repeat(TITLE_ENCODED_MAX + 1)
            ),
            DropReason::FieldTooLong(Field::Title),
        ),
        (
            format!("state=idle:id={}:id=ok", "s".repeat(ID_SEGMENT_MAX + 1)),
            DropReason::FieldTooLong(Field::Id),
        ),
    ] {
        assert_eq!(dropped(&payload), reason, "{payload}");
    }
}

#[test]
fn a_hard_failure_drops_regardless_of_state() {
    assert_eq!(dropped("msg=a"), DropReason::BadBase64(Field::Msg));
    assert_eq!(
        dropped("state=bogus:msg=a"),
        DropReason::BadBase64(Field::Msg)
    );
}

// --- clear ---------------------------------------------------------------

#[test]
fn clear_without_id_clears_everything() {
    assert_eq!(parse("state=clear"), Osc7501::Clear { id: String::new() });
}

#[test]
fn clear_ignores_soft_fields_but_not_hard_failures() {
    assert_eq!(
        parse("state=clear:id=a/b:kind=auth:progress=5:app=x"),
        Osc7501::Clear { id: "a/b".into() }
    );
    assert_eq!(
        dropped("state=clear:msg=a"),
        DropReason::BadBase64(Field::Msg)
    );
    assert_eq!(dropped("state=clear:id=a//b"), DropReason::BadId);
}

#[test]
fn clear_is_not_a_state() {
    assert!(State::ALL.iter().all(|s| s.as_str() != "clear"));
}
