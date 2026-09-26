use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use felis_client_core::{ConnectError, Connection, Reconnector};
use felis_protocol::messages::{
    MAX_PASTE_BYTES, RetargetCarrier, RetargetLanding, RetargetTarget, SessionInfo,
};
use felis_protocol::{
    codec,
    messages::{InputMsg, SessionToClientMsg, SessionToDaemonMsg},
};

use super::{
    SessionOp,
    attachment::{RetargetArgs, local_carrier, retarget_from_args, staged_retarget},
    mutate::{
        chord_key_event, cmd_send, collect_send_payload, over_limit_payload, read_send_payload,
        resolve_spawn_cwd, tag_result,
    },
    parse_region_source, plan,
    read::{RowEncoding, session_passes_tag_filter},
    source_names,
};
use crate::cli_output::{ErrorKind, Format, PointFormat, Reporter, SessionRef};

fn retarget_args() -> RetargetArgs {
    RetargetArgs {
        carrier: RetargetCarrier::DefaultLocal,
        session: None,
        from: None,
        command: Vec::new(),
        attachment: None,
        format: Format::Human,
    }
}

fn staging_out() -> Reporter {
    Reporter::point(Format::Human)
}

/// A payload the daemon would refuse must be caught here: on the
/// wire the refusal is a closed connection, which the caller can
/// only report as the transient `daemon_lost` class.
#[test]
fn an_over_limit_payload_is_refused_before_the_dial() {
    use felis_protocol::limits::{MAX_PASTE_BYTES, PTY_INPUT_BUDGET};

    let out = staging_out();
    assert_eq!(over_limit_payload(&out, MAX_PASTE_BYTES, false), None);
    assert_eq!(
        over_limit_payload(&out, MAX_PASTE_BYTES + 1, false),
        Some(1)
    );
    // `--raw` skips the bracketing, so the whole budget is its bound.
    assert_eq!(over_limit_payload(&out, MAX_PASTE_BYTES + 1, true), None);
    assert_eq!(over_limit_payload(&out, PTY_INPUT_BUDGET, true), None);
    assert_eq!(
        over_limit_payload(&out, PTY_INPUT_BUDGET + 1, true),
        Some(1)
    );
}

/// One assertion per carrier arm: a wrong mapping is silent on the
/// CLI side (a socket path collapsing to `DefaultLocal` re-dials
/// the caller's own socket and still exits 0).
#[test]
fn each_destination_builds_its_own_arm() {
    let ssh = RetargetArgs {
        carrier: RetargetCarrier::Ssh {
            destination: "user@devbox".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        },
        ..retarget_args()
    };
    assert_eq!(
        retarget_from_args(ssh).carrier,
        RetargetCarrier::Ssh {
            destination: "user@devbox".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        }
    );

    let endpoint = RetargetArgs {
        carrier: local_carrier(Some(Path::new("/tmp/felis-work.sock"))).unwrap(),
        ..retarget_args()
    };
    assert_eq!(
        retarget_from_args(endpoint).carrier,
        RetargetCarrier::LocalEndpoint("/tmp/felis-work.sock".into())
    );

    // No destination is the way home, not a usage error.
    assert_eq!(
        retarget_from_args(retarget_args()).carrier,
        RetargetCarrier::DefaultLocal
    );
}

/// `--session` attaches; its absence creates, with `-- <cmd>`
/// filling in the program.
#[test]
fn the_session_flag_picks_attach_over_create() {
    let attach = RetargetArgs {
        session: Some("1a2b".into()),
        ..retarget_args()
    };
    assert_eq!(
        retarget_from_args(attach).landing,
        RetargetLanding::Attach("1a2b".into())
    );

    let create = RetargetArgs {
        command: vec!["htop".into(), "-d".into()],
        ..retarget_args()
    };
    let RetargetLanding::Create(args) = retarget_from_args(create).landing else {
        panic!("no --session must create");
    };
    assert_eq!(args.command, "htop");
    assert_eq!(args.args, vec!["-d".to_string()]);

    let RetargetLanding::Create(args) = retarget_from_args(retarget_args()).landing else {
        panic!("no --session must create");
    };
    assert!(args.command.is_empty(), "$SHELL is the daemon's to pick");
}

#[test]
fn collect_send_payload_returns_text_verbatim() {
    let (bytes, len) = collect_send_payload("hello", MAX_PASTE_BYTES).unwrap();
    assert_eq!(bytes, b"hello");
    assert_eq!(len, 5);
}

#[test]
fn collect_send_payload_reads_stdin_only_for_bare_dash() {
    // A real stdin feed needs a fork+pipe heavier than the assertion
    // is worth: `-` reads the empty harness stdin, so empty is the
    // exact expected value (`!= b"-"` would also accept the wrong
    // branch).
    let (bytes, len) = collect_send_payload("-", MAX_PASTE_BYTES).unwrap();
    assert!(bytes.is_empty(), "expected empty harness stdin: {bytes:?}");
    assert_eq!(len, 0);
}

/// The refusal must name what the caller actually piped, not the
/// point the bounded read stopped at: `cap + 1` for a source three
/// times the cap would understate it.
#[test]
fn read_send_payload_counts_past_the_cap_without_buffering_it() {
    let source = vec![b'a'; 30];

    let (bytes, len) = read_send_payload(source.as_slice(), 10).unwrap();

    assert_eq!(len, 30, "the length names the whole source");
    assert_eq!(bytes.len(), 11, "only the cap plus one byte is held");
}

/// `--cwd` wins; a local spawn adopts the caller's cwd (a deliberate
/// `/` included); a `--host` spawn keeps the empty remote default.
#[test]
fn resolve_spawn_cwd_prefers_flag_then_local_caller_cwd() {
    let here = Some(PathBuf::from("/home/me/proj"));

    let resolved = |explicit, is_local, current| {
        resolve_spawn_cwd(explicit, is_local, current).expect("a UTF-8 cwd resolves")
    };

    assert_eq!(resolved(Some("/srv"), true, here.clone()), "/srv");
    assert_eq!(resolved(Some("/srv"), false, here.clone()), "/srv");
    assert_eq!(resolved(None, true, here.clone()), "/home/me/proj");
    assert_eq!(resolved(None, true, Some(PathBuf::from("/"))), "/");
    assert_eq!(resolved(None, true, None), "");
    assert_eq!(resolved(None, false, here), "");
}

/// A relative `--cwd` is anchored to the caller for a local spawn
/// and passes through for a `--host` one.
#[test]
fn resolve_spawn_cwd_anchors_a_relative_flag_to_the_local_caller() {
    let here = Some(PathBuf::from("/home/me/proj"));

    let resolved = |explicit, is_local, current| {
        resolve_spawn_cwd(explicit, is_local, current).expect("a UTF-8 cwd resolves")
    };

    // Joined through `Path`: the separator is the platform's.
    let anchored = PathBuf::from("/home/me/proj").join("build");
    assert_eq!(
        resolved(Some("build"), true, here.clone()),
        anchored.to_string_lossy(),
    );
    assert_eq!(resolved(Some("build"), false, here), "build");
    assert_eq!(resolved(Some("build"), true, None), "build");
}

/// A non-UTF-8 caller cwd is refused rather than spelled with
/// U+FFFD, both on its own and as the anchor of a relative `--cwd`.
#[test]
#[cfg(unix)]
fn resolve_spawn_cwd_refuses_a_non_utf8_caller_cwd() {
    let here = Some(non_utf8_path());

    assert!(resolve_spawn_cwd(None, true, here.clone()).is_err());
    assert!(resolve_spawn_cwd(Some("build"), true, here.clone()).is_err());
    // A remote spawn never reads the caller's cwd, so it still resolves.
    assert_eq!(resolve_spawn_cwd(None, false, here.clone()).unwrap(), "");
    // An absolute `--cwd` is the user's own UTF-8 spelling.
    assert_eq!(resolve_spawn_cwd(Some("/srv"), true, here).unwrap(), "/srv");
}

/// A non-UTF-8 `<SOCKET>` is a usage error: dialing the U+FFFD
/// spelling would fail against a path the user never named.
#[test]
#[cfg(unix)]
fn local_carrier_refuses_a_non_utf8_socket_path() {
    let path = non_utf8_path();

    assert!(local_carrier(Some(&path)).is_err());
}

#[cfg(unix)]
fn non_utf8_path() -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;

    PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/felis-\xff"))
}

#[test]
fn a_rosterless_result_carries_the_full_id_and_no_prefix() {
    let value = crate::cli_output::body_value(&SessionRef::new(0x1a2b));
    assert_eq!(value["id"], "00000000000000000000000000001a2b");
    assert!(value.get("short_id").is_none());
    assert!(
        crate::cli_output::body_value(&tag_result(0x1a2b, &[]))
            .get("short_id")
            .is_none()
    );
    // The optional halves stay absent on the plain form, not null.
    assert!(value.get("was_attached").is_none());
    assert!(value.get("exit_code").is_none());
}

/// `tags` is always present, `[]` when the set is now empty.
#[test]
fn tag_result_emits_an_always_present_sorted_array() {
    let value =
        crate::cli_output::body_value(&tag_result(1, &["agent".to_owned(), "work".to_owned()]));
    assert_eq!(value["id"], "00000000000000000000000000000001");
    assert_eq!(value["tags"], serde_json::json!(["agent", "work"]));
    let emptied = crate::cli_output::body_value(&tag_result(0xff, &[]));
    assert_eq!(emptied["tags"], serde_json::json!([]));
}

fn tagged(tags: &[&str]) -> SessionInfo {
    SessionInfo {
        id: 1,
        dims: felis_protocol::messages::GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: None,
        cwd: None,
        idle_seconds: None,
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        last_notification: None,
        foreground: None,
        exited: false,
        last_exit_code: None,
        attachments: Vec::new(),
        sequence: std::num::NonZeroU64::MIN,
    }
}

#[test]
fn empty_tag_filter_passes_every_session() {
    assert!(session_passes_tag_filter(&tagged(&[]), &[]));
    assert!(session_passes_tag_filter(&tagged(&["work"]), &[]));
}

#[test]
fn tag_filter_matches_on_any_requested_tag() {
    // "Any of": one of several requested tags passes; none is
    // filtered out.
    let filter = vec!["work".to_owned(), "agent".to_owned()];
    assert!(session_passes_tag_filter(&tagged(&["agent"]), &filter));
    assert!(!session_passes_tag_filter(&tagged(&["personal"]), &filter));
    assert!(!session_passes_tag_filter(&tagged(&[]), &filter));
}

/// Every `--ansi` / `--format` pairing resolves; `--ansi --format
/// jsonl` yields both fields.
#[test]
fn row_encoding_resolves_every_flag_pairing() {
    assert_eq!(
        RowEncoding::resolve(false, Format::Human),
        RowEncoding::Text
    );
    assert_eq!(
        RowEncoding::resolve(false, Format::Jsonl),
        RowEncoding::Text
    );
    assert_eq!(RowEncoding::resolve(true, Format::Human), RowEncoding::Ansi);
    assert_eq!(RowEncoding::resolve(true, Format::Jsonl), RowEncoding::Both);
    assert!(!RowEncoding::Text.wants_ansi());
    assert!(RowEncoding::Ansi.wants_ansi());
    assert!(RowEncoding::Both.wants_ansi());
}

/// Ids print as zero-padded 32-hex, so any prefix of this one is a
/// run of leading zeros.
const SEND_TEST_ID: u128 = 0x00ab_cdef;
const SEND_TEST_PREFIX: &str = "00000000";

fn send_test_session() -> SessionInfo {
    SessionInfo {
        id: SEND_TEST_ID,
        dims: felis_protocol::messages::GridDims {
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
    }
}

/// Run `cmd_send` against a scripted daemon over an in-memory duplex
/// pair and return every `InputMsg` frame it put on the wire.
/// Reading the frames back is the only way to tell `--raw` from the
/// default: with bracketed paste off both deliver byte-identical
/// PTY input, so the distinction lives in the message variant.
async fn input_frames_from_send(bytes: &[u8], raw: bool, keys: &[&str]) -> Vec<InputMsg> {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{GridMsg, OpsToClientMsg, OpsToDaemonMsg};
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let mut writer = FrameWriter::at_build_minor(daemon_write);
        let mut inputs = Vec::new();
        while let Some(frame) = reader.next_frame().await.expect("read client frame") {
            match MessageKind::from_u16(frame.kind).expect("known frame kind") {
                MessageKind::Ops => {
                    assert!(matches!(
                        codec::decode::<OpsToDaemonMsg>(&frame.body).expect("decode Ops"),
                        OpsToDaemonMsg::List,
                    ));
                    // The reply must echo the request id, or the
                    // client's driver refuses it as unmatchable.
                    let correlation = codec::peek_correlation(&frame.body)
                        .expect("decode envelope")
                        .expect("an Ops verb carries a request id");
                    let listed = OpsToClientMsg::Listed {
                        sessions: vec![send_test_session()],
                    };
                    writer
                        .send_correlated(&listed, correlation)
                        .await
                        .expect("write Listed");
                }
                MessageKind::Session => {
                    match codec::decode::<SessionToDaemonMsg>(&frame.body).expect("decode Session")
                    {
                        SessionToDaemonMsg::Attach { target, .. } => {
                            assert_eq!(
                                target,
                                felis_protocol::messages::AttachTarget::Prefix(
                                    SEND_TEST_PREFIX.to_owned()
                                ),
                                "send must let the daemon resolve the prefix"
                            );
                            let ready = SessionToClientMsg::Attached {
                                info: send_test_session(),
                            };
                            writer.send(&ready).await.expect("write Ready");
                            writer
                                .send(&GridMsg::RehydrateEnd)
                                .await
                                .expect("write RehydrateEnd");
                        }
                        SessionToDaemonMsg::InputFence => {
                            let correlation = codec::peek_correlation(&frame.body)
                                .expect("decode envelope")
                                .expect("a fence carries a request id");
                            writer
                                .send_correlated(&SessionToClientMsg::InputAccepted, correlation)
                                .await
                                .expect("write InputAccepted");
                        }
                        SessionToDaemonMsg::Detach => break,
                        other => panic!("unexpected session message: {other:?}"),
                    }
                }
                MessageKind::Input => {
                    inputs.push(codec::decode::<InputMsg>(&frame.body).expect("decode"));
                }
                other => panic!("unexpected frame kind: {other:?}"),
            }
        }
        inputs
    });

    let conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        felis_protocol::ConnectionMode::Window,
    );
    let keys: Vec<felis_client_core::Chord> =
        keys.iter().map(|k| k.parse().expect("chord")).collect();
    let out = Reporter::point(Format::Human);
    let code = cmd_send(
        conn,
        &out,
        SEND_TEST_PREFIX,
        bytes.to_vec(),
        raw,
        &keys,
        None,
    )
    .await
    .expect("send completes");
    assert_eq!(code, 0, "the scripted daemon satisfies every step");
    daemon.await.expect("daemon task")
}

/// Run `cmd_send` against a daemon that reads the attach and then
/// hangs up, and return the exit code it reports.
async fn send_exit_code_when_the_daemon_drops_at_attach() -> i32 {
    use felis_protocol::MessageKind;
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let writer = FrameWriter::at_build_minor(daemon_write);
        while let Some(frame) = reader.next_frame().await.expect("read client frame") {
            if MessageKind::from_u16(frame.kind).expect("known frame kind") == MessageKind::Session
            {
                break;
            }
        }
        drop(writer);
        drop(reader);
    });

    let conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        felis_protocol::ConnectionMode::Window,
    );
    let out = Reporter::point(Format::Json);
    let code = cmd_send(
        conn,
        &out,
        SEND_TEST_PREFIX,
        b"hi".to_vec(),
        true,
        &[],
        None,
    )
    .await
    .expect("send reports rather than panicking");
    daemon.await.expect("daemon task");
    code
}

/// A socket that closes mid-attach is the daemon going away, not
/// the daemon answering "no such session": the exit contract gives
/// the first `2` (retry) and the second `1` (pick again), and a
/// caller that cannot tell them apart retries the wrong thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_closing_mid_attach_exits_two() {
    assert_eq!(send_exit_code_when_the_daemon_drops_at_attach().await, 2);
}

#[test]
fn only_a_typed_refusal_reports_a_which_session_failure() {
    use felis_protocol::messages::AttachFailure;

    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::AttachFailed {
            reason: AttachFailure::NoMatch,
            detail: String::new(),
        }),
        ErrorKind::NoMatch
    );
    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::AttachFailed {
            reason: AttachFailure::Ambiguous,
            detail: String::new(),
        }),
        ErrorKind::Ambiguous
    );
    for transport in [ConnectError::EofMidAttach, ConnectError::EofBeforeWelcome] {
        assert_eq!(
            ErrorKind::from_connect_error(&transport),
            ErrorKind::DaemonLost
        );
    }
    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::NotSessionAttached),
        ErrorKind::Protocol
    );
    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::UnexpectedKind { kind: 99 }),
        ErrorKind::Protocol
    );
    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::InvalidSessionPrefix {
            prefix: String::new(),
            reason: "empty".to_owned(),
        }),
        ErrorKind::InvalidRequest
    );
}

/// The planner is where the refusal has to live: it runs before
/// [`Dial`], so an over-limit payload never costs a connection.
/// Every verb that carries one is checked through `plan` itself,
/// not through the helper, since a verb that forgot to call it
/// would pass a helper-level test.
fn planning_target() -> Reconnector {
    felis_client_core::reconnector_for_target(
        &RetargetTarget {
            carrier: RetargetCarrier::DefaultLocal,
            landing: RetargetLanding::Attach(String::new()),
        },
        // Never dialed: `plan` refuses before it returns a `Plan`.
        Some(PathBuf::from("/nonexistent/felis-planning.sock")),
    )
    .expect("a named socket resolves the default-local carrier")
}

fn send_op(text: String, raw: bool) -> SessionOp {
    SessionOp::Send {
        id: SEND_TEST_PREFIX.to_owned(),
        text: Some(text),
        raw,
        wait: false,
        timeout: None,
        output: PointFormat {
            format: Format::Json,
        },
        keys: Vec::new(),
    }
}

/// An over-limit payload never reaches a dial: the verb exits 1
/// under `invalid_request` (REQ-105a), and the boundary itself is
/// still accepted. `--raw` is checked too, against its own cap.
#[test]
fn an_over_limit_send_payload_is_refused_before_the_dial() {
    use felis_protocol::messages::{MAX_PASTE_BYTES, MAX_RAW_INPUT_BYTES};

    let target = planning_target();
    for (raw, cap) in [(false, MAX_PASTE_BYTES), (true, MAX_RAW_INPUT_BYTES)] {
        assert!(
            matches!(
                plan(send_op("a".repeat(cap + 1), raw), &target),
                ControlFlow::Break(1)
            ),
            "an over-limit send (raw={raw}) reached the dial"
        );
        assert!(
            matches!(
                plan(send_op("a".repeat(cap), raw), &target),
                ControlFlow::Continue(_)
            ),
            "the boundary itself must still send (raw={raw})"
        );
    }
}

/// The same for `spawn`, whose limit is on the program path.
#[test]
fn an_over_limit_spawn_is_refused_before_the_dial() {
    use felis_protocol::messages::MAX_SPAWN_PATH_BYTES;

    let target = planning_target();
    let spawn = |cmd: Vec<String>| SessionOp::Spawn {
        cwd: Some("/tmp".to_owned()),
        env: Vec::new(),
        rows: None,
        cols: None,
        tags: Vec::new(),
        cmd,
        output: PointFormat {
            format: Format::Json,
        },
    };
    assert!(matches!(
        plan(spawn(vec!["a".repeat(MAX_SPAWN_PATH_BYTES + 1)]), &target),
        ControlFlow::Break(1)
    ));
    assert!(matches!(
        plan(spawn(vec!["/bin/sh".to_owned()]), &target),
        ControlFlow::Continue(_)
    ));
}

/// And for a retarget, whose staging is the pre-dial check-point
/// every retarget verb shares.
#[test]
fn an_over_limit_retarget_descriptor_is_refused_before_the_dial() {
    use felis_protocol::messages::MAX_RETARGET_DESCRIPTOR_BYTES;

    let out = Reporter::point(Format::Json);
    let staged = staged_retarget(
        &out,
        RetargetArgs {
            carrier: RetargetCarrier::Ssh {
                destination: "a".repeat(MAX_RETARGET_DESCRIPTOR_BYTES + 1),
                ssh_args: Vec::new(),
            },
            from: Some(SEND_TEST_PREFIX.to_owned()),
            ..retarget_args()
        },
        "retarget",
    );
    assert_eq!(staged.err(), Some(1));
}

/// The payload rides one `InputMsg::Paste`, which is what lets the
/// daemon bracket it under `?2004`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_without_raw_emits_the_payload_as_a_paste() {
    let frames = input_frames_from_send(b"ls -la", false, &[]).await;
    assert_eq!(frames, vec![InputMsg::Paste(b"ls -la".to_vec())]);
}

/// `--raw` rides `InputMsg::KeyBytes`, never wrapped in paste
/// brackets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_with_raw_emits_the_payload_as_key_bytes() {
    let frames = input_frames_from_send(b"\x1b[A", true, &[]).await;
    assert_eq!(frames, vec![InputMsg::KeyBytes(b"\x1b[A".to_vec())]);
}

/// `--key enter` rides its own structured frame *after* the payload:
/// a CR inside a bracketed paste is literal data, not a keypress.
/// The chord goes out unencoded, so the same daemon-side encoder
/// serves it and a GUI keystroke.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_enter_rides_its_own_structured_frame_after_the_payload() {
    let enter = InputMsg::Key(chord_key_event(&"enter".parse().expect("chord")));

    let frames = input_frames_from_send(b"echo hi", false, &["enter"]).await;
    assert_eq!(
        frames,
        vec![InputMsg::Paste(b"echo hi".to_vec()), enter.clone()],
    );

    let frames = input_frames_from_send(b"echo hi", true, &["enter"]).await;
    assert_eq!(frames, vec![InputMsg::KeyBytes(b"echo hi".to_vec()), enter],);
}

/// A chord reaches the daemon as the facts it encodes from, and the
/// composed text is present only where a real keyboard would deliver
/// one: under Ctrl / Alt / Super the encoder derives the bytes
/// itself.
#[test]
fn a_chord_becomes_the_key_event_a_keystroke_would() {
    use felis_protocol::messages::{Key, KeyEventKind, KeyLocation, NamedKey};

    let event =
        |spell: &str| chord_key_event(&spell.parse::<felis_client_core::Chord>().expect("chord"));

    let up = event("up");
    assert_eq!(up.key, Key::Named(NamedKey::ArrowUp));
    assert_eq!(up.text, None);
    assert_eq!(up.kind, KeyEventKind::Press);
    assert_eq!(up.location, KeyLocation::Standard);

    let plain = event("a");
    assert_eq!(plain.key, Key::Character("a".into()));
    assert_eq!(plain.text.as_deref(), Some("a"));

    let ctrl_c = event("ctrl+c");
    assert_eq!(ctrl_c.key, Key::Character("c".into()));
    assert_eq!(ctrl_c.text, None);
    assert!(ctrl_c.mods.control_key());
}

/// `A`, `shift+A` and `shift+a` are one chord, and a keyboard
/// delivers that keystroke as `A` with `A` composed.
#[test]
fn a_shifted_letter_chord_sends_the_uppercase_letter() {
    use felis_protocol::messages::Key;

    let event =
        |spell: &str| chord_key_event(&spell.parse::<felis_client_core::Chord>().expect("chord"));
    for spell in ["A", "shift+A", "shift+a"] {
        let shifted = event(spell);
        assert_eq!(shifted.key, Key::Character("A".into()), "{spell}");
        assert_eq!(shifted.text.as_deref(), Some("A"), "{spell}");
        assert!(shifted.mods.shift_key(), "{spell}");
    }

    let ctrl_shift = event("ctrl+shift+a");
    assert_eq!(ctrl_shift.key, Key::Character("A".into()));
    assert_eq!(ctrl_shift.text, None);

    let shift_digit = event("shift+1");
    assert_eq!(shift_digit.key, Key::Character("1".into()));
}

/// The refusal lists exactly today's vocabulary: a renamed or added
/// `SourceArg` variant must not leave the bridge telling a machine
/// client to send a spelling the parser now rejects.
#[test]
fn the_source_refusal_names_exactly_the_current_vocabulary() {
    let message = parse_region_source("bogus").unwrap_err();
    assert_eq!(
        message,
        "unknown source `bogus` (expected visible, scrollback, command-output, or last-command)",
    );
    for name in source_names() {
        assert!(
            message.contains(&name),
            "`{name}` is missing from {message}"
        );
    }
}
