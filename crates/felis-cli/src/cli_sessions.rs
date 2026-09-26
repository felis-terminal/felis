//! `felis sessions …`: the CLI surface for daemon IPC operations.
//! Scripted automation rides the `felis` binary under `sessions` rather
//! than a separate `felisctl` (docs/reference/ipc.md "CLI clients"). Only
//! `spawn` auto-spawns the daemon; every other verb exits 2 on a cold socket.

#![expect(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "this module's job is printing to stdout/stderr"
)]

mod attachment;
mod driver;
mod mutate;
mod read;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use self::attachment::RETARGET_DIAL;
pub(crate) use self::attachment::{
    RetargetArgs, RetargetFlags, local_carrier, reject_global_carrier, run_retarget,
};
pub(crate) use self::mutate::tag_result;
pub(crate) use self::read::search_match;

use std::future::Future;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use clap::builder::TypedValueParser as _;
use felis_client_core::{AttachIntent, CarrierConnection, ConnectError, Connection, Reconnector};
use felis_protocol::convert::WireError;
use felis_protocol::messages::{
    InfoOutcome, MAX_PASTE_BYTES, MAX_RAW_INPUT_BYTES, MAX_SEARCH_PATTERN_BYTES, ResolvedId,
    SessionInfo, SwitchScope, Validate, check_limit,
};
use felis_protocol::messages::{RegionSource, SessionToDaemonMsg};
use tokio::io::{AsyncRead, AsyncWrite};

use self::attachment::{cmd_switch, require_switch_from};
use self::mutate::{
    WaitParams, cmd_evict, cmd_kill, cmd_send, cmd_tag, collect_send_payload, parse_chord_arg,
    parse_env_pair, spawn_args_for, spawn_on,
};
use self::read::{RowEncoding, cmd_capture, cmd_info, cmd_list, cmd_search};
use crate::cli_output::{ErrorKind, Format, PointFormat, Reporter, StreamFormat};
use crate::conn::Dial;
use crate::session_id_from_resolved;

/// The roster is an `Ops::List` query, not part of `Welcome`.
async fn roster<R, W>(conn: &mut Connection<R, W>) -> Result<Vec<SessionInfo>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    conn.list_sessions()
        .await
        .context("list sessions from daemon")
}

/// A roster failure after a successful connect is a wire failure: exit
/// 2, never the exit-1 "no such session" class a bare `?` would report
/// (docs/reference/cli.md exit contract). Every id-taking verb goes
/// through this.
async fn checked_roster<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
) -> Result<Vec<SessionInfo>, i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    roster(conn)
        .await
        .map_err(|e| out.fail(ErrorKind::Protocol, e))
}

/// Exit 1 with the same remedies the client-side resolver prints, so
/// "which session?" failures have one voice.
fn require_resolved(out: &Reporter, resolved: ResolvedId, prefix: &str) -> Result<u128, i32> {
    session_id_from_resolved(resolved, prefix)
        .map_err(|e| out.fail(ErrorKind::from_prefix_error(&e), e))
}

/// The one rendering of a "which session?" failure, whichever side
/// resolved the prefix.
fn prefix_failure(out: &Reporter, err: &crate::SessionPrefixError) -> i32 {
    out.fail(ErrorKind::from_prefix_error(err), err)
}

/// One session's row and its display prefix, both resolved against the
/// daemon's pool in one `Ops::Info` reply.
async fn session_info_by_prefix<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
) -> Result<(SessionInfo, String), i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let outcome = conn
        .session_info(id_prefix.to_owned())
        .await
        .map_err(|e| out.fail(ErrorKind::Protocol, format!("session info: {e}")))?;
    match outcome {
        InfoOutcome::Found { session, short_id } => Ok((*session, short_id)),
        InfoOutcome::NoMatch => Err(prefix_failure(
            out,
            &crate::SessionPrefixError::NoMatch {
                prefix: id_prefix.to_owned(),
            },
        )),
        InfoOutcome::Ambiguous { matches } => Err(prefix_failure(
            out,
            &crate::SessionPrefixError::Ambiguous {
                prefix: id_prefix.to_owned(),
                matches: matches as usize,
            },
        )),
    }
}

/// Attach by prefix, resolved inside the daemon. Returns the full id
/// the attach landed on.
async fn attach_by_prefix<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
) -> Result<u128, i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match conn
        .attach_by_prefix(id_prefix.to_owned(), AttachIntent::Deliberate)
        .await
    {
        Ok(info) => Ok(info.id),
        // `NoMatch` carries no count, so it renders the resolver's own
        // sentence; `Ambiguous` carries one only in `detail`, which is
        // where the number a caller needs lives.
        Err(ConnectError::AttachFailed {
            reason: felis_protocol::messages::AttachFailure::NoMatch,
            ..
        }) => Err(prefix_failure(
            out,
            &crate::SessionPrefixError::NoMatch {
                prefix: id_prefix.to_owned(),
            },
        )),
        Err(ConnectError::AttachFailed { reason, detail }) => Err(out.fail(
            ErrorKind::from_attach_failure(reason),
            format!("attach session `{id_prefix}`: {detail}"),
        )),
        Err(e) => Err(out.fail(
            ErrorKind::from_connect_error(&e),
            format!("attach session `{id_prefix}`: {e}"),
        )),
    }
}

/// A socket close would re-pool too, but the connection-drop path
/// takes ~200 ms (the post-exit grace tick); explicit Detach is
/// immediate.
async fn detach<R, W>(conn: &mut Connection<R, W>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    conn.writer
        .send(&SessionToDaemonMsg::Detach)
        .await
        .context("write Detach")?;
    Ok(())
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionOp {
    /// List sessions, one line each. `--format json` emits one object
    /// holding the whole roster.
    List {
        #[command(flatten)]
        output: PointFormat,
        /// Show only sessions carrying at least one of the given tags
        /// (repeatable; the match is "any of"). Tags are set with
        /// `sessions tag` or `spawn --tag`.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Print one session's details: id, size, idle age, title, cwd,
    /// tags.
    Info {
        /// Hex id or any unique prefix; optional `0x` strip.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Type text into a session, window attached or not.
    ///
    /// Pasted into the session (bracketed paste if requested); `--raw`
    /// sends bytes untouched. CLI input lands alongside GUI keystrokes.
    Send {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        /// Literal text to send. `-` reads from stdin.
        #[arg(value_name = "TEXT", required_unless_present_any = ["wait", "keys"])]
        text: Option<String>,
        /// Send the bytes untouched, without bracketed-paste quoting.
        /// Use when injecting control bytes or escape sequences that
        /// must not be quoted as paste data.
        #[arg(long, requires = "text")]
        raw: bool,
        /// Block until the next command completes and print its exit code:
        /// decimal on stdout, `-` for bare `OSC 133 ; D`.
        ///
        /// Subscription starts before input injection. With no text or keys,
        /// watches an existing command. Needs OSC 133 shell integration.
        #[arg(long)]
        wait: bool,
        /// With `--wait`: give up after this many seconds (exit 1).
        /// Without it, wait indefinitely.
        ///
        /// `0` is a deadline already past: the wait gives up at once.
        #[arg(long, value_name = "SECS", requires = "wait")]
        timeout: Option<u64>,
        #[command(flatten)]
        output: PointFormat,
        /// Press a named key after the text: `--key up`, `--key enter`,
        /// `--key ctrl+c` (repeatable, pressed in order).
        ///
        /// Keys are encoded against current keyboard modes; hand-written
        /// `--raw` escapes send the wrong bytes to mode-switched programs.
        #[arg(
            long = "key",
            value_name = "CHORD",
            value_parser = parse_chord_arg
        )]
        keys: Vec<felis_client_core::Chord>,
    },
    /// Terminate a session and remove it from the daemon.
    ///
    /// `--format json` emits `{"id":"<hex>"}` with the resolved full
    /// id; an unknown or ambiguous prefix is a typed error object on
    /// stderr with exit 1, never a result.
    Kill {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Evict every window attached to a session; the session lives on.
    ///
    /// Returns to the detached pool. Exits 0 whether a window was
    /// attached or not, 1 for no such session. Named `evict` because
    /// `detach` is the one-window key action.
    Evict {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Print a session's screen as plain text.
    ///
    /// Soft-wrapped rows are stitched back into one logical line.
    /// `--format jsonl` emits one unstitched object per row (`text`, `row`,
    /// `soft_wrap_continued`) followed by the stream terminal.
    Capture {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        #[command(flatten)]
        output: StreamFormat,
        /// Reproduce each cell's color and style as inline escapes.
        #[arg(long)]
        ansi: bool,
        /// Region to capture: `visible` (default), `scrollback`,
        /// `command-output` / `last-command` (the last OSC 133
        /// command's ranges; their `--format jsonl` rows count from 0 within
        /// the region).
        #[arg(long, value_name = "SOURCE", default_value = "visible",
              value_parser = clap::builder::EnumValueParser::<SourceArg>::new()
                  .map(RegionSource::from))]
        source: RegionSource,
        /// Print only the last N rows of the selected source (the tail an
        /// agent polling a session wants). A filter, not a renumbering:
        /// `--format jsonl` row indices keep untrimmed capture values.
        #[arg(long, value_name = "N")]
        lines: Option<u32>,
    },
    /// Create a detached session and print its id.
    ///
    /// The one sessions verb that starts a daemon when none is running,
    /// on the host it dials, `--host` included. Bare `spawn` runs the
    /// shell a new window would; `-- <cmd> [args...]` runs another.
    Spawn {
        /// Set working directory for the child.
        #[arg(long, value_name = "PATH")]
        cwd: Option<String>,
        /// Environment overrides: `KEY=VAL`. May be repeated. Applied
        /// last, over the daemon's inherited-then-sanitized env and
        /// its identity stamps; `FELIS_SESSION_ID` is rejected.
        // Validated at the clap layer so a malformed value exits 2 like
        // every other usage error, not 1 via anyhow.
        #[arg(long, value_name = "KEY=VAL", value_parser = parse_env_pair)]
        env: Vec<(String, String)>,
        /// Initial geometry, rows. Must be given with `--cols`;
        /// neither means the daemon's default grid.
        // The wire field's own width, not a `u16`: the daemon admits
        // geometry once, and a second bound here would refuse at the
        // flag what a newer daemon accepts, in different words.
        #[arg(long, value_name = "N", requires = "cols")]
        rows: Option<u32>,
        /// Initial geometry, cols. Must be given with `--rows`.
        #[arg(long, value_name = "N", requires = "rows")]
        cols: Option<u32>,
        /// Label the session at creation (repeatable). These are the same
        /// labels `sessions tag` adds, but already set when the
        /// session first becomes listable, so a `list --tag` filter
        /// can never catch it untagged.
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Program + args. Empty: use the daemon's default shell.
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Point one window attached to a session at another session.
    ///
    /// The by-id counterpart of the in-window switch keybinding.
    /// Moves one window (defaults to this one under `$FELIS_SESSION_ID`).
    Switch {
        /// Session to switch to: hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        /// Session whose attached window should move. Defaults to
        /// `$FELIS_SESSION_ID` (the session this command runs in).
        #[arg(long, value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        from: Option<String>,
        #[command(flatten)]
        attachment: AttachmentTarget,
        #[command(flatten)]
        output: PointFormat,
    },
    /// Search a session's scrollback and screen.
    ///
    /// Matches stream grep-style as `<line>:<text>` (line -1 is newest
    /// scrollback row). Substring match by default, `--regex` for regex.
    Search {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        /// Pattern to look for. Substring by default; with `--regex`
        /// the daemon's `regex` crate parses it.
        #[arg(value_name = "PATTERN")]
        pattern: String,
        /// Treat the pattern as a regex.
        #[arg(long)]
        regex: bool,
        /// Case-insensitive match.
        #[arg(long)]
        case_insensitive: bool,
        #[command(flatten)]
        output: StreamFormat,
    },
    /// Add and remove labels, for pickers and scripts to filter on.
    ///
    /// Positional TAGs are added; `--remove <TAG>` removes. Both may
    /// appear in one call for atomic relabeling. Re-adding an existing
    /// tag or removing an absent one is a no-op.
    Tag {
        /// Hex id or any unique prefix.
        #[arg(value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        id: String,
        /// Labels to add.
        #[arg(value_name = "TAG", required_unless_present = "remove")]
        tags: Vec<String>,
        /// Label to remove (repeatable).
        #[arg(long = "remove", value_name = "TAG")]
        remove: Vec<String>,
        #[command(flatten)]
        output: PointFormat,
    },
}

/// A **decimal string**, not an integer: the machine output spells the
/// `u64` id as a string (docs/reference/cli.md "Machine output"), and
/// accepting the same spelling lets `jq -r` pipe straight back in.
#[derive(Debug, Clone, Copy, clap::Args)]
pub(crate) struct AttachmentTarget {
    /// Move this attachment specifically, instead of the session's
    /// last window input owner. Ids come from `sessions list` /
    /// `sessions info` with `--format json`; an id that has since detached is an
    /// error, never a redirect to another window.
    #[arg(long = "attachment", value_name = "ID", value_parser = parse_attachment_id)]
    pub(crate) attachment: Option<u64>,
}

impl AttachmentTarget {
    fn scope(&self) -> SwitchScope {
        self.attachment
            .map_or(SwitchScope::Default, SwitchScope::Attachment)
    }
}

/// Rejects the leading `+`/`-` and whitespace `u64::from_str` would
/// accept, so the error names the shape the roster prints.
pub(crate) fn parse_attachment_id(raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "`{raw}` is not an attachment id: expected decimal digits, as \
             `sessions info <id> --format json` prints them"
        ));
    }
    raw.parse()
        .map_err(|_| format!("attachment id `{raw}` is out of range"))
}

/// The wire's [`RegionSource`] is the one source vocabulary; the
/// keymap `pipe` action names the same regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SourceArg {
    Visible,
    Scrollback,
    CommandOutput,
    LastCommand,
}

impl From<SourceArg> for RegionSource {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Visible => Self::Visible,
            SourceArg::Scrollback => Self::Scrollback,
            SourceArg::CommandOutput => Self::CommandOutput,
            SourceArg::LastCommand => Self::LastCommand,
        }
    }
}

/// Every `SourceArg` spelling, in `value_variants` order, as `--source`
/// and the bridge accept it.
pub(crate) fn source_names() -> Vec<String> {
    use clap::ValueEnum as _;

    SourceArg::value_variants()
        .iter()
        .filter_map(|variant| Some(variant.to_possible_value()?.get_name().to_owned()))
        .collect()
}

/// `felis bridge` takes the same vocabulary as a JSON string rather
/// than through clap, so it spells the refusal out itself. The
/// spellings come from [`source_names`]: a literal list here would
/// still name a variant that has since been renamed away.
pub(crate) fn parse_region_source(raw: &str) -> Result<RegionSource, String> {
    <SourceArg as clap::ValueEnum>::from_str(raw, false)
        .map(RegionSource::from)
        .map_err(|_| {
            let expected = match source_names().as_slice() {
                [only] => only.clone(),
                [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
                [] => String::new(),
            };
            format!("unknown source `{raw}` (expected {expected})")
        })
}

/// Exit codes per docs/reference/ipc.md "CLI clients": 0 success, 1
/// op-domain failure, 2 daemon-unreachable / protocol error.
///
/// Order is contract: [`plan`] validates before dialing so bad
/// invocations exit before spinning up SSH relays.
pub(crate) fn run(
    runtime: &tokio::runtime::Runtime,
    op: SessionOp,
    target: &Reconnector,
) -> Result<i32> {
    runtime.block_on(async move {
        let plan = match plan(op, target) {
            ControlFlow::Continue(plan) => plan,
            ControlFlow::Break(code) => return Ok(code),
        };
        let Plan { dial, out, run } = plan;
        // Through the verb's own reporter: for a stream verb a failed
        // dial is a failure *before the first item*, which the contract
        // makes the stream's one terminal rather than a stderr line.
        let conn = match dial.open(target, &out).await {
            Ok(conn) => conn,
            Err(code) => return Ok(code),
        };
        run(conn).await
    })
}

type VerbBody = Box<dyn FnOnce(CarrierConnection) -> VerbFuture + Send>;
type VerbFuture = Pin<Box<dyn Future<Output = Result<i32>> + Send>>;

pub(crate) struct Plan {
    pub(crate) dial: Dial,
    /// Shared with the body: the dial that precedes it can fail and
    /// owes the same contract.
    out: Arc<Reporter>,
    run: VerbBody,
}

impl Plan {
    fn ops<F, Fut>(out: Arc<Reporter>, run: F) -> Self
    where
        F: FnOnce(CarrierConnection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self::new(Dial::Ops, out, run)
    }

    fn new<F, Fut>(dial: Dial, out: Arc<Reporter>, run: F) -> Self
    where
        F: FnOnce(CarrierConnection) -> Fut + Send + 'static,
        Fut: Future<Output = Result<i32>> + Send + 'static,
    {
        Self {
            dial,
            out,
            run: Box::new(move |conn| Box::pin(run(conn))),
        }
    }
}

/// Runs before the dial: a usage error exits 2, and the local reads
/// (stdin for `send`, this process's cwd for `spawn`) happen with no
/// live connection.
// One frame per short-lived invocation, no recursion.
#[allow(clippy::large_stack_frames)]
pub(crate) fn plan(op: SessionOp, target: &Reconnector) -> ControlFlow<i32, Plan> {
    let plan = match op {
        SessionOp::List { output, tags } => {
            let (out, body) = point_out(output.format);
            Plan::ops(out, move |conn| async move {
                cmd_list(conn, &body, &tags).await
            })
        }
        SessionOp::Info { id, output } => {
            let (out, body) = point_out(output.format);
            Plan::ops(
                out,
                move |conn| async move { cmd_info(conn, &body, &id).await },
            )
        }
        SessionOp::Send {
            id,
            text,
            raw,
            wait,
            timeout,
            output,
            keys,
        } => {
            // An empty payload is *not* a usage error: it must still
            // resolve the id so an unknown session reports 1;
            // `cmd_send` then skips the no-op paste.
            let (out, body) = point_out(output.format);
            let (field, cap) = if raw {
                ("Input::KeyBytes", MAX_RAW_INPUT_BYTES)
            } else {
                ("Input::Paste", MAX_PASTE_BYTES)
            };
            let (bytes, len) = match text.as_deref() {
                // A stdin that cannot be read is this process's own
                // pipe failing, reported in the verb's framing rather
                // than as a bare line (docs/reference/cli.md "Machine
                // output").
                Some(text) => match collect_send_payload(text, cap) {
                    Ok(payload) => payload,
                    Err(err) => {
                        return ControlFlow::Break(
                            out.fail(ErrorKind::InputFailed, format!("send: {err:#}")),
                        );
                    }
                },
                // clap has already refused a call with none of the
                // three.
                None => (Vec::new(), 0),
            };
            // The length alone decides it, so the payload is measured
            // where it lies: building an `InputMsg` to hand `validate`
            // would copy up to 16 MiB only to drop it again.
            if let Err(code) = refuse_breach(&out, "send", check_limit(field, len, cap)) {
                return ControlFlow::Break(code);
            }
            let wait = wait.then_some(WaitParams { timeout });
            Plan::ops(out, move |conn| async move {
                cmd_send(conn, &body, &id, bytes, raw, &keys, wait).await
            })
        }
        SessionOp::Kill { id, output } => {
            let (out, body) = point_out(output.format);
            Plan::ops(
                out,
                move |conn| async move { cmd_kill(conn, &body, &id).await },
            )
        }
        SessionOp::Evict { id, output } => {
            let (out, body) = point_out(output.format);
            Plan::ops(
                out,
                move |conn| async move { cmd_evict(conn, &body, &id).await },
            )
        }
        SessionOp::Capture {
            id,
            output,
            ansi,
            source,
            lines,
        } => {
            let (out, body) = stream_out(output.format);
            let enc = RowEncoding::resolve(ansi, output.format);
            Plan::ops(out, move |conn| async move {
                cmd_capture(conn, &body, &id, enc, source, lines).await
            })
        }
        SessionOp::Spawn {
            cwd,
            env,
            rows,
            cols,
            tags,
            cmd,
            output,
        } => {
            let (out, body) = point_out(output.format);
            let spawn_args =
                match spawn_args_for(target, cwd.as_deref(), &env, rows, cols, tags, &cmd) {
                    Ok(args) => args,
                    Err(err) => {
                        return ControlFlow::Break(
                            out.fail(ErrorKind::Usage, format!("spawn: {err:#}")),
                        );
                    }
                };
            if let Err(code) = refuse_over_limit(&out, "spawn", &spawn_args) {
                return ControlFlow::Break(code);
            }
            Plan::new(Dial::OpsOrSpawn, out, move |conn| async move {
                spawn_on(conn, &body, spawn_args).await
            })
        }
        SessionOp::Switch {
            id,
            from,
            attachment,
            output,
        } => {
            let (out, body) = point_out(output.format);
            // Resolved before the dial so "not inside felis and no
            // --from" never spins up an SSH relay just to reject.
            let from_prefix = match require_switch_from(&out, from.as_deref()) {
                Ok(prefix) => prefix,
                Err(code) => return ControlFlow::Break(code),
            };
            let scope = attachment.scope();
            Plan::ops(out, move |conn| async move {
                cmd_switch(conn, &body, &id, &from_prefix, scope).await
            })
        }
        SessionOp::Search {
            id,
            pattern,
            regex,
            case_insensitive,
            output,
        } => {
            let (out, body) = stream_out(output.format);
            if let Err(code) = refuse_breach(
                &out,
                "search",
                check_limit(
                    "Search::Query.query",
                    pattern.len(),
                    MAX_SEARCH_PATTERN_BYTES,
                ),
            ) {
                return ControlFlow::Break(code);
            }
            Plan::ops(out, move |conn| async move {
                cmd_search(conn, &body, &id, &pattern, regex, case_insensitive).await
            })
        }
        SessionOp::Tag {
            id,
            tags,
            remove,
            output,
        } => {
            let (out, body) = point_out(output.format);
            Plan::ops(out, move |conn| async move {
                cmd_tag(conn, &body, &id, tags, remove).await
            })
        }
    };
    ControlFlow::Continue(plan)
}

impl SessionOp {
    pub(crate) const fn format(&self) -> Format {
        match self {
            Self::List { output, .. }
            | Self::Info { output, .. }
            | Self::Send { output, .. }
            | Self::Kill { output, .. }
            | Self::Evict { output, .. }
            | Self::Spawn { output, .. }
            | Self::Switch { output, .. }
            | Self::Tag { output, .. } => output.format,
            Self::Capture { output, .. } | Self::Search { output, .. } => output.format,
        }
    }
}

fn point_out(format: Format) -> (Arc<Reporter>, Arc<Reporter>) {
    let out = Arc::new(Reporter::point(format));
    let body = Arc::clone(&out);
    (out, body)
}

fn stream_out(format: Format) -> (Arc<Reporter>, Arc<Reporter>) {
    let out = Arc::new(Reporter::stream(format));
    let body = Arc::clone(&out);
    (out, body)
}

/// Refuse a payload past its documented per-operation limit
/// (REQ-105a) before the dial, so the caller reads `invalid_request`
/// against the verb that produced it rather than a torn-down
/// connection.
fn refuse_over_limit<T: Validate>(out: &Reporter, verb: &str, value: &T) -> Result<(), i32> {
    refuse_breach(out, verb, value.validate())
}

/// The half of [`refuse_over_limit`] a verb reaches for when it has a
/// length but no message yet: the same exit code and wording, without
/// materializing a payload to hand [`Validate`].
fn refuse_breach(out: &Reporter, verb: &str, checked: Result<(), WireError>) -> Result<(), i32> {
    checked.map_err(|e| out.fail(ErrorKind::InvalidRequest, format!("{verb}: {e}")))
}
