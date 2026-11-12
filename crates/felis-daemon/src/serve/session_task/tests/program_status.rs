use super::*;

/// The shared meta follows every `OSC 2` / `OSC 7`, not just the
/// first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meta_follows_every_title_and_cwd_change() {
    let mut task = SessionTask::for_tests(session_with(
        "printf '\\033]2;first\\007\\033]7;file:///first\\007'; read _x; \
             printf '\\033]2;second\\007\\033]7;file:///second\\007'; read _y",
    ));
    let (sub, _rx) = attach_sub(&mut task, false);

    wait_for_grid_title(&task, "first").await;
    task.after_drive();
    {
        let meta = task.meta.lock().unwrap();
        assert_eq!(meta.title.as_deref(), Some("first"));
        assert_eq!(meta.cwd.as_deref(), Some("file:///first"));
    }

    let end = task.handle_input(sub, InputMsg::KeyBytes(b"\n".to_vec()));
    assert!(end.is_none());
    wait_for_grid_title(&task, "second").await;
    task.after_drive();
    let meta = task.meta.lock().unwrap();
    assert_eq!(meta.title.as_deref(), Some("second"));
    assert_eq!(meta.cwd.as_deref(), Some("file:///second"));
}

async fn wait_for_grid_title(task: &SessionTask, expect: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let seen = task.session.lock_core().grid.title().map(str::to_owned);
        if seen.as_deref() == Some(expect) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "grid title never became {expect:?} (last: {seen:?})"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn listed_status(task: &SessionTask) -> Vec<(String, String)> {
    task.meta
        .lock()
        .unwrap()
        .program_status
        .iter()
        .map(|r| (r.id.clone(), r.state.clone()))
        .collect()
}

fn status(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(id, state)| ((*id).to_owned(), (*state).to_owned()))
        .collect()
}

/// Records reach the roster on the drive that applies them, and a
/// detach leaves them in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_status_reports_reach_the_roster_and_survive_a_detach() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (win, _rx) = attach_sub(&mut task, false);
    write_to_grid(
        &task,
        b"\x1b]7501;state=working:app=cargo\x1b\\\x1b]7501;id=b/c:state=blocked:kind=question\x07",
    );
    task.drive_cycle().expect("drive");
    let records = task.meta.lock().unwrap().program_status.clone();
    assert_eq!(records.len(), 2);
    assert_eq!(
        (
            records[0].id.as_str(),
            records[0].state.as_str(),
            records[0].app.as_deref()
        ),
        ("", "working", Some("cargo"))
    );
    assert_eq!(
        (records[1].id.as_str(), records[1].kind.as_deref()),
        ("b/c", Some("question"))
    );

    task.remove_sub(win);
    task.refresh_meta();
    assert_eq!(task.meta.lock().unwrap().program_status, records);
}

/// One burst, applied in byte order: the prompt start expires what
/// came before it and spares what came after; a continuation
/// prompt (`k=s`) expires nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_start_expires_transient_records_in_byte_order() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    write_to_grid(
        &task,
        concat!(
            "\x1b]7501;id=w:state=working\x07",
            "\x1b]7501;id=b:state=blocked\x07",
            "\x1b]7501;id=d:state=done\x07",
            "\x1b]7501;id=i:state=idle\x07",
            "\x1b]7501;id=e:state=error\x07",
            "\x1b]133;A\x07",
            "\x1b]7501;id=after:state=working\x07",
            "\x1b]133;A;k=s\x07",
        )
        .as_bytes(),
    );
    task.drive_cycle().expect("drive");
    assert_eq!(
        listed_status(&task),
        status(&[
            ("after", "working"),
            ("d", "done"),
            ("e", "error"),
            ("i", "idle")
        ])
    );
}

/// Records belong to the session, not a screen: switching screens
/// and DECSTR keep them, and a prompt on the alternate screen
/// still expires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_switches_and_decstr_keep_records_and_an_alternate_prompt_expires() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    write_to_grid(
        &task,
        b"\x1b]7501;id=w:state=working\x07\x1b]7501;id=d:state=done\x07\x1b[?1049h\x1b[!p\x1b[?1049l",
    );
    task.drive_cycle().expect("drive");
    assert_eq!(
        listed_status(&task),
        status(&[("d", "done"), ("w", "working")])
    );

    write_to_grid(&task, b"\x1b[?1049h\x1b]133;A\x07");
    task.drive_cycle().expect("drive");
    assert_eq!(listed_status(&task), status(&[("d", "done")]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ris_clears_records_and_a_report_after_it_survives() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    write_to_grid(
        &task,
        b"\x1b]7501;id=d:state=done\x07\x1bc\x1b]7501;id=new:state=idle\x07",
    );
    task.drive_cycle().expect("drive");
    assert_eq!(listed_status(&task), status(&[("new", "idle")]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clear_report_removes_the_subtree_from_the_roster() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    write_to_grid(
        &task,
        b"\x1b]7501;id=a:state=done\x07\x1b]7501;id=a/b:state=done\x07\x1b]7501;id=ab:state=done\x07\x1b]7501;id=a:state=clear\x07",
    );
    task.drive_cycle().expect("drive");
    assert_eq!(listed_status(&task), status(&[("ab", "done")]));
}

fn listed_progress(task: &SessionTask) -> felis_grid::progress::ProgressReport {
    task.meta.lock().unwrap().terminal_progress
}

const fn progress(
    state: felis_grid::progress::ProgressState,
    value: Option<u8>,
) -> felis_grid::progress::Progress {
    felis_grid::progress::Progress { state, value }
}

/// Each report replaces the last whole, and the session keeps it
/// across screen switches, DECSTR, a continuation prompt and a
/// detach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_progress_reaches_the_roster_and_is_replaced_whole() {
    use felis_grid::progress::ProgressState;
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let (win, _rx) = attach_sub(&mut task, false);
    write_to_grid(&task, b"\x1b]9;4;2;71\x1b\\");
    task.drive_cycle().expect("drive");
    assert_eq!(
        listed_progress(&task),
        Some(progress(ProgressState::Error, Some(71)))
    );

    write_to_grid(
        &task,
        b"\x1b]9;4;4\x07\x1b[?1049h\x1b[!p\x1b]133;A;k=s\x07\x1b[?1049l",
    );
    task.drive_cycle().expect("drive");
    assert_eq!(
        listed_progress(&task),
        Some(progress(ProgressState::Paused, None))
    );

    task.remove_sub(win);
    task.refresh_meta();
    assert_eq!(
        listed_progress(&task),
        Some(progress(ProgressState::Paused, None))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clear_ris_or_primary_prompt_start_clears_terminal_progress() {
    for clear in [&b"\x1b]9;4;0\x07"[..], b"\x1bc", b"\x1b]133;A\x07"] {
        let mut task = SessionTask::for_tests(session_with("read _x"));
        write_to_grid(&task, &[&b"\x1b]9;4;1;50\x07"[..], clear].concat());
        task.drive_cycle().expect("drive");
        assert_eq!(listed_progress(&task), None, "{clear:?}");
    }
}

/// The detection reply is written to the child with nobody
/// attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_program_status_query_is_answered_on_the_pty() {
    let tmp = tempfile::tempdir().unwrap();
    let reply = tmp.path().join("reply");
    // The caller's PATH, not `session_with`'s: `stty` and `head` are
    // external.
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!(
        "stty raw -echo; printf ready; head -c 10 > '{}'; sleep 30",
        reply.display()
    ));
    let mut task = SessionTask::for_tests(Session::from_spawned(
        crate::SpawnedPty::spawn(cmd).expect("spawn /bin/sh"),
    ));
    wait_for_grid_text(&task.session.core, "ready").await;
    write_to_grid(&task, b"\x1b]7501;?\x1b\\");
    task.drive_cycle().expect("drive");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let got = loop {
        let got = std::fs::read(&reply).unwrap_or_default();
        if got.len() >= 10 || tokio::time::Instant::now() >= deadline {
            break got;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(got, b"\x1b]7501;?\x1b\\");
}

/// A response the PTY refuses ends the drain's writes, not its state
/// effects: a report queued behind it still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_reply_does_not_cost_the_reports_behind_it() {
    use tokio::io::AsyncWriteExt as _;

    let mut task = SessionTask::for_tests(session_with("read _x"));
    task.session
        .writer
        .shutdown()
        .await
        .expect("writer shuts down");
    write_to_grid(&task, b"\x1b]7501;?\x07\x1b[c\x1b]7501;state=done\x07");
    assert!(
        task.drain_effects().is_err(),
        "the refused reply is reported"
    );
    assert_eq!(task.session.program_status.len(), 1);
    task.refresh_meta();
    assert_eq!(listed_status(&task), status(&[("", "done")]));
}

fn row_with_records(records: usize, msg_bytes: usize) -> SessionInfo {
    SessionInfo {
        id: 1,
        dims: GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        idle_seconds: None,
        tags: Vec::new(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
        spawn_cwd: None,
        terminal_progress: None,
        program_status: (0..records)
            .map(|i| felis_protocol::messages::ProgramStatusRecord {
                id: format!("r{i}"),
                state: "working".into(),
                kind: None,
                progress: None,
                app: None,
                title: None,
                msg: Some("m".repeat(msg_bytes)),
            })
            .collect(),
    }
}

/// A roster reply is charged at least its encoded size against the
/// subscriber backlog, so a peer that keeps listing while reading
/// slowly cannot queue unbounded bytes under a flat charge.
#[test]
fn roster_replies_are_charged_at_least_their_encoded_size() {
    use felis_protocol::messages::{InfoOutcome, SpawnOutcome};

    // Every string at its bound, with and without records: the
    // record allowances must not be what covers the row's own fields.
    let mut full = row_with_records(0, 0);
    full.tags = (0..felis_protocol::messages::MAX_SESSION_TAGS)
        .map(|i| {
            format!(
                "{i:0>width$}",
                width = felis_protocol::messages::MAX_TAG_BYTES
            )
        })
        .collect();
    full.title = Some("t".repeat(4096));
    full.cwd = Some("/".repeat(4096));
    full.foreground = Some("f".repeat(4096));
    full.idle_seconds = Some(u64::MAX);
    full.last_exit_code = Some(u32::MAX);
    let mut heavy = full.clone();
    heavy.program_status = row_with_records(64, 2048).program_status;
    for record in &mut heavy.program_status {
        record.kind = Some("permission".into());
        record.progress = Some(100);
        record.app = Some("a".repeat(32));
        record.title = Some("t".repeat(192));
    }
    let correlation = Correlation::Request(RequestId::new(1).unwrap());
    for row in [full, heavy] {
        let events = [
            OutEvent::Ops {
                msg: Box::new(OpsToClientMsg::Listed {
                    sessions: vec![row.clone(); 16],
                }),
                correlation,
            },
            OutEvent::Ops {
                msg: Box::new(OpsToClientMsg::InfoReply {
                    outcome: InfoOutcome::Found {
                        session: Box::new(row.clone()),
                        short_id: "1".into(),
                    },
                }),
                correlation,
            },
            OutEvent::Ops {
                msg: Box::new(OpsToClientMsg::Spawned {
                    outcome: SpawnOutcome::Ok {
                        info: Box::new(row),
                    },
                }),
                correlation,
            },
        ];
        for ev in events {
            let encoded = streaming::body_for(&ev).expect("sendable").body_len();
            assert!(
                ev.approx_wire_len() >= encoded,
                "{} charged for a {encoded}-byte body: {ev:?}",
                ev.approx_wire_len()
            );
        }
    }
}
