//! Property: OSC payloads carrying any C0 control byte or DEL must
//! never replace the existing title or cwd.
//!
//! Spec source: `docs/explanation/security-model.md` "OSC 8 hyperlinks and
//! OSC 7 CWD"; the same rule applies to titles.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;
use felis_vt::Parser;
use proptest::prelude::*;

fn osc(code: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + code.len() + 1 + payload.len() + 1);
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(code);
    out.push(b';');
    out.extend_from_slice(payload);
    out.push(0x07);
    out
}

fn drive(grid: &mut Grid, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(grid, bytes);
}

/// C1 ST (0x9C) is not a terminator here: the parser treats it as a
/// UTF-8 data byte.
const fn is_terminator(b: u8) -> bool {
    matches!(b, 0x07 | 0x18 | 0x1A | 0x1B)
}

fn has_non_terminator_control_byte(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .copied()
        .any(|b| !is_terminator(b) && (b < 0x20 || b == 0x7F))
}

proptest! {
    #[test]
    fn osc_title_rejects_any_payload_with_a_control_byte(
        prior in "[a-z]{1,16}",
        use_osc_0 in any::<bool>(),
        payload in proptest::collection::vec(
            (0u8..=255).prop_filter("not a terminator", |b| !is_terminator(*b)),
            1..32,
        ),
    ) {
        prop_assume!(has_non_terminator_control_byte(&payload));
        let code: &[u8] = if use_osc_0 { b"0" } else { b"2" };
        let mut g = Grid::new(1, 1);
        drive(&mut g, &osc(b"2", prior.as_bytes()));
        prop_assert_eq!(g.title(), Some(prior.as_str()));
        drive(&mut g, &osc(code, &payload));
        prop_assert_eq!(g.title(), Some(prior.as_str()));
    }

    #[test]
    fn osc_cwd_rejects_any_payload_with_a_control_byte(
        prior in "[a-z]{1,16}",
        payload in proptest::collection::vec(
            (0u8..=255).prop_filter("not a terminator", |b| !is_terminator(*b)),
            1..32,
        ),
    ) {
        prop_assume!(has_non_terminator_control_byte(&payload));
        let mut g = Grid::new(1, 1);
        drive(&mut g, &osc(b"7", prior.as_bytes()));
        prop_assert_eq!(g.cwd(), Some(prior.as_str()));
        drive(&mut g, &osc(b"7", &payload));
        prop_assert_eq!(g.cwd(), Some(prior.as_str()));
    }

    #[test]
    fn osc_dispatch_never_panics_on_arbitrary_input(
        code in proptest::collection::vec(b'0'..=b'9', 1..4),
        payload in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut g = Grid::new(2, 8);
        drive(&mut g, &osc(&code, &payload));
        prop_assert!(g.rows() == 2 && g.cols() == 8);
    }

    #[test]
    fn osc_title_accepts_clean_utf8_payloads(
        text in "[\\x20-\\x7E]{1,32}",
    ) {
        let mut g = Grid::new(1, 1);
        drive(&mut g, &osc(b"2", text.as_bytes()));
        prop_assert_eq!(g.title(), Some(text.as_str()));
    }

    /// Pins 0x9C-as-data: kanji like `作` (E4 BD 9C) carry the C1 ST
    /// byte mid-character.
    #[test]
    fn osc_title_accepts_arbitrary_printable_unicode(
        text in "\\PC{1,32}",
        use_osc_0 in any::<bool>(),
    ) {
        let code: &[u8] = if use_osc_0 { b"0" } else { b"2" };
        let mut g = Grid::new(1, 1);
        drive(&mut g, &osc(code, text.as_bytes()));
        prop_assert_eq!(g.title(), Some(text.as_str()));
    }

    #[test]
    fn osc_cwd_accepts_arbitrary_printable_unicode(
        text in "\\PC{1,32}",
    ) {
        let mut g = Grid::new(1, 1);
        drive(&mut g, &osc(b"7", text.as_bytes()));
        prop_assert_eq!(g.cwd(), Some(text.as_str()));
    }

    #[test]
    fn osc_8_dispatch_never_panics_on_arbitrary_input(
        id in proptest::collection::vec(any::<u8>(), 0..32),
        uri in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut payload = Vec::with_capacity(id.len() + 1 + uri.len());
        payload.extend_from_slice(&id);
        payload.push(b';');
        payload.extend_from_slice(&uri);
        let mut g = Grid::new(2, 8);
        drive(&mut g, &osc(b"8", &payload));
        drive(&mut g, b"x");
        prop_assert!(g.rows() == 2 && g.cols() == 8);
    }

    #[test]
    fn osc_8_clean_uri_stamps_link_on_every_inside_cell_and_clears_after(
        path in "[a-zA-Z0-9./:_-]{1,32}",
        inside in 1u16..=8,
    ) {
        let uri = format!("https://{path}");
        let mut g = Grid::new(1, 16);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]8;;");
        bytes.extend_from_slice(uri.as_bytes());
        bytes.push(0x07);
        bytes.extend(std::iter::repeat_n(b'X', usize::from(inside)));
        bytes.extend_from_slice(b"\x1b]8;;\x07Y");
        drive(&mut g, &bytes);
        let id = g.cell(0, 0).expect("cell 0").link;
        prop_assert!(id.is_some(), "uri {uri} did not install a link");
        for c in 1..inside {
            prop_assert_eq!(
                g.cell(0, c).unwrap().link,
                id,
                "link slot drifted at col {}", c,
            );
        }
        prop_assert_eq!(g.cell(0, inside).unwrap().link, None);
    }

    #[test]
    fn osc_8_uri_accepts_arbitrary_printable_unicode(
        path in "\\PC{1,32}",
    ) {
        let uri = format!("file://{path}");
        let mut g = Grid::new(1, 4);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]8;;");
        bytes.extend_from_slice(uri.as_bytes());
        bytes.push(0x07);
        bytes.push(b'X');
        drive(&mut g, &bytes);
        let id = g.cell(0, 0).expect("cell 0").link;
        prop_assert!(id.is_some(), "uri {uri:?} did not install a link");
        let entry = g.hyperlink(id.unwrap()).expect("link table entry");
        prop_assert_eq!(entry.uri.as_str(), uri.as_str());
    }

    #[test]
    fn osc_8_rejects_payload_with_a_control_byte(
        uri in proptest::collection::vec(
            (0u8..=255).prop_filter("not a terminator", |b| !is_terminator(*b)),
            1..32,
        ),
    ) {
        prop_assume!(has_non_terminator_control_byte(&uri));
        let mut g = Grid::new(1, 4);
        let mut payload = Vec::new();
        payload.push(b';');
        payload.extend_from_slice(&uri);
        drive(&mut g, &osc(b"8", &payload));
        drive(&mut g, b"x");
        prop_assert_eq!(g.cell(0, 0).unwrap().link, None);
    }

    /// REQ-910: only allowlisted schemes open a link.
    #[test]
    fn osc_8_rejects_any_scheme_outside_the_allowlist(
        scheme in "[a-zA-Z][a-zA-Z0-9.+-]{0,15}",
        path in "[a-zA-Z0-9/_.-]{1,16}",
    ) {
        let lower = scheme.to_ascii_lowercase();
        prop_assume!(!matches!(
            lower.as_str(),
            "http" | "https" | "mailto" | "file",
        ));
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        let bytes = format!("\x1b]8;;{scheme}://{path}\x07X\x1b]8;;\x07");
        p.advance(&mut g, bytes.as_bytes());
        prop_assert_eq!(g.hyperlink_count(), 0);
        prop_assert_eq!(g.cell(0, 0).unwrap().link, None);
    }

    #[test]
    fn osc_8_rejects_extended_prefixes_of_allowlisted_schemes(
        base in prop_oneof!["http", "https", "mailto", "file"],
        suffix in "[a-zA-Z0-9.+-]{1,8}",
        path in "[a-zA-Z0-9/_.-]{1,16}",
    ) {
        let scheme = format!("{base}{suffix}");
        prop_assume!(
            !["http", "https", "mailto", "file"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        );
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        let bytes = format!("\x1b]8;;{scheme}://{path}\x07X\x1b]8;;\x07");
        p.advance(&mut g, bytes.as_bytes());
        prop_assert_eq!(
            g.hyperlink_count(),
            0,
            "scheme {:?} leaked past the exact-match allowlist",
            scheme,
        );
        prop_assert_eq!(g.cell(0, 0).unwrap().link, None);
    }

    /// RFC 3986 §3.1: scheme matching is case-insensitive.
    #[test]
    fn osc_8_accepts_any_case_variant_of_an_allowlisted_scheme(
        base in prop_oneof!["http", "https", "mailto", "file"],
        flips in proptest::collection::vec(proptest::bool::ANY, 8),
        path in "[a-zA-Z0-9/_.-]{1,16}",
    ) {
        let scheme: String = base
            .bytes()
            .zip(flips.iter().cycle())
            .map(|(b, &up)| if up { b.to_ascii_uppercase() } else { b.to_ascii_lowercase() } as char)
            .collect();
        let mut p = Parser::new();
        let mut g = Grid::new(1, 4);
        let bytes = format!("\x1b]8;;{scheme}://{path}\x07X\x1b]8;;\x07");
        p.advance(&mut g, bytes.as_bytes());
        prop_assert!(
            g.cell(0, 0).unwrap().link.is_some(),
            "case-variant scheme {:?} was rejected — RFC 3986 §3.1 requires case-insensitive match",
            scheme,
        );
    }
}
