use super::*;

/// The documented `font.features` grammar, over every 4-byte tag:
/// harfrust's `Feature::from_str` rejects the `on` and numeric forms,
/// so each surface form here is one felis must accept itself.
fn arb_tag() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9]{4}").expect("literal tag regex")
}

proptest! {
    #[test]
    fn parse_feature_accepts_every_documented_surface_form(
        tag in arb_tag(),
        value in any::<u16>(),
        pad_left in "[ ]{0,2}",
        pad_right in "[ ]{0,2}",
        quote in prop_oneof![Just(""), Just("'"), Just("\"")],
    ) {
        let raw = Tag::new_checked(tag.as_bytes()).expect("a 4-byte tag");
        let quoted = format!("{quote}{tag}{quote}");
        let wrap = |entry: &str| format!("{pad_left}{entry}{pad_right}");
        for (entry, want) in [
            (wrap(&quoted), 1),
            (wrap(&format!("-{quoted}")), 0),
            (wrap(&format!("{quoted} on")), 1),
            (wrap(&format!("{quoted} off")), 0),
            (wrap(&format!("{quoted} {value}")), value),
        ] {
            let s = parse_feature(&entry).ok_or_else(|| {
                TestCaseError::fail(format!("{entry:?} must parse"))
            })?;
            prop_assert_eq!((s.tag, s.value), (raw, u32::from(want)), "entry {:?}", entry);
        }
    }

    #[test]
    fn parse_feature_rejects_negation_with_a_value_and_oversize_tags(
        tag in arb_tag(),
        value in any::<u16>(),
        extra in "[a-z0-9]{1,4}",
    ) {
        prop_assert_eq!(parse_feature(&format!("-{tag} {value}")), None);
        prop_assert_eq!(parse_feature(&format!("-{tag} off")), None);
        prop_assert_eq!(parse_feature(&format!("{tag}{extra}")), None);
        prop_assert_eq!(parse_feature(&format!("{tag} {value} {value}")), None);
    }
}

#[test]
fn parse_feature_rejects_malformed_entries() {
    for bad in [
        "",
        "   ",
        "-",
        "ligatures",  // > 4 bytes
        "ss03 maybe", // bad value token
        "ss03 1 2",   // trailing garbage
        "-clig off",  // negation + explicit value contradict
        "ss03 65536", // u16 overflow
        "ss\u{30c3}", // non-ASCII tag
    ] {
        assert!(parse_feature(bad).is_none(), "{bad:?} must not parse");
    }
}
