//! Property tests for the glyph-atlas shelf packer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::num::NonZeroU32;

use felis_render_wgpu::atlas::{Allocation, ShelfAtlas};
use proptest::prelude::*;

const fn overlap(a: Allocation, b: Allocation) -> bool {
    if a.width == 0 || a.height == 0 || b.width == 0 || b.height == 0 {
        return false;
    }
    let ax2 = a.x + a.width;
    let ay2 = a.y + a.height;
    let bx2 = b.x + b.width;
    let by2 = b.y + b.height;
    a.x < bx2 && b.x < ax2 && a.y < by2 && b.y < ay2
}

proptest! {
    /// Every successful allocation lies within the atlas and overlaps no
    /// earlier successful allocation.
    #[test]
    fn successful_allocations_never_overlap_and_stay_in_bounds(
        atlas_w in 16u32..=512,
        atlas_h in 16u32..=512,
        sizes in proptest::collection::vec((1u32..=64, 1u32..=64), 0..64),
    ) {
        let mut atlas = ShelfAtlas::new(
            NonZeroU32::new(atlas_w).unwrap(),
            NonZeroU32::new(atlas_h).unwrap(),
        );
        let mut placed: Vec<Allocation> = Vec::new();
        for (w, h) in sizes {
            if let Some(a) = atlas.alloc(w, h) {
                prop_assert!(a.x + a.width <= atlas.width());
                prop_assert!(a.y + a.height <= atlas.height());
                for prev in &placed {
                    prop_assert!(
                        !overlap(a, *prev),
                        "overlap: {a:?} vs {prev:?} (atlas {atlas_w}x{atlas_h})"
                    );
                }
                placed.push(a);
            }
        }
    }

    /// After `reset`, the atlas allocates exactly like a fresh one.
    #[test]
    fn reset_brings_atlas_back_to_a_fresh_state(
        atlas_w in 16u32..=128,
        atlas_h in 16u32..=128,
        sizes in proptest::collection::vec((1u32..=32, 1u32..=32), 0..16),
    ) {
        let mut a = ShelfAtlas::new(
            NonZeroU32::new(atlas_w).unwrap(),
            NonZeroU32::new(atlas_h).unwrap(),
        );
        let mut b = ShelfAtlas::new(
            NonZeroU32::new(atlas_w).unwrap(),
            NonZeroU32::new(atlas_h).unwrap(),
        );
        for (w, h) in &sizes {
            let _ = a.alloc(*w, *h);
        }
        a.reset();
        for (w, h) in &sizes {
            prop_assert_eq!(a.alloc(*w, *h), b.alloc(*w, *h));
        }
    }
}
