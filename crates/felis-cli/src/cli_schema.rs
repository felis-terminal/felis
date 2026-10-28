//! The published JSON Schema document for the machine surface,
//! rendered from the serde types (docs/reference/cli.md "JSON
//! Schema"). Nothing here spells a field name; only the envelope,
//! which serde has no type for, is composed by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use schemars::{SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};

use crate::cli_output::{
    CaptureRow, CheckResult, ConfigPathResult, DaemonStatusResult, DaemonStopResult,
    DaemonUpgradeResult, DoctorReportResult, DoctorResult, EffectiveConfigResult, ListResult,
    MachineError, SURFACE_VERSION, SearchMatch, SessionObject, SessionRef, SwitchResult, TagResult,
};
use crate::cli_version::VersionResult;

const CLI_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schemas/felis-cli-v1.schema.json"
);

const RAW_BASE: &str =
    "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas";

/// A session id as the surface publishes it, never as a request may
/// spell it: output is always the full lowercase rendering.
const SESSION_ID_PATTERN: &str = "^[0-9a-f]{32}$";
/// An attachment id as the roster prints it, which is also what
/// `--attachment` accepts back (`cli_sessions::parse_attachment_id`).
const ATTACHMENT_ID_PATTERN: &str = "^[0-9]+$";

fn version_property() -> Value {
    json!({
        "type": "object",
        "properties": { "v": { "type": "integer", "const": SURFACE_VERSION } },
        "required": ["v"],
    })
}

/// `v` plus one body.
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
        generator.subschema_for::<DaemonUpgradeResult>().to_value(),
        generator.subschema_for::<ConfigPathResult>().to_value(),
        generator.subschema_for::<CheckResult>().to_value(),
        generator
            .subschema_for::<EffectiveConfigResult>()
            .to_value(),
        generator.subschema_for::<DoctorResult>().to_value(),
        generator.subschema_for::<DoctorReportResult>().to_value(),
        generator.subschema_for::<VersionResult<'_>>().to_value(),
        generator
            .subschema_for::<crate::cli_output::FontFamiliesResult>()
            .to_value(),
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
    classes.insert("point_result".to_owned(), versioned(&any_of(&point_bodies)));
    classes.insert("point_error".to_owned(), versioned(&error_body));
    classes.insert("stream_item".to_owned(), versioned(&any_of(&item_bodies)));
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
            "allOf": [error_body, event_object("error", &json!({}), &[])],
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
