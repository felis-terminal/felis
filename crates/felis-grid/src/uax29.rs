//! The UAX#29 grapheme-break properties GB11 reads, generated from the
//! UCD of the version unicode-width encodes, which unicode-width does
//! not expose. The `unicode-segmentation` cross-checks in the tests
//! catch a version skew.

use std::cmp::Ordering;

mod tables;

use tables::{EXTEND, EXTENDED_PICTOGRAPHIC};

fn in_ranges(ranges: &[(char, char)], c: char) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if hi < c {
                Ordering::Less
            } else if lo > c {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

pub(crate) fn is_extended_pictographic(c: char) -> bool {
    in_ranges(EXTENDED_PICTOGRAPHIC, c)
}

pub(crate) fn is_grapheme_extend(c: char) -> bool {
    in_ranges(EXTEND, c)
}

#[cfg(test)]
mod tests;
