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
