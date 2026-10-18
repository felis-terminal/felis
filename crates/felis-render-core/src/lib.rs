//! `felis-render-core`: the renderer-neutral half of the native renderer.

use felis_protocol::{ImageId, messages::ImageFormat};

pub mod atlas;
pub mod box_drawing;
pub mod overlay;
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
}

/// Supplies image pixels for GPU atlas re-uploads when the atlas recycles itself.
///
/// Images are held in `felis_client_core::ImageShadow`. Returns `None` if pixels have not
/// fully arrived or were deleted, in which case placements paint nothing.
pub trait ImageSource {
    fn image_data(&self, id: ImageId) -> Option<ImageData<'_>>;
}
