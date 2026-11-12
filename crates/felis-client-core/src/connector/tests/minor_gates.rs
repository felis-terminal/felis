use super::*;

/// The arm whose row a post-release minor would carry: nothing
/// declares it on the wire, and it stands in for the first real one.
struct FutureArm;

impl Directed for FutureArm {
    const ARMS: &'static [felis_protocol::messages::ArmMeta] =
        &[felis_protocol::messages::ArmMeta::new(
            "Future::Arm",
            felis_protocol::messages::Direction::ToDaemon,
            felis_protocol::messages::CorrelationClass::Uncorrelated,
            felis_protocol::messages::ModeSet::OPS,
        )
        .since(preface::PROTOCOL_MINOR + 1)];

    fn arm_index(&self) -> usize {
        0
    }
}

/// A client may name only the modes the negotiated minor defines.
/// Every shipped mode is a base-schema one, so the case that pins
/// the gate is a synthetic mode from one minor past this build: the
/// shape a newer client dialing an older daemon carries.
#[test]
fn a_mode_above_the_effective_minor_is_refused() {
    for mode in [
        ConnectionMode::Window,
        ConnectionMode::Ops,
        ConnectionMode::Observer,
    ] {
        assert!(
            check_mode_minor(mode.since_minor(), 0).is_ok(),
            "{mode:?} ships with the base schema"
        );
    }

    let future = preface::PROTOCOL_MINOR + 1;
    let refused = check_mode_minor(future, preface::PROTOCOL_MINOR);
    match refused {
        Err(ConnectError::MinorTooOld {
            needs, effective, ..
        }) => assert_eq!((needs, effective), (future, preface::PROTOCOL_MINOR)),
        other => panic!("expected MinorTooOld, got {other:?}"),
    }
    assert!(
        !ConnectError::MinorTooOld {
            needs: future,
            effective: preface::PROTOCOL_MINOR,
            feature: "the connection mode this client asks for",
        }
        .is_transient(),
        "a daemon that predates the mode answers the same way on every retry"
    );
}

/// The whole first-release schema is authorized at minor 0, so the
/// client-side gate is pinned by the row the first post-release
/// arm will carry: a newer client dialing an older daemon refuses
/// the verb locally rather than losing the connection at the
/// daemon's decode.
#[test]
fn an_arm_above_the_effective_minor_is_refused_locally() {
    let future = preface::PROTOCOL_MINOR + 1;
    match check_arm_minor(&FutureArm, preface::PROTOCOL_MINOR, "a future verb") {
        Err(ConnectError::MinorTooOld {
            needs,
            effective,
            feature,
        }) => assert_eq!(
            (needs, effective, feature),
            (future, preface::PROTOCOL_MINOR, "a future verb")
        ),
        other => panic!("expected MinorTooOld, got {other:?}"),
    }
    assert!(
        check_arm_minor(&FutureArm, future, "a future verb").is_ok(),
        "the gate opens at the minor the arm's own row names"
    );
    assert!(
        check_arm_minor(
            &OpsToDaemonMsg::Status,
            0,
            "the daemon's resource accounting"
        )
        .is_ok(),
        "every first-release arm is authorized at minor 0"
    );
}

/// A daemon whose effective minor predates the upgrade arm is refused
/// before a byte leaves: sent, the arm would cost the connection rather
/// than earn a reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_against_a_minor_zero_daemon_is_refused_without_sending() {
    use tokio::io::AsyncReadExt as _;

    let (client_side, mut daemon_side) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client_side);
    let mut conn = Connection::from_halves(
        FrameReader::new(read),
        FrameWriter::baseline(write),
        ConnectionMode::Ops,
    );
    assert_eq!(conn.effective_minor, 0);

    let refused = tokio::time::timeout(
        Duration::from_secs(5),
        conn.daemon_upgrade("/usr/bin/felis-daemon".to_owned()),
    )
    .await
    .expect("the gate answers without a round trip");
    match refused {
        Err(ConnectError::MinorTooOld {
            needs, effective, ..
        }) => assert_eq!((needs, effective), (1, 0)),
        other => panic!("expected MinorTooOld, got {other:?}"),
    }

    drop(conn);
    let mut sent = Vec::new();
    daemon_side
        .read_to_end(&mut sent)
        .await
        .expect("read the daemon side");
    assert!(sent.is_empty(), "nothing reached the daemon: {sent:?}");
}

/// One minor carried every session-addressed form, so below it each one
/// is refused before a byte is written: a daemon from before it would
/// read the search or capture as its own attached one and the send as
/// an empty `Ops` oneof.
#[tokio::test]
async fn every_session_addressed_call_is_refused_unsent_on_the_previous_minor() {
    use felis_protocol::messages::{
        OpsToDaemonMsg, RegionSource, RegionToDaemonMsg, SearchOptions, SearchSource,
        SearchToDaemonMsg, SendStep,
    };
    use felis_transport::{FrameReader, FrameWriter};
    use tokio::io::AsyncReadExt as _;

    let since = OpsToDaemonMsg::Send {
        id_prefix: String::new(),
        steps: Vec::new(),
    }
    .meta()
    .since_minor;
    let (client_side, mut daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let mut conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::new(client_write, since - 1),
        ConnectionMode::Ops,
    );
    let too_old = |result: Result<_, ConnectError>| {
        assert!(
            matches!(result, Err(ConnectError::MinorTooOld { needs, .. }) if needs == since),
            "expected a local refusal",
        );
    };

    too_old(
        conn.open_session_stream(&SearchToDaemonMsg::Query {
            query: "x".into(),
            options: SearchOptions::default(),
            source: SearchSource::Scrollback,
            session: Some(1),
        })
        .await
        .map(drop),
    );
    too_old(
        conn.open_session_stream(&RegionToDaemonMsg::Rows {
            source: RegionSource::Visible,
            ansi: false,
            max_rows: None,
            session: Some(1),
        })
        .await
        .map(drop),
    );
    too_old(
        conn.send_steps("1".into(), vec![SendStep::Paste(b"x".to_vec())])
            .await
            .map(drop),
    );
    too_old(
        conn.open_send_and_wait("1".into(), Vec::new())
            .await
            .map(drop),
    );
    drop(conn);

    let mut sent = Vec::new();
    daemon_side.read_to_end(&mut sent).await.unwrap();
    assert!(
        sent.is_empty(),
        "nothing may reach the daemon, got {sent:?}"
    );
}
