use std::ops::ControlFlow;

use anyhow::Result;
use felis_client_core::{ConnectError, Connection, check_value_minor};
use felis_protocol::messages::SessionInfo;
use felis_protocol::{
    SessionHex,
    messages::{
        ByteSpan, ColSpan, RegionSource, SearchOptions, SearchSource, SearchToClientMsg,
        SearchToDaemonMsg,
    },
};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{
    attach_by_prefix, checked_roster, detach,
    driver::{DrainTick, Drained, Stall, drain_frames, drain_rehydrate},
    session_info_by_prefix,
};
use crate::cli_output::{
    CaptureRow, ErrorKind, Format, ListResult, Reporter, SearchMatch, SessionObject, short_id_in,
};

/// "No daemon" (exit 2) and "0 sessions" (exit 0) are distinct
/// (docs/reference/ipc.md "CLI clients").
pub(super) async fn cmd_list<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    tag_filter: &[String],
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let sessions = match checked_roster(&mut conn, out).await {
        Ok(s) => s,
        Err(code) => return Ok(code),
    };
    let filtered = || {
        sessions
            .iter()
            .filter(|s| session_passes_tag_filter(s, tag_filter))
    };
    if out.machine() {
        // `short_id` is shortened against the whole roster, not the
        // filtered view: a prefix unique only within a `--tag` filter
        // would stop resolving as soon as the filter changed.
        out.result(&ListResult {
            sessions: filtered()
                .map(|s| SessionObject::new(s, &sessions))
                .collect(),
        });
    } else {
        // A `--tag` filter that excludes everything prints nothing.
        let mut any = false;
        for s in filtered() {
            any = true;
            out.line(crate::format_session_line(s, &short_id_in(s.id, &sessions)));
        }
        if !any && tag_filter.is_empty() {
            out.line("no sessions");
        }
    }
    Ok(0)
}

pub(super) async fn cmd_info<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id: &str,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (session, short_id) = match session_info_by_prefix(&mut conn, out, id).await {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };
    let session = &session;
    if out.machine() {
        out.result(&SessionObject::with_short_id(session, short_id));
    } else {
        // No column alignment: free-form title/cwd would push the value
        // column past any sane width. `id:` first so `head -1` yields
        // the hex id.
        println!("id:       {}", SessionHex(session.id));
        println!("short:    {short_id}");
        println!("size:     {}x{}", session.dims.rows, session.dims.cols);
        if let Some(secs) = session.idle_seconds {
            println!("idle:     {}", crate::format_idle_age(secs));
        }
        if session.exited {
            println!("exited:   yes (in post-exit grace)");
        }
        if let Some(title) = session.title.as_deref().filter(|t| !t.trim().is_empty()) {
            println!("title:    {}", title.trim());
        }
        if let Some(cwd) = session.cwd.as_deref().filter(|c| !c.trim().is_empty()) {
            println!("cwd:      {}", cwd.trim());
        }
        if let Some(fg) = session
            .foreground
            .as_deref()
            .filter(|f| !f.trim().is_empty())
        {
            println!("running:  {}", fg.trim());
        }
        if !session.tags.is_empty() {
            println!("tags:     {}", session.tags.join(" "));
        }
        if let Some(code) = session.last_exit_code {
            println!("exit:     {code}");
        }
        for record in &session.program_status {
            println!("status:   {}", status_line(record));
        }
        for attachment in &session.attachments {
            println!(
                "window:   {} (attached {}){}",
                attachment.id,
                crate::timestamp::rfc3339_utc(attachment.attached_at),
                if attachment.input_owner {
                    " [input owner]"
                } else {
                    ""
                },
            );
        }
        if let Some(n) = &session.last_notification {
            let text = if n.notification.body.trim().is_empty() {
                n.notification.title.as_deref().unwrap_or("").trim()
            } else {
                n.notification.body.trim()
            };
            println!(
                "notify:   [{}] {} ({} ago)",
                n.notification.urgency.as_str(),
                text,
                crate::format_idle_age(n.age_seconds),
            );
        }
    }
    Ok(0)
}

/// The wire always carries plain `text`; `--ansi` asks the daemon to
/// send each row's SGR reconstruction beside it, so every pairing is
/// serviceable for every source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowEncoding {
    Text,
    /// SGR-reconstructed rows standing in for the text (plain
    /// `--ansi`, meant for a pager).
    Ansi,
    Both,
}

impl RowEncoding {
    pub(super) const fn resolve(ansi: bool, format: Format) -> Self {
        match (ansi, format.is_machine()) {
            (false, _) => Self::Text,
            (true, false) => Self::Ansi,
            (true, true) => Self::Both,
        }
    }

    pub(super) fn wants_ansi(self) -> bool {
        self != Self::Text
    }
}

struct CapturedRow {
    row: i32,
    text: String,
    ansi: Option<String>,
    soft_wrap_continued: bool,
}

fn open_stream_failure(out: &Reporter, verb: &str, err: &ConnectError) -> i32 {
    let kind = match err {
        ConnectError::Driver(_) => ErrorKind::Protocol,
        ConnectError::MinorTooOld { .. } => ErrorKind::Unsupported,
        _ => ErrorKind::DaemonLost,
    };
    out.fail(kind, format!("{verb}: open a stream: {err}"))
}

/// Request and drain the region row stream.
/// Returns rows, range-closing exit code, and abandon reason if any.
/// A stream missing its terminal returns `Err(2)` so consumers never
/// mistake a truncated capture for a complete one.
async fn drain_region_rows<R, W>(
    conn: &mut Connection<R, W>,
    out: &Reporter,
    source: RegionSource,
    encoding: RowEncoding,
    max_rows: Option<u32>,
) -> Result<(Vec<CapturedRow>, Option<u32>, Option<(ErrorKind, String)>), i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use felis_protocol::messages::{RegionToClientMsg, RegionToDaemonMsg};

    // The client allocates the id, so the opening request already
    // names what a cancel or a terminal refers to.
    let stream = conn
        .open_stream(&RegionToDaemonMsg::Rows {
            source,
            ansi: encoding.wants_ansi(),
            max_rows,
        })
        .await
        .map_err(|e| open_stream_failure(out, "capture", &e))?;

    let mut rows = Vec::new();
    let mut exit_code = None;
    let mut daemon_errored: Option<(ErrorKind, String)> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    drain_frames(
        conn,
        Some(stream),
        Some(deadline),
        std::time::Duration::from_secs(1),
        |tick| match tick {
            DrainTick::TimedOut => Stall::Retry,
            DrainTick::Eof => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                "capture: daemon closed the connection mid-capture",
            )),
            DrainTick::DeadlineExceeded => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                "capture: the daemon did not finish the region in time",
            )),
            DrainTick::Error(e) => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                format!("capture: connection error: {e}"),
            )),
            DrainTick::Corrupt(err) => Stall::Fail(out.fail(
                ErrorKind::Protocol,
                format!("capture: daemon sent an unreadable frame: {err}"),
            )),
        },
        |msg| match msg {
            Drained::Item(RegionToClientMsg::Row {
                row,
                text,
                ansi,
                soft_wrap_continued,
            }) => {
                rows.push(CapturedRow {
                    row,
                    text,
                    ansi,
                    soft_wrap_continued,
                });
                ControlFlow::Continue(())
            }
            // `RowsDone` carries the exit code, but the stream is not
            // over until its terminal lands.
            Drained::Item(RegionToClientMsg::RowsDone { exit_code: code }) => {
                exit_code = code;
                ControlFlow::Continue(())
            }
            Drained::Item(_) => ControlFlow::Continue(()),
            Drained::End { .. } => ControlFlow::Break(()),
            Drained::Failed { reason, detail } => {
                daemon_errored = Some((
                    ErrorKind::from_stream_reason(reason),
                    format!("{}: {detail}", reason.as_str()),
                ));
                ControlFlow::Break(())
            }
        },
    )
    .await?;

    Ok((rows, exit_code, daemon_errored))
}

/// The daemon indexes rows in the region's own coordinate space
/// (scrollback negative, live grid `0..rows`, mark ranges from 0) and
/// applies the `--lines` tail before anything crosses the wire.
pub(super) async fn cmd_capture<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
    encoding: RowEncoding,
    source: RegionSource,
    lines: Option<u32>,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let _resolved = match attach_by_prefix(&mut conn, out, id_prefix).await {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };

    if let Err(code) = drain_rehydrate(&mut conn, out).await {
        return Ok(code);
    }
    let (rows, exit_code, errored) =
        match drain_region_rows(&mut conn, out, source, encoding, lines).await {
            Ok(v) => v,
            Err(code) => return Ok(code),
        };

    // A failed detach is the stream's failing terminal, not a bare `?`:
    // a consumer blocked on the terminal would otherwise see neither
    // the capture nor a close.
    if let Err(e) = detach(&mut conn).await {
        return Ok(out.fail(ErrorKind::DaemonLost, format!("capture: {e}")));
    }

    print_capture(out, &rows, encoding);
    // A daemon-reported failure closes the stream as its failing
    // terminal: the rows already emitted stand, and the terminal says
    // the capture is not complete.
    if let Some((kind, err)) = errored {
        return Ok(out.fail(kind, format!("capture: {err}")));
    }
    out.end(u64::try_from(rows.len()).unwrap_or(u64::MAX), exit_code);
    Ok(0)
}

/// Text mode stitches soft-wrapped rows into logical lines. Machine framing
/// keeps one object per row so daemon-assigned indices stay meaningful,
/// carrying `soft_wrap_continued` so consumers can stitch if desired.
fn print_capture(out: &Reporter, rows: &[CapturedRow], encoding: RowEncoding) {
    if out.machine() {
        for r in rows {
            out.item(&CaptureRow {
                row: i64::from(r.row),
                text: &r.text,
                ansi: match encoding {
                    RowEncoding::Both => r.ansi.as_deref(),
                    RowEncoding::Text | RowEncoding::Ansi => None,
                },
                soft_wrap_continued: r.soft_wrap_continued,
            });
        }
        return;
    }

    let texts: Vec<&str> = rows
        .iter()
        .map(|r| match encoding {
            RowEncoding::Ansi => r.ansi.as_deref().unwrap_or(&r.text),
            RowEncoding::Text | RowEncoding::Both => r.text.as_str(),
        })
        .collect();
    let continued: Vec<bool> = rows.iter().map(|r| r.soft_wrap_continued).collect();
    for line in felis_grid::logical_lines(&texts, &continued) {
        let line = line.concat();
        println!("{line}");
    }
}

/// `SearchToDaemonMsg::Query` is accepted only attached. Exit 0 with at least
/// one match, 1 for none or session not found, 2 daemon-unreachable /
/// protocol error.
pub(super) async fn cmd_search<R, W>(
    mut conn: Connection<R, W>,
    out: &Reporter,
    id_prefix: &str,
    pattern: &str,
    options: SearchOptions,
    source: SearchSource,
) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let query = SearchToDaemonMsg::Query {
        query: pattern.to_string(),
        options,
        source,
    };
    // Before the attach: a daemon below the source's minor would ignore
    // the field and answer a scrollback search in its place.
    if let Err(e) = check_value_minor(&query, conn.effective_minor) {
        return Ok(out.fail(ErrorKind::Unsupported, format!("search: {e}")));
    }

    let _resolved = match attach_by_prefix(&mut conn, out, id_prefix).await {
        Ok(v) => v,
        Err(code) => return Ok(code),
    };

    // Rehydrate frames arrive in parallel on the Grid kind; the drain
    // sees only the Search-kind ones and the stream's own terminal.
    let opened = conn.open_stream(&query).await;
    let stream = match opened {
        Ok(stream) => stream,
        Err(e) => return Ok(open_stream_failure(out, "search", &e)),
    };

    let mut total: u32 = 0;
    let mut errored: Option<(ErrorKind, String)> = None;
    let drained = drain_frames(
        &mut conn,
        Some(stream),
        None,
        std::time::Duration::from_secs(10),
        |tick| match tick {
            DrainTick::TimedOut => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                "search: the daemon stopped answering mid-search",
            )),
            DrainTick::Eof => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                "search: daemon closed the connection mid-search",
            )),
            DrainTick::Error(e) => Stall::Fail(out.fail(
                ErrorKind::DaemonLost,
                format!("search: connection error: {e}"),
            )),
            DrainTick::Corrupt(err) => Stall::Fail(out.fail(
                ErrorKind::Protocol,
                format!("search: daemon sent an unreadable frame: {err}"),
            )),
            DrainTick::DeadlineExceeded => unreachable!("search sets no deadline"),
        },
        |msg| match msg {
            Drained::Item(SearchToClientMsg::Match {
                line_index,
                text,
                byte_spans,
                col_spans,
            }) => {
                total = total.saturating_add(1);
                print_search_match(out, line_index, &text, &byte_spans, &col_spans);
                ControlFlow::Continue(())
            }
            Drained::End { count } => {
                out.end(u64::from(count), None);
                ControlFlow::Break(())
            }
            Drained::Failed { reason, detail } => {
                errored = Some((
                    ErrorKind::from_stream_reason(reason),
                    format!("{}: {detail}", reason.as_str()),
                ));
                ControlFlow::Break(())
            }
        },
    )
    .await;
    if let Err(code) = drained {
        return Ok(code);
    }

    if let Some((kind, err)) = errored {
        drop(detach(&mut conn).await);
        return Ok(out.fail(kind, format!("search: {err}")));
    }

    // The stream's clean terminal already stands, so this bookend
    // cannot change the answer; it only saves the ~200 ms re-pool grace.
    // A failure here is a stderr diagnostic, never a second terminal or
    // a disagreeing exit code.
    if let Err(e) = detach(&mut conn).await {
        eprintln!("search: {e}");
    }
    Ok(i32::from(total == 0))
}

fn print_search_match(
    out: &Reporter,
    line_index: i64,
    text: &str,
    byte_spans: &[ByteSpan],
    col_spans: &[ColSpan],
) {
    if out.machine() {
        out.item(&search_match(line_index, text, byte_spans, col_spans));
    } else {
        // grep-style `<line>:<text>`; `line_index` anchors a stitched
        // soft-wrapped line at its topmost row.
        println!("{line_index}:{text}");
    }
}

/// Shared with the bridge's `sessions.search` items. `col_spans` are
/// per-row highlight segments `[row_line_index, col_start, col_end]`;
/// a match crossing a soft-wrap edge emits one per touched row.
pub(crate) fn search_match<'a>(
    line_index: i64,
    text: &'a str,
    byte_spans: &[ByteSpan],
    col_spans: &[ColSpan],
) -> SearchMatch<'a> {
    SearchMatch {
        line_index,
        text,
        byte_spans: byte_spans.iter().map(|s| [s.start, s.end]).collect(),
        col_spans: col_spans
            .iter()
            .map(|c| [c.line_index, i64::from(c.col_start), i64::from(c.col_end)])
            .collect(),
    }
}

pub(super) fn session_passes_tag_filter(s: &SessionInfo, filter: &[String]) -> bool {
    filter.is_empty() || s.tags.iter().any(|t| filter.iter().any(|f| f == t))
}

/// One record on one line: `<id> <state>[ <kind>][ <progress>%][ [<app>]][ <title>][: <msg>]`,
/// the root record shown as `(root)`. Producer text is printed with its
/// direction controls spelled out: this line lands outside any grid
/// that would mark them.
fn status_line(record: &felis_protocol::messages::ProgramStatusRecord) -> String {
    use std::fmt::Write as _;

    let mut line = if record.id.is_empty() {
        "(root)".to_owned()
    } else {
        disarm(&record.id)
    };
    let _ = write!(line, " {}", disarm(&record.state));
    if let Some(kind) = &record.kind {
        let _ = write!(line, " {}", disarm(kind));
    }
    if let Some(progress) = record.progress {
        let _ = write!(line, " {progress}%");
    }
    if let Some(app) = &record.app {
        let _ = write!(line, " [{}]", disarm(app));
    }
    if let Some(title) = &record.title {
        let _ = write!(line, " {}", disarm(title));
    }
    if let Some(msg) = &record.msg {
        let _ = write!(line, ": {}", disarm(msg));
    }
    line
}

/// Spells each UAX #9 explicit-formatting character and the implicit
/// direction marks (LRM, RLM, ALM) as `<U+XXXX>`, and every other
/// control character the same way.
fn disarm(text: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if felis_grid::bidi::is_override(c)
            || matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}')
            || c.is_control()
        {
            let _ = write!(out, "<U+{:04X}>", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod status_line_tests {
    use felis_protocol::messages::ProgramStatusRecord;

    use super::status_line;

    fn record(id: &str, state: &str) -> ProgramStatusRecord {
        ProgramStatusRecord {
            id: id.into(),
            state: state.into(),
            kind: None,
            progress: None,
            app: None,
            title: None,
            msg: None,
        }
    }

    #[test]
    fn a_bare_root_record_prints_its_state() {
        assert_eq!(status_line(&record("", "idle")), "(root) idle");
    }

    #[test]
    fn every_key_prints_in_order() {
        let line = status_line(&ProgramStatusRecord {
            kind: Some("permission".into()),
            progress: Some(40),
            app: Some("cargo".into()),
            title: Some("tests".into()),
            msg: Some("allow network?".into()),
            ..record("build/test", "blocked")
        });
        assert_eq!(
            line,
            "build/test blocked permission 40% [cargo] tests: allow network?"
        );
    }

    #[test]
    fn direction_controls_in_producer_text_are_spelled_out() {
        let line = status_line(&ProgramStatusRecord {
            title: Some("a\u{202E}b\u{2066}c".into()),
            msg: Some("\u{200F}x\u{061C}".into()),
            ..record("", "done")
        });
        assert_eq!(line, "(root) done a<U+202E>b<U+2066>c: <U+200F>x<U+061C>");
    }

    #[test]
    fn right_to_left_script_is_content_and_printed_as_is() {
        let line = status_line(&ProgramStatusRecord {
            msg: Some("\u{05E9}\u{05DC}\u{05D5}\u{05DD}".into()),
            ..record("", "done")
        });
        assert_eq!(line, "(root) done: \u{05E9}\u{05DC}\u{05D5}\u{05DD}");
    }
}
