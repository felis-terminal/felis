//! Resolving and sanitizing child process environments.
//!
//! Validates, folds, and scrubs environment snapshots against denylists
//! and reserved keys (`docs/explanation/security-model.md`).

use std::borrow::Cow;

use thiserror::Error;

/// One environment pair in the platform's own representation.
pub type EnvEntry = (Vec<u8>, Vec<u8>);

/// Whether children are spawned on a platform whose environment
/// strings are UTF-16 code units compared case-insensitively. The
/// daemon's platform is the target platform: a snapshot only travels
/// between the two ends of a local carrier.
pub(crate) const TARGET_IS_WINDOWS: bool = cfg!(windows);

/// Why a create's environment was refused. Contentless by design: a
/// variant quoting the name or value would put inherited environment
/// into a refusal payload and, through the caller's error reporting,
/// into a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnvError {
    #[error("SpawnArgs: an environment name was empty")]
    EmptyName,
    #[error("SpawnArgs: an environment name contained '='")]
    NameHasEquals,
    /// An embedded NUL would split the directly constructed Windows
    /// environment block into extra entries and smuggle a key past the
    /// reserved check.
    #[error("SpawnArgs: an environment name or value contained a NUL")]
    HasNul,
    #[error("SpawnArgs: an environment entry was not whole UTF-16 code units")]
    OddLength,
    /// A Windows `=`-prefixed pseudo-variable (`=C:`, the per-drive
    /// current directory) is process state the OS maintains, not a
    /// variable to set.
    #[error("SpawnArgs: an environment name was a Windows '='-prefixed pseudo-variable")]
    PseudoVariable,
    #[error(
        "SpawnArgs: env key {0} is reserved — felis owns FELIS_SESSION_ID and the sanitize \
         denylist (docs/reference/terminal-identity.md)"
    )]
    Reserved(&'static str),
    #[error("SpawnArgs: two env names are the same variable on this platform")]
    NameCollision,
    #[error("SpawnArgs: env_base {found} {unit} exceeds the limit of {cap}")]
    OverCap {
        found: usize,
        cap: usize,
        unit: CapUnit,
    },
}

/// What an [`EnvError::OverCap`] counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapUnit {
    Entries,
    Bytes,
}

impl std::fmt::Display for CapUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Entries => "entries",
            Self::Bytes => "bytes",
        })
    }
}

/// Canonicalize a name under `windows` semantics. ASCII-only folding:
/// it must match both Win32's invariant-locale compare and
/// `felis_pty::Command`'s key fold, so a key found reserved here is the
/// same entry a later override reaches.
#[must_use]
pub(crate) fn canonical_name(name: &str, windows: bool) -> Cow<'_, str> {
    if windows && name.bytes().any(|b| b.is_ascii_lowercase()) {
        Cow::Owned(name.to_ascii_uppercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// [`canonical_name`] for a raw platform name (bytes on Unix, LE `u16`
/// code units on Windows). The fold walks code units: a byte-wise fold
/// would turn U+6141 into U+4141.
#[must_use]
pub(crate) fn canonical_name_bytes(name: &[u8], windows: bool) -> Vec<u8> {
    if !windows {
        return name.to_vec();
    }
    name.chunks(2)
        .flat_map(|unit| match unit {
            [lo, 0] => [lo.to_ascii_uppercase(), 0],
            [lo, hi] => [*lo, *hi],
            // Only an odd-length name reaches here, and `check_name` refuses
            // those first.
            other => [other[0], 0],
        })
        .collect()
}

/// The value a base gives `name` under the target platform's name
/// semantics, later entries winning. Canonicalizes both sides itself:
/// the identity hatch ([`crate::TermHatch::from_base`]) reads keys the
/// scrub removes, so it runs on an unsanitized base.
#[must_use]
pub fn lookup<'a>(entries: &'a [EnvEntry], name: &str, windows: bool) -> Option<&'a [u8]> {
    let wanted = canonical_name_bytes(&literal_name_bytes(name, windows), windows);
    entries
        .iter()
        .rev()
        .find(|(key, _)| canonical_name_bytes(key, windows) == wanted)
        .map(|(_, value)| value.as_slice())
}

#[must_use]
fn literal_name_bytes(name: &str, windows: bool) -> Vec<u8> {
    if windows {
        name.encode_utf16().flat_map(u16::to_le_bytes).collect()
    } else {
        name.as_bytes().to_vec()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameKind {
    Ordinary,
    Pseudo,
}

fn units(raw: &[u8], windows: bool) -> Result<Vec<u16>, EnvError> {
    if !windows {
        return Ok(raw.iter().map(|&b| u16::from(b)).collect());
    }
    if !raw.len().is_multiple_of(2) {
        return Err(EnvError::OddLength);
    }
    Ok(raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .collect())
}

pub(crate) fn check_name(raw: &[u8], windows: bool) -> Result<NameKind, EnvError> {
    let units = units(raw, windows)?;
    let Some((&first, rest)) = units.split_first() else {
        return Err(EnvError::EmptyName);
    };
    if units.contains(&0) {
        return Err(EnvError::HasNul);
    }
    // The leading `=` is checked before the general one: on Windows it
    // identifies a pseudo-variable, and refusing it would fail a create
    // over state the OS put in the environment itself.
    if windows && first == u16::from(b'=') {
        return if rest.contains(&u16::from(b'=')) {
            Err(EnvError::NameHasEquals)
        } else {
            Ok(NameKind::Pseudo)
        };
    }
    if units.contains(&u16::from(b'=')) {
        return Err(EnvError::NameHasEquals);
    }
    Ok(NameKind::Ordinary)
}

pub(crate) fn check_value(raw: &[u8], windows: bool) -> Result<(), EnvError> {
    if units(raw, windows)?.contains(&0) {
        return Err(EnvError::HasNul);
    }
    Ok(())
}

/// Sanitize a captured base into the entries a child may receive.
///
/// # Errors
/// [`EnvError`] for a cap breach or an entry the target platform
/// cannot represent; every other disposition is a silent drop.
pub(crate) fn sanitize_base(
    entries: &[EnvEntry],
    windows: bool,
) -> Result<Vec<EnvEntry>, EnvError> {
    use felis_protocol::messages::{MAX_ENV_BASE_BYTES, MAX_ENV_BASE_ENTRIES};

    if entries.len() > MAX_ENV_BASE_ENTRIES {
        return Err(EnvError::OverCap {
            found: entries.len(),
            cap: MAX_ENV_BASE_ENTRIES,
            unit: CapUnit::Entries,
        });
    }
    let bytes: usize = entries
        .iter()
        .map(|(name, value)| name.len().saturating_add(value.len()))
        .fold(0, usize::saturating_add);
    if bytes > MAX_ENV_BASE_BYTES {
        return Err(EnvError::OverCap {
            found: bytes,
            cap: MAX_ENV_BASE_BYTES,
            unit: CapUnit::Bytes,
        });
    }

    let reserved: Vec<Vec<u8>> = crate::reserved_env_keys()
        .iter()
        .map(|key| literal_name_bytes(key, windows))
        .collect();
    let mut resolved: Vec<EnvEntry> = Vec::new();
    for (name, value) in entries {
        if check_name(name, windows)? == NameKind::Pseudo {
            continue;
        }
        check_value(value, windows)?;
        let name = canonical_name_bytes(name, windows);
        if reserved.contains(&name) {
            continue;
        }
        match resolved.iter_mut().find(|(seen, _)| *seen == name) {
            // Later wins: a capture can hand over two spellings of one Windows
            // variable, and a child cannot rely on whichever a map happened to
            // keep.
            Some(slot) => slot.1.clone_from(value),
            None => resolved.push((name, value.clone())),
        }
    }
    Ok(resolved)
}

/// Validate explicit `SpawnArgs.env` pairs (REQ-912).
///
/// # Errors
/// Returns [`EnvError`] on violation, without echoing the caller's key spelling.
pub(crate) fn check_explicit(env: &[(String, String)], windows: bool) -> Result<(), EnvError> {
    let mut seen: Vec<Cow<'_, str>> = Vec::with_capacity(env.len());
    for (key, value) in env {
        if key.is_empty() {
            return Err(EnvError::EmptyName);
        }
        if key.contains('\0') || value.contains('\0') {
            return Err(EnvError::HasNul);
        }
        if windows && key.starts_with('=') {
            return Err(EnvError::PseudoVariable);
        }
        if key.contains('=') {
            return Err(EnvError::NameHasEquals);
        }
        let canonical = canonical_name(key, windows);
        if let Some(owned) = crate::reserved_env_keys()
            .iter()
            .copied()
            .find(|reserved| *reserved == canonical.as_ref())
        {
            return Err(EnvError::Reserved(owned));
        }
        if seen.contains(&canonical) {
            return Err(EnvError::NameCollision);
        }
        seen.push(canonical);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// Parameterized rather than `cfg`-gated so the Windows branch runs
    /// on every host.
    #[test]
    fn canonicalization_follows_the_target_platforms_name_semantics() {
        assert_eq!(canonical_name("felis_session_id", true), "FELIS_SESSION_ID");
        assert_eq!(
            canonical_name("felis_session_id", false),
            "felis_session_id"
        );
        assert_eq!(canonical_name("Path", true), "PATH");
        assert_eq!(canonical_name("Path", false), "Path");

        assert_eq!(canonical_name_bytes(b"Path", false), b"Path".to_vec());
        assert_eq!(canonical_name_bytes(&wide("Path"), true), wide("PATH"));
    }

    /// A caller-supplied `FELIS_SOCKET` would redirect every carrier-less
    /// verb in the session; ignoring it silently would look like honoring
    /// it.
    #[test]
    fn a_supplied_daemon_address_is_refused() {
        assert_eq!(
            check_explicit(
                &[("FELIS_SOCKET".into(), "/tmp/elsewhere.sock".into())],
                false
            )
            .unwrap_err(),
            EnvError::Reserved("FELIS_SOCKET")
        );
        assert_eq!(
            check_explicit(
                &[("felis_socket".into(), "/tmp/elsewhere.sock".into())],
                true
            )
            .unwrap_err(),
            EnvError::Reserved("FELIS_SOCKET")
        );
    }

    /// A base captured inside another felis window carries that window's
    /// address.
    #[test]
    fn an_inherited_daemon_address_is_scrubbed_from_a_base() {
        let base = vec![(
            b"FELIS_SOCKET".to_vec(),
            b"/run/felis-elsewhere/daemon.sock".to_vec(),
        )];
        assert_eq!(
            sanitize_base(&base, false).unwrap(),
            Vec::<(Vec<u8>, Vec<u8>)>::new()
        );
    }

    /// The last spelling wins, the same rule the dedup follows.
    #[test]
    fn a_lookup_folds_the_name_and_takes_the_last_entry() {
        let base = vec![
            (b"SHELL".to_vec(), b"/bin/first".to_vec()),
            (b"SHELL".to_vec(), b"/bin/last".to_vec()),
        ];
        assert_eq!(lookup(&base, "SHELL", false), Some(b"/bin/last".as_slice()));
        assert_eq!(lookup(&base, "shell", false), None);

        let windows_base = vec![(wide("Shell"), wide("pwsh.exe"))];
        assert_eq!(
            lookup(&windows_base, "SHELL", true),
            Some(wide("pwsh.exe").as_slice())
        );
    }

    /// U+6141's LE bytes are `41 61`; a byte-wise fold would rewrite the
    /// character.
    #[test]
    fn the_windows_fold_walks_code_units_not_bytes() {
        let name = 0x6141_u16.to_le_bytes().to_vec();
        assert_eq!(canonical_name_bytes(&name, true), name);
    }

    /// Canonicalization must run before the reserved check.
    #[test]
    fn a_case_varied_reserved_key_is_refused_under_windows_semantics() {
        let env = vec![("felis_session_id".to_string(), "beef".to_string())];
        assert_eq!(
            check_explicit(&env, true).unwrap_err(),
            EnvError::Reserved("FELIS_SESSION_ID")
        );
        assert_eq!(check_explicit(&env, false), Ok(()));

        assert_eq!(
            check_explicit(&[("FELIS_SESSION_ID".into(), "beef".into())], false).unwrap_err(),
            EnvError::Reserved("FELIS_SESSION_ID")
        );
        assert_eq!(
            check_explicit(&[("vte_version".into(), "1".into())], true).unwrap_err(),
            EnvError::Reserved("VTE_VERSION")
        );
    }

    #[test]
    fn colliding_explicit_names_refuse_the_spawn() {
        let env = vec![
            ("Path".to_string(), "/a".to_string()),
            ("PATH".to_string(), "/b".to_string()),
        ];
        assert_eq!(
            check_explicit(&env, true).unwrap_err(),
            EnvError::NameCollision
        );
        assert_eq!(check_explicit(&env, false), Ok(()));
    }

    #[test]
    fn platform_validity_is_enforced_on_both_sources() {
        assert_eq!(check_name(b"", false), Err(EnvError::EmptyName));
        assert_eq!(check_name(b"A=B", false), Err(EnvError::NameHasEquals));
        assert_eq!(check_name(b"A\0B", false), Err(EnvError::HasNul));
        assert_eq!(check_value(b"a\0b", false), Err(EnvError::HasNul));
        assert_eq!(check_name(b"PATH", false), Ok(NameKind::Ordinary));

        assert_eq!(check_name(b"ABC", true), Err(EnvError::OddLength));
        assert_eq!(check_value(b"abc", true), Err(EnvError::OddLength));
        assert_eq!(check_name(&wide("=C:"), true), Ok(NameKind::Pseudo));
        assert_eq!(check_name(b"=C:", false), Err(EnvError::NameHasEquals));
        assert_eq!(check_name(&wide("PATH"), true), Ok(NameKind::Ordinary));
    }

    #[test]
    fn a_base_is_scrubbed_silently() {
        let base = vec![
            (b"PATH".to_vec(), b"/bin".to_vec()),
            (b"FELIS_SESSION_ID".to_vec(), b"stale".to_vec()),
            (b"VTE_VERSION".to_vec(), b"6003".to_vec()),
        ];
        assert_eq!(
            sanitize_base(&base, false).unwrap(),
            vec![(b"PATH".to_vec(), b"/bin".to_vec())]
        );

        let windows_base = vec![
            (wide("=C:"), wide("C:\\src")),
            (wide("felis_session_id"), wide("stale")),
            (wide("Path"), wide("C:\\bin")),
        ];
        assert_eq!(
            sanitize_base(&windows_base, true).unwrap(),
            vec![(wide("PATH"), wide("C:\\bin"))]
        );
    }

    /// Two spellings of one Windows variable: the later wins.
    #[test]
    fn colliding_base_entries_dedup_with_the_later_winning() {
        let base = vec![
            (wide("Path"), wide("/first")),
            (wide("PATH"), wide("/second")),
        ];
        assert_eq!(
            sanitize_base(&base, true).unwrap(),
            vec![(wide("PATH"), wide("/second"))]
        );
        let base = vec![
            (b"Path".to_vec(), b"/first".to_vec()),
            (b"PATH".to_vec(), b"/second".to_vec()),
        ];
        assert_eq!(sanitize_base(&base, false).unwrap().len(), 2);
    }

    /// An unrepresentable entry is evidence the framed field is wrong,
    /// not inherited noise.
    #[test]
    fn an_invalid_base_entry_refuses_the_create() {
        let base = vec![(b"A=B".to_vec(), b"1".to_vec())];
        assert_eq!(
            sanitize_base(&base, false).unwrap_err(),
            EnvError::NameHasEquals
        );
    }

    #[test]
    fn the_base_caps_are_enforced() {
        use felis_protocol::messages::{MAX_ENV_BASE_BYTES, MAX_ENV_BASE_ENTRIES};

        let many: Vec<(Vec<u8>, Vec<u8>)> = (0..=MAX_ENV_BASE_ENTRIES)
            .map(|i| (format!("K{i}").into_bytes(), Vec::new()))
            .collect();
        assert_eq!(
            sanitize_base(&many, false).unwrap_err(),
            EnvError::OverCap {
                found: MAX_ENV_BASE_ENTRIES + 1,
                cap: MAX_ENV_BASE_ENTRIES,
                unit: CapUnit::Entries,
            }
        );

        let big = vec![(b"BIG".to_vec(), vec![b'x'; MAX_ENV_BASE_BYTES])];
        assert!(matches!(
            sanitize_base(&big, false).unwrap_err(),
            EnvError::OverCap {
                unit: CapUnit::Bytes,
                ..
            }
        ));
    }

    #[test]
    fn over_cap_names_the_counted_unit_in_its_message() {
        let entries = EnvError::OverCap {
            found: 3,
            cap: 2,
            unit: CapUnit::Entries,
        };
        let bytes = EnvError::OverCap {
            found: 30,
            cap: 20,
            unit: CapUnit::Bytes,
        };
        assert_eq!(
            entries.to_string(),
            "SpawnArgs: env_base 3 entries exceeds the limit of 2"
        );
        assert_eq!(
            bytes.to_string(),
            "SpawnArgs: env_base 30 bytes exceeds the limit of 20"
        );
    }

    /// No variant's rendered text carries a caller's name or value.
    #[test]
    fn no_refusal_carries_environment_content() {
        let secret = "s3cr3t-token-value";
        let errors = [
            check_explicit(&[(format!("MY_KEY{secret}"), secret.into())], false),
            check_explicit(&[(format!("A={secret}"), secret.into())], false),
            check_explicit(&[(format!("\0{secret}"), secret.into())], false),
            check_explicit(
                &[
                    (format!("Dup{secret}"), secret.into()),
                    (format!("DUP{secret}"), secret.into()),
                ],
                true,
            ),
        ];
        for err in errors.into_iter().filter_map(Result::err) {
            assert!(!err.to_string().contains(secret), "{err}");
        }
        let base_err = sanitize_base(
            &[(format!("A={secret}").into_bytes(), secret.into())],
            false,
        )
        .unwrap_err();
        assert!(!base_err.to_string().contains(secret), "{base_err}");
    }
}
