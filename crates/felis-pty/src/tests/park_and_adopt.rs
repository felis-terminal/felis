use super::*;

#[cfg(unix)]
fn cat() -> Command {
    Command::new("cat")
}

/// A child that reads no input, in raw mode so the tty queues input
/// instead of discarding past a canonical line limit.
#[cfg(unix)]
fn raw_non_reader() -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "stty raw -echo; printf ready; exec sleep 1000"]);
    cmd
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_completes_on_an_idle_session_and_resume_keeps_it_working() {
    let (collected, sink) = collector();
    let session = spawn(cat(), small_size(), sink).expect("spawn");
    let quiescer = session.quiescer.clone();
    let PtyParts {
        mut writer, child, ..
    } = session;

    let parked = quiescer
        .park(Duration::from_secs(2))
        .expect("park an idle session");
    assert_eq!(parked, Parked::default(), "an idle session holds no input");
    quiescer.resume().expect("resume");

    tokio::io::AsyncWriteExt::write_all(&mut writer, b"after-resume\n")
        .await
        .expect("write");
    assert!(
        sink_until(&collected, "after-resume")
            .await
            .contains("after-resume")
    );
    child.kill().expect("kill");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_hands_back_input_a_non_reading_child_has_not_taken() {
    let (collected, sink) = collector();
    let session = spawn(raw_non_reader(), small_size(), sink).expect("spawn");
    let quiescer = session.quiescer.clone();
    let PtyParts { writer, child, .. } = session;
    assert!(sink_until(&collected, "ready").await.contains("ready"));

    let payload: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    assert!(
        writer
            .write_owned(payload.clone(), None)
            .expect("queue")
            .queued()
    );
    // The tty queue fills well short of 64 KiB, leaving the writer
    // blocked on a child that never reads.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(writer.pending_bytes() > 0, "the writer is blocked");

    let parked = quiescer
        .park(Duration::from_secs(2))
        .expect("park a blocked writer");
    assert_ne!(parked.unwritten, Vec::<u8>::new());
    assert!(
        parked.unwritten.len() < payload.len(),
        "the tty took a prefix"
    );
    assert!(
        payload.ends_with(&parked.unwritten),
        "the unwritten bytes are exactly the payload's untaken tail",
    );

    let more = b"queued while parked".to_vec();
    assert!(
        writer
            .write_owned(more.clone(), None)
            .expect("queue")
            .queued()
    );
    let again = quiescer.park(Duration::from_secs(2)).expect("park again");
    assert_eq!(
        again.unwritten,
        [parked.unwritten.as_slice(), more.as_slice()].concat(),
        "input queued while parked is handed back after what was already held",
    );

    quiescer.resume().expect("resume");
    child.kill().expect("kill");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_session_reaches_the_same_child_through_the_same_master() {
    let (_collected, sink) = collector();
    let session = spawn(cat(), small_size(), sink).expect("spawn");
    let quiescer = session.quiescer.clone();
    let PtyParts { child, resizer, .. } = session;
    quiescer.park(Duration::from_secs(2)).expect("park");

    let master = resizer
        .master
        .as_fd()
        .try_clone_to_owned()
        .expect("dup master");
    let pid = child
        .child
        .lock()
        .unwrap()
        .process_id()
        .expect("live child");
    let (adopted_collected, adopted_sink) = collector();
    let adopted = handover::Adopted::inherited(master, pid, None)
        .and_then(|adopted| adopted.start(adopted_sink))
        .expect("adopt");
    let PtyParts {
        mut writer,
        child: adopted_child,
        ..
    } = adopted;

    tokio::io::AsyncWriteExt::write_all(&mut writer, b"through-adoption\n")
        .await
        .expect("write");
    assert!(
        sink_until(&adopted_collected, "through-adoption")
            .await
            .contains("through-adoption")
    );
    assert!(
        adopted_child.kill().expect("kill"),
        "the adopted child is live"
    );
    assert!(
        wait_for_exit(&adopted_child).await,
        "the adopted child is reaped by pid"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_child_already_reaped_is_never_signaled() {
    let (_collected, sink) = collector();
    let session = spawn(cat(), small_size(), sink).expect("spawn");
    let PtyParts { child, resizer, .. } = session;
    let master = resizer
        .master
        .as_fd()
        .try_clone_to_owned()
        .expect("dup master");

    // This test process's own pid: a signal sent to it would end the test run.
    let own_pid = i32::try_from(std::process::id()).expect("pid fits i32");
    let (_adopted_collected, adopted_sink) = collector();
    let adopted = handover::Adopted::inherited(master, own_pid, Some(0))
        .and_then(|adopted| adopted.start(adopted_sink))
        .expect("adopt");
    let PtyParts {
        child: adopted_child,
        ..
    } = adopted;

    assert!(
        !adopted_child.hangup().expect("hangup"),
        "nothing is sent to a reaped pid"
    );
    assert!(
        !adopted_child.kill().expect("kill"),
        "nothing is sent to a reaped pid"
    );
    assert!(adopted_child.try_wait().expect("try_wait").is_some());
    child.kill().expect("kill");
}

#[cfg(windows)]
fn spawn_bundled(command: &Command, sink: ByteSink) -> PtyParts {
    let (master, child) =
        platform::open_and_spawn_on(platform::Host::Bundled, command, small_size())
            .expect("spawn on the bundled host");
    session_over(master, child, sink).expect("start the session")
}

/// `cmd` waiting at its prompt: the reader is blocked in a `ReadFile`
/// on a pipe that stays silent.
#[cfg(windows)]
fn idle_cmd() -> Command {
    let mut cmd = Command::new(comspec());
    cmd.args(["/q", "/k", "echo ready"]);
    cmd
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_wakes_a_reader_blocked_on_an_idle_console_and_resume_keeps_it_working() {
    let (collected, sink) = collector();
    let session = spawn_bundled(&idle_cmd(), sink);
    let quiescer = session.quiescer.clone();
    let PtyParts { writer, child, .. } = session;
    assert!(sink_until(&collected, "ready").await.contains("ready"));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let parked = quiescer
        .park(Duration::from_secs(2))
        .expect("park an idle session");
    assert_eq!(parked, Parked::default(), "an idle session holds no input");
    quiescer.resume().expect("resume");

    assert!(
        writer
            .write_owned(b"echo after-resume\r".to_vec(), None)
            .expect("queue")
            .queued()
    );
    assert!(
        sink_until(&collected, "after-resume")
            .await
            .contains("after-resume")
    );
    child.kill().expect("kill");
}

/// Parking again and again while the child writes loses nothing and
/// repeats nothing: a cancelled read takes no bytes from the pipe.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_parks_while_the_child_writes_lose_and_repeat_no_line() {
    const LINES: u32 = 400;
    let (collected, sink) = collector();
    let mut cmd = Command::new(comspec());
    cmd.args([
        "/q",
        "/c",
        &format!("for /l %i in (1,1,{LINES}) do @echo line-%i-end"),
    ]);
    let session = spawn_bundled(&cmd, sink);
    let quiescer = session.quiescer.clone();
    let PtyParts { child, .. } = session;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while child.try_wait().expect("try_wait").is_none() {
        assert!(std::time::Instant::now() < deadline, "the child finishes");
        quiescer.park(Duration::from_secs(2)).expect("park");
        tokio::time::sleep(Duration::from_millis(3)).await;
        quiescer.resume().expect("resume");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let text = sink_until(&collected, &format!("line-{LINES}-end")).await;
    let mut next = 1;
    for found in text.split("line-").skip(1) {
        let Some((n, _)) = found.split_once("-end") else {
            continue;
        };
        let n: u32 = n.parse().expect("a line number");
        assert_eq!(n, next, "lines arrive once each, in order");
        next += 1;
    }
    assert_eq!(next, LINES + 1, "every line arrived");
}

/// A child that never reads its console input does not keep the
/// writer from parking: the console host drains its input pipe.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_completes_while_a_child_reads_no_input() {
    let (collected, sink) = collector();
    let mut cmd = Command::new(comspec());
    cmd.args(["/q", "/c", "echo ready & ping -n 600 127.0.0.1 >nul"]);
    let session = spawn_bundled(&cmd, sink);
    let quiescer = session.quiescer.clone();
    let PtyParts { writer, child, .. } = session;
    assert!(sink_until(&collected, "ready").await.contains("ready"));

    let payload: Vec<u8> = (0..1024 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    assert!(
        writer
            .write_owned(payload.clone(), Some(Box::new(())))
            .expect("queue")
            .queued()
    );
    let parked = quiescer
        .park(Duration::from_secs(5))
        .expect("park with input queued");
    assert!(
        payload.ends_with(&parked.unwritten),
        "the unwritten bytes are the payload's untaken tail"
    );
    quiescer.resume().expect("resume");
    child.kill().expect("kill");
}

/// A session handed over within one process: the adopted copy reads
/// the same console and writes to the same child.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_console_reaches_the_same_child() {
    let (_collected, sink) = collector();
    let session = spawn_bundled(&idle_cmd(), sink);
    let quiescer = session.quiescer.clone();
    let PtyParts { child, resizer, .. } = session;
    tokio::time::sleep(Duration::from_millis(500)).await;
    quiescer.park(Duration::from_secs(2)).expect("park");

    let handover = handover::Handover::of(&resizer, &child).expect("take the handles");
    let values = handover
        .values_in(&handover::Process::current().expect("this process"))
        .expect("duplicate the handles");
    let adopted = handover::prepare_adopt(&values).expect("adopt");
    let (adopted_collected, adopted_sink) = collector();
    let adopted = adopted.start(adopted_sink).expect("start");
    let PtyParts {
        writer,
        child: adopted_child,
        resizer: adopted_resizer,
        ..
    } = adopted;

    adopted_resizer
        .resize(Size {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("the packed pseudoconsole resizes");
    assert!(
        writer
            .write_owned(b"echo through-adoption\r".to_vec(), None)
            .expect("queue")
            .queued()
    );
    assert!(
        sink_until(&adopted_collected, "through-adoption")
            .await
            .contains("through-adoption")
    );
    assert!(
        adopted_child.kill().expect("kill"),
        "the adopted child is live"
    );
    assert!(
        wait_for_exit(&child).await,
        "the original handle sees it end"
    );
}
