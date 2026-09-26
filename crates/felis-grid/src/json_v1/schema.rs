//! The published JSON Schema for the v1 DTOs, rendered from the same
//! types the conversions use, so the document cannot drift from what a
//! writer emits.

use schemars::{SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};

use super::{
    CONN, ConnJson, FORMAT_VERSION, GRID, GridJson, IMAGE, INPUT, ImageJson, InputJson, SESSION,
    SessionJson, VERSION_KEY,
};

const RAW_BASE: &str =
    "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-grid/schemas";

/// One envelope branch: the version, the kind token, and the DTO the
/// token admits. Nothing is closed, because v1 readers must accept a
/// field a later v1 writer adds (`docs/reference/ipc.md`).
fn branch(kind: &str, msg: Value) -> Value {
    let mut properties = Map::new();
    properties.insert(VERSION_KEY.to_owned(), json!({ "const": FORMAT_VERSION }));
    properties.insert("kind".to_owned(), json!({ "const": kind }));
    properties.insert("msg".to_owned(), msg);
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": [VERSION_KEY, "kind", "msg"],
    })
}

/// The whole published document.
#[must_use]
pub fn document() -> Value {
    let mut generator: SchemaGenerator = SchemaSettings::draft2020_12().into_generator();
    let branches = vec![
        branch(GRID, generator.subschema_for::<GridJson>().to_value()),
        branch(IMAGE, generator.subschema_for::<ImageJson>().to_value()),
        branch(CONN, generator.subschema_for::<ConnJson>().to_value()),
        branch(SESSION, generator.subschema_for::<SessionJson>().to_value()),
        branch(INPUT, generator.subschema_for::<InputJson>().to_value()),
    ];
    let defs: Map<String, Value> = generator.take_definitions(true);

    let mut root = Map::new();
    root.insert(
        "$schema".to_owned(),
        Value::from("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert(
        "$id".to_owned(),
        Value::from(format!("{RAW_BASE}/felis-json-v1.schema.json")),
    );
    root.insert("title".to_owned(), Value::from("felis-json v1"));
    root.insert(
        "description".to_owned(),
        Value::from(
            "One daemon frame body in its felis-json v1 form. \
             A reader selects its branch on `kind` after checking `felis_json`, \
             and ignores any property this document does not name: within the \
             epoch a writer may add an optional field.",
        ),
    );
    root.insert("oneOf".to_owned(), Value::Array(branches));
    root.insert("$defs".to_owned(), Value::Object(defs));
    let mut document = Value::Object(root);
    bound_integers(&mut document);
    constrain_sizing(&mut document);
    document
}

/// The exact range a Rust integer width admits, as a validator can
/// enforce it: `format` is an annotation draft 2020-12 leaves
/// non-normative, so without this a `u32` field accepts 2^32.
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

/// `Sizing::new` admits a fraction only when `frac_num < frac_den`, and
/// JSON Schema cannot compare two properties: one `if`/`then` per
/// denominator says the same thing in what a validator can read.
/// `frac_den = 0` carries no fraction and leaves `frac_num` free.
fn constrain_sizing(document: &mut Value) {
    let branches: Vec<Value> = (1..=15u8)
        .map(|den| {
            json!({
                "if": {
                    "properties": { "frac_den": { "const": den } },
                    "required": ["frac_den"],
                },
                "then": {
                    "properties": { "frac_num": { "maximum": den - 1 } },
                },
            })
        })
        .collect();
    document["$defs"]["SizingJson"]["allOf"] = Value::Array(branches);
}

#[cfg(test)]
mod tests {
    use super::document;

    const PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/schemas/felis-json-v1.schema.json"
    );

    /// Regenerated by `just schema`, never edited by hand.
    #[test]
    fn the_felis_json_schema_is_up_to_date() {
        let mut want = serde_json::to_string_pretty(&document()).expect("the document renders");
        want.push('\n');
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            let path = std::path::Path::new(PATH);
            std::fs::create_dir_all(path.parent().expect("the schema has a directory"))
                .expect("the schema directory is writable");
            std::fs::write(path, &want).expect("the schema is writable");
            return;
        }
        let have = std::fs::read_to_string(PATH)
            .unwrap_or_else(|err| panic!("{PATH} is missing ({err}); run `just schema`"));
        assert_eq!(have, want, "{PATH} is stale; regenerate with `just schema`");
    }

    /// A v1 reader must tolerate what a later v1 writer adds, so no
    /// object in the published document may be closed.
    #[test]
    fn no_published_object_forbids_additional_properties() {
        fn walk(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    assert_ne!(
                        map.get("additionalProperties"),
                        Some(&serde_json::Value::Bool(false)),
                        "{path} is closed",
                    );
                    for (key, child) in map {
                        walk(child, &format!("{path}/{key}"));
                    }
                }
                serde_json::Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        walk(child, &format!("{path}/{index}"));
                    }
                }
                _ => {}
            }
        }
        walk(&document(), "");
    }
}
