use super::*;

/// `Ops::Status` carries a row for every accounted resource; a missing row
/// is the bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_every_accounted_resource_with_its_limit() {
    use felis_protocol::messages::{ResourceKind, ResourceUnit, SubjectKind};

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let caps = DaemonCaps {
        max_sessions: 7,
        ..DaemonCaps::default()
    };
    spawn_daemon_with_caps(&path, pool.clone(), shell_factory("sleep 30"), caps);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;
    drain_rehydrate(&mut reader).await;

    send_request(&mut writer, &OpsToDaemonMsg::Status, 1).await;
    let reply = loop {
        let frame = reader.next_frame().await.unwrap().expect("a Status reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    let OpsToClientMsg::StatusReply {
        resources,
        worker_threads,
        draining,
    } = reply
    else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    assert!(!draining, "a daemon nobody stopped is not draining");

    let find = |kind: ResourceKind| {
        resources
            .iter()
            .find(|r| r.resource == kind)
            .unwrap_or_else(|| panic!("no {kind:?} row in {resources:?}"))
    };
    assert!(
        worker_threads > 0,
        "the runtime this daemon runs on has workers"
    );
    for kind in [
        ResourceKind::Connections,
        ResourceKind::Sessions,
        ResourceKind::ImageStoreBytes,
        ResourceKind::InFlightDecodes,
        ResourceKind::InFlightDecodeBytes,
        ResourceKind::SubscriberQueueBytes,
    ] {
        let row = find(kind);
        assert!(
            per_subject_limit(row)
                .or_else(|| global_limit(row))
                .is_some(),
            "{kind:?} must report the ceiling it is admitted against"
        );
        // The scope arm is the whole point of the shape: a subject
        // sample belongs to a row that has subjects, and the type
        // already keeps a per-subject ceiling off a daemon row.
        assert_eq!(
            max_subject_used(row).is_some(),
            subject_kind(row).is_some(),
            "{kind:?} carries a deepest subject exactly when it has subjects: {row:?}"
        );
    }
    let connections = find(ResourceKind::Connections);
    assert_eq!(
        connections.total_used, 1,
        "this query is the only connection"
    );
    assert_eq!(
        global_limit(connections),
        Some(crate::pool::MAX_CONNECTIONS as u64),
        "the compiled connection cap is reported"
    );
    assert_eq!(subject_kind(connections), None);
    let sessions = find(ResourceKind::Sessions);
    assert_eq!(sessions.total_used, 1, "the one live session is the count");
    assert_eq!(
        global_limit(sessions),
        Some(7),
        "the configured cap is reported"
    );
    assert_eq!(sessions.unit, ResourceUnit::Count);
    assert_eq!(subject_kind(sessions), None);
    let images = find(ResourceKind::ImageStoreBytes);
    assert_eq!(subject_kind(images), Some(SubjectKind::Session));
    assert_eq!(
        per_subject_limit(images),
        Some(crate::pool::DEFAULT_IMAGE_BYTE_CAP as u64 * 2)
    );
    assert_eq!(
        images.total_used, 0,
        "a shell that drew no image holds no bytes"
    );
    assert_eq!(max_subject_used(images), Some(0));
    assert_eq!(find(ResourceKind::InFlightDecodes).total_used, 0);
    assert_eq!(find(ResourceKind::InFlightDecodeBytes).total_used, 0);
    let outbox = find(ResourceKind::SubscriberQueueBytes);
    assert_eq!(subject_kind(outbox), Some(SubjectKind::Subscriber));
    assert_eq!(
        per_subject_limit(outbox),
        Some(session_task::SUBSCRIBER_BUFFER_CAP as u64)
    );
}

/// Drives two samples from opposite ends so a row fed from the wrong source
/// fails rather than coincidentally agreeing, and holds two sessions of
/// different sizes so a sum cannot pass for a deepest subject.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_samples_what_the_sessions_are_holding() {
    use felis_grid::images::{ImageEntry, ImageFormat, ImageId};
    use felis_protocol::messages::ResourceKind;

    const IMAGE_BYTES: usize = 32 * 32 * 4;
    const PARKED: &[u8] = b"aaaabbbb";

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    spawn_daemon(&path, pool.clone(), shell_factory("sleep 30"));

    let mut owned = owned_session("sleep 30");
    owned
        .images
        .insert(
            ImageId(9),
            ImageEntry::new(32, 32, ImageFormat::Rgba32, vec![0xAB; IMAGE_BYTES]),
        )
        .expect("insert under cap");
    let head = format!("Ga=T,f=100,m=1;{}", std::str::from_utf8(PARKED).unwrap());
    let cmd = felis_grid::kitty_graphics::parse(head.as_bytes()).expect("a graphics command");
    assert!(
        matches!(
            owned.graphics_reassembler.feed(&cmd),
            felis_grid::kitty_graphics::Outcome::Pending
        ),
        "an m=1 head parks the transmission"
    );
    let image_cap = owned.images.bytes_cap() as u64 * 2;
    let deeper = owned.images.bytes_used() as u64;
    let _life = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    // Half the pixels of the first: the deepest session must be the
    // other one, so a max reported as a sum fails here.
    let mut smaller = owned_session("sleep 30");
    smaller
        .images
        .insert(
            ImageId(10),
            ImageEntry::new(16, 32, ImageFormat::Rgba32, vec![0xCD; IMAGE_BYTES / 2]),
        )
        .expect("insert under cap");
    let shallower = smaller.images.bytes_used() as u64;
    assert!(
        shallower < deeper,
        "the second session must hold strictly less: {shallower} vs {deeper}"
    );
    let _life2 = session_task::spawn_owned(
        &pool,
        smaller,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_request(&mut writer, &OpsToDaemonMsg::Status, 1).await;
    let reply = loop {
        let frame = reader.next_frame().await.unwrap().expect("a Status reply");
        if frame.kind != MessageKind::Ops.as_u16() {
            continue;
        }
        break codec::decode::<OpsToClientMsg>(&frame.body).unwrap();
    };
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    let find = |kind: ResourceKind| {
        resources
            .iter()
            .find(|r| r.resource == kind)
            .unwrap_or_else(|| panic!("no {kind:?} row in {resources:?}"))
    };

    assert_eq!(find(ResourceKind::Sessions).total_used, 2);
    let images = find(ResourceKind::ImageStoreBytes);
    assert_eq!(
        images.total_used,
        deeper + shallower,
        "the total is every session's store summed: {images:?}"
    );
    assert_eq!(
        max_subject_used(images),
        Some(deeper),
        "the subject sample is the deepest single session, not the sum: {images:?}"
    );
    assert_eq!(
        per_subject_limit(images),
        Some(image_cap),
        "the reported ceiling is the one the store enforces"
    );
    assert_eq!(find(ResourceKind::InFlightDecodes).total_used, 1);
    assert_eq!(
        find(ResourceKind::InFlightDecodeBytes).total_used,
        PARKED.len() as u64
    );
    assert_eq!(
        max_subject_used(find(ResourceKind::InFlightDecodeBytes)),
        Some(PARKED.len() as u64)
    );
}

/// The subscriber row is the one whose halves come from two different
/// `SessionStats` fields (a per-session sum and a per-session max), so
/// four outboxes of distinct depths spread over two sessions pin the
/// split: the deepest subject is a single subscriber, never a session's
/// sum and never the daemon-wide one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_sums_every_subscriber_backlog_and_names_the_deepest() {
    use felis_protocol::messages::ResourceKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The deepest single outbox is in the second session and is smaller
    // than the first session's two summed, so neither a per-session sum
    // nor the daemon-wide total can pass for the max.
    const DEPTHS: [[usize; 2]; 2] = [[4096, 8192], [16384, 256]];

    let pool = Arc::new(Mutex::new(SessionPool::new()));
    // Held for the whole test: dropping a receiver evicts the
    // subscriber, taking its backlog out of the report.
    let mut attached = Vec::new();
    for depths in DEPTHS {
        let session_task::SessionLifecycle { id, .. } = session_task::spawn_owned(
            &pool,
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
        let handle = pool
            .lock()
            .await
            .handle_cloned(id)
            .expect("session spawned");
        for depth in depths {
            let (tx, rx) = mpsc::unbounded_channel();
            let buffered = Arc::new(AtomicUsize::new(0));
            let (reply_tx, reply_rx) = oneshot::channel();
            handle
                .cmd
                .send(SessionCmd::Subscribe(SubscribeReq {
                    mode: ConnectionMode::Window,
                    pull_paced: false,
                    minor: preface::PROTOCOL_MINOR,
                    if_exited: session_task::IfExited::Land,
                    image_regions: false,
                    carrier: AttachCarrier::Local { client_pid: None },
                    tx,
                    buffered: Arc::clone(&buffered),
                    reply: reply_tx,
                }))
                .await
                .expect("the session task accepts the subscribe");
            reply_rx
                .await
                .expect("subscribe reply")
                .expect("the subscribe was admitted");
            // Stamped after the reply, which lands once the rehydrate
            // burst is queued: the gauge is then exactly this depth and
            // nothing drains it, since the receiver is never read.
            buffered.store(depth, Ordering::Relaxed);
            attached.push(rx);
        }
    }

    assert_eq!(
        attached.len(),
        4,
        "every outbox the report must sum is still attached"
    );

    let reply = daemon_status(&pool, &DaemonCaps::default()).await;
    let OpsToClientMsg::StatusReply { resources, .. } = reply else {
        panic!("expected OpsToClientMsg::StatusReply, got {reply:?}");
    };
    let outbox = resources
        .iter()
        .find(|r| r.resource == ResourceKind::SubscriberQueueBytes)
        .expect("a subscriber_queue_bytes row");

    let all: Vec<u64> = DEPTHS.iter().flatten().map(|d| *d as u64).collect();
    assert_eq!(
        outbox.total_used,
        all.iter().sum::<u64>(),
        "the total is every subscriber outbox of every session summed: {outbox:?}"
    );
    assert_eq!(
        max_subject_used(outbox),
        all.iter().copied().max(),
        "the subject sample is the deepest single subscriber: {outbox:?}"
    );
}
