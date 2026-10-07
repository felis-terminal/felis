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
        ("cursor", |v| v["screen"]["cursor"]["row"] = 5.into()),
        ("tab stops", |v| v["tab_stops"] = serde_json::json!([true])),
        ("row codec", |v| v["screen"]["row_codec"] = 99.into()),
        ("ring", |v| v["screen"]["ring"]["phys_cap"] = 4.into()),
        ("links", |v| v["screen"]["links"] = serde_json::json!([])),
        ("styles", |v| {
            v["screen"]["styles"].as_array_mut().unwrap().truncate(1);
        }),
        ("margins", |v| v["margins"]["bottom"] = 9.into()),
    ];
    for (what, edit) in edits {
        assert!(corrupt(&grid, edit).is_some(), "{what} was accepted");
    }
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
