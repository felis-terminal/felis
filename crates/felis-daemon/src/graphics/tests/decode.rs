use super::*;

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

        let entry = decode_image(&mut complete(&controls, &b64(&pixels)))
            .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(entry.width, width);
        prop_assert_eq!(entry.height, height);
        prop_assert_eq!(entry.format, format);
        prop_assert_eq!(entry.pixels(), &pixels[..expected]);

        let short = decode_image(&mut complete(&controls, &b64(&pixels[..expected - 1])));
        prop_assert!(matches!(short, Err(DecodeError::NoData(_))));
    }
}

#[test]
fn missing_format_defaults_to_rgba32() {
    // Pins the Kitty spec's documented default `f=32`.
    let pixels = [0xAA, 0xBB, 0xCC, 0xDD];
    let mut cmd = complete(&[(b's', b"1"), (b'v', b"1")], &b64(&pixels));
    let entry = decode_image(&mut cmd).unwrap();
    assert_eq!(entry.format, ImageFormat::Rgba32);
    assert_eq!(entry.pixels(), pixels);
}

#[test]
fn raw_rejects_missing_dimensions() {
    let mut cmd = complete(&[(b'f', b"32"), (b'v', b"1")], &b64(&[0; 4]));
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

#[test]
fn raw_rejects_dimensions_over_per_image_cap() {
    // Pins: rejected before allocation, not after.
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"8000"), (b'v', b"8000")],
        &b64(b""),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::OverBudget)
    ));
}

#[test]
fn raw_rejects_when_s_times_v_overflows_usize() {
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"4000000000"), (b'v', b"4000000000")],
        &b64(b""),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::InvalidValue(_) | DecodeError::OverBudget)
    ));
}

#[test]
fn a_middle_chunk_outside_base64_is_answered_once_at_the_final_chunk() {
    let mut r = Reassembler::new();
    let (mut grid, mut images, mut placements) = handler_state();
    let mut replies = Vec::new();
    for chunk in [
        &b"Ga=t,i=3,f=24,s=1,v=1,m=1;YW"[..],
        b"Gm=1;J*",
        b"Gm=1;ZG",
        b"Gm=0;Vm",
    ] {
        replies.push(dispatch_apc_body(
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
            &mut r,
            chunk,
        ));
    }
    assert_eq!(
        replies,
        [
            None,
            None,
            None,
            Some(b"\x1b_Gi=3;EINVAL:malformed base64 payload\x1b\\".to_vec())
        ]
    );
    assert_eq!(images.len(), 0);
}

#[test]
fn bad_base64_payload_returns_invalid_value() {
    let mut cmd = complete(&[(b'f', b"32"), (b's', b"1"), (b'v', b"1")], b"!@#$");
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::InvalidValue(_))
    ));
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
    let mut cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(name.trim_start_matches('/').as_bytes()),
    );
    let entry = decode_image(&mut cmd).expect("macOS t=s segment must decode");
    assert_eq!(entry.pixels(), [0, 0, 0, 0]);
    unlink_shm_segment(&name);
}

#[test]
#[cfg(windows)]
fn windows_declines_file_temp_and_shm_transmission_with_enotsup() {
    // ENOTSUP, not EBADF: EBADF is retryable and would tell a producer to
    // keep trying a path that can never work.
    for method in [b"f".as_slice(), b"t".as_slice(), b"s".as_slice()] {
        let mut cmd = complete(
            &[(b't', method), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
            &b64(br"C:\Users\test\felis-image.rgba"),
        );
        assert!(
            matches!(decode_image(&mut cmd), Err(DecodeError::Unsupported(_))),
            "t={} must decline with ENOTSUP, got: {:?}",
            char::from(method[0]),
            decode_image(&mut cmd),
        );
    }
}

#[test]
#[cfg(windows)]
fn windows_direct_transmission_still_decodes() {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut cmd = complete(&ctrls, &payload);
    let entry = decode_image(&mut cmd).expect("t=d must decode on Windows");
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
    let response = run(
        &mut grid,
        &mut images,
        &mut placements,
        complete(&controls, &path),
    )
    .expect("the q=0 default replies to errors");
    assert!(
        response.starts_with(b"\x1b_Gi=41;ENOTSUP"),
        "got: {response:?}",
    );

    let mut quiet = controls.to_vec();
    quiet.push((b'q', b"2"));
    assert!(
        run(
            &mut grid,
            &mut images,
            &mut placements,
            complete(&quiet, &path),
        )
        .is_none(),
        "q=2 must suppress the rejection too",
    );
}

#[test]
fn malformed_transmission_method_returns_invalid_value() {
    // EINVAL, not ENOTSUP: producers may retry ENOTSUP.
    let mut cmd = complete(
        &[(b't', b"x"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        b"",
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::InvalidValue(_))
    ));
}

#[test]
fn unknown_format_returns_invalid_value() {
    let mut cmd = complete(&[(b'f', b"64"), (b's', b"1"), (b'v', b"1")], &b64(&[0; 4]));
    assert!(matches!(
        decode_image(&mut cmd),
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
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"2"), (b'v', b"2"), (b'o', b"z")],
        &b64(&compressed),
    );
    let entry = decode_image(&mut cmd).expect("zlib + raw RGBA round-trips");
    assert_eq!(entry.pixels(), pixels);
}

#[test]
fn zlib_zip_bomb_is_rejected() {
    // Pins: the inflate stops at the per-image cap instead of growing.
    let huge = vec![0u8; MAX_DECODED_BYTES + 1024];
    let compressed = zlib_compress(&huge);
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"1"), (b'v', b"1"), (b'o', b"z")],
        &b64(&compressed),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::OverBudget)
    ));
}

#[test]
fn zlib_inflating_past_the_declared_size_is_over_budget() {
    let compressed = zlib_compress(&[0x77u8; 17]);
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"2"), (b'v', b"2"), (b'o', b"z")],
        &b64(&compressed),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::OverBudget)
    ));
}

#[test]
fn unknown_compression_returns_unsupported() {
    let mut cmd = complete(
        &[(b'f', b"32"), (b's', b"1"), (b'v', b"1"), (b'o', b"lz4")],
        &b64(b""),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::Unsupported(_))
    ));
}

#[test]
fn png_payload_decodes_to_rgba8() {
    let png = tiny_png();
    let mut cmd = complete(&[(b'f', b"100")], &b64(&png));
    let entry = decode_image(&mut cmd).expect("PNG round-trips");
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
    let mut cmd = complete(&[(b'f', b"100")], &b64(&gray));
    let entry = decode_image(&mut cmd).expect("gray PNG decodes");
    assert_eq!(entry.format, ImageFormat::Rgba32);
    assert_eq!(
        entry.pixels(),
        &[0x00, 0x00, 0x00, 0xFF, 0x7F, 0x7F, 0x7F, 0xFF]
    );

    let rgb = encode_png(1, 1, png::ColorType::Rgb, &[0x01, 0x02, 0x03]);
    let mut cmd = complete(&[(b'f', b"100")], &b64(&rgb));
    let entry = decode_image(&mut cmd).expect("RGB PNG decodes");
    assert_eq!(entry.pixels(), &[0x01, 0x02, 0x03, 0xFF]);
}

#[test]
fn corrupt_png_returns_bad_image() {
    let mut png = tiny_png();
    for b in &mut png[..8] {
        *b = 0;
    }
    let mut cmd = complete(&[(b'f', b"100")], &b64(&png));
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::BadImage(_))
    ));
}

#[test]
fn zlib_plus_png_round_trips() {
    let png = tiny_png();
    let compressed = zlib_compress(&png);
    let mut cmd = complete(&[(b'f', b"100"), (b'o', b"z")], &b64(&compressed));
    let entry = decode_image(&mut cmd).expect("zlib + PNG round-trips");
    assert_eq!(entry.width, 2);
    assert_eq!(entry.height, 1);
    assert_eq!(entry.format, ImageFormat::Rgba32);
}
