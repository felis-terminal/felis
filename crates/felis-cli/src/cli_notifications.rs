//! `felis notifications subscribe`: desktop-notification observer CLI.
//!
//! A stream verb (`--format jsonl`) that writes one object per notification
//! plus a terminal record (docs/reference/protocols/notifications.md).

use anyhow::Result;
use clap::Subcommand;
use felis_protocol::messages::{ConnToClientMsg, NotifyToClientMsg};
use felis_transport::{Delivery, Incoming};
use tokio::io::{AsyncRead, AsyncWrite};

use felis_client_core::{ConnectError, Connection, Reconnector};

use crate::cli_output::{
    ErrorKind, Format, NotificationObject, Reporter, StreamFormat, short_id_alone,
};
use crate::conn::Dial;
use crate::session_id_from_resolved;

/// An observer of a daemon that is already running: a subscriber that
/// resurrected one would report on an empty machine of its own making.
pub(crate) const DIAL: Dial = Dial::Observer;

#[derive(Debug, Subcommand)]
pub(crate) enum NotificationOp {
    /// Stream desktop notifications (OSC 9 / 99 / 777) until interrupted.
    ///
    /// Streams every session's notifications by default. felis draws no popup;
    /// pipe output into an external notifier (`notify-send`, `terminal-notifier`).
    Subscribe {
        /// Only relay notifications from this session (hex id or any
        /// unique prefix).
        #[arg(long, value_name = "ID-OR-PREFIX", value_parser = crate::validate_session_id_prefix)]
        session: Option<String>,
        /// Print the first (matching) notification and exit 0: the
        /// notification-side `sessions send --wait`, for TUI programs
        /// that emit no OSC 133 marks. Exits 1 if the stream ends first.
        #[arg(long)]
        once: bool,
        /// Give up after this many seconds without a (matching)
        /// notification and exit 1. Requires `--once`: an endless
        /// subscribe has no timeout.
        ///
        /// `0` is a deadline already past: the wait gives up at once.
        #[arg(long, value_name = "SECS", requires = "once")]
        timeout: Option<u64>,
        #[command(flatten)]
        output: StreamFormat,
    },
}

/// The human line is not a parse target (the JSONL framing is), so it
/// optimizes for scanning a live tail.
fn notification_line(
    session_id: u128,
    notification: &felis_protocol::messages::Notification,
) -> String {
    let title = notification.title.as_deref().unwrap_or("").trim();
    let body = notification.body.trim();
    let text = match (title.is_empty(), body.is_empty()) {
        (false, false) => format!("{title} — {body}"),
        (false, true) => title.to_owned(),
        (true, _) => body.to_owned(),
    };
    format!(
        "{}  [{}]  {}",
        short_id_alone(session_id),
        notification.urgency.as_str(),
        text
    )
}

/// Exit codes (docs/reference/cli.md): 2 when unreachable; 0 when the stream ends;
/// under `--once`, 0 on first notification, 1 on stream end or timeout.
///
/// Uses the daemon's typed terminal rather than bare EOF to map exit codes.
#[allow(
    clippy::needless_pass_by_value,
    reason = "by-value clap subcommand dispatch, mirroring cli_sessions::run; the enum grows further ops"
)]
impl NotificationOp {
    pub(crate) const fn format(&self) -> Format {
        match self {
            Self::Subscribe { output, .. } => output.format,
        }
    }
}

pub(crate) fn run(
    runtime: &tokio::runtime::Runtime,
    op: NotificationOp,
    target: &Reconnector,
) -> Result<i32> {
    runtime.block_on(async move {
        match op {
            NotificationOp::Subscribe {
                session,
                once,
                timeout,
                output,
            } => {
                let out = Reporter::stream(output.format);
                // A refused open is a failure before the first item,
                // which the stream contract makes the stream's one
                // terminal rather than a bare stderr line.
                let conn = match DIAL.open(target, &out).await {
                    Ok(conn) => conn,
                    Err(code) => return Ok(code),
                };
                stream_notifications(conn, &out, session, once, timeout).await
            }
        }
    })
}

#[allow(clippy::print_stdout)]
async fn stream_notifications<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    session_prefix: Option<String>,
    once: bool,
    timeout: Option<u64>,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use std::io::Write as _;

    // The failure below is before the first item, so it is reported as
    // the stream's one terminal rather than propagated: a `?` would
    // write no object, and a consumer reading until the terminal would
    // wait forever (docs/reference/cli.md "Machine output").
    if let Err(err) = conn.subscribe_notifications(session_prefix.clone()).await {
        let kind = match err {
            ConnectError::Driver(_) => ErrorKind::Protocol,
            _ => ErrorKind::DaemonLost,
        };
        return Ok(out.fail(kind, format!("notifications: subscribe: {err}")));
    }

    let deadline = crate::timeout_deadline(timeout);
    let mut delivered: u64 = 0;

    let timed_out = || {
        out.fail(
            ErrorKind::Timeout,
            format!(
                "notifications: no notification within {}s",
                timeout.unwrap_or(0)
            ),
        )
    };

    loop {
        // `timeout_at` polls its inner future before the expired timer,
        // so an elapsed deadline has to end the wait on its own: a
        // frame already in the reader's read-ahead would otherwise be
        // delivered past the deadline the caller set.
        if deadline.is_some_and(|at| at <= tokio::time::Instant::now()) {
            return Ok(timed_out());
        }
        // Through the connection, not the raw reader: a frame parked
        // by a verb that ran on this connection must be read first.
        let next = conn.next_frame();
        let frame = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, next).await {
                Ok(read) => read,
                Err(_elapsed) => return Ok(timed_out()),
            },
            None => next.await,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            // EOF without a terminal: the daemon died rather than
            // ending the stream, and a `--once` caller cannot tell
            // whether its notification was lost.
            Ok(None) => {
                return Ok(out.fail(
                    ErrorKind::DaemonLost,
                    "notifications: daemon closed the stream without ending it",
                ));
            }
            Err(err) => {
                return Ok(out.fail(
                    ErrorKind::DaemonLost,
                    format!("notifications: stream error: {err}"),
                ));
            }
        };
        let msg = match conn.driver.classify(&frame) {
            // The daemon is done publishing. Under `--once` the
            // promised notification never came (nonzero), but the
            // terminal is still clean.
            Ok(Incoming::Control(ConnToClientMsg::End { .. })) => {
                out.end(delivered, None);
                return Ok(i32::from(once));
            }
            Ok(Incoming::Control(ConnToClientMsg::Error { reason, detail, .. })) => {
                return Ok(out.fail(
                    ErrorKind::from_stream_reason(reason),
                    format!("notifications: {}: {detail}", reason.as_str()),
                ));
            }
            Ok(Incoming::Control(_) | Incoming::CancelIgnored { .. }) => continue,
            Ok(Incoming::Payload(payload)) => {
                match conn.driver.decode::<NotifyToClientMsg>(&payload) {
                    Ok(Delivery::Deliver(delivered)) => delivered.msg,
                    Ok(Delivery::DroppedAfterCancel | Delivery::RefuseStream { .. }) => continue,
                    Err(err) => {
                        return Ok(out.fail(
                            ErrorKind::Protocol,
                            format!("notifications: daemon sent an unreadable frame: {err}"),
                        ));
                    }
                }
            }
            Err(err) => {
                return Ok(out.fail(
                    ErrorKind::Protocol,
                    format!("notifications: daemon sent an unreadable frame: {err}"),
                ));
            }
        };
        if let NotifyToClientMsg::Subscribed { filter } = msg {
            // A failed `--session` resolution arrives as a typed ack;
            // the daemon closes right after.
            if let Some(resolved) = filter
                && let Err(err) = session_id_from_resolved(
                    resolved,
                    session_prefix.as_deref().unwrap_or_default(),
                )
            {
                let kind = match err {
                    crate::SessionPrefixError::NoMatch { .. } => ErrorKind::NoMatch,
                    crate::SessionPrefixError::Ambiguous { .. } => ErrorKind::Ambiguous,
                };
                return Ok(out.fail(kind, format!("notifications: {err}")));
            }
            continue;
        }
        if let NotifyToClientMsg::Lagged { missed } = msg {
            // The marker rides stdout as an in-band event: the event a
            // piped consumer waits on may be among the missed.
            out.lag(missed);
            let _flushed = std::io::stdout().flush().is_ok();
            continue;
        }
        if let NotifyToClientMsg::Event {
            session_id,
            notification,
            notify_id,
            session_title,
            cwd,
            attached,
        } = msg
        {
            delivered += 1;
            if out.machine() {
                out.item(&NotificationObject::new(
                    session_id,
                    &notification,
                    notify_id.as_deref(),
                    session_title.as_deref(),
                    cwd.as_deref(),
                    attached,
                ));
            } else {
                println!("{}", notification_line(session_id, &notification));
            }
            // Flush so a piped consumer sees each notification now, not
            // on block-buffer fill; a flush failure on a closed pipe
            // ends the stream on the next write anyway.
            let _flushed = std::io::stdout().flush().is_ok();
            if once {
                out.end(delivered, None);
                return Ok(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use felis_protocol::messages::{Notification, Urgency};

    use super::*;

    fn notification(title: Option<&str>, body: &str) -> Notification {
        Notification {
            title: title.map(str::to_owned),
            body: body.to_owned(),
            urgency: Urgency::Normal,
        }
    }

    /// A title-only or body-only notification must not print a
    /// dangling separator.
    #[test]
    fn the_human_line_joins_only_the_halves_that_exist() {
        let id = 0xCAFE_0000_0000_0000_0000_0000_0000_0000_u128;
        assert_eq!(
            notification_line(id, &notification(Some("Build"), "green")),
            "cafe0000  [normal]  Build — green"
        );
        assert_eq!(
            notification_line(id, &notification(Some("Build"), "  ")),
            "cafe0000  [normal]  Build"
        );
        assert_eq!(
            notification_line(id, &notification(None, "green")),
            "cafe0000  [normal]  green"
        );
    }
}
