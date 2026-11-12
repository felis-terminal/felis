use super::*;

/// The reservation is what bounds the input path, so what it counts is
/// the contract: the post-transform byte count, computed without
/// consulting a mode the child can flip before the write happens.
#[test]
fn a_paste_reserves_its_bracketing_whatever_the_mode() {
    use felis_protocol::limits::{MAX_MOUSE_REPORT_BYTES, MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD};
    use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

    let reserved = |msg: &InputMsg| input_reservation_bytes(msg).map(|n| n.map(usize::try_from));

    assert_eq!(
        reserved(&InputMsg::KeyBytes(b"abc".to_vec())).unwrap(),
        Some(Ok(3)),
        "keystroke bytes reach the child verbatim",
    );
    assert_eq!(
        reserved(&InputMsg::Paste(Vec::new())).unwrap(),
        Some(Ok(PASTE_BRACKET_OVERHEAD)),
        "an empty paste still costs its brackets",
    );
    assert_eq!(
        reserved(&InputMsg::Paste(b"abc".to_vec())).unwrap(),
        Some(Ok(3 + PASTE_BRACKET_OVERHEAD)),
    );
    assert_eq!(
        reserved(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES])).unwrap(),
        Some(Ok(MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD)),
        "the largest admitted paste must still fit the budget",
    );
    // A mouse report is client input, not a reply the actor generated:
    // it reaches the PTY, so it is charged like a keystroke (the whole
    // widest encoding, since the actor picks the encoding after this).
    assert_eq!(
        reserved(&InputMsg::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action: MouseAction::Press,
            mods: InputMods::empty(),
            x: 1,
            y: 1,
            px: 1,
            py: 1,
        }))
        .unwrap(),
        Some(Ok(MAX_MOUSE_REPORT_BYTES)),
    );
    match input_reservation_bytes(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES + 1])) {
        Err(ConnError::InputOverLimit { bytes, limit }) => {
            assert_eq!((bytes, limit), (MAX_PASTE_BYTES + 1, MAX_PASTE_BYTES));
        }
        other => panic!("an over-limit paste must be refused, got {other:?}"),
    }

    for msg in [
        InputMsg::NextGridFrame,
        InputMsg::FocusChange { focused: true },
        InputMsg::Viewport {
            lines_from_bottom: 3,
            anchor: None,
        },
    ] {
        assert_eq!(
            input_reservation_bytes(&msg).unwrap(),
            None,
            "{msg:?} reaches no PTY and must not charge the budget",
        );
    }
}

/// A key's encoded width depends on modes the child owns, so the permit
/// covers the widest report any mode can produce and the two per-key
/// caps are enforced before it is asked for.
#[test]
fn a_key_reserves_the_worst_case_report_and_refuses_an_oversized_one() {
    use felis_protocol::limits::{
        MAX_KEY_CHARACTER_BYTES, MAX_KEY_REPORT_BYTES, MAX_KEY_TEXT_BYTES,
    };
    use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, NamedKey};

    let event = |key: Key, text: Option<String>| {
        InputMsg::Key(KeyEvent {
            key,
            text,
            mods: KeyMods::empty(),
            kind: KeyEventKind::Press,
            location: KeyLocation::Standard,
        })
    };

    assert_eq!(
        input_reservation_bytes(&event(Key::Named(NamedKey::Enter), None))
            .unwrap()
            .map(usize::try_from),
        Some(Ok(MAX_KEY_REPORT_BYTES)),
        "the permit cannot depend on a mode the child can flip after admission",
    );
    assert_eq!(
        input_reservation_bytes(&event(
            Key::Character("a".repeat(MAX_KEY_CHARACTER_BYTES)),
            Some("a".repeat(MAX_KEY_TEXT_BYTES)),
        ))
        .unwrap()
        .map(usize::try_from),
        Some(Ok(MAX_KEY_REPORT_BYTES)),
        "the largest admitted key costs no more than any other",
    );

    for (msg, limit) in [
        (
            event(
                Key::Character("a".repeat(MAX_KEY_CHARACTER_BYTES + 1)),
                None,
            ),
            MAX_KEY_CHARACTER_BYTES,
        ),
        (
            event(
                Key::Named(NamedKey::Enter),
                Some("a".repeat(MAX_KEY_TEXT_BYTES + 1)),
            ),
            MAX_KEY_TEXT_BYTES,
        ),
    ] {
        match input_reservation_bytes(&msg) {
            Err(ConnError::InputOverLimit { bytes, limit: got }) => {
                assert_eq!((bytes, got), (limit + 1, limit));
            }
            other => panic!("an over-limit key must be refused, got {other:?}"),
        }
    }
}

/// Reads until the fence reply lands, discarding the grid traffic that
/// shares the connection. `None` is the connection ending first, by
/// EOF or by the reset a daemon-side close leaves behind.
#[cfg(unix)]
async fn input_accepted<R>(reader: &mut FrameReader<R>) -> Option<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let frame = reader.next_frame().await.ok().flatten()?;
        if frame.kind == MessageKind::Session.as_u16()
            && matches!(
                codec::decode::<SessionToClientMsg>(&frame.body).unwrap(),
                SessionToClientMsg::InputAccepted
            )
        {
            return Some(());
        }
    }
}

/// A fence with nothing ahead of it is answered on the spot: the
/// barrier is over this connection's own earlier input, not over the
/// session's work.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_fence_is_answered_at_once() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("read _x"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;

    send_request(&mut writer, &SessionToDaemonMsg::InputFence, 1).await;
    tokio::time::timeout(Duration::from_secs(10), input_accepted(&mut reader))
        .await
        .expect("an empty fence is answered without waiting on anything")
        .expect("the connection stays open");
}

/// What the barrier buys the caller: once the fence is answered the
/// bytes are the daemon's, so a client that hangs up straight after
/// still gets them typed into the child.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_admitted_before_the_fence_survives_the_client_hanging_up() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        Arc::clone(&pool),
        shell_factory("read line; printf 'GOT:%s' \"$line\""),
    );

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    send_input(&mut writer, &InputMsg::Paste(b"felis\r".to_vec())).await;
    send_request(&mut writer, &SessionToDaemonMsg::InputFence, 1).await;
    tokio::time::timeout(Duration::from_secs(30), input_accepted(&mut reader))
        .await
        .expect("the fence behind an admitted paste is answered")
        .expect("the connection stays open");
    drop(writer);
    drop(reader);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (mut watcher, mut watcher_writer) = dial(&path, ConnectionMode::Window, false).await;
        let rows = attach_existing(&mut watcher, &mut watcher_writer, id).await;
        if rows.iter().any(|row| row.contains("GOT:felis")) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never read the admitted paste: {rows:?}",
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The boundary the reference states: a fence sent once the session is
/// gone is answered by the connection ending, never by an
/// acknowledgement of input no session can hold.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fence_after_the_session_ended_gets_no_reply() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, Arc::clone(&pool), shell_factory("read _x"));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut reader, &mut writer).await;

    let delivered = pool
        .lock()
        .await
        .handle_cloned(SessionId(id))
        .expect("the session is still pooled")
        .shutdown()
        .await;
    assert!(delivered, "the session task takes the shutdown");
    wait_for_gone(&pool, SessionId(id)).await;

    // The write itself may already meet the closed connection, which is
    // the same verdict the read below gives.
    let _sent = writer
        .send_correlated(
            &SessionToDaemonMsg::InputFence,
            Correlation::request(RequestId::new(1).expect("non-zero request id")),
        )
        .await;
    assert!(
        tokio::time::timeout(Duration::from_secs(10), input_accepted(&mut reader))
            .await
            .expect("the connection ends rather than hanging")
            .is_none(),
        "a fence the session cannot answer ends the connection",
    );
}

/// The barrier `felis sessions send` relies on: the fence it issues
/// after the input cannot be answered while that input is still parked
/// on the budget, so a reply is proof the key was admitted rather than
/// dropped.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_parked_on_a_full_budget_holds_back_the_fence_behind_it() {
    use felis_protocol::limits::MAX_PASTE_BYTES;
    use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, NamedKey};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let gate = gate(tmp.path());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing until the gate opens, so the budget-filling paste
    // stays charged and the key behind it cannot be admitted.
    spawn_daemon(
        &path,
        pool,
        script_factory(format!(
            "stty raw -echo; read _ < {}; exec cat >/dev/null",
            gate.display()
        )),
    );

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;

    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;
    send_input(
        &mut typist_writer,
        &InputMsg::Key(KeyEvent {
            key: Key::Named(NamedKey::Enter),
            text: None,
            mods: KeyMods::empty(),
            kind: KeyEventKind::Press,
            location: KeyLocation::Standard,
        }),
    )
    .await;
    send_request(&mut typist_writer, &SessionToDaemonMsg::InputFence, 1).await;

    // Draining the grid stream is what keeps the daemon's outbound half
    // from being the thing that blocks; the fence reply is the frame
    // under test.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), input_accepted(&mut typist_reader))
            .await
            .is_err(),
        "the fence was answered while the key ahead of it was still unadmitted",
    );
    open_gate(&gate).await;
    tokio::time::timeout(Duration::from_secs(60), input_accepted(&mut typist_reader))
        .await
        .expect("the fence must be answered once the child drains the budget")
        .expect("the connection stays open");

    drop(typist_writer);
}

/// Verify `MAX_PASTE_BYTES` is grantable in one reservation against the budget.
///
/// Holds almost the entire budget while the child stalls, releases it once drained,
/// and refuses bytes past the cap before hitting the semaphore.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exact_limit_paste_takes_the_budget_whole_and_completes() {
    use felis_protocol::limits::{MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD};

    const RESERVED: u64 = (MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD) as u64;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let gate = gate(tmp.path());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing until the gate opens, so the writer thread blocks
    // with the whole reservation still held; `cat` then drains all of it.
    // Raw mode for the same reason as the stall test above: a line
    // discipline would discard the tail instead of throttling the master.
    spawn_daemon(
        &path,
        pool,
        script_factory(format!(
            "stty raw -echo; read _ < {}; exec cat >/dev/null",
            gate.display()
        )),
    );

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    let mut request = 1;

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;

    // Held whole: the permit rides the bytes to the writer thread and is
    // dropped only once `write_all` returns, so while the child stalls
    // the reservation is the entire paste plus its bracketing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let peak = loop {
        let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        if max_subject_used(&row) == Some(RESERVED) {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the budget held {} of {RESERVED} bytes",
            max_subject_used(&row).unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        per_subject_limit(&peak)
            .zip(max_subject_used(&peak))
            .is_some_and(|(limit, used)| used <= limit),
        "one paste must fit the budget it is pinned against: {peak:?}",
    );

    // And given back once the child starts reading: this is the half a
    // reservation-size unit test cannot see.
    open_gate(&gate).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        if max_subject_used(&row) == Some(0) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child drained but {} bytes are still reserved",
            max_subject_used(&row).unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // One byte past the cap is refused before any permit is asked for: a
    // reservation that large could never be granted, so the connection
    // goes rather than parking forever.
    let (mut over_reader, mut over_writer) = dial(&path, ConnectionMode::Window, false).await;
    let _rehydrated = attach_existing(&mut over_reader, &mut over_writer, id).await;
    // Written as a raw frame, because `FrameWriter::send` refuses this
    // body itself: the peer under test here is the one that ignored the
    // published limit, which is the only way the daemon's own guard is
    // ever reached.
    let over_limit = codec::encode(&InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES + 1]));
    over_writer
        .write_frame_unchecked(&Frame {
            kind: MessageKind::Input.as_u16(),
            body: &over_limit,
        })
        .await
        .unwrap();
    over_writer.flush().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Ok(Some(_frame)) = over_reader.next_frame().await {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "an over-limit paste must end its connection"
    );

    let after = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
    request += 1;
    assert_eq!(
        max_subject_used(&after),
        Some(0),
        "a refused paste must not have charged the budget: {after:?}",
    );
    assert_eq!(
        resource_row(
            &mut ops_reader,
            &mut ops_writer,
            request,
            felis_protocol::messages::ResourceKind::Sessions,
        )
        .await
        .total_used,
        1,
        "the session survives the connection that over-reached",
    );

    drain.abort();
    drop(typist_writer);
}

/// The race the mode-independent reservation exists for: the pump
/// counts a paste's bytes while `?2004` is off, the child turns it on
/// while that paste is parked on the budget, and the actor then writes
/// the bracketed form. The brackets must come out of the reservation
/// that was already granted, not out of a budget nobody charged.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paste_admitted_before_a_mode_flip_is_written_bracketed_within_its_reservation() {
    use felis_protocol::limits::{MAX_PASTE_BYTES, PTY_INPUT_BUDGET};

    // Past the 64 bytes the first paste leaves unreserved, so this one
    // cannot be admitted beside it and must park on the budget, which
    // is what puts the mode flip between its admission and its write.
    const MARKER: &str = "FLIPPED-0123456789-0123456789-0123456789-0123456789-\
0123456789-0123456789-0123456789";

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let received = tmp.path().join("received");
    let gate = gate(tmp.path());
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Reads nothing while the first paste fills the budget and the
    // second parks behind it, *then* enables bracketed paste, and only
    // afterwards drains: the flip therefore lands between the second
    // paste's admission and its write.
    spawn_daemon(
        &path,
        pool,
        script_factory(format!(
            "stty raw -echo; read _ < {}; printf '\\033[?2004h'; exec cat > {}",
            gate.display(),
            received.display()
        )),
    );

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    // Takes the budget whole, under `?2004` off.
    send_input(
        &mut typist_writer,
        &InputMsg::Paste(vec![b'x'; MAX_PASTE_BYTES]),
    )
    .await;
    send_input(
        &mut typist_writer,
        &InputMsg::Paste(MARKER.as_bytes().to_vec()),
    )
    .await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut request = 1;
    loop {
        let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        // The first paste leaves a few bytes free, and tokio's semaphore
        // hands free permits to the queued acquisition, so the whole
        // budget in use means the marker paste is the one parked.
        if max_subject_used(&row) == Some(PTY_INPUT_BUDGET as u64) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the marker paste never parked on the budget: {row:?}",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    open_gate(&gate).await;

    let expected = format!("\x1b[200~{MARKER}\x1b[201~");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if std::fs::read(&received).is_ok_and(|bytes| bytes.ends_with(expected.as_bytes())) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the paste admitted before the mode flip never reached the child bracketed",
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    drain.abort();
    drop(typist_writer);
}

/// The acceptance test for the input bound: a child that stopped reading
/// its stdin must stop the connection feeding it, not grow the daemon,
/// and must leave the session actor answering everyone else.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_child_parks_the_typist_and_not_the_daemon() {
    use felis_protocol::messages::{ResourceKind, SubjectKind};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Child never reads stdin, filling the PTY buffer so writes block.
    // Raw mode prevents canonical line discipline from discarding over-long tails
    // or echoing bytes back through the parser. It writes ticks while refusing reads.
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; while :; do printf 'tick\\r\\n'; sleep 0.1; done"),
    );

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    // The daemon's outbound half must never be what blocks: this test is
    // about the inbound direction.
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    let mut request = 1;
    let typist = fill_until_parked(
        typist_writer,
        &mut ops_reader,
        &mut ops_writer,
        &mut request,
    )
    .await;

    // The reply is itself the liveness proof: `Status` asks every session
    // task for its stats, so a frozen actor would hang here.
    let row = tokio::time::timeout(
        Duration::from_secs(5),
        pty_input_row(&mut ops_reader, &mut ops_writer, request),
    )
    .await
    .expect("the session actor answers while a connection is parked");
    request += 1;
    assert_eq!(
        per_subject_limit(&row),
        Some(felis_protocol::limits::PTY_INPUT_BUDGET as u64),
    );
    assert_eq!(subject_kind(&row), Some(SubjectKind::Session));
    assert!(
        max_subject_used(&row)
            .is_some_and(|max| max > 0 && per_subject_limit(&row).is_some_and(|cap| max <= cap)),
        "the parked session's own input must be visible and inside its budget: {row:?}",
    );

    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    // Mirror the session while the typist is parked. The mirror must reach
    // `RehydrateEnd` and receive live deltas to prove diff fan-out does not
    // deadlock or hold permits across awaits.
    let (mut mirror_reader, mut mirror_writer) = dial(&path, ConnectionMode::Window, false).await;
    let _rehydrated = tokio::time::timeout(
        Duration::from_secs(15),
        attach_existing(&mut mirror_reader, &mut mirror_writer, id),
    )
    .await
    .expect("a second window may attach while another connection is parked");
    // Past the rehydrate boundary every row is a diff the session task
    // composed and fanned out with the typist still parked.
    expect_row_containing(&mut mirror_reader, "tick").await;

    // And it may leave again: an unsubscribe is answered by the same
    // actor the parked connection is waiting on.
    send_kind(&mut mirror_writer, &SessionToDaemonMsg::Detach).await;
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    // The parked connection going away must not wedge anything: its
    // pending acquisition is dropped with it, and the slot comes back
    // without waiting for a child that never drains.
    drain.abort();
    typist.abort();
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 1).await;
    let after = tokio::time::timeout(
        Duration::from_secs(5),
        pty_input_row(&mut ops_reader, &mut ops_writer, request),
    )
    .await
    .expect("the daemon still answers after the parked peer hangs up");
    request += 1;
    assert!(
        per_subject_limit(&after) == per_subject_limit(&row),
        "the budget is a per-session constant: {after:?}",
    );
    assert_eq!(
        resource_row(
            &mut ops_reader,
            &mut ops_writer,
            request,
            ResourceKind::Sessions
        )
        .await
        .total_used,
        1,
        "the session survives its typist",
    );
}

/// Peers disconnecting while parked on the budget must release connection slots.
///
/// Ensures a pump parked awaiting budget notices client disconnects even when
/// child processes produce no output to drive the outbound writer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_hangs_up_while_parked_gives_its_connection_back() {
    use felis_protocol::limits::{PASTE_BRACKET_OVERHEAD, PTY_INPUT_BUDGET};

    const CHUNK: usize = 1024 * 1024;
    const RESERVED: usize = CHUNK + PASTE_BRACKET_OVERHEAD;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(
        &path,
        pool,
        shell_factory("stty raw -echo; printf raw-ready; sleep 30"),
    );

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let _id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    // A paste that beats `stty raw` meets canonical mode, where macOS
    // discards input past `MAX_INPUT` and the write completes, so the
    // budget drains instead of filling.
    expect_row_containing(&mut typist_reader, "raw-ready").await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    let mut request = 1;

    // Filled to just under the budget, one admitted chunk at a time, so
    // the hangup below is the clean kind: the daemon has taken every
    // byte this peer wrote, which is what a `felis sessions send` that
    // finishes and exits leaves behind.
    let mut admitted = 0usize;
    while admitted + RESERVED <= PTY_INPUT_BUDGET {
        send_input(&mut typist_writer, &InputMsg::Paste(vec![b'x'; CHUNK])).await;
        admitted += RESERVED;
        let want = admitted as u64;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let row = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
            request += 1;
            if max_subject_used(&row).is_some_and(|used| used >= want) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon admitted {} of {want} bytes",
                max_subject_used(&row).unwrap_or_default(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // One more than the budget can grant: the pump reads it whole and
    // then parks, leaving nothing of this peer's unread on the socket.
    send_input(&mut typist_writer, &InputMsg::Paste(vec![b'x'; CHUNK])).await;
    // The waiting acquisition takes the budget's remainder (tokio's
    // semaphore hands free permits to the queued waiter), so the whole
    // budget in use is the pump parked on the last chunk.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let parked = pty_input_row(&mut ops_reader, &mut ops_writer, request).await;
        request += 1;
        if max_subject_used(&parked) == Some(PTY_INPUT_BUDGET as u64) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the last chunk never parked on the budget: {parked:?}",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 2).await;

    drain.abort();
    drop(typist_writer);
    await_connections(&mut ops_reader, &mut ops_writer, &mut request, 1).await;
}

/// The other half of the deadlock surface: a session destroyed under a
/// connection that is parked on its input budget. The parked pump waits
/// on a semaphore no one will post, so the actor's end has to be what
/// releases it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destroying_a_session_releases_a_connection_parked_on_its_budget() {
    use felis_protocol::messages::ResolvedId;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool, shell_factory("stty raw -echo; sleep 30"));

    let (mut typist_reader, mut typist_writer) = dial(&path, ConnectionMode::Window, false).await;
    let id = create_and_attach(&mut typist_reader, &mut typist_writer).await;
    let drain =
        tokio::spawn(
            async move { while let Ok(Some(_frame)) = typist_reader.next_frame().await {} },
        );

    let (mut ops_reader, mut ops_writer) = dial(&path, ConnectionMode::Ops, false).await;
    let mut request = 1;
    let typist = fill_until_parked(
        typist_writer,
        &mut ops_reader,
        &mut ops_writer,
        &mut request,
    )
    .await;

    send_request(
        &mut ops_writer,
        &OpsToDaemonMsg::Destroy {
            id_prefix: format!("{id:032x}"),
        },
        request,
    )
    .await;
    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = ops_reader.next_frame().await.unwrap().expect("a reply");
            if frame.kind == MessageKind::Ops.as_u16() {
                break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
            }
        }
    })
    .await
    .expect("a destroy must not wait for a stalled child");
    assert!(
        matches!(
            reply,
            OpsToClientMsg::Destroyed {
                resolved: ResolvedId::Ok { .. }
            }
        ),
        "{reply:?}",
    );

    // The parked pump has to end with the session it was feeding: the
    // budget it waits on is a semaphore nothing will post once the
    // actor that would have drained the writes is gone. The drain task
    // ends when the daemon closes the connection.
    tokio::time::timeout(Duration::from_secs(10), drain)
        .await
        .expect("the parked connection must be released by the session's end")
        .expect("the drain task must not panic");
    typist.abort();
}

/// Pastes into a session whose child reads nothing until the pump
/// carrying the pastes parks on the input budget. The returned task owns
/// the typist's write half, still blocked on the socket; aborting it is
/// the typist hanging up.
#[cfg(unix)]
async fn fill_until_parked<W, R2, W2>(
    mut typist: FrameWriter<W>,
    ops_reader: &mut FrameReader<R2>,
    ops_writer: &mut FrameWriter<W2>,
    request: &mut u64,
) -> tokio::task::JoinHandle<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    R2: tokio::io::AsyncRead + Unpin,
    W2: tokio::io::AsyncWrite + Unpin,
{
    use felis_protocol::limits::PTY_INPUT_BUDGET;

    const CHUNK: usize = 1024 * 1024;
    // Enough to fill the budget plus whatever the socket buffers, with
    // headroom so a slow machine is not what fails the assertion.
    const MAX_CHUNKS: usize = 64;

    let sender = tokio::spawn(async move {
        for _ in 0..MAX_CHUNKS {
            let paste = InputMsg::Paste(vec![b'x'; CHUNK]);
            if typist.send(&paste).await.is_err() {
                return;
            }
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            !sender.is_finished(),
            "{MAX_CHUNKS} MiB reached a child reading nothing: the daemon is not bounding input",
        );
        let row = pty_input_row(ops_reader, ops_writer, *request).await;
        *request += 1;
        // A chunk too large for what is left takes the budget's remainder while
        // it waits (tokio's semaphore hands free permits to the queued
        // acquisition), so the whole budget in use means the pump is
        // parked.
        if max_subject_used(&row) == Some(PTY_INPUT_BUDGET as u64) {
            return sender;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pump never parked on the budget: {row:?}",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Polls until the daemon reports `want` served connections, counting
/// the one asking. Teardown is a task ending, not a reply, so the count
/// it frees lands some time after whatever caused it.
#[cfg(unix)]
async fn await_connections<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: &mut u64,
    want: u64,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use felis_protocol::messages::ResourceKind;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut seen = u64::MAX;
    while tokio::time::Instant::now() < deadline {
        seen = resource_row(reader, writer, *request, ResourceKind::Connections)
            .await
            .total_used;
        *request += 1;
        if seen == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the daemon still serves {seen} connections, expected {want}");
}

#[cfg(unix)]
async fn pty_input_row<R, W>(
    reader: &mut FrameReader<R>,
    writer: &mut FrameWriter<W>,
    request: u64,
) -> felis_protocol::messages::ResourceReport
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    resource_row(
        reader,
        writer,
        request,
        felis_protocol::messages::ResourceKind::PtyInputBytes,
    )
    .await
}

/// The cap is checked against registrations plus the slots creates in
/// flight already hold (REQ-915), and a refusal names that number, so
/// the row must count the same thing: a burst that fills the cap with
/// reservations would otherwise print room the next create is refused
/// for.
#[tokio::test]
async fn the_sessions_row_counts_a_slot_reserved_before_registration() {
    use felis_protocol::messages::ResourceKind;

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 3,
        ..DaemonCaps::default()
    };
    let sessions_row = async |pool: &Arc<Mutex<SessionPool>>| {
        let reply = daemon_status(pool, &caps).await;
        let OpsToClientMsg::StatusReply { resources, .. } = reply else {
            panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
        };
        *resources
            .iter()
            .find(|r| r.resource == ResourceKind::Sessions)
            .unwrap_or_else(|| panic!("no sessions row in {resources:?}"))
    };

    let slot = pool
        .lock()
        .await
        .try_reserve(caps.max_sessions)
        .expect("a free slot");
    let reserved = sessions_row(&pool).await;
    assert_eq!(
        reserved.total_used, 1,
        "a create between reservation and registration is admitted: {reserved:?}"
    );
    assert_eq!(global_limit(&reserved), Some(3));

    drop(slot);
    let released = sessions_row(&pool).await;
    assert_eq!(
        released.total_used, 0,
        "a create that never registered gives its slot back: {released:?}"
    );
}

#[test]
fn a_bracketed_paste_body_keeps_tab_newline_and_return_and_drops_other_controls() {
    assert_eq!(
        frame_paste(true, b"a\tb\nc\rd\x1b[201~e\x00\x03\x7f".to_vec()),
        b"\x1b[200~a\tb\nc\rd[201~e\x7f\x1b[201~",
    );
}

#[test]
fn an_unbracketed_paste_is_written_unchanged() {
    assert_eq!(frame_paste(false, b"a\x1bb\x03".to_vec()), b"a\x1bb\x03");
}

proptest::proptest! {
    /// The only `ESC` in a bracketed paste is in its two brackets.
    #[test]
    fn a_bracketed_paste_carries_no_escape_inside_its_brackets(
        body in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256),
    ) {
        let framed = frame_paste(true, body);
        let inner = &framed[6..framed.len() - 6];
        proptest::prop_assert!(!inner.contains(&0x1b));
    }
}
