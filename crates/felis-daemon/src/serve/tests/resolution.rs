use super::*;

async fn attach_failure<R>(reader: &mut FrameReader<R>) -> (AttachRefusal, String)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("a refusal");
    match codec::decode::<SessionToClientMsg>(&frame.body).unwrap() {
        SessionToClientMsg::AttachFailed { reason, detail } => (reason, detail),
        other => panic!("expected SessionToClientMsg::AttachFailed, got {other:?}"),
    }
}

async fn info_outcome<R>(reader: &mut FrameReader<R>) -> InfoOutcome
where
    R: tokio::io::AsyncRead + Unpin,
{
    let frame = reader.next_frame().await.unwrap().expect("an InfoReply");
    match codec::decode::<OpsToClientMsg>(&frame.body).unwrap() {
        OpsToClientMsg::InfoReply { outcome } => outcome,
        other => panic!("expected OpsToClientMsg::InfoReply, got {other:?}"),
    }
}

async fn pooled_session(pool: &Arc<Mutex<SessionPool>>) -> SessionId {
    let session_task::SessionLifecycle { id, .. } = session_task::spawn_owned(
        pool,
        owned_session("sleep 30"),
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;
    id
}

/// Session ids are random, so an ambiguous prefix has to be found
/// rather than written down: sessions are added until two of them share
/// a leading hex digit, which sixteen possible digits make quick.
async fn prefix_matching_several(pool: &Arc<Mutex<SessionPool>>) -> (String, u32) {
    let mut ids: Vec<u128> = Vec::new();
    for _ in 0..24 {
        ids.push(pooled_session(pool).await.0);
        let mut counts = std::collections::BTreeMap::<char, u32>::new();
        for id in &ids {
            let hex = hex(*id);
            let first = hex.chars().next().expect("a 32-digit rendering");
            *counts.entry(first).or_default() += 1;
        }
        if let Some((digit, matches)) = counts.iter().find(|(_, matches)| **matches > 1) {
            return (digit.to_string(), *matches);
        }
    }
    panic!("24 random ids shared no leading hex digit");
}

/// A prefix names a session the daemon resolves for itself, and the ack
/// carries the full id back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attach_by_prefix_lands_on_the_session_the_daemon_resolves() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let id = pooled_session(&pool).await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let hex = hex(id.0);
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(hex[..8].to_owned()),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(attached_info(&mut reader).await.id, id.0);
}

/// The resolution and the handle are taken under one lock, so a session
/// destroyed before the attach arrives is a `NoMatch` on the prefix
/// rather than an attach to a row the pool has dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prefix_whose_session_was_destroyed_refuses_with_no_match() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let doomed = pooled_session(&pool).await;
    let hex = hex(doomed.0);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Ops, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Destroy {
            id_prefix: hex[..8].to_owned(),
        },
        1,
    )
    .await;
    let destroyed = reader.next_frame().await.unwrap().expect("a Destroyed");
    assert!(matches!(
        codec::decode::<OpsToClientMsg>(&destroyed.body).unwrap(),
        OpsToClientMsg::Destroyed {
            resolved: ResolvedId::Ok { .. }
        }
    ));

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(hex[..8].to_owned()),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    assert_eq!(
        attach_failure(&mut reader).await.0,
        AttachRefusal::Attach(AttachFailure::NoMatch)
    );
}

/// A prefix short enough to name two sessions is refused by count, not
/// resolved to whichever the pool happened to walk last.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_prefix_on_attach_refuses_with_the_match_count() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let (prefix, matches) = prefix_matching_several(&pool).await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Prefix(prefix),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    let (reason, detail) = attach_failure(&mut reader).await;
    assert_eq!(reason, AttachRefusal::Attach(AttachFailure::Ambiguous));
    assert!(
        detail.contains(&matches.to_string()),
        "the match count rides in detail: {detail}"
    );
}

/// `Ops::Info` answers the row and the display prefix from one pool
/// snapshot, and the prefix it reports resolves back to the same
/// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_info_answers_a_row_and_a_short_id_checked_against_the_pool() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let first = pooled_session(&pool).await;
    let second = pooled_session(&pool).await;
    let hex = hex(first.0);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Info {
            id_prefix: hex[..16].to_owned(),
        },
        1,
    )
    .await;
    let InfoOutcome::Found { session, short_id } = info_outcome(&mut reader).await else {
        panic!("the prefix names exactly one session");
    };
    assert_eq!(session.id, first.0);
    assert_eq!(
        felis_protocol::session_prefix::resolve_session_prefix(&short_id, [first.0, second.0]),
        ResolvedId::Ok { id: first.0 },
        "the reported short id must resolve against the pool it was shortened against"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ops_info_reports_no_match_and_ambiguity_as_outcome_arms() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));
    let (prefix, matches) = prefix_matching_several(&pool).await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_request(
        &mut writer,
        &OpsToDaemonMsg::Info {
            id_prefix: "f".repeat(32),
        },
        1,
    )
    .await;
    assert_eq!(info_outcome(&mut reader).await, InfoOutcome::NoMatch);

    send_request(&mut writer, &OpsToDaemonMsg::Info { id_prefix: prefix }, 2).await;
    assert_eq!(
        info_outcome(&mut reader).await,
        InfoOutcome::Ambiguous { matches }
    );
}
