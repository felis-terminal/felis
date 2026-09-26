//! Tests for pipe-region serialization.

use super::*;
use felis_grid::{AttrFlags, Attributes, Color, Grapheme, StyleId};

fn ascii_cell(b: u8) -> Cell {
    Cell {
        grapheme: Grapheme::Ascii(b),
        style: StyleId::DEFAULT,
        link: None,
        sizing: None,
    }
}

fn write_row(grid: &mut Grid, row: u16, s: &str) {
    for (col, b) in s.bytes().enumerate() {
        grid.set_cell(row, u16::try_from(col).unwrap(), ascii_cell(b));
    }
}

#[test]
fn visible_serializes_the_live_screen_with_trailing_rows_trimmed() {
    let mut grid = Grid::new(4, 8);
    write_row(&mut grid, 0, "hello");
    write_row(&mut grid, 1, "world");
    let out = serialize_region(&grid, 0, RegionSource::Visible, false).unwrap();
    assert_eq!(out, b"hello\nworld\n");
}

#[test]
fn visible_reconstructs_color() {
    let mut grid = Grid::new(2, 4);
    let style = grid.style_table_mut().intern(Attributes {
        flags: AttrFlags::BOLD,
        fg: Color::Indexed(1),
        ..Attributes::default()
    });
    grid.set_cell(
        0,
        0,
        Cell {
            grapheme: Grapheme::Ascii(b'X'),
            style,
            link: None,
            sizing: None,
        },
    );
    let out = serialize_region(&grid, 0, RegionSource::Visible, true).unwrap();
    assert_eq!(out, b"\x1b[1;31mX\x1b[0m\n");
}

#[test]
fn scrollback_source_includes_the_live_screen() {
    let mut grid = Grid::new(3, 8);
    write_row(&mut grid, 0, "prompt$");
    let out = serialize_region(&grid, 0, RegionSource::Scrollback, false).unwrap();
    assert_eq!(out, b"prompt$\n");
}

#[test]
fn fully_blank_screen_serializes_empty() {
    let grid = Grid::new(4, 8);
    let out = serialize_region(&grid, 0, RegionSource::Visible, false).unwrap();
    assert_eq!(out, Vec::<u8>::new());
}

#[test]
fn osc133_sources_are_none_without_a_completed_command() {
    let grid = Grid::new(2, 4);
    assert!(serialize_region(&grid, 0, RegionSource::CommandOutput, false).is_none());
    assert!(serialize_region(&grid, 0, RegionSource::LastCommand, false).is_none());
}

fn drive_command_cycle(grid: &mut Grid, prompt: &str, command: &str, output: &[&str]) {
    let mut p = felis_vt::Parser::new();
    p.advance(grid, b"\x1b]133;A\x07");
    p.advance(grid, prompt.as_bytes());
    p.advance(grid, b"\x1b]133;B\x07");
    p.advance(grid, command.as_bytes());
    p.advance(grid, b"\r\n");
    p.advance(grid, b"\x1b]133;C\x07");
    for line in output {
        p.advance(grid, line.as_bytes());
        p.advance(grid, b"\r\n");
    }
    p.advance(grid, b"\x1b]133;D;0\x07");
}

#[test]
fn last_command_includes_the_prompt_and_command_line() {
    let mut grid = Grid::new(4, 20);
    drive_command_cycle(&mut grid, "$ ", "echo hi", &["hi"]);
    let out = serialize_region(&grid, 0, RegionSource::LastCommand, false).unwrap();
    assert_eq!(out, b"$ echo hi\nhi\n");
}

#[test]
fn command_range_spans_scrollback_after_the_output_scrolls() {
    // A 2-row grid pushes the B mark into scrollback while C stays on
    // screen; absolute line coordinates must stitch the halves
    // (docs/explanation/data-model/scrollback.md).
    let mut grid = Grid::new(2, 20);
    drive_command_cycle(&mut grid, "$ ", "echo hi", &["hi"]);
    assert_eq!(grid.scrollback_total_pushed(), 1, "prompt row scrolled off");

    let output = serialize_region(&grid, 0, RegionSource::CommandOutput, false).unwrap();
    assert_eq!(output, b"hi\n");

    let last = serialize_region(&grid, 0, RegionSource::LastCommand, false).unwrap();
    assert_eq!(last, b"$ echo hi\nhi\n");
}

#[test]
fn last_mark_range_picks_the_most_recent_completed_command() {
    use felis_protocol::messages::PromptKind::{CommandEnd, InputStart, OutputStart};
    let mark = |line, kind| PromptMark {
        line,
        kind,
        exit_code: None,
    };
    let marks = [
        mark(0, InputStart),
        mark(1, OutputStart),
        mark(3, CommandEnd),
        mark(4, InputStart),
        mark(5, OutputStart),
        mark(8, CommandEnd),
        mark(9, InputStart), // running command, no D
    ];
    assert_eq!(last_mark_range(&marks, OutputStart), Some((5, 8)));
    assert_eq!(last_mark_range(&marks, InputStart), Some((4, 8)));
}

#[test]
fn trim_trailing_blank_rows_drops_the_tail_only() {
    let mut rows = vec![
        (b"a".to_vec(), false),
        (Vec::new(), false), // interior blank line is preserved
        (b"b".to_vec(), false),
        (Vec::new(), false), // trailing blanks dropped
        (Vec::new(), false),
    ];
    trim_trailing_blank_rows(&mut rows);
    assert_eq!(join_rows(&rows), b"a\n\nb\n");
}

#[test]
fn join_rows_stitches_soft_wrap_continuations_without_a_newline() {
    let rows = vec![
        (b"https://example.com/".to_vec(), false),
        (b"long/path".to_vec(), true),
        (b"next line".to_vec(), false),
    ];
    assert_eq!(
        join_rows(&rows),
        b"https://example.com/long/path\nnext line\n".to_vec()
    );
}

#[test]
fn join_rows_keeps_a_mid_line_sgr_reset_inside_a_stitched_line() {
    // Pins the mid-line reset: a hint picker strips SGR before matching
    // (docs/explanation/data-model/scrollback.md).
    let rows = vec![
        (b"\x1b[31mfoo\x1b[0m".to_vec(), false),
        (b"\x1b[31mbar\x1b[0m".to_vec(), true),
    ];
    assert_eq!(
        join_rows(&rows),
        b"\x1b[31mfoo\x1b[0m\x1b[31mbar\x1b[0m\n".to_vec()
    );
}

#[test]
fn visible_stitches_a_soft_wrapped_line_in_plain_mode() {
    let mut grid = Grid::new(3, 8);
    let mut p = felis_vt::Parser::new();
    p.advance(&mut grid, b"/usr/local/x");
    let out = serialize_region(&grid, 0, RegionSource::Visible, false).unwrap();
    assert_eq!(out, b"/usr/local/x\n");
}

fn fill_scrollback(grid: &mut Grid, count: usize) {
    let mut p = felis_vt::Parser::new();
    for i in 0..count {
        p.advance(grid, format!("line{i}\r\n").as_bytes());
    }
}

#[test]
fn scrollback_position_names_the_line_at_the_top_of_the_window() {
    let mut grid = Grid::new(4, 20);
    fill_scrollback(&mut grid, 10);
    let sb_len = grid.scrollback().len();
    let rows = region_rows(&grid, 6, RegionSource::Scrollback, false).unwrap();
    let position = region_position(&grid, 6, RegionSource::Scrollback, &rows).unwrap();

    assert_eq!(position.top_line, u32::try_from(sb_len - 6 + 1).unwrap());
    let region = serialize_region(&grid, 6, RegionSource::Scrollback, false).unwrap();
    let text = String::from_utf8(region).unwrap();
    let opened_at = text.lines().nth(sb_len - 6).unwrap();
    let visible = serialize_region(&grid, 6, RegionSource::Visible, false).unwrap();
    let visible = String::from_utf8(visible).unwrap();
    assert_eq!(opened_at, visible.lines().next().unwrap());
}

#[test]
fn scrollback_position_at_the_live_bottom_is_the_first_live_row() {
    let mut grid = Grid::new(4, 20);
    fill_scrollback(&mut grid, 10);
    let sb_len = grid.scrollback().len();
    let rows = region_rows(&grid, 0, RegionSource::Scrollback, false).unwrap();
    let position = region_position(&grid, 0, RegionSource::Scrollback, &rows).unwrap();
    assert_eq!(position.top_line, u32::try_from(sb_len + 1).unwrap());
}

#[test]
fn visible_position_is_always_the_regions_first_line() {
    let mut grid = Grid::new(4, 20);
    fill_scrollback(&mut grid, 10);
    for viewport in [0, 2, 5] {
        let rows = region_rows(&grid, viewport, RegionSource::Visible, false).unwrap();
        let position = region_position(&grid, viewport, RegionSource::Visible, &rows).unwrap();
        assert_eq!(position.top_line, 1, "viewport {viewport}");
    }
}

#[test]
fn a_soft_wrapped_row_reports_the_head_of_its_logical_line() {
    let rows = vec![
        (b"first".to_vec(), false),
        (b"wrapped".to_vec(), false),
        (b"continuation".to_vec(), true),
        (b"third".to_vec(), false),
    ];
    assert_eq!(line_of(&rows, 0), 1);
    assert_eq!(line_of(&rows, 1), 2);
    assert_eq!(line_of(&rows, 2), 2, "continuation reports its line head");
    assert_eq!(line_of(&rows, 3), 3);
}

#[test]
fn a_row_past_the_trimmed_tail_clamps_to_the_last_line() {
    let rows = vec![(b"only".to_vec(), false)];
    assert_eq!(line_of(&rows, 9), 1);
}

#[test]
fn cursor_position_tracks_the_live_cursor_through_the_scrollback_region() {
    let mut grid = Grid::new(4, 20);
    fill_scrollback(&mut grid, 10);
    let mut p = felis_vt::Parser::new();
    p.advance(&mut grid, b"$ echo");
    let sb_len = grid.scrollback().len();
    let rows = region_rows(&grid, 6, RegionSource::Scrollback, false).unwrap();
    let position = region_position(&grid, 6, RegionSource::Scrollback, &rows).unwrap();

    assert_eq!(
        position.cursor_line,
        u32::try_from(sb_len + usize::from(grid.cursor().row) + 1).unwrap()
    );
    assert_eq!(position.cursor_column, 7);
}

#[test]
fn the_cursor_column_counts_a_stitched_wrap_from_the_logical_line_start() {
    let mut grid = Grid::new(3, 8);
    let mut p = felis_vt::Parser::new();
    p.advance(&mut grid, b"/usr/local/x");
    let rows = region_rows(&grid, 0, RegionSource::Visible, false).unwrap();
    let position = region_position(&grid, 0, RegionSource::Visible, &rows).unwrap();
    assert_eq!(position.cursor_line, 1);
    assert_eq!(position.cursor_column, 13, "8 wrapped + 4 typed, 1-based");
}

#[test]
fn an_empty_region_has_no_position() {
    let grid = Grid::new(4, 8);
    let rows = region_rows(&grid, 0, RegionSource::Visible, false).unwrap();
    assert!(region_position(&grid, 0, RegionSource::Visible, &rows).is_none());
}

#[test]
fn mark_range_sources_carry_no_position() {
    let mut grid = Grid::new(4, 20);
    drive_command_cycle(&mut grid, "$ ", "echo hi", &["hi"]);
    let (bytes, position) =
        serialize_region_positioned(&grid, 0, RegionSource::CommandOutput, false).unwrap();
    assert_eq!(bytes, b"hi\n");
    assert!(position.is_none());
}
