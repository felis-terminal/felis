//! [`ImageMsg`]: daemon→client Kitty graphics (kind 3).

use std::num::NonZeroU32;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{ArmMeta, CorrelationClass, Directed, Direction, ModeSet};

use crate::kitty_graphics::{ImageId, PlacementId};

/// Decoded pixel format (Kitty `f=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat {
    /// Kitty `f=24`.
    Rgb24,
    /// Kitty `f=32`.
    Rgba32,
}

impl ImageFormat {
    #[must_use]
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb24 => 3,
            Self::Rgba32 => 4,
        }
    }
}

/// Sub-region of an image, in image pixels (Kitty `x` / `y` / `w` /
/// `h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Producer-side cap on a single [`ImageMsg::Chunk`]'s payload bytes.
///
/// Bounds how long interactive input waits behind bulk image frames
/// (`docs/reference/ipc.md` "Backpressure").
pub const MAX_IMAGE_CHUNK_PAYLOAD: usize = 256 * 1024;

const _: () = assert!((MAX_IMAGE_CHUNK_PAYLOAD as u32) < crate::frame::DEFAULT_MAX_BODY);

/// What an [`ImageMsg::Header`] opens a transfer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageTarget {
    /// A fresh image under this id, replacing whatever the id held,
    /// animation frames included.
    New {
        width: u32,
        height: u32,
        format: ImageFormat,
    },
    /// One frame of an image already transferred, in Kitty's 1-based
    /// numbering: `1` is the root frame in place, `2..=frames + 1`
    /// edits or appends. Geometry is inherited.
    Frame { number: NonZeroU32 },
}

/// Image-family messages ([`crate::MessageKind::Image`], kind 3).
///
/// Transferred daemon-to-client as [`ImageMsg::Header`], sequential
/// [`ImageMsg::Chunk`]s, and [`ImageMsg::Complete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMsg {
    /// Opens a transfer. The byte count that must arrive before the
    /// matching `Complete` is the target image's
    /// `width × height × bytes_per_pixel`.
    Header { id: ImageId, target: ImageTarget },
    /// Pixel bytes appended to the open transfer, in order.
    Chunk {
        /// Names the open transfer's image; a mismatch is malformed.
        id: ImageId,
        /// Empty `bytes` is allowed (a keepalive). Refcounted: a view
        /// of the image store's buffer on the daemon side and of the
        /// frame body on the client side, so a queued chunk pins its
        /// backing allocation until written.
        bytes: Bytes,
    },
    /// The open transfer delivered every byte it announced.
    Complete { id: ImageId },
    /// Drop an image and any placements that reference it (`a=d`, and
    /// session destruction).
    Delete { id: ImageId },
    /// New / updated placement record.
    Placement {
        image_id: ImageId,
        /// `None` is the image's default placement; at most one per
        /// image.
        placement_id: Option<PlacementId>,
        /// 1-based anchor row. Rows ≤ 0 anchor in the scrollback (row 0
        /// is the youngest scrollback line), so a placement keeps
        /// tracking its text after it scrolls off the top
        /// (`docs/reference/protocols/kitty-graphics.md`
        /// "Scrollback-anchored placements").
        anchor_row: i32,
        /// 1-based anchor column.
        anchor_col: u16,
        /// `c=`.
        cols: u16,
        /// `r=`.
        rows: u16,
        source: Option<SourceRect>,
        /// `z=`.
        z_index: i32,
    },
    /// Remove one placement (Kitty `a=d, d=p`).
    PlacementRemoved {
        image_id: ImageId,
        /// `None` matches the default placement.
        placement_id: Option<PlacementId>,
    },
    /// Kitty Unicode-placeholder virtual placement (`U=1`).
    ///
    /// Transmits cell extents for tile sizing without grid anchor positions.
    /// Re-sent idempotently on repeated `U=1` transmissions for the same id.
    VirtualPlacement {
        image_id: ImageId,
        /// `c=`.
        cols: u16,
        /// `r=`.
        rows: u16,
        /// `z=`.
        z_index: i32,
    },
    /// Display a frame of image `id` now: emitted by the daemon's
    /// animation timer on every advance and on reattach.
    ShowFrame {
        id: ImageId,
        /// The same 1-based numbering [`ImageTarget::Frame`] uses.
        number: NonZeroU32,
    },
    /// Notification that placement anchors shifted upward into scrollback.
    ///
    /// Placements scrolled past retention receive explicit removal frames;
    /// the client never evicts placements autonomously.
    PlacementsShifted { lines: u32 },
}

const fn arm(name: &'static str) -> ArmMeta {
    ArmMeta::new(
        name,
        Direction::ToClient,
        CorrelationClass::Uncorrelated,
        ModeSet::ATTACHERS,
    )
}

impl Directed for ImageMsg {
    const ARMS: &'static [ArmMeta] = &[
        arm("Image::Header"),
        arm("Image::Chunk"),
        arm("Image::Complete"),
        arm("Image::Delete"),
        arm("Image::Placement"),
        arm("Image::PlacementRemoved"),
        arm("Image::VirtualPlacement"),
        arm("Image::ShowFrame"),
        arm("Image::PlacementsShifted"),
    ];

    fn arm_index(&self) -> usize {
        match self {
            Self::Header { .. } => 0,
            Self::Chunk { .. } => 1,
            Self::Complete { .. } => 2,
            Self::Delete { .. } => 3,
            Self::Placement { .. } => 4,
            Self::PlacementRemoved { .. } => 5,
            Self::VirtualPlacement { .. } => 6,
            Self::ShowFrame { .. } => 7,
            Self::PlacementsShifted { .. } => 8,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::codec::{decode, encode};
    use crate::messages::test_support::{assert_covers_every_arm, roundtrip};

    #[test]
    fn bytes_per_pixel_matches_the_kitty_f_field() {
        assert_eq!(ImageFormat::Rgb24.bytes_per_pixel(), 3);
        assert_eq!(ImageFormat::Rgba32.bytes_per_pixel(), 4);
    }

    fn image_cases() -> Vec<ImageMsg> {
        vec![
            ImageMsg::Header {
                id: ImageId(7),
                target: ImageTarget::New {
                    width: 256,
                    height: 128,
                    format: ImageFormat::Rgba32,
                },
            },
            ImageMsg::Chunk {
                id: ImageId(7),
                bytes: vec![0xAA; 64].into(),
            },
            ImageMsg::Chunk {
                id: ImageId(7),
                bytes: Bytes::new(),
            },
            ImageMsg::Complete { id: ImageId(7) },
            ImageMsg::Delete { id: ImageId(7) },
            ImageMsg::Placement {
                image_id: ImageId(7),
                placement_id: Some(PlacementId(3)),
                anchor_row: 1,
                anchor_col: 1,
                cols: 10,
                rows: 5,
                source: Some(SourceRect {
                    x: 0,
                    y: 0,
                    width: 256,
                    height: 128,
                }),
                z_index: -1,
            },
            ImageMsg::Placement {
                image_id: ImageId(7),
                placement_id: None,
                anchor_row: 24,
                anchor_col: 80,
                cols: 0,
                rows: 0,
                source: None,
                z_index: 0,
            },
            ImageMsg::PlacementRemoved {
                image_id: ImageId(7),
                placement_id: Some(PlacementId(3)),
            },
            ImageMsg::PlacementRemoved {
                image_id: ImageId(7),
                placement_id: None,
            },
            ImageMsg::VirtualPlacement {
                image_id: ImageId(7),
                cols: 40,
                rows: 12,
                z_index: -1,
            },
            ImageMsg::Header {
                id: ImageId(7),
                target: ImageTarget::Frame {
                    number: NonZeroU32::new(2).unwrap(),
                },
            },
            ImageMsg::Chunk {
                id: ImageId(7),
                bytes: vec![0x11, 0x22, 0x33, 0x44].into(),
            },
            ImageMsg::Complete { id: ImageId(7) },
            ImageMsg::ShowFrame {
                id: ImageId(7),
                number: NonZeroU32::new(2).unwrap(),
            },
            ImageMsg::ShowFrame {
                id: ImageId(7),
                number: NonZeroU32::new(1).unwrap(),
            },
            ImageMsg::PlacementsShifted { lines: 3 },
            ImageMsg::Placement {
                image_id: ImageId(7),
                placement_id: None,
                anchor_row: -42,
                anchor_col: 1,
                cols: 10,
                rows: 5,
                source: None,
                z_index: 0,
            },
        ]
    }

    #[test]
    fn image_messages_round_trip() {
        for msg in image_cases() {
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn image_cases_cover_every_variant() {
        assert_covers_every_arm(&image_cases());
    }

    #[test]
    fn max_chunk_payload_encodes_within_frame_ceiling() {
        let chunk = ImageMsg::Chunk {
            id: ImageId(u32::MAX),
            bytes: vec![0xAB; MAX_IMAGE_CHUNK_PAYLOAD].into(),
        };
        let bytes = encode(&chunk);
        assert!(
            (bytes.len() as u64) <= u64::from(crate::frame::DEFAULT_MAX_BODY),
            "MAX_IMAGE_CHUNK_PAYLOAD-sized chunk encoded to {} bytes, \
             above DEFAULT_MAX_BODY = {}",
            bytes.len(),
            crate::frame::DEFAULT_MAX_BODY,
        );
        assert!(
            bytes.len() + (1 << 20) <= crate::frame::DEFAULT_MAX_BODY as usize,
            "headroom under the frame ceiling shrank to {} bytes",
            crate::frame::DEFAULT_MAX_BODY as usize - bytes.len(),
        );
    }

    #[test]
    fn near_ceiling_image_burst_round_trips() {
        let original = ImageMsg::Chunk {
            id: ImageId(0xDEAD_BEEF),
            // Pseudo-random so a corrupt write surfaces as a mismatch.
            bytes: (0..MAX_IMAGE_CHUNK_PAYLOAD)
                .map(|i| (i as u32).wrapping_mul(2_654_435_761) as u8)
                .collect(),
        };
        let bytes = encode(&original);
        let back: ImageMsg = decode(&bytes).unwrap();
        assert_eq!(back, original);
    }
}
