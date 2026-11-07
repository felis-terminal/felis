//! The `--class` vocabulary both front ends parse, so a name `felis`
//! forwards is never one `felis-client` then refuses.

/// The longest name accepted, in bytes. A Win32 class name holds at most
/// 256 UTF-16 units including the terminator, and no UTF-8 string of 255
/// bytes needs more than 255 of them.
pub const MAX_WINDOW_CLASS_BYTES: usize = 255;

/// Clap value parser for `--class`: non-empty, at most
/// [`MAX_WINDOW_CLASS_BYTES`] bytes, no control characters.
pub fn validate_window_class(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("window class must not be empty".into());
    }
    if raw.len() > MAX_WINDOW_CLASS_BYTES {
        return Err(format!(
            "window class is {} bytes; the limit is {MAX_WINDOW_CLASS_BYTES}",
            raw.len(),
        ));
    }
    if let Some(c) = raw.chars().find(|c| c.is_control()) {
        return Err(format!(
            "window class must not contain control characters (found U+{:04X})",
            u32::from(c),
        ));
    }
    Ok(raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn an_empty_name_is_refused() {
        assert!(validate_window_class("").is_err());
    }

    #[test]
    fn the_byte_bound_is_inclusive_at_255() {
        assert_eq!(
            validate_window_class(&"a".repeat(255)).as_deref(),
            Ok("a".repeat(255).as_str()),
        );
        let err = validate_window_class(&"a".repeat(256)).unwrap_err();
        assert!(err.contains("256 bytes"), "{err}");
    }

    #[test]
    fn the_bound_counts_bytes_not_characters() {
        // 85 three-byte characters are 255 bytes; one more is 258.
        assert!(validate_window_class(&"端".repeat(85)).is_ok());
        assert!(validate_window_class(&"端".repeat(86)).is_err());
    }

    #[test]
    fn non_ascii_names_are_accepted_verbatim() {
        assert_eq!(validate_window_class("端末-é").as_deref(), Ok("端末-é"));
    }

    #[test]
    fn c0_del_and_c1_controls_are_refused() {
        for name in ["a\tb", "a\nb", "\0", "a\u{7f}", "a\u{85}b", "\u{9b}"] {
            assert!(validate_window_class(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn spaces_and_punctuation_are_accepted() {
        assert!(validate_window_class("felis scratch.pad_1").is_ok());
    }

    proptest! {
        // 63 characters of at most four bytes each stay under the bound.
        #[test]
        fn a_short_printable_name_round_trips(raw in "\\PC{1,63}") {
            prop_assert_eq!(validate_window_class(&raw), Ok(raw));
        }

        #[test]
        fn one_control_character_anywhere_refuses_the_name(
            head in "\\PC{0,20}",
            control in "\\p{Cc}",
            tail in "\\PC{0,20}",
        ) {
            let name = [head, control, tail].concat();
            prop_assert!(validate_window_class(&name).is_err(), "{:?}", name);
        }

        #[test]
        fn a_printable_name_over_the_bound_is_refused(raw in "\\PC{256,300}") {
            prop_assert!(validate_window_class(&raw).is_err());
        }
    }
}
