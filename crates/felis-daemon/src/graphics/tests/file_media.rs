use super::*;

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
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let entry = images.get(ImageId(100)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
}

/// kitty reads `t=f` / `t=t` data from `O=` for `S=` bytes, as for `t=s`;
/// producers that pack several images into one file rely on it.
#[test]
fn file_transmissions_read_the_offset_and_size_they_name() {
    let bytes = [0, 0, 0xDE, 0xAD, 0xBE, 0xEF, 0, 0];
    let ranged = |medium: &'static [u8], path: &std::path::Path| {
        decode_image(&mut complete(
            &[
                (b't', medium),
                (b'f', b"32"),
                (b's', b"1"),
                (b'v', b"1"),
                (b'O', b"2"),
                (b'S', b"4"),
            ],
            &b64(path.to_str().unwrap().as_bytes()),
        ))
    };
    let f = tempfile_with_bytes(&bytes);
    assert_eq!(
        ranged(b"f", f.path()).unwrap().pixels(),
        [0xDE, 0xAD, 0xBE, 0xEF]
    );
    let t = graphics_tempfile_with_bytes(&bytes);
    assert_eq!(ranged(b"t", &t).unwrap().pixels(), [0xDE, 0xAD, 0xBE, 0xEF]);
    assert!(!t.exists());
}

#[test]
fn a_temp_file_range_past_its_end_is_refused_and_still_unlinked() {
    let t = graphics_tempfile_with_bytes(&[0; 4]);
    let result = decode_image(&mut complete(
        &[
            (b't', b"t"),
            (b'f', b"32"),
            (b's', b"1"),
            (b'v', b"1"),
            (b'S', b"8"),
        ],
        &b64(t.to_str().unwrap().as_bytes()),
    ));
    assert!(matches!(result, Err(DecodeError::IoError(_))));
    assert!(!t.exists());
}

#[test]
fn transmit_via_file_path_rejects_relative_path() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"some/relative/file.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"101"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(response.starts_with(b"\x1b_Gi=101;EINVAL"));
}

#[test]
fn transmit_via_file_path_rejects_parent_dir_component() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"/tmp/../etc/passwd");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"102"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=102;EINVAL"),
        "got: {response:?}"
    );
}

#[test]
fn transmit_via_file_path_missing_file_returns_ebadf() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"/this/path/does/not/exist/even/maybe.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"103"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=103;EBADF"),
        "got: {response:?}"
    );
}

#[test]
fn transmit_via_file_path_rejects_directory_target_with_ebadf() {
    let (mut grid, mut images, mut placements) = handler_state();
    let dir = tempfile::tempdir().expect("tempdir");
    let path_b64 = b64(dir.path().to_str().unwrap().as_bytes());
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"105"), (b't', b"f"), (b'f', b"32")],
        &path_b64,
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=105;EBADF:Failed to read image file"),
        "a directory gets the one file-medium reply, got: {response:?}",
    );
}

#[test]
fn transmit_via_temp_file_reads_then_unlinks() {
    let (mut grid, mut images, mut placements) = handler_state();
    let pixels = [0xFE, 0xED, 0xFA, 0xCE];
    let path = graphics_tempfile_with_bytes(&pixels);
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
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let entry = images.get(ImageId(200)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
    assert!(
        !path.exists(),
        "t=t must unlink the temp file after reading",
    );
}

#[test]
fn a_marked_temp_file_in_a_nested_temp_directory_is_unlinked() {
    let dir = tempfile::Builder::new()
        .prefix("tty-graphics-protocol-")
        .tempdir()
        .unwrap();
    let nested = dir.path().join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    let t = nested.join("image");
    std::fs::write(&t, [0xFE, 0xED, 0xFA, 0xCE]).unwrap();
    decode_image(&mut complete(
        &[(b't', b"t"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(t.to_str().unwrap().as_bytes()),
    ))
    .expect("decoded");
    assert!(!t.exists());
}

#[test]
fn a_temp_file_without_the_protocol_marker_is_read_but_kept() {
    let t = tempfile_with_bytes(&[0xFE, 0xED, 0xFA, 0xCE]);
    let image = decode_image(&mut complete(
        &[(b't', b"t"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(t.path().to_str().unwrap().as_bytes()),
    ))
    .expect("decoded");
    assert_eq!(image.pixels(), [0xFE, 0xED, 0xFA, 0xCE]);
    assert!(t.path().exists());
}

#[test]
fn a_temp_file_outside_the_temp_directories_is_not_deletable() {
    let marked = "/etc/tty-graphics-protocol-x";
    assert!(!deletable_path(std::path::Path::new("/etc"), marked));
    let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let in_tmp = format!("{}/tty-graphics-protocol-x", tmp.display());
    assert!(deletable_path(&tmp, &in_tmp));
    assert!(!deletable_path(&tmp, &format!("{}/x", tmp.display())));
}

#[test]
fn kernel_interfaces_and_devices_are_not_readable_locations() {
    let readable = |p: &str| readable_location(std::path::Path::new(p));
    assert!(!readable("/proc/self/environ"));
    assert!(!readable("/sys/kernel/notes"));
    assert!(!readable("/dev/null"));
    assert!(!readable("/dev/shm"));
    assert!(readable("/dev/shm/tty-graphics-protocol-x"));
    assert!(readable("/tmp/image.png"));
    assert!(readable("/home/user/procfile"));
}

fn file_command(medium: &'static [u8], path: &std::path::Path) -> CompleteCommand {
    complete(
        &[(b't', medium), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(path.to_str().unwrap().as_bytes()),
    )
}

/// Missing, not regular, out of range or undecodable: a producer told
/// any of these apart would learn what the path holds.
#[test]
fn every_file_medium_failure_gets_the_same_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let short = tempfile_with_bytes(b"ab");
    let ranged = tempfile_with_bytes(&[0; 4]);
    let failures = [
        decode_image(&mut file_command(b"f", &dir.path().join("missing"))),
        decode_image(&mut file_command(b"f", dir.path())),
        decode_image(&mut file_command(b"f", short.path())),
        decode_image(&mut complete(
            &[
                (b't', b"f"),
                (b'f', b"32"),
                (b's', b"1"),
                (b'v', b"1"),
                (b'O', b"8"),
            ],
            &b64(ranged.path().to_str().unwrap().as_bytes()),
        )),
        decode_image(&mut file_command(b"t", &dir.path().join("missing"))),
        decode_image(&mut file_command(b"t", short.path())),
    ];
    for failure in &failures {
        assert_eq!(failure, &failures[0]);
    }
    assert!(matches!(failures[0], Err(DecodeError::IoError(_))));
}

#[cfg(target_os = "linux")]
#[test]
fn file_media_refuse_proc_even_through_a_symlinked_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let link = dir.path().join("self");
    std::os::unix::fs::symlink("/proc/self", &link).expect("symlink");
    for path in [
        std::path::Path::new("/proc/self/status"),
        &link.join("status"),
    ] {
        for medium in [&b"f"[..], b"t"] {
            assert!(
                matches!(
                    decode_image(&mut file_command(medium, path)),
                    Err(DecodeError::IoError(_))
                ),
                "{path:?} must not be read",
            );
        }
    }
}

/// A resolved parent is walked without following links, so a component
/// that became a symlink after the canonicalize is refused.
#[test]
fn the_resolved_parent_walk_refuses_a_symlinked_component() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = std::fs::canonicalize(dir.path()).expect("canonicalize");
    std::fs::create_dir(real.join("sub")).expect("mkdir");
    std::os::unix::fs::symlink(real.join("sub"), real.join("link")).expect("symlink");
    assert!(open_resolved_dir(&real.join("sub")).is_ok());
    assert!(open_resolved_dir(&real.join("link")).is_err());
}

#[test]
fn a_fifo_is_refused_without_waiting_for_a_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fifo = dir.path().join("tty-graphics-protocol-fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success());
    for medium in [&b"f"[..], b"t"] {
        assert!(matches!(
            decode_image(&mut file_command(medium, &fifo)),
            Err(DecodeError::IoError(_))
        ));
    }
}

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
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=201;EBADF"),
        "symlink target → EBADF under NOFOLLOW, got: {response:?}",
    );
    assert!(
        target.path().exists(),
        "NOFOLLOW reject must not touch the symlink target",
    );
}

#[test]
fn transmit_via_temp_file_rejects_relative_path() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path_b64 = b64(b"relative/temp.png");
    let cmd = complete(
        &[(b'a', b"t"), (b'i', b"202"), (b't', b"t"), (b'f', b"32")],
        &path_b64,
    );
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=202;EINVAL"),
        "got: {response:?}"
    );
}

#[test]
fn transmit_via_file_path_oversize_file_returns_ebadf() {
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
    let response = run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    assert!(
        response.starts_with(b"\x1b_Gi=106;EBADF:Failed to read image file"),
        "an oversize file gets the one file-medium reply, got: {response:?}",
    );
}
