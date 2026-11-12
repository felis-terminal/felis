use crate::test_support::{apc_bodies, drive};
use crate::*;
use felis_vt::Parser;
use proptest::prelude::*;

use super::*;

mod advance_cursor;
mod apc_outbox;
mod extent;
mod placements;
mod scroll_band;
mod store;

pub(super) fn entry(size: usize) -> ImageEntry {
    ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8; size])
}

/// Caps and expected totals are written through the charge rather than
/// as pixel counts, so they keep meaning what they say if the charge
/// changes.
pub(super) fn charge(size: usize) -> usize {
    entry(size).byte_len()
}

pub(super) fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap()
}

pub(super) fn cells(n: u16) -> Extent {
    Extent::Requested(nz(n))
}

pub(super) fn placement(image: u32, placement: Option<u32>, z: i32) -> Placement {
    Placement {
        image_id: ImageId(image),
        placement_id: placement.map(PlacementId),
        anchor: CellPos { row: 1, col: 1 },
        cols: cells(1),
        rows: cells(1),
        source: None,
        z_index: z,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    }
}

pub(super) fn requested(
    image: u32,
    pid: u32,
    cols: u16,
    rows: u16,
    resolved: (u16, u16),
) -> Placement {
    let axis = |requested, other, resolved| match (NonZeroU16::new(requested), other) {
        (Some(n), _) => Extent::Requested(n),
        (None, 0) => Extent::Natural(nz(resolved)),
        (None, _) => Extent::Derived(nz(resolved)),
    };
    Placement {
        cols: axis(cols, rows, resolved.0),
        rows: axis(rows, cols, resolved.1),
        ..placement(image, Some(pid), 0)
    }
}
