//! Properties of `apply_screen_switch` across arbitrary alt-screen
//! toggle sequences (`docs/reference/protocols/kitty-graphics.md`
//! "Lifecycle").

use felis_grid::images::{ImageStore, Placements};

use crate::common::{KittyState, b64, body};
use proptest::prelude::*;

const TEST_STORE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum Toggle {
    Enter,
    Leave,
    PlacePrimary { id: u32 },
    PlaceAlt { id: u32 },
}

fn toggle_strategy() -> impl Strategy<Value = Toggle> {
    prop_oneof![
        Just(Toggle::Enter),
        Just(Toggle::Leave),
        (1u32..16).prop_map(|id| Toggle::PlacePrimary { id }),
        (16u32..32).prop_map(|id| Toggle::PlaceAlt { id }),
    ]
}

struct Harness(KittyState);

impl std::ops::Deref for Harness {
    type Target = KittyState;
    fn deref(&self) -> &KittyState {
        &self.0
    }
}

impl std::ops::DerefMut for Harness {
    fn deref_mut(&mut self) -> &mut KittyState {
        &mut self.0
    }
}

impl Harness {
    fn new() -> Self {
        Self(KittyState::new(24, 80, TEST_STORE_CAP))
    }

    fn place(&mut self, id: u32) {
        self.dispatch(&body(
            &format!("Ga=T,i={id},f=32,s=1,v=1,c=4,r=2"),
            &b64(&[0xAA, 0xBB, 0xCC, 0xDD]),
        ));
    }

    fn drive(&mut self, t: Toggle) {
        match t {
            Toggle::Enter => self.replay(b"\x1b[?1049h"),
            Toggle::Leave => self.replay(b"\x1b[?1049l"),
            Toggle::PlacePrimary { id } | Toggle::PlaceAlt { id } => self.place(id),
        }
    }

    fn on_primary(&self) -> bool {
        !self.grid.on_alternate_screen()
    }
}

fn refcount_sum(images: &ImageStore) -> u32 {
    images
        .iter_ids()
        .filter_map(|id| images.get(id))
        .map(felis_grid::images::ImageEntry::refcount)
        .sum()
}

fn placement_keys(p: &Placements) -> Vec<(u32, Option<u32>, i32, u16, (u16, u16), i32, bool)> {
    let mut v: Vec<_> = p
        .iter()
        .map(|p| {
            (
                p.image_id.0,
                p.placement_id.map(|id| id.0),
                p.anchor.row,
                p.anchor.col,
                p.extent(),
                p.z_index,
                p.no_cursor_move,
            )
        })
        .collect();
    v.sort();
    v
}

proptest! {
    #[test]
    fn primary_then_alt_round_trip_preserves_placements(
        primary_ids in proptest::collection::vec(1u32..16, 0..6),
    ) {
        let mut h = Harness::new();
        for id in &primary_ids {
            h.drive(Toggle::PlacePrimary { id: *id });
        }
        h.events.clear();
        let before = placement_keys(&h.placements);

        h.drive(Toggle::Enter);
        h.drive(Toggle::Leave);

        let after = placement_keys(&h.placements);
        prop_assert_eq!(before, after);
        prop_assert!(h.saved.primary.is_none());
        prop_assert!(h.on_primary());
    }

    #[test]
    fn alt_round_trip_destroys_alt_placements_only(
        ops in proptest::collection::vec(toggle_strategy(), 0..32),
    ) {
        let mut h = Harness::new();
        for op in &ops {
            h.drive(*op);
        }
        if !h.on_primary() {
            h.drive(Toggle::Leave);
        }
        // One ref per live placement: `i=` reuse makes
        // `ImageStore::insert` overwrite and reset the refcount to zero.
        prop_assert!(h.saved.primary.is_none(), "primary-only end state");
        prop_assert_eq!(refcount_sum(&h.images) as usize, h.placements.len());
    }

    /// A slot populated while on primary would be overwritten by the
    /// next `?1049h`.
    #[test]
    fn save_slot_is_populated_iff_on_alt(
        ops in proptest::collection::vec(toggle_strategy(), 0..32),
    ) {
        let mut h = Harness::new();
        for op in &ops {
            h.drive(*op);
            prop_assert_eq!(
                h.saved.primary.is_some(),
                !h.on_primary(),
                "slot vs grid state out of sync after op {:?}",
                op,
            );
        }
    }
}
