//! Cross-handler scenario snapshots: shell prompts (OSC title + cwd + 133
//! marks + SGR), alt-screen cycles, pager repaints, synchronized-output
//! bursts. The per-handler snapshots live in `snapshot_csi.rs` /
//! `snapshot_osc.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::Grid;

mod common;
use common::{drive, styled_cell_lines, write_cells, write_cursor};

fn render(grid: &Grid) -> String {
    let mut out = String::new();
    write_cursor(&mut out, grid);
    writeln!(out, "alt_screen: {}", grid.on_alternate_screen() as u8).unwrap();
    if let Some(title) = grid.title() {
        writeln!(out, "title: {title:?}").unwrap();
    }
    if let Some(cwd) = grid.cwd() {
        writeln!(out, "cwd: {cwd:?}").unwrap();
    }
    write_cells(&mut out, grid);
    let attr_lines = styled_cell_lines(grid);
    if !attr_lines.is_empty() {
        writeln!(out, "attrs:").unwrap();
        for line in attr_lines {
            writeln!(out, "{line}").unwrap();
        }
    }
    out
}

#[test]
fn shell_prompt_with_title_cwd_and_osc_133_marks() {
    let g = drive(
        3,
        20,
        b"\x1b]0;~/proj/felis\x07\
          \x1b]7;file:///home/u/proj/felis\x07\
          \x1b]133;A\x07\
          \x1b[1;34muser@host\x1b[0m:\x1b[1;36m~\x1b[0m$ \
          \x1b]133;B\x07\
          ls\r\n\
          \x1b]133;C\x07\
          foo bar\r\n\
          \x1b]133;D;0\x07",
    );
    insta::assert_snapshot!(render(&g), @r#"
    cursor: row=2 col=0 visible=1 pending_wrap=0
    alt_screen: 0
    title: "~/proj/felis"
    cwd: "file:///home/u/proj/felis"
    grid 3x20:
    |user@host:~$ ls.....|
    |foo bar.............|
    |....................|
    attrs:
      (0,0): fg=idx4 bg=default flags=B
      (0,1): fg=idx4 bg=default flags=B
      (0,2): fg=idx4 bg=default flags=B
      (0,3): fg=idx4 bg=default flags=B
      (0,4): fg=idx4 bg=default flags=B
      (0,5): fg=idx4 bg=default flags=B
      (0,6): fg=idx4 bg=default flags=B
      (0,7): fg=idx4 bg=default flags=B
      (0,8): fg=idx4 bg=default flags=B
      (0,10): fg=idx6 bg=default flags=B
    "#);
}

#[test]
fn alt_screen_cycle_preserves_primary_buffer() {
    let g = drive(
        4,
        16,
        b"primary line 1\r\n\
          primary line 2\
          \x1b[?1049h\
          \x1b[2J\x1b[1;1Halt content\
          \x1b[?1049l",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=14 visible=1 pending_wrap=0
    alt_screen: 0
    grid 4x16:
    |primary line 1..|
    |primary line 2..|
    |................|
    |................|
    ");
}

#[test]
fn less_style_paging_via_cup_and_el_replaces_screen() {
    let g = drive(
        4,
        12,
        b"\x1b[1;1Hpage1 row1\x1b[K\r\n\
          page1 row2\x1b[K\r\n\
          page1 row3\x1b[K\r\n\
          page1 row4\x1b[K\
          \x1b[1;1Hpage2 row1\x1b[K\r\n\
          page2 row2\x1b[K\r\n\
          page2 row3\x1b[K\r\n\
          page2 row4\x1b[K",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=3 col=10 visible=1 pending_wrap=0
    alt_screen: 0
    grid 4x12:
    |page2 row1..|
    |page2 row2..|
    |page2 row3..|
    |page2 row4..|
    ");
}

/// Sync hides intermediate states, not the eventual layout.
#[test]
fn synchronized_output_release_lands_composed_state() {
    let g = drive(
        3,
        20,
        b"old line 1\r\nold line 2\r\nold line 3\
          \x1b[?2026h\
          \x1b[1;1HNEW_A\x1b[K\
          \x1b[2;1HNEW_B\x1b[K\
          \x1b[3;1HNEW_C\x1b[K\
          \x1b[?2026l",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=2 col=5 visible=1 pending_wrap=0
    alt_screen: 0
    grid 3x20:
    |NEW_A...............|
    |NEW_B...............|
    |NEW_C...............|
    ");
}

#[test]
fn clear_and_redraw_cycle_blanks_prior_attrs() {
    let g = drive(
        3,
        16,
        b"\x1b[1;1H\x1b[7mreverse text\x1b[0m\r\n\
          plain second\
          \x1b[2J\x1b[1;1H\
          \x1b[1mbold prompt$ \x1b[0m",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=0 col=13 visible=1 pending_wrap=0
    alt_screen: 0
    grid 3x16:
    |bold prompt$ ...|
    |................|
    |................|
    attrs:
      (0,0): fg=default bg=default flags=B
      (0,1): fg=default bg=default flags=B
      (0,2): fg=default bg=default flags=B
      (0,3): fg=default bg=default flags=B
      (0,4): fg=default bg=default flags=B
      (0,5): fg=default bg=default flags=B
      (0,6): fg=default bg=default flags=B
      (0,7): fg=default bg=default flags=B
      (0,8): fg=default bg=default flags=B
      (0,9): fg=default bg=default flags=B
      (0,10): fg=default bg=default flags=B
      (0,11): fg=default bg=default flags=B
      (0,12): fg=default bg=default flags=B
    ");
}

#[test]
fn osc_8_hyperlink_spans_only_the_bracketed_text() {
    let g = drive(
        2,
        20,
        b"before \x1b]8;;http://example.com\x1b\\link text\x1b]8;;\x1b\\ after",
    );
    insta::assert_snapshot!(render(&g), @r"
    cursor: row=1 col=2 visible=1 pending_wrap=0
    alt_screen: 0
    grid 2x20:
    |before link text aft|
    |er..................|
    ");
}
