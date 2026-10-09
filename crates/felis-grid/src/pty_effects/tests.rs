use proptest::prelude::*;

use super::*;
use crate::{ErasedRange, ScreenSwitch};

fn apc(n: usize) -> PtyEffect {
    PtyEffect::Apc(ApcBody {
        body: format!("Gi={n}").into_bytes(),
        cursor_row: 0,
        cursor_col: 0,
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
    queue.push(PtyEffect::ScreenSwitch(ScreenSwitch::EnteredAlternate));
    queue.push_scroll(4);

    assert_eq!(
        queue.take(),
        vec![
            PtyEffect::ScrolledIntoScrollback(3),
            PtyEffect::ScreenSwitch(ScreenSwitch::EnteredAlternate),
            PtyEffect::ScrolledIntoScrollback(4),
        ]
    );
}

#[test]
fn a_scroll_after_another_kind_starts_its_own_entry() {
    let mut queue = PtyEffectQueue::default();
    queue.push(PtyEffect::Erased(ErasedRange {
        top: 0,
        bottom: 1,
        force: false,
    }));
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
