use super::*;

/// Parks on the `read` builtin rather than an external `sleep`: a host
/// without an FHS `/bin` leaves `sleep` unresolved, and the shell then
/// exits at once, taking the PTY pieces with it.
#[cfg(unix)]
fn keepalive_command() -> felis_pty::Command {
    crate::test_support::sh("read _x", "/bin:/usr/bin")
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
        pty:
            felis_pty::PtyParts {
                eof,
                writer,
                child,
                resizer,
                quiescer,
            },
        core,
        signals,
    } = outer;
    let mut session = Session {
        core,
        signals,
        images: felis_grid::images::ImageStore::new(crate::pool::DEFAULT_IMAGE_BYTE_CAP),
        placements: felis_grid::images::Placements::new(),
        saved_screens: SavedScreens::default(),
        graphics_reassembler: Reassembler::new(),
        image_events: Vec::new(),
        scroll_ops: Vec::new(),
        command_outputs: Vec::new(),
        observed: Vec::new(),
        program_status: crate::program_status::ProgramStatusStore::default(),
        cell_pixel_w: 8,
        cell_pixel_h: 16,
        eof,
        writer,
        child: std::sync::Arc::new(child),
        resizer: std::sync::Arc::new(resizer),
        shm_segments: ShmDeferral::default(),
        quiescer,
        spawn_cwd: None,
        terminal_progress: None,
    };
    let bodies = prep(&mut session);
    let cursor = session.lock_core().grid.cursor();
    let bodies: Vec<ApcBody> = bodies
        .into_iter()
        .map(|body| ApcBody {
            body,
            cursor_row: cursor.row,
            cursor_col: cursor.col,
            stops_parse: false,
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
        out.extend_from_slice(&b64(cmd.payload.as_deref().unwrap()));
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
fn dispatch_apc_bodies_skips_pending_chunks_silently() {
    let head = body("Ga=t,i=4,f=24,s=1,v=1,m=1", &b64(&[0; 1]));
    let bodies = vec![head];
    let (responses, _session) = dispatch_against_session(|_| bodies);
    assert!(
        responses.is_empty(),
        "head-only transmission must not respond yet",
    );
}

fn prepare_for_test(
    grid: &mut felis_grid::Grid,
    images: &mut felis_grid::images::ImageStore,
    placements: &mut felis_grid::images::Placements,
    shm: &mut ShmDeferral,
    complete: CompleteCommand,
) -> Prepared {
    prepare_complete(
        &mut ApcCtx {
            grid,
            images,
            placements,
            events: &mut Vec::new(),
            shm,
            cell_pixel_w: 8,
            cell_pixel_h: 16,
            anchor_cursor: Some((0, 0)),
        },
        complete,
        None,
        0,
    )
}

/// The existence check runs before the decode would read, and delete,
/// a `t=t` source.
#[cfg(unix)]
#[test]
fn a_frame_for_a_missing_image_is_refused_before_its_temp_file_is_read() {
    let (mut grid, mut images, mut placements) = handler_state();
    let path = graphics_tempfile_with_bytes(&[0; 4]);
    let path_b64 = b64(path.to_str().unwrap().as_bytes());
    let cmd = complete(
        &[
            (b'a', b"f"),
            (b'i', b"9"),
            (b't', b"t"),
            (b's', b"1"),
            (b'v', b"1"),
        ],
        &path_b64,
    );
    let prepared = prepare_for_test(
        &mut grid,
        &mut images,
        &mut placements,
        &mut ShmDeferral::default(),
        cmd,
    );
    let Prepared::Done(reply) = prepared else {
        panic!("a frame for a missing image has nothing to decode");
    };
    assert_eq!(
        reply,
        Some(b"\x1b_Gi=9;ENOENT:a=f: image not in store\x1b\\".to_vec())
    );
    assert!(path.exists(), "the temp file is left for the producer");
    std::fs::remove_file(&path).unwrap();
}

/// A decoder that panicked answers as a failed decode and stores,
/// places and records nothing.
#[test]
fn a_commit_without_its_decode_answers_as_a_failed_decode() {
    let (mut grid, mut images, mut placements) = handler_state();
    let mut shm = ShmDeferral::default();
    for (controls, payload, want) in [
        (
            &[(b'a', b"T".as_slice()), (b'i', b"5"), (b'f', b"100")][..],
            tiny_png(),
            &b"\x1b_Gi=5;EBADPNG:image decoder failed\x1b\\"[..],
        ),
        (
            &[(b'a', b"t"), (b'i', b"6"), (b't', b"s")],
            b"felis-panicked".to_vec(),
            b"\x1b_Gi=6;EBADF:Failed to read image file\x1b\\",
        ),
    ] {
        let prepared = prepare_for_test(
            &mut grid,
            &mut images,
            &mut placements,
            &mut shm,
            complete(controls, &payload),
        );
        let Prepared::Decode(PreparedDecode { plan, .. }) = prepared else {
            panic!("a transmission decodes");
        };
        assert_eq!(commit_failed_decode(&plan).as_deref(), Some(want));
    }
    assert_eq!(images.len(), 0);
    assert!(placements.is_empty());
    assert_eq!(grid.cursor().row, 0, "nothing was placed to step past");
    assert!(shm.names().next().is_none());
}
