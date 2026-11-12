use felis_vt::Parser;
use proptest::prelude::*;

use crate::test_support::{drive, responses};
use crate::*;

/// esctest's `GetIndexedColors()` probes `Co` (hex `436f`) to pick the
/// OSC 4 alias offset, so `Co` must answer.
#[test]
fn xtgettcap_reply_covers_co_and_unknown_caps() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q436f\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP1+r436f=323536\x1b\\"
    );
    drive(&mut p, &mut g, b"\x1bP+q7878\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP0+r7878\x1b\\");
}

#[test]
fn xtgettcap_multi_cap_query_joins_replies_and_collapses_status() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q436f;6c69\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP1+r436f=323536;6c69=34\x1b\\"
    );
    drive(&mut p, &mut g, b"\x1bP+q436f;7878\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP0+r436f=323536;7878\x1b\\"
    );
}

#[test]
fn xtgettcap_long_name_dimension_aliases_answer() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q636f6c73\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP1+r636f6c73=38\x1b\\"
    );
    drive(&mut p, &mut g, b"\x1bP+q6c696e6573\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP1+r6c696e6573=34\x1b\\"
    );
}

proptest! {
    #[test]
    fn hex_codec_round_trips_and_emits_lowercase(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let hex = ascii_to_hex(&bytes);
        prop_assert_eq!(hex.len(), bytes.len() * 2);
        prop_assert!(hex.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)));
        prop_assert_eq!(hex_to_ascii(&hex), (!bytes.is_empty()).then(|| bytes.clone()));
        let upper = hex.to_ascii_uppercase();
        prop_assert_eq!(hex_to_ascii(&upper), (!bytes.is_empty()).then_some(bytes));
    }

    #[test]
    fn hex_to_ascii_rejects_odd_length_and_non_hex(
        bytes in prop::collection::vec(any::<u8>(), 1..32),
        stray in any::<u8>(),
    ) {
        let hex = ascii_to_hex(&bytes);
        prop_assert_eq!(hex_to_ascii(&hex[..hex.len() - 1]), None);
        if !stray.is_ascii_hexdigit() {
            let mut poisoned = hex;
            poisoned[0] = stray;
            prop_assert_eq!(hex_to_ascii(&poisoned), None);
        }
    }
}

/// `co` / `li` / `TN` resolve from the live grid in `xtgettcap_reply`,
/// not from the static table.
#[test]
fn xtgettcap_value_covers_each_known_cap() {
    assert_eq!(xtgettcap_value(b"Co"), Some("256"));
    assert_eq!(xtgettcap_value(b"colors"), Some("256"));
    assert_eq!(xtgettcap_value(b"TN"), None);
    assert_eq!(xtgettcap_value(b"name"), None);
    assert_eq!(xtgettcap_value(b"co"), None);
    assert_eq!(xtgettcap_value(b"li"), None);
    assert_eq!(xtgettcap_value(b"zz"), None);
}

#[test]
fn xtgettcap_reply_reports_live_dimensions() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q636f\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1+r636f=38\x1b\\");
    drive(&mut p, &mut g, b"\x1bP+q6c69\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP1+r6c69=34\x1b\\");
    g.resize(30, 100);
    drive(&mut p, &mut g, b"\x1bP+q636f\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP1+r636f=313030\x1b\\"
    );
}

#[test]
fn xtgettcap_tn_and_name_report_the_stamped_term_verbatim() {
    for term in ["xterm-felis", "xterm-kitty"] {
        let mut g = Grid::new(4, 8);
        g.set_term_name(term);
        let mut p = Parser::new();
        let hex = String::from_utf8(ascii_to_hex(term.as_bytes())).unwrap();
        drive(&mut p, &mut g, b"\x1bP+q544e\x1b\\");
        let r = responses(&mut g);
        assert_eq!(
            std::str::from_utf8(&r[0]).unwrap(),
            format!("\x1bP1+r544e={hex}\x1b\\"),
            "TN for {term}"
        );
        drive(&mut p, &mut g, b"\x1bP+q6e616d65\x1b\\");
        let r = responses(&mut g);
        assert_eq!(
            std::str::from_utf8(&r[0]).unwrap(),
            format!("\x1bP1+r6e616d65={hex}\x1b\\"),
            "name for {term}"
        );
    }
}

#[test]
fn xtgettcap_tn_is_unknown_until_the_host_stamps_a_term() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q544e\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP0+r544e\x1b\\");
}

#[test]
fn ris_keeps_the_stamped_term_for_xtgettcap_tn() {
    let mut g = Grid::new(4, 8);
    g.set_term_name("xterm-felis");
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bc");
    assert_eq!(g.term_name(), Some("xterm-felis"));
    drive(&mut p, &mut g, b"\x1bP+q544e\x1b\\");
    let r = responses(&mut g);
    let hex = String::from_utf8(ascii_to_hex(b"xterm-felis")).unwrap();
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        format!("\x1bP1+r544e={hex}\x1b\\")
    );
}

#[test]
fn xtgettcap_undecodable_name_is_answered_empty_not_echoed() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q436f;\nid\n\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        "\x1bP0+r436f=323536;\x1b\\"
    );
}

proptest! {
    /// Whatever the query body, the reply between its `DCS 0+r` /
    /// `DCS 1+r` opener and `ST` is lowercase hex, `;` and `=` alone.
    #[test]
    fn xtgettcap_reply_body_carries_no_producer_bytes(
        body in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut g = Grid::new(4, 8);
        g.xtgettcap_reply(&body);
        let r = responses(&mut g);
        prop_assert_eq!(r.len(), 1);
        let reply = &r[0];
        prop_assert!(reply.starts_with(b"\x1bP0+r") || reply.starts_with(b"\x1bP1+r"));
        prop_assert!(reply.ends_with(b"\x1b\\"));
        let inner = &reply[5..reply.len() - 2];
        prop_assert!(
            inner.iter().all(|b| b.is_ascii_hexdigit() || *b == b';' || *b == b'='),
            "reply leaked {:?}",
            String::from_utf8_lossy(reply)
        );
    }
}

/// `xtgettcap_reply` echoes each cap's hex verbatim, so a reply carrying
/// the trailing `co` proves the last byte before the limit survived.
#[test]
fn xtgettcap_body_exactly_at_the_dcs_buffer_limit_is_answered_intact() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    // The filler is one unknown (odd-length, so un-decodable) cap.
    let mut body = vec![b'0'; DCS_BUFFER_LIMIT - 5];
    body.extend_from_slice(b";636f");
    assert_eq!(body.len(), DCS_BUFFER_LIMIT);

    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);

    let reply = responses(&mut g)
        .into_iter()
        .next()
        .expect("XTGETTCAP reply");
    assert!(
        reply.ends_with(b";636f=3830\x1b\\"),
        "reply lost the cap at the limit: {:?}",
        String::from_utf8_lossy(&reply)
    );
}

/// One byte past the limit the overflow is dropped: the query's final
/// `f` is gone, so `636` cannot decode and the cap is answered empty.
#[test]
fn xtgettcap_body_one_byte_past_the_dcs_buffer_limit_drops_the_overflow() {
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    let mut body = vec![b'0'; DCS_BUFFER_LIMIT - 4];
    body.extend_from_slice(b";636f");
    assert_eq!(body.len(), DCS_BUFFER_LIMIT + 1);

    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);

    let reply = responses(&mut g)
        .into_iter()
        .next()
        .expect("XTGETTCAP reply");
    assert!(
        reply.ends_with(b";\x1b\\"),
        "expected the last body byte dropped, got: {:?}",
        String::from_utf8_lossy(&reply)
    );
    assert!(
        !reply.contains(&b'='),
        "a truncated cap name must not be answered"
    );
}

/// REQ-903: the flooding-producer shape leaves the grid untouched and
/// the parser back in Ground.
#[test]
fn input_after_an_oversized_dcs_body_parses_normally() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    let mut bytes = b"\x1bP+q".to_vec();
    bytes.extend(std::iter::repeat_n(b'6', DCS_BUFFER_LIMIT * 8));
    bytes.extend_from_slice(b"\x1b\\ok");
    drive(&mut p, &mut g, &bytes);

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'o'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'k'));
    assert_eq!(g.cursor().col, 2);
    assert_eq!(g.cursor().row, 0);
}

/// The REQ-903 discard that mirrors `csi_dispatch`: without the `ignore`
/// bail the `$ q` tail would still read as DECRQSS and answer a
/// malformed request.
#[test]
fn dcs_with_overflowed_intermediates_is_dropped_without_a_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    // Four intermediates against `MAX_INTERMEDIATES = 3`.
    drive(&mut p, &mut g, b"\x1bP$$$$qm\x1b\\ok");

    assert!(
        responses(&mut g).is_empty(),
        "an overflowed DCS opener must not be answered"
    );
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'o'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Ascii(b'k'));
}
