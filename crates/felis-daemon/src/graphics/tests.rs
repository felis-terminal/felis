use felis_vt::Parser;
use proptest::prelude::*;

use super::*;

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

#[test]
fn single_chunk_command_assembles_in_one_dispatch() {
    let mut r = Reassembler::new();
    let complete = assembled(reassemble_apc_body(&mut r, &body("Gi=1,a=q", b"")))
        .expect("single-chunk command must assemble");
    assert_eq!(complete.payload, b"");
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'i' && v == b"1")
    );
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'a' && v == b"q")
    );
}

#[test]
fn chunked_transmission_assembles_across_three_bodies() {
    let mut r = Reassembler::new();
    assert_eq!(
        reassemble_apc_body(&mut r, &body("Gi=7,a=t,m=1", b"head-")),
        BodyDispatch::Silent,
        "head chunk is pending",
    );
    assert_eq!(
        reassemble_apc_body(&mut r, &body("Gm=1", b"middle-")),
        BodyDispatch::Silent,
        "middle chunk is pending",
    );
    let complete = assembled(reassemble_apc_body(&mut r, &body("Gm=0", b"tail")))
        .expect("terminal chunk assembles");
    assert_eq!(complete.payload, b"head-middle-tail");
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'i' && v == b"7")
    );
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'a' && v == b"t")
    );
    assert!(!complete.controls.iter().any(|(k, _)| *k == b'm'));
}

#[test]
fn foreign_apc_dialect_stays_silent_without_disturbing_reassembler() {
    // Pins: a non-`G` APC is another dialect, so no reply (a reply would
    // inject kitty bytes into a protocol we do not speak).
    let mut r = Reassembler::new();
    assert_eq!(
        reassemble_apc_body(&mut r, b"not-a-kitty-graphics-body"),
        BodyDispatch::Silent
    );
    let complete = assembled(reassemble_apc_body(&mut r, &body("Ga=q", b"")))
        .expect("valid command after a foreign body must assemble");
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'a' && v == b"q")
    );
}

#[test]
fn malformed_g_envelope_replies_einval() {
    // Pins: unparseable controls reply id-less EINVAL (no `q=` was read,
    // so none can be honored).
    let mut r = Reassembler::new();
    match reassemble_apc_body(&mut r, b"Gbadcontrols") {
        BodyDispatch::Reply(reply) => {
            assert_eq!(
                reply,
                b"\x1b_G;EINVAL:malformed graphics escape\x1b\\".to_vec()
            );
        }
        other => panic!("expected an EINVAL reply, got {other:?}"),
    }
    assert!(assembled(reassemble_apc_body(&mut r, &body("Ga=q", b""))).is_some());
}

#[test]
fn reassembly_overflow_replies_einval_with_the_head_ids() {
    // Pins: the overflow error echoes the head chunk's `i=` (the
    // continuation carries no controls of its own).
    let mut r = Reassembler::new();
    let near_cap = vec![b'x'; kitty_graphics::REASSEMBLY_BUFFER_LIMIT - 4];
    let mut head = b"Gi=9,a=t,m=1;".to_vec();
    head.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head), BodyDispatch::Silent);
    match reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])) {
        BodyDispatch::Reply(reply) => {
            let text = String::from_utf8_lossy(&reply).into_owned();
            assert!(text.starts_with("\x1b_Gi=9;EINVAL:"), "reply was {text:?}");
            assert!(
                text.contains("reassembly buffer limit"),
                "reply was {text:?}"
            );
        }
        other => panic!("expected an EINVAL reply, got {other:?}"),
    }
    assert!(assembled(reassemble_apc_body(&mut r, &body("Ga=q", b""))).is_some());
}

#[test]
fn reassembly_overflow_reply_respects_q_gating() {
    let near_cap = vec![b'x'; kitty_graphics::REASSEMBLY_BUFFER_LIMIT - 4];

    let mut r = Reassembler::new();
    let mut head_q2 = b"Gi=9,a=t,q=2,m=1;".to_vec();
    head_q2.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head_q2), BodyDispatch::Silent);
    assert_eq!(
        reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])),
        BodyDispatch::Silent,
        "q=2 must suppress the overflow reply",
    );

    let mut r = Reassembler::new();
    let mut head_q1 = b"Gi=9,a=t,q=1,m=1;".to_vec();
    head_q1.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head_q1), BodyDispatch::Silent);
    assert!(
        matches!(
            reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])),
            BodyDispatch::Reply(_)
        ),
        "q=1 suppresses OK only; the overflow error must still reply",
    );
}

#[test]
fn end_to_end_parser_feeds_apc_through_grid_relay_into_dispatcher() {
    let mut p = Parser::new();
    let mut g = felis_grid::Grid::new(1, 4);
    p.advance(&mut g, b"\x1b_Gi=42,a=t,f=24;rgb-payload\x1b\\");
    let bodies: Vec<_> = g
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            felis_grid::PtyEffect::Apc(body) => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 1, "one APC body reached the grid");
    let mut r = Reassembler::new();
    let complete = assembled(reassemble_apc_body(&mut r, &bodies[0].body))
        .expect("daemon assembles the relayed body");
    assert_eq!(complete.payload, b"rgb-payload");
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'i' && v == b"42")
    );
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'f' && v == b"24")
    );
}

fn complete(controls: &[(u8, &[u8])], payload: &[u8]) -> CompleteCommand {
    CompleteCommand {
        controls: controls.iter().map(|(k, v)| (*k, v.to_vec())).collect(),
        payload: payload.to_vec(),
    }
}

fn b64(input: &[u8]) -> Vec<u8> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize]);
        out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        if chunk.len() == 1 {
            out.push(b'=');
            out.push(b'=');
        } else {
            out.push(ALPHA[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]);
            if chunk.len() == 2 {
                out.push(b'=');
            } else {
                out.push(ALPHA[(b2 & 0x3F) as usize]);
            }
        }
    }
    out
}

proptest! {
    /// A raw transmission carries its shape in `s=`/`v=`, so the entry
    /// is `s x v x bpp` bytes of the payload's head. The payload may run
    /// long (mpv's `--vo-kitty-use-shm` sends no `S=` and macOS rounds
    /// the segment up to a page) and kitty ignores the excess rather
    /// than rejecting the frame; one byte short is still an error.
    #[test]
    fn a_raw_transmission_decodes_its_declared_shape(
        width in 1u32..6,
        height in 1u32..6,
        rgba in any::<bool>(),
        extra in 0usize..40,
    ) {
        let (f, format, bpp) = if rgba {
            (&b"32"[..], ImageFormat::Rgba32, 4)
        } else {
            (&b"24"[..], ImageFormat::Rgb24, 3)
        };
        let expected = (width as usize) * (height as usize) * bpp;
        let pixels: Vec<u8> = (0..expected + extra).map(|i| (i % 251) as u8).collect();
        let width_s = width.to_string();
        let height_s = height.to_string();
        let controls = [
            (b'f', f),
            (b's', width_s.as_bytes()),
            (b'v', height_s.as_bytes()),
        ];

        let entry = decode_image(&complete(&controls, &b64(&pixels)))
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(entry.width, width);
        prop_assert_eq!(entry.height, height);
        prop_assert_eq!(entry.format, format);
        prop_assert_eq!(entry.pixels(), &pixels[..expected]);

        let short = decode_image(&complete(&controls, &b64(&pixels[..expected - 1])));
        prop_assert!(matches!(short, Err(DecodeError::InvalidValue(_))));
    }
}

#[test]
fn missing_format_defaults_to_rgba32() {
    // Pins the Kitty spec's documented default `f=32`.
    let pixels = [0xAA, 0xBB, 0xCC, 0xDD];
    let cmd = complete(&[(b's', b"1"), (b'v', b"1")], &b64(&pixels));
    let entry = decode_image(&cmd).unwrap();
    assert_eq!(entry.format, ImageFormat::Rgba32);
    assert_eq!(entry.pixels(), pixels);
}

#[test]
fn raw_rejects_missing_dimensions() {
    let cmd = complete(&[(b'f', b"32"), (b'v', b"1")], &b64(&[0; 4]));
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

#[test]
fn raw_rejects_dimensions_over_per_image_cap() {
    // Pins: rejected before allocation, not after.
    let cmd = complete(
        &[(b'f', b"32"), (b's', b"8000"), (b'v', b"8000")],
        &b64(b""),
    );
    assert!(matches!(decode_image(&cmd), Err(DecodeError::OverBudget)));
}

#[test]
fn raw_rejects_when_s_times_v_overflows_usize() {
    let cmd = complete(
        &[(b'f', b"32"), (b's', b"4000000000"), (b'v', b"4000000000")],
        &b64(b""),
    );
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_) | DecodeError::OverBudget)
    ));
}

#[test]
fn bad_base64_payload_returns_bad_image() {
    let cmd = complete(&[(b'f', b"32"), (b's', b"1"), (b'v', b"1")], b"!@#$");
    assert!(matches!(decode_image(&cmd), Err(DecodeError::BadImage(_))));
}

#[test]
#[cfg(all(unix, not(target_os = "linux")))]
fn shared_memory_transmission_decodes_off_linux() {
    // Pins macOS shm decoding: macOS shm fds lack `S_IFREG` bits
    // in `fstat`, require slash restoration, and use page-rounded reads.
    let name = format!("/felis-ts-{}", std::process::id());
    let fd = rustix::shm::open(
        &name,
        rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create test shm");
    rustix::fs::ftruncate(&fd, 4).expect("size test shm");
    drop(fd);
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(name.trim_start_matches('/').as_bytes()),
    );
    let entry = decode_image(&cmd).expect("macOS t=s segment must decode");
    assert_eq!(entry.pixels(), [0, 0, 0, 0]);
    unlink_shm_segment(&name);
}

#[test]
#[cfg(windows)]
fn windows_declines_file_temp_and_shm_transmission_with_enotsup() {
    // ENOTSUP, not EIO: EIO is retryable and would tell a producer to
    // keep trying a path that can never work.
    for method in [b"f".as_slice(), b"t".as_slice(), b"s".as_slice()] {
        let cmd = complete(
            &[(b't', method), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
            &b64(br"C:\Users\test\felis-image.rgba"),
        );
        assert!(
            matches!(decode_image(&cmd), Err(DecodeError::Unsupported(_))),
            "t={} must decline with ENOTSUP, got: {:?}",
            char::from(method[0]),
            decode_image(&cmd),
        );
    }
}

#[test]
#[cfg(windows)]
fn windows_direct_transmission_still_decodes() {
    let (ctrls, payload) = tiny_rgba_controls();
    let cmd = complete(&ctrls, &payload);
    let entry = decode_image(&cmd).expect("t=d must decode on Windows");
    assert_eq!(entry.pixels(), [0xAA, 0xBB, 0xCC, 0xDD]);
}

#[test]
#[cfg(windows)]
fn windows_transmission_rejection_echoes_id_and_honors_quiet() {
    let (mut grid, mut images, mut placements) = handler_state();
    let controls: [(u8, &[u8]); 6] = [
        (b'a', b"t"),
        (b'i', b"41"),
        (b't', b"f"),
        (b'f', b"32"),
        (b's', b"1"),
        (b'v', b"1"),
    ];
    let path = b64(br"C:\Users\test\felis-image.rgba");
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&controls, &path),
    )
    .expect("the q=0 default replies to errors");
    assert!(
        response.starts_with(b"\x1b_Gi=41;ENOTSUP"),
        "got: {response:?}",
    );

    let mut quiet = controls.to_vec();
    quiet.push((b'q', b"2"));
    assert!(
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &complete(&quiet, &path),
        )
        .is_none(),
        "q=2 must suppress the rejection too",
    );
}

#[test]
fn malformed_transmission_method_returns_invalid_value() {
    // EINVAL, not ENOTSUP: producers may retry ENOTSUP.
    let cmd = complete(
        &[(b't', b"x"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        b"",
    );
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

#[test]
fn unknown_format_returns_invalid_value() {
    let cmd = complete(&[(b'f', b"64"), (b's', b"1"), (b'v', b"1")], &b64(&[0; 4]));
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

fn zlib_compress(input: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(input, 6)
}

#[test]
fn zlib_compressed_raw_payload_round_trips() {
    let pixels = vec![0x77u8; 16];
    let compressed = zlib_compress(&pixels);
    let cmd = complete(
        &[(b'f', b"32"), (b's', b"2"), (b'v', b"2"), (b'o', b"z")],
        &b64(&compressed),
    );
    let entry = decode_image(&cmd).expect("zlib + raw RGBA round-trips");
    assert_eq!(entry.pixels(), pixels);
}

#[test]
fn zlib_zip_bomb_is_rejected() {
    // Pins: the inflate stops at the per-image cap instead of growing.
    let huge = vec![0u8; MAX_DECODED_BYTES + 1024];
    let compressed = zlib_compress(&huge);
    let cmd = complete(
        &[(b'f', b"32"), (b's', b"1"), (b'v', b"1"), (b'o', b"z")],
        &b64(&compressed),
    );
    assert!(matches!(decode_image(&cmd), Err(DecodeError::OverBudget)));
}

#[test]
fn unknown_compression_returns_unsupported() {
    let cmd = complete(
        &[(b'f', b"32"), (b's', b"1"), (b'v', b"1"), (b'o', b"lz4")],
        &b64(b""),
    );
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::Unsupported(_))
    ));
}

fn encode_png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(data).expect("png data");
    writer.finish().expect("png finish");
    bytes
}

fn tiny_png() -> Vec<u8> {
    encode_png(
        2,
        1,
        png::ColorType::Rgba,
        &[0xFF, 0x00, 0x00, 0xFF, 0x00, 0xFF, 0x00, 0x80],
    )
}

#[test]
fn png_payload_decodes_to_rgba8() {
    let png = tiny_png();
    let cmd = complete(&[(b'f', b"100")], &b64(&png));
    let entry = decode_image(&cmd).expect("PNG round-trips");
    assert_eq!(entry.width, 2);
    assert_eq!(entry.height, 1);
    assert_eq!(entry.format, ImageFormat::Rgba32);
    assert_eq!(entry.pixels().len(), 2 * 4);
    // Byte order is RGBA, not BGRA or ARGB.
    assert_eq!(&entry.pixels()[..4], &[0xFF, 0x00, 0x00, 0xFF]);
}

#[test]
fn non_rgba_png_widens_to_rgba8() {
    let gray = encode_png(2, 1, png::ColorType::Grayscale, &[0x00, 0x7F]);
    let cmd = complete(&[(b'f', b"100")], &b64(&gray));
    let entry = decode_image(&cmd).expect("gray PNG decodes");
    assert_eq!(entry.format, ImageFormat::Rgba32);
    assert_eq!(
        entry.pixels(),
        &[0x00, 0x00, 0x00, 0xFF, 0x7F, 0x7F, 0x7F, 0xFF]
    );

    let rgb = encode_png(1, 1, png::ColorType::Rgb, &[0x01, 0x02, 0x03]);
    let cmd = complete(&[(b'f', b"100")], &b64(&rgb));
    let entry = decode_image(&cmd).expect("RGB PNG decodes");
    assert_eq!(entry.pixels(), &[0x01, 0x02, 0x03, 0xFF]);
}

#[test]
fn corrupt_png_returns_bad_image() {
    let mut png = tiny_png();
    for b in &mut png[..8] {
        *b = 0;
    }
    let cmd = complete(&[(b'f', b"100")], &b64(&png));
    assert!(matches!(decode_image(&cmd), Err(DecodeError::BadImage(_))));
}

#[test]
fn zlib_plus_png_round_trips() {
    let png = tiny_png();
    let compressed = zlib_compress(&png);
    let cmd = complete(&[(b'f', b"100"), (b'o', b"z")], &b64(&compressed));
    let entry = decode_image(&cmd).expect("zlib + PNG round-trips");
    assert_eq!(entry.width, 2);
    assert_eq!(entry.height, 1);
    assert_eq!(entry.format, ImageFormat::Rgba32);
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

#[test]
fn transmit_inserts_into_image_store_and_emits_ok() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'i', b"7".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .expect("q=0 default emits an OK response");
    assert_eq!(response, b"\x1b_Gi=7;OK\x1b\\");
    assert!(
        images.get(ImageId(7)).is_some(),
        "image must land in the store"
    );
    assert!(placements.is_empty(), "a=t records no placement");
}

#[test]
fn transmit_with_q1_suppresses_ok_response() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"t".as_slice()),
        (b'i', b"9".as_slice()),
        (b'q', b"1".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    assert!(
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &cmd,
        )
        .is_none(),
        "q=1 OK is suppressed"
    );
    assert!(
        images.get(ImageId(9)).is_some(),
        "store mutation runs regardless of q="
    );
}

#[test]
fn transmit_with_q1_still_emits_errors() {
    let (mut grid, mut images, mut placements) = handler_state();
    // A missing `i=` auto-allocates, so the reserved zero id triggers
    // the error.
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"t".as_slice()),
        (b'i', b"0".as_slice()),
        (b'q', b"1".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .expect("q=1 errors still emit");
    assert!(
        response.starts_with(b"\x1b_Gi=0;EINVAL"),
        "q=1 error must still surface, got: {response:?}",
    );
}

#[test]
fn transmit_with_q2_suppresses_every_response_including_errors() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'q', b"2".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    assert!(
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &cmd,
        )
        .is_none(),
        "q=2 silences errors too"
    );
}

#[test]
fn transmit_with_q3_or_higher_is_treated_as_silent() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"t".as_slice()),
        (b'i', b"3".as_slice()),
        (b'q', b"99".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    assert!(
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &cmd,
        )
        .is_none(),
        "q=99 silences",
    );
}

#[test]
fn transmit_with_missing_image_id_auto_allocates_an_anonymous_id() {
    // The Kitty spec allows an anonymous transmit (yazi's direct
    // preview path sends one); the response then carries no id.
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert_eq!(
        response, b"\x1b_G;OK\x1b\\",
        "anonymous transmit → id-less OK, got: {response:?}",
    );
    assert!(
        images.get(ImageId(u32::MAX)).is_some(),
        "anonymous image lands under a terminal-allocated id",
    );
}

#[test]
fn transmit_with_image_id_zero_rejected() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'i', b"0".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(response.starts_with(b"\x1b_Gi=0;EINVAL"));
}

#[test]
fn transmit_and_display_records_placement_at_cursor() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"42".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(images.get(ImageId(42)).is_some());
    let placement = placements
        .iter()
        .next()
        .expect("a=T must record a placement");
    assert_eq!(placement.image_id, ImageId(42));
    assert_eq!(placement.anchor, CellPos { row: 1, col: 1 });
    assert_eq!(
        images.get(ImageId(42)).unwrap().refcount(),
        1,
        "placement bumps the image's refcount so eviction can't drop it",
    );
}

#[test]
fn display_existing_without_image_returns_enoent() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = complete(&[(b'a', b"p"), (b'i', b"42")], b"");
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=42;ENOENT"),
        "missing image → ENOENT, got: {response:?}",
    );
    assert!(placements.is_empty(), "ENOENT must not record a placement");
}

#[test]
fn display_existing_with_known_image_records_placement() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'i', b"5".as_slice())];
    all.extend_from_slice(&ctrls);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&all, &payload),
    )
    .unwrap();
    let display = complete(&[(b'a', b"p"), (b'i', b"5"), (b'z', b"-1")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &display,
    )
    .unwrap();
    assert_eq!(placements.len(), 1);
    assert_eq!(placements.iter().next().unwrap().z_index, -1);
}

#[test]
fn delete_with_d_eq_a_clears_all_placements_but_keeps_images() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    for id in 1..=3u32 {
        let id_str = id.to_string();
        let mut all = vec![(b'a', b"T".as_slice()), (b'i', id_str.as_bytes())];
        all.extend_from_slice(&ctrls);
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &complete(&all, &payload),
        )
        .unwrap();
    }
    assert_eq!(placements.len(), 3);
    let cmd = complete(&[(b'a', b"d"), (b'd', b"a")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert_eq!(images.len(), 3, "images survive d=a");
    for id in 1..=3 {
        assert_eq!(images.get(ImageId(id)).unwrap().refcount(), 0);
    }
}

#[test]
fn delete_with_d_eq_capital_a_clears_everything() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"77".as_slice())];
    all.extend_from_slice(&ctrls);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&all, &payload),
    )
    .unwrap();
    let cmd = complete(&[(b'a', b"d"), (b'd', b"A")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert!(images.is_empty(), "d=A frees the image store too");
}

#[test]
fn delete_default_d_eq_i_removes_placement_keeps_image() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"12".as_slice())];
    all.extend_from_slice(&ctrls);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&all, &payload),
    )
    .unwrap();
    let cmd = complete(&[(b'a', b"d"), (b'i', b"12")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert!(images.get(ImageId(12)).is_some(), "image survives d=i");
}

use felis_grid::images::{CellPos, Placement, PlacementId};

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
        cols,
        rows,
        source: None,
        z_index: z,
        no_cursor_move,
        quiet: 0,
    }
}

fn delete_test_state(
    image_ids: &[u32],
) -> (
    felis_grid::Grid,
    felis_grid::images::ImageStore,
    felis_grid::images::Placements,
) {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    for id in image_ids {
        let id_str = id.to_string();
        let mut all = vec![(b'a', b"t".as_slice()), (b'i', id_str.as_bytes())];
        all.extend_from_slice(&ctrls);
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &complete(&all, &payload),
        )
        .unwrap();
    }
    (grid, images, placements)
}

#[test]
fn delete_d_eq_p_drops_placement_intersecting_the_xy_cell() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 5, 10, 2, 5, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, 0, false));
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"p"), (b'x', b"12"), (b'y', b"5")],
        b"",
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert_eq!(placements.len(), 1, "image 2 should survive the d=p");
    assert_eq!(placements.iter().next().unwrap().image_id, ImageId(2));
    assert!(
        images.get(ImageId(1)).is_some(),
        "lowercase d=p keeps image bytes",
    );
}

#[test]
fn delete_d_eq_capital_p_also_frees_the_image_store() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[7]);
    placements.upsert(seeded_placement(7, None, 5, 10, 2, 5, 0, false));
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"P"), (b'x', b"12"), (b'y', b"5")],
        b"",
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert!(placements.is_empty());
    assert!(images.get(ImageId(7)).is_none(), "d=P frees the image too");
}

#[test]
fn delete_d_eq_x_drops_placements_intersecting_the_column() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 1, 10, 1, 6, 0, false));
    placements.upsert(seeded_placement(2, None, 20, 12, 1, 3, 0, false));
    placements.upsert(seeded_placement(3, None, 5, 1, 1, 5, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"x"), (b'x', b"12")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3], "only image 3 (col 1..=5) survives x=12");
}

#[test]
fn delete_d_eq_y_drops_placements_intersecting_the_row() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 5, 1, 3, 2, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 70, 1, 1, 0, false));
    placements.upsert(seeded_placement(3, None, 10, 1, 1, 1, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"y"), (b'y', b"5")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3], "only image 3 (row 10) survives y=5");
}

#[test]
fn delete_d_eq_z_drops_placements_at_the_specified_z_index() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2, 3]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, -1, false));
    placements.upsert(seeded_placement(3, None, 1, 1, 1, 1, -1, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"z"), (b'z', b"-1")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![1], "z=-1 wipes the two -1 placements");
}

#[test]
fn delete_d_eq_q_requires_all_three_coords_to_match() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 5, 5, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 5, 1, 1, 3, false));
    let cmd = complete(
        &[
            (b'a', b"d"),
            (b'd', b"q"),
            (b'x', b"5"),
            (b'y', b"5"),
            (b'z', b"3"),
        ],
        b"",
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![1], "image 1 at z=0 survives the z=3 filter");
}

#[test]
fn delete_d_eq_c_uses_the_current_cursor_position() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 5, 5, 1, 1, 0, false));
    let cmd = complete(&[(b'a', b"d"), (b'd', b"c")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![2]);
}

/// For `d=r`, `x=`/`y=` are an image-id range, not cell coordinates
/// (the Kitty spec overloads the keys).
#[test]
fn delete_d_eq_r_drops_placements_in_the_image_id_range() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[3, 5, 7, 9]);
    for id in [3u32, 5, 7, 9] {
        placements.upsert(seeded_placement(id, None, 1, 1, 1, 1, 0, false));
    }
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"r"), (b'x', b"5"), (b'y', b"7")],
        b"",
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let kept: Vec<_> = placements.iter().map(|p| p.image_id.0).collect();
    assert_eq!(kept, vec![3, 9], "ids 5 and 7 inside [5..=7] are dropped");
    assert!(images.get(ImageId(5)).is_some());
}

#[test]
fn delete_d_eq_capital_r_also_frees_images_in_range() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[3, 5, 7]);
    for id in [3u32, 5, 7] {
        placements.upsert(seeded_placement(id, None, 1, 1, 1, 1, 0, false));
    }
    let cmd = complete(
        &[(b'a', b"d"), (b'd', b"R"), (b'x', b"5"), (b'y', b"7")],
        b"",
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    assert_eq!(placements.len(), 1);
    assert!(images.get(ImageId(5)).is_none());
    assert!(images.get(ImageId(7)).is_none());
    assert!(images.get(ImageId(3)).is_some(), "id 3 outside the range");
}

#[test]
fn delete_extended_modes_emit_placement_removed_per_evicted_entry() {
    let (mut grid, mut images, mut placements) = delete_test_state(&[1, 2]);
    placements.upsert(seeded_placement(1, None, 1, 1, 1, 1, 0, false));
    placements.upsert(seeded_placement(2, None, 1, 1, 1, 1, 0, false));
    let mut events = Vec::new();
    let cmd = complete(&[(b'a', b"d"), (b'd', b"c")], b"");
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .ok_or(())
    .expect_err("deletes are not acknowledged on success (kitty parity)");
    let removed_count = events
        .iter()
        .filter(|e| matches!(e, ImageEvent::PlacementRemoved { .. }))
        .count();
    assert_eq!(removed_count, 2);
}

#[test]
fn query_with_valid_payload_emits_ok() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"q".as_slice()), (b'i', b"31".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert_eq!(response, b"\x1b_Gi=31;OK\x1b\\");
    assert!(
        images.is_empty(),
        "a=q is a dry run; the store stays untouched",
    );
}

#[test]
fn query_with_invalid_payload_emits_specific_error() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = complete(
        &[(b'a', b"q"), (b'f', b"99"), (b's', b"1"), (b'v', b"1")],
        b"",
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_G;EINVAL"),
        "f=99 → EINVAL, got: {response:?}",
    );
}

#[test]
fn unknown_action_returns_einval() {
    let (mut grid, mut images, mut placements) = handler_state();
    let cmd = complete(&[(b'a', b"X"), (b'i', b"1")], b"");
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(response.starts_with(b"\x1b_Gi=1;EINVAL"));
}

fn transmit_root(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    id: &[u8],
) {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![(b'a', b"t".as_slice()), (b'i', id)];
    all.extend_from_slice(&ctrls);
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
        &complete(&all, &payload),
    );
}

fn push_frame_cmd(gap: &'static [u8], pixel: [u8; 4]) -> (Vec<(u8, &'static [u8])>, Vec<u8>) {
    let ctrls: Vec<(u8, &[u8])> = vec![
        (b'a', b"f"),
        (b'i', b"1"),
        (b'f', b"32"),
        (b's', b"1"),
        (b'v', b"1"),
        (b't', b"d"),
        (b'z', gap),
    ];
    (ctrls, b64(&pixel))
}

#[test]
fn animation_frame_appends_and_emits_frame_transmission() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
    let resp = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );
    let entry = images.get(ImageId(1)).unwrap();
    assert_eq!(entry.frame_count(), 2, "a=f appends a frame");
    assert_eq!(entry.frame(1).unwrap().pixels, vec![0x11, 0x22, 0x33, 0x44]);
    assert_eq!(entry.frame(1).unwrap().gap_ms, 100, "z=100 → 100ms gap");
    let wire = materialize_image_events(&events, &images);
    assert!(
        wire.iter().any(|e| matches!(
            e,
            ImageMsg::Header {
                target: ImageTarget::Frame { number },
                ..
            } if number.get() == 2
        )),
        "the new frame's pixels ship to attached clients, numbered from 1",
    );
    assert!(
        wire.iter()
            .any(|e| matches!(e, ImageMsg::Complete { id: ImageId(1) }))
    );
    assert_eq!(resp, Some(b"\x1b_Gi=1;OK\x1b\\".to_vec()));
}

/// The producer-facing half of the frame cap: `a=f` past 4096 answers
/// `ENOTSUP` (kitty's status set has no `ENOSPC`) and leaves the
/// animation exactly as it was, rather than evicting a frame the
/// producer would never learn it had lost.
#[test]
fn an_animation_frame_past_the_cap_is_declined_and_changes_nothing() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    // Filled through the store: the dispatcher's answer is what is
    // under test, not 4095 round trips through the parser.
    for _ in 1..felis_protocol::messages::MAX_IMAGE_FRAMES {
        images
            .push_frame(
                ImageId(1),
                Frame {
                    pixels: vec![0u8; 4].into(),
                    gap_ms: 40,
                },
            )
            .unwrap();
    }

    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
    let resp = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );

    let resp = resp.unwrap();
    assert!(
        resp.starts_with(b"\x1b_Gi=1;ENOTSUP"),
        "got {}",
        String::from_utf8_lossy(&resp),
    );
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame_count(),
        felis_protocol::messages::MAX_IMAGE_FRAMES,
        "a declined frame must not have grown the animation",
    );
    assert!(
        events.is_empty(),
        "nothing is announced for a frame that was not stored, got {events:?}",
    );
}

/// A frame push makes room by evicting other refcount-0 images. The
/// client mirrors the store's byte total against its own session cap,
/// so an eviction it never hears about would leave it counting bytes
/// the daemon has already freed and refusing the next honest header.
#[test]
fn a_frame_push_that_evicts_announces_the_eviction() {
    let mut grid = felis_grid::Grid::new(24, 80);
    let charge = ImageEntry::new(1, 1, ImageFormat::Rgba32, vec![0u8; 4]).byte_len();
    let mut images = felis_grid::images::ImageStore::new(2 * charge);
    let mut placements = felis_grid::images::Placements::new();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    transmit_root(&mut grid, &mut images, &mut placements, b"2");

    let (ctrls, payload) = push_frame_cmd(b"100", [0x11, 0x22, 0x33, 0x44]);
    let mut events = Vec::new();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );

    assert!(
        images.get(ImageId(2)).is_none(),
        "the frame must have cost image 2 its place",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImageEvent::Delete { id } if *id == ImageId(2))),
        "the eviction must reach attached clients, got {events:?}",
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame_count(), 2);
}

#[test]
fn animation_frame_default_gap_is_40ms_and_negative_is_gapless() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (mut ctrls, payload) = push_frame_cmd(b"0", [1, 2, 3, 4]);
    ctrls.retain(|(k, _)| *k != b'z');
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame(1).unwrap().gap_ms, 40);
    let (ctrls, payload) = push_frame_cmd(b"-5", [5, 6, 7, 8]);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame(2).unwrap().gap_ms, 0);
}

#[test]
fn animation_control_sets_state_loops_and_jumps() {
    use felis_grid::images::AnimationMode;
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    for px in [[1, 1, 1, 1], [2, 2, 2, 2]] {
        let (ctrls, payload) = push_frame_cmd(b"50", px);
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut Vec::new(),
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &complete(&ctrls, &payload),
        );
    }
    let a: &[(u8, &[u8])] = &[
        (b'a', b"a"),
        (b'i', b"1"),
        (b's', b"3"),
        (b'v', b"3"),
        (b'c', b"2"),
    ];
    let mut events = Vec::new();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(a, b""),
    );
    let entry = images.get(ImageId(1)).unwrap();
    assert_eq!(entry.mode(), AnimationMode::Running);
    assert_eq!(entry.current_frame(), 1, "c=2 jumps to 0-based index 1");
    assert!(
        events.iter().any(|e| matches!(
            e,
            ImageEvent::ShowFrame {
                id: ImageId(1),
                index: 1
            }
        )),
        "the jump ships a ShowFrame",
    );
}

#[test]
fn delete_frame_removes_it_and_reships_the_image() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"50", [9, 9, 9, 9]);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );
    assert_eq!(images.get(ImageId(1)).unwrap().frame_count(), 2);
    let d: &[(u8, &[u8])] = &[(b'a', b"d"), (b'd', b"f"), (b'i', b"1"), (b'r', b"2")];
    let mut events = Vec::new();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(d, b""),
    );
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame_count(),
        1,
        "frame removed"
    );
    // Frame removal has no per-frame wire message: the client gets a
    // full rebuild, so a fresh root Header is re-emitted.
    assert!(
        materialize_image_events(&events, &images)
            .iter()
            .any(|e| matches!(e, ImageMsg::Header { id: ImageId(1), .. })),
        "frame delete re-ships the image so the client rebuilds its frames",
    );
}

#[test]
fn animation_frame_on_missing_image_is_not_found() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (mut ctrls, payload) = push_frame_cmd(b"50", [1, 2, 3, 4]);
    for c in &mut ctrls {
        if c.0 == b'i' {
            c.1 = b"99";
        }
    }
    let resp = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    )
    .expect("error response emitted");
    assert!(
        resp.starts_with(b"\x1b_Gi=99;ENOENT"),
        "a=f on an unknown image is ENOENT, got: {resp:?}",
    );
}

#[test]
fn compose_copies_a_region_between_frames() {
    let (mut grid, mut images, mut placements) = handler_state();
    transmit_root(&mut grid, &mut images, &mut placements, b"1");
    let (ctrls, payload) = push_frame_cmd(b"50", [0xAB, 0xCD, 0xEF, 0xFF]);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(&ctrls, &payload),
    );
    let c: &[(u8, &[u8])] = &[
        (b'a', b"c"),
        (b'i', b"1"),
        (b'r', b"2"),
        (b'c', b"1"),
        (b'C', b"1"),
    ];
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &complete(c, b""),
    );
    assert_eq!(
        images.get(ImageId(1)).unwrap().frame(0).unwrap().pixels,
        vec![0xAB, 0xCD, 0xEF, 0xFF],
    );
}

/// Parks on the `read` builtin rather than an external `sleep`: a host
/// without an FHS `/bin` leaves `sleep` unresolved, and the shell then
/// exits at once, taking the PTY pieces with it.
#[cfg(unix)]
fn keepalive_command() -> felis_pty::Command {
    let mut cmd = felis_pty::Command::new("/bin/sh");
    cmd.args(["-c", "read _x"]);
    cmd.env_clear();
    cmd.env("PATH", "/bin:/usr/bin");
    cmd
}

#[cfg(windows)]
fn keepalive_command() -> felis_pty::Command {
    let comspec =
        std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
    let mut cmd = felis_pty::Command::new(comspec);
    // `pause` is a cmd builtin: blocks forever with no PATH dependency.
    cmd.args(["/c", "pause"]);
    cmd
}

fn dispatch_against_session<F>(prep: F) -> (Vec<Vec<u8>>, Session)
where
    F: FnOnce(&mut Session) -> Vec<Vec<u8>>,
{
    let outer = crate::SpawnedPty::spawn(keepalive_command()).expect("spawn keep-alive child");
    let crate::SpawnedPty {
        reader,
        writer,
        child,
        resizer,
        core,
        signals,
    } = outer;
    let mut session = Session {
        core,
        signals,
        images: felis_grid::images::ImageStore::new(crate::pool::DEFAULT_IMAGE_BYTE_CAP),
        placements: felis_grid::images::Placements::new(),
        saved_primary_placements: None,
        graphics_reassembler: Reassembler::new(),
        image_events: Vec::new(),
        scroll_ops: Vec::new(),
        cell_pixel_w: 8,
        cell_pixel_h: 16,
        reader,
        writer,
        child: std::sync::Arc::new(child),
        resizer: std::sync::Arc::new(resizer),
        shm_segments: ShmDeferral::default(),
    };
    let bodies = prep(&mut session);
    let cursor = session.lock_core().grid.cursor();
    let bodies: Vec<ApcBody> = bodies
        .into_iter()
        .map(|body| ApcBody {
            body,
            cursor_row: cursor.row,
            cursor_col: cursor.col,
        })
        .collect();
    let responses = {
        let core = std::sync::Arc::clone(&session.core);
        let mut guard = core.lock();
        dispatch_apc_bodies(&mut session, &mut guard.grid, bodies)
    };
    (responses, session)
}

#[test]
fn dispatch_apc_bodies_returns_responses_in_order() {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut head_a = vec![(b'a', b"t".as_slice()), (b'i', b"1".as_slice())];
    head_a.extend_from_slice(&ctrls);
    let mut head_b = vec![(b'a', b"t".as_slice()), (b'i', b"2".as_slice())];
    head_b.extend_from_slice(&ctrls);
    let cmd_a = complete(&head_a, &payload);
    let cmd_b = complete(&head_b, &payload);
    let body_for = |cmd: &CompleteCommand| {
        let mut out = b"G".to_vec();
        for (i, (k, v)) in cmd.controls.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.push(*k);
            out.push(b'=');
            out.extend_from_slice(v);
        }
        out.push(b';');
        out.extend_from_slice(&cmd.payload);
        out
    };
    let bodies = vec![body_for(&cmd_a), body_for(&cmd_b)];
    let (responses, session) = dispatch_against_session(|_| bodies);
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0], b"\x1b_Gi=1;OK\x1b\\");
    assert_eq!(responses[1], b"\x1b_Gi=2;OK\x1b\\");
    assert_eq!(session.images.len(), 2);
}

#[test]
fn transmit_and_display_advances_cursor_by_cells() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"T".as_slice()),
        (b'i', b"50".as_slice()),
        (b'c', b"4".as_slice()),
        (b'r', b"2".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let cur = grid.cursor();
    assert_eq!(
        (cur.row, cur.col),
        (2, 4),
        "cursor must advance by (r=2 rows, c=4 cols) from origin",
    );
}

#[test]
fn transmit_and_display_with_no_cursor_move_keeps_cursor() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"T".as_slice()),
        (b'i', b"51".as_slice()),
        (b'c', b"4".as_slice()),
        (b'r', b"2".as_slice()),
        (b'C', b"1".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let cur = grid.cursor();
    assert_eq!((cur.row, cur.col), (0, 0), "C=1 → cursor stays put");
    assert_eq!(placements.len(), 1);
}

#[test]
fn natural_sizing_resolves_cells_from_image_pixels_and_advances_cursor() {
    // Pins: `c=0 r=0` still advances the cursor past the natural cell
    // box; otherwise the next prompt prints under the image and
    // z-ordering hides it (the pixcat burn-in bug).
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = vec![0u8; 32 * 16 * 4];
    let payload = b64(&pixels);
    let ctrls = [
        (b'f', b"32".as_slice()),
        (b's', b"32".as_slice()),
        (b'v', b"16".as_slice()),
        (b't', b"d".as_slice()),
    ];
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"42".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 8,
            cell_pixel_h: 16,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let cur = grid.cursor();
    assert_eq!(
        (cur.row, cur.col),
        (1, 4),
        "natural sizing must advance cursor past the resolved cell box",
    );
    let p = placements.iter().next().expect("placement recorded");
    assert_eq!((p.cols, p.rows), (4, 1));
}

#[test]
fn natural_sizing_falls_back_when_cell_pixel_dims_are_zero() {
    // Pins: before any client reports cell pixel dims, the cursor still
    // moves (no advance burns the image in permanently).
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = vec![0u8; 32 * 16 * 4];
    let payload = b64(&pixels);
    let ctrls = [
        (b'f', b"32".as_slice()),
        (b's', b"32".as_slice()),
        (b'v', b"16".as_slice()),
        (b't', b"d".as_slice()),
    ];
    let mut all = vec![(b'a', b"T".as_slice()), (b'i', b"43".as_slice())];
    all.extend_from_slice(&ctrls);
    let cmd = complete(&all, &payload);
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let cur = grid.cursor();
    assert_ne!((cur.row, cur.col), (0, 0));
}

#[cfg(unix)]
fn tempfile_with_bytes(bytes: &[u8]) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(bytes).expect("write");
    f.flush().expect("flush");
    f
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_reads_bytes_from_disk() {
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = [0xDE, 0xAD, 0xBE, 0xEF];
    let f = tempfile_with_bytes(&pixels);
    let path_b64 = b64(f.path().to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"100"),
            (b't', b"f"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let entry = images.get(ImageId(100)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_rejects_relative_path() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"some/relative/file.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"101"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(response.starts_with(b"\x1b_Gi=101;EINVAL"));
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_rejects_parent_dir_component() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"/tmp/../etc/passwd");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"102"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=102;EINVAL"),
        "got: {response:?}"
    );
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_missing_file_returns_eio() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"/this/path/does/not/exist/even/maybe.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"103"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=103;EIO"),
        "got: {response:?}"
    );
}

#[cfg(unix)] // symlink NOFOLLOW guard is POSIX-only
#[test]
fn transmit_via_file_path_rejects_symlink_with_nofollow() {
    // Pins the `OFlags::NOFOLLOW` open (docs/explanation/security-model.md
    // "Kitty graphics").
    use std::os::unix::fs::symlink;
    let (mut grid, mut images, mut placements) = handler_state();
    let target = tempfile_with_bytes(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let link_dir = tempfile::tempdir().expect("tempdir");
    let link_path = link_dir.path().join("link");
    symlink(target.path(), &link_path).expect("symlink");
    let path_b64 = b64(link_path.to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"104"),
            (b't', b"f"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=104;EIO"),
        "symlinked path → EIO under NOFOLLOW, got: {response:?}",
    );
    assert!(
        images.get(ImageId(104)).is_none(),
        "no image must land in the store on NOFOLLOW reject",
    );
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_rejects_directory_target() {
    let (mut grid, mut images, mut placements) = handler_state();
    let dir = tempfile::tempdir().expect("tempdir");
    let path_b64 = b64(dir.path().to_str().unwrap().as_bytes());
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"105"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=105;")
            && (response.windows(6).any(|w| w == b"EINVAL")
                || response.windows(3).any(|w| w == b"EIO")),
        "directory target → EINVAL or EIO, got: {response:?}",
    );
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_temp_file_reads_then_unlinks() {
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = [0xFE, 0xED, 0xFA, 0xCE];
    let f = tempfile_with_bytes(&pixels);
    let path = f.path().to_owned();
    // Keep the guard alive but off the file, so its Drop cannot race the
    // dispatcher's `unlinkat`.
    let _keep_dir_alive = f.into_temp_path();
    std::fs::write(&path, pixels).expect("rewrite for t=t test");
    let path_b64 = b64(path.to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"200"),
            (b't', b"t"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let entry = images.get(ImageId(200)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
    assert!(
        !path.exists(),
        "t=t must unlink the temp file after reading",
    );
}

#[cfg(unix)] // symlink NOFOLLOW guard is POSIX-only
#[test]
fn transmit_via_temp_file_rejects_symlink_with_nofollow() {
    use std::os::unix::fs::symlink;
    let (mut grid, mut images, mut placements) = handler_state();
    let target = tempfile_with_bytes(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let link_dir = tempfile::tempdir().expect("tempdir");
    let link_path = link_dir.path().join("link");
    symlink(target.path(), &link_path).expect("symlink");
    let path_b64 = b64(link_path.to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"201"),
            (b't', b"t"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=201;EIO"),
        "symlink target → EIO under NOFOLLOW, got: {response:?}",
    );
    assert!(
        target.path().exists(),
        "NOFOLLOW reject must not touch the symlink target",
    );
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_temp_file_rejects_relative_path() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"relative/temp.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"202"), (b't', b"t"), (b'f', b"32")],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=202;EINVAL"),
        "got: {response:?}"
    );
}

#[cfg(unix)] // t=f/t=t file transmission is Unix-only (Windows declines with ENOTSUP)
#[test]
fn transmit_via_file_path_oversize_file_returns_overbudget() {
    // `set_len` is sparse on macOS and Linux, so the file costs no disk.
    let (mut grid, mut images, mut placements) = handler_state();
    let f = tempfile::NamedTempFile::new().expect("tempfile");
    let oversize = (MAX_DECODED_BYTES as u64).saturating_add(1024);
    f.as_file().set_len(oversize).expect("set_len sparse");
    let path_b64 = b64(f.path().to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"106"),
            (b't', b"f"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    let response = handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=106;ENOTSUP"),
        "oversize → ENOTSUP (per OverBudget mapping), got: {response:?}",
    );
}

// Fills the segment with `write`, which macOS shm fds reject
// (mmap/ftruncate/fstat/close only), so the cases below are Linux-only.
#[cfg(target_os = "linux")]
fn shm_with_bytes(suffix: &str, bytes: &[u8]) -> String {
    use std::io::Write;
    let name = format!("/felis-test-{}-{suffix}", std::process::id());
    let fd = rustix::shm::open(
        &name,
        rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create test shm object");
    std::fs::File::from(fd)
        .write_all(bytes)
        .expect("fill test shm object");
    name
}

#[cfg(unix)]
fn shm_exists(name: &str) -> bool {
    rustix::shm::open(name, rustix::shm::OFlags::RDONLY, Mode::empty()).is_ok()
}

#[test]
#[cfg(target_os = "linux")]
fn transmit_via_shm_reads_without_unlinking() {
    // mpv's `--vo-kitty-use-shm` shape: slash-less name, one segment
    // reused across frames. A per-read unlink would make the next reopen
    // allocate a zero-filled inode (the "black bottom half" bug), so the
    // unlink is deferred to session teardown.
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = [0xDE, 0xAD, 0xBE, 0xEF];
    let name = shm_with_bytes("roundtrip", &pixels);
    let name_b64 = b64(name.trim_start_matches('/').as_bytes());
    let cmd = complete(
        &[
            (b'a', b"t"),
            (b'i', b"300"),
            (b't', b"s"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &name_b64,
    );
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &cmd,
    )
    .unwrap();
    let entry = images.get(ImageId(300)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
    assert!(
        shm_exists(&name),
        "t=s must leave the segment in place for the teardown sweep",
    );
    assert_eq!(
        shm_segment_name(&cmd).as_deref(),
        Some(name.trim_start_matches('/')),
    );
    unlink_shm_segment(&name);
    assert!(!shm_exists(&name), "teardown sweep must unlink the segment");
}

#[test]
#[cfg(target_os = "linux")]
fn shm_offset_and_size_select_subrange() {
    // `S=`/`O=` mirror kitty's `mmap(…, g->data_sz, g->data_offset)`.
    let name = shm_with_bytes(
        "subrange",
        &[0x00, 0x11, 0xAA, 0xBB, 0xCC, 0xDD, 0x22, 0x33],
    );
    let cmd = complete(
        &[
            (b't', b"s"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
            (b'O', b"2"),
            (b'S', b"4"),
        ],
        &b64(name.as_bytes()),
    );
    let entry = decode_image(&cmd).expect("subrange decodes");
    assert_eq!(entry.pixels(), [0xAA, 0xBB, 0xCC, 0xDD]);
    assert!(shm_exists(&name));
    unlink_shm_segment(&name);
}

#[test]
#[cfg(target_os = "linux")]
fn shm_range_past_end_is_invalid_and_leaves_name_for_teardown() {
    // Pins: the error path leaves the segment in place too, so a
    // retrying producer reusing the name keeps a stable inode.
    let name = shm_with_bytes("overrange", &[0u8; 4]);
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b'S', b"16")],
        &b64(name.as_bytes()),
    );
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_))
    ));
    assert!(
        shm_exists(&name),
        "failed t=s reads must not unlink (teardown owns it)",
    );
    unlink_shm_segment(&name);
}

#[test]
#[cfg(target_os = "linux")]
fn shm_over_budget_leaves_name_for_teardown() {
    // ftruncate is sparse on tmpfs; the object costs no memory untouched.
    let name = shm_with_bytes("overbudget", b"");
    let fd = rustix::shm::open(&name, rustix::shm::OFlags::RDWR, Mode::empty())
        .expect("reopen test shm");
    rustix::fs::ftruncate(&fd, (MAX_DECODED_BYTES as u64) + 1).expect("sparse-grow shm");
    drop(fd);
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(name.as_bytes()),
    );
    assert!(matches!(decode_image(&cmd), Err(DecodeError::OverBudget)));
    assert!(
        shm_exists(&name),
        "over-budget t=s must not unlink (teardown owns it)",
    );
    unlink_shm_segment(&name);
}

#[test]
#[cfg(target_os = "linux")]
fn shm_missing_object_returns_io_error() {
    // EIO (retryable), not EINVAL: the producer may have raced its own
    // cleanup.
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(b"felis-test-no-such-object"),
    );
    assert!(matches!(decode_image(&cmd), Err(DecodeError::IoError(_))));
}

#[test]
#[cfg(target_os = "linux")]
fn shm_name_with_embedded_slash_is_invalid() {
    // glibc's shm_open name rules reject an embedded '/' with EINVAL, so
    // traversal out of /dev/shm is not expressible.
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(b"../etc/passwd"),
    );
    assert!(matches!(
        decode_image(&cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

/// Filled by `ftruncate`, not `write`: macOS shm fds accept only
/// mmap/ftruncate/fstat/close.
#[cfg(unix)]
fn zeroed_shm(suffix: &str) -> String {
    let name = format!("/felis-{}-{suffix}", std::process::id());
    let fd = rustix::shm::open(
        &name,
        rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR,
        Mode::RUSR | Mode::WUSR,
    )
    .expect("create test shm object");
    rustix::fs::ftruncate(&fd, 4).expect("size test shm object");
    name
}

fn dispatch_with_deferral(cmd: &CompleteCommand, shm: &mut ShmDeferral) -> Option<Vec<u8>> {
    let (mut grid, mut images, mut placements) = handler_state();
    handle_complete(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut Vec::new(),
            shm,
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: Some((0, 0)),
        },
        cmd,
    )
}

#[test]
fn the_deferral_evicts_the_oldest_name_once_full() {
    let mut deferral = ShmDeferral::default();
    for i in 0..ShmDeferral::CAP {
        assert!(
            deferral.record(format!("/seg-{i}")).is_none(),
            "the first CAP names fit without displacing anyone",
        );
    }
    assert_eq!(
        deferral.record("/seg-overflow".to_owned()).as_deref(),
        Some("/seg-0"),
        "the name that leaves is the least recently seen",
    );
    assert_eq!(deferral.names().len(), ShmDeferral::CAP);
}

#[test]
fn a_reused_name_outlives_a_flood_of_rotating_ones() {
    // Pins: a reused name keeps its slot, or the bound would unlink a
    // segment the producer is still writing into (mpv's shape).
    let mut deferral = ShmDeferral::default();
    assert!(deferral.record("/mpv-kitty".to_owned()).is_none());
    for i in 0..ShmDeferral::CAP * 4 {
        let evicted = deferral.record(format!("/rotating-{i}"));
        assert_ne!(evicted.as_deref(), Some("/mpv-kitty"));
        assert!(
            deferral.record("/mpv-kitty".to_owned()).is_none(),
            "re-sending a deferred name refreshes it rather than duplicating it",
        );
    }
    assert!(deferral.names().any(|name| name == "/mpv-kitty"));
    assert_eq!(deferral.names().len(), ShmDeferral::CAP);
}

#[test]
fn a_failed_t_eq_s_command_defers_nothing() {
    // Pins: a name the daemon never opened must not spend a slot.
    let cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(b"felis-test-no-such-object"),
    );
    let mut deferral = ShmDeferral::default();
    dispatch_with_deferral(&cmd, &mut deferral);
    assert_eq!(deferral.names().len(), 0);
}

#[test]
#[cfg(unix)]
fn overflowing_the_deferral_unlinks_the_name_it_drops() {
    let names: Vec<String> = (0..=ShmDeferral::CAP)
        .map(|i| zeroed_shm(&format!("dq{i}")))
        .collect();
    let mut deferral = ShmDeferral::default();
    for name in &names {
        let cmd = complete(
            &[
                (b'a', b"q"),
                (b't', b"s"),
                (b'f', b"32"),
                (b's', b"1"),
                (b'v', b"1"),
            ],
            &b64(name.trim_start_matches('/').as_bytes()),
        );
        dispatch_with_deferral(&cmd, &mut deferral);
    }
    assert_eq!(deferral.names().len(), ShmDeferral::CAP);
    assert!(
        !shm_exists(&names[0]),
        "the evicted name must be unlinked, not merely forgotten",
    );
    for name in &names[1..] {
        assert!(
            shm_exists(name),
            "a deferred segment stays in place until teardown",
        );
        unlink_shm_segment(name);
    }
}

#[test]
fn dispatch_apc_bodies_skips_pending_chunks_silently() {
    let head = body("Ga=t,i=4,f=24,s=1,v=1,m=1", &b64(&[0; 1]));
    let bodies = vec![head];
    let (responses, _session) = dispatch_against_session(|_| bodies);
    assert!(
        responses.is_empty(),
        "head-only transmission must not respond yet",
    );
}
/// Bytes captured from `kitten icat --transfer-mode=stream`. Pins two
/// icat contracts: payloads use Go's `RawStdEncoding` (no base64
/// padding), and `a=a` carries no `q=` while icat never reads
/// responses, so a successful `a=a` must stay silent as kitty does or
/// the OK leaks into the shell as text.
#[test]
fn icat_replay_unpadded_frame_appends_and_a_eq_a_is_silent() {
    let mut r = Reassembler::new();
    let (mut grid, mut images, mut placements) = handler_state();
    let mut events = Vec::new();
    let body_t: &[u8] = "Ga=T,q=2,f=24,o=z,s=220,v=220,I=629321462;eJzswoEMAAAMBDGa85cbwDw+TXoFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAzPgAA///wJeeX".as_bytes();
    // Payload length 231: not a multiple of 4 (unpadded base64).
    let body_f: &[u8] = "Ga=f,q=2,f=24,o=z,s=220,v=220,c=1,I=629321462,z=80;eJzswoEMAAAMAzCa+yNdYgDzWJo09wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMKMBAAD//87/TU0".as_bytes();
    assert!(
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut events,
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &mut r,
            body_t,
        )
        .is_none(),
        "q=2 suppresses the a=T response",
    );
    assert!(images.get(ImageId(629_321_462)).is_some());
    events.clear();
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        body_f,
    );
    assert!(resp.is_none(), "q=2 suppresses the a=f response");
    let entry = images.get(ImageId(629_321_462)).unwrap();
    assert_eq!(entry.frame_count(), 2, "unpadded a=f payload must decode");
    assert_eq!(entry.frame(1).unwrap().gap_ms, 80);
    assert!(
        materialize_image_events(&events, &images)
            .iter()
            .any(|e| matches!(
                e,
                ImageMsg::Header {
                    target: ImageTarget::Frame { number },
                    ..
                } if number.get() == 2
            )),
        "the appended frame ships to attached clients",
    );
    let body_a: &[u8] = b"Ga=a,s=3,v=1,r=1,I=629321462,z=80";
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        body_a,
    );
    assert!(resp.is_none(), "successful a=a must not be acknowledged");
    // a=a against a missing image still reports ENOENT, as kitty does.
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        b"Ga=a,s=3,i=424242",
    )
    .expect("a=a on an unknown image responds");
    assert!(resp.starts_with(b"\x1b_Gi=424242;ENOENT"), "got: {resp:?}");
}
