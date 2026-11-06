//! Which texels of an image an atlas slot needs rewritten to show the
//! image's current pixels: the whole image, or only the rectangles its
//! displayed frame was patched in since the slot was last written.

use std::collections::HashMap;

use felis_protocol::{ImageId, messages::SourceRect};

use crate::{ImageData, ImageSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageUpload {
    Whole,
    /// Each inside the image; empty when the slot already shows the
    /// current pixels.
    Rects(Vec<SourceRect>),
}

/// The daemon's own per-frame cap, so a single fan-out's rectangles
/// travel as they arrived.
pub const MAX_UPLOAD_RECTS: usize = 16;

#[derive(Debug, Clone, Copy)]
struct Shown {
    generation: u64,
    width: u32,
    height: u32,
}

/// What each atlas slot's texels show. The owner forgets an id whenever
/// its slot is reallocated or the sheet recycled, since the texels then
/// are someone else's.
#[derive(Debug, Default)]
pub struct ResidentImages {
    shown: HashMap<ImageId, Shown>,
    scratch: Vec<SourceRect>,
}

impl ResidentImages {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The upload that brings `id`'s slot to `data`.
    pub fn plan(
        &mut self,
        id: ImageId,
        data: &ImageData<'_>,
        source: &impl ImageSource,
    ) -> ImageUpload {
        let Some(held) = self
            .shown
            .get(&id)
            .filter(|held| (held.width, held.height) == (data.width, data.height))
        else {
            return ImageUpload::Whole;
        };
        self.scratch.clear();
        if !source.edits_since(id, held.generation, &mut self.scratch) {
            return ImageUpload::Whole;
        }
        coalesce(&self.scratch, data.width, data.height)
    }

    /// Only after the write was recorded: a slot whose write failed must
    /// take a whole upload next time, not a patch over stale texels.
    pub fn uploaded(&mut self, id: ImageId, data: &ImageData<'_>) {
        self.shown.insert(
            id,
            Shown {
                generation: data.generation,
                width: data.width,
                height: data.height,
            },
        );
    }

    pub fn forget(&mut self, id: ImageId) {
        self.shown.remove(&id);
    }

    pub fn clear(&mut self) {
        self.shown.clear();
    }
}

/// Past half the image a whole upload is taken instead, as the daemon
/// does for the wire: the per-rectangle copies then buy back too little.
#[must_use]
pub fn coalesce(rects: &[SourceRect], width: u32, height: u32) -> ImageUpload {
    let inside = |r: &SourceRect| {
        u64::from(r.x) + u64::from(r.width) <= u64::from(width)
            && u64::from(r.y) + u64::from(r.height) <= u64::from(height)
    };
    if !rects.iter().all(inside) {
        return ImageUpload::Whole;
    }
    let mut kept: Vec<SourceRect> = rects
        .iter()
        .filter(|r| r.width > 0 && r.height > 0)
        .copied()
        .collect();
    if kept.len() > MAX_UPLOAD_RECTS {
        kept = vec![bounding_box(&kept)];
    }
    let area: u64 = kept
        .iter()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum();
    if area * 2 > u64::from(width) * u64::from(height) {
        return ImageUpload::Whole;
    }
    ImageUpload::Rects(kept)
}

fn bounding_box(rects: &[SourceRect]) -> SourceRect {
    let x = rects.iter().map(|r| r.x).min().unwrap_or(0);
    let y = rects.iter().map(|r| r.y).min().unwrap_or(0);
    let right = rects.iter().map(|r| r.x + r.width).max().unwrap_or(0);
    let bottom = rects.iter().map(|r| r.y + r.height).max().unwrap_or(0);
    SourceRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

/// `data`'s pixels from `rect`'s first texel on, and the byte stride
/// between its rows; `rect` must lie inside the image.
#[must_use]
pub fn rect_rows<'a>(data: &ImageData<'a>, rect: SourceRect) -> (&'a [u8], u32) {
    let bpp = data.format.bytes_per_pixel();
    let stride = data.width as usize * bpp;
    let from = rect.y as usize * stride + rect.x as usize * bpp;
    (data.pixels.get(from..).unwrap_or_default(), stride as u32)
}

#[cfg(test)]
mod tests {
    use felis_protocol::messages::ImageFormat;
    use proptest::prelude::*;

    use super::*;

    const fn rect(x: u32, y: u32, width: u32, height: u32) -> SourceRect {
        SourceRect {
            x,
            y,
            width,
            height,
        }
    }

    /// One image whose edit log the test sets directly.
    struct Edited {
        generation: u64,
        edits: Option<Vec<SourceRect>>,
        since: std::cell::Cell<Option<u64>>,
    }

    impl ImageSource for Edited {
        fn image_data(&self, _: ImageId) -> Option<ImageData<'_>> {
            None
        }

        fn edits_since(&self, _: ImageId, generation: u64, out: &mut Vec<SourceRect>) -> bool {
            self.since.set(Some(generation));
            self.edits.as_ref().is_some_and(|e| {
                out.extend_from_slice(e);
                true
            })
        }
    }

    fn data(generation: u64, width: u32, height: u32) -> ImageData<'static> {
        ImageData {
            width,
            height,
            format: ImageFormat::Rgba32,
            pixels: &[],
            generation,
        }
    }

    fn edited(generation: u64, edits: Option<Vec<SourceRect>>) -> Edited {
        Edited {
            generation,
            edits,
            since: std::cell::Cell::new(None),
        }
    }

    #[test]
    fn an_image_never_uploaded_takes_a_whole_upload() {
        let mut resident = ResidentImages::new();
        let source = edited(2, Some(vec![rect(0, 0, 1, 1)]));
        let plan = resident.plan(ImageId(1), &data(source.generation, 8, 8), &source);
        assert_eq!(plan, ImageUpload::Whole);
    }

    #[test]
    fn a_resident_image_takes_the_edits_since_the_generation_its_slot_shows() {
        let mut resident = ResidentImages::new();
        resident.uploaded(ImageId(1), &data(5, 8, 8));
        let source = edited(7, Some(vec![rect(1, 2, 3, 1)]));
        let plan = resident.plan(ImageId(1), &data(7, 8, 8), &source);
        assert_eq!(plan, ImageUpload::Rects(vec![rect(1, 2, 3, 1)]));
        assert_eq!(source.since.get(), Some(5));
    }

    #[test]
    fn a_source_that_cannot_answer_takes_a_whole_upload() {
        let mut resident = ResidentImages::new();
        resident.uploaded(ImageId(1), &data(5, 8, 8));
        let plan = resident.plan(ImageId(1), &data(7, 8, 8), &edited(7, None));
        assert_eq!(plan, ImageUpload::Whole);
    }

    #[test]
    fn a_forgotten_cleared_or_resized_slot_takes_a_whole_upload() {
        let source = edited(7, Some(vec![rect(0, 0, 1, 1)]));
        let mut resident = ResidentImages::new();
        resident.uploaded(ImageId(1), &data(5, 8, 8));
        resident.forget(ImageId(1));
        assert_eq!(
            resident.plan(ImageId(1), &data(7, 8, 8), &source),
            ImageUpload::Whole
        );
        resident.uploaded(ImageId(1), &data(5, 8, 8));
        resident.clear();
        assert_eq!(
            resident.plan(ImageId(1), &data(7, 8, 8), &source),
            ImageUpload::Whole
        );
        resident.uploaded(ImageId(1), &data(5, 8, 8));
        assert_eq!(
            resident.plan(ImageId(1), &data(7, 8, 9), &source),
            ImageUpload::Whole
        );
    }

    #[test]
    fn coalesce_drops_empty_rects_and_refuses_one_outside_the_image() {
        assert_eq!(
            coalesce(&[rect(0, 0, 0, 4), rect(1, 1, 1, 1)], 8, 8),
            ImageUpload::Rects(vec![rect(1, 1, 1, 1)])
        );
        assert_eq!(coalesce(&[rect(7, 0, 2, 1)], 8, 8), ImageUpload::Whole);
        assert_eq!(coalesce(&[], 8, 8), ImageUpload::Rects(vec![]));
    }

    #[test]
    fn coalesce_merges_past_the_cap_and_goes_whole_past_half_the_image() {
        let many: Vec<SourceRect> = (0..=MAX_UPLOAD_RECTS as u32)
            .map(|i| rect(i, i, 1, 1))
            .collect();
        let side = MAX_UPLOAD_RECTS as u32 * 2;
        assert_eq!(
            coalesce(&many, side, side),
            ImageUpload::Rects(vec![rect(0, 0, side / 2 + 1, side / 2 + 1)])
        );
        assert_eq!(coalesce(&[rect(0, 0, 8, 5)], 8, 8), ImageUpload::Whole);
        assert_eq!(
            coalesce(&[rect(0, 0, 8, 4)], 8, 8),
            ImageUpload::Rects(vec![rect(0, 0, 8, 4)])
        );
    }

    #[test]
    fn rect_rows_start_at_the_rect_origin_with_the_image_stride() {
        let pixels: Vec<u8> = (0..4 * 3 * 3).collect();
        let image = ImageData {
            width: 4,
            height: 3,
            format: ImageFormat::Rgb24,
            pixels: &pixels,
            generation: 0,
        };
        let (rows, stride) = rect_rows(&image, rect(1, 2, 2, 1));
        assert_eq!((rows[0], stride), (27, 12));
    }

    fn arb_rect(side: u32) -> impl Strategy<Value = SourceRect> {
        (0..side, 0..side)
            .prop_flat_map(move |(x, y)| (Just(x), Just(y), 0..=side - x, 0..=side - y))
            .prop_map(|(x, y, width, height)| rect(x, y, width, height))
    }

    fn covered(rects: &[SourceRect], x: u32, y: u32) -> bool {
        rects
            .iter()
            .any(|r| (r.x..r.x + r.width).contains(&x) && (r.y..r.y + r.height).contains(&y))
    }

    proptest! {
        /// Whatever coalescing does, every texel an edit touched is
        /// rewritten, and nothing outside the image is.
        #[test]
        fn coalesce_covers_every_edited_texel(
            rects in prop::collection::vec(arb_rect(12), 0..40),
        ) {
            let side = 12;
            match coalesce(&rects, side, side) {
                ImageUpload::Whole => {}
                ImageUpload::Rects(kept) => {
                    prop_assert!(kept.len() <= MAX_UPLOAD_RECTS);
                    for r in &kept {
                        prop_assert!(r.x + r.width <= side && r.y + r.height <= side);
                    }
                    for x in 0..side {
                        for y in 0..side {
                            if covered(&rects, x, y) {
                                prop_assert!(covered(&kept, x, y), "({x}, {y})");
                            }
                        }
                    }
                }
            }
        }
    }
}
