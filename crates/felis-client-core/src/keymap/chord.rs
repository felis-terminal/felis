//! `Chord` (modifier set + key) and the parser / Display impls for the
//! chord-string syntax of docs/reference/keybindings.md "Chord grammar quick facts".

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

pub use felis_protocol::messages::{FKey, KeyMods as Modifiers, NamedKey};

/// The keys a chord can bind: [`felis_protocol::messages::Key`] without
/// the `Other` variant, which no binding can name.
///
/// Character keys are case-sensitive (docs/reference/keybindings.md)
/// so non-Latin layouts where Shift does not uppercase still bind.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
    Named(NamedKey),
    /// A single Unicode grapheme cluster.
    Character(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    pub mods: Modifiers,
    pub key: KeyCode,
}

impl Chord {
    #[must_use]
    pub const fn bare(key: KeyCode) -> Self {
        Self {
            mods: Modifiers::empty(),
            key,
        }
    }

    #[must_use]
    pub const fn with_mods(mods: Modifiers, key: KeyCode) -> Self {
        Self { mods, key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseChordError {
    Empty,
    /// `"ctrl+"`; the literal `+` key is spelled `"ctrl++"`.
    EmptyKey,
    /// `+` between modifiers or leading: `"ctrl++a"`, `"+a"`.
    EmptyModifier,
    DuplicateModifier(String),
    UnknownModifier(String),
    UnknownNamedKey(String),
    InvalidFKeyIndex(u32),
}

impl fmt::Display for ParseChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty chord string"),
            Self::EmptyKey => write!(f, "chord ends with `+` and no key follows"),
            Self::EmptyModifier => {
                write!(f, "empty modifier token (consecutive `+` or leading `+`)")
            }
            Self::DuplicateModifier(name) => write!(f, "duplicate modifier `{name}`"),
            Self::UnknownModifier(name) => write!(f, "unknown modifier `{name}`"),
            Self::UnknownNamedKey(name) => write!(f, "unknown named key `{name}`"),
            Self::InvalidFKeyIndex(n) => {
                write!(f, "F-key index `{n}` outside the supported range 1..=35")
            }
        }
    }
}

impl std::error::Error for ParseChordError {}

/// Folds an uppercase ASCII letter to lowercase plus SHIFT, the form
/// the client lifts every such keypress to before the lookup.
impl FromStr for Chord {
    type Err = ParseChordError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseChordError::Empty);
        }

        let Some(split) = split_chord(s.as_bytes()) else {
            return Err(ParseChordError::EmptyKey);
        };

        let mut mods = Modifiers::empty();
        if let Some(section) = split.mods {
            for tok in section.split(|b| *b == b'+') {
                if tok.is_empty() {
                    return Err(ParseChordError::EmptyModifier);
                }
                let Some(bit) = modifier_bit(tok) else {
                    return Err(ParseChordError::UnknownModifier(text(tok).into_owned()));
                };
                if mods.contains(bit) {
                    return Err(ParseChordError::DuplicateModifier(text(tok).into_owned()));
                }
                mods |= bit;
            }
        }

        let key = match parse_key(split.key)? {
            KeyCode::Character(c) if c.len() == 1 && c.as_bytes()[0].is_ascii_uppercase() => {
                mods |= Modifiers::SHIFT;
                KeyCode::Character(c.to_ascii_lowercase())
            }
            key => key,
        };
        Ok(Self { mods, key })
    }
}

/// The literal `+` key, spelled `++` at the end of a chord string.
const PLUS_KEY: &[u8] = b"+";

/// A chord string cut into its modifier section (everything before the
/// final `+`) and its key.
struct ChordSplit<'a> {
    /// `None` is a chord that names no modifier section at all; it is not
    /// the same as `Some(b"")`, which is the leading `+` of `"+a"`.
    mods: Option<&'a [u8]>,
    key: &'a [u8],
}

/// `None` is [`ParseChordError::EmptyKey`]: a trailing `+` with a key
/// before it and none after it.
fn split_chord(s: &[u8]) -> Option<ChordSplit<'_>> {
    let Some(last) = s.iter().rposition(|b| *b == b'+') else {
        return Some(ChordSplit { mods: None, key: s });
    };
    if last + 1 < s.len() {
        return Some(ChordSplit {
            mods: Some(&s[..last]),
            key: &s[last + 1..],
        });
    }
    if last == 0 {
        return Some(ChordSplit {
            mods: None,
            key: PLUS_KEY,
        });
    }
    if s[last - 1] == b'+' {
        return Some(ChordSplit {
            mods: Some(&s[..last - 1]),
            key: PLUS_KEY,
        });
    }
    None
}

/// Every slice reaching this comes from a `&str` cut at ASCII `+`
/// boundaries, so the lossy conversion never replaces anything.
fn text(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

const MODIFIER_NAMES: [(&[u8], Modifiers); 4] = [
    (b"ctrl", Modifiers::CONTROL),
    (b"shift", Modifiers::SHIFT),
    (b"alt", Modifiers::ALT),
    (b"super", Modifiers::SUPER),
];

const NAMED_KEY_NAMES: [(&[u8], NamedKey); 15] = [
    (b"enter", NamedKey::Enter),
    (b"tab", NamedKey::Tab),
    (b"escape", NamedKey::Escape),
    (b"space", NamedKey::Space),
    (b"backspace", NamedKey::Backspace),
    (b"insert", NamedKey::Insert),
    (b"delete", NamedKey::Delete),
    (b"home", NamedKey::Home),
    (b"end", NamedKey::End),
    (b"page_up", NamedKey::PageUp),
    (b"page_down", NamedKey::PageDown),
    (b"up", NamedKey::ArrowUp),
    (b"down", NamedKey::ArrowDown),
    (b"left", NamedKey::ArrowLeft),
    (b"right", NamedKey::ArrowRight),
];

fn modifier_bit(tok: &[u8]) -> Option<Modifiers> {
    MODIFIER_NAMES
        .iter()
        .find(|(name, _)| tok.eq_ignore_ascii_case(name))
        .map(|(_, bit)| *bit)
}

fn named_key(s: &[u8]) -> Option<NamedKey> {
    NAMED_KEY_NAMES
        .iter()
        .find(|(name, _)| s.eq_ignore_ascii_case(name))
        .map(|(_, key)| *key)
}

/// The digit run of an `f<n>` key name, or `None` when `s` is not one.
fn f_key_digits(s: &[u8]) -> Option<&[u8]> {
    let (first, rest) = s.split_first()?;
    if first.eq_ignore_ascii_case(&b'f') && !rest.is_empty() && rest.iter().all(u8::is_ascii_digit)
    {
        Some(rest)
    } else {
        None
    }
}

/// Saturating rather than wrapping or fallible: an absurd digit run has
/// no in-range F-key to name, so clamping reports it as out-of-range
/// instead of needing an overflow branch of its own.
///
/// Every byte must be an ASCII digit.
fn decimal_value(digits: &[u8]) -> u32 {
    let mut acc: u32 = 0;
    for d in digits {
        acc = acc.saturating_mul(10).saturating_add(u32::from(*d - b'0'));
    }
    acc
}

fn parse_key(s: &[u8]) -> Result<KeyCode, ParseChordError> {
    if s.is_empty() {
        return Err(ParseChordError::EmptyKey);
    }

    if let Some(digits) = f_key_digits(s) {
        let n = decimal_value(digits);
        return match u8::try_from(n).ok().and_then(FKey::new) {
            Some(k) => Ok(KeyCode::Named(NamedKey::F(k))),
            None => Err(ParseChordError::InvalidFKeyIndex(n)),
        };
    }

    if let Some(named) = named_key(s) {
        return Ok(KeyCode::Named(named));
    }

    // Multi-codepoint input is reported as an unknown named key: the
    // actionable error for a user who typed `"pgup"` or `"return"`.
    let key = text(s);
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(ParseChordError::EmptyKey);
    };
    if chars.next().is_some() {
        return Err(ParseChordError::UnknownNamedKey(key.into_owned()));
    }
    Ok(KeyCode::Character(first.to_string()))
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(Modifiers::CONTROL) {
            f.write_str("ctrl+")?;
        }
        if self.mods.contains(Modifiers::SHIFT) {
            f.write_str("shift+")?;
        }
        if self.mods.contains(Modifiers::ALT) {
            f.write_str("alt+")?;
        }
        if self.mods.contains(Modifiers::SUPER) {
            f.write_str("super+")?;
        }
        match &self.key {
            KeyCode::Character(c) => f.write_str(c),
            KeyCode::Named(NamedKey::Enter) => f.write_str("enter"),
            KeyCode::Named(NamedKey::Tab) => f.write_str("tab"),
            KeyCode::Named(NamedKey::Escape) => f.write_str("escape"),
            KeyCode::Named(NamedKey::Space) => f.write_str("space"),
            KeyCode::Named(NamedKey::Backspace) => f.write_str("backspace"),
            KeyCode::Named(NamedKey::Insert) => f.write_str("insert"),
            KeyCode::Named(NamedKey::Delete) => f.write_str("delete"),
            KeyCode::Named(NamedKey::Home) => f.write_str("home"),
            KeyCode::Named(NamedKey::End) => f.write_str("end"),
            KeyCode::Named(NamedKey::PageUp) => f.write_str("page_up"),
            KeyCode::Named(NamedKey::PageDown) => f.write_str("page_down"),
            KeyCode::Named(NamedKey::ArrowUp) => f.write_str("up"),
            KeyCode::Named(NamedKey::ArrowDown) => f.write_str("down"),
            KeyCode::Named(NamedKey::ArrowLeft) => f.write_str("left"),
            KeyCode::Named(NamedKey::ArrowRight) => f.write_str("right"),
            KeyCode::Named(NamedKey::F(n)) => write!(f, "f{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn distinct_chords_compare_unequal() {
        let ctrl_a = Chord::with_mods(Modifiers::CONTROL, KeyCode::Character("a".into()));
        let ctrl_shift_a = Chord::with_mods(
            Modifiers::CONTROL | Modifiers::SHIFT,
            KeyCode::Character("a".into()),
        );
        let ctrl_b = Chord::with_mods(Modifiers::CONTROL, KeyCode::Character("b".into()));
        assert_ne!(ctrl_a, ctrl_shift_a);
        assert_ne!(ctrl_a, ctrl_b);
    }

    #[test]
    fn character_keys_are_case_sensitive() {
        let lower = Chord::bare(KeyCode::Character("a".into()));
        let upper = Chord::bare(KeyCode::Character("A".into()));
        assert_ne!(lower, upper);
    }

    fn parse(s: &str) -> Chord {
        s.parse().unwrap_or_else(|err| panic!("parse {s:?}: {err}"))
    }

    #[test]
    fn parses_bare_character_key() {
        assert_eq!(parse("a"), Chord::bare(KeyCode::Character("a".into())),);
        assert_eq!(parse("é"), Chord::bare(KeyCode::Character("é".into())),);
        assert_eq!(parse("É"), Chord::bare(KeyCode::Character("É".into())),);
    }

    #[test]
    fn an_uppercase_ascii_letter_parses_as_the_lowercase_letter_plus_shift() {
        let shift_a = Chord::with_mods(Modifiers::SHIFT, KeyCode::Character("a".into()));
        assert_eq!(parse("A"), shift_a);
        assert_eq!(parse("shift+A"), shift_a);
        assert_eq!(parse("shift+a"), shift_a);
        assert_eq!(
            parse("ctrl+Z"),
            Chord::with_mods(
                Modifiers::CONTROL | Modifiers::SHIFT,
                KeyCode::Character("z".into()),
            ),
        );
        assert_eq!(parse("A").to_string(), "shift+a");
    }

    #[test]
    fn parses_bare_plus_character() {
        // The bare `+` key must parse so users can bind `Shift+=`.
        assert_eq!(parse("+"), Chord::bare(KeyCode::Character("+".into())),);
        assert_eq!(
            parse("ctrl++"),
            Chord::with_mods(Modifiers::CONTROL, KeyCode::Character("+".into())),
        );
        assert_eq!(
            parse("ctrl+shift++"),
            Chord::with_mods(
                Modifiers::CONTROL | Modifiers::SHIFT,
                KeyCode::Character("+".into()),
            ),
        );
    }

    #[test]
    fn parses_single_modifier_chord() {
        assert_eq!(
            parse("ctrl+a"),
            Chord::with_mods(Modifiers::CONTROL, KeyCode::Character("a".into())),
        );
    }

    #[test]
    fn each_modifier_has_exactly_one_token() {
        assert_eq!(
            parse("super+v"),
            Chord::with_mods(Modifiers::SUPER, KeyCode::Character("v".into())),
        );
        for tok in ["control", "cmd", "win"] {
            let spelling = format!("{tok}+v");
            let err = spelling.parse::<Chord>().unwrap_err();
            assert!(
                matches!(err, ParseChordError::UnknownModifier(ref m) if m == tok),
                "expected unknown modifier for {spelling}, got {err:?}",
            );
        }
    }

    #[test]
    fn modifier_and_named_key_tokens_are_case_insensitive() {
        assert_eq!(
            parse("CTRL+SHIFT+Page_Up"),
            Chord::with_mods(
                Modifiers::CONTROL | Modifiers::SHIFT,
                KeyCode::Named(NamedKey::PageUp),
            ),
        );
    }

    #[test]
    fn parses_every_named_key() {
        assert_eq!(parse("enter"), Chord::bare(KeyCode::Named(NamedKey::Enter)));
        assert_eq!(
            parse("escape"),
            Chord::bare(KeyCode::Named(NamedKey::Escape)),
        );
        assert_eq!(parse("space"), Chord::bare(KeyCode::Named(NamedKey::Space)));
        assert_eq!(parse("tab"), Chord::bare(KeyCode::Named(NamedKey::Tab)));
        assert_eq!(parse("home"), Chord::bare(KeyCode::Named(NamedKey::Home)));
        assert_eq!(parse("end"), Chord::bare(KeyCode::Named(NamedKey::End)));
        assert_eq!(
            parse("page_up"),
            Chord::bare(KeyCode::Named(NamedKey::PageUp)),
        );
        assert_eq!(
            parse("page_down"),
            Chord::bare(KeyCode::Named(NamedKey::PageDown)),
        );
        assert_eq!(parse("up"), Chord::bare(KeyCode::Named(NamedKey::ArrowUp)));
        assert_eq!(
            parse("down"),
            Chord::bare(KeyCode::Named(NamedKey::ArrowDown)),
        );
        assert_eq!(
            parse("left"),
            Chord::bare(KeyCode::Named(NamedKey::ArrowLeft)),
        );
        assert_eq!(
            parse("right"),
            Chord::bare(KeyCode::Named(NamedKey::ArrowRight)),
        );
        assert_eq!(
            parse("insert"),
            Chord::bare(KeyCode::Named(NamedKey::Insert)),
        );
        assert_eq!(
            parse("delete"),
            Chord::bare(KeyCode::Named(NamedKey::Delete)),
        );
        assert_eq!(
            parse("backspace"),
            Chord::bare(KeyCode::Named(NamedKey::Backspace)),
        );
    }

    #[test]
    fn parses_f_keys_in_range() {
        assert_eq!(
            parse("f1"),
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(1))))
        );
        assert_eq!(
            parse("f12"),
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(12))))
        );
        assert_eq!(
            parse("f35"),
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(35))))
        );
        assert_eq!(
            parse("F5"),
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(5))))
        );
    }

    #[test]
    fn rejects_empty_string() {
        assert_eq!("".parse::<Chord>(), Err(ParseChordError::Empty));
    }

    #[test]
    fn rejects_trailing_plus_without_key() {
        assert_eq!("ctrl+".parse::<Chord>(), Err(ParseChordError::EmptyKey));
    }

    #[test]
    fn rejects_leading_plus_or_consecutive_plus_between_mods() {
        assert_eq!("+a".parse::<Chord>(), Err(ParseChordError::EmptyModifier));
        assert_eq!(
            "ctrl++a".parse::<Chord>(),
            Err(ParseChordError::EmptyModifier),
        );
    }

    #[test]
    fn rejects_duplicate_modifier() {
        let err = "ctrl+ctrl+a".parse::<Chord>().unwrap_err();
        assert!(
            matches!(err, ParseChordError::DuplicateModifier(ref m) if m == "ctrl"),
            "expected duplicate modifier, got {err:?}",
        );
    }

    #[test]
    fn rejects_unknown_modifier() {
        let err = "meta+a".parse::<Chord>().unwrap_err();
        assert!(
            matches!(err, ParseChordError::UnknownModifier(ref m) if m == "meta"),
            "expected unknown modifier, got {err:?}",
        );
    }

    #[test]
    fn rejects_unknown_named_key() {
        let err = "ctrl+pgup".parse::<Chord>().unwrap_err();
        assert!(
            matches!(err, ParseChordError::UnknownNamedKey(ref k) if k == "pgup"),
            "expected unknown named key, got {err:?}",
        );
    }

    #[test]
    fn rejects_f_key_out_of_range() {
        assert_eq!(
            "f0".parse::<Chord>(),
            Err(ParseChordError::InvalidFKeyIndex(0)),
        );
        assert_eq!(
            "f36".parse::<Chord>(),
            Err(ParseChordError::InvalidFKeyIndex(36)),
        );
        assert_eq!(
            "f100".parse::<Chord>(),
            Err(ParseChordError::InvalidFKeyIndex(100)),
        );
    }

    #[test]
    fn f_key_index_beyond_u32_reports_the_clamped_index() {
        assert_eq!(
            "f99999999999".parse::<Chord>(),
            Err(ParseChordError::InvalidFKeyIndex(u32::MAX)),
        );
    }

    #[test]
    fn f_key_index_ignores_leading_zeros() {
        assert_eq!(
            parse("f0000000000000005"),
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(5)))),
        );
    }

    #[test]
    fn plus_after_the_modifier_section_stays_the_literal_key() {
        assert_eq!(
            "ctrl+++".parse::<Chord>(),
            Err(ParseChordError::EmptyModifier),
        );
        assert_eq!("++".parse::<Chord>(), Err(ParseChordError::EmptyModifier));
        assert_eq!("a+".parse::<Chord>(), Err(ParseChordError::EmptyKey));
    }

    #[test]
    fn modifier_and_named_key_spellings_ignore_ascii_case() {
        assert_eq!(
            parse("CTRL+SHIFT+PAGE_UP"),
            Chord::with_mods(
                Modifiers::CONTROL | Modifiers::SHIFT,
                KeyCode::Named(NamedKey::PageUp),
            ),
        );
    }

    #[test]
    fn display_canonicalizes_modifier_order() {
        let chord = Chord::with_mods(
            Modifiers::SUPER | Modifiers::ALT | Modifiers::SHIFT | Modifiers::CONTROL,
            KeyCode::Character("a".into()),
        );
        assert_eq!(chord.to_string(), "ctrl+shift+alt+super+a");
    }

    #[test]
    fn display_writes_named_key_canonical_name() {
        assert_eq!(
            Chord::bare(KeyCode::Named(NamedKey::Escape)).to_string(),
            "escape",
        );
        assert_eq!(
            Chord::bare(KeyCode::Named(NamedKey::PageUp)).to_string(),
            "page_up",
        );
        assert_eq!(
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(12)))).to_string(),
            "f12",
        );
    }

    #[test]
    fn chord_hashes_into_a_hashmap() {
        let mut map: HashMap<Chord, &'static str> = HashMap::new();
        map.insert(
            Chord::with_mods(
                Modifiers::CONTROL | Modifiers::SHIFT,
                KeyCode::Character("f".into()),
            ),
            "search",
        );
        map.insert(
            Chord::with_mods(Modifiers::SHIFT, KeyCode::Named(NamedKey::PageUp)),
            "scroll-up",
        );
        map.insert(
            Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(5)))),
            "refresh",
        );

        assert_eq!(
            map.get(&Chord::with_mods(
                Modifiers::SHIFT | Modifiers::CONTROL,
                KeyCode::Character("f".into()),
            )),
            Some(&"search"),
            "modifier-set bitor order must not change the hash",
        );
        assert_eq!(
            map.get(&Chord::bare(KeyCode::Named(NamedKey::F(FKey::lit(5))))),
            Some(&"refresh"),
        );
        assert_eq!(map.len(), 3);
    }

    fn arb_modifiers() -> impl Strategy<Value = Modifiers> {
        (0u8..=0b1111).prop_map(Modifiers::from_bits_truncate)
    }

    fn arb_named_key() -> impl Strategy<Value = NamedKey> {
        prop_oneof![
            Just(NamedKey::Enter),
            Just(NamedKey::Tab),
            Just(NamedKey::Escape),
            Just(NamedKey::Space),
            Just(NamedKey::Backspace),
            Just(NamedKey::Insert),
            Just(NamedKey::Delete),
            Just(NamedKey::Home),
            Just(NamedKey::End),
            Just(NamedKey::PageUp),
            Just(NamedKey::PageDown),
            Just(NamedKey::ArrowUp),
            Just(NamedKey::ArrowDown),
            Just(NamedKey::ArrowLeft),
            Just(NamedKey::ArrowRight),
            (1u8..=35).prop_map(|n| NamedKey::F(FKey::lit(n))),
        ]
    }

    /// ASCII printable band, so `+` exercises the trailing-`++` path;
    /// uppercase letters are left out because no parsed chord holds one.
    fn arb_character_key() -> impl Strategy<Value = String> {
        prop::char::range('!', '~')
            .prop_filter("uppercase ASCII folds on parse", |c| {
                !c.is_ascii_uppercase()
            })
            .prop_map(|c| c.to_string())
    }

    fn arb_key_code() -> impl Strategy<Value = KeyCode> {
        prop_oneof![
            arb_named_key().prop_map(KeyCode::Named),
            arb_character_key().prop_map(KeyCode::Character),
        ]
    }

    fn arb_chord() -> impl Strategy<Value = Chord> {
        (arb_modifiers(), arb_key_code()).prop_map(|(mods, key)| Chord { mods, key })
    }

    proptest! {
        #[test]
        fn parser_never_panics(s in ".*") {
            let _result = s.parse::<Chord>();
        }

        /// Display then parse yields an equal Chord.
        #[test]
        fn display_then_parse_round_trips(chord in arb_chord()) {
            let canonical = chord.to_string();
            let reparsed: Chord = canonical
                .parse()
                .map_err(|err: ParseChordError| TestCaseError::fail(err.to_string()))?;
            prop_assert_eq!(reparsed, chord);
        }

        /// Pins the string round-trip, catching a Display impl that emits
        /// two valid forms for the same Chord.
        #[test]
        fn display_is_idempotent_via_parse(chord in arb_chord()) {
            let first = chord.to_string();
            let reparsed: Chord = first
                .parse()
                .map_err(|err: ParseChordError| TestCaseError::fail(err.to_string()))?;
            let second = reparsed.to_string();
            prop_assert_eq!(first, second);
        }

        #[test]
        fn an_uppercase_ascii_letter_parses_like_shift_and_its_lowercase(
            mods in arb_modifiers(),
            letter in prop::char::range('A', 'Z'),
        ) {
            let written = Chord::with_mods(mods, KeyCode::Character(letter.to_string())).to_string();
            let folded = Chord::with_mods(
                mods | Modifiers::SHIFT,
                KeyCode::Character(letter.to_ascii_lowercase().to_string()),
            );
            prop_assert_eq!(parse(&written), folded);
        }

        /// Any rotation of the modifier tokens parses to the same Chord.
        #[test]
        fn modifier_token_rotation_is_irrelevant(
            chord in arb_chord(),
            rotate in 0usize..4,
        ) {
            prop_assume!(!chord.mods.is_empty());

            let canonical = chord.to_string();
            let (mods_part, key_part) = if canonical.ends_with('+') && canonical.len() >= 2 {
                let stripped = canonical.trim_end_matches('+');
                (stripped, "+")
            } else {
                let idx = canonical
                    .rfind('+')
                    .ok_or_else(|| TestCaseError::fail("modifier present but no `+` in display"))?;
                (&canonical[..idx], &canonical[idx + 1..])
            };
            let mut tokens: Vec<&str> = mods_part.split('+').collect();
            let n = tokens.len();
            let r = rotate % n;
            tokens.rotate_right(r);
            tokens.push(key_part);
            let rotated = tokens.join("+");

            let reparsed: Chord = rotated
                .parse()
                .map_err(|err: ParseChordError| {
                    TestCaseError::fail(format!("{err} (rotated form was {rotated:?})"))
                })?;
            prop_assert_eq!(reparsed, chord);
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{FKey, Modifiers, NamedKey, decimal_value, modifier_bit, named_key, split_chord};

    /// Every slice comparison is an explicit indexed loop: a derived or
    /// slice `==` lowers to the builtin `memcmp`, whose loop sits outside
    /// `#[kani::unwind]`.
    fn slices_equal(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }

    fn contains_plus(s: &[u8]) -> bool {
        let mut i = 0;
        while i < s.len() {
            if s[i] == b'+' {
                return true;
            }
            i += 1;
        }
        false
    }

    /// The split reconstructs its input exactly, and rejects exactly the
    /// strings that end in a `+` which is not the literal `+` key.
    #[kani::proof]
    #[kani::unwind(10)]
    fn split_chord_partitions_its_input() {
        const N: usize = 8;
        let bytes: [u8; N] = kani::any();
        let len: usize = kani::any();
        kani::assume(1 <= len && len <= N);
        let s = &bytes[..len];

        let dangling_plus = len >= 2 && s[len - 1] == b'+' && s[len - 2] != b'+';

        let Some(split) = split_chord(s) else {
            assert!(dangling_plus);
            return;
        };
        assert!(!dangling_plus);
        assert!(!split.key.is_empty());

        // A key of `+` can only come from the literal-`+` spelling: the
        // key of any other chord starts after the final `+`.
        if slices_equal(split.key, b"+") {
            match split.mods {
                None => assert!(len == 1 && s[0] == b'+'),
                Some(mods) => {
                    assert!(mods.len() + 2 == len);
                    assert!(s[len - 1] == b'+' && s[len - 2] == b'+');
                    assert!(slices_equal(mods, &s[..mods.len()]));
                }
            }
            return;
        }

        assert!(!contains_plus(split.key));
        match split.mods {
            None => assert!(slices_equal(split.key, s)),
            Some(mods) => {
                assert!(mods.len() + 1 + split.key.len() == len);
                assert!(s[mods.len()] == b'+');
                assert!(slices_equal(mods, &s[..mods.len()]));
                assert!(slices_equal(split.key, &s[mods.len() + 1..]));
            }
        }
    }

    /// Modifier and key names are ASCII-case-insensitive over the whole
    /// byte domain, not just the spellings the unit tests list.
    ///
    /// The unwind bound is set by the 15-entry name table, not by the
    /// 10-byte input.
    #[kani::proof]
    #[kani::unwind(17)]
    fn name_lookups_ignore_ascii_case() {
        const N: usize = 10;
        let bytes: [u8; N] = kani::any();
        let len: usize = kani::any();
        kani::assume(len <= N);

        let mut lowered = bytes;
        let mut i = 0;
        while i < N {
            lowered[i] = bytes[i].to_ascii_lowercase();
            i += 1;
        }

        let modifier: Option<Modifiers> = modifier_bit(&bytes[..len]);
        assert!(modifier == modifier_bit(&lowered[..len]));
        let named: Option<NamedKey> = named_key(&bytes[..len]);
        assert!(named == named_key(&lowered[..len]));
    }

    /// Nine digits cannot overflow `u32`, so saturation must be invisible
    /// there: the accumulator agrees with a wider reference.
    #[kani::proof]
    #[kani::unwind(11)]
    fn decimal_value_matches_a_wider_reference() {
        const N: usize = 9;
        let digits: [u8; N] = kani::any();
        let mut i = 0;
        while i < N {
            kani::assume(digits[i].is_ascii_digit());
            i += 1;
        }
        let len: usize = kani::any();
        kani::assume(len <= N);

        let mut reference: u64 = 0;
        let mut j = 0;
        while j < len {
            reference = reference * 10 + u64::from(digits[j] - b'0');
            j += 1;
        }

        assert!(u64::from(decimal_value(&digits[..len])) == reference);
    }

    /// A digit run long enough to overflow `u32` still lands outside
    /// `1..=35`, so no absurd index is ever admitted as an F-key.
    #[kani::proof]
    #[kani::unwind(14)]
    fn f_key_admission_is_exactly_one_through_thirty_five() {
        const N: usize = 12;
        let digits: [u8; N] = kani::any();
        let mut i = 0;
        while i < N {
            kani::assume(digits[i].is_ascii_digit());
            i += 1;
        }
        let len: usize = kani::any();
        kani::assume(len <= N);

        let value = decimal_value(&digits[..len]);
        let admitted = u8::try_from(value).ok().and_then(FKey::new).is_some();
        assert!(admitted == (1 <= value && value <= 35));
    }
}
