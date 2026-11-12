use super::*;

/// A sink that parks inside `write` until the test opens it, so the
/// pending gauge is exercised against a controlled stall rather than
/// a platform-dependent pipe buffer.
#[derive(Clone)]
struct GatedSink {
    inner: Arc<GateInner>,
}

struct GateInner {
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    open: bool,
    failing: bool,
    taken: usize,
}

impl GatedSink {
    fn new() -> Self {
        Self {
            inner: Arc::new(GateInner {
                state: Mutex::new(GateState {
                    open: false,
                    failing: false,
                    taken: 0,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    fn open(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.open = true;
        self.inner.changed.notify_all();
    }

    /// Re-gate a sink an earlier `open` released, so a test can hold
    /// a second batch of writes unwritten.
    fn close(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.open = false;
    }

    fn fail(&self) {
        let mut state = self.inner.state.lock().unwrap();
        state.failing = true;
        self.inner.changed.notify_all();
    }

    fn taken(&self) -> usize {
        self.inner.state.lock().unwrap().taken
    }
}

impl Write for GatedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.inner.state.lock().unwrap();
        while !state.open && !state.failing {
            state = self.inner.changed.wait(state).unwrap();
        }
        if state.failing {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"));
        }
        state.taken += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A child that stopped reading its stdin must park the writer, not
/// grow the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_park_once_the_pending_cap_is_reached() {
    use tokio::io::AsyncWriteExt;

    let sink = GatedSink::new();
    let mut writer = spawn_writer(sink.clone()).expect("writer");

    writer
        .write_all(&vec![7u8; PTY_WRITE_PENDING_CAP])
        .await
        .expect("the first write fills the gauge without waiting");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), writer.write_all(b"x"))
            .await
            .is_err(),
        "a write past the cap must wait for the sink, not queue",
    );

    sink.open();
    tokio::time::timeout(Duration::from_secs(5), writer.write_all(b"x"))
        .await
        .expect("the drained sink frees space")
        .expect("write");
    tokio::time::timeout(Duration::from_secs(5), writer.flush())
        .await
        .expect("flush completes once the sink took everything")
        .expect("flush");
    assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP + 1);
}

/// "Flushed" must mean the OS writer took the bytes; the callers
/// that report a write failure have nothing else to wait on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_waits_for_the_sink_not_the_enqueue() {
    use tokio::io::AsyncWriteExt;

    let sink = GatedSink::new();
    let mut writer = spawn_writer(sink.clone()).expect("writer");
    writer.write_all(b"hello").await.expect("write");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), writer.flush())
            .await
            .is_err(),
        "flush must not complete while the bytes are still queued",
    );

    sink.open();
    tokio::time::timeout(Duration::from_secs(5), writer.flush())
        .await
        .expect("flush completes after delivery")
        .expect("flush");
    assert_eq!(sink.taken(), 5);
}

/// The failure belongs to the flush: the write that queued the bytes
/// had nothing to report yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_reports_a_sink_failure() {
    use tokio::io::AsyncWriteExt;

    let sink = GatedSink::new();
    let mut writer = spawn_writer(sink.clone()).expect("writer");
    writer.write_all(b"hello").await.expect("the write queues");
    sink.fail();

    let err = tokio::time::timeout(Duration::from_secs(5), writer.flush())
        .await
        .expect("flush resolves once the write failed")
        .expect_err("the sink failure surfaces");
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
}

/// Shutdown drains what is queued and then closes: a write after it
/// must fail rather than queue bytes for a thread that is leaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_then_refuses_further_writes() {
    use tokio::io::AsyncWriteExt;

    let sink = GatedSink::new();
    let mut writer = spawn_writer(sink.clone()).expect("writer");
    writer.write_all(b"hello").await.expect("write");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), writer.shutdown())
            .await
            .is_err(),
        "shutdown must not complete while the bytes are still queued",
    );

    sink.open();
    tokio::time::timeout(Duration::from_secs(5), writer.shutdown())
        .await
        .expect("shutdown completes after delivery")
        .expect("shutdown");
    assert_eq!(sink.taken(), 5);

    let err = writer
        .write_all(b"after")
        .await
        .expect_err("a write past shutdown has nowhere to go");
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(sink.taken(), 5, "the refused write reached no sink");
    assert_eq!(
        writer.pending_bytes(),
        0,
        "the refused write leaves no bytes a later flush would wait on",
    );
}

/// The drop notice is one per saturation episode, not one per
/// dropped reply: a child that queries in a loop while refusing to
/// read its stdin must not be able to write the daemon's log. A
/// gauge that drains opens the next episode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drop_notice_is_claimed_once_per_saturation_episode() {
    let sink = GatedSink::new();
    let writer = spawn_writer(sink.clone()).expect("writer");

    assert!(
        writer
            .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
            .expect("enqueue")
            .queued(),
        "an empty gauge takes the write whole",
    );
    let mut reported = 0;
    for _ in 0..3 {
        match writer.write_owned(b"reply".to_vec(), None).expect("gauge") {
            WriteOutcome::Dropped { report } => reported += usize::from(report),
            WriteOutcome::Queued => panic!("a full unreserved gauge takes nothing"),
        }
    }
    assert_eq!(
        reported, 1,
        "one episode reports once, however many replies it drops",
    );

    sink.open();
    for _ in 0..100 {
        if writer.pending_bytes() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(writer.pending_bytes(), 0, "the gauge drained");

    // Re-gated before the refill, so the second episode is reached
    // by the cap rather than by racing the writer thread.
    sink.close();
    assert!(
        writer
            .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
            .expect("enqueue")
            .queued(),
        "the drained gauge takes a write again",
    );
    assert_eq!(
        writer.write_owned(b"reply".to_vec(), None).expect("gauge"),
        WriteOutcome::Dropped { report: true },
        "a gauge that emptied in between opens a new episode",
    );

    sink.open();
}

/// The daemon's non-blocking path: reserved bytes always enqueue,
/// unreserved ones are dropped rather than queued past the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_owned_drops_unreserved_bytes_past_the_cap() {
    let sink = GatedSink::new();
    let writer = spawn_writer(sink.clone()).expect("writer");

    assert!(
        writer
            .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP], None)
            .expect("enqueue")
            .queued(),
        "an empty gauge takes the write whole",
    );
    assert!(
        !writer
            .write_owned(b"reply".to_vec(), None)
            .expect("gauge")
            .queued(),
        "an unreserved write past the cap is dropped",
    );
    let reservation: WriteReservation = Box::new(());
    assert!(
        writer
            .write_owned(b"typed".to_vec(), Some(reservation))
            .expect("enqueue")
            .queued(),
        "reserved bytes were admitted upstream and always enqueue",
    );
    assert_eq!(writer.pending_bytes(), PTY_WRITE_PENDING_CAP as u64 + 5);

    sink.open();
    for _ in 0..100 {
        if writer.pending_bytes() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP + 5);
}

/// A paste the daemon admitted against its own budget must not
/// starve the session's replies: a child that is merely behind on a
/// 16 MiB paste still has to receive its `DA` answer and the mouse
/// reports of the window driving it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_bytes_do_not_close_the_unreserved_gauge() {
    let sink = GatedSink::new();
    let writer = spawn_writer(sink.clone()).expect("writer");

    let reservation: WriteReservation = Box::new(());
    assert!(
        writer
            .write_owned(vec![0u8; PTY_WRITE_PENDING_CAP * 4], Some(reservation))
            .expect("enqueue")
            .queued(),
        "reserved bytes always enqueue",
    );
    assert!(
        writer
            .write_owned(b"reply".to_vec(), None)
            .expect("gauge")
            .queued(),
        "an unreserved reply behind a reserved paste still enqueues",
    );

    sink.open();
    for _ in 0..100 {
        if writer.pending_bytes() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(sink.taken(), PTY_WRITE_PENDING_CAP * 4 + 5);
}
