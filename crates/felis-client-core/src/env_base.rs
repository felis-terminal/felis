//! Capturing this process's environment for `SpawnArgs.env_base`.
//!
//! Captures raw platform bytes only over local sockets; over SSH relays the
//! field stays absent (`docs/reference/ipc.md` "Session (kind = 4)").

use felis_protocol::messages::SpawnArgs;

use crate::connector::Carrier;

/// `None` when the snapshot breaks a cap: the daemon refuses an
/// over-cap snapshot outright, which would turn a large environment into
/// a window that will not open. Order carries no meaning; the daemon
/// dedups again under the target platform's name semantics. The warning
/// names counts alone (REQ-912a forbids environment content in logs).
#[must_use]
pub fn capture() -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut seen: std::collections::HashMap<Vec<u8>, usize> = std::collections::HashMap::new();
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let key = platform_bytes(&key);
        let value = platform_bytes(&value);
        if let Some(&at) = seen.get(&key) {
            pairs[at] = (key, value);
        } else {
            seen.insert(key.clone(), pairs.len());
            pairs.push((key, value));
        }
    }
    within_caps(pairs)
}

fn within_caps(pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    use felis_protocol::messages::{MAX_ENV_BASE_BYTES, MAX_ENV_BASE_ENTRIES};

    let bytes: usize = pairs
        .iter()
        .map(|(key, value)| key.len().saturating_add(value.len()))
        .fold(0, usize::saturating_add);
    if pairs.len() > MAX_ENV_BASE_ENTRIES || bytes > MAX_ENV_BASE_BYTES {
        tracing::warn!(
            entries = pairs.len(),
            bytes,
            max_entries = MAX_ENV_BASE_ENTRIES,
            max_bytes = MAX_ENV_BASE_BYTES,
            "environment too large to send with this create; the session will inherit the \
             daemon's environment"
        );
        return None;
    }
    Some(pairs)
}

#[must_use]
pub fn fill_for_carrier(mut args: SpawnArgs, carrier: &Carrier) -> SpawnArgs {
    args.env_base = matches!(carrier, Carrier::Local(_)).then(capture).flatten();
    args
}

#[cfg(unix)]
fn platform_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}

/// `u16` code units, little-endian, unpaired surrogates included: a
/// transparent envelope around what `CreateProcessW` takes, not a field
/// the wire interprets (the preface is big-endian).
#[cfg(windows)]
fn platform_bytes(s: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_local_carrier_carries_an_environment() {
        let local = fill_for_carrier(
            SpawnArgs::default(),
            &Carrier::Local(std::path::PathBuf::from("/run/felis.sock").into()),
        );
        assert!(
            local.env_base.is_some_and(|base| !base.is_empty()),
            "a local dial injects this process's environment"
        );

        let remote = fill_for_carrier(
            SpawnArgs::default(),
            &Carrier::Ssh {
                destination: "host".into(),
                ssh_args: Vec::new(),
            },
        );
        assert_eq!(remote.env_base, None);
    }

    #[test]
    fn an_over_cap_capture_is_dropped_rather_than_sent() {
        use felis_protocol::messages::{MAX_ENV_BASE_BYTES, MAX_ENV_BASE_ENTRIES};

        let too_many: Vec<(Vec<u8>, Vec<u8>)> = (0..=MAX_ENV_BASE_ENTRIES)
            .map(|i| (format!("K{i}").into_bytes(), Vec::new()))
            .collect();
        assert_eq!(within_caps(too_many), None);

        let too_big = vec![(b"BIG".to_vec(), vec![b'x'; MAX_ENV_BASE_BYTES])];
        assert_eq!(within_caps(too_big), None);

        let ordinary = vec![(b"PATH".to_vec(), b"/bin".to_vec())];
        assert_eq!(within_caps(ordinary.clone()), Some(ordinary));
    }

    #[test]
    fn the_capture_carries_the_process_environment() {
        let captured = capture().expect("a test environment fits the caps");
        let want = std::env::var_os("PATH").expect("a test environment has PATH");
        // Not a `b"PATH"` byte compare: on Windows the capture is UTF-16LE
        // and the name is spelled `Path`.
        assert!(
            captured.iter().any(|(key, value)| {
                felis_pty::env_from_bytes(key).is_some_and(|key| key.eq_ignore_ascii_case("PATH"))
                    && felis_pty::env_from_bytes(value).as_ref() == Some(&want)
            }),
            "PATH must survive the platform-bytes round trip"
        );
        let mut keys: Vec<&Vec<u8>> = captured.iter().map(|(key, _)| key).collect();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), before, "keys are deduped at capture");
    }
}
