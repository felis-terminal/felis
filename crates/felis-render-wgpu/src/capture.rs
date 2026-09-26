//! Off-screen frame capture for non-interactive frontend smoke tests.
//!
//! Re-encodes the cell pass into an off-screen target rather than reading the swapchain,
//! avoiding `COPY_SRC` capability requirements on the surface during normal runs.

use std::collections::HashMap;

use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Extent3d, MapMode, Origin3d,
    PollType, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect,
    TextureDescriptor, TextureDimension, TextureUsages, TextureViewDescriptor,
};

use crate::{Renderer, Target};

const COPY_ROW_ALIGNMENT: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// Every surface format felis selects (`*8Unorm`/`*8UnormSrgb`) is 4
/// bytes per texel; anything else reports no capture.
const SUPPORTED_TEXEL_BYTES: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCapture {
    /// Framebuffer width in physical pixels.
    pub width: u32,
    /// Framebuffer height in physical pixels.
    pub height: u32,
    pub total_pixels: u64,
    /// Pixels whose color differs from the most common color.
    pub painted_pixels: u64,
    pub distinct_colors: usize,
}

impl Renderer {
    /// `None` (no frame yet, unsupported texel size, readback failure)
    /// is no evidence, never a pass. Allocates a full framebuffer and
    /// blocks on the device: one call per smoke run, not a paint loop.
    #[must_use]
    pub fn capture_last_frame(&self) -> Option<FrameCapture> {
        let counts = self.last_counts.as_ref()?;
        let format = self.format;
        if format.block_copy_size(Some(TextureAspect::All)) != Some(SUPPORTED_TEXEL_BYTES) {
            tracing::warn!(?format, "frame capture skipped: unsupported texel size");
            return None;
        }
        let (width, height) = (self.width, self.height);
        let unpadded_row = width * SUPPORTED_TEXEL_BYTES;
        let padded_row = unpadded_row.next_multiple_of(COPY_ROW_ALIGNMENT);

        let texture = self.device.create_texture(&TextureDescriptor {
            label: Some("felis frame capture"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());
        let staging = self.device.create_buffer(&BufferDescriptor {
            label: Some("felis frame capture staging"),
            size: u64::from(padded_row) * u64::from(height),
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("felis frame capture encoder"),
            });
        self.encode_cell_pass(&mut encoder, &view, counts);
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &staging,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        staging.slice(..).map_async(MapMode::Read, |_| ());
        if let Err(err) = self.device.poll(PollType::wait_indefinitely()) {
            tracing::warn!(?err, "frame capture skipped: device poll failed");
            return None;
        }
        let capture = {
            let mapped = match staging.slice(..).get_mapped_range() {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(?err, "frame capture skipped: map failed");
                    return None;
                }
            };
            summarize(&mapped, width, height, padded_row)
        };
        staging.unmap();
        Some(capture)
    }

    /// Replaces `out` with the frame the last `render` drew as tightly
    /// packed `Rgba8UnormSrgb`, `width * height * 4` bytes. Alpha is
    /// whatever the passes composed; nothing promises it is opaque.
    /// Blocks on the device. After a `Poll` or `Map` error `out` holds a
    /// partial frame.
    pub fn read_frame(&mut self, out: &mut Vec<u8>) -> Result<(), ReadFrameError> {
        let Target::Offscreen(offscreen) = &mut self.target else {
            return Err(ReadFrameError::NotOffscreen);
        };
        if !offscreen.rendered {
            return Err(ReadFrameError::NoFrame);
        }
        let (width, height) = (self.width, self.height);
        let unpadded_row = width * SUPPORTED_TEXEL_BYTES;
        let padded_row = unpadded_row.next_multiple_of(COPY_ROW_ALIGNMENT);
        // A texture within the 2D limit can still need a readback past
        // `max_buffer_size` (8193 x 8193 is 270 MB against 256 MiB), and
        // an oversized `create_buffer` is a validation panic, not an Err.
        let strip_rows = strip_rows(self.device.limits().max_buffer_size, padded_row, height)
            .ok_or(ReadFrameError::RowPastBufferLimit)?;
        let staging = offscreen.staging.get_or_insert_with(|| {
            self.device.create_buffer(&BufferDescriptor {
                label: Some("felis offscreen readback"),
                size: u64::from(padded_row) * u64::from(strip_rows),
                usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
                mapped_at_creation: false,
            })
        });

        out.clear();
        out.reserve(unpadded_row as usize * height as usize);
        let mut top = 0;
        while top < height {
            let rows = strip_rows.min(height - top);
            let mut encoder = self
                .device
                .create_command_encoder(&CommandEncoderDescriptor {
                    label: Some("felis offscreen readback encoder"),
                });
            encoder.copy_texture_to_buffer(
                TexelCopyTextureInfo {
                    texture: &offscreen.texture,
                    mip_level: 0,
                    origin: Origin3d { x: 0, y: top, z: 0 },
                    aspect: TextureAspect::All,
                },
                TexelCopyBufferInfo {
                    buffer: staging,
                    layout: TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row),
                        rows_per_image: Some(rows),
                    },
                },
                Extent3d {
                    width,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );
            self.queue.submit(std::iter::once(encoder.finish()));

            let copied = staging.slice(..u64::from(padded_row) * u64::from(rows));
            copied.map_async(MapMode::Read, |_| ());
            self.device
                .poll(PollType::wait_indefinitely())
                .map_err(|e| ReadFrameError::Poll(e.to_string()))?;
            {
                let mapped = copied
                    .get_mapped_range()
                    .map_err(|e| ReadFrameError::Map(e.to_string()))?;
                for row in mapped.chunks_exact(padded_row as usize) {
                    out.extend_from_slice(&row[..unpadded_row as usize]);
                }
            }
            staging.unmap();
            top += rows;
        }
        Ok(())
    }
}

/// Rows per readback strip: as many as `max_buffer_size` holds, at
/// most the frame; `None` when not even one row fits.
fn strip_rows(max_buffer_size: u64, padded_row: u32, height: u32) -> Option<u32> {
    let fit = max_buffer_size / u64::from(padded_row);
    (fit > 0).then(|| u32::try_from(fit).unwrap_or(u32::MAX).min(height.max(1)))
}

#[derive(Debug, thiserror::Error)]
pub enum ReadFrameError {
    #[error("only an offscreen renderer reads its frame back")]
    NotOffscreen,
    #[error("nothing rendered since the target was created or resized")]
    NoFrame,
    #[error("one row of the frame exceeds the device's max_buffer_size")]
    RowPastBufferLimit,
    #[error("device poll: {0}")]
    Poll(String),
    #[error("map readback buffer: {0}")]
    Map(String),
}

fn summarize(bytes: &[u8], width: u32, height: u32, padded_row: u32) -> FrameCapture {
    let mut histogram: HashMap<u32, u64> = HashMap::new();
    for row in 0..height as usize {
        let start = row * padded_row as usize;
        let row_bytes = bytes
            .get(start..start + width as usize * SUPPORTED_TEXEL_BYTES as usize)
            .unwrap_or_default();
        let (texels, _) = row_bytes.as_chunks::<{ SUPPORTED_TEXEL_BYTES as usize }>();
        for texel in texels {
            *histogram.entry(u32::from_ne_bytes(*texel)).or_default() += 1;
        }
    }
    let total: u64 = histogram.values().sum();
    let most_common = histogram.values().copied().max().unwrap_or(0);
    FrameCapture {
        width,
        height,
        total_pixels: total,
        painted_pixels: total - most_common,
        distinct_colors: histogram.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame that cleared and drew nothing scores zero painted pixels.
    #[test]
    fn a_readback_strip_fits_the_buffer_limit() {
        assert_eq!(strip_rows(256 << 20, 33_024, 8193), Some(8128));
        assert_eq!(strip_rows(u64::MAX, 256, 20), Some(20));
        assert_eq!(strip_rows(256, 256, 20), Some(1));
        assert_eq!(strip_rows(255, 256, 20), None);
    }

    #[test]
    fn a_single_color_frame_counts_no_painted_pixels() {
        let bytes = vec![7u8; 4 * 4 * 2];
        let capture = summarize(&bytes, 4, 2, 16);
        assert_eq!(capture.painted_pixels, 0);
        assert_eq!(capture.distinct_colors, 1);
        assert_eq!(capture.total_pixels, 8);
    }

    /// Pixels differing from the dominant color are the painted ones,
    /// and row padding never reaches the histogram.
    #[test]
    fn padding_bytes_stay_out_of_the_histogram() {
        let mut bytes = vec![0u8; 24];
        bytes[8..12].copy_from_slice(&[9, 9, 9, 9]);
        bytes[20..24].copy_from_slice(&[9, 9, 9, 9]);
        bytes[4..8].copy_from_slice(&[1, 2, 3, 4]);
        let capture = summarize(&bytes, 2, 2, 12);
        assert_eq!(capture.total_pixels, 4);
        assert_eq!(capture.distinct_colors, 2);
        assert_eq!(capture.painted_pixels, 1);
    }
}
