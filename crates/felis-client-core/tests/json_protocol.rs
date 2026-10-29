//! Golden JSON protocol messages: what a version 1 writer emits, byte
//! for byte, what a reader must still accept, and what the published
//! schema says of both. Regenerate with `just golden`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "json")]

use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::OnceLock;

use boon::{Compiler, SchemaIndex, Schemas};
use felis_client_core::json::{
    AttachTarget, Base64, CaptureRowJson, ClientHello, ClientMessage, ErrorKind, ErrorObject,
    FramingError, GridJson, ImageJson, InputJson, Item, KeyEventJson, Ledger, NotificationJson,
    OpResult, Open, RegionSourceJson, Request, SearchMatchJson, SearchSourceJson, SendStepJson,
    ServerHello, ServerMessage, SessionId, SessionObject, SessionRef, SessionRow, SpawnArgsJson,
    TagResult, negotiate,
};
use felis_grid::wire::{self, MAX_CELLS_PER_ROW, RowEncode};
use felis_grid::{Cell, Grapheme, StyleTable};
use felis_protocol::RowPayload;
use felis_protocol::kitty_graphics::{ImageId, PlacementId};
use felis_protocol::messages::{
    ByteSpan, ClipboardSelection, ClipboardWrite, ColSpan, GridMsg, ImageFormat, ImageMsg,
    ImageTarget, InputMsg, Key, KeyEvent, KeyEventKind, KeyLocation, KeyMods, NamedKey,
    Notification, NotifyToClientMsg, RegionToClientMsg, SearchToClientMsg, SourceRect, Urgency,
    ViewportAnchor,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const ID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/json-protocol")
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

fn session_object() -> SessionObject {
    serde_json::from_value(json!({
        "id": SESSION,
        "short_id": "01234567",
        "rows": 24,
        "cols": 80,
        "title": "zsh",
        "cwd": "file://localhost/tmp",
        "tags": ["work"],
        "foreground": "claude",
        "last_exit_code": 0,
        "attachments": [],
        "program_status": [],
    }))
    .expect("a session row")
}

fn session_row() -> SessionRow {
    SessionRow {
        session: session_object(),
        sequence: 3,
    }
}

fn key_enter() -> KeyEventJson {
    KeyEvent {
        key: Key::Named(NamedKey::Enter),
        text: Some("\r".to_owned()),
        mods: KeyMods::CTRL | KeyMods::SHIFT,
        kind: KeyEventKind::Press,
        location: KeyLocation::Standard,
    }
    .into()
}

/// One message and how to read it back as the type it was written as.
struct Case {
    name: &'static str,
    value: Value,
    reread: Box<dyn Fn(&Value) -> Value>,
}

fn case<T: Serialize + DeserializeOwned + PartialEq + Debug + 'static>(
    name: &'static str,
    message: T,
) -> Case {
    let value = serde_json::to_value(&message).expect("a message serializes");
    Case {
        name,
        value,
        reread: Box::new(move |value| {
            let back: T = serde_json::from_value(value.clone())
                .unwrap_or_else(|err| panic!("{name} does not read back: {err}"));
            assert_eq!(back, message, "{name}");
            serde_json::to_value(&back).expect("a message re-serializes")
        }),
    }
}

const fn request(id: u32, request: Request) -> ClientMessage {
    ClientMessage::Request { id, request }
}

const fn open(id: u32, stream: Open) -> ClientMessage {
    ClientMessage::Open { id, stream }
}

const fn reply(id: u32, result: OpResult) -> ServerMessage {
    ServerMessage::Reply { id, result }
}

const fn item(id: u32, item: Item) -> ServerMessage {
    ServerMessage::Item { id, item }
}

/// At least one message of every kind, with the attach items a browser
/// view mirrors a grid from.
fn cases() -> Vec<Case> {
    let session_ref = || SessionRef {
        id: SESSION.to_owned(),
        was_attached: None,
        exit_code: None,
    };
    vec![
        case("hello", ClientHello::Hello { versions: vec![1] }),
        case("welcome", negotiate(&[1])),
        case("refused", negotiate(&[7])),
        case("request-list", request(1, Request::List)),
        case(
            "request-info",
            request(
                2,
                Request::Info {
                    session: SessionId(ID),
                },
            ),
        ),
        case(
            "request-create",
            request(
                3,
                Request::Create {
                    args: serde_json::from_value::<SpawnArgsJson>(
                        json!({ "command": "htop", "tags": ["monitor"] }),
                    )
                    .unwrap(),
                },
            ),
        ),
        case(
            "request-kill",
            request(
                4,
                Request::Kill {
                    session: SessionId(ID),
                },
            ),
        ),
        case(
            "request-tag",
            request(
                5,
                Request::Tag {
                    session: SessionId(ID),
                    add: vec!["build".to_owned()],
                    remove: vec![],
                },
            ),
        ),
        case(
            "request-send",
            request(
                6,
                Request::Send {
                    session: SessionId(ID),
                    steps: vec![
                        SendStepJson::Paste {
                            bytes: Base64(b"make test".to_vec()),
                        },
                        SendStepJson::Key { event: key_enter() },
                    ],
                },
            ),
        ),
        case(
            "open-attach-session",
            open(
                7,
                Open::Attach {
                    target: AttachTarget::Session(SessionId(ID)),
                },
            ),
        ),
        case(
            "open-attach-create",
            open(
                8,
                Open::Attach {
                    target: AttachTarget::Create(SpawnArgsJson::default()),
                },
            ),
        ),
        case(
            "open-wait",
            open(
                9,
                Open::Wait {
                    session: SessionId(ID),
                    steps: vec![SendStepJson::Raw {
                        bytes: Base64(b"\x1b[A\r".to_vec()),
                    }],
                },
            ),
        ),
        case(
            "open-search",
            open(
                10,
                Open::Search {
                    session: SessionId(ID),
                    query: "error".to_owned(),
                    regex: false,
                    case_insensitive: true,
                    source: SearchSourceJson::Scrollback,
                },
            ),
        ),
        case(
            "open-capture",
            open(
                11,
                Open::Capture {
                    session: SessionId(ID),
                    source: RegionSourceJson::CommandOutput,
                    ansi: false,
                    max_rows: Some(100),
                },
            ),
        ),
        case(
            "open-notifications",
            open(12, Open::Notifications { session: None }),
        ),
        case(
            "input-key",
            ClientMessage::Input {
                id: 7,
                input: InputJson::try_from(InputMsg::Key(key_enter().try_into().unwrap())).unwrap(),
            },
        ),
        case(
            "input-paste",
            ClientMessage::Input {
                id: 7,
                input: InputMsg::Paste(b"hi".to_vec()).try_into().unwrap(),
            },
        ),
        case("cancel", ClientMessage::Cancel { id: 7 }),
        case(
            "reply-list",
            reply(
                1,
                OpResult::List {
                    sessions: vec![session_row()],
                },
            ),
        ),
        case(
            "reply-info",
            reply(
                2,
                OpResult::Info {
                    session: Box::new(session_row()),
                },
            ),
        ),
        case(
            "reply-create",
            reply(
                3,
                OpResult::Create {
                    session: Box::new(session_row()),
                },
            ),
        ),
        case("reply-kill", reply(4, OpResult::Kill(session_ref()))),
        case(
            "reply-tag",
            reply(
                5,
                OpResult::Tag(TagResult {
                    id: SESSION.to_owned(),
                    tags: vec!["build".to_owned(), "work".to_owned()],
                }),
            ),
        ),
        case("reply-send", reply(6, OpResult::Send(session_ref()))),
        case(
            "item-attached",
            item(
                7,
                Item::Attached {
                    minor: 3,
                    session: Box::new(session_row()),
                },
            ),
        ),
        case(
            "item-grid-row-delta",
            item(
                7,
                Item::Grid {
                    msg: GridMsg::RowDelta {
                        rows: vec![(0, packed_row("hi")), (3, packed_row("felis"))],
                    }
                    .try_into()
                    .unwrap(),
                },
            ),
        ),
        case(
            "item-grid-clipboard-set",
            item(
                7,
                Item::Grid {
                    msg: GridMsg::ClipboardSet {
                        write: ClipboardWrite {
                            selection: ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY,
                            data: b"copied".to_vec(),
                        },
                    }
                    .try_into()
                    .unwrap(),
                },
            ),
        ),
        case(
            "item-image-header",
            item(
                7,
                Item::Image {
                    msg: ImageMsg::Header {
                        id: ImageId(9),
                        target: ImageTarget::New {
                            width: 64,
                            height: 32,
                            format: ImageFormat::Rgba32,
                            serial: Some(2),
                        },
                    }
                    .into(),
                },
            ),
        ),
        case(
            "item-image-chunk",
            item(
                7,
                Item::Image {
                    msg: ImageMsg::Chunk {
                        id: ImageId(9),
                        bytes: vec![0, 1, 2, 254, 255].into(),
                    }
                    .into(),
                },
            ),
        ),
        case(
            "item-image-placement",
            item(
                7,
                Item::Image {
                    msg: ImageMsg::Placement {
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
                        requested: None,
                        serial: Some(5),
                    }
                    .into(),
                },
            ),
        ),
        case(
            "item-match",
            item(
                10,
                Item::Match(SearchMatchJson::from(SearchToClientMsg::Match {
                    line_index: -4,
                    text: "error: linker failed".to_owned(),
                    byte_spans: vec![ByteSpan { start: 0, end: 5 }],
                    col_spans: vec![ColSpan {
                        line_index: -4,
                        col_start: 0,
                        col_end: 5,
                    }],
                    anchor: Some(ViewportAnchor {
                        pushed: 1200,
                        generation: 2,
                    }),
                })),
            ),
        ),
        case(
            "item-row",
            item(
                11,
                Item::Row(
                    CaptureRowJson::try_from(RegionToClientMsg::Row {
                        row: -1,
                        text: "test result: ok".to_owned(),
                        ansi: None,
                        soft_wrap_continued: false,
                    })
                    .unwrap(),
                ),
            ),
        ),
        case(
            "item-notification",
            item(
                12,
                Item::Notification(
                    NotificationJson::try_from(NotifyToClientMsg::Event {
                        session_id: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
                        notification: Notification {
                            title: Some("build".to_owned()),
                            body: "done".to_owned(),
                            urgency: Urgency::Normal,
                        },
                        notify_id: None,
                        session_title: Some("zsh".to_owned()),
                        cwd: None,
                        attached: false,
                    })
                    .unwrap(),
                ),
            ),
        ),
        case("item-sent", item(9, Item::Sent { id: SessionId(ID) })),
        case("item-mark", item(9, Item::Mark { exit_code: Some(0) })),
        case("lag", ServerMessage::Lag { id: 12, dropped: 3 }),
        case(
            "end",
            ServerMessage::End {
                id: 11,
                count: 1,
                exit_code: Some(0),
            },
        ),
        case(
            "error",
            ServerMessage::Error {
                id: 7,
                error: ErrorObject::new(ErrorKind::SessionEnded, "the session ended"),
            },
        ),
        case(
            "error-moved",
            ServerMessage::Error {
                id: 7,
                error: ErrorObject {
                    session: Some(SessionId(ID)),
                    ..ErrorObject::new(
                        ErrorKind::Moved,
                        "another client switched this window to another session",
                    )
                },
            },
        ),
        case("lagged", ServerMessage::Lagged { id: 7 }),
    ]
}

fn rendered(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a message renders");
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
fn every_golden_message_matches_what_a_writer_emits() {
    for case in cases() {
        golden(case.name, &case.value);
    }
}

#[test]
fn every_golden_message_reads_back_as_what_was_written() {
    for case in cases() {
        assert_eq!((case.reread)(&case.value), case.value, "{}", case.name);
    }
}

/// The grid conversions are the reason a browser can mirror a session,
/// so a golden row delta must also recode to the wire body it came from.
#[test]
fn a_golden_row_delta_recodes_to_its_wire_body() {
    let msg = GridMsg::RowDelta {
        rows: vec![(0, packed_row("hi")), (3, packed_row("felis"))],
    };
    let json = GridJson::try_from(msg.clone()).unwrap();
    assert_eq!(GridMsg::try_from(json).unwrap(), msg);
    let value = golden_path("item-grid-row-delta");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(value).unwrap()).unwrap();
    let cells = &value["item"]["msg"]["rows"][0]["cells"];
    assert_eq!(cells["encoding"], json!("rle"), "{cells}");
    assert!(
        cells["graphemes"]
            .as_array()
            .expect("a row carries its graphemes")
            .iter()
            .all(|g| g["type"] == json!("ascii") && g["byte"].is_number()),
        "graphemes must be structural, got: {cells}"
    );
}

#[test]
fn image_bytes_travel_as_a_base64_string() {
    let msg = ImageJson::from(ImageMsg::Chunk {
        id: ImageId(9),
        bytes: vec![104, 105].into(),
    });
    assert_eq!(serde_json::to_value(&msg).unwrap()["bytes"], json!("aGk="));
}

/// Within a version a writer may add an optional field, so a reader
/// that predates it must still read the message.
#[test]
fn a_message_carrying_an_unknown_optional_field_still_reads() {
    let mut value = cases()
        .into_iter()
        .find(|case| case.name == "item-grid-row-delta")
        .unwrap()
        .value;
    value["invented_later"] = json!(1);
    value["item"]["invented_later"] = json!(true);
    value["item"]["msg"]["rows"][0]["invented_later"] = json!({ "any": "shape" });
    let back: ServerMessage = serde_json::from_value(value).expect("the message still reads");
    assert!(matches!(back, ServerMessage::Item { id: 7, .. }));
}

#[test]
fn a_writer_answers_with_the_highest_shared_version() {
    assert_eq!(negotiate(&[1]), ServerHello::Welcome { version: 1 });
    assert_eq!(negotiate(&[3, 1, 2]), ServerHello::Welcome { version: 1 });
}

#[test]
fn a_writer_refuses_an_offer_it_shares_no_version_with() {
    for offered in [&[][..], &[0], &[2, 3]] {
        assert!(
            matches!(
                negotiate(offered),
                ServerHello::Refused { ref versions, .. } if versions == &[1]
            ),
            "{offered:?}"
        );
    }
}

/// A later version may reshape every message but the handshake, so a
/// hello a later client writes must still read here.
#[test]
fn a_hello_offering_later_versions_reads() {
    let hello: ClientHello =
        serde_json::from_value(json!({ "type": "hello", "versions": [1, 2, 9], "later": 1 }))
            .unwrap();
    let ClientHello::Hello { versions } = hello;
    assert_eq!(negotiate(&versions), ServerHello::Welcome { version: 1 });
}

fn search(id: u32) -> ClientMessage {
    open(
        id,
        Open::Search {
            session: SessionId(ID),
            query: "x".to_owned(),
            regex: false,
            case_insensitive: false,
            source: SearchSourceJson::Visible,
        },
    )
}

const fn a_mark() -> Item {
    Item::Mark { exit_code: None }
}

#[test]
fn a_stream_takes_items_until_exactly_one_terminal() {
    for terminal in [
        ServerMessage::End {
            id: 1,
            count: 0,
            exit_code: None,
        },
        ServerMessage::Lagged { id: 1 },
        ServerMessage::Error {
            id: 1,
            error: ErrorObject::new(ErrorKind::Internal, String::new()),
        },
    ] {
        let mut ledger = Ledger::new();
        ledger
            .client(&open(
                1,
                Open::Wait {
                    session: SessionId(ID),
                    steps: vec![],
                },
            ))
            .unwrap();
        ledger.server(&item(1, a_mark())).unwrap();
        ledger
            .server(&ServerMessage::Lag { id: 1, dropped: 2 })
            .unwrap();
        ledger.server(&terminal).unwrap();
        assert!(!ledger.is_open(1));
        assert_eq!(ledger.server(&terminal), Err(FramingError::Unknown(1)));
        assert_eq!(
            ledger.server(&item(1, a_mark())),
            Err(FramingError::Unknown(1))
        );
    }
}

#[test]
fn a_request_ends_with_one_reply_and_is_no_stream() {
    let mut ledger = Ledger::new();
    ledger.client(&request(1, Request::List)).unwrap();
    assert_eq!(
        ledger.server(&item(1, a_mark())),
        Err(FramingError::NotAStream(1))
    );
    assert_eq!(
        ledger.server(&ServerMessage::End {
            id: 1,
            count: 0,
            exit_code: None,
        }),
        Err(FramingError::NotAStream(1))
    );
    ledger
        .server(&reply(1, OpResult::List { sessions: vec![] }))
        .unwrap();
    assert_eq!(
        ledger.server(&reply(1, OpResult::List { sessions: vec![] })),
        Err(FramingError::Unknown(1))
    );
}

#[test]
fn a_reply_does_not_end_a_stream() {
    let mut ledger = Ledger::new();
    ledger.client(&search(1)).unwrap();
    assert_eq!(
        ledger.server(&reply(1, OpResult::List { sessions: vec![] })),
        Err(FramingError::NotARequest(1))
    );
    assert!(ledger.is_open(1));
}

#[test]
fn an_open_id_cannot_be_taken_twice() {
    let mut ledger = Ledger::new();
    ledger.client(&search(1)).unwrap();
    assert_eq!(
        ledger.client(&request(1, Request::List)),
        Err(FramingError::InUse(1))
    );
}

#[test]
fn an_item_must_belong_to_its_stream() {
    let mut ledger = Ledger::new();
    ledger.client(&search(1)).unwrap();
    assert_eq!(
        ledger.server(&item(1, a_mark())),
        Err(FramingError::WrongStream(1))
    );
}

#[test]
fn input_rides_an_open_attach_stream_only() {
    let input = |id| ClientMessage::Input {
        id,
        input: InputMsg::FocusChange { focused: true }.try_into().unwrap(),
    };
    let mut ledger = Ledger::new();
    ledger.client(&search(1)).unwrap();
    assert_eq!(ledger.client(&input(1)), Err(FramingError::WrongStream(1)));
    assert_eq!(ledger.client(&input(2)), Err(FramingError::Unknown(2)));
    ledger
        .client(&open(
            2,
            Open::Attach {
                target: AttachTarget::Session(SessionId(ID)),
            },
        ))
        .unwrap();
    ledger.client(&input(2)).unwrap();
}

/// An attach opener names one session or one spawn, never neither.
#[test]
fn an_attach_opener_naming_no_target_is_refused() {
    assert!(
        serde_json::from_value::<ClientMessage>(
            json!({ "type": "open", "id": 1, "stream": { "stream": "attach" } })
        )
        .is_err()
    );
}

const SCHEMA_URL: &str = "https://raw.githubusercontent.com/felis-terminal/felis/main/crates/felis-client-core/schemas/json-protocol-v1.schema.json";

/// The committed document, compiled once and offline.
fn schema() -> &'static (Schemas, SchemaIndex) {
    static COMPILED: OnceLock<(Schemas, SchemaIndex)> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schemas/json-protocol-v1.schema.json");
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

fn validates(message: &Value) -> bool {
    let (schemas, index) = schema();
    schemas.validate(message, *index).is_ok()
}

/// What a reader refuses on a server message, as a relay's browser
/// reads it.
fn reads(message: &Value) -> bool {
    let Ok(message) = serde_json::from_value::<ServerMessage>(message.clone()) else {
        return false;
    };
    match message {
        ServerMessage::Item {
            item: Item::Grid { msg },
            ..
        } => GridMsg::try_from(msg).is_ok(),
        _ => true,
    }
}

#[test]
fn every_golden_message_validates_against_the_published_schema() {
    for case in cases() {
        let (schemas, index) = schema();
        if let Err(err) = schemas.validate(&case.value, *index) {
            panic!("{} does not validate:\n{err}", case.name);
        }
    }
}

fn row_delta_with(cells: &Value) -> Value {
    json!({
        "type": "item",
        "id": 1,
        "item": {
            "item": "grid",
            "msg": { "type": "row_delta", "rows": [{ "row": 0, "cells": cells }] },
        },
    })
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

/// A validator must refuse what the reader refuses, or a consumer that
/// checks the schema first still meets a read failure.
#[test]
fn what_the_reader_refuses_the_schema_refuses() {
    let mut uppercase_id = serde_json::to_value(item(9, Item::Sent { id: SessionId(ID) })).unwrap();
    uppercase_id["item"]["id"] = json!(SESSION.to_uppercase());

    let mut unpadded = serde_json::to_value(item(
        7,
        Item::Image {
            msg: ImageMsg::Chunk {
                id: ImageId(9),
                bytes: b"hi".to_vec().into(),
            }
            .into(),
        },
    ))
    .unwrap();
    unpadded["item"]["msg"]["bytes"] = json!("aGk");

    let image_id_past_u32 = json!({
        "type": "item",
        "id": 1,
        "item": { "item": "image", "msg": { "type": "complete", "id": u64::from(u32::MAX) + 1 } },
    });

    let id_past_u32 = json!({ "type": "lagged", "id": u64::from(u32::MAX) + 1 });

    let unknown_error_kind = json!({
        "type": "error",
        "id": 1,
        "error": { "kind": "minted_later", "message": "" },
    });

    let mut too_many_graphemes = row_delta_with(&row_cells(
        &vec![grapheme(b'x'); MAX_CELLS_PER_ROW + 1],
        &json!([]),
    ));
    too_many_graphemes["item"]["msg"]["rows"][0]["cells"]["attr_runs"][0]["len"] =
        json!(MAX_CELLS_PER_ROW + 1);

    for (name, message) in [
        (
            "a control byte in an ascii grapheme",
            row_delta_with(&row_cells(&[grapheme(10)], &json!([]))),
        ),
        ("an uppercase session id", uppercase_id),
        ("unpadded base64", unpadded),
        ("an image id past u32", image_id_past_u32),
        ("a correlation id past u32", id_past_u32),
        (
            "an error kind this version does not name",
            unknown_error_kind,
        ),
        (
            "a fraction that is not below one",
            row_delta_with(&row_cells(&[grapheme(b'x')], &sized_cell(1, 1))),
        ),
        ("a row wider than the codec admits", too_many_graphemes),
    ] {
        assert!(
            !validates(&message),
            "{name} validated but must not: {message}"
        );
        assert!(!reads(&message), "{name} read but must not: {message}");
    }
}

/// The bound itself is admitted, so the refusals above are attributable
/// to the value and not to a schema that rejects the whole class.
#[test]
fn every_value_at_a_published_bound_is_admitted() {
    for (name, message) in [
        (
            "an image id at u32::MAX",
            json!({
                "type": "item",
                "id": u32::MAX,
                "item": { "item": "image", "msg": { "type": "complete", "id": u32::MAX } },
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
    ] {
        assert!(validates(&message), "{name} must validate: {message}");
        assert!(reads(&message), "{name} must read: {message}");
    }
}

#[test]
fn an_attach_opener_naming_both_targets_is_refused() {
    let both = json!({
        "type": "open",
        "id": 1,
        "stream": { "stream": "attach", "session": SESSION, "create": {} },
    });
    assert!(!validates(&both), "{both}");
    assert!(serde_json::from_value::<ClientMessage>(both).is_err());
}

/// The protocol's session row is the CLI's session object with the
/// creation sequence beside it, so a reader of either shape reads both.
#[test]
fn a_session_row_is_the_cli_session_object_plus_its_sequence() {
    let row = serde_json::to_value(session_row()).unwrap();
    let mut object = serde_json::to_value(session_object()).unwrap();
    object["sequence"] = json!(3);
    assert_eq!(row, object);
    let cli: SessionObject = serde_json::from_value(row).unwrap();
    assert_eq!(cli, session_object());
}

#[test]
fn a_search_match_keeps_its_history_anchor() {
    let anchor = ViewportAnchor {
        pushed: 7,
        generation: 1,
    };
    let item = SearchMatchJson::from(SearchToClientMsg::Match {
        line_index: -1,
        text: "x".to_owned(),
        byte_spans: vec![],
        col_spans: vec![],
        anchor: Some(anchor),
    });
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(value["anchor"], json!({ "pushed": 7, "generation": 1 }));
    let back: SearchMatchJson = serde_json::from_value(value).unwrap();
    assert_eq!(back, item);
}

#[test]
fn a_request_is_not_closed_by_another_operations_result() {
    let mut ledger = Ledger::new();
    ledger.client(&request(1, Request::List)).unwrap();
    let kill = reply(
        1,
        OpResult::Kill(SessionRef {
            id: SESSION.to_owned(),
            was_attached: None,
            exit_code: None,
        }),
    );
    assert_eq!(ledger.server(&kill), Err(FramingError::WrongResult(1)));
    assert!(ledger.is_open(1));
    ledger
        .server(&reply(1, OpResult::List { sessions: vec![] }))
        .unwrap();
}
