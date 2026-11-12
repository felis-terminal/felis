use felis_protocol::messages::SourceRect;

use felis_vt::Parser;

use proptest::prelude::*;

use super::*;

mod animation;
mod commands;
mod decode;
// Windows declines file transmission with ENOTSUP.
#[cfg(unix)]
mod file_media;
mod placements;
mod queries;
mod reassembly;
mod session_dispatch;
mod shm;
mod sizing;
mod store_cap;

fn body(controls: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(controls.len() + 1 + payload.len());
    out.extend_from_slice(controls.as_bytes());
    out.push(b';');
    out.extend_from_slice(payload);
    out
}

fn assembled(d: BodyDispatch) -> Option<CompleteCommand> {
    match d {
        BodyDispatch::Assembled(complete) => Some(complete),
        BodyDispatch::Reply(_) | BodyDispatch::Silent => None,
    }
}

fn complete(controls: &[(u8, &[u8])], payload: &[u8]) -> CompleteCommand {
    CompleteCommand {
        controls: controls.iter().map(|(k, v)| (*k, v.to_vec())).collect(),
        payload: felis_protocol::base64::decode(payload),
    }
}

fn tiny_png() -> Vec<u8> {
    encode_png(
        2,
        1,
        png::ColorType::Rgba,
        &[0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x80],
    )
}

fn handler_state() -> (
    felis_grid::Grid,
    felis_grid::images::ImageStore,
    felis_grid::images::Placements,
) {
    (
        felis_grid::Grid::new(24, 80),
        felis_grid::images::ImageStore::new(crate::pool::DEFAULT_IMAGE_BYTE_CAP),
        felis_grid::images::Placements::new(),
    )
}

fn tiny_rgba_controls() -> ([(u8, &'static [u8]); 4], Vec<u8>) {
    (
        [(b'f', b"32"), (b's', b"1"), (b'v', b"1"), (b't', b"d")],
        b64(&[0xAA, 0xBB, 0xCC, 0xDD]),
    )
}

/// `keys` over a direct 1×1 RGBA transmission.
fn tiny_rgba_cmd(keys: &[(u8, &[u8])]) -> CompleteCommand {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = keys.to_vec();
    all.extend_from_slice(&ctrls);
    complete(&all, &payload)
}

/// `cmd` before any window reported its cell size, anchored at the live
/// cursor, its image events dropped.
fn run(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    cmd: CompleteCommand,
) -> Option<Vec<u8>> {
    handle_complete(
        &mut ApcCtx {
            grid,
            images,
            placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        cmd,
    )
}

use std::num::NonZeroU16;

use felis_grid::images::{CellPos, Extent, Placement, PlacementId};

fn seeded_placement(
    image: u32,
    placement: Option<u32>,
    row_1based: i32,
    col_1based: u16,
    rows: u16,
    cols: u16,
    z: i32,
    no_cursor_move: bool,
) -> Placement {
    Placement {
        image_id: ImageId(image),
        placement_id: placement.map(PlacementId),
        anchor: CellPos {
            row: row_1based,
            col: col_1based,
        },
        cols: Extent::Requested(NonZeroU16::new(cols).unwrap()),
        rows: Extent::Requested(NonZeroU16::new(rows).unwrap()),
        source: None,
        z_index: z,
        no_cursor_move,
        quiet: 0,
        anonymous: false,
        serial: 0,
    }
}

#[cfg(unix)]
fn tempfile_with_bytes(bytes: &[u8]) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(bytes).expect("write");
    f.flush().expect("flush");
    f
}

/// A temp file kitty's `t=t` deletion rule admits: under the temp
/// directory, with `tty-graphics-protocol` in its name.
#[cfg(unix)]
fn graphics_tempfile_with_bytes(bytes: &[u8]) -> std::path::PathBuf {
    use std::io::Write;
    let mut f = tempfile::Builder::new()
        .prefix("tty-graphics-protocol-")
        .tempfile()
        .expect("tempfile");
    f.write_all(bytes).expect("write");
    f.flush().expect("flush");
    f.into_temp_path().keep().expect("keep")
}

fn dispatch_controls(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    controls: &[(u8, &[u8])],
    payload: &[u8],
) -> Option<Vec<u8>> {
    handle_complete(
        &mut ApcCtx {
            grid,
            images,
            placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 8,
            cell_pixel_h: 16,
            anchor_cursor: Some((0, 0)),
        },
        complete(controls, payload),
    )
}
