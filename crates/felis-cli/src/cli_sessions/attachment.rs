use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use felis_client_core::{Connection, Reconnector, SwitchReply, spawn_args_from_cli};
use felis_protocol::SessionHex;
use felis_protocol::messages::{
    RetargetCarrier, RetargetLanding, RetargetTarget, SwitchDenied, SwitchScope,
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{AttachmentTarget, refuse_over_limit, require_resolved};
use crate::cli_output::{ErrorKind, Format, PointFormat, Reporter, RetargetResult, SwitchResult};
use crate::conn::Dial;

/// Flattened by every verb that retargets a window, so the spellings
/// cannot drift into two grammars. The destination is the verb's own
/// positional, which is what tells the two verbs apart.
#[derive(Debug, clap::Args)]
pub(crate) struct RetargetFlags {
    /// Attach to this session on the *target* daemon (hex id or unique
    /// prefix, resolved after dialing) instead of creating a fresh one.
    /// Mutually exclusive with a `-- <cmd>` override.
    #[arg(long, value_name = "TARGET-PREFIX", value_parser = crate::validate_session_id_prefix,
          conflicts_with = "command")]
    pub(crate) session: Option<String>,
    /// Move a different window: the session whose window should re-dial
    /// (hex id or prefix). Defaults to `$FELIS_SESSION_ID` (the window
    /// this command runs in).
    #[arg(long, value_name = "PREFIX", value_parser = crate::validate_session_id_prefix)]
    pub(crate) from: Option<String>,
    /// Program + args for the fresh target session, in the trailing
    /// slot the front door spells the same way. Refused with
    /// `--session`, which attaches to what is already running.
    #[arg(last = true, value_name = "CMD")]
    pub(crate) command: Vec<String>,
    #[command(flatten)]
    pub(crate) output: PointFormat,
}

pub(crate) struct RetargetArgs {
    pub carrier: RetargetCarrier,
    pub session: Option<String>,
    pub from: Option<String>,
    pub command: Vec<String>,
    pub attachment: Option<u64>,
    pub format: Format,
}

impl RetargetArgs {
    pub(crate) fn from_flags(
        carrier: RetargetCarrier,
        attachment: AttachmentTarget,
        flags: RetargetFlags,
    ) -> Self {
        Self {
            carrier,
            session: flags.session,
            from: flags.from,
            command: flags.command,
            attachment: attachment.attachment,
            format: flags.output.format,
        }
    }
}

/// The local daemon a `window retarget` destination names: a socket
/// path, or the default local daemon when the positional is absent.
/// Not `to_string_lossy`: the endpoint is a wire `string`, so a lossy
/// path dials a U+FFFD spelling no daemon listens on.
pub(crate) fn local_carrier(socket: Option<&Path>) -> Result<RetargetCarrier> {
    socket.map_or(Ok(RetargetCarrier::DefaultLocal), |path| {
        path.to_str()
            .map(|path| RetargetCarrier::LocalEndpoint(path.to_owned()))
            .with_context(|| format!("<SOCKET>: `{}` is not valid UTF-8", path.display()))
    })
}

/// Reject global carrier flags on subcommands that do not dial the named daemon.
/// The user asked for two daemons and felis will not guess which to honor
/// (docs/reference/cli.md "Re-point a window across daemons: `felis ssh`,
/// `felis window retarget`"). Refusal goes
/// through the verb's reporter framing (docs/reference/cli.md "Machine output").
pub(crate) fn reject_global_carrier(out: &Reporter, verb: &str, tail: &str) -> i32 {
    out.fail(
        ErrorKind::Usage,
        format!(
            "{verb}: the global --host/--socket/--ssh-arg name the daemon a headless verb \
             dials; {tail}"
        ),
    )
}

/// `window retarget`: relay a cross-carrier retarget push to this window's
/// session so it re-dials another daemon (docs/how-to/attach-over-ssh.md).
/// Exit codes: `0` pushed to a capable window, `1` no from-session or
/// descriptor over limit, `2` bad flags / unreachable / wire mismatch.
pub(crate) fn run_retarget(
    runtime: &tokio::runtime::Runtime,
    target: &Reconnector,
    verb: &'static str,
    args: RetargetArgs,
) -> Result<i32> {
    let scope = args
        .attachment
        .map_or(SwitchScope::Default, SwitchScope::Attachment);
    let out = Arc::new(Reporter::point(args.format));
    let (from_prefix, retarget) = match staged_retarget(&out, args, verb) {
        Ok(staged) => staged,
        Err(code) => return Ok(code),
    };
    runtime.block_on(async move {
        let conn = match RETARGET_DIAL.open(target, &out).await {
            Ok(conn) => conn,
            Err(code) => return Ok(code),
        };
        cmd_retarget(conn, &out, verb, &from_prefix, retarget, scope).await
    })
}

/// A retarget moves a window that is already attached here, and the
/// landing it asks for is dialed by the receiving window on the target
/// host, not by this process (docs/reference/cli.md "Auto-spawning").
pub(crate) const RETARGET_DIAL: Dial = Dial::Ops;

/// Shared by the retarget verbs so they cannot drift on which requests
/// are refused; `verb` names the caller in those messages.
pub(super) fn staged_retarget(
    out: &Reporter,
    args: RetargetArgs,
    verb: &'static str,
) -> Result<(String, RetargetTarget), i32> {
    // Resolved before the dial so "not inside felis and no --from"
    // never spins up an SSH relay just to reject.
    let from_prefix = require_switch_from(out, args.from.as_deref())?;
    let retarget = retarget_from_args(args);
    refuse_over_limit(out, verb, &retarget)?;
    Ok((from_prefix, retarget))
}

/// Maps CLI arguments to the wire union without needing a daemon.
/// Silent mis-mappings surface as windows landing on the wrong target.
/// Assumes [`staged_retarget`]'s checks passed.
pub(super) fn retarget_from_args(args: RetargetArgs) -> RetargetTarget {
    let carrier = args.carrier;
    let landing = match args.session {
        Some(prefix) => RetargetLanding::Attach(prefix),
        None => RetargetLanding::Create(spawn_args_from_cli(args.command)),
    };
    RetargetTarget { carrier, landing }
}

/// The explicit `--from`, else `$FELIS_SESSION_ID` (the session a felis
/// window exports into its child env).
fn resolve_switch_from(from: Option<&str>) -> Option<String> {
    from.map(str::to_owned).or_else(|| {
        std::env::var("FELIS_SESSION_ID")
            .ok()
            .and_then(|raw| crate::validate_session_id_prefix(&raw).ok())
    })
}

pub(super) fn require_switch_from(out: &Reporter, from: Option<&str>) -> Result<String, i32> {
    resolve_switch_from(from).ok_or_else(|| {
        out.fail(
            ErrorKind::InvalidRequest,
            "FELIS_SESSION_ID is not set (not inside a felis window, or the session predates \
             it); pass --from <id>",
        )
    })
}

/// Exit 1, not 2: the request reached the daemon and was answered; the
/// session simply has no window matching the scope, the same domain
/// failure class as an unknown session id.
fn report_denial(out: &Reporter, denied: SwitchDenied, from: u128) -> i32 {
    match denied {
        SwitchDenied::NoInputOwner => out.fail(
            ErrorKind::NoInputOwner,
            format!(
                "session {} has no window to move: nothing has typed in one, and it has no \
                 single attached window to fall back on — name one with `--attachment <id>` \
                 (`felis sessions info <id> --format json` lists them)",
                SessionHex(from)
            ),
        ),
        SwitchDenied::NoSuchAttachment { attachment } => out.fail(
            ErrorKind::NoSuchAttachment,
            format!(
                "attachment {attachment} is no longer on session {} — nothing moved (attachment \
                 ids are never reused, so it is gone rather than reassigned)",
                SessionHex(from)
            ),
        ),
    }
}

/// Exit 1 distinguishes nothing-to-move (no window attached, only
/// scripted reads the daemon leaves in place) and unknown target from
/// success; both leave every window where it was.
pub(super) async fn cmd_switch<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    to_prefix: &str,
    from_prefix: &str,
    scope: SwitchScope,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reply = match conn
        .switch_session(from_prefix.to_owned(), to_prefix.to_owned(), scope)
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                format!("switch to session `{to_prefix}`: {e}"),
            ));
        }
    };
    let SwitchReply {
        from,
        to,
        queued,
        denied,
    } = reply;
    // `from` first, then `to`, so a failure on either prints exactly
    // one error.
    let from = match require_resolved(out, from, from_prefix) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    let to = match to.map(|to| require_resolved(out, to, to_prefix)) {
        Some(Ok(id)) => id,
        Some(Err(code)) => return Ok(code),
        // A Session target always resolves; an absent resolution is a
        // daemon this build did not produce.
        None => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                "switch reply carried no target resolution",
            ));
        }
    };
    if let Some(denied) = denied {
        return Ok(report_denial(out, denied, from));
    }
    if queued == 0 && from != to {
        // The reply cannot distinguish "no subscriber" from "only scripted
        // reads attached"; `from == to` is already satisfied.
        return Ok(out.fail(
            ErrorKind::NotQueued,
            format!(
                "no switch-capable client took the switch for session {} — nothing moved \
                 (a window running an older felis-client does not count)",
                SessionHex(from)
            ),
        ));
    }
    // Queued, not landed: each window now runs its own attach.
    tracing::debug!(queued, "switch push queued");
    if out.machine() {
        out.result(&SwitchResult {
            from: SessionHex(from).to_string(),
            to: SessionHex(to).to_string(),
            queued,
        });
    } else if queued == 0 {
        println!("already on session {}; nothing queued", SessionHex(to));
    } else {
        println!(
            "queued on {queued} {}; not landed — each window runs its own attach",
            windows(queued)
        );
    }
    Ok(0)
}

const fn windows(queued: u32) -> &'static str {
    if queued == 1 { "window" } else { "windows" }
}

/// Resolve the from-session on this daemon, then push
/// [`RetargetTarget`] to its capable windows. The target session is not
/// resolved here: it lives in the target daemon's namespace, which the
/// client resolves after dialing. Exit codes mirror [`cmd_switch`].
async fn cmd_retarget<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    verb: &str,
    from_prefix: &str,
    retarget: RetargetTarget,
    scope: SwitchScope,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reply = match conn
        .retarget_window(from_prefix.to_owned(), retarget, scope)
        .await
    {
        Ok(reply) => reply,
        Err(e) => {
            return Ok(out.fail(
                ErrorKind::Protocol,
                format!("{verb}: retarget session `{from_prefix}`: {e}"),
            ));
        }
    };
    let from = match require_resolved(out, reply.from, from_prefix) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    if let Some(denied) = reply.denied {
        return Ok(report_denial(out, denied, from));
    }
    let queued = reply.queued;
    if queued == 0 {
        // As with `cmd_switch`: the reply cannot tell "no window
        // attached" from "only scripted reads attached".
        return Ok(out.fail(
            ErrorKind::NotQueued,
            format!(
                "no host-switch-capable client took the move for session {} — nothing moved \
                 (a window running an older felis-client does not count)",
                SessionHex(from)
            ),
        ));
    }
    // Queued, not landed: each window re-dials on its own.
    tracing::debug!(queued, "retarget push queued");
    if out.machine() {
        out.result(&RetargetResult {
            from: SessionHex(from).to_string(),
            queued,
        });
    } else {
        println!(
            "queued on {queued} {}; not landed — each window re-dials on its own",
            windows(queued)
        );
    }
    Ok(0)
}
