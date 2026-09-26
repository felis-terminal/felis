//! Conformance snapshots for the OSC 66 Kitty text-sizing parser: curated
//! metadata combinations from `docs/reference/protocols/kitty-text-sizing.md`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_vt::kitty_text_sizing::{Run, parse};

/// `None` renders as `"REJECTED"` so accept↔reject flips show as a diff on
/// the first line. `pin` assembles the `;`-separated body from wire fields.
fn pin(parts: &[&[u8]]) -> String {
    match parse(&parts.join(&b';')) {
        Some(Run { sizing, text }) => format!("{sizing:#?}\ntext: {text:?}"),
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

#[test]
fn integer_scale_2() {
    insta::assert_snapshot!(pin(&[b"66", b"s=2", b"X"]), @r"
    Sizing {
        scale: 2,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn integer_scale_max_7() {
    insta::assert_snapshot!(pin(&[b"66", b"s=7", b"X"]), @r"
    Sizing {
        scale: 7,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn cell_width_override_4() {
    insta::assert_snapshot!(pin(&[b"66", b"w=4", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 4,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn cell_width_max_7() {
    insta::assert_snapshot!(pin(&[b"66", b"w=7", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 7,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn fractional_scale_smallest_one_over_two() {
    insta::assert_snapshot!(pin(&[b"66", b"n=1:d=2", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 1,
        frac_den: 2,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn fractional_scale_max_fourteen_over_fifteen() {
    insta::assert_snapshot!(pin(&[b"66", b"n=14:d=15", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 14,
        frac_den: 15,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn valign_top_is_default() {
    insta::assert_snapshot!(pin(&[b"66", b"v=0", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn valign_bottom() {
    insta::assert_snapshot!(pin(&[b"66", b"v=1", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Bottom,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn valign_center() {
    insta::assert_snapshot!(pin(&[b"66", b"v=2", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Center,
        halign: Left,
    }
    text: [88]
    ");
}

#[test]
fn halign_right() {
    insta::assert_snapshot!(pin(&[b"66", b"h=1", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Right,
    }
    text: [88]
    ");
}

#[test]
fn halign_center() {
    insta::assert_snapshot!(pin(&[b"66", b"h=2", b"X"]), @r"
    Sizing {
        scale: 1,
        cell_width: 0,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Center,
    }
    text: [88]
    ");
}

#[test]
fn integer_scale_with_width_override_and_alignment() {
    insta::assert_snapshot!(pin(&[b"66", b"s=2:w=4:h=1", b"H"]), @r"
    Sizing {
        scale: 2,
        cell_width: 4,
        frac_num: 0,
        frac_den: 0,
        valign: Top,
        halign: Right,
    }
    text: [72]
    ");
}

#[test]
fn fractional_scale_with_centered_valign() {
    insta::assert_snapshot!(pin(&[b"66", b"s=3:n=1:d=4:v=2", b"H"]), @r"
    Sizing {
        scale: 3,
        cell_width: 0,
        frac_num: 1,
        frac_den: 4,
        valign: Center,
        halign: Left,
    }
    text: [72]
    ");
}

#[test]
fn maximum_combined_metadata_parses() {
    // s=7 with the largest fractional factor `d > n` allows. Effective
    // scale `7 × 14/15 ≈ 6.533` is below `s` by design: the fractional
    // component always shrinks `s` (REQ-401 + REQ-403).
    insta::assert_snapshot!(pin(&[b"66", b"s=7:n=14:d=15", b"H"]), @r"
    Sizing {
        scale: 7,
        cell_width: 0,
        frac_num: 14,
        frac_den: 15,
        valign: Top,
        halign: Left,
    }
    text: [72]
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
