//! First-fit shelf packer for the glyph atlas. Shelves are never
//! defragmented: when the atlas fills, the caller resets and rebuilds.

use std::num::NonZeroU32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub struct ShelfAtlas {
    width: u32,
    height: u32,
    shelves: Vec<Shelf>,
    next_shelf_y: u32,
}

#[derive(Debug)]
struct Shelf {
    y_origin: u32,
    height: u32,
    x_cursor: u32,
}

impl ShelfAtlas {
    #[must_use]
    pub const fn new(width: NonZeroU32, height: NonZeroU32) -> Self {
        Self {
            width: width.get(),
            height: height.get(),
            shelves: Vec::new(),
            next_shelf_y: 0,
        }
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn alloc(&mut self, width: u32, height: u32) -> Option<Allocation> {
        if width == 0 || height == 0 {
            return Some(Allocation {
                x: 0,
                y: 0,
                width,
                height,
            });
        }
        if width > self.width || height > self.height {
            return None;
        }
        for shelf in &mut self.shelves {
            if height <= shelf.height && shelf.x_cursor + width <= self.width {
                let alloc = Allocation {
                    x: shelf.x_cursor,
                    y: shelf.y_origin,
                    width,
                    height,
                };
                shelf.x_cursor += width;
                return Some(alloc);
            }
        }
        if self.next_shelf_y.saturating_add(height) > self.height {
            return None;
        }
        let y = self.next_shelf_y;
        self.shelves.push(Shelf {
            y_origin: y,
            height,
            x_cursor: width,
        });
        self.next_shelf_y += height;
        Some(Allocation {
            x: 0,
            y,
            width,
            height,
        })
    }

    pub fn reset(&mut self) {
        self.shelves.clear();
        self.next_shelf_y = 0;
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.shelves.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(v: u32) -> NonZeroU32 {
        NonZeroU32::new(v).unwrap()
    }

    fn overlap(a: Allocation, b: Allocation) -> bool {
        let ax2 = a.x + a.width;
        let ay2 = a.y + a.height;
        let bx2 = b.x + b.width;
        let by2 = b.y + b.height;
        a.x < bx2 && b.x < ax2 && a.y < by2 && b.y < ay2
    }

    #[test]
    fn first_alloc_lands_at_origin() {
        let mut atlas = ShelfAtlas::new(nz(64), nz(64));
        let a = atlas.alloc(10, 12).unwrap();
        assert_eq!(
            a,
            Allocation {
                x: 0,
                y: 0,
                width: 10,
                height: 12
            }
        );
    }

    #[test]
    fn fills_first_shelf_then_opens_a_second() {
        let mut atlas = ShelfAtlas::new(nz(20), nz(20));
        let a = atlas.alloc(10, 8).unwrap();
        let b = atlas.alloc(10, 8).unwrap();
        assert_eq!(a.y, b.y, "two items of equal height share a shelf");
        assert_eq!(b.x, 10);

        let c = atlas.alloc(10, 5).unwrap();
        assert_eq!(c.y, 8);
        assert_eq!(c.x, 0);
    }

    #[test]
    fn returns_none_when_atlas_is_full() {
        let mut atlas = ShelfAtlas::new(nz(8), nz(8));
        assert!(atlas.alloc(8, 8).is_some());
        assert!(atlas.alloc(1, 1).is_none(), "no shelves left to open");
    }

    #[test]
    fn rejects_request_larger_than_atlas() {
        let mut atlas = ShelfAtlas::new(nz(16), nz(16));
        assert!(atlas.alloc(17, 1).is_none());
        assert!(atlas.alloc(1, 17).is_none());
    }

    #[test]
    fn zero_sized_alloc_succeeds_without_consuming_space() {
        let mut atlas = ShelfAtlas::new(nz(8), nz(8));
        let a = atlas.alloc(0, 0).unwrap();
        assert_eq!(
            a,
            Allocation {
                x: 0,
                y: 0,
                width: 0,
                height: 0
            }
        );
        assert!(atlas.is_empty());
    }

    #[test]
    fn reset_makes_the_atlas_reusable() {
        let mut atlas = ShelfAtlas::new(nz(8), nz(8));
        atlas.alloc(8, 8).unwrap();
        assert!(atlas.alloc(1, 1).is_none());
        atlas.reset();
        assert!(atlas.alloc(8, 8).is_some());
    }

    #[test]
    fn shelf_height_is_locked_by_first_item() {
        let mut atlas = ShelfAtlas::new(nz(20), nz(30));
        let _tall = atlas.alloc(5, 10).unwrap();
        let next = atlas.alloc(5, 12).unwrap();
        assert_eq!(next.y, 10, "new shelf opens immediately above the first");
    }

    #[test]
    fn allocations_never_overlap_for_a_realistic_glyph_mix() {
        let mut atlas = ShelfAtlas::new(nz(128), nz(128));
        let sizes = [
            (8, 12),
            (8, 12),
            (8, 12),
            (12, 12),
            (8, 14),
            (8, 14),
            (10, 16),
            (10, 16),
            (8, 12),
            (8, 12),
            (16, 20),
            (8, 12),
        ];
        let mut allocs = Vec::new();
        for (w, h) in sizes {
            let a = atlas.alloc(w, h).expect("realistic mix should fit");
            for prev in &allocs {
                assert!(!overlap(a, *prev), "{a:?} overlaps {prev:?}");
            }
            assert!(a.x + a.width <= atlas.width());
            assert!(a.y + a.height <= atlas.height());
            allocs.push(a);
        }
    }
}
