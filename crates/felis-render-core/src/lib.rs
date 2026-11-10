#![doc = include_str!("../README.md")]

use felis_protocol::{
    ImageId,
    messages::{ImageFormat, SourceRect},
};

pub mod appearance;
pub mod atlas;
pub mod blend;
#[cfg(feature = "glyphs")]
pub mod box_drawing;
pub mod contrast;
pub mod display_list;
#[cfg(feature = "glyphs")]
pub mod glyphs;
pub mod image_upload;
pub mod layout;
pub mod overlay;
#[cfg(feature = "glyphs")]
pub mod painter;
pub mod palette;

/// `start` / `end` are normalized by the client: row-major min/max for
/// a linear selection, axis-aligned bounding box for a rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: (u16, u16),
    pub end: (u16, u16),
    pub rectangle: bool,
}

/// `pixels.len()` equals `width * height * bytes_per_pixel(format)`;
/// the daemon dispatcher enforces it.
#[derive(Debug, Clone, Copy)]
pub struct ImageData<'a> {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub pixels: &'a [u8],
    /// Names these pixels: a source answers [`ImageSource::edits_since`]
    /// for it once the renderer has uploaded them.
    pub generation: u64,
}

/// Supplies image pixels for GPU atlas re-uploads when the atlas recycles itself.
///
/// Images are held in `felis_client_core::ImageShadow`. Returns `None` if pixels have not
/// fully arrived or were deleted, in which case placements paint nothing.
pub trait ImageSource {
    fn image_data(&self, id: ImageId) -> Option<ImageData<'_>>;

    /// Pushes onto `out` every rectangle `id`'s pixels changed in since
    /// [`ImageData::generation`] was `generation`, or returns `false` when
    /// the source cannot say, and the renderer uploads the whole image.
    fn edits_since(&self, id: ImageId, generation: u64, out: &mut Vec<SourceRect>) -> bool {
        let _ = (id, generation, out);
        false
    }
}
