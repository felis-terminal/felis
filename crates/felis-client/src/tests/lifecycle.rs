//! Issue #22: a window takes the exit ladder when its shell exits, and
//! re-dials the same session when the transport drops under it.

use super::*;

/// A [`Place`](exit_ladder::Place) on a named local socket.
fn test_place(socket: &str, session_id: u128) -> exit_ladder::Place {
    exit_ladder::Place {
        reconnector: Reconnector {
            carrier: Carrier::Local(PathBuf::from(socket).into()),
            offer: Offer::window(true),
        },
        session_id,
    }
}

mod daemon_closed {
    use super::*;

    const SAVED: ViewportPosition = ViewportPosition {
        offset: 12,
        anchor: Some(ViewportAnchor {
            pushed: 40,
            generation: 0,
        }),
    };
    const AWAITED: ViewportPosition = ViewportPosition {
        offset: 7,
        anchor: None,
    };

    #[test]
    fn daemon_closed_on_the_live_connection_reconnects_when_nothing_is_in_flight() {
        assert_eq!(
            daemon_closed_outcome(3, 3, false, &PipeState::Idle),
            ClosedOutcome::Reconnect,
        );
    }

    #[test]
    fn daemon_closed_on_the_live_connection_defers_to_an_in_flight_switch() {
        // A landing started before the dying session's connection
        // reached EOF, so that EOF arrives on the still-current
        // generation.
        assert_eq!(
            daemon_closed_outcome(3, 3, true, &PipeState::Idle),
            ClosedOutcome::AwaitSwitch,
        );
    }

    #[test]
    fn daemon_closed_while_attached_to_a_transient_returns_to_the_origin() {
        assert_eq!(
            daemon_closed_outcome(
                3,
                3,
                false,
                &PipeState::Active {
                    viewport: SAVED,
                    region: None,
                    source: test_place("/tmp/a.sock", 0xaaa),
                },
            ),
            ClosedOutcome::PipeReturn { viewport: SAVED },
        );
    }

    #[test]
    fn superseded_connection_eof_never_closes_the_window() {
        // A superseded connection's reader can post `DaemonClosed` after a subsequent
        // switch completes. When returning to Idle, the generation stamp distinguishes
        // that stale EOF from the live daemon dying.
        for &switch_in_flight in &[true, false] {
            for pipe_state in &[
                PipeState::Idle,
                PipeState::Awaiting {
                    target: felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
                    viewport: SAVED,
                },
                PipeState::Active {
                    viewport: SAVED,
                    region: None,
                    source: test_place("/tmp/a.sock", 0xaaa),
                },
                PipeState::Returning {
                    viewport: SAVED,
                    expected: test_place("/tmp/a.sock", 0xaaa),
                },
            ] {
                assert_eq!(
                    daemon_closed_outcome(1, 2, switch_in_flight, pipe_state),
                    ClosedOutcome::Ignore,
                    "stale EOF must not be read as the live connection's",
                );
            }
        }
    }

    #[test]
    fn a_transient_landing_leaves_the_handoff_state_intact() {
        // Only the return may consume the state: consuming it on the way
        // in unlinks the region file under the command still reading it,
        // drops the parked origin, and lets a second chord fire inside
        // the live transient.
        let dir = std::env::temp_dir().join(format!("felis-pipe-landing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("region.txt");
        std::fs::write(&path, b"region\n").unwrap();
        let mut state = PipeState::Active {
            viewport: SAVED,
            region: Some(StagedRegion::adopt(path.clone())),
            source: test_place("/tmp/a.sock", 0xaaa),
        };
        assert!(
            state
                .settle_returning(&test_place("/tmp/a.sock", 1), false)
                .is_none()
        );
        assert!(matches!(
            state,
            PipeState::Active {
                viewport: SAVED,
                region: Some(_),
                ..
            },
        ));
        assert!(path.exists(), "the staged region must survive the landing");

        drop(state);
        assert!(!path.exists(), "the return path's drop is what unlinks it");
        std::fs::remove_dir(&dir).unwrap();
    }

    /// Only the exact place the viewport belongs to gets it back:
    /// restoring on an older trail entry would paste one session's
    /// scroll position onto another's grid.
    #[test]
    fn a_return_landing_restores_the_viewport_only_at_the_expected_place() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: SAVED,
            expected: expected.clone(),
        };
        assert_eq!(state.settle_returning(&expected, false), Some(SAVED));
        assert!(matches!(state, PipeState::Idle));

        let mut state = PipeState::Returning {
            viewport: SAVED,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/b.sock", 0xaaa), false),
            None,
            "the same id on another daemon is another place"
        );
        assert!(
            matches!(state, PipeState::Idle),
            "settling on a real place ends the handoff, sent or not"
        );
    }

    /// A pipe/run chain that hops transient to transient is still one
    /// visit, so the handoff outlives every landing inside it and the
    /// return still restores the viewport at the far end.
    #[test]
    fn a_handoff_survives_the_landings_inside_a_transient_chain() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: SAVED,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/local.sock", 0x1), true),
            None
        );
        assert!(
            matches!(state, PipeState::Returning { .. }),
            "a landing the chain's own re-point made keeps the handoff parked"
        );
        assert_eq!(state.settle_returning(&expected, false), Some(SAVED));
        assert!(matches!(state, PipeState::Idle));
    }

    /// The guard a nested pipe/run chord asks: only a live transient
    /// refuses. A return parked after a chain re-pointed to an ordinary
    /// session does not, so a chord fired there supersedes it instead of
    /// being refused until some unrelated landing settles it.
    #[test]
    fn only_a_live_transient_refuses_a_nested_visit() {
        assert!(
            PipeState::Active {
                viewport: SAVED,
                region: None,
                source: test_place("/tmp/a.sock", 0xaaa),
            }
            .in_visit()
        );
        assert!(
            !PipeState::Returning {
                viewport: SAVED,
                expected: test_place("/tmp/a.sock", 0xaaa),
            }
            .in_visit()
        );
        assert!(!PipeState::Idle.in_visit());
        assert!(
            !PipeState::Awaiting {
                target: felis_client_core::PipeTarget::Clipboard,
                viewport: SAVED,
            }
            .in_visit()
        );
    }

    /// A continuation that failed or never started leaves nothing to
    /// wait for, so the handoff is dropped rather than left for an
    /// unrelated later switch to that place to inherit.
    #[test]
    fn a_handoff_with_nothing_left_to_continue_it_is_dropped() {
        let mut state = PipeState::Returning {
            viewport: SAVED,
            expected: test_place("/tmp/a.sock", 0xaaa),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/b.sock", 0xbbb), false),
            None
        );
        assert!(
            matches!(state, PipeState::Idle),
            "a settled unwind keeps no handoff"
        );
    }

    /// An intent the daemon already reported delivered is a
    /// continuation, so the unwind has not settled and the handoff is
    /// not that landing's to drop.
    #[test]
    fn a_handoff_survives_a_landing_with_a_continuation_still_parked() {
        let expected = test_place("/tmp/a.sock", 0xaaa);
        let mut state = PipeState::Returning {
            viewport: SAVED,
            expected: expected.clone(),
        };
        assert_eq!(
            state.settle_returning(&test_place("/tmp/c.sock", 0x9), true),
            None
        );
        assert!(matches!(state, PipeState::Returning { .. }));
        assert_eq!(state.settle_returning(&expected, false), Some(SAVED));
    }

    #[test]
    fn region_data_that_answers_no_request_leaves_a_live_transient_alone() {
        // A region reply carries no sink of its own, so it is only
        // answerable while `Awaiting`.
        let mut state = PipeState::Active {
            viewport: SAVED,
            region: None,
            source: test_place("/tmp/a.sock", 0xaaa),
        };
        assert!(state.take_awaiting().is_none());
        assert!(matches!(
            state,
            PipeState::Active {
                viewport: SAVED,
                ..
            }
        ));

        let mut state = PipeState::Awaiting {
            target: felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
            viewport: AWAITED,
        };
        let (target, viewport) = state.take_awaiting().expect("the awaited sink comes back");
        assert_eq!(
            target,
            felis_client_core::PipeTarget::Command(vec!["less".to_owned()]),
        );
        assert_eq!(viewport, AWAITED);
        assert!(matches!(state, PipeState::Idle));
    }

    #[test]
    fn the_first_connection_is_live_before_any_switch() {
        // `drive` stamps `FIRST_CONN_GEN` onto the initial pump and seeds
        // `App::conn_gen` with the same value.
        assert_eq!(
            daemon_closed_outcome(FIRST_CONN_GEN, FIRST_CONN_GEN, false, &PipeState::Idle),
            ClosedOutcome::Reconnect,
        );
    }
}

/// What a shell exit does to a landing already in flight
/// (`AppEvent::SessionExited`: newest intent wins).
mod switch_state_carries_the_exit {
    use super::*;

    /// The failure handler tests the retry intent before the missing
    /// session, so a landing left with its retry would re-fetch a roster
    /// on the connection the exit tore down and stay busy on a dead grid
    /// forever.
    #[test]
    fn an_in_flight_landings_retry_is_dropped_with_the_session() {
        let mut state = SwitchState::InFlight {
            session_gone: false,
            retry: Some(SwitchDirection::Next),
        };
        assert!(state.carry_exit());
        assert_eq!(
            state,
            SwitchState::InFlight {
                session_gone: true,
                retry: None,
            },
        );
    }

    /// The caller closes the window instead.
    #[test]
    fn an_idle_window_carries_nothing() {
        let mut state = SwitchState::Idle;
        assert!(!state.carry_exit());
        assert_eq!(state, SwitchState::Idle);
    }
}

/// The latch `App::on_daemon_closed` sets on a close that arrives
/// mid-landing; the classifier's verdict is pinned in
/// `daemon_closed`.
#[test]
fn the_lost_latch_starts_clear_and_stays_set_once_marked() {
    let (outgoing, _rx) = outgoing_channel();
    assert!(!outgoing.is_lost());
    outgoing.mark_lost();
    assert!(
        outgoing.is_lost(),
        "the failed landing decides on this bit, not on the queue's depth"
    );
}

/// Pins the criterion "terminal states are visible": each reason
/// names a remedy; the nonzero status is the return type.
#[test]
fn every_terminal_reconnect_state_names_a_remedy() {
    for reason in [ExitReason::Refused, ExitReason::RetriesExhausted] {
        assert_ne!(reason.headline(), "");
        assert!(
            reason.remedy().contains("felis "),
            "{reason:?} must name the command that recovers it"
        );
    }
    // docs/reference/cli.md "Exit codes": a connection this build
    // could not use is a failure to ask.
    assert_eq!(ExitReason::Refused.code().get(), 2);
    assert_eq!(ExitReason::RetriesExhausted.code().get(), 2);
}

/// A session that ended is not a terminal state: the window unwinds
/// its trail, and a spent ladder closes on `0` like any shell exit.
#[test]
fn only_a_connection_the_window_cannot_use_ends_it() {
    let eof = || DialError::Create(felis_client_core::ConnectError::EofBeforeWelcome);
    let gone = felis_client_core::ReconnectError::SessionGone(eof());
    assert_eq!(ExitReason::from_reconnect(&gone), None);

    let refused = felis_client_core::ReconnectError::Refused(eof());
    assert_eq!(
        ExitReason::from_reconnect(&refused),
        Some(ExitReason::Refused)
    );

    let spent = felis_client_core::ReconnectError::Exhausted {
        attempts: 6,
        source: eof(),
    };
    assert_eq!(
        ExitReason::from_reconnect(&spent),
        Some(ExitReason::RetriesExhausted)
    );
}

/// Pins the criterion "retry bounds are visible": the indicator is
/// on the one surface a window with no frames can still repaint.
#[test]
fn the_disconnected_window_says_so_in_its_title() {
    assert_eq!(
        app_methods::window_title_for(None, "zsh: ~/work", true),
        "zsh: ~/work \u{2014} disconnected"
    );
    assert_eq!(
        app_methods::window_title_for(Some("[work]"), "zsh", false),
        "[work] zsh"
    );
}
