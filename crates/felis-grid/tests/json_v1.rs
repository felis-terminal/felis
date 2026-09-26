//! Golden `felis-json` v1 frames: what a v1 writer emits, byte for
//! byte, and what a v1 reader must still accept. Regenerate with
//! `just golden`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "json")]

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use std::sync::OnceLock;

use boon::{Compiler, SchemaIndex, Schemas};
use felis_grid::json_v1::{self, JsonError};
use felis_grid::wire::{self, MAX_CELLS_PER_ROW, RowEncode};
use felis_grid::{Cell, Grapheme, StyleTable};
use felis_protocol::build_identity::BuildIdentity;
use felis_protocol::kitty_graphics::{ImageId, PlacementId};
use felis_protocol::messages::{
    AttachFailure, AttachRefusal, Attachment, ClipboardSelection, ClipboardWrite, ConnToClientMsg,
    CreateFailure, GridDims, GridMsg, ImageMsg, InputMsg, Key, KeyEvent, KeyEventKind, KeyLocation,
    KeyMods, NamedKey, SessionInfo, SessionToClientMsg, SourceRect,
};
use felis_protocol::{MessageKind, RowPayload, codec};
use serde_json::{Value, json};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/json-v1")
        .join(format!("{name}.json"))
}

fn packed_row(text: &str) -> RowPayload {
    let cells: Vec<Cell> = text
        .bytes()
        .map(|byte| Cell {
            grapheme: Grapheme::Ascii(byte),
            ..Cell::default()
        })
        .collect();
    RowPayload(
        wire::encode_row(
            RowEncode {
                cells: &cells,
                pad_to: cells.len(),
                sized_cells: &[],
                soft_wrap_continued: false,
            },
            &StyleTable::new(),
        )
        .expect("an ASCII row encodes"),
    )
}

fn session_info() -> SessionInfo {
    SessionInfo {
        id: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
        dims: GridDims {
            rows: 24,
            cols: 80,
            pixel_w: 0,
            pixel_h: 0,
        },
        title: Some("zsh".to_owned()),
        cwd: Some("file://localhost/tmp".to_owned()),
        idle_seconds: Some(12),
        tags: vec!["work".to_owned()],
        last_notification: None,
        foreground: Some("claude".to_owned()),
        exited: false,
        last_exit_code: Some(0),
        attachments: vec![Attachment {
            id: 7,
            attached_at: UNIX_EPOCH + Duration::new(1_700_000_000, 500),
            input_owner: true,
        }],
        sequence: NonZeroU64::new(3).unwrap(),
    }
}

/// One representative frame per family, plus the `RowDelta` whose rows
/// are the reason this format exists.
fn cases() -> Vec<(&'static str, MessageKind, Vec<u8>)> {
    vec![
        (
            "grid-row-delta",
            MessageKind::Grid,
            codec::encode(&GridMsg::RowDelta {
                rows: vec![(0, packed_row("hi")), (3, packed_row("felis"))],
            }),
        ),
        (
            "grid-clipboard-set",
            MessageKind::Grid,
            codec::encode(&GridMsg::ClipboardSet {
                write: ClipboardWrite {
                    selection: ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY,
                    data: b"copied".to_vec(),
                },
            }),
        ),
        (
            "image-placement",
            MessageKind::Image,
            codec::encode(&ImageMsg::Placement {
                image_id: ImageId(9),
                placement_id: Some(PlacementId(2)),
                anchor_row: -1,
                anchor_col: 4,
                cols: 10,
                rows: 5,
                source: Some(SourceRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 32,
                }),
                z_index: -3,
            }),
        ),
        (
            "conn-welcome",
            MessageKind::Conn,
            codec::encode(&ConnToClientMsg::Welcome {
                identity: Some(BuildIdentity::from_build_env(
                    "0.1.0",
                    "e3abf80e3abf80e3abf80e3abf80e3abf80e3abf",
                )),
            }),
        ),
        (
            "session-attached",
            MessageKind::Session,
            codec::encode(&SessionToClientMsg::Attached {
                info: session_info(),
            }),
        ),
        (
            "session-attach-failed-attach-half",
            MessageKind::Session,
            codec::encode(&SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Attach(AttachFailure::Ambiguous),
                detail: "`ca` matches 3 sessions".to_owned(),
            }),
        ),
        (
            "session-attach-failed-create-half",
            MessageKind::Session,
            codec::encode(&SessionToClientMsg::AttachFailed {
                reason: AttachRefusal::Create(CreateFailure::DaemonDraining),
                detail: "the daemon is draining toward exit".to_owned(),
            }),
        ),
        (
            "input-key",
            MessageKind::Input,
            codec::encode(&InputMsg::Key(KeyEvent {
                key: Key::Named(NamedKey::Enter),
                text: Some("\r".to_owned()),
                mods: KeyMods::CTRL | KeyMods::SHIFT,
                kind: KeyEventKind::Press,
                location: KeyLocation::Standard,
            })),
        ),
    ]
}

fn rendered(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a frame renders");
    text.push('\n');
    text
}

fn golden(name: &str, value: &Value) -> Value {
    let path = golden_path(name);
    let want = rendered(value);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("the golden has a directory"))
            .expect("the golden directory is writable");
        std::fs::write(&path, &want).expect("the golden is writable");
        return value.clone();
    }
    let have = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} is missing ({err}); run `just golden`", path.display()));
    assert_eq!(
        have,
        want,
        "{} is stale; regenerate with `just golden`",
        path.display()
    );
    serde_json::from_str(&have).expect("a golden is JSON")
}

#[test]
fn every_golden_frame_matches_what_a_v1_writer_emits() {
    for (name, kind, body) in cases() {
        let value = json_v1::encode(kind, &body).unwrap_or_else(|err| panic!("{name}: {err}"));
        golden(name, &value);
    }
}

/// The wire body a golden decodes to is the body it was written from,
/// which is what makes a recording replayable.
#[test]
fn every_golden_frame_survives_json_wire_json() {
    for (name, kind, body) in cases() {
        let value = json_v1::encode(kind, &body).unwrap_or_else(|err| panic!("{name}: {err}"));
        let (back_kind, back_body) =
            json_v1::decode(&value).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(back_kind, kind, "{name}");
        assert_eq!(back_body, body, "{name}");
        assert_eq!(
            json_v1::encode(back_kind, &back_body).expect("the frame re-encodes"),
            value,
            "{name}"
        );
    }
}

/// A `RowDelta` row reads as the row codec's structural form, never as
/// the packed bytes the socket carries (#193).
#[test]
fn a_golden_row_delta_carries_structural_cells() {
    let (name, kind, body) = cases().remove(0);
    let value = golden(
        name,
        &json_v1::encode(kind, &body).expect("the frame encodes"),
    );
    let cells = &value["msg"]["rows"][0]["cells"];
    assert_eq!(cells["encoding"], json!("rle"), "{cells}");
    let graphemes = cells["graphemes"]
        .as_array()
        .expect("a row carries its graphemes");
    assert!(
        graphemes
            .iter()
            .all(|g| g["type"] == json!("ascii") && g["byte"].is_number()),
        "graphemes must be structural, got: {graphemes:?}"
    );
    assert!(
        cells["attr_runs"][0]["attrs"]["fg"]["type"] == json!("default"),
        "a pen must be structural, got: {cells}"
    );
}

/// Within the epoch a writer may add an optional field, so a reader
/// that predates it must still decode the frame.
#[test]
fn a_v1_frame_carrying_an_unknown_optional_field_still_decodes() {
    let (_, kind, body) = cases().remove(0);
    let mut value = json_v1::encode(kind, &body).expect("the frame encodes");
    value["msg"]["invented_later"] = json!({ "any": "shape" });
    value["msg"]["rows"][0]["invented_later"] = json!(true);
    value["invented_later"] = json!(1);
    let (back_kind, back_body) = json_v1::decode(&value).expect("the frame still decodes");
    assert_eq!((back_kind, back_body), (kind, body));
}

/// A new kind, variant or required field is `felis_json: 2`, and a v1
/// reader refuses it on the version alone.
#[test]
fn a_later_format_version_is_refused_at_the_version_check() {
    let (_, kind, body) = cases().remove(0);
    let mut value = json_v1::encode(kind, &body).expect("the frame encodes");
    value["felis_json"] = json!(2);
    assert!(matches!(
        json_v1::decode(&value),
        Err(JsonError::Version { .. })
    ));
}

/// A kind outside v1 is this format's boundary, not a decode failure
/// deep inside a DTO.
#[test]
fn a_kind_outside_v1_is_refused() {
    let (_, kind, body) = cases().remove(0);
    let mut value = json_v1::encode(kind, &body).expect("the frame encodes");
    value["kind"] = json!("region");
    assert!(matches!(json_v1::decode(&value), Err(JsonError::Kind(_))));
}

const SCHEMA_URL: &str = "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-grid/schemas/felis-json-v1.schema.json";

/// The committed document, compiled once and offline: nothing here
/// reaches the network.
fn schema() -> &'static (Schemas, SchemaIndex) {
    static COMPILED: OnceLock<(Schemas, SchemaIndex)> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schemas/felis-json-v1.schema.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("{} is missing ({err}); run `just schema`", path.display())
        });
        let mut compiler = Compiler::new();
        compiler
            .add_resource(
                SCHEMA_URL,
                serde_json::from_str(&text).expect("the schema is JSON"),
            )
            .expect("the schema is a resource");
        let mut schemas = Schemas::new();
        let index = compiler
            .compile(SCHEMA_URL, &mut schemas)
            .expect("the schema compiles");
        (schemas, index)
    })
}

fn validates(frame: &Value) -> bool {
    let (schemas, index) = schema();
    schemas.validate(frame, *index).is_ok()
}

/// The published document is what a consumer validates against, so the
/// frames a writer really emits are its positive fixtures.
#[test]
fn every_golden_frame_validates_against_the_published_schema() {
    for (name, kind, body) in cases() {
        let value = json_v1::encode(kind, &body).unwrap_or_else(|err| panic!("{name}: {err}"));
        let (schemas, index) = schema();
        if let Err(err) = schemas.validate(&value, *index) {
            panic!("{name} does not validate against the published schema:\n{err}");
        }
    }
}

/// A validator must refuse what the reader refuses, or a consumer that
/// checks the schema first still meets a decode failure.
#[test]
fn what_the_reader_refuses_the_schema_refuses() {
    let (_, kind, body) = cases().remove(0);
    let mut control_byte = json_v1::encode(kind, &body).expect("the frame encodes");
    control_byte["msg"]["rows"][0]["cells"]["graphemes"][0] =
        json!({ "type": "ascii", "byte": 10 });

    let (_, kind, body) = cases().remove(4);
    let mut uppercase_id = json_v1::encode(kind, &body).expect("the frame encodes");
    uppercase_id["msg"]["info"]["id"] = json!("0123456789ABCDEF0123456789ABCDEF");

    let past_the_last_f_key = json!({
        "felis_json": 1,
        "kind": "input",
        "msg": {
            "type": "key",
            "event": {
                "key": { "key": "named", "named": { "name": "f", "index": 36 } },
                "text": null,
                "mods": 0,
                "kind": "press",
                "location": "standard",
            },
        },
    });

    let image_id_past_u32 = json!({
        "felis_json": 1,
        "kind": "image",
        "msg": { "type": "complete", "id": u64::from(u32::MAX) + 1 },
    });

    let whole_fraction = row_delta_with(&row_cells(&[grapheme(b'x')], &sized_cell(1, 1)));

    let mut too_many_graphemes = row_delta_with(&row_cells(
        &vec![grapheme(b'x'); MAX_CELLS_PER_ROW + 1],
        &json!([]),
    ));
    too_many_graphemes["msg"]["rows"][0]["cells"]["attr_runs"][0]["len"] =
        json!(MAX_CELLS_PER_ROW + 1);

    for (name, frame) in [
        ("a control byte in an ascii grapheme", control_byte),
        ("an uppercase session id", uppercase_id),
        ("a function key past F35", past_the_last_f_key),
        ("an image id past u32", image_id_past_u32),
        ("a fraction that is not below one", whole_fraction),
        ("a row wider than the codec admits", too_many_graphemes),
    ] {
        assert!(!validates(&frame), "{name} validated but must not: {frame}");
        assert!(
            json_v1::decode(&frame).is_err(),
            "{name} decoded but must not: {frame}",
        );
    }
}

/// The bound itself is admitted, so the refusals above are attributable
/// to the value and not to a schema that rejects the whole class.
#[test]
fn every_value_at_a_published_bound_is_admitted() {
    let at_the_bounds = [
        (
            "an image id at u32::MAX",
            json!({
                "felis_json": 1,
                "kind": "image",
                "msg": { "type": "complete", "id": u32::MAX },
            }),
        ),
        (
            "the widest fraction under its denominator",
            row_delta_with(&row_cells(&[grapheme(b'x')], &sized_cell(14, 15))),
        ),
        (
            "a fraction-free sizing",
            row_delta_with(&row_cells(&[grapheme(b'x')], &sized_cell(15, 0))),
        ),
        (
            "an ascii grapheme at each end of the printable window",
            row_delta_with(&row_cells(&[grapheme(0x20), grapheme(0x7E)], &json!([]))),
        ),
        (
            "a function key at F35",
            json!({
                "felis_json": 1,
                "kind": "input",
                "msg": {
                    "type": "key",
                    "event": {
                        "key": { "key": "named", "named": { "name": "f", "index": 35 } },
                        "text": null,
                        "mods": 0,
                        "kind": "press",
                        "location": "standard",
                    },
                },
            }),
        ),
    ];
    for (name, frame) in at_the_bounds {
        assert!(validates(&frame), "{name} must validate: {frame}");
        assert!(
            json_v1::decode(&frame).is_ok(),
            "{name} must decode: {frame}",
        );
    }
}

fn grapheme(byte: u8) -> Value {
    json!({ "type": "ascii", "byte": byte })
}

fn sized_cell(frac_num: u8, frac_den: u8) -> Value {
    json!([{
        "col": 0,
        "sizing": {
            "scale": 1,
            "cell_width": 0,
            "frac_num": frac_num,
            "frac_den": frac_den,
            "valign": "top",
            "halign": "left",
        },
    }])
}

fn row_cells(graphemes: &[Value], sized_cells: &Value) -> Value {
    let len = graphemes.len();
    json!({
        "encoding": "rle",
        "graphemes": graphemes,
        "attr_runs": [{
            "len": len,
            "attrs": {
                "fg": { "type": "default" },
                "bg": { "type": "default" },
                "underline_color": { "type": "default" },
                "flags": 0,
                "underline_style": "single",
            },
            "link": null,
        }],
        "sized_cells": sized_cells,
        "soft_wrap_continued": false,
    })
}

fn row_delta_with(cells: &Value) -> Value {
    json!({
        "felis_json": 1,
        "kind": "grid",
        "msg": { "type": "row_delta", "rows": [{ "row": 0, "cells": cells }] },
    })
}
