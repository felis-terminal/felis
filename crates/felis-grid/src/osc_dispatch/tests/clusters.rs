use super::*;

/// A mutant returning the constant `1` or leaking the table fails the
/// second assertion.
#[test]
fn cluster_count_and_table_track_interned_entries() {
    let mut g = Grid::new(1, 8);
    assert_eq!(g.cluster_count(), 0);
    let id1 = g.intern_cluster("e\u{0301}").expect("intern");
    let id2 = g.intern_cluster("o\u{0308}").expect("intern");
    assert_eq!(g.cluster_count(), 2);
    let table = g.cluster_table();
    assert_eq!(table.len(), 2);
    assert_eq!(table.get(id1), Some("e\u{0301}"));
    assert_eq!(table.get(id2), Some("o\u{0308}"));
}

/// Installing at a sparse id spans the table to `id` and leaves the ids
/// below it absent: a visible-first rehydrate ships the entries its
/// rows name, so the skipped ids are entries still in flight.
#[test]
fn install_cluster_places_text_at_the_daemon_assigned_id() {
    use std::num::NonZeroU32;
    let mut g = Grid::new(1, 8);
    let id = NonZeroU32::new(3).unwrap();
    g.install_cluster(id, ClusterText::new("zz").expect("under cap"));
    assert_eq!(g.cluster_count(), 3, "the table spans to the installed id");
    assert_eq!(g.cluster_str(id), Some("zz"));
    assert_eq!(g.cluster_str(NonZeroU32::new(1).unwrap()), None);
    assert_eq!(g.cluster_str(NonZeroU32::new(2).unwrap()), None);
    g.install_cluster(
        NonZeroU32::new(2).unwrap(),
        ClusterText::new("mid").expect("under cap"),
    );
    assert_eq!(g.cluster_count(), 3);
    assert_eq!(g.cluster_str(NonZeroU32::new(2).unwrap()), Some("mid"));
    assert_eq!(g.cluster_str(NonZeroU32::new(1).unwrap()), None);
}
