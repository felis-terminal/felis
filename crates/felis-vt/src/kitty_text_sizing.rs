//! Kitty text-sizing protocol parser: `OSC 66 ; <metadata> ; <text> ST`.
//! Keys and the unsupported legacy `CSI Pn:...:Pn t` form match
//! `docs/reference/protocols/kitty-text-sizing.md` "Wire format".

// The sizing vocabulary lives in `felis-protocol` so the renderer and
// headless client read it without depending on this crate; re-exported so
// this path stays stable.
pub use felis_protocol::kitty_text_sizing::{HAlign, Sizing, VAlign};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run<'a> {
    pub sizing: Sizing,
    /// Opaque at this layer.
    pub text: &'a [u8],
}

/// Parse an OSC 66 body (`66;<metadata>;<text>`), where text includes any
/// literal `;` past the second separator. Returns `None` on non-66 codes,
/// missing metadata, unknown keys, out-of-range values, or `d <= n`.
#[must_use]
pub fn parse(body: &[u8]) -> Option<Run<'_>> {
    let (code, rest) = crate::split_osc_first(body);
    if code != b"66" {
        return None;
    }
    let (metadata, text) = crate::split_osc_first(rest?);
    let sizing = parse_metadata(metadata)?;
    Some(Run {
        sizing,
        text: text.unwrap_or_default(),
    })
}

/// Empty metadata returns the default sizing. The numeric range checks
/// live in `Sizing::new`, the single owner of the spec ranges.
pub fn parse_metadata(bytes: &[u8]) -> Option<Sizing> {
    if bytes.is_empty() {
        return Some(Sizing::default());
    }
    let default = Sizing::default();
    let mut scale = default.scale();
    let mut cell_width = default.cell_width();
    let mut frac_num = default.frac_num();
    let mut frac_den = default.frac_den();
    let mut valign = default.valign();
    let mut halign = default.halign();
    for pair in bytes.split(|b| *b == b':') {
        if pair.is_empty() {
            // The spec gives a leading / trailing / consecutive colon no meaning.
            return None;
        }
        let eq = pair.iter().position(|&b| b == b'=')?;
        if eq != 1 {
            return None;
        }
        let key = pair[0];
        let value_bytes = &pair[2..];
        let value = parse_u8(value_bytes)?;
        match key {
            b's' => scale = value,
            b'w' => cell_width = value,
            b'n' => frac_num = value,
            b'd' => frac_den = value,
            b'v' => {
                valign = match value {
                    0 => VAlign::Top,
                    1 => VAlign::Bottom,
                    2 => VAlign::Center,
                    _ => return None,
                };
            }
            b'h' => {
                halign = match value {
                    0 => HAlign::Left,
                    1 => HAlign::Right,
                    2 => HAlign::Center,
                    _ => return None,
                };
            }
            _ => return None,
        }
    }
    Sizing::new(scale, cell_width, frac_num, frac_den, valign, halign)
}

fn parse_u8(bytes: &[u8]) -> Option<u8> {
    if bytes.is_empty() {
        return None;
    }
    let mut acc: u32 = 0;
    for &b in bytes {
        let digit = (b as char).to_digit(10)?;
        acc = acc.checked_mul(10)?.checked_add(digit)?;
        if acc > u8::MAX as u32 {
            return None;
        }
    }
    Some(acc as u8)
}

// Kani proofs (`docs/reference/testing.md` "Kani proof inventory").
#[cfg(kani)]
mod kani_proofs {
    use super::parse_u8;

    /// Four bytes is one wider than the longest in-range value (`255`), so
    /// the `> u8::MAX` rejection is exercised alongside the non-digit
    /// rejection and the 1–3 digit accepts.
    #[kani::proof]
    #[kani::unwind(5)]
    fn parse_u8_matches_decimal_reference() {
        let bytes: [u8; 4] = kani::any();
        let mut acc: u32 = 0;
        let mut expected: Option<u8> = Some(0);
        for &b in &bytes {
            if !(b'0'..=b'9').contains(&b) {
                expected = None;
                break;
            }
            acc = acc * 10 + u32::from(b - b'0');
            if acc > u32::from(u8::MAX) {
                expected = None;
                break;
            }
            expected = Some(acc as u8);
        }
        assert_eq!(parse_u8(&bytes), expected);
    }

    /// Only totality is proved: the spec ranges are carried by the `Sizing`
    /// type, whose checked constructor is the only way to mint one.
    #[kani::proof]
    #[kani::unwind(10)]
    fn parse_metadata_never_panics() {
        let bytes: [u8; 8] = kani::any();
        let _ = super::parse_metadata(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metadata_yields_default_sizing() {
        // The spec says empty metadata means "render at default size".
        assert_eq!(parse_metadata(b""), Some(Sizing::default()));
    }

    #[test]
    fn full_metadata_parses_each_key() {
        let s = parse_metadata(b"s=2:w=4:n=1:d=2:v=2:h=1").unwrap();
        assert_eq!(s.scale(), 2);
        assert_eq!(s.cell_width(), 4);
        assert_eq!(s.frac_num(), 1);
        assert_eq!(s.frac_den(), 2);
        assert_eq!(s.valign(), VAlign::Center);
        assert_eq!(s.halign(), HAlign::Right);
    }

    #[test]
    fn keys_are_independent_and_omittable() {
        let s = parse_metadata(b"s=3:h=2").unwrap();
        assert_eq!(s.scale(), 3);
        assert_eq!(s.halign(), HAlign::Center);
        assert_eq!(s.cell_width(), 0);
        assert_eq!(s.frac_num(), 0);
        assert_eq!(s.frac_den(), 0);
        assert_eq!(s.valign(), VAlign::Top);
    }

    #[test]
    fn parse_u8_pins_the_255_boundary() {
        // The exhaustive `parse_u8` proof runs only under Kani; 255 and 256
        // together separate `>` from `==`/`>=`.
        assert_eq!(parse_u8(b"255"), Some(255));
        assert_eq!(parse_u8(b"256"), None);
    }

    #[test]
    fn s_must_be_in_range_one_to_seven() {
        assert!(parse_metadata(b"s=0").is_none());
        assert!(parse_metadata(b"s=8").is_none());
        assert!(parse_metadata(b"s=255").is_none());
        for v in 1..=7 {
            assert!(parse_metadata(format!("s={v}").as_bytes()).is_some());
        }
    }

    #[test]
    fn w_must_be_in_range_zero_to_seven() {
        for v in 0..=7 {
            assert!(parse_metadata(format!("w={v}").as_bytes()).is_some());
        }
        assert!(parse_metadata(b"w=8").is_none());
    }

    #[test]
    fn n_and_d_must_be_in_range_zero_to_fifteen() {
        // d=0 deactivates the fractional component, so n is ignored.
        for v in 0..=15 {
            assert!(
                parse_metadata(format!("n={v}").as_bytes()).is_some(),
                "n={v}"
            );
        }
        for v in 0..=15 {
            assert!(
                parse_metadata(format!("d={v}").as_bytes()).is_some(),
                "d={v}"
            );
        }
        assert!(parse_metadata(b"n=16").is_none());
        assert!(parse_metadata(b"d=16").is_none());
    }

    #[test]
    fn v_and_h_reject_values_above_two() {
        for v in 0..=2 {
            assert!(parse_metadata(format!("v={v}").as_bytes()).is_some());
            assert!(parse_metadata(format!("h={v}").as_bytes()).is_some());
        }
        assert!(parse_metadata(b"v=3").is_none());
        assert!(parse_metadata(b"h=3").is_none());
    }

    #[test]
    fn d_must_strictly_exceed_n_when_nonzero() {
        // d=2,n=2 is an effective scale of s + 1.0, representable via s alone.
        assert!(parse_metadata(b"n=0:d=0").is_some()); // both zero ok
        assert!(parse_metadata(b"n=0:d=1").is_some());
        assert!(parse_metadata(b"n=2:d=3").is_some());
        assert!(parse_metadata(b"n=2:d=2").is_none());
        assert!(parse_metadata(b"n=3:d=2").is_none());
    }

    #[test]
    fn unknown_key_is_rejected() {
        // A producer typo must not fall through unnoticed.
        assert!(parse_metadata(b"x=1").is_none());
    }

    #[test]
    fn malformed_pairs_are_rejected() {
        assert!(parse_metadata(b"s").is_none()); // no =
        assert!(parse_metadata(b"=1").is_none()); // empty key
        assert!(parse_metadata(b"s=").is_none()); // empty value
        assert!(parse_metadata(b"sx=1").is_none()); // multi-char key
        assert!(parse_metadata(b"s=1:").is_none()); // trailing colon
        assert!(parse_metadata(b":s=1").is_none()); // leading colon
        assert!(parse_metadata(b"s=1::w=2").is_none()); // double colon
    }

    #[test]
    fn parse_full_payload_with_simple_text() {
        let run = parse(b"66;s=2;hello").unwrap();
        assert_eq!(run.sizing.scale(), 2);
        assert_eq!(run.text, b"hello");
    }

    #[test]
    fn parse_payload_keeps_text_with_embedded_semicolon() {
        // `;` is framing only, never a text separator.
        let run = parse(b"66;;a;b").unwrap();
        assert_eq!(run.text, b"a;b");
    }

    #[test]
    fn parse_payload_with_no_text_yields_empty_run() {
        // Empty text is a "set this sizing on the cursor" hint; the
        // dispatcher decides what to do with it.
        let run = parse(b"66;s=2;").unwrap();
        assert_eq!(run.sizing.scale(), 2);
        assert_eq!(run.text, b"");
    }

    #[test]
    fn parse_rejects_a_body_without_a_metadata_field() {
        assert!(parse(b"66").is_none());
    }

    #[test]
    fn parse_rejects_wrong_osc_code() {
        assert!(parse(b"52;s=2;hello").is_none());
    }

    #[test]
    fn parse_rejects_payload_with_invalid_metadata() {
        assert!(parse(b"66;s=8;hello").is_none());
    }
}
