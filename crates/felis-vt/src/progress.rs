//! `ConEmu` `OSC 9 ; 4 ; <st> [; <pr>]` progress decoder (decode-only),
//! per `docs/reference/protocols/terminal-progress.md`.

use crate::split_osc_first;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum ProgressState {
    Normal,
    /// The producer has failed so far; it may still be running.
    Error,
    Indeterminate,
    Paused,
}

impl ProgressState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Error => "error",
            Self::Indeterminate => "indeterminate",
            Self::Paused => "paused",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct Progress {
    pub state: ProgressState,
    /// `0..=100`.
    pub value: Option<u8>,
}

/// `None` for a clear (`st=0`).
pub type ProgressReport = Option<Progress>;

/// The fields after `9 ; 4 ;`; the outer `None` drops the sequence.
#[must_use]
pub fn parse_progress(fields: &[u8]) -> Option<ProgressReport> {
    let (st, rest) = split_osc_first(fields);
    let state = match st {
        b"0" => return Some(None),
        b"3" => {
            return Some(Some(Progress {
                state: ProgressState::Indeterminate,
                value: None,
            }));
        }
        b"1" => ProgressState::Normal,
        b"2" => ProgressState::Error,
        b"4" => ProgressState::Paused,
        _ => return None,
    };
    let pr = rest.map_or(&b""[..], |rest| split_osc_first(rest).0);
    let value = if pr.is_empty() {
        None
    } else {
        Some(percent(pr)?)
    };
    Some(Some(Progress { state, value }))
}

fn percent(digits: &[u8]) -> Option<u8> {
    let mut n: u8 = 0;
    for &b in digits {
        let digit = b.checked_sub(b'0').filter(|d| *d < 10)?;
        n = n.saturating_mul(10).saturating_add(digit).min(100);
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn set(state: ProgressState, value: Option<u8>) -> Progress {
        Progress { state, value }
    }

    #[test]
    fn each_state_code_maps_to_its_state() {
        assert_eq!(
            parse_progress(b"1;42"),
            Some(Some(set(ProgressState::Normal, Some(42))))
        );
        assert_eq!(
            parse_progress(b"2;71"),
            Some(Some(set(ProgressState::Error, Some(71))))
        );
        assert_eq!(
            parse_progress(b"4;5"),
            Some(Some(set(ProgressState::Paused, Some(5))))
        );
        assert_eq!(
            parse_progress(b"3"),
            Some(Some(set(ProgressState::Indeterminate, None)))
        );
    }

    #[test]
    fn st_0_clears_whatever_follows() {
        for fields in [&b"0"[..], b"0;", b"0;0", b"0;junk;more"] {
            assert_eq!(parse_progress(fields), Some(None), "{fields:?}");
        }
    }

    #[test]
    fn st_3_is_indeterminate_whatever_follows() {
        for fields in [&b"3;"[..], b"3;2", b"3;junk"] {
            assert_eq!(
                parse_progress(fields),
                Some(Some(set(ProgressState::Indeterminate, None))),
                "{fields:?}"
            );
        }
    }

    #[test]
    fn a_missing_or_empty_value_is_absent() {
        for fields in [&b"1"[..], b"1;", b"2;", b"4"] {
            assert_eq!(
                parse_progress(fields).flatten().map(|p| p.value),
                Some(None),
                "{fields:?}"
            );
        }
    }

    #[test]
    fn a_value_above_100_is_clamped() {
        assert_eq!(
            parse_progress(b"1;250"),
            Some(Some(set(ProgressState::Normal, Some(100))))
        );
        assert_eq!(
            parse_progress(b"1;99999999999999999999"),
            Some(Some(set(ProgressState::Normal, Some(100))))
        );
        assert_eq!(
            parse_progress(b"2;007"),
            Some(Some(set(ProgressState::Error, Some(7))))
        );
    }

    #[test]
    fn a_malformed_state_or_value_drops_the_sequence() {
        for fields in [
            &b""[..],
            b";50",
            b"5;50",
            b"01;50",
            b"x",
            b"1;-1",
            b"1;5x",
            b"1; 5",
            b"4;+3",
        ] {
            assert_eq!(parse_progress(fields), None, "{fields:?}");
        }
    }
}
