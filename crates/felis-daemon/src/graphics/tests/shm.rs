use super::*;

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
    let segment = shm_segment_name(&cmd);
    run(&mut grid, &mut images, &mut placements, cmd).unwrap();
    let entry = images.get(ImageId(300)).expect("image stored");
    assert_eq!(entry.pixels(), pixels);
    assert!(
        shm_exists(&name),
        "t=s must leave the segment in place for the teardown sweep",
    );
    assert_eq!(segment.as_deref(), Some(name.trim_start_matches('/')),);
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
    let mut cmd = complete(
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
    let entry = decode_image(&mut cmd).expect("subrange decodes");
    assert_eq!(entry.pixels(), [0xAA, 0xBB, 0xCC, 0xDD]);
    assert!(shm_exists(&name));
    unlink_shm_segment(&name);
}

#[test]
#[cfg(target_os = "linux")]
fn shm_range_past_end_is_refused_and_leaves_name_for_teardown() {
    // Pins: the error path leaves the segment in place too, so a
    // retrying producer reusing the name keeps a stable inode.
    let name = shm_with_bytes("overrange", &[0u8; 4]);
    let mut cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b'S', b"16")],
        &b64(name.as_bytes()),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::IoError(_))
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
    let mut cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(name.as_bytes()),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::IoError(_))
    ));
    assert!(
        shm_exists(&name),
        "over-budget t=s must not unlink (teardown owns it)",
    );
    unlink_shm_segment(&name);
}

#[test]
#[cfg(target_os = "linux")]
fn shm_missing_object_returns_io_error() {
    // EBADF (retryable), not EINVAL: the producer may have raced its own
    // cleanup.
    let mut cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(b"felis-test-no-such-object"),
    );
    assert!(matches!(
        decode_image(&mut cmd),
        Err(DecodeError::IoError(_))
    ));
}

#[test]
#[cfg(target_os = "linux")]
fn shm_name_with_embedded_slash_is_invalid() {
    // glibc's shm_open name rules reject an embedded '/' with EINVAL, so
    // traversal out of /dev/shm is not expressible.
    let mut cmd = complete(
        &[(b't', b"s"), (b'f', b"32"), (b's', b"1"), (b'v', b"1")],
        &b64(b"../etc/passwd"),
    );
    assert!(matches!(
        decode_image(&mut cmd),
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

fn dispatch_with_deferral(cmd: CompleteCommand, shm: &mut ShmDeferral) -> Option<Vec<u8>> {
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
    dispatch_with_deferral(cmd, &mut deferral);
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
        dispatch_with_deferral(cmd, &mut deferral);
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
