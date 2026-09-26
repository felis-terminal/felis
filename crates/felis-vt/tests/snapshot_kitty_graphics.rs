//! Conformance snapshots for the Kitty graphics command parser: per-input
//! ground truth for representative wire forms from
//! `docs/reference/protocols/kitty-graphics.md` "Transmission methods",
//! "Image formats", and "Actions". Per-key value grammars live at the
//! dispatcher, out of scope here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_vt::kitty_graphics::{Command, parse};

/// `None` renders as `"REJECTED"` so accept→reject flips show on the first
/// line of the diff.
fn pin(body: &[u8]) -> String {
    match parse(body) {
        Some(Command { controls, payload }) => {
            let mut out = String::from("controls:\n");
            if controls.is_empty() {
                out.push_str("  (none)\n");
            } else {
                for (k, v) in &controls {
                    writeln!(
                        out,
                        "  {}={}",
                        *k as char,
                        std::str::from_utf8(v).expect("parser guarantees ASCII"),
                    )
                    .unwrap();
                }
            }
            write!(out, "payload ({} bytes): {payload:?}", payload.len()).unwrap();
            out
        }
        None => "REJECTED".to_string(),
    }
}

/// One-line variant of [`pin`]: controls in order, then the payload byte count.
fn pin_line(body: &[u8]) -> String {
    match parse(body) {
        Some(Command { controls, payload }) => {
            let rendered: Vec<String> = controls
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}={}",
                        *k as char,
                        std::str::from_utf8(v).expect("parser guarantees ASCII"),
                    )
                })
                .collect();
            format!("[{}] payload {} bytes", rendered.join(" "), payload.len())
        }
        None => "REJECTED".to_string(),
    }
}

/// The parser checks only envelope shape and per-pair byte legality, so
/// every representative wire form takes the same branches; one table pins
/// them all.
#[test]
fn representative_wire_forms_parse_to_their_controls_and_payload() {
    let cases: &[&[u8]] = &[
        // Actions.
        b"Ga=t,f=32,s=1,v=1",
        b"Ga=T,f=32,s=1,v=1",
        b"Ga=p,i=1",
        b"Ga=d,i=1",
        b"Ga=a,i=1",
        b"Ga=f,i=1,r=2",
        b"Ga=c,i=1",
        b"Ga=q,i=1",
        // Transmission methods.
        b"Ga=T,t=d,f=32;BASE64DATA",
        b"Ga=T,t=f,f=100;L3RtcC9maWxl",
        b"Ga=T,t=t,f=100;L3RtcC9YeXo",
        b"Ga=T,t=s,f=32;L2Rldi9zaG0vWHl6",
        // Image formats.
        b"Ga=T,f=24,s=1,v=1;UkdC",
        b"Ga=T,f=32,s=1,v=1;UkdCQQ",
        b"Ga=T,f=100;iVBORw0KGgo",
        // Combined forms.
        b"Ga=T,t=d,f=32,s=1,v=1;UkdCQQ",
        b"Ga=T,t=f,f=100,i=42;L3RtcC9hLnBuZw",
    ];
    let mut out = String::new();
    for body in cases {
        writeln!(
            out,
            "{} => {}",
            std::str::from_utf8(body).unwrap(),
            pin_line(body),
        )
        .unwrap();
    }
    insta::assert_snapshot!(out, @"
    Ga=t,f=32,s=1,v=1 => [a=t f=32 s=1 v=1] payload 0 bytes
    Ga=T,f=32,s=1,v=1 => [a=T f=32 s=1 v=1] payload 0 bytes
    Ga=p,i=1 => [a=p i=1] payload 0 bytes
    Ga=d,i=1 => [a=d i=1] payload 0 bytes
    Ga=a,i=1 => [a=a i=1] payload 0 bytes
    Ga=f,i=1,r=2 => [a=f i=1 r=2] payload 0 bytes
    Ga=c,i=1 => [a=c i=1] payload 0 bytes
    Ga=q,i=1 => [a=q i=1] payload 0 bytes
    Ga=T,t=d,f=32;BASE64DATA => [a=T t=d f=32] payload 10 bytes
    Ga=T,t=f,f=100;L3RtcC9maWxl => [a=T t=f f=100] payload 12 bytes
    Ga=T,t=t,f=100;L3RtcC9YeXo => [a=T t=t f=100] payload 11 bytes
    Ga=T,t=s,f=32;L2Rldi9zaG0vWHl6 => [a=T t=s f=32] payload 16 bytes
    Ga=T,f=24,s=1,v=1;UkdC => [a=T f=24 s=1 v=1] payload 4 bytes
    Ga=T,f=32,s=1,v=1;UkdCQQ => [a=T f=32 s=1 v=1] payload 6 bytes
    Ga=T,f=100;iVBORw0KGgo => [a=T f=100] payload 11 bytes
    Ga=T,t=d,f=32,s=1,v=1;UkdCQQ => [a=T t=d f=32 s=1 v=1] payload 6 bytes
    Ga=T,t=f,f=100,i=42;L3RtcC9hLnBuZw => [a=T t=f f=100 i=42] payload 14 bytes
    ");
}

#[test]
fn compressed_rgba() {
    insta::assert_snapshot!(pin(b"Ga=T,t=d,f=32,o=z,s=1,v=1;eJxr"), @"
    controls:
      a=T
      t=d
      f=32
      o=z
      s=1
      v=1
    payload (4 bytes): [101, 74, 120, 114]
    ");
}

#[test]
fn chunked_continuation_marker() {
    // The parser's job ends at preserving `m=1`.
    insta::assert_snapshot!(pin(b"Ga=T,t=d,f=32,m=1;CHUNK1"), @"
    controls:
      a=T
      t=d
      f=32
      m=1
    payload (6 bytes): [67, 72, 85, 78, 75, 49]
    ");
}

#[test]
fn placement_delete_with_image_and_placement_ids() {
    insta::assert_snapshot!(pin(b"Ga=d,i=1,p=1"), @r#"
    controls:
      a=d
      i=1
      p=1
    payload (0 bytes): []
    "#);
}

#[test]
fn empty_body_with_just_g() {
    // The parser accepts an empty controls vec; the dispatcher decides
    // what that means.
    insta::assert_snapshot!(pin(b"G"), @r#"
    controls:
      (none)
    payload (0 bytes): []
    "#);
}

#[test]
fn payload_only_no_controls() {
    insta::assert_snapshot!(pin(b"G;UkdC"), @r#"
    controls:
      (none)
    payload (4 bytes): [85, 107, 100, 67]
    "#);
}

#[test]
fn controls_only_with_trailing_semicolon() {
    insta::assert_snapshot!(pin(b"Ga=T,f=32;"), @r#"
    controls:
      a=T
      f=32
    payload (0 bytes): []
    "#);
}

#[test]
fn signed_integer_value_for_z_index() {
    // The spec allows a leading `-` on signed ints (`z=-1`).
    insta::assert_snapshot!(pin(b"Ga=p,i=1,z=-1"), @r#"
    controls:
      a=p
      i=1
      z=-1
    payload (0 bytes): []
    "#);
}

#[test]
fn duplicate_key_preserved_in_order() {
    // The spec is inconsistent about duplicate-key semantics, so the
    // dispatcher picks.
    insta::assert_snapshot!(pin(b"Ga=T,a=q"), @r#"
    controls:
      a=T
      a=q
    payload (0 bytes): []
    "#);
}
