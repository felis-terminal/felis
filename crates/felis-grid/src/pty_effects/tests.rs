use felis_vt::Parser;
use proptest::prelude::*;

use super::*;
use crate::test_support::drive;
use crate::{Grid, ImageClear, ScreenSwitch, ScrollDirection};

fn apc(n: usize) -> PtyEffect {
    PtyEffect::Apc(ApcBody {
        body: format!("Gi={n}").into_bytes(),
        cursor_row: 0,
        cursor_col: 0,
        stops_parse: false,
    })
}

#[test]
fn the_apc_budget_admits_exactly_the_cap_and_refuses_past_it() {
    let mut queue = PtyEffectQueue::default();
    for i in 0..APC_OUTBOX_CAP {
        assert!(queue.push_apc(b"Gi=1", 0, 0), "APC {i} is within the cap");
    }
    assert!(
        !queue.push_apc(b"Gi=1", 0, 0),
        "the entry past the cap must be refused, not queued"
    );
    assert_eq!(queue.take().len(), APC_OUTBOX_CAP);
}

#[test]
fn a_non_apc_effect_never_spends_the_apc_budget() {
    let mut queue = PtyEffectQueue::default();
    for _ in 0..1000 {
        assert!(queue.push(PtyEffect::Response(vec![b'x'])));
        queue.push_scroll(1);
    }
    assert!(
        queue.push_apc(b"Gi=1", 0, 0),
        "responses and scrolls are not what the APC cap bounds"
    );
}

#[test]
fn pushing_an_apc_as_a_plain_effect_still_charges_the_budget() {
    let mut queue = PtyEffectQueue::default();
    for i in 0..APC_OUTBOX_CAP {
        assert!(queue.push(apc(i)));
    }
    assert!(!queue.push(apc(APC_OUTBOX_CAP)));
    assert!(!queue.push_apc(b"Gi=1", 0, 0));
}

#[test]
fn draining_the_queue_refills_the_apc_budget() {
    let mut queue = PtyEffectQueue::default();
    for _ in 0..APC_OUTBOX_CAP {
        assert!(queue.push_apc(b"Gi=1", 0, 0));
    }
    assert!(!queue.push_apc(b"Gi=1", 0, 0));

    assert_eq!(queue.take().len(), APC_OUTBOX_CAP);
    assert!(
        queue.push_apc(b"Gi=1", 0, 0),
        "the next burst starts on a full budget"
    );
}

/// `PartialEq` is hand-written to skip the APC count; it must still see
/// different effects.
#[test]
fn queues_holding_different_effects_are_not_equal() {
    let mut empty = PtyEffectQueue::default();
    let mut one = PtyEffectQueue::default();
    one.push_scroll(1);
    assert_ne!(one, empty);

    let mut other = PtyEffectQueue::default();
    other.push_scroll(2);
    assert_ne!(one, other, "a scroll of 1 is not a scroll of 2");

    empty.push_scroll(1);
    assert_eq!(one, empty);
}

#[test]
fn adjacent_scrolls_coalesce_into_one_entry() {
    let mut queue = PtyEffectQueue::default();
    for _ in 0..100 {
        queue.push_scroll(1);
    }
    let effects = queue.take();
    assert_eq!(effects.len(), 1, "a plaintext burst costs one slot");
    assert_eq!(effects[0], PtyEffect::ScrolledIntoScrollback(100));
}

#[test]
fn an_intervening_effect_ends_a_scroll_run() {
    let mut queue = PtyEffectQueue::default();
    queue.push_scroll(3);
    queue.push(PtyEffect::ScreenSwitch(ScreenSwitch::EnteredAlternate {
        resumed: false,
    }));
    queue.push_scroll(4);

    assert_eq!(
        queue.take(),
        vec![
            PtyEffect::ScrolledIntoScrollback(3),
            PtyEffect::ScreenSwitch(ScreenSwitch::EnteredAlternate { resumed: false }),
            PtyEffect::ScrolledIntoScrollback(4),
        ]
    );
}

#[test]
fn a_scroll_after_another_kind_starts_its_own_entry() {
    let mut queue = PtyEffectQueue::default();
    queue.push(PtyEffect::ImagesCleared(ImageClear::Screen));
    queue.push_scroll(2);
    queue.push_scroll(2);

    let effects = queue.take();
    assert_eq!(effects.len(), 2);
    assert_eq!(effects[1], PtyEffect::ScrolledIntoScrollback(4));
}

#[derive(Debug, Clone)]
enum Op {
    PushApc(usize),
    PushApcEffect(usize),
    Take,
}

fn body_len() -> impl Strategy<Value = usize> {
    let limit = felis_vt::APC_BUFFER_LIMIT;
    prop_oneof![0..64usize, (limit - 1024)..=limit, 0usize..=limit]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => body_len().prop_map(Op::PushApc),
        2 => body_len().prop_map(Op::PushApcEffect),
        1 => Just(Op::Take),
    ]
}

proptest! {
    /// Whatever mix of body sizes and drains: the queue never holds more
    /// than its count and byte caps, a refused body changes nothing, and
    /// a queue not reporting itself spent admits a body of any size, so
    /// a parse that yields on `apc_budget_spent` never has one refused.
    #[test]
    fn the_apc_budget_bounds_the_queue_and_never_refuses_before_it_reports_spent(
        ops in proptest::collection::vec(op(), 0..60),
    ) {
        let buf = vec![b'x'; felis_vt::APC_BUFFER_LIMIT];
        let mut queue = PtyEffectQueue::default();
        let mut held: Vec<usize> = Vec::new();
        for op in ops {
            let spent = queue.apc_budget_spent();
            let (len, admitted) = match op {
                Op::Take => {
                    let taken = queue.take();
                    prop_assert_eq!(taken.len(), held.len());
                    held.clear();
                    prop_assert!(!queue.apc_budget_spent(), "a drain refills the budget");
                    continue;
                }
                Op::PushApc(len) => (len, queue.push_apc(&buf[..len], 0, 0)),
                Op::PushApcEffect(len) => (
                    len,
                    queue.push(PtyEffect::Apc(ApcBody {
                        body: buf[..len].to_vec(),
                        cursor_row: 0,
                        cursor_col: 0,
                        stops_parse: false,
                    })),
                ),
            };
            if !spent {
                prop_assert!(admitted, "a {len}-byte body refused before the queue reported spent");
            }
            if admitted {
                held.push(len);
            }
            prop_assert!(held.len() <= APC_OUTBOX_CAP);
            prop_assert!(held.iter().sum::<usize>() <= APC_OUTBOX_BYTES);
        }
    }
}

//
// The unified effect queue's one contract: effects come out in the
// order the program wrote them (cross-kind order inside one PTY burst is
// the recurring Kitty-graphics bug class).

/// `ED` → APC → DA1 → OSC 7501 → OSC 133 `A` → bottom-LF scroll →
/// `?1049h`, in byte-stream order.
#[test]
fn pty_effects_drain_in_byte_stream_order() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b[2J"); // ED 2 → ImagesCleared
    burst.extend_from_slice(b"\x1b_Ga=q,i=1;\x1b\\"); // → Apc
    burst.extend_from_slice(b"\x1b[c"); // DA1 → Response
    burst.extend_from_slice(b"\x1b]7501;state=working\x07"); // → ProgramStatus
    burst.extend_from_slice(b"\x1b]133;A\x07"); // → PromptStarted
    burst.extend_from_slice(b"a\r\nb\r\n"); // bottom LF → ScrolledIntoScrollback
    burst.extend_from_slice(b"\x1b[?1049h"); // → ScreenSwitch
    drive(&mut p, &mut g, &burst);
    let effects = g.take_pty_effects();
    let kinds: Vec<&str> = effects
        .iter()
        .map(|e| match e {
            PtyEffect::Response(_) => "response",
            PtyEffect::Apc(_) => "apc",
            PtyEffect::ImagesCleared(_) => "images_cleared",
            PtyEffect::ScrolledIntoScrollback(_) => "scrolled",
            PtyEffect::AltScreenScrolled(_) => "alt_scrolled",
            PtyEffect::RegionScrolled { .. } => "region_scrolled",
            PtyEffect::HardReset => "hard_reset",
            PtyEffect::Scrolled { .. } => "scroll_op",
            PtyEffect::ScreenSwitch(_) => "switch",
            PtyEffect::PrimaryReflowed(_) => "reflowed",
            PtyEffect::BracketedPasteDisabled => "paste_guard_off",
            PtyEffect::ProgramStatus(_) => "program_status",
            PtyEffect::PromptStarted => "prompt_started",
            PtyEffect::Prompt(_) => "prompt",
            PtyEffect::TerminalProgress(_) => "terminal_progress",
            PtyEffect::CommandOutput { .. } => "command_output",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "images_cleared",
            "apc",
            "response",
            "program_status",
            "prompt_started",
            "prompt",
            "scrolled",
            "scroll_op",
            "switch"
        ],
        "effects={effects:?}"
    );
    assert_eq!(g.take_pty_effects(), Vec::<PtyEffect>::new());
}

/// A primary `A` and every `D` queue a prompt edge where the program
/// wrote them among the status effects, on either screen; a continuation
/// `A` and the `B` / `C` marks queue none.
#[test]
fn prompt_edges_queue_in_byte_stream_order_on_both_screens() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 20);
    let mut burst = Vec::new();
    burst.extend_from_slice(b"\x1b]133;D;3\x07");
    burst.extend_from_slice(b"\x1b]7501;state=idle\x07");
    burst.extend_from_slice(b"\x1b]133;A\x07");
    burst.extend_from_slice(b"\x1b]133;A;k=s\x07");
    burst.extend_from_slice(b"\x1b]133;B\x07\x1b]133;C\x07");
    burst.extend_from_slice(b"\x1b[?1049h\x1b]133;D\x07");
    drive(&mut p, &mut g, &burst);
    let edges: Vec<String> = g
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Prompt(edge) => Some(format!("{edge:?}")),
            PtyEffect::ProgramStatus(_) => Some("status".to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        edges,
        [
            "CommandFinished { exit_code: Some(3) }",
            "status",
            "Shown",
            "CommandFinished { exit_code: None }",
        ]
    );
}

/// Prompt edges commute with scrolling, so the scrolls on either side
/// of a `D` / `A` pair still coalesce: a command per prompt costs one
/// scroll directive and one scrollback count per drain, not one per
/// command.
#[test]
fn prompt_edges_do_not_split_scroll_runs() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 20);
    let mut burst = Vec::new();
    for _ in 0..3 {
        burst.extend_from_slice(b"out\nout\nout\nout\n");
        burst.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07");
    }
    drive(&mut p, &mut g, &burst);
    let kinds: Vec<&str> = g
        .take_pty_effects()
        .iter()
        .map(|e| match e {
            PtyEffect::Prompt(_) => "edge",
            PtyEffect::Scrolled { .. } => "scrolled",
            PtyEffect::ScrolledIntoScrollback(_) => "into_scrollback",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "into_scrollback",
            "scrolled",
            "edge",
            "edge",
            "edge",
            "edge",
            "edge",
            "edge"
        ]
    );
}

/// The split is the ordering information the queue exists to keep: a
/// scroll before an `a=T` must not shift the placement `a=T` creates.
#[test]
fn adjacent_scrollback_pushes_coalesce_and_interleaved_effects_split_the_run() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"a\r\nb\r\nc\r\nd\r\n");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::ScrolledIntoScrollback(3),
                PtyEffect::Scrolled { .. },
            ]
        ),
        "the directive between the pushes must not split them: effects={effects:?}"
    );
    drive(&mut p, &mut g, b"e\r\n\x1b[cf\r\n");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::ScrolledIntoScrollback(1),
                PtyEffect::Scrolled { .. },
                PtyEffect::Response(_),
                PtyEffect::ScrolledIntoScrollback(1),
                PtyEffect::Scrolled { .. },
            ]
        ),
        "effects={effects:?}"
    );
}

#[test]
fn region_scrolls_of_one_band_coalesce_and_report_their_band() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 8);
    drive(&mut p, &mut g, b"\x1b[2;4r\x1b[4;1H\n\n\x1b[2;1H\x1bM");
    let regions: Vec<_> = g
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::RegionScrolled { op, into_history } => Some((
                op.region_top,
                op.region_bottom,
                op.n_rows,
                op.direction,
                into_history,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        regions,
        [
            (1, 3, 2, ScrollDirection::Up, false),
            (1, 3, 1, ScrollDirection::Down, false),
        ]
    );
}

#[test]
fn a_region_anchored_at_the_primary_top_scrolls_into_history() {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 8);
    drive(&mut p, &mut g, b"\x1b[1;3r\x1b[3;1H\n");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::RegionScrolled {
                    op: ScrollOp {
                        region_top: 0,
                        region_bottom: 2,
                        n_rows: 1,
                        direction: ScrollDirection::Up,
                    },
                    into_history: true,
                },
                PtyEffect::Scrolled { .. },
            ]
        ),
        "effects={effects:?}"
    );
}

#[test]
fn a_full_screen_reverse_scroll_is_a_region_scroll_of_the_whole_screen() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1bM");
    let effects = g.take_pty_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [
                PtyEffect::RegionScrolled {
                    op: ScrollOp {
                        region_top: 0,
                        region_bottom: 3,
                        n_rows: 1,
                        direction: ScrollDirection::Down,
                    },
                    into_history: false,
                },
                PtyEffect::Scrolled { .. },
            ]
        ),
        "effects={effects:?}"
    );
}

/// A probe running under S8C1T reads C1-led responses.
#[test]
fn pty_effects_drain_applies_c1_transform_to_responses() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 8);
    drive(&mut p, &mut g, b"\x1b G\x1b[c"); // S8C1T, then DA1
    let effects = g.take_pty_effects();
    let [PtyEffect::Response(bytes)] = effects.as_slice() else {
        panic!("expected one response, got {effects:?}");
    };
    assert_eq!(bytes[0], 0x9B, "CSI must collapse to its 8-bit C1 form");
}
