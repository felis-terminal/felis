use anyhow::Result;
use felis_protocol::{
    codec,
    messages::{
        Correlation, InputMsg, NotifyToDaemonMsg, OpsToDaemonMsg, RegionSource, RegionToDaemonMsg,
        RequestId, SearchOptions, SearchToDaemonMsg,
    },
};
use serde_json::Value;

use super::{MAX_ID_BYTES, MAX_SAFE_JSON_INTEGER, core::Core, envelope::BridgeError, json_integer};
use crate::cli_output::{ErrorKind, SURFACE_VERSION};

fn validate_session_prefix(prefix: &str) -> Result<(), BridgeError> {
    crate::validate_session_id_prefix(prefix)
        .map(|_| ())
        .map_err(BridgeError::malformed)
}

fn check_body_len(len: usize) -> Result<(), BridgeError> {
    felis_protocol::messages::check_limit(
        "daemon frame body",
        len,
        felis_protocol::frame::DEFAULT_MAX_BODY as usize,
    )
    .map_err(|err| BridgeError::over_limit(&err))
}

pub(super) fn preflight_correlated<M: codec::Correlated>(msg: &M) -> Result<(), BridgeError> {
    codec::WireCodec::validate(msg).map_err(|err| BridgeError::over_limit(&err))?;
    let Some(max_request_id) = RequestId::new(u64::MAX) else {
        return Err(BridgeError::invalid("the request-id domain is empty"));
    };
    check_body_len(codec::encode_correlated(msg, Correlation::request(max_request_id)).len())
}

pub(super) fn preflight_uncorrelated<M: codec::WireCodec>(msg: &M) -> Result<(), BridgeError> {
    codec::WireCodec::validate(msg).map_err(|err| BridgeError::over_limit(&err))?;
    check_body_len(codec::encode(msg).len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Daemon(DaemonOp),
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DaemonOp {
    List,
    Info,
    Spawn,
    Send,
    Kill,
    Evict,
    Switch,
    Tag,
    Capture,
    Search,
    Subscribe,
}

impl Operation {
    /// In the order the published request schema lists them.
    pub(crate) const ALL: [Self; 12] = [
        Self::Daemon(DaemonOp::List),
        Self::Daemon(DaemonOp::Info),
        Self::Daemon(DaemonOp::Spawn),
        Self::Daemon(DaemonOp::Send),
        Self::Daemon(DaemonOp::Kill),
        Self::Daemon(DaemonOp::Evict),
        Self::Daemon(DaemonOp::Switch),
        Self::Daemon(DaemonOp::Tag),
        Self::Daemon(DaemonOp::Capture),
        Self::Daemon(DaemonOp::Search),
        Self::Daemon(DaemonOp::Subscribe),
        Self::Cancel,
    ];

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.name() == name)
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Daemon(DaemonOp::List) => "sessions.list",
            Self::Daemon(DaemonOp::Info) => "sessions.info",
            Self::Daemon(DaemonOp::Spawn) => "sessions.spawn",
            Self::Daemon(DaemonOp::Send) => "sessions.send",
            Self::Daemon(DaemonOp::Kill) => "sessions.kill",
            Self::Daemon(DaemonOp::Evict) => "sessions.evict",
            Self::Daemon(DaemonOp::Switch) => "sessions.switch",
            Self::Daemon(DaemonOp::Tag) => "sessions.tag",
            Self::Daemon(DaemonOp::Capture) => "sessions.capture",
            Self::Daemon(DaemonOp::Search) => "sessions.search",
            Self::Daemon(DaemonOp::Subscribe) => "notifications.subscribe",
            Self::Cancel => "cancel",
        }
    }

    pub(crate) const fn params(self) -> &'static [&'static str] {
        match self {
            Self::Daemon(DaemonOp::List) => &["tags"],
            Self::Daemon(
                DaemonOp::Info | DaemonOp::Kill | DaemonOp::Evict | DaemonOp::Subscribe,
            ) => &["session"],
            Self::Daemon(DaemonOp::Spawn) => &["cwd", "env", "rows", "cols", "tags", "cmd"],
            Self::Daemon(DaemonOp::Send) => &["session", "text", "raw"],
            Self::Daemon(DaemonOp::Switch) => &["to", "from", "attachment"],
            Self::Daemon(DaemonOp::Tag) => &["session", "add", "remove"],
            Self::Daemon(DaemonOp::Capture) => &["session", "source", "ansi", "lines"],
            Self::Daemon(DaemonOp::Search) => &["session", "pattern", "regex", "case_insensitive"],
            Self::Cancel => &["target"],
        }
    }

    pub(crate) const fn is_streaming(self) -> bool {
        matches!(self, Self::Daemon(op) if op.is_streaming())
    }
}

impl DaemonOp {
    pub(crate) const fn is_streaming(self) -> bool {
        matches!(self, Self::Capture | Self::Search | Self::Subscribe)
    }
}

pub(super) struct Request {
    /// Held as a `Value` so the bridge never re-spells the client's id
    /// (an integer stays an integer).
    pub(super) id: Value,
    pub(super) op: Result<Operation, String>,
    pub(super) params: Value,
}

impl Request {
    pub(super) const fn streaming(&self) -> bool {
        matches!(self.op, Ok(op) if op.is_streaming())
    }
}

/// A parse failure is not fatal: an editor that emits one bad line
/// must not lose the session it has open. The error echoes the id when
/// one could be read and `null` otherwise.
pub(super) fn parse_request(line: &str) -> Result<Request, (Value, bool, BridgeError)> {
    let value: Value = serde_json::from_str(line).map_err(|err| {
        (
            Value::Null,
            false,
            BridgeError::malformed(format!("not JSON: {err}")),
        )
    })?;
    let Some(object) = value.as_object() else {
        return Err((
            Value::Null,
            false,
            BridgeError::malformed("a request must be a JSON object"),
        ));
    };
    let op_name = object.get("op").and_then(Value::as_str);
    let op = op_name.and_then(Operation::from_name);
    let streaming = op.is_some_and(Operation::is_streaming);
    // The id is read first, so every later failure on this line is
    // still attributable to the request.
    let id = match object.get("id") {
        Some(Value::String(id)) if id.len() <= MAX_ID_BYTES => Value::String(id.clone()),
        Some(Value::String(id)) => {
            return Err((
                Value::Null,
                streaming,
                BridgeError::malformed(format!(
                    "a string `id` must be at most {MAX_ID_BYTES} bytes (got {})",
                    id.len()
                )),
            ));
        }
        // Normalized, not echoed verbatim: the id keys the in-flight
        // table, and `1.0` must claim the same slot as `1`.
        Some(id @ Value::Number(_)) => match json_integer(id) {
            Some(id) if id <= MAX_SAFE_JSON_INTEGER => Value::from(id),
            _ => {
                return Err((
                    Value::Null,
                    streaming,
                    BridgeError::malformed(format!(
                        "a numeric `id` must be an integer between 0 and {MAX_SAFE_JSON_INTEGER}"
                    )),
                ));
            }
        },
        Some(_) | None => {
            return Err((
                Value::Null,
                streaming,
                BridgeError::malformed("`id` must be present and a string or a safe integer"),
            ));
        }
    };
    if let Some(unknown) = object
        .keys()
        .find(|key| !matches!(key.as_str(), "v" | "id" | "op" | "params"))
    {
        return Err((
            id,
            streaming,
            BridgeError::malformed(format!("unknown request field `{unknown}`")),
        ));
    }
    match object.get("v") {
        Some(v) if json_integer(v) == Some(u64::from(SURFACE_VERSION)) => {}
        Some(v) => {
            return Err((
                id,
                streaming,
                BridgeError::malformed(format!(
                    "unsupported surface version {v}; this bridge speaks v{SURFACE_VERSION}"
                )),
            ));
        }
        None => {
            return Err((
                id,
                streaming,
                BridgeError::malformed("`v` is required on every request"),
            ));
        }
    }
    let Some(op_name) = op_name else {
        return Err((
            id,
            streaming,
            BridgeError::malformed("`op` must be a string"),
        ));
    };
    let params = match object.get("params") {
        None => Value::Null,
        Some(params @ Value::Object(_)) => params.clone(),
        Some(_) => {
            return Err((
                id,
                streaming,
                BridgeError::malformed("`params` must be an object"),
            ));
        }
    };
    Ok(Request {
        id,
        op: op.ok_or_else(|| op_name.to_owned()),
        params,
    })
}

pub(super) struct Params<'a>(pub(super) &'a Value);

impl Params<'_> {
    pub(super) fn only(&self, allowed: &[&str]) -> Result<(), BridgeError> {
        if self.0.is_null() {
            return Ok(());
        }
        let Some(object) = self.0.as_object() else {
            return Err(BridgeError::malformed("`params` must be an object"));
        };
        if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(BridgeError::malformed(format!(
                "unknown parameter `{unknown}`"
            )));
        }
        Ok(())
    }

    pub(super) fn str_opt(&self, key: &str) -> Result<Option<String>, BridgeError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(BridgeError::malformed(format!("`{key}` must be a string"))),
        }
    }

    pub(super) fn str_req(&self, key: &str) -> Result<String, BridgeError> {
        self.str_opt(key)?
            .ok_or_else(|| BridgeError::malformed(format!("`{key}` is required")))
    }

    pub(super) fn bool(&self, key: &str) -> Result<bool, BridgeError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(BridgeError::malformed(format!("`{key}` must be a boolean"))),
        }
    }

    pub(super) fn u32_opt(&self, key: &str) -> Result<Option<u32>, BridgeError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => json_integer(v)
                .and_then(|n| u32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| {
                    BridgeError::malformed(format!("`{key}` must be a non-negative integer"))
                }),
        }
    }

    pub(super) fn strings(&self, key: &str) -> Result<Vec<String>, BridgeError> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_owned).ok_or_else(|| {
                        BridgeError::malformed(format!("`{key}` must be an array of strings"))
                    })
                })
                .collect(),
            Some(_) => Err(BridgeError::malformed(format!("`{key}` must be an array"))),
        }
    }

    /// Pairs, not an object: the wire's `SpawnArgs::env` is an ordered
    /// list, and a JSON object would silently reorder and dedupe it.
    pub(super) fn env_pairs(&self) -> Result<Vec<(String, String)>, BridgeError> {
        // `null` reads as absent, as it does for every other optional
        // parameter: the published schema renders an `Option` as a
        // nullable type, so refusing it here would make a request the
        // schema calls valid a `malformed_request`.
        let items = match self.0.get("env") {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(items)) => items,
            Some(_) => {
                return Err(BridgeError::malformed(
                    "`env` must be an array of [KEY, VALUE] pairs",
                ));
            }
        };
        items
            .iter()
            .map(|pair| match pair.as_array().map(Vec::as_slice) {
                Some([Value::String(k), Value::String(v)]) => Ok((k.clone(), v.clone())),
                _ => Err(BridgeError::malformed(
                    "each `env` entry must be a [KEY, VALUE] pair of strings",
                )),
            })
            .collect()
    }
}

/// The child's base environment is captured from *this* process, as it
/// is on `sessions spawn`: over a relay the bridge's environment
/// describes the wrong host, so it stays absent there (REQ-912a,
/// docs/reference/ipc.md "Session (kind = 4)").
pub(super) fn spawn_args(
    params: &Params<'_>,
    carrier: &felis_client_core::Carrier,
) -> Result<felis_protocol::messages::SpawnArgs, BridgeError> {
    let args =
        felis_client_core::env_base::fill_for_carrier(spawn_request(params, carrier)?, carrier);
    felis_protocol::messages::Validate::validate(&args).map_err(|e| BridgeError::over_limit(&e))?;
    Ok(args)
}

fn spawn_request(
    params: &Params<'_>,
    carrier: &felis_client_core::Carrier,
) -> Result<felis_protocol::messages::SpawnArgs, BridgeError> {
    let local = matches!(carrier, felis_client_core::Carrier::Local(_));
    Ok(felis_protocol::messages::SpawnArgs {
        cwd: resolve_bridge_cwd(params.str_opt("cwd")?, local, std::env::current_dir())?,
        env: params.env_pairs()?,
        dims: spawn_dims(params)?,
        tags: params.strings("tags")?,
        ..felis_client_core::spawn_args_from_cli(params.strings("cmd")?)
    })
}

/// No `$FELIS_SESSION_ID` fallback: the bridge runs inside the editor,
/// not a felis window, so it would name whatever session launched the
/// editor.
pub(super) fn switch_from(params: &Params<'_>) -> Result<String, BridgeError> {
    params.str_opt("from")?.ok_or_else(|| {
        BridgeError::invalid("`from` is required: name the session whose window should move")
    })
}

pub(super) fn switch_scope(
    params: &Params<'_>,
) -> Result<felis_protocol::messages::SwitchScope, BridgeError> {
    Ok(match params.str_opt("attachment")? {
        Some(raw) => felis_protocol::messages::SwitchScope::Attachment(
            crate::cli_sessions::parse_attachment_id(&raw).map_err(BridgeError::malformed)?,
        ),
        None => felis_protocol::messages::SwitchScope::Default,
    })
}

pub(super) fn region_source(params: &Params<'_>) -> Result<RegionSource, BridgeError> {
    match params.str_opt("source")? {
        Some(raw) => crate::cli_sessions::parse_region_source(&raw).map_err(BridgeError::malformed),
        None => Ok(RegionSource::Visible),
    }
}

/// `text` only, no chord spelling: the bridge freezes the parameters
/// its clients have needed, and `keys` is additive within `v:1`
/// (docs/reference/cli.md "`felis bridge`"). A client that wants a
/// control byte today sends it with `raw`.
pub(super) fn send_input(params: &Params<'_>) -> Result<InputMsg, BridgeError> {
    let bytes = params.str_req("text")?.into_bytes();
    Ok(if params.bool("raw")? {
        InputMsg::KeyBytes(bytes)
    } else {
        InputMsg::Paste(bytes)
    })
}

pub(super) fn check_search_pattern(pattern: &str) -> Result<(), BridgeError> {
    felis_protocol::messages::check_limit(
        "Search::Query.query",
        pattern.len(),
        felis_protocol::messages::MAX_SEARCH_PATTERN_BYTES,
    )
    .map_err(|err| BridgeError::over_limit(&err))
}

/// The in-flight key a `cancel` names, normalized as the id was, or
/// `1.0` would never find the request admitted as `1`.
pub(super) fn cancel_target(params: &Params<'_>) -> Option<String> {
    match params.0.get("target") {
        Some(Value::String(target)) if target.len() <= MAX_ID_BYTES => {
            Some(Value::String(target.clone()).to_string())
        }
        Some(target @ Value::Number(_)) => json_integer(target)
            .filter(|target| *target <= MAX_SAFE_JSON_INTEGER)
            .map(|target| Value::from(target).to_string()),
        Some(_) | None => None,
    }
}

/// A create names its whole grid or none of it: absence is what asks
/// for the daemon's default, so a lone `rows` would have to invent the
/// other axis to say anything at all.
pub(super) fn spawn_dims(
    params: &Params<'_>,
) -> Result<Option<felis_protocol::messages::RequestedDims>, BridgeError> {
    match (params.u32_opt("rows")?, params.u32_opt("cols")?) {
        (Some(rows), Some(cols)) => Ok(Some(felis_protocol::messages::RequestedDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        })),
        (None, None) => Ok(None),
        _ => Err(BridgeError::invalid(
            "`rows` and `cols` must be given together; omit both for the daemon's default grid",
        )),
    }
}

pub(super) fn resolve_bridge_cwd(
    explicit: Option<String>,
    local: bool,
    current: std::io::Result<std::path::PathBuf>,
) -> Result<String, BridgeError> {
    match explicit {
        Some(cwd) if local && !std::path::Path::new(&cwd).is_absolute() => {
            let base = current.map_err(|err| {
                BridgeError::new(
                    ErrorKind::InvalidRequest,
                    format!("resolve the bridge's current directory: {err}"),
                )
            })?;
            let resolved = base.join(cwd);
            if !resolved.is_absolute() {
                return Err(BridgeError::new(
                    ErrorKind::InvalidRequest,
                    "the local `cwd` could not be resolved to an absolute path",
                ));
            }
            // Not `to_string_lossy`: `SpawnArgs.cwd` is a wire `string`
            // and the daemon chdirs into it, so a U+FFFD spelling would
            // name a directory that does not exist.
            resolved.to_str().map(str::to_owned).ok_or_else(|| {
                BridgeError::new(
                    ErrorKind::InvalidRequest,
                    format!(
                        "the local `cwd` resolved to `{}`, which is not valid UTF-8",
                        resolved.display()
                    ),
                )
            })
        }
        Some(cwd) => Ok(cwd),
        None => Ok(String::new()),
    }
}

impl Core {
    pub(super) fn validate_request(&self, request: &Request) -> Result<Operation, BridgeError> {
        let op = match &request.op {
            Ok(op) => *op,
            Err(name) => return Err(BridgeError::malformed(format!("unknown op `{name}`"))),
        };
        let params = Params(&request.params);
        params.only(op.params())?;
        match op {
            Operation::Daemon(DaemonOp::List) => {
                let _tags = params.strings("tags")?;
                preflight_correlated(&OpsToDaemonMsg::List)?;
            }
            Operation::Daemon(DaemonOp::Info) => {
                validate_session_prefix(&params.str_req("session")?)?;
                preflight_correlated(&OpsToDaemonMsg::List)?;
            }
            Operation::Daemon(DaemonOp::Kill) => {
                let session = params.str_req("session")?;
                validate_session_prefix(&session)?;
                preflight_correlated(&OpsToDaemonMsg::Destroy { id_prefix: session })?;
            }
            Operation::Daemon(DaemonOp::Evict) => {
                let session = params.str_req("session")?;
                validate_session_prefix(&session)?;
                preflight_correlated(&OpsToDaemonMsg::ForceDetach { id_prefix: session })?;
            }
            Operation::Daemon(DaemonOp::Spawn) => {
                let args = spawn_request(&params, &self.target.carrier)?;
                felis_protocol::messages::Validate::validate(&args)
                    .map_err(|err| BridgeError::over_limit(&err))?;
                preflight_correlated(&OpsToDaemonMsg::Spawn { args })?;
            }
            Operation::Daemon(DaemonOp::Switch) => {
                let to = params.str_req("to")?;
                validate_session_prefix(&to)?;
                let from = switch_from(&params)?;
                let scope = switch_scope(&params)?;
                validate_session_prefix(&from)?;
                preflight_correlated(&OpsToDaemonMsg::Switch {
                    from_prefix: from,
                    target: felis_protocol::messages::SwitchTarget::Session(to),
                    scope,
                })?;
            }
            Operation::Daemon(DaemonOp::Tag) => {
                let session = params.str_req("session")?;
                validate_session_prefix(&session)?;
                let add = params.strings("add")?;
                let remove = params.strings("remove")?;
                if add.is_empty() && remove.is_empty() {
                    return Err(BridgeError::malformed(
                        "at least one of `add` or `remove` is required",
                    ));
                }
                preflight_correlated(&OpsToDaemonMsg::Tag {
                    id_prefix: session,
                    add,
                    remove,
                })?;
            }
            Operation::Daemon(DaemonOp::Send) => {
                validate_session_prefix(&params.str_req("session")?)?;
                preflight_uncorrelated(&send_input(&params)?)?;
            }
            Operation::Daemon(DaemonOp::Capture) => {
                validate_session_prefix(&params.str_req("session")?)?;
                let source = region_source(&params)?;
                let ansi = params.bool("ansi")?;
                let max_rows = params.u32_opt("lines")?;
                preflight_correlated(&RegionToDaemonMsg::Rows {
                    source,
                    ansi,
                    max_rows,
                })?;
            }
            Operation::Daemon(DaemonOp::Search) => {
                validate_session_prefix(&params.str_req("session")?)?;
                let pattern = params.str_req("pattern")?;
                let regex = params.bool("regex")?;
                let case_insensitive = params.bool("case_insensitive")?;
                check_search_pattern(&pattern)?;
                preflight_correlated(&SearchToDaemonMsg::Query {
                    query: pattern,
                    options: SearchOptions {
                        regex,
                        case_insensitive,
                    },
                })?;
            }
            Operation::Daemon(DaemonOp::Subscribe) => {
                let session_prefix = params.str_opt("session")?;
                if let Some(prefix) = &session_prefix {
                    validate_session_prefix(prefix)?;
                }
                preflight_correlated(&NotifyToDaemonMsg::Subscribe { session_prefix })?;
            }
            Operation::Cancel => {
                if cancel_target(&params).is_none() {
                    return Err(BridgeError::malformed(
                        "`target` must be the id of an in-flight request",
                    ));
                }
            }
        }
        Ok(op)
    }
}
