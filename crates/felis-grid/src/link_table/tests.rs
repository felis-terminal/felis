use super::*;

#[test]
fn interning_the_same_link_twice_reuses_one_row() {
    let mut t = LinkTable::default();
    let uri = "https://example.test/a";
    let first = t.intern(None, uri).expect("interned");
    assert_eq!(t.intern(None, uri), Some(first));
    assert_eq!(t.len(), 1);
    // A distinct `id=` is a distinct link (OSC 8 anchor semantics).
    let anchored = t
        .intern(Some("anchor"), "https://example.test/a")
        .expect("interned");
    assert_ne!(anchored, first);
    assert_eq!(t.len(), 2);
}

/// `PartialEq` is hand-written to skip the index and total; it must
/// still see different entries.
#[test]
fn tables_holding_different_links_are_not_equal() {
    let mut empty = LinkTable::default();
    assert!(empty.is_empty());

    let mut one = LinkTable::default();
    one.intern(None, "h://a").expect("interned");
    assert!(!one.is_empty());
    assert_ne!(one, empty);

    let mut other = LinkTable::default();
    other.intern(None, "h://b").expect("interned");
    assert_ne!(one, other);

    empty.intern(None, "h://a").expect("interned");
    assert_eq!(one, empty, "same links, however each got there");
}

#[test]
fn the_byte_budget_refuses_a_link_the_table_cannot_afford() {
    let mut t = LinkTable::default();
    let long = "x".repeat(4000);
    let mut interned = 0;
    for i in 0..65_535 {
        if t.intern(None, &format!("https://example.test/{i}/{long}"))
            .is_none()
        {
            break;
        }
        interned += 1;
    }
    assert!(
        interned < 65_535,
        "the budget must bind before the id space does at this URI length"
    );
    assert!(
        t.bytes <= LINK_TABLE_BYTE_CAP,
        "charged {} against a {LINK_TABLE_BYTE_CAP}-byte cap",
        t.bytes
    );
    // A refusal must not wedge the table.
    assert!(t.intern(None, "https://example.test/short").is_some());
}

#[test]
fn the_id_space_refuses_a_link_past_u16_max_rows() {
    let mut t = LinkTable::default();
    for i in 0..u16::MAX {
        assert!(t.intern(None, &format!("h://{i}")).is_some(), "row {i}");
    }
    assert_eq!(t.len(), u16::MAX as usize);
    assert_eq!(t.intern(None, "h://overflow"), None);
    assert_eq!(t.len(), u16::MAX as usize);
    let last = NonZeroU16::new(u16::MAX).expect("nonzero");
    assert_eq!(t.get(last).map(|e| e.uri.as_str()), Some("h://65534"));
    assert_eq!(t.intern(None, "h://65534"), Some(last));
}

#[test]
fn the_last_link_the_budget_can_afford_is_still_interned() {
    let mut t = LinkTable::default();
    let long = "x".repeat(4000);
    for i in 0.. {
        if t.intern(None, &format!("h://{i}/{long}")).is_none() {
            break;
        }
    }
    let fixed = LinkTable::charge(None, "");
    let exact = LINK_TABLE_BYTE_CAP - t.bytes - fixed;
    assert!(
        t.intern(None, &"y".repeat(exact)).is_some(),
        "lands on the cap"
    );
    assert_eq!(t.bytes, LINK_TABLE_BYTE_CAP);
    assert_eq!(t.intern(None, "z"), None, "one byte past is refused");
}

#[test]
fn the_charge_grows_with_every_part_of_an_entry() {
    let base = LinkTable::charge(None, "");
    assert!(
        base >= size_of::<HyperlinkEntry>(),
        "the row in the vector is charged for"
    );
    assert!(
        LinkTable::charge(None, "xx") > LinkTable::charge(None, "x"),
        "a longer URI costs more"
    );
    assert!(
        LinkTable::charge(Some("a"), "x") > LinkTable::charge(None, "x"),
        "an anchor id is payload too"
    );
}

/// Visible-first rehydrate can deliver a high id before the low ones it
/// skipped.
#[test]
fn installing_a_high_id_first_leaves_a_gap_that_a_later_low_id_backfills() {
    let mut t = LinkTable::default();
    let id = |n| NonZeroU16::new(n).expect("nonzero");
    let entry = |uri: &str| HyperlinkEntry {
        id: None,
        uri: LinkText::new(uri).expect("under cap"),
    };
    t.install(id(4), entry("h://high"));
    assert_eq!(t.get(id(4)).map(|e| e.uri.as_str()), Some("h://high"));
    for missing in [1, 2, 3] {
        assert!(
            t.get(id(missing)).is_none(),
            "handle {missing} never arrived"
        );
    }
    t.install(id(2), entry("h://low"));
    assert_eq!(t.get(id(2)).map(|e| e.uri.as_str()), Some("h://low"));
    assert!(
        t.get(id(1)).is_none(),
        "backfilling one id fills only that id"
    );
    assert_eq!(
        t.get(id(4)).map(|e| e.uri.as_str()),
        Some("h://high"),
        "the backfill preserves the tail",
    );
}

/// A gap is an un-arrived link; an empty URI is a link the daemon sent.
#[test]
fn an_installed_empty_uri_is_not_a_gap() {
    let mut t = LinkTable::default();
    let id = |n| NonZeroU16::new(n).expect("nonzero");
    t.install(
        id(2),
        HyperlinkEntry {
            id: None,
            uri: LinkText::new("").expect("under cap"),
        },
    );
    assert_eq!(t.get(id(2)).map(|e| e.uri.as_str()), Some(""));
    assert!(t.get(id(1)).is_none(), "the skipped slot stays absent");
}

/// A high id is charged for its own entry, not for the gap it opens.
#[test]
fn the_byte_budget_still_binds_when_ids_arrive_out_of_order() {
    let mut t = LinkTable::default();
    let uri = "x".repeat(LinkText::CAP);
    let mut installed = 0_u32;
    for n in (1..=u16::MAX).rev() {
        let id = NonZeroU16::new(n).expect("nonzero");
        t.install(
            id,
            HyperlinkEntry {
                id: None,
                uri: LinkText::new(&uri).expect("under cap"),
            },
        );
        if t.get(id).is_some() {
            installed += 1;
        }
    }
    assert!(
        installed < u32::from(u16::MAX),
        "the budget must refuse some of a full-id-space flood at this URI length",
    );
    assert!(
        t.bytes <= LINK_TABLE_BYTE_CAP,
        "charged {} against a {LINK_TABLE_BYTE_CAP}-byte cap",
        t.bytes,
    );
}

/// Not truncation: a truncated URI is a different target.
#[test]
fn interning_refuses_an_anchor_past_the_cap() {
    let mut t = LinkTable::default();
    assert!(t.intern(None, &"x".repeat(LinkText::CAP)).is_some());
    assert_eq!(t.intern(None, &"x".repeat(LinkText::CAP + 1)), None);
    assert_eq!(
        t.intern(Some(&"x".repeat(LinkText::CAP + 1)), "h://a"),
        None
    );
    assert_eq!(t.len(), 1);
}
