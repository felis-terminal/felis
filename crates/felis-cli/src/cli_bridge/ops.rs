use std::sync::Arc;

use anyhow::Result;
use felis_client_core::Offer;
use felis_protocol::{
    SessionHex,
    messages::{
        InfoOutcome, NotifyToClientMsg, OpsToClientMsg, OpsToDaemonMsg, RegionToClientMsg,
        RegionToDaemonMsg, ResolvedId, SearchOptions, SearchToClientMsg, SearchToDaemonMsg,
        SessionInfo, SpawnOutcome,
    },
};
use felis_transport::Payload;
use serde_json::Value;

use super::{
    admission::{
        Params, check_search_pattern, region_source, send_input, spawn_args, switch_from,
        switch_scope,
    },
    core::{Core, Op},
    envelope::BridgeError,
    link::{DaemonMsg, Link, StreamEvent, dial},
};
use crate::cli_output::{ErrorKind, body_value};

impl Core {
    async fn roster(&self) -> Result<Vec<SessionInfo>, BridgeError> {
        match self.anchor.request(&OpsToDaemonMsg::List).await? {
            DaemonMsg::Ops(OpsToClientMsg::Listed { sessions }) => Ok(sessions),
            other => Err(unexpected_reply(&other)),
        }
    }

    /// One session's row and the display prefix the daemon shortened
    /// against its own pool.
    async fn info(&self, prefix: &str) -> Result<(SessionInfo, String), BridgeError> {
        let reply = self
            .anchor
            .request(&OpsToDaemonMsg::Info {
                id_prefix: prefix.to_owned(),
            })
            .await?;
        let DaemonMsg::Ops(OpsToClientMsg::InfoReply { outcome }) = reply else {
            return Err(unexpected_reply(&reply));
        };
        match outcome {
            InfoOutcome::Found { session, short_id } => Ok((*session, short_id)),
            InfoOutcome::NoMatch => Err(BridgeError::new(
                ErrorKind::NoMatch,
                format!("no session id starts with `{prefix}`"),
            )),
            InfoOutcome::Ambiguous { matches } => Err(BridgeError::new(
                ErrorKind::Ambiguous,
                format!("`{prefix}` matches {matches} sessions; use a longer prefix"),
            )),
        }
    }

    pub(super) async fn op_list(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let tags = params.strings("tags")?;
        let sessions = self.roster().await?;
        // Shortened against the whole roster, not the filtered view: a
        // `short_id` unique only within a tag filter would stop
        // resolving the moment the filter changed.
        // The CLI verb's own body, so the two framings of one roster
        // cannot drift.
        op.result(body_value(&crate::cli_output::ListResult {
            sessions: sessions
                .iter()
                .filter(|s| tags.is_empty() || s.tags.iter().any(|t| tags.contains(t)))
                .map(|s| crate::cli_output::SessionObject::new(s, &sessions))
                .collect(),
        }));
        Ok(())
    }

    pub(super) async fn op_info(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let (session, short_id) = self.info(&prefix).await?;
        op.result(body_value(
            &crate::cli_output::SessionObject::with_short_id(&session, short_id),
        ));
        Ok(())
    }

    pub(super) async fn op_spawn(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let args = spawn_args(params, &self.target.carrier)?;
        let reply = self.anchor.request(&OpsToDaemonMsg::Spawn { args }).await?;
        let DaemonMsg::Ops(OpsToClientMsg::Spawned { outcome }) = reply else {
            return Err(unexpected_reply(&reply));
        };
        let info = match outcome {
            SpawnOutcome::Ok { info } => *info,
            SpawnOutcome::Refused { reason, detail } => {
                return Err(BridgeError::new(
                    ErrorKind::from_create_failure(reason),
                    format!("spawn refused ({reason:?}): {detail}"),
                ));
            }
        };
        op.result(body_value(&crate::cli_output::SessionRef::new(info.id)));
        Ok(())
    }

    pub(super) async fn op_kill(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let reply = self
            .anchor
            .request(&OpsToDaemonMsg::Destroy {
                id_prefix: prefix.clone(),
            })
            .await?;
        let DaemonMsg::Ops(OpsToClientMsg::Destroyed { resolved }) = reply else {
            return Err(unexpected_reply(&reply));
        };
        let id = resolved_id(resolved, &prefix)?;
        op.result(body_value(&crate::cli_output::SessionRef::new(id)));
        Ok(())
    }

    pub(super) async fn op_evict(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let reply = self
            .anchor
            .request(&OpsToDaemonMsg::ForceDetach {
                id_prefix: prefix.clone(),
            })
            .await?;
        let DaemonMsg::Ops(OpsToClientMsg::Detached {
            resolved,
            was_attached,
        }) = reply
        else {
            return Err(unexpected_reply(&reply));
        };
        let id = resolved_id(resolved, &prefix)?;
        op.result(body_value(&crate::cli_output::SessionRef {
            was_attached: Some(was_attached),
            ..crate::cli_output::SessionRef::new(id)
        }));
        Ok(())
    }

    pub(super) async fn op_switch(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let to_prefix = params.str_req("to")?;
        let from_prefix = switch_from(params)?;
        let scope = switch_scope(params)?;
        let reply = self
            .anchor
            .request(&OpsToDaemonMsg::Switch {
                from_prefix: from_prefix.clone(),
                target: felis_protocol::messages::SwitchTarget::Session(to_prefix.clone()),
                scope,
            })
            .await?;
        let DaemonMsg::Ops(OpsToClientMsg::Switched {
            from,
            to,
            queued,
            denied,
        }) = reply
        else {
            return Err(unexpected_reply(&reply));
        };
        if let Some(denied) = denied {
            return Err(match denied {
                felis_protocol::messages::SwitchDenied::NoInputOwner => BridgeError::new(
                    ErrorKind::NoInputOwner,
                    "the session has no window to move: nothing has typed in one, and it has no \
                     single attached window to fall back on — name one with `attachment`",
                ),
                felis_protocol::messages::SwitchDenied::NoSuchAttachment { attachment } => {
                    BridgeError::new(
                        ErrorKind::NoSuchAttachment,
                        format!("attachment {attachment} is no longer on this session"),
                    )
                }
            });
        }
        let from = resolved_id(from, &from_prefix)?;
        let to = to
            .ok_or_else(|| BridgeError::new(ErrorKind::Protocol, "switch reply carried no target"))
            .and_then(|to| resolved_id(to, &to_prefix))?;
        // `queued == 0` is data, not an error: whether "nothing
        // moved" is a failure is the editor's judgement.
        op.result(body_value(&crate::cli_output::SwitchResult {
            from: SessionHex(from).to_string(),
            to: SessionHex(to).to_string(),
            queued,
        }));
        Ok(())
    }

    pub(super) async fn op_tag(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let reply = self
            .anchor
            .request(&OpsToDaemonMsg::Tag {
                id_prefix: prefix.clone(),
                add: params.strings("add")?,
                remove: params.strings("remove")?,
            })
            .await?;
        let DaemonMsg::Ops(OpsToClientMsg::TagsUpdated {
            resolved,
            tags,
            denied,
        }) = reply
        else {
            return Err(unexpected_reply(&reply));
        };
        let id = resolved_id(resolved, &prefix)?;
        if let Some(reason) = denied {
            return Err(BridgeError::invalid(reason));
        }
        op.result(body_value(&crate::cli_sessions::tag_result(id, &tags)));
        Ok(())
    }

    pub(super) async fn op_send(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let msg = send_input(params)?;
        // Validated before `resolve`/`acquire`, not inside the closure:
        // `acquire` dials and attaches when this is the session's first
        // bridge user, so validating later would make an over-limit
        // payload cost a real connection before being refused.
        felis_protocol::messages::Validate::validate(&msg)
            .map_err(|e| BridgeError::over_limit(&e))?;
        let (id, ()) = self
            .with_session_by_prefix(op, &prefix, async |link| link.send_input(&msg).await)
            .await?;
        op.result(body_value(&crate::cli_output::SessionRef::new(id)));
        Ok(())
    }

    pub(super) async fn op_capture(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let source = region_source(params)?;
        let ansi = params.bool("ansi")?;
        let lines = params.u32_opt("lines")?;
        let (_id, (count, exit_code)) = self
            .with_session_by_prefix(op, &prefix, async |link| {
                let mut stream = link
                    .open_stream(&RegionToDaemonMsg::Rows {
                        source,
                        ansi,
                        max_rows: lines,
                    })
                    .await?;
                op.bind_stream(&link, stream.id);
                let mut exit_code = None;
                loop {
                    match stream.next().await {
                        StreamEvent::Item(item) => match item.msg {
                            DaemonMsg::Region(RegionToClientMsg::Row { .. }) => {
                                op.item(capture_row_json(&item.payload)?).await?;
                            }
                            DaemonMsg::Region(RegionToClientMsg::RowsDone { exit_code: code }) => {
                                exit_code = code;
                            }
                            _ => {}
                        },
                        StreamEvent::End { count } => {
                            return Ok((u64::from(count), exit_code));
                        }
                        StreamEvent::Failed(err) => return Err(err.into()),
                    }
                }
            })
            .await?;
        op.end(count, exit_code);
        Ok(())
    }

    pub(super) async fn op_search(&self, op: &Op, params: &Params<'_>) -> Result<(), BridgeError> {
        let prefix = params.str_req("session")?;
        let pattern = params.str_req("pattern")?;
        let options = SearchOptions {
            regex: params.bool("regex")?,
            case_insensitive: params.bool("case_insensitive")?,
        };
        check_search_pattern(&pattern)?;
        let (_id, count) = self
            .with_session_by_prefix(op, &prefix, async |link| {
                let mut stream = link
                    .open_stream(&SearchToDaemonMsg::Query {
                        query: pattern,
                        options,
                    })
                    .await?;
                op.bind_stream(&link, stream.id);
                loop {
                    match stream.next().await {
                        StreamEvent::Item(item) => {
                            if let DaemonMsg::Search(SearchToClientMsg::Match {
                                line_index,
                                text,
                                byte_spans,
                                col_spans,
                            }) = item.msg
                            {
                                op.item(body_value(&crate::cli_sessions::search_match(
                                    line_index,
                                    &text,
                                    &byte_spans,
                                    &col_spans,
                                )))
                                .await?;
                            }
                        }
                        StreamEvent::End { count } => return Ok(u64::from(count)),
                        StreamEvent::Failed(err) => return Err(err.into()),
                    }
                }
            })
            .await?;
        op.end(count, None);
        Ok(())
    }

    /// Its own connection: the fan-out is an `Observer`-mode surface
    /// and the anchor is `Ops`, and the daemon serves exactly one
    /// subscription per observer connection.
    pub(super) async fn op_notifications(
        &self,
        op: &Op,
        params: &Params<'_>,
    ) -> Result<(), BridgeError> {
        let session_prefix = params.str_opt("session")?;
        let _permit = self.reserve_link()?;
        let conn = dial(&self.target, Offer::observer()).await?;
        let link = Link::start(conn, "observer");
        op.track_link(&link);
        let outcome = self.stream_notifications(op, &link, session_prefix).await;
        link.shutdown().await;
        let count = outcome?;
        op.end(count, None);
        Ok(())
    }

    async fn stream_notifications(
        &self,
        op: &Op,
        link: &Arc<Link>,
        session_prefix: Option<String>,
    ) -> Result<u64, BridgeError> {
        let mut stream = link.subscribe_notifications(session_prefix).await?;
        op.bind_stream(link, stream.id);
        loop {
            match stream.next().await {
                StreamEvent::Item(item) => match item.msg {
                    DaemonMsg::Notify(NotifyToClientMsg::Event {
                        session_id,
                        notification,
                        notify_id,
                        session_title,
                        cwd,
                        attached,
                    }) => {
                        op.item(body_value(&crate::cli_output::NotificationObject::new(
                            session_id,
                            &notification,
                            notify_id.as_deref(),
                            session_title.as_deref(),
                            cwd.as_deref(),
                            attached,
                        )))
                        .await?;
                    }
                    // Lag is in-band and non-terminal; an error would
                    // tell a client to re-subscribe when it need only
                    // note the gap.
                    DaemonMsg::Notify(NotifyToClientMsg::Lagged { missed }) => {
                        op.lag(missed).await?;
                    }
                    // `Subscribed` acks the filter; an unresolvable one
                    // is followed by the daemon's own error terminal.
                    _ => {}
                },
                StreamEvent::End { count } => return Ok(u64::from(count)),
                StreamEvent::Failed(err) => return Err(err.into()),
            }
        }
    }
}

/// The typed failure kinds the machine-output contract names
/// (`docs/reference/cli.md`, "Other verbs": `no_match`, `ambiguous`).
fn resolved_id(resolved: ResolvedId, prefix: &str) -> Result<u128, BridgeError> {
    crate::session_id_from_resolved(resolved, prefix).map_err(|err| prefix_error(&err))
}

/// One mapping, so `info` (client-side resolution) and `kill`
/// (daemon-side) report the same failure the same way.
fn prefix_error(err: &crate::SessionPrefixError) -> BridgeError {
    BridgeError::new(ErrorKind::from_prefix_error(err), err.to_string())
}

fn unexpected_reply(msg: &DaemonMsg) -> BridgeError {
    BridgeError::new(
        ErrorKind::Protocol,
        format!(
            "the daemon answered with an unexpected {} message",
            msg.kind()
        ),
    )
}

/// Extract the inner row object from the rendered frame.
///
/// Strips the outer `Row` tag the family's serde form carries
/// (docs/reference/cli.md "Machine output").
pub(super) fn capture_row_json(payload: &Payload) -> Result<Value, BridgeError> {
    let value = crate::cli_bridge_json::body_to_json(payload.kind, &payload.body)
        .map_err(|err| BridgeError::new(ErrorKind::Protocol, format!("row transcode: {err}")))?;
    Ok(value.get("Row").cloned().unwrap_or(value))
}
