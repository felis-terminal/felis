//! Property-based tests for the session-id prefix vocabulary.
//!
//! The client shortens an id for display and the daemon resolves what a
//! user typed back; these are the laws that keep the two halves from
//! disagreeing about what a prefix is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::SessionHex;
use felis_protocol::messages::ResolvedId;
use felis_protocol::session_prefix::{
    SHORT_SESSION_PREFIX_MIN, normalize_session_prefix, resolve_session_prefix,
    short_session_prefix, validate_session_id_prefix,
};
use proptest::prelude::*;

/// Ids whose top 32 bits come from a four-value pool, so a roster
/// collides inside the display floor often enough to exercise the
/// extension path.
fn colliding_id() -> impl Strategy<Value = u128> {
    (0_u128..4, any::<u128>()).prop_map(|(high, low)| (high << 96) | (low >> 32))
}

fn roster() -> impl Strategy<Value = Vec<u128>> {
    prop::collection::btree_set(colliding_id(), 1..8).prop_map(|ids| ids.into_iter().collect())
}

/// Raw argument text: hex, non-hex, case and `0x` forms mixed.
fn raw_prefix() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::string::string_regex("(0x|0X)?[0-9a-fA-F]{0,34}").unwrap(),
        prop::string::string_regex("[ -~]{0,8}").unwrap(),
    ]
}

proptest! {
    /// What a display shows resolves back to the id it was computed for.
    #[test]
    fn a_short_prefix_resolves_to_the_id_it_renders(
        (ids, pick) in roster().prop_flat_map(|ids| {
            let len = ids.len();
            (Just(ids), 0..len)
        }),
    ) {
        let id = ids[pick];
        let short = short_session_prefix(id, ids.clone());
        prop_assert_eq!(resolve_session_prefix(&short, ids), ResolvedId::Ok { id });
    }

    /// A short prefix is a real prefix of the full rendering, never
    /// shorter than the display floor and never longer than the id.
    #[test]
    fn a_short_prefix_is_a_bounded_prefix_of_the_full_hex(
        (ids, pick) in roster().prop_flat_map(|ids| {
            let len = ids.len();
            (Just(ids), 0..len)
        }),
    ) {
        let id = ids[pick];
        let hex = SessionHex(id).to_string();
        let short = short_session_prefix(id, ids);
        prop_assert!(hex.starts_with(&short));
        prop_assert!(short.len() >= SHORT_SESSION_PREFIX_MIN);
        prop_assert!(short.len() <= hex.len());
    }

    /// The resolver's verdict is the count of ids the normalized prefix
    /// selects, and nothing else.
    #[test]
    fn resolution_counts_the_ids_the_prefix_selects(
        ids in roster(),
        raw in raw_prefix(),
    ) {
        let normalized = normalize_session_prefix(&raw);
        let selected: Vec<u128> = if validate_session_id_prefix(&raw).is_ok() {
            ids.iter()
                .copied()
                .filter(|id| SessionHex(*id).to_string().starts_with(&normalized))
                .collect()
        } else {
            Vec::new()
        };
        let expected = match selected.as_slice() {
            [] => ResolvedId::NoMatch,
            [id] => ResolvedId::Ok { id: *id },
            many => ResolvedId::Ambiguous { matches: many.len() as u32 },
        };
        prop_assert_eq!(resolve_session_prefix(&raw, ids), expected);
    }

    /// The two front doors share one vocabulary: what the client's
    /// argument parser accepts is what the daemon will look up, in the
    /// same normalized form, and what it rejects the daemon never
    /// matches.
    #[test]
    fn the_validator_and_the_resolver_admit_the_same_prefixes(
        raw in raw_prefix(),
        id in any::<u128>(),
    ) {
        match validate_session_id_prefix(&raw) {
            Ok(prefix) => {
                prop_assert_eq!(&prefix, &normalize_session_prefix(&raw));
                prop_assert_eq!(
                    resolve_session_prefix(&raw, [id]),
                    resolve_session_prefix(&prefix, [id])
                );
                prop_assert!(!prefix.is_empty() && prefix.len() <= 32);
                prop_assert!(prefix.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            }
            Err(_) => {
                prop_assert_eq!(resolve_session_prefix(&raw, [id]), ResolvedId::NoMatch);
            }
        }
    }
}
