//! Client-side session ring: which session a switch chord or a
//! shell-exit fallback lands on, against a roster fetched immediately
//! before the pick (`architecture/session-lifecycle.md` "Picking the
//! session a chord lands on", which argues the sequence ordering and
//! the anchor being a value rather than a position).

use std::num::NonZeroU64;

use felis_protocol::messages::SessionInfo;

use crate::action::SwitchDirection;

/// The id tiebreak keeps the ring total: no two live rows share a
/// sequence, but an anchor the roster has already reaped can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RingKey {
    pub sequence: NonZeroU64,
    pub id: u128,
}

impl RingKey {
    #[must_use]
    pub const fn of(info: &SessionInfo) -> Self {
        Self {
            sequence: info.sequence,
            id: info.id,
        }
    }
}

/// Exited-but-graced sessions are skipped for automatic picks only (a
/// deliberate `switch` / `attach` may still read a final screen): a
/// window landed on a corpse would never be pushed off it, and its
/// subscription blocks the daemon's reap
/// (`architecture/session-lifecycle.md` "Post-exit reaping").
fn candidates(roster: &[SessionInfo], anchor: RingKey) -> impl Iterator<Item = &SessionInfo> {
    roster
        .iter()
        .filter(move |s| !s.exited && s.id != anchor.id)
}

#[must_use]
pub fn pick_switch_target(
    roster: &[SessionInfo],
    anchor: RingKey,
    direction: SwitchDirection,
) -> Option<u128> {
    let key = RingKey::of;
    match direction {
        SwitchDirection::Next => candidates(roster, anchor)
            .filter(|s| key(s) > anchor)
            .min_by_key(|s| key(s))
            .or_else(|| candidates(roster, anchor).min_by_key(|s| key(s))),
        SwitchDirection::Previous => candidates(roster, anchor)
            .filter(|s| key(s) < anchor)
            .max_by_key(|s| key(s))
            .or_else(|| candidates(roster, anchor).max_by_key(|s| key(s))),
    }
    .map(|s| s.id)
}

/// The shell-exit ladder's last rung: the nearest live neighbor of the
/// session that exited. `None` means no live session remains, and the
/// caller closes the window. There is no predecessor preference: the
/// window's trail already expressed where it has been
/// (`architecture/session-lifecycle.md`).
#[must_use]
pub fn pick_exit_switch_target(roster: &[SessionInfo], anchor: RingKey) -> Option<u128> {
    candidates(roster, anchor)
        .min_by_key(|s| {
            let key = RingKey::of(s);
            (ring_distance(key, anchor), key > anchor, key.id)
        })
        .map(|s| s.id)
}

const fn ring_distance(key: RingKey, anchor: RingKey) -> u64 {
    key.sequence.get().abs_diff(anchor.sequence.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(n: u64) -> NonZeroU64 {
        NonZeroU64::new(n).expect("the ring is built from minted sequences")
    }

    fn session(sequence: u64, id: u128) -> SessionInfo {
        SessionInfo {
            id,
            dims: felis_protocol::messages::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            },
            title: None,
            cwd: None,
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments: Vec::new(),
            sequence: seq(sequence),
        }
    }

    fn exited(sequence: u64, id: u128) -> SessionInfo {
        SessionInfo {
            exited: true,
            ..session(sequence, id)
        }
    }

    fn anchor(info: &SessionInfo) -> RingKey {
        RingKey::of(info)
    }

    /// The trail, not the ring pick, is what remembers where the window
    /// has been: this rung only answers "nearest live neighbor".
    #[test]
    fn the_exit_fallback_takes_the_nearest_live_neighbor() {
        let current = session(10, 0xccc);
        let roster = vec![
            session(7, 0xaaa),
            exited(9, 0xbbb),
            current.clone(),
            session(12, 0xddd),
        ];
        // 9 is nearest but exited; 12 is two away, 7 is three away.
        assert_eq!(
            pick_exit_switch_target(&roster, anchor(&current)),
            Some(0xddd)
        );
    }

    #[test]
    fn the_exit_fallback_breaks_distance_ties_toward_the_older_session() {
        let current = session(5, 0xccc);
        let roster = vec![session(3, 0xaaa), current.clone(), session(7, 0xddd)];
        assert_eq!(
            pick_exit_switch_target(&roster, anchor(&current)),
            Some(0xaaa)
        );
    }

    #[test]
    fn a_fully_exited_roster_reports_nowhere_to_go() {
        let current = session(2, 0xccc);
        let roster = vec![exited(1, 0xaaa), current.clone()];
        assert_eq!(pick_exit_switch_target(&roster, anchor(&current)), None);
        assert_eq!(pick_exit_switch_target(&[], anchor(&current)), None);
    }

    proptest::proptest! {
        /// Which session a chord lands on: the ring is the live rows
        /// other than the anchor, ordered by `RingKey`, and a pick is the
        /// anchor's neighbor in that order, wrapping at the end. Listing
        /// order, a reaped anchor, and an anchor past either end are
        /// all cases of it.
        #[test]
        fn a_directional_pick_is_the_anchors_ring_neighbor(
            rows in proptest::collection::vec((1u64..=6, proptest::bool::ANY), 0..8),
            anchor_sequence in 1u64..=7,
            anchor_id in 0u128..=8,
        ) {
            let roster: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(i, &(sequence, dead))| {
                    let id = i as u128 + 1;
                    if dead { exited(sequence, id) } else { session(sequence, id) }
                })
                .collect();
            let anchor = RingKey { sequence: seq(anchor_sequence), id: anchor_id };

            let mut ring: Vec<RingKey> = roster
                .iter()
                .filter(|s| !s.exited && s.id != anchor.id)
                .map(RingKey::of)
                .collect();
            ring.sort_unstable();

            proptest::prop_assert_eq!(
                pick_switch_target(&roster, anchor, SwitchDirection::Next),
                ring.iter().find(|k| **k > anchor).or_else(|| ring.first()).map(|k| k.id),
            );
            proptest::prop_assert_eq!(
                pick_switch_target(&roster, anchor, SwitchDirection::Previous),
                ring.iter().rev().find(|k| **k < anchor).or_else(|| ring.last()).map(|k| k.id),
            );
        }

        /// All three picks answer iff a live session exists, never with a
        /// corpse, even with the anchor absent or out of range.
        #[test]
        fn picks_answer_whenever_a_live_session_remains(
            rows in proptest::collection::vec((1u64..=32, proptest::bool::ANY), 0..10),
            anchor_sequence in 1u64..=40,
        ) {
            let roster: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(i, &(sequence, dead))| {
                    let id = (i as u128 + 1) * 0x100;
                    if dead { exited(sequence, id) } else { session(sequence, id) }
                })
                .collect();
            // Id 0 is outside the generated range: the anchor is always absent.
            let anchor = RingKey { sequence: seq(anchor_sequence), id: 0 };
            let live: Vec<u128> = roster.iter().filter(|s| !s.exited).map(|s| s.id).collect();
            for pick in [
                pick_switch_target(&roster, anchor, SwitchDirection::Next),
                pick_switch_target(&roster, anchor, SwitchDirection::Previous),
                pick_exit_switch_target(&roster, anchor),
            ] {
                proptest::prop_assert_eq!(pick.is_some(), !live.is_empty());
                if let Some(id) = pick {
                    proptest::prop_assert!(live.contains(&id));
                }
            }
        }
    }
}
