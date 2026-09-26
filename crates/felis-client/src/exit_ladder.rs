//! The window's shell-exit ladder
//! (`architecture/session-lifecycle.md` "The exit ladder"). Every decision it makes lives here as a value, so
//! the ordering and the budget can be tested without a winit loop.

use std::time::{Duration, Instant};

use felis_client_core::{RECONNECT_ATTEMPT_TIMEOUT, Reconnector, roster::RingKey};

use crate::TrailRecord;

/// Whether a landing records the place it is leaving. A transient is
/// never a place to return to, so a chain of pipe/run hops collapses
/// into the one return the originating place stands for; and a source
/// that died while the landing was in flight is a corpse an automatic
/// rung must never land on.
pub(crate) const fn pushes_trail(
    record: TrailRecord,
    leaving_a_transient: bool,
    source_gone: bool,
) -> bool {
    matches!(record, TrailRecord::Push | TrailRecord::PushIntoTransient)
        && !leaving_a_transient
        && !source_gone
}

/// Whether the window sits on a pipe/run transient once this landing
/// installs. The reconnect answer is the standing one: it re-attaches
/// the place the window never left, and reading it as a fresh
/// non-transient landing would end a visit whose handoff is still
/// parked, letting the next chord push a transient onto the trail.
pub(crate) const fn transient_after(record: TrailRecord, was_on_transient: bool) -> bool {
    match record {
        TrailRecord::Reconnect => was_on_transient,
        TrailRecord::Keep | TrailRecord::Push | TrailRecord::CarryHandoff => false,
        TrailRecord::PushIntoTransient => true,
    }
}

/// Whether the parked pipe/run handoff outlives this landing.
///
/// Only chain re-points continue the visit. Other landings settle the handoff
/// so stale viewports do not apply to subsequent sessions.
pub(crate) const fn carries_handoff(record: TrailRecord) -> bool {
    matches!(record, TrailRecord::CarryHandoff)
}

/// Classifies an intent-requested landing.
///
/// Landings reached from a pipe/run visit form part of the same chain (`scrollback.md`):
/// they push nothing and carry the parked handoff forward for return restoration.
pub(crate) const fn intent_record(on_transient: bool, settled: TrailRecord) -> TrailRecord {
    if on_transient {
        TrailRecord::CarryHandoff
    } else {
        settled
    }
}

/// One session on one daemon. The reconnector is the id's namespace, so
/// two entries name the same place only when both halves match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) reconnector: Reconnector,
    pub(crate) session_id: u128,
}

/// The ring rung's target, distinct from a [`Place`]: the pick needs the
/// exited row's creation sequence, which survives the row's reap only
/// because the window carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExitAnchor {
    pub(crate) reconnector: Reconnector,
    pub(crate) ring_key: RingKey,
}

/// Deep enough that an ordinary session's hops all fit, shallow enough
/// that a trail of dead remote entries cannot cost more ssh dials than
/// the ladder's budget can pay for.
pub(crate) const TRAIL_CAPACITY: usize = 16;

/// The places this window has been, oldest first, each appearing once.
/// Overflow evicts the oldest: the newest is the one a ctrl-D is about
/// to want.
#[derive(Debug, Default)]
pub(crate) struct Trail {
    places: Vec<Place>,
}

impl Trail {
    pub(crate) fn push(&mut self, place: Place) {
        // Without this an A↔B ping-pong fills every slot with two
        // places and evicts the history a deeper unwind wants.
        self.places.retain(|entry| entry != &place);
        if self.places.len() == TRAIL_CAPACITY {
            self.places.remove(0);
        }
        self.places.push(place);
    }

    pub(crate) fn pop(&mut self) -> Option<Place> {
        self.places.pop()
    }

    #[cfg(test)]
    pub(crate) fn newest(&self) -> Option<&Place> {
        self.places.last()
    }

    pub(crate) fn prune(&mut self, place: &Place) {
        self.places.retain(|entry| entry != place);
    }

    #[cfg(test)]
    pub(crate) const fn len(&self) -> usize {
        self.places.len()
    }
}

/// The whole ladder's own attempts, minus the ring rung: an exit must
/// reach a terminal state in a time a user can wait out.
pub(crate) const EXIT_LADDER_BUDGET: Duration = Duration::from_secs(30);

/// One unwind, from the exit that started it to the landing that ends
/// it.
#[derive(Debug)]
pub(crate) struct ExitLadder {
    deadline: Instant,
    anchor: ExitAnchor,
    /// Spent once per ladder, so a ring pick whose own landing fails
    /// reaches the close instead of picking forever.
    ring_spent: bool,
}

/// What the ladder does next; each carries the bound its dial runs
/// under.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rung {
    /// A push the daemon already reported delivered, which outranks
    /// every automatic rung.
    Intent {
        budget: Duration,
    },
    Trail {
        place: Place,
        budget: Duration,
    },
    Ring {
        anchor: ExitAnchor,
        budget: Duration,
    },
    Close,
}

impl ExitLadder {
    /// `now` is when the ladder actually begins, not when the exit was
    /// observed: a user landing that carried the exit may have run for
    /// longer than the budget, and the window's own attempts have not
    /// started until that landing settles.
    pub(crate) fn begin(now: Instant, anchor: ExitAnchor) -> Self {
        Self {
            deadline: now + EXIT_LADDER_BUDGET,
            anchor,
            ring_spent: false,
        }
    }

    pub(crate) fn next(&mut self, now: Instant, trail: &mut Trail, intent_parked: bool) -> Rung {
        if intent_parked {
            return Rung::Intent {
                budget: self.budget(now),
            };
        }
        if now < self.deadline
            && let Some(place) = trail.pop()
        {
            return Rung::Trail {
                place,
                budget: self.budget(now),
            };
        }
        if self.ring_spent {
            return Rung::Close;
        }
        self.ring_spent = true;
        Rung::Ring {
            anchor: self.anchor.clone(),
            // Outside the ladder's budget by design: a trail that ate
            // the clock must not cost the one attempt at the daemon the
            // window is already on.
            budget: RECONNECT_ATTEMPT_TIMEOUT,
        }
    }

    fn budget(&self, now: Instant) -> Duration {
        RECONNECT_ATTEMPT_TIMEOUT.min(self.deadline.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use felis_client_core::{Carrier, Offer};

    fn reconnector(socket: &str) -> Reconnector {
        Reconnector {
            carrier: Carrier::Local(std::path::PathBuf::from(socket).into()),
            offer: Offer::window(true),
        }
    }

    fn place(socket: &str, session_id: u128) -> Place {
        Place {
            reconnector: reconnector(socket),
            session_id,
        }
    }

    fn anchor() -> ExitAnchor {
        ExitAnchor {
            reconnector: reconnector("/tmp/a.sock"),
            ring_key: RingKey {
                sequence: std::num::NonZeroU64::new(4).expect("a minted sequence"),
                id: 0xdead,
            },
        }
    }

    #[test]
    fn only_a_user_landing_from_a_live_non_transient_place_pushes() {
        assert!(pushes_trail(TrailRecord::Push, false, false));
        assert!(pushes_trail(TrailRecord::PushIntoTransient, false, false));

        // Every exit-driven rung, the transport reconnect, and the
        // transient's own return.
        assert!(!pushes_trail(TrailRecord::Keep, false, false));
        // Leaving a transient: the originating place is already on the
        // trail and the transient itself is not one.
        assert!(!pushes_trail(TrailRecord::Push, true, false));
        assert!(!pushes_trail(TrailRecord::PushIntoTransient, true, false));
        // The source exited while this landing was in flight.
        assert!(!pushes_trail(TrailRecord::Push, false, true));
        assert!(!pushes_trail(TrailRecord::PushIntoTransient, false, true));
    }

    #[test]
    fn only_the_window_s_own_transient_switch_in_lands_on_a_transient() {
        for was in [false, true] {
            assert!(transient_after(TrailRecord::PushIntoTransient, was));
            // A trail or ring rung lands the window back on a real
            // place, which ends the visit and lets the next chord push.
            assert!(!transient_after(TrailRecord::Keep, was));
            assert!(!transient_after(TrailRecord::Push, was));
            // The chain's own re-point leaves the transient for a
            // session the window cannot tell from any other, so the
            // user's moves out of it are recorded again.
            assert!(!transient_after(TrailRecord::CarryHandoff, was));
        }
    }

    /// Re-attaching the place the window never left says nothing about
    /// whether it is inside a pipe/run visit, and a reconnect that
    /// ended one would let the next chord push a transient onto the
    /// trail and drop the handoff still parked against it.
    #[test]
    fn a_transport_reconnect_leaves_the_visit_as_it_found_it() {
        assert!(transient_after(TrailRecord::Reconnect, true));
        assert!(!transient_after(TrailRecord::Reconnect, false));
    }

    /// A reconnect is not a landing the user made, so it records
    /// nothing either, however many times the carrier drops.
    #[test]
    fn a_transport_reconnect_records_nothing() {
        for was in [false, true] {
            assert!(!pushes_trail(TrailRecord::Reconnect, was, false));
            assert!(!pushes_trail(TrailRecord::Reconnect, was, true));
        }
    }

    #[test]
    fn an_intent_reached_from_a_pipe_run_visit_carries_the_handoff() {
        assert_eq!(
            intent_record(true, TrailRecord::Keep),
            TrailRecord::CarryHandoff
        );
        assert_eq!(
            intent_record(true, TrailRecord::Push),
            TrailRecord::CarryHandoff
        );
        assert_eq!(intent_record(false, TrailRecord::Keep), TrailRecord::Keep);
        assert_eq!(intent_record(false, TrailRecord::Push), TrailRecord::Push);
    }

    /// The re-point is the one landing whose target the window cannot
    /// judge, so it is the one that keeps a handoff parked on a place it
    /// did not match.
    #[test]
    fn only_the_chain_s_re_point_carries_the_handoff_past_its_own_landing() {
        assert!(carries_handoff(TrailRecord::CarryHandoff));
        assert!(!carries_handoff(TrailRecord::PushIntoTransient));
        assert!(!carries_handoff(TrailRecord::Keep));
        assert!(!carries_handoff(TrailRecord::Push));
        assert!(!carries_handoff(TrailRecord::Reconnect));
    }

    /// The bug a stale transient mark causes: the window sits on the
    /// ordinary session an intent named, and the user's own move out of
    /// it has to reach the trail like any other.
    #[test]
    fn a_move_out_of_the_session_an_intent_named_still_pushes() {
        let landed_by_intent = intent_record(true, TrailRecord::Push);
        let on_transient = transient_after(landed_by_intent, true);
        assert!(!on_transient);
        assert!(pushes_trail(TrailRecord::Push, on_transient, false));
    }

    /// Why the pipe/run handoff captures its place when the visit starts
    /// instead of reading the trail when it ends: the source's own
    /// `SessionExited` prunes it, and the newest entry is then somewhere
    /// else entirely.
    #[test]
    fn pruning_the_source_leaves_an_unrelated_place_newest() {
        let source = place("/tmp/a.sock", 0xaaa);
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 0x111));
        trail.push(source.clone());
        trail.prune(&source);
        assert_eq!(trail.newest(), Some(&place("/tmp/a.sock", 0x111)));
    }

    #[test]
    fn the_trail_pops_newest_first() {
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        trail.push(place("/tmp/b.sock", 2));
        assert_eq!(trail.pop(), Some(place("/tmp/b.sock", 2)));
        assert_eq!(trail.pop(), Some(place("/tmp/a.sock", 1)));
        assert_eq!(trail.pop(), None);
    }

    #[test]
    fn the_same_id_on_another_daemon_is_another_place() {
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 7));
        trail.push(place("/tmp/b.sock", 7));
        trail.prune(&place("/tmp/b.sock", 7));
        assert_eq!(trail.newest(), Some(&place("/tmp/a.sock", 7)));
    }

    #[test]
    fn pruning_removes_the_entry_naming_the_dead_place() {
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        trail.push(place("/tmp/a.sock", 2));
        trail.prune(&place("/tmp/a.sock", 1));
        assert_eq!(trail.len(), 1);
        assert_eq!(trail.newest(), Some(&place("/tmp/a.sock", 2)));
    }

    /// A window bouncing between two sessions must not lose the places
    /// it reached before them.
    #[test]
    fn a_repeated_place_moves_to_the_top_instead_of_stacking_up() {
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        trail.push(place("/tmp/a.sock", 2));
        for _ in 0..TRAIL_CAPACITY {
            trail.push(place("/tmp/a.sock", 3));
            trail.push(place("/tmp/a.sock", 4));
        }
        assert_eq!(trail.len(), 4);
        assert_eq!(trail.pop(), Some(place("/tmp/a.sock", 4)));
        assert_eq!(trail.pop(), Some(place("/tmp/a.sock", 3)));
        assert_eq!(
            trail.pop(),
            Some(place("/tmp/a.sock", 2)),
            "the older history survives the ping-pong"
        );
        assert_eq!(trail.pop(), Some(place("/tmp/a.sock", 1)));
    }

    /// The newest entry is the one the next ctrl-D wants, so the cap
    /// drops from the far end.
    #[test]
    fn overflow_evicts_the_oldest_entry() {
        let mut trail = Trail::default();
        for id in 0..u128::try_from(TRAIL_CAPACITY).unwrap() + 3 {
            trail.push(place("/tmp/a.sock", id));
        }
        assert_eq!(trail.len(), TRAIL_CAPACITY);
        assert_eq!(
            trail.newest(),
            Some(&place(
                "/tmp/a.sock",
                u128::try_from(TRAIL_CAPACITY).unwrap() + 2
            ))
        );
        assert_eq!(trail.pop().map(|p| p.session_id), Some(18));
    }

    #[test]
    fn the_ladder_walks_the_trail_then_the_ring_then_closes() {
        let now = Instant::now();
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        trail.push(place("/tmp/b.sock", 2));
        let mut ladder = ExitLadder::begin(now, anchor());

        assert_eq!(
            ladder.next(now, &mut trail, false),
            Rung::Trail {
                place: place("/tmp/b.sock", 2),
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
        assert_eq!(
            ladder.next(now, &mut trail, false),
            Rung::Trail {
                place: place("/tmp/a.sock", 1),
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
        assert_eq!(
            ladder.next(now, &mut trail, false),
            Rung::Ring {
                anchor: anchor(),
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
        assert_eq!(ladder.next(now, &mut trail, false), Rung::Close);
    }

    /// The chain the ssh feel promises: desktop → devbox → prod unwinds
    /// to devbox, then to desktop, one hop per exit, and a refused entry
    /// is discarded rather than retried.
    #[test]
    fn a_nested_chain_unwinds_one_hop_at_a_time() {
        let desktop = place("/tmp/desktop.sock", 0xa);
        let devbox = place("/tmp/devbox.sock", 0xb);
        let mut trail = Trail::default();
        trail.push(desktop.clone());
        trail.push(devbox.clone());

        let now = Instant::now();
        let mut first = ExitLadder::begin(now, anchor());
        assert_eq!(
            first.next(now, &mut trail, false),
            Rung::Trail {
                place: devbox,
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );

        // The landing on devbox ends that ladder; the next exit starts
        // its own and finds the entry below.
        let mut second = ExitLadder::begin(now, anchor());
        assert_eq!(
            second.next(now, &mut trail, false),
            Rung::Trail {
                place: desktop,
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
        assert_eq!(trail.len(), 0, "a consumed entry is never re-pushed");
    }

    /// A rung whose dial never started reports no failure of its own,
    /// so the ladder has to fall through on the spot rather than wait
    /// for a `SessionSwitchFailed` that will not come.
    #[test]
    fn a_rung_that_never_started_falls_through_to_the_next_one() {
        let now = Instant::now();
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        let mut ladder = ExitLadder::begin(now, anchor());

        assert!(matches!(
            ladder.next(now, &mut trail, true),
            Rung::Intent { .. }
        ));
        // The slot is empty now: the caller consumed the intent and its
        // setup failed synchronously.
        assert!(matches!(
            ladder.next(now, &mut trail, false),
            Rung::Trail { .. }
        ));
    }

    /// A ring pick whose own landing fails must reach the close, or the
    /// window picks forever.
    #[test]
    fn the_ring_rung_is_spent_once_per_ladder() {
        let now = Instant::now();
        let mut trail = Trail::default();
        let mut ladder = ExitLadder::begin(now, anchor());
        assert!(matches!(
            ladder.next(now, &mut trail, false),
            Rung::Ring { .. }
        ));
        assert_eq!(ladder.next(now, &mut trail, false), Rung::Close);
    }

    /// A rung near the deadline gets what is left of it, never a fresh
    /// attempt timeout.
    #[test]
    fn a_trail_rung_is_bounded_by_whatever_is_left_of_the_budget() {
        let now = Instant::now();
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        let mut ladder = ExitLadder::begin(now, anchor());
        let left = Duration::from_secs(3);
        let late = now + EXIT_LADDER_BUDGET.checked_sub(left).expect("3s of budget");
        assert_eq!(
            ladder.next(late, &mut trail, false),
            Rung::Trail {
                place: place("/tmp/a.sock", 1),
                budget: left,
            }
        );
    }

    /// The remaining entries are skipped, and the one rung outside the
    /// budget still runs with a whole attempt of its own.
    #[test]
    fn an_expired_budget_skips_the_trail_and_still_takes_the_ring_rung() {
        let now = Instant::now();
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        trail.push(place("/tmp/b.sock", 2));
        let mut ladder = ExitLadder::begin(now, anchor());
        let expired = now + EXIT_LADDER_BUDGET + Duration::from_millis(1);
        assert_eq!(
            ladder.next(expired, &mut trail, false),
            Rung::Ring {
                anchor: anchor(),
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
        assert_eq!(ladder.next(expired, &mut trail, false), Rung::Close);
    }

    /// An intent the daemon already reported delivered is consumed
    /// before the trail, before the ring, and before the close.
    #[test]
    fn a_parked_intent_outranks_every_automatic_rung() {
        let now = Instant::now();
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        let mut ladder = ExitLadder::begin(now, anchor());
        assert_eq!(
            ladder.next(now, &mut trail, true),
            Rung::Intent {
                budget: RECONNECT_ATTEMPT_TIMEOUT
            }
        );
        assert_eq!(trail.len(), 1, "an intent must not consume a trail entry");

        assert!(matches!(
            ladder.next(now, &mut trail, false),
            Rung::Trail { .. }
        ));
        assert!(matches!(
            ladder.next(now, &mut trail, true),
            Rung::Intent { .. }
        ));
        assert!(matches!(
            ladder.next(now, &mut trail, false),
            Rung::Ring { .. }
        ));
        assert!(matches!(
            ladder.next(now, &mut trail, true),
            Rung::Intent { .. }
        ));
        assert_eq!(ladder.next(now, &mut trail, false), Rung::Close);
    }

    /// A carried landing can outlive the budget on its own; the ladder's
    /// clock starts when the window's own attempts do.
    #[test]
    fn the_deadline_runs_from_the_moment_the_ladder_begins() {
        let exit_observed = Instant::now();
        let settled = exit_observed + EXIT_LADDER_BUDGET + Duration::from_secs(60);
        let mut trail = Trail::default();
        trail.push(place("/tmp/a.sock", 1));
        let mut ladder = ExitLadder::begin(settled, anchor());
        assert_eq!(
            ladder.next(settled, &mut trail, false),
            Rung::Trail {
                place: place("/tmp/a.sock", 1),
                budget: RECONNECT_ATTEMPT_TIMEOUT,
            }
        );
    }
}
