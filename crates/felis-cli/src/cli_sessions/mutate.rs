use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use felis_client_core::{Carrier, ConnectError, Connection, Reconnector, spawn_args_from_cli};
use felis_protocol::{SessionHex, messages::InputMsg};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    attach_by_prefix, detach,
    driver::{DrainTick, Drained, Stall, drain_frames},
    require_resolved, session_info_by_prefix,
};
use crate::cli_output::{ErrorKind, Reporter, SessionRef, TagResult};

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
    bytes: Vec<u8>,
    raw: bool,
    keys: &[felis_client_core::Chord],
    wait: Option<WaitParams>,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use felis_protocol::messages::Validate as _;

    // Refused here rather than on the wire: the daemon answers an
    // over-budget input frame by closing the connection, which reaches
    // the caller as `daemon_lost` (exit 2, "retry") when the condition
    // is a permanent, request-level one this process can see before it
    // writes a byte.
    if let Some(code) = over_limit_payload(out, bytes.len(), raw) {
        return Ok(code);
    }

    // An empty payload still resolves the target, so a caller learns
    // the full id; pasting nothing is a no-op, so the attach is
    // skipped. `--key` and `--wait` still take the full path.
    if bytes.is_empty() && keys.is_empty() && wait.is_none() {
        let (session, _short_id) = match session_info_by_prefix(&mut conn, out, id_prefix).await {
            Ok(v) => v,
            Err(code) => return Ok(code),
        };
        out.result(&SessionRef::new(session.id));
        return Ok(0);
    }

    let resolved = match attach_by_prefix(&mut conn, out, id_prefix).await {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let reached = || SessionRef::new(resolved);

    // Drain the rehydrate burst before injecting input: an unread outbox
    // makes the daemon pump hit EPIPE mid-burst and drop the input frame.
    {
        use felis_protocol::messages::GridMsg;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let drained = drain_frames(
            &mut conn,
            None,
            Some(deadline),
            std::time::Duration::from_secs(1),
            |tick| match tick {
                DrainTick::DeadlineExceeded => Stall::Fail(out.fail(
                    ErrorKind::DaemonLost,
                    "send: the daemon did not finish syncing session state within 5s",
                )),
                DrainTick::TimedOut | DrainTick::Eof | DrainTick::Error(_) => {
                    Stall::Fail(out.fail(
                        ErrorKind::DaemonLost,
                        "send: connection lost while syncing session state",
                    ))
                }
                DrainTick::Corrupt(err) => Stall::Fail(out.fail(
                    ErrorKind::Protocol,
                    format!("send: daemon sent an unreadable frame: {err}"),
                )),
            },
            |msg| {
                // The burst opens no stream; `RehydrateEnd` is its only
                // terminator.
                let Drained::Item(msg) = msg else {
                    return ControlFlow::Continue(());
                };
                if matches!(msg, GridMsg::RehydrateEnd) {
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            },
        )
        .await;
        if let Err(code) = drained {
            return Ok(code);
        }
    }

    // Built before any frame goes out: a chord the daemon would refuse
    // at admission must fail the whole command rather than deliver a
    // truncated key sequence.
    let mut key_frames: Vec<InputMsg> = Vec::with_capacity(keys.len());
    for chord in keys {
        let msg = InputMsg::Key(chord_key_event(chord));
        if let Err(e) = msg.validate() {
            return Ok(out.fail(
                ErrorKind::InvalidRequest,
                format!("send: --key {chord}: {e}"),
            ));
        }
        key_frames.push(msg);
    }

    let wrote_input = !bytes.is_empty() || !key_frames.is_empty();
    if !bytes.is_empty() {
        let msg = if raw {
            InputMsg::KeyBytes(bytes)
        } else {
            InputMsg::Paste(bytes)
        };
        if let Err(e) = conn.writer.send_unflushed(&msg).await {
            return Ok(out.fail(
                ErrorKind::DaemonLost,
                format!("send: write input frame: {e}"),
            ));
        }
    }

    // Each chord rides its own frame *after* the paste frame, never
    // appended to the paste payload: under `?2004` the daemon brackets
    // the whole Paste body, and a keystroke inside the brackets is
    // literal paste data (`--key enter` must run the line).
    for msg in key_frames {
        if let Err(e) = conn.writer.send_unflushed(&msg).await {
            return Ok(out.fail(
                ErrorKind::DaemonLost,
                format!("send: write --key frame: {e}"),
            ));
        }
    }

    // A `Session::InputFence` proves input passed admission before closing the
    // socket; otherwise closing while the daemon pump is parked behind the byte
    // budget abandons the input. `--timeout` bounds this confirmation so a
    // wedged child does not hang.
    let timeout_secs = wait.as_ref().and_then(|params| params.timeout);
    let deadline = crate::timeout_deadline(timeout_secs);
    if wrote_input {
        let confirmed = match deadline {
            // `timeout_at` polls its inner future before the expired
            // timer, so an elapsed deadline is checked on its own: a
            // reply already in the read-ahead would otherwise confirm
            // an admission past the caller's deadline.
            Some(at) if at <= tokio::time::Instant::now() => Err(()),
            Some(at) => tokio::time::timeout_at(at, conn.input_fence())
                .await
                .map_err(|_elapsed| ()),
            None => Ok(conn.input_fence().await),
        };
        match confirmed {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Ok(out.fail(
                    ErrorKind::DaemonLost,
                    format!("send: confirming the session took the input: {e}"),
                ));
            }
            Err(()) => {
                return Ok(out.fail(
                    ErrorKind::Timeout,
                    format!(
                        "send: the session did not take the input within {}s — its child \
                         is not reading its stdin",
                        timeout_secs.unwrap_or(0)
                    ),
                ));
            }
        }
    }

    // This connection subscribed before the input frames went out, so
    // the D mark of even an instantly-finishing command still streams
    // here; a separately-connecting verb could not close that race.
    if let Some(params) = wait {
        let outcome = match wait_for_command_end(&mut conn, out, params.timeout, deadline).await {
            Ok(o) => o,
            Err(code) => return Ok(code),
        };
        let code = report_wait_outcome(out, &outcome, &reached);
        if !matches!(outcome, WaitOutcome::SessionEnded) {
            // Best-effort: the pool re-pools on drop anyway; detach
            // skips the ~200 ms grace tick.
            drop(detach(&mut conn).await);
        }
        return Ok(code);
    }

    if let Err(e) = detach(&mut conn).await {
        return Ok(out.fail(ErrorKind::DaemonLost, format!("send: {e}")));
    }
    out.result(&reached());
    Ok(0)
}

pub(super) struct WaitParams {
    pub(super) timeout: Option<u64>,
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
    /// `exit_code` is `None` for a bare `OSC 133 ; D`.
    Completed {
        exit_code: Option<u32>,
    },
    TimedOut {
        secs: u64,
    },
    SessionEnded,
}

/// Block until the next `CommandEnd` prompt mark.
/// The caller must drain past `RehydrateEnd` so historical marks are skipped.
/// `Err(code)` is transport failure; timeout and EOF are data. The caller's
/// `deadline` bounds the entire post-write phase.
async fn wait_for_command_end<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
    timeout: Option<u64>,
    deadline: Option<tokio::time::Instant>,
) -> Result<WaitOutcome, i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use felis_protocol::messages::GridMsg;
    use felis_protocol::messages::PromptKind;

    let mut ended = false;
    let mut completed: Option<WaitOutcome> = None;
    drain_frames(
        conn,
        None,
        deadline,
        std::time::Duration::from_secs(1),
        |tick| match tick {
            // A quiet second is the normal case while the command runs.
            DrainTick::TimedOut => Stall::Retry,
            DrainTick::DeadlineExceeded => Stall::Stop,
            DrainTick::Eof => {
                ended = true;
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
        // The watch opens no stream: only the `CommandEnd` mark or a
        // stall ends it.
        |msg| {
            let Drained::Item(msg) = msg else {
                return ControlFlow::Continue(());
            };
            if let GridMsg::PromptMark {
                kind: PromptKind::CommandEnd,
                exit_code,
                ..
            } = msg
            {
                completed = Some(WaitOutcome::Completed { exit_code });
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        },
    )
    .await?;
    Ok(completed.unwrap_or_else(|| {
        if ended {
            WaitOutcome::SessionEnded
        } else {
            WaitOutcome::TimedOut {
                secs: timeout.unwrap_or(0),
            }
        }
    }))
}

/// `0` a command completed (its exit code goes to stdout, never this
/// process's status), `1` timeout or session end. Human framing prints
/// the code in decimal, `-` for a bare `D`; the result object carries
/// `exit_code`, omitted for a bare `D`.
fn report_wait_outcome(
    out: &Reporter,
    outcome: &WaitOutcome,
    reached: &impl Fn() -> SessionRef,
) -> i32 {
    match outcome {
        WaitOutcome::Completed { exit_code } => {
            if out.machine() {
                out.result(&SessionRef {
                    exit_code: *exit_code,
                    ..reached()
                });
            } else {
                match exit_code {
                    Some(code) => println!("{code}"),
                    None => println!("-"),
                }
            }
            0
        }
        WaitOutcome::TimedOut { secs } => out.fail(
            ErrorKind::Timeout,
            format!("wait: no command completed within {secs}s"),
        ),
        WaitOutcome::SessionEnded => out.fail(
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
        println!("{}", tags.join(" "));
    }
    Ok(0)
}

/// `tags` is always present and arrives sorted from the daemon,
/// matching `list`/`info`. Shared with the bridge's `sessions.tag`.
pub(crate) fn tag_result(id: u128, tags: &[String]) -> TagResult {
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
        println!("{}", SessionHex(ack.id));
    }
    Ok(0)
}

/// Bare `-` is the only stdin trigger; anything else is the payload
/// itself.
pub(super) fn collect_send_payload(text: &str, cap: usize) -> Result<(Vec<u8>, usize)> {
    if text == "-" {
        return read_send_payload(std::io::stdin(), cap);
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
