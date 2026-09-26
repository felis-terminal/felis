//! Session-id prefix vocabulary: the client-side normalize / validate
//! pair and the daemon-side resolver behind `OpsToDaemonMsg`'s `id_prefix`
//! fields, in one module so they cannot disagree on what a prefix is.

use crate::SessionHex;
use crate::messages::ResolvedId;

/// Strip an optional `0x` / `0X` prefix and lowercase.
#[must_use]
pub fn normalize_session_prefix(raw: &str) -> String {
    let s = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    s.to_lowercase()
}

/// Clap value parser for every `<ID-OR-PREFIX>` argument: 1..=32 hex
/// digits after stripping the optional `0x`, returned normalized.
pub fn validate_session_id_prefix(raw: &str) -> Result<String, String> {
    let normalized = normalize_session_prefix(raw);
    if normalized.is_empty() {
        return Err("session id prefix must not be empty".into());
    }
    if normalized.len() > 32 {
        return Err(format!(
            "session id prefix `{normalized}` is longer than 32 hex digits",
        ));
    }
    if !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "session id prefix `{normalized}` must be hex digits only",
        ));
    }
    Ok(normalized)
}

/// Resolve a possibly-truncated, possibly-unnormalized hex prefix
/// against a set of session ids: the daemon's side of every
/// `id_prefix` field. A malformed wire prefix (empty, overlong,
/// non-hex) is [`ResolvedId::NoMatch`]: only a non-felis peer can send
/// one, and no lowercase-hex id can start with it.
pub fn resolve_session_prefix(prefix: &str, ids: impl IntoIterator<Item = u128>) -> ResolvedId {
    let normalized = normalize_session_prefix(prefix);
    if normalized.is_empty()
        || normalized.len() > 32
        || !normalized.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return ResolvedId::NoMatch;
    }
    let mut found: Option<u128> = None;
    let mut matches: u32 = 0;
    for id in ids {
        if SessionHex(id).to_string().starts_with(&normalized) {
            matches += 1;
            found = Some(id);
        }
    }
    match (matches, found) {
        (1, Some(id)) => ResolvedId::Ok { id },
        (0, _) | (_, None) => ResolvedId::NoMatch,
        _ => ResolvedId::Ambiguous { matches },
    }
}

/// Shortest prefix a display ever shows, in hex digits.
pub const SHORT_SESSION_PREFIX_MIN: usize = 8;

/// Generates a unique hex prefix for `id` against other ids in `roster`.
///
/// Extends [`SHORT_SESSION_PREFIX_MIN`] digits until unique. Display only;
/// not persisted across roster changes (`docs/reference/cli.md` "Machine output").
#[must_use]
pub fn short_session_prefix(id: u128, roster: impl IntoIterator<Item = u128>) -> String {
    let hex = SessionHex(id).to_string();
    let mut needed = SHORT_SESSION_PREFIX_MIN;
    for other in roster {
        if other == id {
            continue;
        }
        let other_hex = SessionHex(other).to_string();
        let shared = hex
            .bytes()
            .zip(other_hex.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        needed = needed.max(shared + 1);
    }
    hex.get(..needed.min(hex.len())).unwrap_or(&hex).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prefix_shows_the_floor_when_nothing_collides() {
        let id = 0xCAFE_0000_0000_0000_0000_0000_0000_0001_u128;
        assert_eq!(
            short_session_prefix(id, [id, 0xDEAD_u128]),
            "cafe0000",
            "8 digits is the floor, and the roster's own copy is not a collision"
        );
    }

    #[test]
    fn short_prefix_extends_past_the_floor_until_it_is_unique() {
        let a = 0xCAFE_0000_0000_1000_0000_0000_0000_0000_u128;
        let b = 0xCAFE_0000_0000_2000_0000_0000_0000_0000_u128;
        assert_eq!(short_session_prefix(a, [a, b]), "cafe000000001");
        assert_eq!(short_session_prefix(b, [a, b]), "cafe000000002");
    }

    #[test]
    fn validate_session_id_prefix_accepts_hex_with_and_without_0x_prefix() {
        assert_eq!(validate_session_id_prefix("deadbeef").unwrap(), "deadbeef",);
        assert_eq!(validate_session_id_prefix("DEADBEEF").unwrap(), "deadbeef",);
        assert_eq!(
            validate_session_id_prefix("0xdeadbeef").unwrap(),
            "deadbeef",
        );
        assert_eq!(
            validate_session_id_prefix("0XdeadBEEF").unwrap(),
            "deadbeef",
        );
    }

    #[test]
    fn validate_session_id_prefix_accepts_full_32_char_hex() {
        let full = format!("{:032x}", 1u128);
        assert_eq!(full.len(), 32);
        assert_eq!(validate_session_id_prefix(&full).unwrap(), full);
    }

    #[test]
    fn validate_session_id_prefix_rejects_invalid_input() {
        assert!(validate_session_id_prefix("").is_err());
        assert!(validate_session_id_prefix("not-hex").is_err());
        assert!(validate_session_id_prefix("1234g").is_err());
        let too_long = "f".repeat(33);
        assert!(validate_session_id_prefix(&too_long).is_err());
    }

    #[test]
    fn resolve_session_prefix_finds_the_unique_match() {
        let ids = [0xCAFE_0000_0000_0000_0000_0000_0000_0001_u128, 0xDEAD];
        assert_eq!(
            resolve_session_prefix("cafe", ids),
            ResolvedId::Ok { id: ids[0] }
        );
        assert_eq!(
            resolve_session_prefix("0xCAFE", ids),
            ResolvedId::Ok { id: ids[0] },
        );
    }

    #[test]
    fn resolve_session_prefix_reports_no_match_and_ambiguity() {
        let ids = [
            0xDEAD_0000_0000_0000_0000_0000_0000_0001_u128,
            0xDEAD_0000_0000_0000_0000_0000_0000_0002,
        ];
        assert_eq!(resolve_session_prefix("beef", ids), ResolvedId::NoMatch);
        assert_eq!(
            resolve_session_prefix("dead", ids),
            ResolvedId::Ambiguous { matches: 2 },
        );
    }
}
