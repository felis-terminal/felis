//! Included by the e2e suites through `#[path]`, because the objects
//! they already produce are the fixtures.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::OnceLock;

use boon::{Compiler, SchemaIndex, Schemas};
use serde_json::Value;

const RAW_BASE: &str =
    "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-cli/schemas";

fn schemas_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

fn read(name: &str) -> Value {
    let path = schemas_dir().join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is missing ({err}); run `just schema`", path.display()));
    serde_json::from_str(&text).expect("the committed schema is JSON")
}

fn cli_schema() -> &'static (Schemas, SchemaIndex) {
    static CLI: OnceLock<(Schemas, SchemaIndex)> = OnceLock::new();
    CLI.get_or_init(|| {
        let cli_url = format!("{RAW_BASE}/felis-cli-v1.schema.json");
        let mut compiler = Compiler::new();
        compiler
            .add_resource(&cli_url, read("felis-cli-v1.schema.json"))
            .unwrap();
        let mut schemas = Schemas::new();
        let cli = compiler.compile(&cli_url, &mut schemas).unwrap();
        (schemas, cli)
    })
}

/// Every object a `--format json` / `--format jsonl` verb writes.
pub(crate) fn assert_cli_object(object: &Value) {
    let (schemas, index) = cli_schema();
    if let Err(err) = schemas.validate(object, *index) {
        panic!("{object} does not validate against the published schema:\n{err}");
    }
}

pub(crate) fn cli_object_is_valid(object: &Value) -> bool {
    let (schemas, index) = cli_schema();
    schemas.validate(object, *index).is_ok()
}

/// A fixture names the class it claims to be: payload objects stay
/// open, so a malformed one can otherwise satisfy a laxer sibling of
/// the union the root validates against.
pub(crate) fn cli_def_rejects(def: &str, instance: &Value) -> bool {
    let cli_url = format!("{RAW_BASE}/felis-cli-v1.schema.json");
    let mut compiler = Compiler::new();
    compiler
        .add_resource(&cli_url, read("felis-cli-v1.schema.json"))
        .unwrap();
    let mut schemas = Schemas::new();
    let index = compiler
        .compile(&format!("{cli_url}#/$defs/{def}"), &mut schemas)
        .unwrap_or_else(|err| panic!("`{def}` is not a class of the CLI schema: {err}"));
    schemas.validate(instance, index).is_err()
}

pub(crate) fn invalid_output_fixtures() -> Vec<(String, Value)> {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema-invalid");
    let mut found: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("out-")
                && std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext == "json")
        })
        .map(|name| {
            let text = std::fs::read_to_string(dir.join(&name)).unwrap();
            (
                name,
                serde_json::from_str(&text).expect("a fixture is JSON"),
            )
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!found.is_empty(), "no `out-` fixtures in {}", dir.display());
    found
}
