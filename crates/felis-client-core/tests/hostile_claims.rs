//! Verifies decoder heap bounds against oversized scalar claims.
//!
//! Decodes frames claiming excessive grid dims, pixels, or frames; asserts peak
//! heap allocation stays flat as claims are refused (`docs/reference/ipc.md`
//! "Semantic limits").

#![allow(clippy::unwrap_used)]

use felis_client_core::ImageShadow;
use felis_protocol::codec::{CodecError, decode};
use felis_protocol::convert::WireError;
use felis_protocol::messages::{GridMsg, ImageMsg};
use felis_protocol::wire::v1;
use prost::Message as _;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Bytes the whole run may hold at its peak. Orders of magnitude below
/// the smallest claim under test, so the margin says nothing about
/// tuning and everything about whether a claim was honored.
const PEAK_CEILING: usize = 1 << 20;

fn grid_size(rows: u32, cols: u32) -> Vec<u8> {
    v1::GridMsg {
        msg: Some(v1::grid_msg::Msg::Size(v1::GridSize {
            dims: Some(v1::GridDims {
                rows,
                cols,
                pixel_w: 0,
                pixel_h: 0,
            }),
        })),
    }
    .encode_to_vec()
}

fn image_header(width: u32, height: u32) -> Vec<u8> {
    header(v1::image_header::Target::Image(v1::ImageNew {
        width,
        height,
        format: v1::ImageFormat::Rgba32 as i32,
    }))
}

fn frame_header(number: u32) -> Vec<u8> {
    header(v1::image_header::Target::Frame(v1::ImageFrame { number }))
}

fn header(target: v1::image_header::Target) -> Vec<u8> {
    v1::ImageMsg {
        msg: Some(v1::image_msg::Msg::Header(v1::ImageHeader {
            id: 1,
            target: Some(target),
        })),
    }
    .encode_to_vec()
}

#[test]
fn maximal_scalars_are_refused_without_allocating_what_they_claim() {
    let _profiler = dhat::Profiler::builder().testing().build();

    let mut images = ImageShadow::new();

    // Announced geometry: ~4.3 billion cells asked for by 20 bytes.
    for body in [
        grid_size(u32::MAX, u32::MAX),
        grid_size(65_535, 65_535),
        grid_size(2049, 80),
    ] {
        assert!(body.len() < 64, "the frame under test must stay tiny");
        let err = decode::<GridMsg>(&body[..]).unwrap_err();
        assert!(
            matches!(err, CodecError::Wire(WireError::OutOfRange { .. })),
            "announced geometry admitted: {err:?}",
        );
    }

    // Image claims: a geometry no store could hold, and frame numbers
    // an image could never have that many of.
    for body in [
        image_header(u32::MAX, u32::MAX),
        image_header(65_536, 65_536),
        frame_header(u32::MAX),
    ] {
        assert!(body.len() < 64, "the frame under test must stay tiny");
        let err = decode::<ImageMsg>(&body[..]).unwrap_err();
        assert!(
            matches!(err, CodecError::Wire(_)),
            "image claim admitted: {err:?}",
        );
    }

    // Selective, not blanket: an honest header of the same shape lands.
    let honest = decode::<ImageMsg>(&image_header(4, 4)[..]).unwrap();
    images.apply(&honest).unwrap();
    // The mirror charges the daemon's unit (pixels plus entry and frame
    // records carrying them), so 64 pixel bytes cost slightly more than 64.
    assert!(
        (64..1024).contains(&images.retained_bytes()),
        "an honest header must land and be billed: {}",
        images.retained_bytes(),
    );

    let stats = dhat::HeapStats::get();
    assert!(
        stats.max_bytes < PEAK_CEILING,
        "refusing the claims peaked at {} bytes; a claim was honored \
         before it was judged",
        stats.max_bytes,
    );
}
