use felis_vt::Parser;
use proptest::prelude::*;

use super::*;

/// What a dump promises to keep: everything but the bytes past each
/// row's occupancy, which no reader looks at, the fast-path hint a
/// stale sized cell there can hold up, and the instant of a sync
/// deadline, which moves with the clock it is measured on.
fn canonical(grid: &Grid) -> Grid {
    fn blank_tails(cells: &mut [Cell], cols: u16, occupancy: &[u16]) {
        let cols = usize::from(cols);
        for (row, &occupied) in occupancy.iter().enumerate() {
            let start = row * cols + usize::from(occupied);
            cells[start..(row + 1) * cols].fill(Cell::default());
        }
    }
    let mut grid = grid.clone();
    let screen = &mut grid.screen;
    blank_tails(&mut screen.cells, screen.cols, &screen.occupancy);
    for saved in [
        screen.saved_primary.as_mut(),
        screen.saved_alternate.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        blank_tails(&mut saved.cells, saved.cols, &saved.occupancy);
    }
    screen.refresh_has_sized_cells();
    if let SyncOutput::On { deadline } = &mut grid.sync_output {
        *deadline = None;
    }
    grid
}

fn hop(parser: &Parser, grid: &Grid) -> (Parser, Grid) {
    let parser_json = serde_json::to_vec(parser).unwrap();
    let grid_json = serde_json::to_vec(grid).unwrap();
    let parser: Parser = serde_json::from_slice(&parser_json).unwrap();
    let grid: Grid = serde_json::from_slice(&grid_json).unwrap();
    parser.check_restored().unwrap();
    grid.check_restored().unwrap();
    (parser, grid)
}

/// Feeds `head`, carries parser and grid across a dump, then feeds
/// `tail` to the original and the restored pair alike: the two agree
/// after the hop and after the tail, queued effects included.
fn assert_survives(rows: u16, cols: u16, scrollback: usize, head: &[u8], tail: &[u8]) {
    let mut parser = Parser::new();
    let mut grid = Grid::with_scrollback(rows, cols, scrollback);
    parser.advance(&mut grid, head);
    let (mut parser2, mut grid2) = hop(&parser, &grid);
    assert!(parser2 == parser, "parser differs after the hop");
    assert!(
        canonical(&grid2) == canonical(&grid),
        "grid differs after the hop"
    );
    parser.advance(&mut grid, tail);
    parser2.advance(&mut grid2, tail);
    assert_eq!(grid2.take_pty_effects(), grid.take_pty_effects());
    assert!(
        canonical(&grid2) == canonical(&grid),
        "grid differs after the tail"
    );
}

fn representative_output() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b]2;build\x07\x1b[22;0t\x1b]2;inner\x07");
    out.extend_from_slice(b"\x1b[?2004h\x1b[?1004h\x1b[>1u\x1b[3g\x1bH");
    for i in 0..60_u32 {
        out.extend_from_slice(
            format!(
                "\x1b[38;2;{};20;30;48;5;{}m line {i} \x1b]8;id=l{i};https://example.com/{i}\x1b\\link\x1b]8;;\x1b\\ \u{65e5}\u{672c} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\x1b[0m\r\n",
                i % 256,
                (i * 7) % 256
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(b"\x1b]66;s=2;big\x07\r\n");
    out.extend_from_slice(b"\x1b[5;12r\x1b[12;1H\n\n\n\x1b[r");
    out.extend_from_slice(b"\x1b[3;4H\x1b7\x1b[?1049h\x1b[2J\x1b[Halt \x1b[1;4mscreen\x1b[0m");
    out.extend_from_slice(b"\x1b[?2026h\x07");
    out.extend_from_slice(b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\");
    out
}

#[test]
fn a_grid_survives_the_hop_on_either_screen_and_mid_sequence() {
    let output = representative_output();
    assert_survives(24, 80, 1000, &output, b"\x1b[?1049l\x1b8more\r\n\x1b[6n");
    let mut cut = output.clone();
    cut.extend_from_slice(b"\x1b[38;2;1");
    assert_survives(24, 80, 1000, &cut, b"0;2m tail\x1b[c");
    assert_survives(24, 80, 1000, &output, b"\x1b]10;?\x07\x1bP$qm\x1b\\");
}

#[test]
fn a_byte_budgeted_apc_queue_survives_the_hop_mid_body() {
    // Five 200 KiB bodies fit the queue's 1 MiB and a sixth does not;
    // the hop lands inside the fifth.
    let body = format!("\x1b_Ga=t,i=1,m=1;{}\x1b\\", "A".repeat(200 * 1024));
    let mut stream = body.repeat(6).into_bytes();
    let tail = stream.split_off(body.len() * 4 + 100 * 1024);
    assert_survives(5, 10, 0, &stream, &tail);
}

/// The status budget is recounted from the queue, not carried: one
/// short of it before the hop, the next report after it yields.
#[test]
fn a_status_queue_one_short_of_its_yield_survives_the_hop() {
    let report = b"\x1b]7501;state=done\x07";
    let mut parser = Parser::new();
    let mut grid = Grid::new(2, 8);
    for _ in 1..crate::STATUS_OUTBOX_YIELD {
        assert_eq!(parser.advance_until_yield(&mut grid, report), None);
    }
    let (mut parser, mut grid) = hop(&parser, &grid);
    assert_eq!(
        parser.advance_until_yield(&mut grid, report),
        Some(report.len())
    );
}

/// A transient report before the hop still owes the next prompt start
/// its expiry after it.
#[test]
fn an_owed_prompt_expiry_survives_the_hop() {
    let mut parser = Parser::new();
    let mut grid = Grid::new(2, 8);
    parser.advance(&mut grid, b"\x1b]7501;state=working\x07");
    assert_eq!(grid.take_pty_effects().len(), 1, "the report");
    let (mut parser, mut grid) = hop(&parser, &grid);
    parser.advance(&mut grid, b"\x1b]133;A\x07");
    assert_eq!(grid.take_pty_effects(), [PtyEffect::PromptStarted]);
}

/// The configured palette is the windows' to report again, so a restored
/// session starts without one rather than answering a departed window's.
#[test]
fn a_configured_palette_does_not_survive_the_hop() {
    let parser = Parser::new();
    let mut grid = Grid::new(2, 8);
    grid.set_palette_config_default(1, Some((0xFF, 0x55, 0x55)));
    let (_, grid) = hop(&parser, &grid);
    assert_eq!(grid.palette_config_default(1), None);
}

#[test]
fn a_full_scrollback_ring_survives_the_hop() {
    let mut output = Vec::new();
    for i in 0..300 {
        output.extend_from_slice(format!("row {i}\r\n").as_bytes());
    }
    assert_survives(10, 20, 100, &output, b"after\r\nmore\r\n");
}

#[test]
fn an_absent_field_takes_a_fresh_terminals_value() {
    let mut parser = Parser::new();
    let mut grid = Grid::new(5, 10);
    parser.advance(&mut grid, b"\x1b[?7l");
    let mut value = serde_json::to_value(&grid).unwrap();
    value.as_object_mut().unwrap().remove("autowrap");
    let restored: Grid = serde_json::from_value(value).unwrap();
    assert!(restored.autowrap);
}

type Edit = fn(&mut serde_json::Value);

fn corrupt(grid: &Grid, edit: impl FnOnce(&mut serde_json::Value)) -> Option<String> {
    let mut value = serde_json::to_value(grid).unwrap();
    edit(&mut value);
    match serde_json::from_value::<Grid>(value) {
        Ok(restored) => restored.check_restored().err().map(|err| err.0),
        Err(err) => Some(err.to_string()),
    }
}

#[test]
fn inconsistent_screen_state_is_refused() {
    let mut parser = Parser::new();
    let mut grid = Grid::new(5, 10);
    parser.advance(
        &mut grid,
        b"\x1b]8;;https://x\x1b\\ab\x1b]8;;\x1b\\\r\n\x1b[1mbold",
    );
    assert_eq!(corrupt(&grid, |_| {}), None);
    let edits: [(&str, Edit); 7] = [
        ("cursor outside the screen", |v| {
            v["screen"]["cursor"]["row"] = 5.into();
        }),
        ("tab stops", |v| v["tab_stops"] = serde_json::json!([true])),
        ("row codec 99 is not the version", |v| {
            v["screen"]["row_codec"] = 99.into();
        }),
        ("ring geometry", |v| {
            v["screen"]["ring"]["phys_cap"] = 4.into();
        }),
        ("names a handle past its table", |v| {
            v["screen"]["links"] = serde_json::json!([]);
        }),
        ("a row names a pen the style table lacks", |v| {
            v["screen"]["styles"].as_array_mut().unwrap().truncate(1);
        }),
        ("scroll margins", |v| v["margins"]["bottom"] = 9.into()),
    ];
    for (expected, edit) in edits {
        let err = corrupt(&grid, edit).unwrap_or_else(|| panic!("{expected:?} was accepted"));
        assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
    }
}

fn assert_admitted(grid: &Grid, case: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    if let Some(err) = corrupt(grid, edit) {
        panic!("{case}: refused with {err:?}");
    }
}

fn assert_refused(grid: &Grid, expected: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let err = corrupt(grid, edit).unwrap_or_else(|| panic!("{expected:?} was accepted"));
    assert!(err.contains(expected), "expected {expected:?}, got {err:?}");
}

#[test]
fn the_cursor_and_margins_may_reach_the_last_row_and_column_but_not_past() {
    let grid = Grid::new(5, 10);
    assert_admitted(&grid, "cursor on the last cell", |v| {
        v["screen"]["cursor"]["row"] = 4.into();
        v["screen"]["cursor"]["col"] = 9.into();
    });
    assert_refused(&grid, "cursor outside the screen", |v| {
        v["screen"]["cursor"]["col"] = 10.into();
    });
    assert_refused(&grid, "scroll margins", |v| {
        v["margins"]["bottom"] = 5.into();
    });
    assert_refused(&grid, "scroll margins", |v| {
        v["margins"]["right"] = 10.into();
    });
}

#[test]
fn the_pen_id_must_name_a_table_entry_that_is_the_pen() {
    let mut parser = Parser::new();
    let mut grid = Grid::new(5, 10);
    parser.advance(&mut grid, b"\x1b[1mx\x1b[0m");
    let pens = serde_json::to_value(&grid).unwrap()["screen"]["styles"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(pens, 2, "the default pen and bold");
    // Past the table, the id would resolve to the default pen, which is
    // the pen here: only the bound tells the two apart.
    assert_refused(&grid, "pen id", |v| v["pen_style"] = pens.into());
    assert_refused(&grid, "pen id", |v| v["pen_style"] = 1.into());
}

#[test]
fn the_scroll_band_must_lie_inside_the_screen_and_its_rotation_inside_the_band() {
    let grid = Grid::new(5, 10);
    let band = |top: usize, len: usize, rot: usize| {
        move |v: &mut serde_json::Value| {
            v["screen"]["band_top"] = top.into();
            v["screen"]["band_len"] = len.into();
            v["screen"]["band_rot"] = rot.into();
        }
    };
    assert_admitted(&grid, "a band ending on the last row", band(1, 4, 3));
    assert_refused(&grid, "scroll band", band(0, 6, 0));
    assert_refused(&grid, "scroll band", band(2, 4, 0));
    assert_refused(&grid, "scroll band", band(1, 4, 4));
}

#[test]
fn damage_must_be_sized_for_the_screen_with_no_bit_past_its_last_row() {
    let grid = Grid::new(5, 10);
    let damage = |blocks: serde_json::Value, len: usize| {
        move |v: &mut serde_json::Value| {
            v["screen"]["damage"] = serde_json::json!({ "blocks": blocks, "len": len });
        }
    };
    assert_admitted(
        &grid,
        "every row dirty",
        damage(serde_json::json!([0b1_1111]), 5),
    );
    assert_refused(&grid, "damage rows", damage(serde_json::json!([0]), 6));
    assert_refused(&grid, "damage rows", damage(serde_json::json!([0, 0]), 5));
    assert_refused(
        &grid,
        "damage rows",
        damage(serde_json::json!([0b10_0000]), 5),
    );
}

#[test]
fn held_bidi_controls_may_fill_their_queue_but_not_overrun_it() {
    let grid = Grid::new(5, 10);
    assert_admitted(&grid, "a full queue", |v| {
        v["pending_bidi"]["len"] = 8.into();
    });
    assert_refused(&grid, "pending bidi controls", |v| {
        v["pending_bidi"]["len"] = 9.into();
    });
}

#[test]
fn an_open_dcs_body_may_fill_its_buffer_but_not_overrun_it() {
    let grid = Grid::new(5, 10);
    let body = |len: usize| {
        move |v: &mut serde_json::Value| {
            v["dcs"] = serde_json::json!({
                "kind": "Other",
                "body": felis_vt::state::b64::encode(&vec![b'x'; len]),
            });
        }
    };
    assert_admitted(&grid, "a full DCS buffer", body(crate::DCS_BUFFER_LIMIT));
    assert_refused(&grid, "DCS body", body(crate::DCS_BUFFER_LIMIT + 1));
}

#[test]
fn queued_apc_bodies_are_held_to_the_outbox_count_byte_and_body_caps() {
    let grid = Grid::new(5, 10);
    let queue = |count: usize, len: usize| {
        let apc = serde_json::to_value(PtyEffect::Apc(ApcBody {
            body: vec![b'A'; len],
            cursor_row: 0,
            cursor_col: 0,
        }))
        .unwrap();
        move |v: &mut serde_json::Value| {
            v["pty_effects"] = serde_json::Value::Array(vec![apc; count]);
        }
    };
    let limit = felis_vt::APC_BUFFER_LIMIT;
    assert_eq!(APC_OUTBOX_BYTES % limit, 0, "whole bodies fill the bytes");
    let fill = APC_OUTBOX_BYTES / limit;
    assert!(fill < APC_OUTBOX_CAP, "the byte cap binds before the count");
    assert_admitted(&grid, "a full count", queue(APC_OUTBOX_CAP, 1));
    assert_admitted(&grid, "full bytes", queue(fill, limit));
    assert_refused(&grid, "queued APC bodies", queue(APC_OUTBOX_CAP + 1, 1));
    assert_refused(&grid, "queued APC bodies", queue(fill + 1, limit));
    assert_refused(&grid, "queued APC bodies", queue(1, limit + 1));
}

#[test]
fn notification_reassembly_is_held_to_its_id_and_byte_caps() {
    let grid = Grid::new(5, 10);
    let pending = |ids: usize, title: usize, body: usize| {
        move |v: &mut serde_json::Value| {
            let partials = (0..ids)
                .map(|id| {
                    let partial = serde_json::json!({
                        "title": "t".repeat(title),
                        "body": "b".repeat(body),
                    });
                    (id.to_string(), partial)
                })
                .collect();
            v["osc99_pending"] = serde_json::Value::Object(partials);
        }
    };
    let (ids, bytes) = (crate::NOTIFY_REASSEMBLY_IDS, crate::NOTIFY_REASSEMBLY_BYTES);
    assert_admitted(&grid, "every id in use", pending(ids, 0, 0));
    assert_admitted(&grid, "a partial at the byte cap", pending(1, bytes, 0));
    assert_refused(&grid, "notification reassembly", pending(ids + 1, 0, 0));
    assert_refused(&grid, "notification reassembly", pending(1, bytes, 1));
}

fn fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-z ]{1,12}".prop_map(String::into_bytes),
        Just(b"\r\n".to_vec()),
        Just("\u{4e2d}\u{1F600}\u{301}".as_bytes().to_vec()),
        Just(b"\x1b[1;31m".to_vec()),
        Just(b"\x1b[38;2;9;8;7m".to_vec()),
        Just(b"\x1b[0m".to_vec()),
        Just(b"\x1b]8;;https://e/x\x1b\\".to_vec()),
        Just(b"\x1b]8;;\x1b\\".to_vec()),
        Just(b"\x1b[2;5r".to_vec()),
        Just(b"\x1b[r".to_vec()),
        Just(b"\x1b7".to_vec()),
        Just(b"\x1b8".to_vec()),
        Just(b"\x1b[?1049h".to_vec()),
        Just(b"\x1b[?1049l".to_vec()),
        Just(b"\x1b[?47h".to_vec()),
        Just(b"\x1b[?47l".to_vec()),
        Just(b"\x1b[3L".to_vec()),
        Just(b"\x1b[2M".to_vec()),
        Just(b"\x1bM".to_vec()),
        Just(b"\x1b[J".to_vec()),
        Just(b"\x1b]66;s=2;w\x07".to_vec()),
        Just(b"\x1b]2;t\x07".to_vec()),
        Just(b"\x1b]7501;state=working:progress=5:msg=aGk=\x07".to_vec()),
        Just(b"\x1b]7501;id=a/b:state=blocked:kind=auth\x1b\\".to_vec()),
        Just(b"\x1b]7501;state=clear\x1b\\".to_vec()),
        Just(b"\x1b]133;A\x07".to_vec()),
        Just(b"\x1b[6n".to_vec()),
        Just(b"\x1b[".to_vec()),
        Just(b"\x1b]".to_vec()),
        Just(b"\xe6\x97".to_vec()),
    ]
}

proptest! {
    /// Wherever the stream stops, inside a sequence or a UTF-8 scalar
    /// included, the restored pair continues exactly as the original.
    #[test]
    fn any_stream_cut_anywhere_survives_the_hop(
        head in proptest::collection::vec(fragment(), 0..60),
        tail in proptest::collection::vec(fragment(), 0..20),
    ) {
        assert_survives(6, 12, 8, &head.concat(), &tail.concat());
    }
}
