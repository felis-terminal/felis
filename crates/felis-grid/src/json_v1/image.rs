//! v1 DTO for [`ImageMsg`], the daemon→client Kitty graphics stream.

use std::num::NonZeroU32;

use felis_protocol::kitty_graphics::{ImageId, PlacementId};
use felis_protocol::messages::{ImageFormat, ImageMsg, ImageTarget, SourceRect};

use super::JsonError;
use super::grid::plain_enum;

json_dto! {
    /// Decoded pixel format (Kitty `f=`).
    #[serde(rename_all = "snake_case")]
    pub enum ImageFormatJson {
        Rgb24,
        Rgba32,
    }

    /// Sub-region of an image, in image pixels.
    pub struct SourceRectJson {
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
    }

    /// What an [`ImageJson::Header`] opens a transfer for.
    #[serde(tag = "target", rename_all = "snake_case")]
    pub enum ImageTargetJson {
        New {
            width: u32,
            height: u32,
            format: ImageFormatJson,
        },
        /// Kitty's 1-based frame numbering; `0` is not a frame.
        Frame {
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            number: u32,
        },
    }

    /// Image-family frames in their v1 form.
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ImageJson {
        Header {
            id: u32,
            target: ImageTargetJson,
        },
        Chunk {
            id: u32,
            bytes: Vec<u8>,
        },
        Complete {
            id: u32,
        },
        Delete {
            id: u32,
        },
        Placement {
            image_id: u32,
            /// `null` is the image's default placement.
            placement_id: Option<u32>,
            /// 1-based; `<= 0` anchors in the scrollback.
            anchor_row: i32,
            /// 1-based.
            anchor_col: u16,
            cols: u16,
            rows: u16,
            source: Option<SourceRectJson>,
            z_index: i32,
        },
        PlacementRemoved {
            image_id: u32,
            placement_id: Option<u32>,
        },
        VirtualPlacement {
            image_id: u32,
            cols: u16,
            rows: u16,
            z_index: i32,
        },
        ShowFrame {
            id: u32,
            #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
            number: u32,
        },
        PlacementsShifted {
            lines: u32,
        },
    }
}

plain_enum!(ImageFormatJson, ImageFormat, Rgb24, Rgba32);

impl From<SourceRect> for SourceRectJson {
    fn from(rect: SourceRect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

impl From<SourceRectJson> for SourceRect {
    fn from(rect: SourceRectJson) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

impl From<ImageTarget> for ImageTargetJson {
    fn from(target: ImageTarget) -> Self {
        match target {
            ImageTarget::New {
                width,
                height,
                format,
            } => Self::New {
                width,
                height,
                format: format.into(),
            },
            ImageTarget::Frame { number } => Self::Frame {
                number: number.get(),
            },
        }
    }
}

impl TryFrom<ImageTargetJson> for ImageTarget {
    type Error = JsonError;

    fn try_from(target: ImageTargetJson) -> Result<Self, Self::Error> {
        Ok(match target {
            ImageTargetJson::New {
                width,
                height,
                format,
            } => Self::New {
                width,
                height,
                format: format.into(),
            },
            ImageTargetJson::Frame { number } => Self::Frame {
                number: frame_number(number)?,
            },
        })
    }
}

fn frame_number(number: u32) -> Result<NonZeroU32, JsonError> {
    NonZeroU32::new(number)
        .ok_or_else(|| JsonError::field("number", "frame numbering is 1-based, so `0` is no frame"))
}

impl From<ImageMsg> for ImageJson {
    fn from(msg: ImageMsg) -> Self {
        match msg {
            ImageMsg::Header { id, target } => Self::Header {
                id: id.0,
                target: target.into(),
            },
            ImageMsg::Chunk { id, bytes } => Self::Chunk {
                id: id.0,
                bytes: bytes.to_vec(),
            },
            ImageMsg::Complete { id } => Self::Complete { id: id.0 },
            ImageMsg::Delete { id } => Self::Delete { id: id.0 },
            ImageMsg::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
            } => Self::Placement {
                image_id: image_id.0,
                placement_id: placement_id.map(|id| id.0),
                anchor_row,
                anchor_col,
                cols,
                rows,
                source: source.map(Into::into),
                z_index,
            },
            ImageMsg::PlacementRemoved {
                image_id,
                placement_id,
            } => Self::PlacementRemoved {
                image_id: image_id.0,
                placement_id: placement_id.map(|id| id.0),
            },
            ImageMsg::VirtualPlacement {
                image_id,
                cols,
                rows,
                z_index,
            } => Self::VirtualPlacement {
                image_id: image_id.0,
                cols,
                rows,
                z_index,
            },
            ImageMsg::ShowFrame { id, number } => Self::ShowFrame {
                id: id.0,
                number: number.get(),
            },
            ImageMsg::PlacementsShifted { lines } => Self::PlacementsShifted { lines },
        }
    }
}

impl TryFrom<ImageJson> for ImageMsg {
    type Error = JsonError;

    fn try_from(msg: ImageJson) -> Result<Self, Self::Error> {
        Ok(match msg {
            ImageJson::Header { id, target } => Self::Header {
                id: ImageId(id),
                target: target.try_into()?,
            },
            ImageJson::Chunk { id, bytes } => Self::Chunk {
                id: ImageId(id),
                bytes: bytes.into(),
            },
            ImageJson::Complete { id } => Self::Complete { id: ImageId(id) },
            ImageJson::Delete { id } => Self::Delete { id: ImageId(id) },
            ImageJson::Placement {
                image_id,
                placement_id,
                anchor_row,
                anchor_col,
                cols,
                rows,
                source,
                z_index,
            } => Self::Placement {
                image_id: ImageId(image_id),
                placement_id: placement_id.map(PlacementId),
                anchor_row,
                anchor_col,
                cols,
                rows,
                source: source.map(Into::into),
                z_index,
            },
            ImageJson::PlacementRemoved {
                image_id,
                placement_id,
            } => Self::PlacementRemoved {
                image_id: ImageId(image_id),
                placement_id: placement_id.map(PlacementId),
            },
            ImageJson::VirtualPlacement {
                image_id,
                cols,
                rows,
                z_index,
            } => Self::VirtualPlacement {
                image_id: ImageId(image_id),
                cols,
                rows,
                z_index,
            },
            ImageJson::ShowFrame { id, number } => Self::ShowFrame {
                id: ImageId(id),
                number: frame_number(number)?,
            },
            ImageJson::PlacementsShifted { lines } => Self::PlacementsShifted { lines },
        })
    }
}
