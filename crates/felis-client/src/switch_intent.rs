//! Ordering contract for the window's directional session switches
//! (`architecture/session-lifecycle.md` "Picking the session a chord
//! lands on"): at most one roster fetch in flight, the newest chord
//! replacing whatever was queued, a shell exit superseding the queue and
//! picking on its own dial, and the `RequestId` guarding a stray reply.

use felis_client_core::action::SwitchDirection;
use felis_protocol::messages::RequestId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fetch {
    pub(crate) direction: SwitchDirection,
    /// The single retry B-11 allows after a landing failed on a target
    /// that had gone away; a landing picked from a retry's roster does not
    /// retry again.
    pub(crate) retry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    Apply(Fetch),
    /// A newer intent superseded this roster's; send this request instead.
    Refetch(Fetch),
    /// Nothing was outstanding.
    Ignore,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingSwitch {
    in_flight: Option<Fetch>,
    /// `None` between deciding to fetch and putting the request on the
    /// wire: no reply can match in that gap.
    request: Option<RequestId>,
    queued: Option<SwitchDirection>,
}

impl PendingSwitch {
    /// `None` means a fetch is already out and this direction is queued
    /// behind it, displacing anything queued before.
    pub(crate) const fn request(&mut self, direction: SwitchDirection) -> Option<Fetch> {
        if self.in_flight.is_some() {
            self.queued = Some(direction);
            return None;
        }
        let fetch = Fetch {
            direction,
            retry: false,
        };
        self.in_flight = Some(fetch);
        Some(fetch)
    }

    /// Unconditional: a landing is only ever in flight once the fetch that
    /// picked it has completed, so nothing can be outstanding here.
    pub(crate) const fn retry(&mut self, direction: SwitchDirection) -> Fetch {
        let fetch = Fetch {
            direction,
            retry: true,
        };
        self.in_flight = Some(fetch);
        self.request = None;
        self.queued = None;
        fetch
    }

    pub(crate) const fn armed(&mut self, request: RequestId) {
        self.request = Some(request);
    }

    /// Both the daemon's refusal of that request and its deadline ask this
    /// before touching the fetch, so neither can clear one that belongs to
    /// a later chord.
    pub(crate) const fn awaiting(&self, request: RequestId) -> bool {
        match self.request {
            Some(outstanding) => outstanding.get() == request.get(),
            None => false,
        }
    }

    /// A listing that answers some other id, or none, is not this fetch's:
    /// applying it would let a daemon push the window somewhere it never
    /// asked to go.
    pub(crate) const fn resolve(&mut self, request: RequestId) -> Resolved {
        if !self.awaiting(request) {
            return Resolved::Ignore;
        }
        self.request = None;
        let Some(fetch) = self.in_flight.take() else {
            return Resolved::Ignore;
        };
        match self.queued.take() {
            // A queued chord is a fresh request with its own retry budget.
            Some(direction) => {
                let next = Fetch {
                    direction,
                    retry: false,
                };
                self.in_flight = Some(next);
                Resolved::Refetch(next)
            }
            None => Resolved::Apply(fetch),
        }
    }

    /// The reply is never coming: the connection is being replaced by a
    /// landing or torn down by a shell exit, the daemon refused the
    /// request, or the fetch outran its deadline.
    pub(crate) const fn cancel(&mut self) {
        self.in_flight = None;
        self.request = None;
        self.queued = None;
    }

    pub(crate) const fn is_fetching(self) -> bool {
        self.in_flight.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEXT: SwitchDirection = SwitchDirection::Next;
    const PREVIOUS: SwitchDirection = SwitchDirection::Previous;

    /// Only the matching matters; the driver hands out the real ids.
    fn id(raw: u64) -> RequestId {
        RequestId::new(raw).expect("request ids start at 1")
    }

    /// What `App::send_roster_request` does.
    fn arm(pending: &mut PendingSwitch, request: u64) {
        pending.armed(id(request));
    }

    #[test]
    fn a_lone_chord_fetches_and_applies() {
        let mut pending = PendingSwitch::default();
        assert_eq!(
            pending.request(NEXT),
            Some(Fetch {
                direction: NEXT,
                retry: false
            })
        );
        arm(&mut pending, 1);
        assert!(pending.is_fetching());
        assert_eq!(
            pending.resolve(id(1)),
            Resolved::Apply(Fetch {
                direction: NEXT,
                retry: false
            })
        );
        assert!(!pending.is_fetching());
    }

    /// The user's last direction is the one that runs.
    #[test]
    fn a_newer_chord_supersedes_the_roster_already_in_flight() {
        let mut pending = PendingSwitch::default();
        pending.request(NEXT);
        arm(&mut pending, 1);
        assert_eq!(pending.request(PREVIOUS), None, "one fetch at a time");
        assert_eq!(
            pending.resolve(id(1)),
            Resolved::Refetch(Fetch {
                direction: PREVIOUS,
                retry: false
            })
        );
        arm(&mut pending, 2);
        assert_eq!(
            pending.resolve(id(2)),
            Resolved::Apply(Fetch {
                direction: PREVIOUS,
                retry: false
            })
        );
    }

    /// The chords between are keystrokes the user has already overtaken.
    #[test]
    fn only_the_newest_queued_chord_survives() {
        let mut pending = PendingSwitch::default();
        pending.request(NEXT);
        arm(&mut pending, 1);
        pending.request(PREVIOUS);
        pending.request(NEXT);
        assert_eq!(
            pending.resolve(id(1)),
            Resolved::Refetch(Fetch {
                direction: NEXT,
                retry: false
            })
        );
    }

    /// A landing picked from a retry's roster carries no further budget,
    /// which stops a session that keeps vanishing from looping the window.
    #[test]
    fn a_retry_fetch_is_marked_so_its_landing_does_not_retry_again() {
        let mut pending = PendingSwitch::default();
        assert_eq!(
            pending.retry(NEXT),
            Fetch {
                direction: NEXT,
                retry: true
            }
        );
        arm(&mut pending, 7);
        assert_eq!(
            pending.resolve(id(7)),
            Resolved::Apply(Fetch {
                direction: NEXT,
                retry: true
            })
        );
    }

    /// A chord pressed during the retry's fetch gets its own retry budget.
    #[test]
    fn a_chord_queued_behind_a_retry_starts_a_fresh_budget() {
        let mut pending = PendingSwitch::default();
        pending.retry(PREVIOUS);
        arm(&mut pending, 1);
        pending.request(NEXT);
        assert_eq!(
            pending.resolve(id(1)),
            Resolved::Refetch(Fetch {
                direction: NEXT,
                retry: false
            })
        );
    }

    #[test]
    fn an_unsolicited_roster_is_ignored() {
        let mut pending = PendingSwitch::default();
        assert_eq!(pending.resolve(id(1)), Resolved::Ignore);
    }

    /// Applying a listing for another request would spend the fetch the
    /// window is still owed an answer for.
    #[test]
    fn a_roster_echoing_another_request_leaves_the_fetch_outstanding() {
        let mut pending = PendingSwitch::default();
        pending.request(NEXT);
        arm(&mut pending, 4);
        assert_eq!(pending.resolve(id(5)), Resolved::Ignore);
        assert!(pending.is_fetching(), "the real reply is still owed");
        assert_eq!(
            pending.resolve(id(4)),
            Resolved::Apply(Fetch {
                direction: NEXT,
                retry: false
            })
        );
    }

    /// The daemon's refusal and the deadline both gate on `awaiting`.
    #[test]
    fn only_the_armed_request_is_awaited() {
        let mut pending = PendingSwitch::default();
        pending.request(NEXT);
        assert!(
            !pending.awaiting(id(1)),
            "nothing is awaited before the request is issued"
        );
        arm(&mut pending, 1);
        assert!(pending.awaiting(id(1)));
        assert!(!pending.awaiting(id(2)));
        pending.cancel();
        assert!(!pending.awaiting(id(1)));
    }

    /// A shell exit tears down the connection the chord's fetch rides.
    #[test]
    fn canceling_drops_the_fetch_and_its_queue() {
        let mut pending = PendingSwitch::default();
        pending.request(NEXT);
        arm(&mut pending, 1);
        pending.request(PREVIOUS);
        pending.cancel();
        assert!(!pending.is_fetching());
        assert_eq!(pending.resolve(id(1)), Resolved::Ignore);
    }
}
