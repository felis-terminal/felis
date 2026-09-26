//! `App` methods that move the window between places: the session
//! switch, reconnect, the exit ladder, reattach, and retarget.

use super::{
    App, AppEvent, Correlation, DialError, DialedConnection, Instant, Landing, PendingReattach,
    PipeState, Reconnector, SessionHex, SessionInfo, SwitchDirection, SwitchState, TrailRecord,
    dial_and_land, dial_and_land_within, info, pick_switch_target, reconnector_for_target,
    resolve_local_socket, warn,
};
use crate::app_methods::window_title_for;
use crate::exit_ladder::{ExitAnchor, ExitLadder, Place, Rung};
use crate::switch_intent::{Fetch, Resolved};
use anyhow::Result;
use felis_client_core::AttachIntent;
use felis_client_core::roster::RingKey;
use felis_client_core::{ROSTER_FETCH_TIMEOUT, redial_session};
use felis_protocol::messages::OpsToDaemonMsg;
use felis_protocol::messages::{RequestId, RetargetLanding, RetargetTarget};
use std::future::Future;
use std::time::Duration;

impl App {
    /// `switch_state` alone misses a pipe/run handoff parked at
    /// `PipeState::Active`. `Returning` is deliberately not busy: its
    /// handoff can outlive several landings while an unwind walks toward
    /// the place the viewport belongs to, and a window busy that long
    /// would refuse every chord and every parked push.
    const fn is_landing(&self) -> bool {
        self.switch_state.in_flight()
            || matches!(
                self.pipe_state,
                PipeState::Awaiting { .. } | PipeState::Active { .. }
            )
    }

    /// A switch chord asks [`Self::is_landing`] instead: a second chord
    /// during a fetch is queued, not dropped.
    pub(crate) const fn is_busy(&self) -> bool {
        self.is_landing() || self.pending_switch.is_fetching()
    }

    /// A remembered value, not a roster lookup: the session it names can
    /// be reaped while the fetch that will use it is in flight.
    pub(crate) const fn ring_anchor(&self) -> RingKey {
        RingKey {
            sequence: self.current_sequence,
            id: self.current_session_id,
        }
    }

    /// The pick needs a roster fresh as of now, so the chord spends a
    /// round trip on the window's own connection first and lands on
    /// [`AppEvent::RosterListed`] (`switch_intent.rs`).
    pub(crate) fn switch_session(&mut self, direction: SwitchDirection) {
        if self.is_landing() {
            tracing::debug!("session switch already in progress; ignoring chord");
            return;
        }
        if let Some(fetch) = self.pending_switch.request(direction) {
            self.send_roster_request(fetch);
        } else {
            tracing::debug!(?direction, "a roster fetch is in flight; queued behind it");
        }
    }

    /// The transport dropped with nothing in flight: keep the shadow on
    /// screen behind a disconnected indicator and re-dial the same
    /// carrier and session
    /// (docs/explanation/architecture/session-lifecycle.md "Transport
    /// loss").
    pub(crate) fn begin_reconnect(&mut self) {
        let carrier = self.reconnector.carrier.clone();
        let offer = self.reconnector.offer;
        let id = self.current_session_id;
        // Both replies ride the connection that just died, and an
        // outstanding region request nobody clears keeps `is_busy` true
        // for the life of the window.
        self.pending_switch.cancel();
        drop(self.pipe_state.take_awaiting());
        self.reconnecting = true;
        self.refresh_window_title();
        self.redraw.request();
        // A landing in flight is what keeps a chord, a push, and a second
        // `DaemonClosed` off the window while the ladder runs.
        self.switch_state = SwitchState::InFlight {
            session_gone: false,
            retry: None,
        };
        warn!(
            id = %SessionHex(id),
            "daemon connection lost; reconnecting to the same session"
        );
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let event = match redial_session(carrier, offer, id).await {
                Ok(dialed) => switch_ready_event(dialed, None, TrailRecord::Reconnect),
                Err(err) => AppEvent::ReconnectFailed {
                    reason: crate::ExitReason::from_reconnect(&err),
                    detail: format!("{:#}", anyhow::Error::new(err)),
                },
            };
            drop(proxy.send_event(event));
        });
    }

    /// The disconnected indicator is client-local presentation
    /// (principle 3): no daemon message says a window is offline.
    pub(crate) fn refresh_window_title(&self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let shell_title = self.shadow.title().unwrap_or("felis");
        surface.window.set_title(&window_title_for(
            self.title_prefix.as_deref(),
            shell_title,
            self.reconnecting,
        ));
    }

    /// A failure to issue the id abandons the fetch rather than leaving
    /// one outstanding: the window would otherwise refuse every later
    /// chord.
    fn send_roster_request(&mut self, fetch: Fetch) {
        let issued = self
            .driver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .issue_request();
        let Ok(request) = issued else {
            tracing::warn!("switch: no request id left on this connection");
            self.abandon_fetch();
            return;
        };
        self.pending_switch.armed(request);
        tracing::debug!(
            direction = ?fetch.direction,
            retry = fetch.retry,
            "fetching the session roster"
        );
        self.send_control_correlated(&OpsToDaemonMsg::List, Correlation::request(request));
        self.arm_roster_deadline(request);
    }

    /// The pump has no request-timeout layer, so a connection healthy
    /// enough to stream the grid but never answering `Ops::List` would
    /// leave the window unable to switch for the rest of the connection.
    /// The timer always fires; the handler decides whether it still means
    /// anything (hence the request id).
    fn arm_roster_deadline(&self, request: RequestId) {
        let proxy = self.proxy.clone();
        let conn_gen = self.conn_gen;
        self.runtime.spawn(async move {
            tokio::time::sleep(ROSTER_FETCH_TIMEOUT).await;
            drop(proxy.send_event(AppEvent::RosterFetchTimedOut { request, conn_gen }));
        });
    }

    /// The fetch gates [`Self::is_busy`], so a push parked behind it has
    /// to run now or never (the daemon already told the CLI that switch
    /// succeeded).
    pub(crate) fn abandon_fetch(&mut self) {
        self.pending_switch.cancel();
        self.run_pending_push();
    }

    /// A listing answering anything but the outstanding fetch is not this
    /// window's pick to make.
    pub(crate) fn apply_roster(&mut self, request: RequestId, sessions: &[SessionInfo]) {
        match self.pending_switch.resolve(request) {
            Resolved::Ignore => tracing::debug!("unsolicited session roster; ignoring"),
            // Acting on a superseded roster would switch in a direction the
            // last chord contradicted.
            Resolved::Refetch(fetch) => self.send_roster_request(fetch),
            Resolved::Apply(fetch) => {
                let anchor = self.ring_anchor();
                if let Some(target) = pick_switch_target(sessions, anchor, fetch.direction) {
                    self.start_chord_switch(target, fetch);
                } else {
                    // Nowhere to go is not an error. The fetch that gated
                    // `is_busy` is spent, so anything parked behind it runs.
                    tracing::debug!(count = sessions.len(), "no other live session to switch to");
                    self.run_pending_push();
                }
            }
        }
    }

    /// Automatic, so the attach takes the live-only form: the picked
    /// session can still exit between this roster and the attach, and a
    /// window that landed on the corpse would never be pushed off it.
    fn start_chord_switch(&mut self, target_id: u128, fetch: Fetch) {
        if self.is_busy() {
            tracing::debug!("session switch already in progress; dropping the pick");
            return;
        }
        let carrier = self.reconnector.carrier.clone();
        let offer = self.reconnector.offer;
        info!(
            target = format!("{target_id:#x}"),
            direction = ?fetch.direction,
            "session switch starting"
        );
        self.switch_state = SwitchState::InFlight {
            session_gone: false,
            // A landing picked from the retry's own roster has spent the
            // budget, or a session that keeps vanishing would loop the window.
            retry: (!fetch.retry).then_some(fetch.direction),
        };
        self.dial_in_background(
            dial_and_land(
                carrier,
                offer,
                Landing::Attach {
                    id: target_id,
                    intent: AttachIntent::Automatic,
                },
            ),
            None,
            TrailRecord::Push,
        );
    }

    /// B-11's single retry after a landing failed on a vanished target.
    pub(crate) fn retry_switch(&mut self, direction: SwitchDirection) {
        let fetch = self.pending_switch.retry(direction);
        self.send_roster_request(fetch);
    }

    pub(crate) fn current_place(&self) -> Place {
        Place {
            reconnector: self.reconnector.clone(),
            session_id: self.current_session_id,
        }
    }

    /// Read now rather than off a roster later: the ring pick needs the
    /// exited row's creation sequence, which the reap takes.
    fn exit_anchor(&self) -> ExitAnchor {
        ExitAnchor {
            reconnector: self.reconnector.clone(),
            ring_key: self.ring_anchor(),
        }
    }

    /// `true` when the ladder is spent and the caller closes the window.
    pub(crate) fn begin_exit_ladder(&mut self, reason: &str) -> bool {
        if self.exit_ladder.is_none() {
            // Now, not when the exit was observed: a user landing that
            // carried the exit may have run for longer than the whole
            // budget, and the window's own attempts start here.
            self.exit_ladder = Some(ExitLadder::begin(Instant::now(), self.exit_anchor()));
        }
        self.continue_exit_ladder(reason)
    }

    fn continue_exit_ladder(&mut self, reason: &str) -> bool {
        // The correlation belonged to the connection the exit tore down,
        // so no reply will ever clear it; a window that unwinds would
        // carry the stuck `is_busy` with it.
        drop(self.pipe_state.take_awaiting());
        // A switch-in whose source died and then failed leaves the
        // transient marked live; a later rung's landing would be read as
        // that transient coming up, and nothing would unlink its region.
        if let PipeState::Active { .. } = self.pipe_state
            && let PipeState::Active { region, .. } =
                std::mem::replace(&mut self.pipe_state, PipeState::Idle)
        {
            drop(region);
        }
        let Some(mut ladder) = self.exit_ladder.take() else {
            return true;
        };
        let parked = self.pending_retarget.is_some() || self.pending_reattach.is_some();
        let rung = ladder.next(Instant::now(), &mut self.trail, parked);
        self.exit_ladder = Some(ladder);
        match rung {
            Rung::Intent { budget } => {
                info!(reason, "running the intent parked behind the exit");
                if self.run_intent_rung(budget) {
                    return false;
                }
                // The slot is empty now, so this recurses at most once
                // per parked intent.
                self.continue_exit_ladder(reason)
            }
            Rung::Trail { place, budget } => {
                info!(
                    reason,
                    target = format!("{:#x}", place.session_id),
                    "unwinding to the place before this one"
                );
                self.dial_trail_entry(place, budget);
                false
            }
            Rung::Ring { anchor, budget } => {
                info!(reason, "picking another session on this daemon");
                self.dial_ring_rung(anchor, budget);
                false
            }
            Rung::Close => {
                self.exit_ladder = None;
                info!(reason, "no session left to open; closing window");
                true
            }
        }
    }

    /// An exact attach: the entry names its session outright, so a dead
    /// one costs one connect and one refusal rather than a roster fetch
    /// on top.
    fn dial_trail_entry(&mut self, place: Place, budget: Duration) {
        let Place {
            reconnector,
            session_id,
        } = place;
        let crossing = (reconnector != self.reconnector).then(|| reconnector.clone());
        self.switch_state = SwitchState::InFlight {
            session_gone: true,
            // A re-pick would fetch its roster on the connection the
            // exit has already torn down.
            retry: None,
        };
        self.dial_in_background(
            dial_and_land_within(
                reconnector.carrier.clone(),
                reconnector.offer,
                Landing::AttachExact {
                    id: session_id,
                    // The window chose this rung, not the user: a place
                    // whose shell exited meanwhile must refuse.
                    intent: AttachIntent::Automatic,
                },
                budget,
            ),
            crossing,
            TrailRecord::Keep,
        );
    }

    /// The roster fetch and the pick both happen on this dial's
    /// connection, not the window's own: the daemon drops this window's
    /// subscriber as it reports the exit, so a fetch left on the old
    /// wire would be answered by nobody.
    fn dial_ring_rung(&mut self, anchor: ExitAnchor, budget: Duration) {
        let ExitAnchor {
            reconnector,
            ring_key,
        } = anchor;
        let crossing = (reconnector != self.reconnector).then(|| reconnector.clone());
        self.switch_state = SwitchState::InFlight {
            session_gone: true,
            retry: None,
        };
        self.dial_in_background(
            dial_and_land_within(
                reconnector.carrier.clone(),
                reconnector.offer,
                Landing::PickExit { anchor: ring_key },
                budget,
            ),
            crossing,
            TrailRecord::Keep,
        );
    }

    /// An intent the daemon already reported delivered, run ahead of
    /// every automatic rung so a ladder that then closes cannot drop a
    /// switch the CLI exited 0 on. `false` when the dial never started,
    /// which no `SessionSwitchFailed` will report, so the caller resumes
    /// the ladder itself.
    fn run_intent_rung(&mut self, budget: Duration) -> bool {
        let record = self.intent_record(TrailRecord::Keep);
        if let Some(target) = self.pending_retarget.take() {
            self.dial_retarget(target, Some(budget), record, true)
        } else if let Some(pending) = self.pending_reattach.take() {
            self.dial_reattach(pending, Some(budget), record, true)
        } else {
            false
        }
    }

    const fn intent_record(&self, settled: TrailRecord) -> TrailRecord {
        crate::exit_ladder::intent_record(self.on_transient, settled)
    }

    /// Every path that clears the busy state must call this: the daemon
    /// has already told the CLI the switch succeeded, so a path that
    /// silently flipped busy back would strand the push for the window's
    /// life.
    pub(crate) fn run_pending_push(&mut self) {
        if self.is_busy() {
            return;
        }
        let record = self.intent_record(TrailRecord::Push);
        if let Some(target) = self.pending_retarget.take() {
            self.dial_retarget(target, None, record, false);
            return;
        }
        if let Some(pending) = self.pending_reattach.take()
            && pending.place() != self.current_place()
        {
            self.dial_reattach(pending, None, record, false);
        }
    }

    pub(crate) fn dial_in_background(
        &self,
        dial: impl Future<Output = Result<DialedConnection, DialError>> + Send + 'static,
        retargeted: Option<Reconnector>,
        record: TrailRecord,
    ) {
        let proxy = self.proxy.clone();
        self.runtime.spawn(async move {
            let event = match dial.await {
                Ok(dialed) => switch_ready_event(dialed, retargeted, record),
                Err(err) => AppEvent::SessionSwitchFailed {
                    // Read before the error moves into `anyhow`: the retry
                    // decision is the typed reason's.
                    target_vanished: err.target_vanished(),
                    // Through `anyhow`: the banner wants the whole chain, and
                    // only the alternate rendering walks the sources.
                    reason: format!("{:#}", anyhow::Error::new(err)),
                },
            };
            drop(proxy.send_event(event));
        });
    }

    pub(crate) fn reattach_now(&mut self, pending: PendingReattach) {
        let record = self.intent_record(TrailRecord::Push);
        self.dial_reattach(pending, None, record, false);
    }

    /// `exit_driven` says there is no live session to stay on, which is
    /// what routes a failure back into the ladder. `false` when nothing
    /// was dialed.
    fn dial_reattach(
        &mut self,
        pending: PendingReattach,
        budget: Option<Duration>,
        record: TrailRecord,
        exit_driven: bool,
    ) -> bool {
        if !exit_driven && self.is_busy() {
            tracing::debug!("session switch already in progress; ignoring");
            return false;
        }
        let PendingReattach { reconnector, id } = pending;
        let crossing = (reconnector != self.reconnector).then(|| reconnector.clone());
        info!(target = format!("{id:#x}"), "session switch starting");
        self.switch_state = SwitchState::InFlight {
            session_gone: exit_driven,
            // This target is the request, not a choice made from a roster.
            retry: None,
        };
        let landing = Landing::AttachExact {
            id,
            // The user named this session; its final screen is a
            // legitimate thing to land on.
            intent: AttachIntent::Deliberate,
        };
        let carrier = reconnector.carrier.clone();
        let offer = reconnector.offer;
        match budget {
            Some(budget) => self.dial_in_background(
                dial_and_land_within(carrier, offer, landing, budget),
                crossing,
                record,
            ),
            None => {
                self.dial_in_background(dial_and_land(carrier, offer, landing), crossing, record);
            }
        }
        true
    }

    /// The `window retarget` path
    /// (docs/explanation/architecture/control-surfaces.md). The swap lands
    /// on [`AppEvent::SessionSwitchReady`] carrying the new [`Reconnector`].
    pub(crate) fn retarget(&mut self, target: RetargetTarget) {
        let record = self.intent_record(TrailRecord::Push);
        self.dial_retarget(target, None, record, false);
    }

    /// `false` when nothing was dialed, so no `SessionSwitchFailed` will
    /// ever report this one.
    fn dial_retarget(
        &mut self,
        target: RetargetTarget,
        budget: Option<Duration>,
        record: TrailRecord,
        exit_driven: bool,
    ) -> bool {
        if !exit_driven && self.is_busy() {
            tracing::debug!("switch/retarget already in progress; ignoring retarget push");
            return false;
        }
        let Some(retargeted) = reconnector_for_target(&target, resolve_local_socket(None).ok())
        else {
            warn!("cannot resolve local socket for a carrier-less retarget; ignoring");
            return false;
        };
        info!(carrier = target.carrier.label(), "host retarget starting");
        let landing = match target.landing {
            RetargetLanding::Attach(prefix) => Landing::ResolveOnTarget(prefix),
            RetargetLanding::Create(args) => Landing::CreateOnTarget(args),
        };
        self.switch_state = SwitchState::InFlight {
            session_gone: exit_driven,
            retry: None,
        };
        let carrier = retargeted.carrier.clone();
        let offer = retargeted.offer;
        match budget {
            Some(budget) => self.dial_in_background(
                dial_and_land_within(carrier, offer, landing, budget),
                Some(retargeted),
                record,
            ),
            None => self.dial_in_background(
                dial_and_land(carrier, offer, landing),
                Some(retargeted),
                record,
            ),
        }
        true
    }

    /// Refuses nested visits: pipe/run chains collapse into a single return.
    /// Carrying parked handoffs across nested round-trips risks silently
    /// dropping them on dead-connection error paths. Chains that retargeted
    /// to ordinary sessions still owe a return.
    pub(crate) fn refuse_nested_visit(&mut self, chord: &str) -> bool {
        if !self.pipe_state.owes_a_return() {
            return false;
        }
        self.set_notice(format!(
            "{chord}: this pipe/run visit still owes a return; finish it first"
        ));
        true
    }

    /// Drops unreached handoffs once unwind completes so subsequent switches
    /// do not inherit stale visit viewports. `carried` reflects
    /// `exit_ladder::carries_handoff`; failed landings carry nothing because
    /// no destination session was installed.
    pub(crate) fn settle_handoff(&mut self, carried: bool) {
        let keep_parked = carried || self.on_transient || self.is_busy();
        let landed = self.current_place();
        if let Some(viewport) = self.pipe_state.settle_returning(&landed, keep_parked)
            && viewport > 0
        {
            self.send_viewport(viewport);
        }
    }
}

/// Built here rather than in the dial task's `match` so the reconnect
/// ladder and the switch path install a landing the same way.
fn switch_ready_event(
    dialed: DialedConnection,
    retargeted: Option<Reconnector>,
    record: TrailRecord,
) -> AppEvent {
    AppEvent::SessionSwitchReady {
        reader: dialed.conn.reader,
        writer: dialed.conn.writer,
        driver: std::sync::Arc::new(std::sync::Mutex::new(dialed.conn.driver)),
        attach: Box::new(dialed.attach),
        pull_enabled: dialed.pull_enabled,
        retargeted,
        record,
    }
}
