use super::*;

#[cfg(unix)]
fn blocks_forever() -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "sleep 60"]);
    cmd
}

#[cfg(windows)]
fn blocks_forever() -> Command {
    let mut cmd = Command::new(comspec());
    cmd.args(["/c", "pause"]);
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_on_a_live_child_gives_up_at_the_timeout() {
    let (_collected, sink) = collector();
    // Held: on Windows dropping the resizer closes the pseudoconsole,
    // which ends the child before the deadline.
    let PtyParts {
        child,
        resizer: _resizer,
        ..
    } = spawn(blocks_forever(), small_size(), sink).expect("spawn");
    let started = tokio::time::Instant::now();
    let waited = child
        .wait_timeout(Duration::from_millis(200))
        .await
        .expect("wait");
    assert!(waited.is_none(), "a live child is not reported exited");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(child.kill().expect("kill"), "the child was still live");
    assert!(wait_for_exit(&child).await, "the killed child is reaped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killing_the_child_reaps_it() {
    let (_collected, sink) = collector();
    let session = spawn(blocks_forever(), small_size(), sink).expect("spawn");
    let PtyParts {
        eof,
        writer,
        child,
        resizer,
        ..
    } = session;
    drop(eof);
    drop(writer);
    drop(resizer);
    // Dropping the halves is not a teardown
    // (see `hangup_ends_a_child_that_outlives_the_master`).
    assert!(child.kill().expect("kill"), "a live child is signaled");

    let waited = child
        .wait_timeout(Duration::from_millis(5000))
        .await
        .expect("wait");
    assert!(waited.is_some(), "kill did not reap child");
    assert!(
        !child.kill().expect("kill"),
        "a reaped child must not be signaled again — that pid may be someone else's by now",
    );
}

/// The child is its own session leader (`setsid` + `TIOCSCTTY` in
/// `pre_exec`), so the reported foreground pgid is its pid.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_pgrp_reports_a_live_child() {
    let mut cmd = Command::new("/bin/sh");
    // No API hands the caller the spawned pid, so the child reports
    // `$$`. Marker last so waiting on it also guarantees the digits
    // arrived.
    cmd.args(["-c", "printf '%d up\\n' $$; sleep 60"]);
    let (collected, sink) = collector();
    let session = spawn(cmd, small_size(), sink).expect("spawn");
    let PtyParts { child, resizer, .. } = session;
    // Wait for the marker, not just for `spawn` to return: the
    // `pre_exec` handshake that installs the foreground pgrp can
    // still be pending when the parent resumes.
    let seen = sink_until(&collected, "up").await;
    let reported: i32 = seen
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("child never reported its pid: {seen:?}"));

    assert_eq!(
        resizer.foreground_pgrp(),
        Some(reported),
        "the session leader's pgid is its own pid",
    );

    child.kill().expect("kill");
}

/// A terminal whose session leader has exited reports "nothing in
/// the foreground", never panics (the `tcgetpgrp` why-not in
/// `unix::Master::foreground_pgrp`).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_pgrp_survives_a_dead_session_leader() {
    let (collected, sink) = collector();
    let session = spawn(shell_echo("gone"), small_size(), sink).expect("spawn");
    let PtyParts { child, resizer, .. } = session;
    sink_until(&collected, "gone").await;
    assert!(wait_for_exit(&child).await, "echo child never exited");

    // The platforms disagree on whether a reaped leader leaves a
    // stale pgid or clears it; a reported pgid must still be usable.
    if let Some(pgrp) = resizer.foreground_pgrp() {
        assert!(pgrp > 0, "a reported pgid must be positive, got {pgrp}");
    }
}

/// Every master half the caller owns can be gone and the child
/// still runs; only a signal ends it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hangup_ends_a_child_that_outlives_the_master() {
    let (_collected, sink) = collector();
    let session = spawn(blocks_forever(), small_size(), sink).expect("spawn");
    let PtyParts {
        eof,
        writer,
        child,
        resizer,
        ..
    } = session;
    drop(eof);
    drop(writer);
    drop(resizer);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "dropping the master halves must be shown not to end the child",
    );

    assert!(child.hangup().expect("hangup"), "a live child is signaled");
    let waited = child
        .wait_timeout(Duration::from_millis(5000))
        .await
        .expect("wait");
    assert!(waited.is_some(), "the hung-up child never exited");
}
