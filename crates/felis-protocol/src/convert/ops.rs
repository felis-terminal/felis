//! `OpsToDaemonMsg` / `OpsToClientMsg` <-> wire, with the
//! session-roster vocabulary its `Listed` reply carries.

use std::num::NonZeroU64;

use super::{WireError, decode_enum, dims_from_wire, id_from_bytes, id_to_bytes};
use crate::messages;
use crate::wire::v1;

impl From<&messages::SessionNotification> for v1::SessionNotification {
    fn from(n: &messages::SessionNotification) -> Self {
        Self {
            notification: Some((&n.notification).into()),
            age_seconds: n.age_seconds,
        }
    }
}

impl TryFrom<v1::SessionNotification> for messages::SessionNotification {
    type Error = WireError;
    fn try_from(n: v1::SessionNotification) -> Result<Self, Self::Error> {
        Ok(Self {
            notification: n
                .notification
                .ok_or(WireError::MissingField("SessionNotification.notification"))?
                .try_into()?,
            age_seconds: n.age_seconds,
        })
    }
}

/// `google.protobuf.Timestamp`'s documented range, 0001-01-01T00:00:00Z
/// through 9999-12-31T23:59:59Z.
const TIMESTAMP_SECONDS: std::ops::RangeInclusive<i64> = -62_135_596_800..=253_402_300_799;
const NANOS_PER_SECOND: i32 = 1_000_000_000;

impl From<&messages::Attachment> for v1::Attachment {
    fn from(a: &messages::Attachment) -> Self {
        Self {
            id: a.id,
            attached_at: Some(a.attached_at.into()),
            input_owner: a.input_owner,
        }
    }
}

impl TryFrom<v1::Attachment> for messages::Attachment {
    type Error = WireError;
    fn try_from(a: v1::Attachment) -> Result<Self, Self::Error> {
        Ok(Self {
            id: a.id,
            attached_at: attached_at_from_wire(
                a.attached_at
                    .ok_or(WireError::MissingField("Attachment.attached_at"))?,
            )?,
            input_owner: a.input_owner,
        })
    }
}

/// A `Timestamp` outside its own documented range, or with a normalized
/// `nanos` field it does not carry, is a sender that did not build the
/// value from the schema; so is one this platform's clock cannot name.
fn attached_at_from_wire(t: prost_types::Timestamp) -> Result<std::time::SystemTime, WireError> {
    if !TIMESTAMP_SECONDS.contains(&t.seconds) || !(0..NANOS_PER_SECOND).contains(&t.nanos) {
        return Err(WireError::MalformedField("Attachment.attached_at"));
    }
    std::time::SystemTime::try_from(t)
        .map_err(|_| WireError::MalformedField("Attachment.attached_at"))
}

impl From<&messages::SessionInfo> for v1::SessionInfo {
    fn from(s: &messages::SessionInfo) -> Self {
        Self {
            id: id_to_bytes(s.id),
            dims: Some(s.dims.into()),
            title: s.title.clone(),
            cwd: s.cwd.clone(),
            idle_seconds: s.idle_seconds,
            tags: s.tags.clone(),
            last_notification: s.last_notification.as_ref().map(Into::into),
            foreground: s.foreground.clone(),
            exited: s.exited,
            last_exit_code: s.last_exit_code,
            attachments: s.attachments.iter().map(Into::into).collect(),
            sequence: s.sequence.get(),
        }
    }
}

impl TryFrom<v1::SessionInfo> for messages::SessionInfo {
    type Error = WireError;
    fn try_from(s: v1::SessionInfo) -> Result<Self, Self::Error> {
        Ok(Self {
            id: id_from_bytes(s.id)?,
            dims: dims_from_wire(s.dims, "SessionInfo.dims")?,
            title: s.title,
            cwd: s.cwd,
            idle_seconds: s.idle_seconds,
            tags: s.tags,
            last_notification: s.last_notification.map(TryInto::try_into).transpose()?,
            foreground: s.foreground,
            exited: s.exited,
            last_exit_code: s.last_exit_code,
            attachments: s
                .attachments
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            sequence: NonZeroU64::new(s.sequence)
                .ok_or(WireError::MalformedField("SessionInfo.sequence"))?,
        })
    }
}

impl From<messages::ResolvedId> for v1::ResolvedId {
    fn from(r: messages::ResolvedId) -> Self {
        use messages::ResolvedId as R;
        use v1::resolved_id::Result as W;
        let result = match r {
            R::Ok { id } => W::Ok(id_to_bytes(id)),
            R::NoMatch => W::NoMatch(v1::ResolvedNoMatch {}),
            R::Ambiguous { matches } => W::Ambiguous(matches),
        };
        Self {
            result: Some(result),
        }
    }
}

impl TryFrom<v1::ResolvedId> for messages::ResolvedId {
    type Error = WireError;
    fn try_from(r: v1::ResolvedId) -> Result<Self, Self::Error> {
        use v1::resolved_id::Result as W;
        Ok(
            match r
                .result
                .ok_or(WireError::MissingOneof("ResolvedId.result"))?
            {
                W::Ok(id) => Self::Ok {
                    id: id_from_bytes(id)?,
                },
                W::NoMatch(_) => Self::NoMatch,
                W::Ambiguous(matches) => Self::Ambiguous { matches },
            },
        )
    }
}

fn resolved_from_wire(
    r: Option<v1::ResolvedId>,
    field: &'static str,
) -> Result<messages::ResolvedId, WireError> {
    messages::ResolvedId::try_from(r.ok_or(WireError::MissingField(field))?)
}

impl From<&messages::SwitchTarget> for v1::SwitchTarget {
    fn from(t: &messages::SwitchTarget) -> Self {
        use messages::SwitchTarget as S;
        use v1::switch_target::Target;
        let target = match t {
            S::Session(prefix) => Target::Session(prefix.clone()),
            S::Carrier(c) => Target::Carrier(c.into()),
        };
        Self {
            target: Some(target),
        }
    }
}

impl TryFrom<v1::SwitchTarget> for messages::SwitchTarget {
    type Error = WireError;
    fn try_from(t: v1::SwitchTarget) -> Result<Self, Self::Error> {
        use v1::switch_target::Target;
        Ok(
            match t
                .target
                .ok_or(WireError::MissingOneof("SwitchTarget.target"))?
            {
                Target::Session(prefix) => Self::Session(prefix),
                Target::Carrier(c) => Self::Carrier(c.try_into()?),
            },
        )
    }
}

impl From<messages::SwitchScope> for v1::SwitchScope {
    fn from(s: messages::SwitchScope) -> Self {
        use messages::SwitchScope as S;
        use v1::switch_scope::Scope;
        let scope = match s {
            S::Default => Scope::Default(v1::SwitchScopeDefault {}),
            S::Attachment(id) => Scope::Attachment(id),
        };
        Self { scope: Some(scope) }
    }
}

impl TryFrom<v1::SwitchScope> for messages::SwitchScope {
    type Error = WireError;
    fn try_from(s: v1::SwitchScope) -> Result<Self, Self::Error> {
        use v1::switch_scope::Scope;
        Ok(
            match s
                .scope
                .ok_or(WireError::MissingOneof("SwitchScope.scope"))?
            {
                Scope::Default(_) => Self::Default,
                Scope::Attachment(id) => Self::Attachment(id),
            },
        )
    }
}

impl From<messages::SwitchDenied> for v1::SwitchDenied {
    fn from(d: messages::SwitchDenied) -> Self {
        use messages::SwitchDenied as D;
        use v1::switch_denied::Reason;
        let reason = match d {
            D::NoInputOwner => Reason::NoInputOwner(v1::SwitchNoInputOwner {}),
            D::NoSuchAttachment { attachment } => Reason::NoSuchAttachment(attachment),
        };
        Self {
            reason: Some(reason),
        }
    }
}

impl TryFrom<v1::SwitchDenied> for messages::SwitchDenied {
    type Error = WireError;
    fn try_from(d: v1::SwitchDenied) -> Result<Self, Self::Error> {
        use v1::switch_denied::Reason;
        Ok(
            match d
                .reason
                .ok_or(WireError::MissingOneof("SwitchDenied.reason"))?
            {
                Reason::NoInputOwner(_) => Self::NoInputOwner,
                Reason::NoSuchAttachment(attachment) => Self::NoSuchAttachment { attachment },
            },
        )
    }
}

const fn resource_kind_to_wire(k: messages::ResourceKind) -> v1::ResourceKind {
    use messages::ResourceKind as K;
    match k {
        K::Sessions => v1::ResourceKind::Sessions,
        K::ImageStoreBytes => v1::ResourceKind::ImageStoreBytes,
        K::InFlightDecodes => v1::ResourceKind::InFlightDecodes,
        K::InFlightDecodeBytes => v1::ResourceKind::InFlightDecodeBytes,
        K::SubscriberQueueBytes => v1::ResourceKind::SubscriberQueueBytes,
        K::Connections => v1::ResourceKind::Connections,
        K::PtyInputBytes => v1::ResourceKind::PtyInputBytes,
    }
}

/// `UNSPECIFIED` is a hard error on all three status enums: a defaulted
/// one would report a resource, unit, or subject the daemon never
/// chose.
fn resource_kind_from_wire(raw: i32) -> Result<messages::ResourceKind, WireError> {
    use messages::ResourceKind as K;
    match decode_enum::<v1::ResourceKind>("ResourceKind", raw)? {
        v1::ResourceKind::Unspecified => Err(WireError::UnspecifiedEnum("ResourceKind")),
        v1::ResourceKind::Sessions => Ok(K::Sessions),
        v1::ResourceKind::ImageStoreBytes => Ok(K::ImageStoreBytes),
        v1::ResourceKind::InFlightDecodes => Ok(K::InFlightDecodes),
        v1::ResourceKind::InFlightDecodeBytes => Ok(K::InFlightDecodeBytes),
        v1::ResourceKind::SubscriberQueueBytes => Ok(K::SubscriberQueueBytes),
        v1::ResourceKind::Connections => Ok(K::Connections),
        v1::ResourceKind::PtyInputBytes => Ok(K::PtyInputBytes),
    }
}

const fn resource_unit_to_wire(u: messages::ResourceUnit) -> v1::ResourceUnit {
    match u {
        messages::ResourceUnit::Count => v1::ResourceUnit::Count,
        messages::ResourceUnit::Bytes => v1::ResourceUnit::Bytes,
    }
}

fn resource_unit_from_wire(raw: i32) -> Result<messages::ResourceUnit, WireError> {
    match decode_enum::<v1::ResourceUnit>("ResourceUnit", raw)? {
        v1::ResourceUnit::Unspecified => Err(WireError::UnspecifiedEnum("ResourceUnit")),
        v1::ResourceUnit::Count => Ok(messages::ResourceUnit::Count),
        v1::ResourceUnit::Bytes => Ok(messages::ResourceUnit::Bytes),
    }
}

const fn subject_kind_to_wire(s: messages::SubjectKind) -> v1::SubjectKind {
    match s {
        messages::SubjectKind::Session => v1::SubjectKind::Session,
        messages::SubjectKind::Subscriber => v1::SubjectKind::Subscriber,
    }
}

fn subject_kind_from_wire(raw: i32) -> Result<messages::SubjectKind, WireError> {
    match decode_enum::<v1::SubjectKind>("SubjectKind", raw)? {
        v1::SubjectKind::Unspecified => Err(WireError::UnspecifiedEnum("SubjectKind")),
        v1::SubjectKind::Session => Ok(messages::SubjectKind::Session),
        v1::SubjectKind::Subscriber => Ok(messages::SubjectKind::Subscriber),
    }
}

impl From<messages::Limit> for v1::Limit {
    fn from(l: messages::Limit) -> Self {
        use v1::limit::Limit as L;
        Self {
            limit: Some(match l {
                messages::Limit::Bounded(bound) => L::Bounded(bound),
                messages::Limit::Unlimited => L::Unlimited(v1::Unlimited {}),
            }),
        }
    }
}

impl TryFrom<v1::Limit> for messages::Limit {
    type Error = WireError;
    fn try_from(l: v1::Limit) -> Result<Self, Self::Error> {
        use v1::limit::Limit as L;
        Ok(
            match l.limit.ok_or(WireError::MissingOneof("Limit.limit"))? {
                L::Bounded(bound) => Self::Bounded(bound),
                L::Unlimited(_) => Self::Unlimited,
            },
        )
    }
}

fn limit_from_wire(
    limit: Option<v1::Limit>,
    field: &'static str,
) -> Result<messages::Limit, WireError> {
    limit.ok_or(WireError::MissingField(field))?.try_into()
}

impl From<messages::ResourceReport> for v1::ResourceReport {
    fn from(r: messages::ResourceReport) -> Self {
        use v1::resource_report::Scope;
        let scope = match r.scope {
            messages::ReportScope::Daemon { global_limit } => Scope::Daemon(v1::DaemonScope {
                global_limit: Some(global_limit.into()),
            }),
            messages::ReportScope::Subject {
                subject,
                max_subject_used,
                per_subject_limit,
                global_limit,
            } => Scope::Subject(v1::SubjectScope {
                subject: subject_kind_to_wire(subject) as i32,
                max_subject_used,
                per_subject_limit: Some(per_subject_limit.into()),
                global_limit: Some(global_limit.into()),
            }),
        };
        Self {
            resource: resource_kind_to_wire(r.resource) as i32,
            unit: resource_unit_to_wire(r.unit) as i32,
            total_used: r.total_used,
            scope: Some(scope),
        }
    }
}

impl TryFrom<v1::ResourceReport> for messages::ResourceReport {
    type Error = WireError;
    fn try_from(r: v1::ResourceReport) -> Result<Self, Self::Error> {
        use v1::resource_report::Scope;
        let scope = match r
            .scope
            .ok_or(WireError::MissingOneof("ResourceReport.scope"))?
        {
            Scope::Daemon(d) => messages::ReportScope::Daemon {
                global_limit: limit_from_wire(d.global_limit, "DaemonScope.global_limit")?,
            },
            Scope::Subject(s) => messages::ReportScope::Subject {
                subject: subject_kind_from_wire(s.subject)?,
                max_subject_used: s.max_subject_used,
                per_subject_limit: limit_from_wire(
                    s.per_subject_limit,
                    "SubjectScope.per_subject_limit",
                )?,
                global_limit: limit_from_wire(s.global_limit, "SubjectScope.global_limit")?,
            },
        };
        Ok(Self {
            resource: resource_kind_from_wire(r.resource)?,
            unit: resource_unit_from_wire(r.unit)?,
            total_used: r.total_used,
            scope,
        })
    }
}

impl From<&messages::OpsToDaemonMsg> for v1::OpsToDaemonMsg {
    fn from(m: &messages::OpsToDaemonMsg) -> Self {
        use messages::OpsToDaemonMsg as O;
        use v1::ops_to_daemon_msg::Msg;
        let msg = match m {
            O::List => Msg::List(v1::OpsList {}),
            O::Destroy { id_prefix } => Msg::Destroy(v1::OpsDestroy {
                id_prefix: id_prefix.clone(),
            }),
            O::ForceDetach { id_prefix } => Msg::ForceDetach(v1::OpsForceDetach {
                id_prefix: id_prefix.clone(),
            }),
            O::Switch {
                from_prefix,
                target,
                scope,
            } => Msg::Switch(v1::OpsSwitch {
                from_prefix: from_prefix.clone(),
                target: Some(target.into()),
                scope: Some((*scope).into()),
            }),
            O::Tag {
                id_prefix,
                add,
                remove,
            } => Msg::Tag(v1::OpsTag {
                id_prefix: id_prefix.clone(),
                add: add.clone(),
                remove: remove.clone(),
            }),
            O::Status => Msg::Status(v1::OpsStatus {}),
            O::Spawn { args } => Msg::Spawn(v1::OpsSpawn {
                args: Some(args.into()),
            }),
            O::Stop { mode } => Msg::Stop(v1::OpsStop {
                mode: Some(match mode {
                    messages::StopMode::IfEmpty => v1::ops_stop::Mode::IfEmpty(v1::StopIfEmpty {}),
                    messages::StopMode::Force => v1::ops_stop::Mode::Force(v1::StopForce {}),
                    messages::StopMode::WhenEmpty => {
                        v1::ops_stop::Mode::WhenEmpty(v1::StopWhenEmpty {})
                    }
                }),
            }),
            O::Info { id_prefix } => Msg::Info(v1::OpsInfo {
                id_prefix: id_prefix.clone(),
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl From<&messages::OpsToClientMsg> for v1::OpsToClientMsg {
    fn from(m: &messages::OpsToClientMsg) -> Self {
        use messages::OpsToClientMsg as O;
        use v1::ops_to_client_msg::Msg;
        let msg = match m {
            O::Listed { sessions } => Msg::Listed(v1::OpsListed {
                sessions: sessions.iter().map(Into::into).collect(),
            }),
            O::Destroyed { resolved } => Msg::Destroyed(v1::OpsDestroyed {
                resolved: Some((*resolved).into()),
            }),
            O::Detached {
                resolved,
                was_attached,
            } => Msg::Detached(v1::OpsDetached {
                resolved: Some((*resolved).into()),
                was_attached: *was_attached,
            }),
            O::Switched {
                from,
                to,
                queued,
                denied,
            } => Msg::Switched(v1::OpsSwitched {
                from: Some((*from).into()),
                to: to.map(Into::into),
                queued: *queued,
                denied: denied.map(Into::into),
            }),
            O::TagsUpdated {
                resolved,
                tags,
                denied,
            } => Msg::TagsUpdated(v1::OpsTagsUpdated {
                resolved: Some((*resolved).into()),
                tags: tags.clone(),
                denied: denied.clone(),
            }),
            O::StatusReply {
                resources,
                worker_threads,
                draining,
            } => Msg::StatusReply(v1::OpsStatusReply {
                resources: resources.iter().copied().map(Into::into).collect(),
                worker_threads: *worker_threads,
                draining: *draining,
            }),
            O::Spawned { outcome } => Msg::Spawned(v1::OpsSpawned {
                outcome: Some(match outcome {
                    messages::SpawnOutcome::Ok { info } => {
                        v1::ops_spawned::Outcome::Ok(info.as_ref().into())
                    }
                    messages::SpawnOutcome::Refused { reason, detail } => {
                        v1::ops_spawned::Outcome::Refused(v1::SpawnRefused {
                            reason: super::session::create_failure_to_wire(*reason) as i32,
                            detail: detail.clone(),
                        })
                    }
                }),
            }),
            O::InfoReply { outcome } => Msg::InfoReply(v1::OpsInfoReply {
                outcome: Some(match outcome {
                    messages::InfoOutcome::Found { session, short_id } => {
                        v1::ops_info_reply::Outcome::Found(v1::OpsInfoFound {
                            session: Some(session.as_ref().into()),
                            short_id: short_id.clone(),
                        })
                    }
                    messages::InfoOutcome::NoMatch => {
                        v1::ops_info_reply::Outcome::NoMatch(v1::OpsInfoNoMatch {})
                    }
                    messages::InfoOutcome::Ambiguous { matches } => {
                        v1::ops_info_reply::Outcome::Ambiguous(v1::OpsInfoAmbiguous {
                            matches: *matches,
                        })
                    }
                }),
            }),
            O::StopReply { outcome } => Msg::StopReply(v1::OpsStopReply {
                outcome: Some(match outcome {
                    messages::StopOutcome::Stopping => {
                        v1::ops_stop_reply::Outcome::Stopping(v1::StopStopping {})
                    }
                    messages::StopOutcome::Refused { sessions } => {
                        v1::ops_stop_reply::Outcome::Refused(v1::StopRefused {
                            sessions: *sessions,
                        })
                    }
                    messages::StopOutcome::Draining { sessions } => {
                        v1::ops_stop_reply::Outcome::Draining(v1::StopDraining {
                            sessions: *sessions,
                        })
                    }
                }),
            }),
        };
        Self {
            msg: Some(msg),
            correlation: None,
        }
    }
}

impl TryFrom<v1::OpsToDaemonMsg> for messages::OpsToDaemonMsg {
    type Error = WireError;
    fn try_from(m: v1::OpsToDaemonMsg) -> Result<Self, Self::Error> {
        use v1::ops_to_daemon_msg::Msg;
        Ok(
            match m.msg.ok_or(WireError::MissingOneof("OpsToDaemonMsg.msg"))? {
                Msg::List(_) => Self::List,
                Msg::Destroy(d) => Self::Destroy {
                    id_prefix: d.id_prefix,
                },
                Msg::ForceDetach(f) => Self::ForceDetach {
                    id_prefix: f.id_prefix,
                },
                Msg::Switch(s) => Self::Switch {
                    from_prefix: s.from_prefix,
                    target: s
                        .target
                        .ok_or(WireError::MissingField("OpsSwitch.target"))?
                        .try_into()?,
                    scope: s
                        .scope
                        .ok_or(WireError::MissingField("OpsSwitch.scope"))?
                        .try_into()?,
                },
                Msg::Tag(t) => Self::Tag {
                    id_prefix: t.id_prefix,
                    add: t.add,
                    remove: t.remove,
                },
                Msg::Status(_) => Self::Status,
                Msg::Spawn(s) => Self::Spawn {
                    args: s
                        .args
                        .ok_or(WireError::MissingField("OpsSpawn.args"))?
                        .try_into()?,
                },
                Msg::Stop(s) => Self::Stop {
                    mode: match s.mode.ok_or(WireError::MissingOneof("OpsStop.mode"))? {
                        v1::ops_stop::Mode::IfEmpty(_) => messages::StopMode::IfEmpty,
                        v1::ops_stop::Mode::Force(_) => messages::StopMode::Force,
                        v1::ops_stop::Mode::WhenEmpty(_) => messages::StopMode::WhenEmpty,
                    },
                },
                Msg::Info(i) => Self::Info {
                    id_prefix: i.id_prefix,
                },
            },
        )
    }
}

impl TryFrom<v1::OpsToClientMsg> for messages::OpsToClientMsg {
    type Error = WireError;
    fn try_from(m: v1::OpsToClientMsg) -> Result<Self, Self::Error> {
        use v1::ops_to_client_msg::Msg;
        Ok(
            match m.msg.ok_or(WireError::MissingOneof("OpsToClientMsg.msg"))? {
                Msg::Listed(l) => Self::Listed {
                    sessions: l
                        .sessions
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                },
                Msg::Destroyed(d) => Self::Destroyed {
                    resolved: resolved_from_wire(d.resolved, "OpsDestroyed.resolved")?,
                },
                Msg::Detached(d) => Self::Detached {
                    resolved: resolved_from_wire(d.resolved, "OpsDetached.resolved")?,
                    was_attached: d.was_attached,
                },
                Msg::Switched(s) => Self::Switched {
                    from: resolved_from_wire(s.from, "OpsSwitched.from")?,
                    to: s.to.map(TryInto::try_into).transpose()?,
                    queued: s.queued,
                    denied: s.denied.map(TryInto::try_into).transpose()?,
                },
                Msg::TagsUpdated(t) => Self::TagsUpdated {
                    resolved: resolved_from_wire(t.resolved, "OpsTagsUpdated.resolved")?,
                    tags: t.tags,
                    denied: t.denied,
                },
                Msg::StatusReply(s) => Self::StatusReply {
                    resources: s
                        .resources
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, _>>()?,
                    worker_threads: s.worker_threads,
                    draining: s.draining,
                },
                Msg::Spawned(s) => Self::Spawned {
                    outcome: match s
                        .outcome
                        .ok_or(WireError::MissingOneof("OpsSpawned.outcome"))?
                    {
                        v1::ops_spawned::Outcome::Ok(info) => messages::SpawnOutcome::Ok {
                            info: Box::new(info.try_into()?),
                        },
                        v1::ops_spawned::Outcome::Refused(r) => messages::SpawnOutcome::Refused {
                            reason: super::session::create_failure_from_wire(r.reason)?,
                            detail: r.detail,
                        },
                    },
                },
                Msg::InfoReply(r) => Self::InfoReply {
                    outcome: match r
                        .outcome
                        .ok_or(WireError::MissingOneof("OpsInfoReply.outcome"))?
                    {
                        v1::ops_info_reply::Outcome::Found(f) => messages::InfoOutcome::Found {
                            session: Box::new(
                                f.session
                                    .ok_or(WireError::MissingField("OpsInfoFound.session"))?
                                    .try_into()?,
                            ),
                            short_id: f.short_id,
                        },
                        v1::ops_info_reply::Outcome::NoMatch(_) => messages::InfoOutcome::NoMatch,
                        v1::ops_info_reply::Outcome::Ambiguous(a) => {
                            messages::InfoOutcome::Ambiguous { matches: a.matches }
                        }
                    },
                },
                Msg::StopReply(s) => Self::StopReply {
                    outcome: match s
                        .outcome
                        .ok_or(WireError::MissingOneof("OpsStopReply.outcome"))?
                    {
                        v1::ops_stop_reply::Outcome::Stopping(_) => messages::StopOutcome::Stopping,
                        v1::ops_stop_reply::Outcome::Refused(r) => messages::StopOutcome::Refused {
                            sessions: r.sessions,
                        },
                        v1::ops_stop_reply::Outcome::Draining(d) => {
                            messages::StopOutcome::Draining {
                                sessions: d.sessions,
                            }
                        }
                    },
                },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_session(sequence: u64, attachments: Vec<v1::Attachment>) -> v1::SessionInfo {
        v1::SessionInfo {
            id: id_to_bytes(0xCAFE),
            dims: Some(v1::GridDims {
                rows: 24,
                cols: 80,
                pixel_w: 0,
                pixel_h: 0,
            }),
            title: None,
            cwd: None,
            idle_seconds: None,
            tags: Vec::new(),
            last_notification: None,
            foreground: None,
            exited: false,
            last_exit_code: None,
            attachments,
            sequence,
        }
    }

    fn wire_attachment(attached_at: Option<prost_types::Timestamp>) -> v1::Attachment {
        v1::Attachment {
            id: 7,
            attached_at,
            input_owner: false,
        }
    }

    /// `1` is the first value a daemon mints. A `0` (which is also what
    /// a peer omitting the field writes) is refused rather than read as
    /// a row outside the ring's order.
    #[test]
    fn the_first_sequence_decodes_and_a_zero_is_rejected() {
        let first = messages::SessionInfo::try_from(wire_session(1, Vec::new())).unwrap();
        assert_eq!(first.sequence, NonZeroU64::MIN);
        assert!(matches!(
            messages::SessionInfo::try_from(wire_session(0, Vec::new())),
            Err(WireError::MalformedField("SessionInfo.sequence"))
        ));
    }

    #[test]
    fn an_attachment_without_a_stamp_is_rejected() {
        assert!(matches!(
            messages::Attachment::try_from(wire_attachment(None)),
            Err(WireError::MissingField("Attachment.attached_at"))
        ));
    }

    /// Outside `google.protobuf.Timestamp`'s own range, or with `nanos`
    /// past a second, the value is refused instead of normalized.
    #[test]
    fn a_timestamp_outside_the_type_range_is_rejected() {
        for stamp in [
            prost_types::Timestamp {
                seconds: 253_402_300_800,
                nanos: 0,
            },
            prost_types::Timestamp {
                seconds: -62_135_596_801,
                nanos: 0,
            },
            prost_types::Timestamp {
                seconds: 0,
                nanos: 1_000_000_000,
            },
            prost_types::Timestamp {
                seconds: 0,
                nanos: -1,
            },
        ] {
            assert!(matches!(
                messages::Attachment::try_from(wire_attachment(Some(stamp))),
                Err(WireError::MalformedField("Attachment.attached_at"))
            ));
        }
    }

    /// The stamp survives an encode/decode of the message at nanosecond
    /// resolution.
    #[test]
    fn an_attach_stamp_round_trips() {
        use prost::Message as _;

        let at = std::time::UNIX_EPOCH + std::time::Duration::new(1_788_250_500, 123_456_789);
        let domain = messages::Attachment {
            id: 7,
            attached_at: at,
            input_owner: true,
        };
        let bytes = v1::Attachment::from(&domain).encode_to_vec();
        let back =
            messages::Attachment::try_from(v1::Attachment::decode(bytes.as_slice()).unwrap())
                .unwrap();
        assert_eq!(back, domain);
    }

    fn wire_report(scope: Option<v1::resource_report::Scope>) -> v1::ResourceReport {
        v1::ResourceReport {
            resource: v1::ResourceKind::Sessions as i32,
            unit: v1::ResourceUnit::Count as i32,
            total_used: 3,
            scope,
        }
    }

    /// A row with no scope names no denominator for its numbers.
    #[test]
    fn a_report_without_a_scope_is_rejected() {
        assert!(matches!(
            messages::ResourceReport::try_from(wire_report(None)),
            Err(WireError::MissingOneof("ResourceReport.scope"))
        ));
    }

    /// An unset `Limit` oneof is neither a bound nor "unlimited", so it
    /// is refused rather than read as either.
    #[test]
    fn a_limit_without_an_arm_is_rejected() {
        assert!(matches!(
            messages::Limit::try_from(v1::Limit { limit: None }),
            Err(WireError::MissingOneof("Limit.limit"))
        ));
        assert!(matches!(
            messages::ResourceReport::try_from(wire_report(Some(
                v1::resource_report::Scope::Daemon(v1::DaemonScope {
                    global_limit: Some(v1::Limit { limit: None }),
                })
            ))),
            Err(WireError::MissingOneof("Limit.limit"))
        ));
    }

    /// A `Limit` message absent altogether is a peer that did not build
    /// from this schema, not a ceiling felis does not budget.
    #[test]
    fn a_missing_limit_message_is_rejected() {
        assert!(matches!(
            messages::ResourceReport::try_from(wire_report(Some(
                v1::resource_report::Scope::Daemon(v1::DaemonScope { global_limit: None })
            ))),
            Err(WireError::MissingField("DaemonScope.global_limit"))
        ));
    }

    /// `UNSPECIFIED` would name a subject the daemon never chose.
    #[test]
    fn a_subject_scope_without_a_kind_is_rejected() {
        assert!(matches!(
            messages::ResourceReport::try_from(wire_report(Some(
                v1::resource_report::Scope::Subject(v1::SubjectScope {
                    subject: v1::SubjectKind::Unspecified as i32,
                    max_subject_used: 0,
                    per_subject_limit: Some(v1::Limit {
                        limit: Some(v1::limit::Limit::Unlimited(v1::Unlimited {})),
                    }),
                    global_limit: Some(v1::Limit {
                        limit: Some(v1::limit::Limit::Unlimited(v1::Unlimited {})),
                    }),
                })
            ))),
            Err(WireError::UnspecifiedEnum("SubjectKind"))
        ));
    }

    /// `Bounded(0)` and `Unlimited` are distinct through the wire: the
    /// sentinel reading this shape replaced would collapse them.
    #[test]
    fn a_zero_bound_survives_as_a_bound() {
        use prost::Message as _;

        let domain = messages::ResourceReport {
            resource: messages::ResourceKind::Sessions,
            unit: messages::ResourceUnit::Count,
            total_used: 0,
            scope: messages::ReportScope::Subject {
                subject: messages::SubjectKind::Session,
                max_subject_used: 0,
                per_subject_limit: messages::Limit::Bounded(0),
                global_limit: messages::Limit::Unlimited,
            },
        };
        let bytes = v1::ResourceReport::from(domain).encode_to_vec();
        let back = messages::ResourceReport::try_from(
            v1::ResourceReport::decode(bytes.as_slice()).unwrap(),
        )
        .unwrap();
        assert_eq!(back, domain);
    }
}
