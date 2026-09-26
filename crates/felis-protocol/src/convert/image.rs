//! `ImageMsg` <-> wire.

use std::num::NonZeroU32;

use super::{WireError, decode_enum, narrow};
use crate::kitty_graphics::{ImageId, PlacementId};
use crate::messages;
use crate::messages::{MAX_IMAGE_BYTES, MAX_IMAGE_FRAMES, check_claim};
use crate::wire::v1;

data_free_enum!("ImageFormat", image_format_to_i32, image_format_from_i32, messages::ImageFormat, v1::ImageFormat, {
    Rgb24 => Rgb24,
    Rgba32 => Rgba32,
});

impl From<messages::SourceRect> for v1::SourceRect {
    fn from(r: messages::SourceRect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

impl From<v1::SourceRect> for messages::SourceRect {
    fn from(r: v1::SourceRect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

impl From<&messages::ImageMsg> for v1::ImageMsg {
    fn from(m: &messages::ImageMsg) -> Self {
        use messages::ImageMsg as I;
        use v1::image_msg::Msg;
        let msg = match m {
            I::Header { id, target } => Msg::Header(v1::ImageHeader {
                id: id.0,
                target: Some(match target {
                    messages::ImageTarget::New {
                        width,
                        height,
                        format,
                    } => v1::image_header::Target::Image(v1::ImageNew {
                        width: *width,
                        height: *height,
                        format: image_format_to_i32(*format),
                    }),
                    messages::ImageTarget::Frame { number } => {
                        v1::image_header::Target::Frame(v1::ImageFrame {
                            number: number.get(),
                        })
                    }
                }),
            }),
            I::Chunk { id, bytes } => Msg::Chunk(v1::ImageChunk {
                id: id.0,
                bytes: bytes.clone(),
            }),
            I::Complete { id } => Msg::Complete(v1::ImageComplete { id: id.0 }),
            I::Delete { id } => Msg::Delete(v1::ImageDelete { id: id.0 }),
            I::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
            } => Msg::Placement(v1::ImagePlacement {
                image_id: image_id.0,
                placement_id: placement_id.map(|p| p.0),
                anchor_row: *anchor_row,
                anchor_col: u32::from(*anchor_col),
                cols: u32::from(*cols),
                rows: u32::from(*rows),
                source: source.map(Into::into),
                z_index: *z_index,
            }),
            I::PlacementRemoved {
                image_id,
                placement_id,
            } => Msg::PlacementRemoved(v1::ImagePlacementRemoved {
                image_id: image_id.0,
                placement_id: placement_id.map(|p| p.0),
            }),
            I::VirtualPlacement {
                image_id,
                cols,
                rows,
                z_index,
            } => Msg::VirtualPlacement(v1::ImageVirtualPlacement {
                image_id: image_id.0,
                cols: u32::from(*cols),
                rows: u32::from(*rows),
                z_index: *z_index,
            }),
            I::ShowFrame { id, number } => Msg::ShowFrame(v1::ImageShowFrame {
                id: id.0,
                number: number.get(),
            }),
            I::PlacementsShifted { lines } => {
                Msg::PlacementsShifted(v1::ImagePlacementsShifted { lines: *lines })
            }
        };
        Self { msg: Some(msg) }
    }
}

/// The header is the whole allocation surface of the image family: a
/// receiver reserves a whole frame buffer from a frame of a few dozen
/// bytes. Both claims a target can make are admitted here so no
/// consumer re-checks.
fn header_from_wire(h: v1::ImageHeader) -> Result<messages::ImageMsg, WireError> {
    use v1::image_header::Target;
    let target = match h
        .target
        .ok_or(WireError::MissingOneof("ImageHeader.target"))?
    {
        Target::Image(n) => {
            let format = image_format_from_i32(n.format)?;
            // Frames are complete canvases (`docs/reference/protocols/kitty-graphics.md`).
            // Saturating multiplication prevents debug overflow panics on
            // unbounded `u32::MAX` axes while safely exceeding `MAX_IMAGE_BYTES`.
            let bytes = u64::from(n.width)
                .saturating_mul(u64::from(n.height))
                .saturating_mul(format.bytes_per_pixel() as u64);
            check_claim("ImageNew.pixels", bytes, MAX_IMAGE_BYTES, "bytes")?;
            messages::ImageTarget::New {
                width: n.width,
                height: n.height,
                format,
            }
        }
        Target::Frame(f) => messages::ImageTarget::Frame {
            number: frame_number("ImageFrame.number", f.number)?,
        },
    };
    Ok(messages::ImageMsg::Header {
        id: ImageId(h.id),
        target,
    })
}

/// A 1-based frame number: `0` is how the wire spells absent, and a
/// number is a count of frames the image must have, so the frame cap
/// bounds it directly.
fn frame_number(field: &'static str, raw: u32) -> Result<NonZeroU32, WireError> {
    let number = NonZeroU32::new(raw).ok_or(WireError::OutOfRange { field, value: raw })?;
    check_claim(field, u64::from(raw), MAX_IMAGE_FRAMES as u64, "frames")?;
    Ok(number)
}

impl TryFrom<v1::ImageMsg> for messages::ImageMsg {
    type Error = WireError;
    fn try_from(m: v1::ImageMsg) -> Result<Self, Self::Error> {
        use v1::image_msg::Msg;
        Ok(
            match m.msg.ok_or(WireError::MissingOneof("ImageMsg.msg"))? {
                Msg::Header(h) => header_from_wire(h)?,
                Msg::Chunk(c) => Self::Chunk {
                    id: ImageId(c.id),
                    bytes: c.bytes,
                },
                Msg::Complete(c) => Self::Complete { id: ImageId(c.id) },
                Msg::Delete(d) => Self::Delete { id: ImageId(d.id) },
                Msg::Placement(p) => Self::Placement {
                    image_id: ImageId(p.image_id),
                    placement_id: p.placement_id.map(PlacementId),
                    anchor_row: p.anchor_row,
                    anchor_col: narrow("Placement.anchor_col", p.anchor_col)?,
                    cols: narrow("Placement.cols", p.cols)?,
                    rows: narrow("Placement.rows", p.rows)?,
                    source: p.source.map(Into::into),
                    z_index: p.z_index,
                },
                Msg::PlacementRemoved(p) => Self::PlacementRemoved {
                    image_id: ImageId(p.image_id),
                    placement_id: p.placement_id.map(PlacementId),
                },
                Msg::VirtualPlacement(v) => Self::VirtualPlacement {
                    image_id: ImageId(v.image_id),
                    cols: narrow("VirtualPlacement.cols", v.cols)?,
                    rows: narrow("VirtualPlacement.rows", v.rows)?,
                    z_index: v.z_index,
                },
                Msg::ShowFrame(s) => Self::ShowFrame {
                    id: ImageId(s.id),
                    number: frame_number("ImageShowFrame.number", s.number)?,
                },
                Msg::PlacementsShifted(s) => Self::PlacementsShifted { lines: s.lines },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_IMAGE_BYTES, MAX_IMAGE_FRAMES, WireError, messages, v1};

    fn new_image(width: u32) -> v1::ImageMsg {
        header(v1::image_header::Target::Image(v1::ImageNew {
            width,
            height: 1,
            format: v1::ImageFormat::Rgba32 as i32,
        }))
    }

    fn frame(number: u32) -> v1::ImageMsg {
        header(v1::image_header::Target::Frame(v1::ImageFrame { number }))
    }

    fn header(target: v1::image_header::Target) -> v1::ImageMsg {
        v1::ImageMsg {
            msg: Some(v1::image_msg::Msg::Header(v1::ImageHeader {
                id: 7,
                target: Some(target),
            })),
        }
    }

    /// The per-image cap is admitted exactly, and one pixel past it is
    /// refused: the geometry is the only statement of the byte count,
    /// so it is the surface the cap has to be applied to.
    #[test]
    fn image_bytes_are_admitted_at_the_cap_and_refused_one_past_it() {
        let pixels = u32::try_from(MAX_IMAGE_BYTES / 4).unwrap();
        assert!(matches!(
            messages::ImageMsg::try_from(new_image(pixels)),
            Ok(messages::ImageMsg::Header { .. })
        ));
        assert!(matches!(
            messages::ImageMsg::try_from(new_image(pixels + 1)),
            Err(WireError::OverLimit {
                field: "ImageNew.pixels",
                ..
            })
        ));
        // Both axes at `u32::MAX` overflow the `u64` product; the
        // saturating multiply refuses rather than wrapping into a
        // plausible count.
        assert!(matches!(
            messages::ImageMsg::try_from(header(v1::image_header::Target::Image(v1::ImageNew {
                width: u32::MAX,
                height: u32::MAX,
                format: v1::ImageFormat::Rgba32 as i32,
            }))),
            Err(WireError::OverLimit { .. })
        ));
    }

    /// The root has one encoding: frame `0` does not decode, so no
    /// frame header can claim to be the fresh-image arm.
    #[test]
    fn frame_zero_is_not_a_frame_number() {
        assert!(matches!(
            messages::ImageMsg::try_from(frame(0)),
            Err(WireError::OutOfRange {
                field: "ImageFrame.number",
                value: 0,
            })
        ));
        assert!(matches!(
            messages::ImageMsg::try_from(v1::ImageMsg {
                msg: Some(v1::image_msg::Msg::ShowFrame(v1::ImageShowFrame {
                    id: 7,
                    number: 0,
                })),
            }),
            Err(WireError::OutOfRange {
                field: "ImageShowFrame.number",
                ..
            })
        ));
    }

    /// A number is a count of frames the image must hold, so the cap
    /// admits it up to `MAX_IMAGE_FRAMES` and no further; `u32::MAX`
    /// must not wrap past the check.
    #[test]
    fn frame_number_is_admitted_up_to_the_frame_cap_only() {
        let last = u32::try_from(MAX_IMAGE_FRAMES).unwrap();
        assert!(matches!(
            messages::ImageMsg::try_from(frame(last)),
            Ok(messages::ImageMsg::Header {
                target: messages::ImageTarget::Frame { number },
                ..
            }) if number.get() == last
        ));
        for number in [last + 1, u32::MAX] {
            assert!(
                matches!(
                    messages::ImageMsg::try_from(frame(number)),
                    Err(WireError::OverLimit {
                        field: "ImageFrame.number",
                        ..
                    })
                ),
                "frame number {number} was admitted",
            );
        }
    }

    /// A header naming neither arm is malformed rather than a default
    /// root.
    #[test]
    fn a_header_without_a_target_is_rejected() {
        assert!(matches!(
            messages::ImageMsg::try_from(v1::ImageMsg {
                msg: Some(v1::image_msg::Msg::Header(v1::ImageHeader {
                    id: 7,
                    target: None,
                })),
            }),
            Err(WireError::MissingOneof("ImageHeader.target"))
        ));
    }
}
