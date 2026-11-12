use super::*;

/// An unresolvable program surfaces as `PtyError::Spawn`, not a
/// hang or a panic.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawning_a_nonexistent_program_reports_spawn_error() {
    let (_collected, sink) = collector();
    let result = spawn(Command::new("/nonexistent/felis-probe"), small_size(), sink);
    match result {
        Err(PtyError::Spawn(_)) => {}
        Err(other) => panic!("expected PtyError::Spawn, got {other:?}"),
        Ok(_) => panic!("expected PtyError::Spawn, got a live session"),
    }
}

/// The initial winsize reaches the kernel and a `Resizer::resize`
/// is visible to a child that asks again (`stty size`).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_winsize_and_resize_reach_the_child() {
    use tokio::io::AsyncWriteExt;

    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "stty size; read _x; stty size; exit 0"]);
    let (collected, sink) = collector();
    let session = spawn(cmd, small_size(), sink).expect("spawn");
    let PtyParts {
        mut writer,
        resizer,
        ..
    } = session;

    let first = sink_until(&collected, "24 80").await;
    assert!(first.contains("24 80"), "initial winsize: {first:?}");

    resizer
        .resize(Size {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("resize");
    writer.write_all(b"\n").await.expect("wake the child");
    let second = sink_until(&collected, "30 100").await;
    assert!(second.contains("30 100"), "post-resize winsize: {second:?}");
}

/// `PATH` for `env_clear`ed test fixtures.
///
/// Combines standard FHS paths (`/bin:/usr/bin`) with the host `PATH` so
/// non-FHS systems (such as NixOS) can resolve test binaries.
#[cfg(unix)]
fn fixture_path() -> std::ffi::OsString {
    let mut path = std::ffi::OsString::from("/bin:/usr/bin");
    if let Some(host) = std::env::var_os("PATH") {
        path.push(":");
        path.push(host);
    }
    path
}

/// The child's stdout reaches the sink across the PTY, none of it
/// leaks into the reader half, and the child exits. On Windows a
/// child misrouted to the parent's std handles
/// (`STARTF_USESTDHANDLES`) surfaces here as an empty sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_output_reaches_the_sink() {
    let (collected, sink) = collector();
    let session = spawn(shell_echo("felis-sink"), small_size(), sink).expect("spawn");
    let PtyParts { mut eof, child, .. } = session;

    let text = sink_until(&collected, "felis-sink").await;
    assert!(text.contains("felis-sink"), "sink missed output: {text:?}");

    // The Unix master reports EOF when the child exits; `ConPTY`
    // closes the output pipe only at `ClosePseudoConsole`, so an
    // idle timeout is a pass too.
    if let Ok(ended) = tokio::time::timeout(Duration::from_secs(2), eof.eof()).await {
        ended.expect("the end of output is not an error");
    }

    let waited = child
        .wait_timeout(Duration::from_millis(1000))
        .await
        .expect("wait");
    assert!(waited.is_some(), "child did not exit");
}

/// The lifecycle EOF is ordered after the sink's last call. The sink
/// sleeps long enough that a "signal EOF, then drain" reader loses
/// the race every time. Unix-only: `ConPTY` never EOFs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eof_is_reported_only_after_the_sink_drained() {
    let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink_side = Arc::clone(&collected);
    let session = spawn(
        shell_echo("felis-drain"),
        small_size(),
        Box::new(move |bytes| {
            thread::sleep(Duration::from_millis(250));
            sink_side
                .lock()
                .expect("sink collector lock")
                .extend_from_slice(bytes);
        }),
    )
    .expect("spawn");
    let PtyParts { mut eof, .. } = session;

    tokio::time::timeout(Duration::from_secs(5), eof.eof())
        .await
        .expect("the output did not end")
        .expect("the end of output is not an error");

    let text = collected_text(&collected);
    assert!(
        text.contains("felis-drain"),
        "EOF outran the sink's last call: {text:?}"
    );
}

/// A panicking sink must not strand the reader thread: a reader
/// parked against the full swap buffer is released and the
/// [`PtyEof`] still resolves. The child floods well
/// past `READ_PENDING_CAP` so the parked state is reachable.
/// Unix-only: `ConPTY` never EOFs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_sink_releases_reader_and_reports_eof() {
    let mut cmd = Command::new("/bin/sh");
    // ~4 MiB of output: enough to fill READ_PENDING_CAP (1 MiB)
    // after the sink dies on its first bytes.
    cmd.args(["-c", "dd if=/dev/zero bs=65536 count=64 2>/dev/null"]);
    let session = spawn(
        cmd,
        small_size(),
        Box::new(|_bytes| panic!("sink dies on first swap (deliberate, test-only)")),
    )
    .expect("spawn");
    let PtyParts { mut eof, .. } = session;

    tokio::time::timeout(Duration::from_secs(10), eof.eof())
        .await
        .expect("the output must end after the sink panics, not park forever")
        .expect("clean EOF");
}

/// Cooked-mode echo is a Unix line-discipline contract; `ConPTY`
/// input handling is not the same mechanism.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_round_trips_via_cat() {
    use tokio::io::AsyncWriteExt;

    let mut cmd = Command::new("/bin/sh");
    // `head -c N` exits on its own; `cat` would hang if the test raced.
    cmd.args(["-c", "head -c 5"]);
    cmd.env_clear();
    cmd.env("PATH", fixture_path());

    let (collected, sink) = collector();
    let session = spawn(cmd, small_size(), sink).expect("spawn");
    let PtyParts { mut writer, .. } = session;

    writer.write_all(b"hello").await.expect("write");
    writer.flush().await.expect("flush");

    let text = sink_until(&collected, "hello").await;
    assert!(text.contains("hello"), "input was not echoed: {text:?}");
}

/// The sink stays shut until the child is reaped, so its output is
/// still queued or unparsed when the probe first runs. The holder keeps
/// the terminal side open past the exit, where no EOF orders anything.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_drained_after_the_reap_means_the_sink_has_all_the_childs_output() {
    for holder in ["", "( read -r _hold <&1 ) &"] {
        for doublings in [0_u32, 6, 12, 15] {
            let expected = 16_usize << doublings;
            let open = Arc::new((Mutex::new(false), Condvar::new()));
            let collected = Arc::new(Mutex::new(Vec::<u8>::new()));
            let sink: ByteSink = Box::new({
                let open = Arc::clone(&open);
                let collected = Arc::clone(&collected);
                move |bytes: &[u8]| {
                    let (lock, cv) = &*open;
                    let mut shut = lock.lock().unwrap();
                    while !*shut {
                        shut = cv.wait(shut).unwrap();
                    }
                    collected.lock().unwrap().extend_from_slice(bytes);
                }
            });
            let mut cmd = Command::new("/bin/sh");
            cmd.args([
                "-c",
                &format!(
                    "{holder} s=xxxxxxxxxxxxxxxx; i=0; \
                     while [ $i -lt {doublings} ]; do s=$s$s; i=$((i+1)); done; \
                     printf %s \"$s\""
                ),
            ]);
            let session = spawn(cmd, small_size(), sink).expect("spawn");
            let PtyParts { child, resizer, .. } = session;
            let pid = child
                .child
                .lock()
                .unwrap()
                .process_id()
                .expect("a spawned child has a pid");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while child.try_wait().expect("try_wait").is_none() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the child never exited"
                );
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(
                !resizer.output_drained(),
                "holder={holder:?} {expected}: the shut sink has parsed nothing"
            );

            *open.0.lock().unwrap() = true;
            open.1.notify_all();
            while !resizer.output_drained() {
                assert!(tokio::time::Instant::now() < deadline, "never drained");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            assert!(
                *collected.lock().unwrap() == vec![b'x'; expected],
                "holder={holder:?} {expected}: the sink has all of it"
            );
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                rustix::process::kill_process_group(pid, rustix::process::Signal::HUP)
                    .unwrap_or_default();
            }
        }
    }
}

fn test_eof() -> (Arc<Mutex<LifecycleState>>, PtyEof) {
    let lifecycle = Arc::new(Mutex::new(LifecycleState {
        eof: false,
        err: None,
        waker: None,
    }));
    let placeholder = thread::spawn(|| {});
    let eof = PtyEof {
        lifecycle: Arc::clone(&lifecycle),
        _reader_thread: placeholder,
    };
    (lifecycle, eof)
}

/// A read error reaches the consumer once, then a clean EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_error_surfaces_once_then_clean_eof() {
    let (lifecycle, mut eof) = test_eof();
    {
        let mut state = lifecycle.lock().unwrap();
        state.eof = true;
        state.err = Some(io::Error::other("pty gone"));
    }

    assert!(eof.eof().await.is_err(), "error first");
    eof.eof().await.expect("clean EOF");
}
