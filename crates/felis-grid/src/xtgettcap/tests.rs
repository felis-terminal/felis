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
fn xtgettcap_tn_and_name_report_the_stamped_term() {
    let mut g = Grid::new(4, 8);
    g.set_term_name("xterm-felis");
    let mut p = Parser::new();
    let hex = String::from_utf8(ascii_to_hex(b"xterm-felis")).unwrap();
    drive(&mut p, &mut g, b"\x1bP+q544e\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        format!("\x1bP1+r544e={hex}\x1b\\")
    );
    drive(&mut p, &mut g, b"\x1bP+q6e616d65\x1b\\");
    let r = responses(&mut g);
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        format!("\x1bP1+r6e616d65={hex}\x1b\\")
    );
}

#[test]
fn xtgettcap_tn_reports_a_hatch_term_verbatim() {
    let mut g = Grid::new(4, 8);
    g.set_term_name("xterm-kitty");
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP+q544e\x1b\\");
    let r = responses(&mut g);
    let hex = String::from_utf8(ascii_to_hex(b"xterm-kitty")).unwrap();
    assert_eq!(
        std::str::from_utf8(&r[0]).unwrap(),
        format!("\x1bP1+r544e={hex}\x1b\\")
    );
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
}
