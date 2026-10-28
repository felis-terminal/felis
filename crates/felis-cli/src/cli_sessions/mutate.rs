use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use felis_client_core::{
    Carrier, ConnectError, Connection, Reconnector, check_session_addressed, spawn_args_from_cli,
};
use felis_protocol::{
    SessionHex,
    messages::{
        InputMsg, OpsToClientMsg, SendOutcome, SendRefusal, SendStep as WireStep, StreamId,
    },
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::mouse::MouseArg;
use super::{
    driver::{DrainTick, Drained, Stall, drain_frames},
    prefix_failure, require_resolved, session_info_by_prefix,
};
use crate::cli_output::{ErrorKind, Reporter, SessionRef, TagResult, stdoutln};

/// `Some(exit code)` when a payload is larger than the daemon would
/// ever admit. `--raw` rides `KeyBytes`, which the session's whole
/// input budget bounds; anything else is a paste, which must leave room
/// for its bracketing.
pub(super) fn over_limit_payload(out: &Reporter, len: usize, raw: bool) -> Option<i32> {
    use felis_protocol::limits::{MAX_PASTE_BYTES, PTY_INPUT_BUDGET};

    let (limit, kind) = if raw {
        (PTY_INPUT_BUDGET, "--raw payload")
    } else {
        (MAX_PASTE_BYTES, "paste")
    };
    (len > limit).then(|| {
        out.fail(
            ErrorKind::InvalidRequest,
            format!("send: {kind} is {len} bytes, past the daemon's {limit}-byte limit"),
        )
    })
}

pub(super) async fn cmd_send<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
    (wire, origins): (Vec<WireStep>, Vec<StepOrigin>),
    steps: &[SendStep],
    wait: Option<WaitParams>,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // An empty payload still resolves the target, so a caller learns
    // the full id; pasting nothing is a no-op, so nothing is sent.
    // `--key`, `--mouse` and `--wait` still take the full path.
    if wire.is_empty() && wait.is_none() {
        let (session, _short_id) = match session_info_by_prefix(&mut conn, out, id_prefix).await {
            Ok(v) => v,
            Err(code) => return Ok(code),
        };
        out.result(&SessionRef::new(session.id));
        return Ok(0);
    }

    if let Err(e) = check_session_addressed(conn.effective_minor) {
        return Ok(out.fail(ErrorKind::Unsupported, format!("send: {e}")));
    }
    let wrote_input = !wire.is_empty();
    let timeout_secs = wait.as_ref().and_then(|params| params.timeout);
    let deadline = crate::timeout_deadline(timeout_secs);

    let Some(params) = wait else {
        let outcome = match conn.send_steps(id_prefix.to_owned(), wire).await {
            Ok(outcome) => outcome,
            Err(e) => {
                return Ok(out.fail(ErrorKind::from_connect_error(&e), format!("send: {e}")));
            }
        };
        return Ok(match outcome {
            SendOutcome::Sent { id } => {
                out.result(&SessionRef::new(id));
                0
            }
            refused => report_refusal(out, id_prefix, &refused, &origins, steps),
        });
    };

    // The daemon registers the watch before it writes the first step,
    // so the `D` mark of even an instantly-finishing command reaches
    // this stream.
    let stream = match conn.open_send_and_wait(id_prefix.to_owned(), wire).await {
        Ok(stream) => stream,
        Err(e) => {
            return Ok(out.fail(ErrorKind::from_connect_error(&e), format!("send: {e}")));
        }
    };
    let outcome = match wait_for_command_end(&mut conn, out, stream, params.timeout, deadline).await
    {
        Ok(o) => o,
        Err(code) => return Ok(code),
    };
    Ok(match outcome {
        WaitOutcome::Refused(refused) => report_refusal(out, id_prefix, &refused, &origins, steps),
        WaitOutcome::NotTaken if wrote_input => out.fail(
            ErrorKind::Timeout,
            format!(
                "send: the session did not take the input within {}s — its child \
                 is not reading its stdin",
                timeout_secs.unwrap_or(0)
            ),
        ),
        // Nothing to take: the deadline only bounded the watch.
        WaitOutcome::NotTaken => report_wait_outcome(
            out,
            &WaitEnd::TimedOut {
                secs: timeout_secs.unwrap_or(0),
            },
        ),
        WaitOutcome::Ended(end) => report_wait_outcome(out, &end),
    })
}

/// Which command-line argument a wire step came from, so a refusal
/// names what the caller typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepOrigin {
    Payload,
    /// An index into the `--key` / `--mouse` steps.
    Arg(usize),
}

/// The payload, then each `--key` and `--mouse` argument as steps of
/// its own: under `?2004` the daemon brackets a whole paste, and a
/// keystroke inside the brackets is literal paste data. Built before
/// dialing, so a request no daemon would take fails as one.
pub(super) fn wire_steps(
    out: &Reporter,
    bytes: Vec<u8>,
    raw: bool,
    steps: &[SendStep],
) -> Result<(Vec<WireStep>, Vec<StepOrigin>), i32> {
    use felis_protocol::limits::MAX_SEND_STEPS;
    use felis_protocol::messages::Validate as _;

    if let Some(code) = over_limit_payload(out, bytes.len(), raw) {
        return Err(code);
    }

    let mut wire = Vec::new();
    let mut origins = Vec::new();
    if !bytes.is_empty() {
        wire.push(if raw {
            WireStep::Raw(bytes)
        } else {
            WireStep::Paste(bytes)
        });
        origins.push(StepOrigin::Payload);
    }
    for (index, step) in steps.iter().enumerate() {
        match step {
            SendStep::Key(chord) => {
                let event = chord_key_event(chord);
                // A chord the daemon would refuse fails here, with the
                // chord as typed, before the request is written.
                if let Err(e) = InputMsg::Key(event.clone()).validate() {
                    return Err(out.fail(
                        ErrorKind::InvalidRequest,
                        format!("send: --key {chord}: {e}"),
                    ));
                }
                wire.push(WireStep::Key(event));
                origins.push(StepOrigin::Arg(index));
            }
            SendStep::Mouse(arg) => {
                for event in arg.events() {
                    wire.push(WireStep::Mouse(event));
                    origins.push(StepOrigin::Arg(index));
                }
            }
        }
    }
    if wire.len() > MAX_SEND_STEPS {
        return Err(out.fail(
            ErrorKind::InvalidRequest,
            format!(
                "send: {} steps, past the daemon's {MAX_SEND_STEPS}-step limit",
                wire.len()
            ),
        ));
    }
    Ok((wire, origins))
}

/// A send that wrote nothing: an unresolved prefix, or a request the
/// daemon refused before its first step.
fn report_refusal(
    out: &Reporter,
    id_prefix: &str,
    outcome: &SendOutcome,
    origins: &[StepOrigin],
    steps: &[SendStep],
) -> i32 {
    let (reason, step, detail) = match outcome {
        SendOutcome::NoMatch => {
            return prefix_failure(
                out,
                &crate::SessionPrefixError::NoMatch {
                    prefix: id_prefix.to_owned(),
                },
            );
        }
        SendOutcome::Ambiguous { matches } => {
            return prefix_failure(
                out,
                &crate::SessionPrefixError::Ambiguous {
                    prefix: id_prefix.to_owned(),
                    matches: *matches as usize,
                },
            );
        }
        SendOutcome::Refused {
            reason,
            step,
            detail,
        } => (*reason, *step, detail),
        // Not a refusal; no caller passes one.
        SendOutcome::Sent { id } => {
            out.result(&SessionRef::new(*id));
            return 0;
        }
    };
    if reason == SendRefusal::SessionEnded {
        return out.fail(
            ErrorKind::SessionEnded,
            "send: the session ended before it took the input",
        );
    }
    let arg = step
        .and_then(|step| origins.get(step as usize))
        .and_then(|origin| match origin {
            StepOrigin::Arg(index) => steps.get(*index),
            StepOrigin::Payload => None,
        });
    let message = match arg {
        Some(SendStep::Mouse(arg)) => format!("send: --mouse {arg}: {detail}"),
        Some(SendStep::Key(chord)) => format!("send: --key {chord}: {detail}"),
        None => format!("send: {detail}"),
    };
    out.fail(ErrorKind::InvalidRequest, message)
}

pub(super) struct WaitParams {
    pub(super) timeout: Option<u64>,
}

/// One `--key` or `--mouse` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendStep {
    Key(felis_client_core::Chord),
    Mouse(MouseArg),
}

/// `--key` and `--mouse` in command-line order. The derive would collect
/// each flag into its own `Vec` and lose how the two interleave, so the
/// pair is read back by argv index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SendSteps(pub(crate) Vec<SendStep>);

const KEY_ID: &str = "keys";
const MOUSE_ID: &str = "mouse";

impl clap::FromArgMatches for SendSteps {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        fn indexed<'m, T: Clone + Send + Sync + 'static>(
            matches: &'m clap::ArgMatches,
            id: &str,
            step: impl Fn(T) -> SendStep + 'm,
        ) -> impl Iterator<Item = (usize, SendStep)> + 'm {
            let indices = matches.indices_of(id).into_iter().flatten();
            let values = matches.get_many::<T>(id).into_iter().flatten().cloned();
            indices.zip(values.map(step))
        }
        let mut steps: Vec<(usize, SendStep)> = indexed(matches, KEY_ID, SendStep::Key)
            .chain(indexed(matches, MOUSE_ID, SendStep::Mouse))
            .collect();
        steps.sort_by_key(|(index, _)| *index);
        Ok(Self(steps.into_iter().map(|(_, step)| step).collect()))
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl clap::Args for SendSteps {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        use clap::{Arg, ArgAction};

        cmd.arg(
            Arg::new(KEY_ID)
                .long("key")
                .value_name("CHORD")
                .action(ArgAction::Append)
                .value_parser(parse_chord_arg)
                .help(
                    "Press a named key after the text: `--key up`, `--key enter`, \
                     `--key ctrl+c` (repeatable, in order with `--mouse`)",
                )
                .long_help(
                    "Press a named key after the text: `--key up`, `--key enter`, \
                     `--key ctrl+c` (repeatable, in order with `--mouse`).\n\n\
                     Keys are encoded against current keyboard modes; hand-written \
                     `--raw` escapes send the wrong bytes to mode-switched programs.",
                ),
        )
        .arg(
            Arg::new(MOUSE_ID)
                .long("mouse")
                .value_name("EVENT")
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(MouseArg))
                .help(
                    "Send a mouse event after the text: `--mouse click:12,4` clicks \
                     0-based column 12, row 4 (repeatable, in order with `--key`)",
                )
                .long_help(
                    "Send a mouse event after the text: `--mouse click:12,4` clicks \
                     0-based column 12, row 4 (repeatable, in order with `--key`).\n\n\
                     EVENT is `[MODS+]ACTION[-BUTTON]:COL,ROW`. ACTION is click, press, \
                     drag, release, wheel-up, wheel-down, wheel-left or wheel-right; \
                     BUTTON is left (the default), middle or right; MODS are shift, alt \
                     and ctrl. COL,ROW count from 0, as `search` and `capture` do.\n\n\
                     Encoded against the program's mouse modes. Refused with nothing \
                     sent when the program has not turned on reporting for the event \
                     or the cell is off the grid.",
                ),
        )
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

/// The keymap chord grammar (docs/reference/config.md `[keymap]`), so
/// `--key up` and a config binding spell a keystroke identically; a
/// parse failure is a clap usage error (exit 2).
pub(super) fn parse_chord_arg(s: &str) -> Result<felis_client_core::Chord, String> {
    s.parse().map_err(|e| format!("{e}"))
}

/// The wire form of one `--key` chord, so a GUI keystroke and a chord
/// reach the daemon's encoder as the same kind of fact.
pub(super) fn chord_key_event(
    chord: &felis_client_core::Chord,
) -> felis_protocol::messages::KeyEvent {
    use felis_client_core::KeyCode;
    use felis_protocol::messages::{Key, KeyEvent, KeyEventKind, KeyLocation};

    let (key, text) = match &chord.key {
        KeyCode::Named(n) => (Key::Named(*n), None),
        KeyCode::Character(s) => {
            // The parser folds `A` to `shift+a`; a keyboard reports that
            // keystroke as `A`, and its composed text is `A`.
            let s =
                if chord.mods.shift_key() && s.len() == 1 && s.as_bytes()[0].is_ascii_lowercase() {
                    s.to_ascii_uppercase()
                } else {
                    s.clone()
                };
            // `text` is the OS-composed text a real keyboard would
            // deliver: absent under Ctrl/Alt/Super, where the encoder
            // derives control bytes / escape forms itself.
            let composed =
                (!chord.mods.control_key() && !chord.mods.alt_key() && !chord.mods.super_key())
                    .then(|| s.clone());
            (Key::Character(s), composed)
        }
    };
    KeyEvent {
        key,
        text,
        mods: chord.mods,
        kind: KeyEventKind::Press,
        // A chord names no side of the keyboard, and DECKPAM is the only
        // encoding a location changes.
        location: KeyLocation::Standard,
    }
}

enum WaitOutcome {
    Ended(WaitEnd),
    /// The deadline passed before the session took the input.
    NotTaken,
    /// The daemon wrote nothing; the stream's first item says why.
    Refused(SendOutcome),
}

/// How a wait on a send the session took ended.
enum WaitEnd {
    /// `exit_code` is `None` for a bare `OSC 133 ; D`.
    Completed {
        id: u128,
        exit_code: Option<u32>,
    },
    TimedOut {
        secs: u64,
    },
    SessionEnded,
}

/// Drain the `SendAndWait` stream to its `Mark`. `Err(code)` is
/// transport failure; timeout and session end are data. The caller's
/// `deadline` bounds the whole stream, the send's admission included.
async fn wait_for_command_end<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
    stream: StreamId,
    timeout: Option<u64>,
    deadline: Option<tokio::time::Instant>,
) -> Result<WaitOutcome, i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut sent: Option<u128> = None;
    let mut eof = false;
    let mut failed = false;
    let mut settled: Option<WaitOutcome> = None;
    drain_frames(
        conn,
        Some(stream),
        deadline,
        std::time::Duration::from_secs(1),
        |tick| match tick {
            // A quiet second is the normal case while the command runs.
            DrainTick::TimedOut => Stall::Retry,
            DrainTick::DeadlineExceeded => Stall::Stop,
            DrainTick::Eof => {
                eof = true;
                Stall::Stop
            }
            DrainTick::Error(e) => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                format!("wait: connection error: {e}"),
            )),
            DrainTick::Corrupt(err) => Stall::Fail(out.fail(
                ErrorKind::Protocol,
                format!("wait: daemon sent an unreadable frame: {err}"),
            )),
        },
        |msg| match msg {
            Drained::Item(OpsToClientMsg::WaitSent {
                outcome: SendOutcome::Sent { id },
            }) => {
                sent = Some(id);
                ControlFlow::Continue(())
            }
            Drained::Item(OpsToClientMsg::WaitSent { outcome }) => {
                settled = Some(WaitOutcome::Refused(outcome));
                ControlFlow::Break(())
            }
            Drained::Item(OpsToClientMsg::Mark { exit_code }) => {
                settled = sent.map(|id| WaitOutcome::Ended(WaitEnd::Completed { id, exit_code }));
                ControlFlow::Continue(())
            }
            Drained::Item(_) => ControlFlow::Continue(()),
            Drained::End { .. } => ControlFlow::Break(()),
            // The daemon ends the watch `Unavailable` when the session
            // ends with no mark.
            Drained::Failed { .. } => {
                failed = true;
                ControlFlow::Break(())
            }
        },
    )
    .await?;
    Ok(settled.unwrap_or_else(|| {
        if eof || failed {
            WaitOutcome::Ended(WaitEnd::SessionEnded)
        } else if sent.is_none() {
            WaitOutcome::NotTaken
        } else {
            WaitOutcome::Ended(WaitEnd::TimedOut {
                secs: timeout.unwrap_or(0),
            })
        }
    }))
}

/// `0` a command completed (its exit code goes to stdout, never this
/// process's status), `1` timeout or session end. Human framing prints
/// the code in decimal, `-` for a bare `D`; the result object carries
/// `exit_code`, omitted for a bare `D`.
fn report_wait_outcome(out: &Reporter, outcome: &WaitEnd) -> i32 {
    match outcome {
        WaitEnd::Completed { id, exit_code } => {
            if out.machine() {
                out.result(&SessionRef {
                    exit_code: *exit_code,
                    ..SessionRef::new(*id)
                });
            } else {
                match exit_code {
                    Some(code) => stdoutln!("{code}"),
                    None => stdoutln!("-"),
                }
            }
            0
        }
        WaitEnd::TimedOut { secs } => out.fail(
            ErrorKind::Timeout,
            format!("wait: no command completed within {secs}s"),
        ),
        WaitEnd::SessionEnded => out.fail(
            ErrorKind::SessionEnded,
            "wait: session ended before a command completed",
        ),
    }
}

pub(super) async fn cmd_kill<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let resolved = match conn.destroy_session(id_prefix.to_owned()).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                format!("kill session `{id_prefix}`: {e}"),
            ));
        }
    };
    let id = match require_resolved(out, resolved, id_prefix) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    out.result(&SessionRef::new(id));
    Ok(0)
}

pub(super) async fn cmd_tag<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
    add: Vec<String>,
    remove: Vec<String>,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (resolved, tags, denied) = match conn.set_tags(id_prefix.to_owned(), add, remove).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                format!("tag session `{id_prefix}`: {e}"),
            ));
        }
    };
    let resolved = match require_resolved(out, resolved, id_prefix) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    if let Some(reason) = denied {
        // Exit 1 like a failed resolution: the daemon refused the
        // request, not the transport.
        return Ok(out.fail(
            ErrorKind::InvalidRequest,
            format!("tag session `{id_prefix}`: {reason}"),
        ));
    }
    // An emptied set prints a blank line / empty array with exit 0,
    // distinguishable from the not-found exit 1.
    if out.machine() {
        out.result(&tag_result(resolved, &tags));
    } else {
        stdoutln!("{}", tags.join(" "));
    }
    Ok(0)
}

/// `tags` is always present and arrives sorted from the daemon,
/// matching `list`/`info`.
pub(super) fn tag_result(id: u128, tags: &[String]) -> TagResult {
    TagResult {
        id: SessionHex(id).to_string(),
        tags: tags.to_vec(),
    }
}

/// Exit 0 once the session is back in the pool, whether or not a
/// client had to be evicted: the post-condition is the same.
/// `was_attached` informs the result object, not the exit code.
pub(super) async fn cmd_evict<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (resolved, was_attached) = match conn.force_detach(id_prefix.to_owned()).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                format!("evict session `{id_prefix}`: {e}"),
            ));
        }
    };
    let id = match require_resolved(out, resolved, id_prefix) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    tracing::debug!(was_attached, "evict complete");
    out.result(&SessionRef {
        was_attached: Some(was_attached),
        ..SessionRef::new(id)
    });
    Ok(0)
}

pub(super) fn parse_env_pair(raw: &str) -> Result<(String, String), String> {
    raw.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("`{raw}` is not `KEY=VAL`"))
}

/// Explicit `--cwd` wins. Local spawns inherit caller's cwd (or anchor
/// relative `--cwd` to it) rather than using the daemon's frozen cwd.
/// Remote `--host` spawns default to the remote cwd and pass relative
/// paths through untouched. Not `to_string_lossy`: the daemon chdirs
/// into what it is sent, so a lossy path spawns nothing.
pub(super) fn resolve_spawn_cwd(
    explicit: Option<&str>,
    is_local: bool,
    current: Option<PathBuf>,
) -> Result<String> {
    let utf8 = |p: PathBuf| {
        p.to_str()
            .map(str::to_owned)
            .with_context(|| format!("the spawn cwd `{}` is not valid UTF-8", p.display()))
    };
    match explicit {
        Some(c) if is_local && !Path::new(c).is_absolute() => {
            current.map_or_else(|| Ok(c.to_string()), |dir| utf8(dir.join(c)))
        }
        Some(c) => Ok(c.to_string()),
        None if is_local => current.map_or_else(|| Ok(String::new()), utf8),
        None => Ok(String::new()),
    }
}

/// Staged pre-dial because the cwd resolution reads this process's own
/// cwd. Empty `cmd` falls back to the daemon's default factory.
pub(super) fn spawn_args_for(
    target: &Reconnector,
    cwd: Option<&str>,
    env: &[(String, String)],
    rows: Option<u32>,
    cols: Option<u32>,
    tags: Vec<String>,
    cmd: &[String],
) -> Result<felis_protocol::messages::SpawnArgs> {
    let args = felis_protocol::messages::SpawnArgs {
        cwd: resolve_spawn_cwd(
            cwd,
            matches!(target.carrier, Carrier::Local(_)),
            std::env::current_dir().ok(),
        )?,
        env: env.to_vec(),
        // Absent geometry is the daemon's default; the flags are a
        // mutually-required pair, so a half-named grid never gets here.
        dims: rows
            .zip(cols)
            .map(|(rows, cols)| felis_protocol::messages::RequestedDims {
                rows,
                cols,
                pixel_w: 0,
                pixel_h: 0,
            }),
        tags,
        ..spawn_args_from_cli(cmd.to_vec())
    };
    // Same carrier question as the cwd: only a local dial's environment
    // describes the host the child will run on.
    Ok(felis_client_core::env_base::fill_for_carrier(
        args,
        &target.carrier,
    ))
}

pub(super) async fn spawn_on<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    spawn_args: felis_protocol::messages::SpawnArgs,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let ack = match conn.spawn_session(spawn_args).await {
        Ok(a) => a,
        // A typed refusal is not a protocol break: the daemon named a
        // reason in a frame this build understands.
        Err(ConnectError::CreateFailed { reason, detail }) => {
            return Ok(out.fail(
                ErrorKind::from_create_failure(reason),
                format!("spawn session: {detail}"),
            ));
        }
        Err(e) => {
            return Ok(out.fail(ErrorKind::Protocol, format!("spawn session: {e}")));
        }
    };
    // Human framing prints the bare full id, pipeable into
    // `felis attach $(felis sessions spawn ...)`; only the full id
    // carries the never-reused guarantee, and this one is meant to be
    // stored.
    if out.machine() {
        out.result(&SessionRef::new(ack.id));
    } else {
        stdoutln!("{}", SessionHex(ack.id));
    }
    Ok(0)
}

/// Bare `-` is the only stdin trigger; anything else is the payload
/// itself.
pub(super) fn collect_send_payload(
    text: &str,
    cap: usize,
    stdin: impl Read,
) -> Result<(Vec<u8>, usize)> {
    if text == "-" {
        return read_send_payload(stdin, cap);
    }
    Ok((text.as_bytes().to_vec(), text.len()))
}

/// Reads bounded payload to prevent allocating huge files before refusal.
/// Reads up to `cap + 1` bytes into memory, then counts any remaining
/// bytes without buffering them so refusals report total payload size.
pub(super) fn read_send_payload(mut source: impl Read, cap: usize) -> Result<(Vec<u8>, usize)> {
    let mut buf = Vec::new();
    (&mut source)
        .take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .context("read text from stdin")?;
    let mut len = buf.len();
    if len > cap {
        let rest =
            std::io::copy(&mut source, &mut std::io::sink()).context("read text from stdin")?;
        len = len.saturating_add(usize::try_from(rest).unwrap_or(usize::MAX));
    }
    Ok((buf, len))
}
