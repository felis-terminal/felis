//! Conformance snapshots for the OSC 66 Kitty text-sizing parser: curated
//! metadata combinations from `docs/reference/protocols/kitty-text-sizing.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_vt::kitty_text_sizing::{Run, parse};

/// `None` renders as `"REJECTED"` so accept↔reject flips show as a diff on
/// the first line. `pin` assembles the `;`-separated body from wire fields.
fn pin(parts: &[&[u8]]) -> String {
    match parse(&parts.join(&b';')) {
        Some(Run { sizing, text }) => format!("{sizing:#?}\ntext: {text:?}"),
        None => "REJECTED".to_string(),
    }
}

/// One-line variant of [`pin`].
fn pin_line(parts: &[&[u8]]) -> String {
    match parse(&parts.join(&b';')) {
        Some(Run { sizing, text }) => format!(
            "s={} w={} n={} d={} v={:?} h={:?} text={text:?}",
            sizing.scale(),
            sizing.cell_width(),
            sizing.frac_num(),
            sizing.frac_den(),
            sizing.valign(),
            sizing.halign(),
        ),
        None => "REJECTED".to_string(),
    }
}

#[test]
fn empty_metadata_yields_default_sizing() {
    insta::assert_snapshot!(pin(&[b"66", b"", b"hi"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [104, 105]
    ");
}

/// One line per metadata form: every key and value range takes the same
/// parse branch, so one table pins them all.
#[test]
fn representative_metadata_parses_to_its_sizing() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"s=2", b"X"),
        (b"s=7", b"X"),
        (b"w=4", b"X"),
        (b"w=7", b"X"),
        (b"n=1:d=2", b"X"),
        (b"n=14:d=15", b"X"),
        (b"v=0", b"X"),
        (b"v=1", b"X"),
        (b"v=2", b"X"),
        (b"h=1", b"X"),
        (b"h=2", b"X"),
        (b"s=2:w=4:h=1", b"H"),
        (b"s=3:n=1:d=4:v=2", b"H"),
        // The fractional factor always shrinks `s` (REQ-401 + REQ-403):
        // the effective scale here is 7 × 14/15 ≈ 6.533.
        (b"s=7:n=14:d=15", b"H"),
    ];
    let mut out = String::new();
    for (metadata, text) in cases {
        writeln!(
            out,
            "{} => {}",
            std::str::from_utf8(metadata).unwrap(),
            pin_line(&[b"66", metadata, text]),
        )
        .unwrap();
    }
    insta::assert_snapshot!(out, @"
    s=2 => s=2 w=0 n=0 d=0 v=Top h=Left text=[88]
    s=7 => s=7 w=0 n=0 d=0 v=Top h=Left text=[88]
    w=4 => s=1 w=4 n=0 d=0 v=Top h=Left text=[88]
    w=7 => s=1 w=7 n=0 d=0 v=Top h=Left text=[88]
    n=1:d=2 => s=1 w=0 n=1 d=2 v=Top h=Left text=[88]
    n=14:d=15 => s=1 w=0 n=14 d=15 v=Top h=Left text=[88]
    v=0 => s=1 w=0 n=0 d=0 v=Top h=Left text=[88]
    v=1 => s=1 w=0 n=0 d=0 v=Bottom h=Left text=[88]
    v=2 => s=1 w=0 n=0 d=0 v=Center h=Left text=[88]
    h=1 => s=1 w=0 n=0 d=0 v=Top h=Right text=[88]
    h=2 => s=1 w=0 n=0 d=0 v=Top h=Center text=[88]
    s=2:w=4:h=1 => s=2 w=4 n=0 d=0 v=Top h=Right text=[72]
    s=3:n=1:d=4:v=2 => s=3 w=0 n=1 d=4 v=Center h=Left text=[72]
    s=7:n=14:d=15 => s=7 w=0 n=14 d=15 v=Top h=Left text=[72]
    ");
}

#[test]
fn text_with_internal_semicolon_survives() {
    // Only the first two `;` are framing.
    insta::assert_snapshot!(pin(&[b"66", b"s=2", b"hello", b"world"]), @r"
    Sizing {
        scale: 2,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [104, 101, 108, 108, 111, 59, 119, 111, 114, 108, 100]
    ");
}

#[test]
fn empty_text_payload_is_allowed() {
    // Metadata keys are all optional and a zero-byte run is well-formed.
    insta::assert_snapshot!(pin(&[b"66", b""]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: []
    ");
}
