use super::*;

fn text(s: &str) -> ClusterText {
    ClusterText::new(s).expect("under cap")
}

#[test]
fn interning_the_same_text_twice_reuses_one_row() {
    let mut t = ClusterTable::default();
    let first = t.intern("e\u{0301}").expect("interned");
    assert_eq!(t.intern("e\u{0301}"), Some(first));
    assert_eq!(t.len(), 1);
    assert_ne!(t.intern("o\u{0308}"), Some(first));
    assert_eq!(t.len(), 2);
}

/// `PartialEq` is hand-written to skip the index; it must still see
/// different entries.
#[test]
fn tables_holding_different_clusters_are_not_equal() {
    let mut empty = ClusterTable::default();
    assert!(empty.is_empty());

    let mut one = ClusterTable::default();
    one.intern("e\u{0301}").expect("interned");
    assert!(!one.is_empty());
    assert_ne!(one, empty);

    let mut other = ClusterTable::default();
    other.intern("a\u{0301}").expect("interned");
    assert_ne!(one, other);

    empty.intern("e\u{0301}").expect("interned");
    assert_eq!(one, empty, "same clusters, however each got there");
}

/// A full table must still resolve and dedup the entries it holds.
#[test]
fn interning_refuses_a_new_entry_once_the_table_is_full() {
    let mut t = ClusterTable::default();
    for i in 0..CLUSTER_TABLE_CAP {
        assert!(
            t.intern(&format!("a{i}")).is_some(),
            "entry {i} is under the cap"
        );
    }
    assert_eq!(t.len(), CLUSTER_TABLE_CAP);
    assert_eq!(t.intern("brand new"), None);
    assert_eq!(t.len(), CLUSTER_TABLE_CAP);
    assert_eq!(
        t.intern("a0"),
        NonZeroU32::new(1),
        "a full table must still resolve its own entries",
    );
}

#[test]
fn interning_refuses_text_past_the_entry_cap() {
    let mut t = ClusterTable::default();
    assert!(t.intern(&"x".repeat(ClusterText::CAP)).is_some());
    assert_eq!(t.intern(&"x".repeat(ClusterText::CAP + 1)), None);
    assert_eq!(t.len(), 1);
}

/// The handle comes off the wire; one near `u32::MAX` would otherwise
/// grow the slot vector to tens of GB from a single message.
#[test]
fn installing_drops_a_handle_past_the_table_cap() {
    let mut t = ClusterTable::default();
    let hostile = NonZeroU32::new(u32::MAX).expect("nonzero");
    t.install(hostile, text("boom"));
    assert_eq!(t.len(), 0, "no table growth from a rejected handle");
    assert_eq!(t.get(hostile), None);
    let last = NonZeroU32::new(u32::try_from(CLUSTER_TABLE_CAP).expect("fits")).expect("nonzero");
    t.install(last, text("ok"));
    assert_eq!(t.get(last), Some("ok"));
    assert_eq!(t.len(), CLUSTER_TABLE_CAP);
    assert_eq!(
        t.get(NonZeroU32::new(1).expect("nonzero")),
        None,
        "spanning to a high handle installs nothing below it",
    );
}

/// Visible-first rehydrate can deliver a high id before the low ones it
/// skipped.
#[test]
fn installing_a_high_id_first_leaves_a_gap_that_a_later_low_id_backfills() {
    let mut t = ClusterTable::default();
    let id = |n| NonZeroU32::new(n).expect("nonzero");
    t.install(id(4), text("high"));
    assert_eq!(t.get(id(4)), Some("high"));
    for missing in [1, 2, 3] {
        assert_eq!(t.get(id(missing)), None, "handle {missing} never arrived");
    }
    t.install(id(2), text("low"));
    assert_eq!(t.get(id(2)), Some("low"));
    assert_eq!(t.get(id(1)), None, "backfilling one id fills only that id");
    assert_eq!(
        t.get(id(4)),
        Some("high"),
        "the backfill preserves the tail"
    );
}

/// A gap falls back in the renderer; an empty entry draws nothing.
#[test]
fn an_installed_empty_cluster_is_not_a_gap() {
    let mut t = ClusterTable::default();
    let id = |n| NonZeroU32::new(n).expect("nonzero");
    t.install(id(2), ClusterText::default());
    assert_eq!(t.get(id(2)), Some(""), "an empty entry resolves to empty");
    assert_eq!(t.get(id(1)), None, "the skipped slot stays absent");
}
