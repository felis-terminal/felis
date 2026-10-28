use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use felis_client_core::{ConnectError, Connection, Reconnector};
use felis_protocol::messages::{
    MAX_PASTE_BYTES, RetargetCarrier, RetargetLanding, RetargetTarget, SessionInfo,
};
use felis_protocol::{
    codec,
    messages::{InputMsg, SendOutcome, SendRefusal, SendStep as WireStep},
};

use super::{
    SessionOp,
    attachment::{RetargetArgs, local_carrier, retarget_from_args, staged_retarget},
    mutate::{
        SendStep, SendSteps, StepOrigin, chord_key_event, cmd_send, collect_send_payload,
        over_limit_payload, read_send_payload, resolve_spawn_cwd, tag_result, wire_steps,
    },
    plan,
    read::{RowEncoding, cmd_search, session_passes_tag_filter},
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
fn over_limit_payload_bounds_a_paste_and_a_raw_payload_by_their_own_caps() {
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

/// Reading stdin for anything but `-` would hang a caller whose stdin
/// is a terminal.
struct UnreadStdin;

impl std::io::Read for UnreadStdin {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        panic!("only a bare `-` may read stdin");
    }
}

#[test]
fn collect_send_payload_returns_text_verbatim() {
    let (bytes, len) = collect_send_payload("hello", MAX_PASTE_BYTES, UnreadStdin).unwrap();
    assert_eq!(bytes, b"hello");
    assert_eq!(len, 5);
}

#[test]
fn collect_send_payload_reads_stdin_only_for_bare_dash() {
    let (bytes, len) = collect_send_payload("-", MAX_PASTE_BYTES, &b"piped\n"[..]).unwrap();
    assert_eq!(bytes, b"piped\n");
    assert_eq!(len, 6);
    let (bytes, _) = collect_send_payload("--", MAX_PASTE_BYTES, UnreadStdin).unwrap();
    assert_eq!(bytes, b"--");
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
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        ..crate::test_support::session_info(1)
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

/// Run `cmd_send` against a scripted daemon over an in-memory duplex
/// pair and return every step it put on the wire, as the input each
/// would be. Reading the steps back is the only way to tell `--raw`
/// from the default: with bracketed paste off both deliver
/// byte-identical PTY input, so the distinction lives in the variant.
async fn input_frames_from_send(bytes: &[u8], raw: bool, keys: &[&str]) -> Vec<InputMsg> {
    let steps: Vec<SendStep> = keys
        .iter()
        .map(|k| SendStep::Key(k.parse().expect("chord")))
        .collect();
    let (code, frames) = send_against(bytes, raw, &steps, |_| None).await;
    assert_eq!(code, 0, "the scripted daemon takes every step");
    frames
}

/// Run `cmd_send` against a scripted daemon that answers its
/// `Ops::Send` with `refuse(steps)`, or `Sent` when that is `None`,
/// returning the exit code and the steps the request carried.
async fn send_against(
    bytes: &[u8],
    raw: bool,
    steps: &[SendStep],
    refuse: impl FnOnce(&[WireStep]) -> Option<SendOutcome> + Send + 'static,
) -> (i32, Vec<InputMsg>) {
    use felis_protocol::MessageKind;
    use felis_protocol::messages::{OpsToClientMsg, OpsToDaemonMsg};
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let mut writer = FrameWriter::at_build_minor(daemon_write);
        let frame = reader
            .next_frame()
            .await
            .expect("read client frame")
            .expect("the client sends its request");
        assert_eq!(
            MessageKind::from_u16(frame.kind),
            Some(MessageKind::Ops),
            "a send is one Ops request, with no attach ahead of it"
        );
        let OpsToDaemonMsg::Send { id_prefix, steps } =
            codec::decode::<OpsToDaemonMsg>(&frame.body).expect("decode Ops")
        else {
            panic!("expected Ops::Send");
        };
        assert_eq!(
            id_prefix, SEND_TEST_PREFIX,
            "send must let the daemon resolve the prefix"
        );
        // The reply must echo the request id, or the client's driver
        // refuses it as unmatchable.
        let correlation = codec::peek_correlation(&frame.body)
            .expect("decode envelope")
            .expect("an Ops verb carries a request id");
        let outcome = refuse(&steps).unwrap_or(SendOutcome::Sent { id: SEND_TEST_ID });
        writer
            .send_correlated(&OpsToClientMsg::Sent { outcome }, correlation)
            .await
            .expect("write Sent");
        assert!(
            reader.next_frame().await.expect("read to close").is_none(),
            "nothing follows the reply"
        );
        steps.into_iter().map(WireStep::into_input).collect()
    });

    let conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        felis_protocol::ConnectionMode::Ops,
    );
    let out = Reporter::point(Format::Human);
    let built = wire_steps(&out, bytes.to_vec(), raw, steps).expect("steps a daemon takes");
    let code = cmd_send(conn, &out, SEND_TEST_PREFIX, built, steps, None)
        .await
        .expect("send completes");
    (code, daemon.await.expect("daemon task"))
}

/// Run `cmd_send` against a daemon that reads the request and then
/// hangs up, and return the exit code it reports.
async fn send_exit_code_when_the_daemon_drops_mid_send() -> i32 {
    use felis_transport::{FrameReader, FrameWriter};

    let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

    let daemon = tokio::spawn(async move {
        let mut reader = FrameReader::new(daemon_read);
        let writer = FrameWriter::at_build_minor(daemon_write);
        drop(reader.next_frame().await.expect("read client frame"));
        drop(writer);
        drop(reader);
    });

    let conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::at_build_minor(client_write),
        felis_protocol::ConnectionMode::Ops,
    );
    let out = Reporter::point(Format::Json);
    let built = wire_steps(&out, b"hi".to_vec(), true, &[]).expect("one raw step");
    let code = cmd_send(conn, &out, SEND_TEST_PREFIX, built, &[], None)
        .await
        .expect("send reports rather than panicking");
    daemon.await.expect("daemon task");
    code
}

/// A socket that closes before the reply is the daemon going away,
/// not the daemon answering "no such session": the exit contract
/// gives the first `2` (retry) and the second `1` (pick again), and a
/// caller that cannot tell them apart retries the wrong thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_closing_mid_send_exits_two() {
    assert_eq!(send_exit_code_when_the_daemon_drops_mid_send().await, 2);
}

/// A daemon below the source's minor would ignore the field and search
/// scrollback, so the verb refuses as `unsupported` (exit 2) before a
/// byte leaves, the prefix lookup included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_visible_search_on_a_minor_zero_daemon_exits_two_unsent() {
    use felis_protocol::messages::{SearchOptions, SearchSource};
    use felis_transport::{FrameReader, FrameWriter};
    use tokio::io::AsyncReadExt as _;

    let (client_side, mut daemon_side) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_side);
    let conn = Connection::from_halves(
        FrameReader::new(client_read),
        FrameWriter::baseline(client_write),
        felis_protocol::ConnectionMode::Ops,
    );
    let out = Reporter::stream(Format::Jsonl);
    let code = cmd_search(
        conn,
        &out,
        SEND_TEST_PREFIX,
        "needle",
        SearchOptions::default(),
        SearchSource::Visible,
    )
    .await
    .expect("search reports rather than erroring");
    assert_eq!(code, 2);

    let mut sent = Vec::new();
    daemon_side
        .read_to_end(&mut sent)
        .await
        .expect("read the daemon side");
    assert!(
        sent.is_empty(),
        "nothing may reach the daemon, got {sent:?}"
    );
}

#[test]
fn an_unexpected_frame_kind_reports_a_protocol_failure() {
    assert_eq!(
        ErrorKind::from_connect_error(&ConnectError::UnexpectedKind { kind: 99 }),
        ErrorKind::Protocol
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
        steps: SendSteps::default(),
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

fn send_steps(argv: &[&str]) -> Vec<SendStep> {
    use clap::Parser as _;

    #[derive(clap::Parser)]
    struct Probe {
        #[command(subcommand)]
        op: SessionOp,
    }
    let argv = ["probe", "send", "1a"].iter().chain(argv);
    match Probe::try_parse_from(argv).expect("parses").op {
        SessionOp::Send { steps, .. } => steps.0,
        _ => unreachable!("parsed a send"),
    }
}

#[test]
fn key_and_mouse_steps_keep_their_command_line_order() {
    let mouse = |s: &str| SendStep::Mouse(s.parse().expect("event"));
    let key = |s: &str| SendStep::Key(s.parse().expect("chord"));
    assert_eq!(
        send_steps(&[
            "--mouse",
            "click:1,2",
            "--key",
            "enter",
            "--mouse",
            "wheel-up:0,0",
            "--key",
            "tab",
        ]),
        [
            mouse("click:1,2"),
            key("enter"),
            mouse("wheel-up:0,0"),
            key("tab")
        ],
    );
}

/// The events go out after the paste and between the keys, as
/// unplaced 1-based cells the daemon encodes against the live modes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mouse_steps_follow_the_paste_in_order_with_the_keys() {
    use felis_protocol::messages::{InputMods, MouseAction, MouseButton, MouseEvent};

    let steps = send_steps(&["--key", "escape", "--mouse", "click:12,4"]);
    let (code, frames) = send_against(b"x", false, &steps, |_| None).await;
    assert_eq!(code, 0);
    let click = |action| {
        InputMsg::Mouse(MouseEvent {
            button: Some(MouseButton::Left),
            action,
            mods: InputMods::empty(),
            x: 13,
            y: 5,
            px: 0,
            py: 0,
        })
    };
    assert_eq!(
        frames,
        [
            InputMsg::Paste(b"x".to_vec()),
            InputMsg::Key(chord_key_event(&"escape".parse().expect("chord"))),
            click(MouseAction::Press),
            click(MouseAction::Release),
        ],
    );
}

/// The daemon names the refused step by its index among the wire
/// steps; the message names the argument it came from, a click's
/// release included.
#[test]
fn a_refused_step_is_reported_against_its_argument() {
    let steps = send_steps(&["--key", "enter", "--mouse", "click:3,4"]);
    let out = staging_out();
    let (wire, origins) =
        wire_steps(&out, b"x".to_vec(), false, &steps).expect("the steps are well-formed");
    assert_eq!(wire.len(), 4, "the paste, the key, the press, the release");
    assert_eq!(
        origins,
        [
            StepOrigin::Payload,
            StepOrigin::Arg(0),
            StepOrigin::Arg(1),
            StepOrigin::Arg(1),
        ]
    );
}

/// A refusal is the daemon's verdict on the whole request: the verb
/// fails as an invalid request (exit 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_the_daemon_refuses_fails_the_send() {
    let steps = send_steps(&["--mouse", "drag:0,0"]);
    let (code, _) = send_against(b"", false, &steps, |_| {
        Some(SendOutcome::Refused {
            reason: SendRefusal::Unreported,
            step: Some(0),
            detail: "the program has no mouse reporting on; nothing was sent".into(),
        })
    })
    .await;
    assert_eq!(code, 1);
}
