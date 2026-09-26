//! The two published JSON Schema documents for the machine surface,
//! rendered from the serde types (docs/reference/cli.md "JSON
//! Schema"). Nothing here spells a field name; only the envelope,
//! which serde has no type for, is composed by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings, json_schema};
use serde_json::{Map, Value, json};

use crate::cli_bridge::{DaemonOp, Operation};
use crate::cli_output::{
    CaptureRow, CheckResult, ConfigPathResult, DaemonStatusResult, DaemonStopResult, DoctorResult,
    EffectiveConfigResult, ListResult, MachineError, SURFACE_VERSION, SearchMatch, SessionObject,
    SessionRef, SwitchResult, TagResult,
};
use crate::cli_version::VersionResult;

const CLI_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/felis-cli-v1.schema.json"
);
const BRIDGE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/felis-bridge-v1.schema.json"
);

const RAW_BASE: &str =
    "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas";

/// A session id as the surface publishes it, never as a request may
/// spell it: output is always the full lowercase rendering.
const SESSION_ID_PATTERN: &str = "^[0-9a-f]{32}$";
/// What `<ID-OR-PREFIX>` accepts, `0x` and mixed case included
/// (`felis_protocol::session_prefix`).
const SESSION_PREFIX_PATTERN: &str = "^(0[xX])?[0-9a-fA-F]{1,32}$";
/// What an `attachment` parameter accepts
/// (`cli_sessions::parse_attachment_id`), which is also how the roster
/// prints the id a caller pipes back in.
const ATTACHMENT_ID_PATTERN: &str = "^[0-9]+$";

/// The largest integer common JSON consumers hold exactly; the bridge
/// refuses a numeric `id` above it.
const MAX_SAFE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

struct RequestId;

impl JsonSchema for RequestId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RequestId".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> schemars::Schema {
        // `maxLength` counts code points, the bridge counts UTF-8
        // bytes; the code-point bound is implied by the byte one, so it
        // stays as the part a stock validator can enforce and
        // `x-max-utf8-bytes` states the bound that actually applies.
        json_schema!({
            "description": "At most 128 UTF-8 bytes when a string; `maxLength` is the \
                            code-point bound that byte limit implies, not the limit itself.",
            "oneOf": [
                {
                    "type": "string",
                    "maxLength": felis_protocol::messages::MAX_TAG_BYTES,
                    "x-max-utf8-bytes": felis_protocol::messages::MAX_TAG_BYTES,
                },
                { "type": "integer", "minimum": 0, "maximum": MAX_SAFE_JSON_INTEGER },
            ],
        })
    }
}

// Deserialize-shaped and closed, matching the binary's own refusal of
// an unknown parameter.

macro_rules! params {
    ($($(#[$meta:meta])* $name:ident { $($field:ident : $ty:ty),* $(,)? })*) => {
        $(
            #[derive(JsonSchema)]
            #[schemars(deny_unknown_fields)]
            $(#[$meta])*
            #[allow(dead_code, reason = "the fields exist to be rendered as a schema")]
            struct $name {
                $($field: $ty,)*
            }
        )*
    };
}

/// A session prefix parameter, as opposed to a published id.
#[derive(JsonSchema)]
#[schemars(transparent)]
#[allow(dead_code, reason = "the field exists to be rendered as a schema")]
struct Prefix(#[schemars(regex(pattern = SESSION_PREFIX_PATTERN))] String);

/// An attachment id as a request spells it: the roster's decimal
/// rendering, not a JSON number.
#[derive(JsonSchema)]
#[schemars(transparent)]
#[allow(dead_code, reason = "the field exists to be rendered as a schema")]
struct AttachmentId(#[schemars(regex(pattern = ATTACHMENT_ID_PATTERN))] String);

/// The `source` vocabulary, read off the same `ValueEnum` the
/// `--source` flag parses so the published enum cannot drift from what
/// `parse_region_source` accepts.
struct RegionSourceValue;

impl JsonSchema for RegionSourceValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RegionSource".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> schemars::Schema {
        json_schema!({ "type": "string", "enum": crate::cli_sessions::source_names() })
    }
}

params! {
    SessionsListParams { tags: Option<Vec<String>> }
    SessionsInfoParams { session: Prefix }
    SessionsKillParams { session: Prefix }
    SessionsEvictParams { session: Prefix }
    /// `spawn_dims`: a create names its whole grid or none of it.
    // Not `dependentRequired`, which keys on a property being present:
    // the bridge reads an explicit `null` as absence, so
    // `{"rows": null}` alone is a request it serves and
    // `{"rows": null, "cols": 80}` one it refuses.
    #[schemars(extend("oneOf" = json!([
        { "properties": { "rows": { "type": "null" }, "cols": { "type": "null" } } },
        {
            "properties": { "rows": { "type": "integer" }, "cols": { "type": "integer" } },
            "required": ["rows", "cols"],
        },
    ])))]
    SessionsSpawnParams {
        cwd: Option<String>,
        env: Option<Vec<[String; 2]>>,
        rows: Option<u32>,
        cols: Option<u32>,
        tags: Option<Vec<String>>,
        cmd: Option<Vec<String>>,
    }
    SessionsSwitchParams {
        to: Prefix,
        from: Prefix,
        attachment: Option<AttachmentId>,
    }
    SessionsTagParams {
        session: Prefix,
        add: Option<Vec<String>>,
        remove: Option<Vec<String>>,
    }
    SessionsSendParams { session: Prefix, text: String, raw: Option<bool> }
    SessionsCaptureParams {
        session: Prefix,
        source: Option<RegionSourceValue>,
        ansi: Option<bool>,
        lines: Option<u32>,
    }
    SessionsSearchParams {
        session: Prefix,
        pattern: String,
        regex: Option<bool>,
        case_insensitive: Option<bool>,
    }
    NotificationsSubscribeParams { session: Option<Prefix> }
    CancelParams { target: RequestId }
}

fn op_has_required_params(defs: &Map<String, Value>, params: &Value) -> bool {
    let name = params["$ref"].as_str().unwrap().rsplit('/').next().unwrap();
    defs[name]
        .get("required")
        .is_some_and(|required| !required.as_array().unwrap().is_empty())
}

fn op_params(generator: &mut SchemaGenerator) -> Vec<(&'static str, Value)> {
    Operation::ALL
        .into_iter()
        .map(|op| (op.name(), params_schema(generator, op)))
        .collect()
}

fn params_schema(generator: &mut SchemaGenerator, op: Operation) -> Value {
    match op {
        Operation::Daemon(DaemonOp::List) => generator.subschema_for::<SessionsListParams>(),
        Operation::Daemon(DaemonOp::Info) => generator.subschema_for::<SessionsInfoParams>(),
        Operation::Daemon(DaemonOp::Spawn) => generator.subschema_for::<SessionsSpawnParams>(),
        Operation::Daemon(DaemonOp::Send) => generator.subschema_for::<SessionsSendParams>(),
        Operation::Daemon(DaemonOp::Kill) => generator.subschema_for::<SessionsKillParams>(),
        Operation::Daemon(DaemonOp::Evict) => generator.subschema_for::<SessionsEvictParams>(),
        Operation::Daemon(DaemonOp::Switch) => generator.subschema_for::<SessionsSwitchParams>(),
        Operation::Daemon(DaemonOp::Tag) => generator.subschema_for::<SessionsTagParams>(),
        Operation::Daemon(DaemonOp::Capture) => generator.subschema_for::<SessionsCaptureParams>(),
        Operation::Daemon(DaemonOp::Search) => generator.subschema_for::<SessionsSearchParams>(),
        Operation::Daemon(DaemonOp::Subscribe) => {
            generator.subschema_for::<NotificationsSubscribeParams>()
        }
        Operation::Cancel => generator.subschema_for::<CancelParams>(),
    }
    .to_value()
}

fn version_property() -> Value {
    json!({
        "type": "object",
        "properties": { "v": { "type": "integer", "const": SURFACE_VERSION } },
        "required": ["v"],
    })
}

/// `v` plus one body, the body left as its own `$ref` so a consumer can
/// point at the payload type alone.
fn versioned(body: &Value) -> Value {
    json!({ "allOf": [version_property(), body] })
}

fn any_of(branches: &[Value]) -> Value {
    json!({ "anyOf": branches })
}

/// A stream's in-band and terminal objects, whose `event` key is the
/// discriminator a consumer switches on.
fn event_object(event: &str, properties: &Value, required: &[&str]) -> Value {
    let mut props = properties.as_object().cloned().unwrap_or_default();
    props.insert("event".to_owned(), json!({ "const": event }));
    let mut required: Vec<Value> = required.iter().copied().map(Value::from).collect();
    required.push(Value::from("event"));
    json!({ "type": "object", "properties": props, "required": required })
}

fn document(
    id: &str,
    title: &str,
    description: &str,
    body: &Value,
    defs: Map<String, Value>,
) -> Value {
    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        Value::from("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert("$id".to_owned(), Value::from(format!("{RAW_BASE}/{id}")));
    root.insert("title".to_owned(), Value::from(title));
    root.insert("description".to_owned(), Value::from(description));
    for (key, value) in body.as_object().cloned().unwrap_or_default() {
        root.insert(key, value);
    }
    root.insert("$defs".to_owned(), Value::Object(defs));
    let mut document = Value::Object(root);
    bound_integers(&mut document);
    document
}

/// The exact range a Rust integer width admits, as a validator can
/// enforce it: `format` is an annotation draft 2020-12 leaves
/// non-normative, and schemars states an explicit bound only up to 16
/// bits, so without this a `u32` field accepts 2^32.
fn integer_bounds(format: &str) -> Option<(Value, Value)> {
    Some(match format {
        "int8" => (json!(i8::MIN), json!(i8::MAX)),
        "int16" => (json!(i16::MIN), json!(i16::MAX)),
        "int32" => (json!(i32::MIN), json!(i32::MAX)),
        "int64" => (json!(i64::MIN), json!(i64::MAX)),
        "uint8" => (json!(0), json!(u8::MAX)),
        "uint16" => (json!(0), json!(u16::MAX)),
        "uint32" => (json!(0), json!(u32::MAX)),
        "uint64" => (json!(0), json!(u64::MAX)),
        _ => return None,
    })
}

fn bound_integers(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let format = map.get("format").and_then(Value::as_str).map(str::to_owned);
            if let Some((minimum, maximum)) = format.as_deref().and_then(integer_bounds) {
                map.entry("minimum").or_insert(minimum);
                map.entry("maximum").or_insert(maximum);
                // A validator comparing JSON numbers as `f64` cannot
                // separate a 64-bit bound from the first few thousand
                // values past it, so what it enforces is the bound
                // rounded to `f64`.
                if matches!(format.as_deref(), Some("int64" | "uint64")) {
                    map.entry("x-bound-exceeds-f64-precision")
                        .or_insert(json!(true));
                }
            }
            for child in map.values_mut() {
                bound_integers(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(bound_integers),
        _ => {}
    }
}

fn output_generator() -> SchemaGenerator {
    SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
}

/// The CLI's own channels: one document per object class, so a
/// consumer can name the class it expects instead of validating against
/// the union.
fn cli_document() -> Value {
    let mut generator = output_generator();
    let point_bodies = vec![
        generator.subschema_for::<ListResult>().to_value(),
        generator.subschema_for::<SessionObject>().to_value(),
        generator.subschema_for::<SessionRef>().to_value(),
        generator.subschema_for::<TagResult>().to_value(),
        generator.subschema_for::<SwitchResult>().to_value(),
        generator
            .subschema_for::<crate::cli_output::RetargetResult>()
            .to_value(),
        generator.subschema_for::<DaemonStatusResult>().to_value(),
        generator.subschema_for::<DaemonStopResult>().to_value(),
        generator.subschema_for::<ConfigPathResult>().to_value(),
        generator.subschema_for::<CheckResult>().to_value(),
        generator
            .subschema_for::<EffectiveConfigResult>()
            .to_value(),
        generator.subschema_for::<DoctorResult>().to_value(),
        generator.subschema_for::<VersionResult<'_>>().to_value(),
    ];
    let item_bodies = vec![
        generator.subschema_for::<CaptureRow<'_>>().to_value(),
        generator.subschema_for::<SearchMatch<'_>>().to_value(),
        generator
            .subschema_for::<crate::cli_output::NotificationObject>()
            .to_value(),
    ];
    let error = generator.subschema_for::<MachineError>().to_value();
    let defs = generator.take_definitions(true);

    let error_body = json!({
        "type": "object",
        "properties": { "error": error },
        "required": ["error"],
    });
    let mut classes = Map::new();
    // The bodies are published separately from the classes that wrap
    // them: `felis bridge` carries the same payloads under its own
    // correlation envelope, so `v` cannot be baked into them.
    classes.insert("point_body".to_owned(), any_of(&point_bodies));
    classes.insert("stream_item_body".to_owned(), any_of(&item_bodies));
    classes.insert("error_body".to_owned(), error_body);
    classes.insert(
        "point_result".to_owned(),
        versioned(&json!({ "$ref": "#/$defs/point_body" })),
    );
    classes.insert(
        "point_error".to_owned(),
        versioned(&json!({ "$ref": "#/$defs/error_body" })),
    );
    classes.insert(
        "stream_item".to_owned(),
        versioned(&json!({ "$ref": "#/$defs/stream_item_body" })),
    );
    classes.insert(
        "lag_event".to_owned(),
        versioned(&event_object(
            "lag",
            &json!({ "dropped": { "type": "integer", "format": "uint64" } }),
            &["dropped"],
        )),
    );
    classes.insert(
        "end_terminal".to_owned(),
        versioned(&event_object(
            "end",
            &json!({
                "count": { "type": "integer", "format": "uint64" },
                "exit_code": { "type": "integer", "format": "uint32" },
            }),
            &["count"],
        )),
    );
    classes.insert(
        "error_terminal".to_owned(),
        versioned(&json!({
            "allOf": [
                { "$ref": "#/$defs/error_body" },
                event_object("error", &json!({}), &[]),
            ],
        })),
    );

    let mut defs: Map<String, Value> = defs;
    let branches: Vec<Value> = [
        "point_result",
        "point_error",
        "stream_item",
        "lag_event",
        "end_terminal",
        "error_terminal",
    ]
    .iter()
    .map(|name| json!({ "$ref": format!("#/$defs/{name}") }))
    .collect();
    defs.extend(classes);
    document(
        "felis-cli-v1.schema.json",
        "felis CLI machine output (v1)",
        "Every object `--format json` or `--format jsonl` may write. \
         The object classes are the `$defs` named `point_result`, `point_error`, \
         `stream_item`, `lag_event`, `end_terminal` and `error_terminal`; \
         payload objects stay open, because a field may be added within an epoch.",
        &any_of(&branches),
        defs,
    )
}

fn bridge_document() -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    let ops = op_params(&mut generator);
    let request_id = generator.subschema_for::<RequestId>().to_value();
    let mut defs = generator.take_definitions(true);

    let by_op: Vec<Value> = ops
        .into_iter()
        .map(|(op, params)| {
            let required = op_has_required_params(&defs, &params);
            let mut branch = json!({
                "type": "object",
                "properties": { "op": { "const": op }, "params": params },
            });
            if required {
                branch["required"] = json!(["params"]);
            }
            branch
        })
        .collect();
    let request = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "v": { "type": "integer", "const": SURFACE_VERSION },
            "id": request_id,
            "op": { "type": "string" },
            "params": { "type": "object" },
        },
        "required": ["v", "id", "op"],
        "oneOf": by_op,
    });

    let cli = format!("{RAW_BASE}/felis-cli-v1.schema.json");
    let correlated = |id: &Value| {
        json!({
            "type": "object",
            "properties": { "id": id },
            "required": ["id"],
        })
    };
    // Only an untagged `error` may answer without an id: a failure the
    // bridge cannot attribute to a line (a cold socket, an unreadable
    // id) has none to echo. Every other shape follows a parsed request.
    let echoed = correlated(&request_id);
    let unattributable = correlated(&json!({ "oneOf": [request_id, { "type": "null" }] }));
    let reply = |body: &Value| json!({ "allOf": [version_property(), echoed.clone(), body] });
    let body_ref = |class: &str| json!({ "$ref": format!("{cli}#/$defs/{class}") });

    defs.insert("request".to_owned(), request);
    defs.insert(
        "reply".to_owned(),
        reply(&json!({
            "type": "object",
            "properties": {
                "result": {
                    "anyOf": [
                        body_ref("point_body"),
                        // `cancel` is the bridge's own verb: no one-shot
                        // CLI invocation can produce this body.
                        { "type": "object", "properties": { "canceled": { "type": "boolean" } },
                          "required": ["canceled"] },
                    ],
                },
            },
            "required": ["result"],
        })),
    );
    defs.insert(
        "item".to_owned(),
        reply(&json!({
            "type": "object",
            "properties": { "item": body_ref("stream_item_body") },
            "required": ["item"],
        })),
    );
    defs.insert(
        "error".to_owned(),
        json!({ "allOf": [version_property(), unattributable, body_ref("error_body")] }),
    );
    defs.insert(
        "lag_event".to_owned(),
        reply(&event_object(
            "lag",
            &json!({ "dropped": { "type": "integer", "format": "uint64" } }),
            &["dropped"],
        )),
    );
    defs.insert(
        "end_terminal".to_owned(),
        reply(&event_object(
            "end",
            &json!({
                "count": { "type": "integer", "format": "uint64" },
                "exit_code": { "type": "integer", "format": "uint32" },
            }),
            &["count"],
        )),
    );
    defs.insert(
        "error_terminal".to_owned(),
        reply(&json!({
            "allOf": [body_ref("error_body"), event_object("error", &json!({}), &[])],
        })),
    );

    let responses: Vec<Value> = [
        "reply",
        "item",
        "error",
        "lag_event",
        "end_terminal",
        "error_terminal",
    ]
    .iter()
    .map(|name| json!({ "$ref": format!("#/$defs/{name}") }))
    .collect();
    document(
        "felis-bridge-v1.schema.json",
        "felis bridge stdio protocol (v1)",
        "`$defs/request` validates one stdin line; the remaining `$defs` are the \
         six shapes stdout may carry, each echoing the request `id`. \
         A request is closed: an unknown top-level key or operation parameter is \
         refused as `malformed_request`.",
        &json!({ "anyOf": responses }),
        defs,
    )
}

fn rendered(document: &Value) -> String {
    let mut text = serde_json::to_string_pretty(document).unwrap();
    text.push('\n');
    text
}

fn sync(path: &str, document: &Value) {
    let want = rendered(document);
    if std::env::var_os("UPDATE_SCHEMA").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{path} is missing ({err}); run `just schema`"));
    assert_eq!(have, want, "{path} is stale; regenerate with `just schema`");
}

#[test]
fn the_cli_schema_is_up_to_date() {
    sync(CLI_PATH, &cli_document());
}

#[test]
fn the_bridge_schema_is_up_to_date() {
    sync(BRIDGE_PATH, &bridge_document());
}

/// The published request grammar and the keys the bridge admits
/// parameters against are one contract; nothing else forces the typed
/// params to track it.
#[test]
fn the_request_schema_admits_exactly_the_parameters_the_bridge_does() {
    let mut generator = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    let schemas: Vec<(Operation, Value)> = Operation::ALL
        .into_iter()
        .map(|op| (op, params_schema(&mut generator, op)))
        .collect();
    let defs = generator.take_definitions(true);
    for (op, params) in schemas {
        let name = params["$ref"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned();
        let mut published: Vec<&str> = defs[&name]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        published.sort_unstable();
        let mut admitted = op.params().to_vec();
        admitted.sort_unstable();
        assert_eq!(published, admitted, "`{}`", op.name());
    }
}

/// The bridge frames an operation as a stream or a point on its own
/// classification, so each operation is checked against the class its
/// CLI verb declares.
#[test]
fn the_bridge_streams_exactly_the_operations_whose_verb_is_a_stream() {
    let root = <crate::Cli as clap::CommandFactory>::command();
    for op in Operation::ALL {
        if op == Operation::Cancel {
            continue;
        }
        let op_name = op.name();
        let verb = op_name.split('.').fold(&root, |cmd, name| {
            cmd.find_subcommand(name)
                .unwrap_or_else(|| panic!("`{op_name}` has no CLI verb"))
        });
        let format = verb
            .get_arguments()
            .find(|arg| arg.get_id() == "format")
            .unwrap_or_else(|| panic!("`{op_name}`'s verb has no --format"));
        let takes_jsonl = format
            .get_possible_values()
            .iter()
            .any(|value| value.get_name() == "jsonl");
        assert_eq!(op.is_streaming(), takes_jsonl, "`{op_name}`");
    }
}

/// A published id is the full 32-hex rendering; the pattern is the only
/// place the schema states it, so a consumer's validator rejects a
/// truncated one.
#[test]
fn published_session_ids_carry_the_full_length_pattern() {
    let document = cli_document();
    assert_eq!(
        document["$defs"]["SessionObject"]["properties"]["id"]["pattern"],
        json!(SESSION_ID_PATTERN),
    );
    assert_eq!(
        document["$defs"]["AttachmentObject"]["properties"]["id"]["pattern"],
        json!(ATTACHMENT_ID_PATTERN),
    );
}

/// The `source` enum a consumer validates against is the set the
/// bridge parses, neither wider nor narrower.
#[test]
fn the_published_source_enum_is_the_set_the_bridge_parses() {
    let document = bridge_document();
    let published = document["$defs"]["RegionSource"]["enum"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(published.len(), crate::cli_sessions::source_names().len());
    for value in &published {
        let value = value.as_str().unwrap();
        assert!(
            crate::cli_sessions::parse_region_source(value).is_ok(),
            "the schema publishes `{value}` but the bridge refuses it",
        );
    }
    assert!(crate::cli_sessions::parse_region_source("bogus").is_err());
}

/// `kind` must stay an open string: the vocabulary grows additively
/// within an epoch, and an `enum` would make a consumer reject a newer
/// token.
#[test]
fn the_error_kind_is_an_open_string_with_this_epoch_s_tokens_listed() {
    let document = cli_document();
    let kind = &document["$defs"]["ErrorKind"];
    assert_eq!(kind["type"], json!("string"));
    assert!(kind.get("enum").is_none(), "{kind}");
    assert_eq!(
        kind["x-known-values"].as_array().unwrap().len(),
        crate::cli_output::ErrorKind::ALL.len(),
    );
}
