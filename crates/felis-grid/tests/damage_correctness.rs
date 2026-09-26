//! Damage-tracking correctness harness (`docs/reference/testing.md`).
//! Asserts no-underdraw contract (`docs/explanation/rendering/damage-tracking.md` "Correctness"):
//! every cell whose content differs from the prior content shifted by the
//! pending scroll directives must have its row marked damaged; overdraw is
//! permitted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use felis_grid::{Cell, Grid, PtyEffect, ScrollDirection};
use felis_vt::Parser;
use proptest::prelude::*;

fn snapshot_cells(g: &Grid) -> Vec<Vec<Cell>> {
    (0..g.rows())
        .map(|r| g.row_content(r).unwrap_or(&[]).to_vec())
        .collect()
}

fn drive(g: &mut Grid, bytes: &[u8]) {
    g.damage_mut().clear();
    drop(g.take_pty_effects());
    let mut p = Parser::new();
    p.advance(g, bytes);
}

/// What a client holding `before` sees once it applies the queued
/// `Scrolled` directives (`docs/reference/ipc.md`): whole rows move and
/// the vacated ones read as default blanks.
fn shifted_by_directives(before: &[Vec<Cell>], g: &mut Grid) -> Vec<Vec<Cell>> {
    let mut rows = before.to_vec();
    for effect in g.take_pty_effects() {
        let PtyEffect::Scrolled { op, .. } = effect else {
            continue;
        };
        let band = &mut rows[usize::from(op.region_top)..=usize::from(op.region_bottom)];
        let n = usize::from(op.n_rows);
        match op.direction {
            ScrollDirection::Up => {
                band.rotate_left(n);
                let len = band.len();
                band[len - n..].fill(Vec::new());
            }
            ScrollDirection::Down => {
                band.rotate_right(n);
                band[..n].fill(Vec::new());
            }
        }
    }
    rows
}

fn assert_no_underdraw(before: &[Vec<Cell>], g: &mut Grid, label: &str) {
    let dirty: HashSet<usize> = g.damage().dirty_rows().collect();
    let shifted = shifted_by_directives(before, g);
    for (r, shifted_row) in shifted.iter().enumerate() {
        if dirty.contains(&r) {
            continue;
        }
        let after_row = g.row_content(r as u16).unwrap_or(&[]);
        let width = shifted_row.len().max(after_row.len());
        for c in 0..width {
            let pre = shifted_row.get(c).copied().unwrap_or_default();
            let post = after_row.get(c).copied().unwrap_or_default();
            assert!(
                pre == post,
                "{label}: UNDERDRAW — cell ({r}, {c}) differs from the \
                 scroll-shifted prior content but row {r} is not in the \
                 damage set. shifted={pre:?} post={post:?}",
            );
        }
    }
}

#[test]
fn single_cell_sgr_change_marks_the_row_dirty() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"helloXX\rworld");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[31mA");
    assert_no_underdraw(&before, &mut g, "single_cell_sgr_change");
}

#[test]
fn single_cell_glyph_change_marks_the_row_dirty() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"hello");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[1;3HZ");
    assert_no_underdraw(&before, &mut g, "single_cell_glyph_change");
}

#[test]
fn single_row_scroll_push_marks_shifted_rows() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"row1\r\nrow2\r\nrow3\r\nrow4");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[4;1H\n");
    assert_no_underdraw(&before, &mut g, "single_row_scroll_push");
}

#[test]
fn multi_row_scroll_push_via_su_marks_shifted_rows() {
    let mut g = Grid::new(6, 8);
    drive(&mut g, b"r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[3S");
    assert_no_underdraw(&before, &mut g, "multi_row_scroll_push_su");
}

#[test]
fn ed_2_whole_screen_erase_marks_every_changed_row() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"r1\r\nr2\r\nr3\r\nr4");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[2J");
    assert_no_underdraw(&before, &mut g, "ed_2_whole_screen_erase");
}

#[test]
fn el_0_marks_the_cursor_row() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"hello123");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[1;6H\x1b[0K");
    assert_no_underdraw(&before, &mut g, "el_0_cursor_row");
}

#[test]
fn il_insert_line_marks_shifted_rows() {
    let mut g = Grid::new(5, 8);
    drive(&mut g, b"r1\r\nr2\r\nr3\r\nr4\r\nr5");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[2;1H\x1b[L");
    assert_no_underdraw(&before, &mut g, "il_insert_line");
}

#[test]
fn dl_delete_line_marks_shifted_rows() {
    let mut g = Grid::new(5, 8);
    drive(&mut g, b"r1\r\nr2\r\nr3\r\nr4\r\nr5");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[2;1H\x1b[M");
    assert_no_underdraw(&before, &mut g, "dl_delete_line");
}

#[test]
fn ich_insert_char_marks_the_cursor_row() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"abcdefgh");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[1;3H\x1b[2@");
    assert_no_underdraw(&before, &mut g, "ich_insert_char");
}

#[test]
fn dch_delete_char_marks_the_cursor_row() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"abcdefgh");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\x1b[1;3H\x1b[2P");
    assert_no_underdraw(&before, &mut g, "dch_delete_char");
}

#[test]
fn sync_output_release_marks_every_buffered_change() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"old1\r\nold2\r\nold3\r\nold4");
    let before = snapshot_cells(&g);
    drive(
        &mut g,
        b"\x1b[?2026h\
          \x1b[1;1HNEW1\
          \x1b[2;1HNEW2\
          \x1b[3;1HNEW3\
          \x1b[?2026l",
    );
    assert_no_underdraw(&before, &mut g, "sync_output_release");
}

#[test]
fn cursor_only_moves_leave_the_damage_empty() {
    let moves: &[&[u8]] = &[
        b"\r",
        b"\n",
        b"\x1bD",
        b"\x1bM",
        b"\x08",
        b"\t",
        b"\x1b[3;4H",
        b"\x1b[2A",
        b"\x1b[2B",
        b"\x1b[2C",
        b"\x1b[2D",
        b"\x1b[5G",
        b"\x1b[3d",
        b"\x1b[2I",
        b"\x1b[2Z",
        b"\x1b7\x1b[4;8H\x1b8",
        b"\x1b[2;3r",
        b"\x1b[?6h\x1b[?6l",
        b"\x1b[?69h\x1b[2;5s",
        b"\x1b[?7h\x1b[?45h\x1b[2;1H\x08",
        b"\x1b[?7h\x1b[?1045h\x1b[1;1H\x08",
    ];
    for &bytes in moves {
        let mut g = Grid::new(4, 8);
        drive(&mut g, b"hello\r\nworld\x1b[2;3H");
        let before = snapshot_cells(&g);
        drive(&mut g, bytes);
        let dirty: Vec<usize> = g.damage().dirty_rows().collect();
        assert!(dirty.is_empty(), "{bytes:?} marked rows {dirty:?}");
        assert_eq!(before, snapshot_cells(&g), "{bytes:?} changed a cell");
    }
}

#[test]
fn a_line_feed_at_the_bottom_owes_only_the_row_it_vacated() {
    let mut g = Grid::new(4, 8);
    drive(&mut g, b"r1\r\nr2\r\nr3\r\nr4");
    let before = snapshot_cells(&g);
    drive(&mut g, b"\r\nr5");
    let dirty: Vec<usize> = g.damage().dirty_rows().collect();
    assert_eq!(dirty, [3]);
    assert_no_underdraw(&before, &mut g, "line_feed_at_the_bottom");
}

/// Motion that neither writes a cell nor reaches a margin a line feed
/// would scroll at.
fn cursor_move_strategy() -> impl Strategy<Value = Vec<u8>> {
    let atom = prop_oneof![
        Just(b"\r".to_vec()),
        Just(b"\x08".to_vec()),
        Just(b"\t".to_vec()),
        Just(b"\x1b7".to_vec()),
        Just(b"\x1b8".to_vec()),
        (1u8..=6, 1u8..=8).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
        (
            1u8..=9,
            prop_oneof![
                Just('A'),
                Just('B'),
                Just('C'),
                Just('D'),
                Just('G'),
                Just('d'),
                Just('I'),
                Just('Z'),
                Just('`'),
                Just('a'),
                Just('e')
            ]
        )
            .prop_map(|(n, op)| format!("\x1b[{n}{op}").into_bytes()),
        (1u8..=6, 1u8..=6).prop_map(|(t, b)| format!("\x1b[{t};{b}r").into_bytes()),
        (1u8..=8, 1u8..=8).prop_map(|(l, r)| format!("\x1b[{l};{r}s").into_bytes()),
        prop_oneof![Just(6u16), Just(7), Just(45), Just(69), Just(1045)].prop_flat_map(
            |m| prop_oneof![
                Just(format!("\x1b[?{m}h").into_bytes()),
                Just(format!("\x1b[?{m}l").into_bytes()),
            ]
        ),
    ];
    proptest::collection::vec(atom, 0..32).prop_map(|chunks| chunks.concat())
}

fn shell_byte_strategy() -> impl Strategy<Value = Vec<u8>> {
    let atom = prop_oneof![
        (b'A'..=b'z').prop_map(|b| vec![b]),
        Just(vec![b'\r']),
        Just(vec![b'\n']),
        Just(vec![0x08]),
        Just(vec![0x07]),
        (0u8..=2).prop_map(|n| {
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(format!("{n}").as_bytes());
            v.push(b'J');
            v
        }),
        (0u8..=2).prop_map(|n| {
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(format!("{n}").as_bytes());
            v.push(b'K');
            v
        }),
        // Row / col ranges match the 6x8 grid in the property test.
        (1u8..=6, 1u8..=8).prop_map(|(r, c)| {
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(format!("{r};{c}").as_bytes());
            v.push(b'H');
            v
        }),
        (
            1u8..=4,
            prop_oneof![
                Just(b'S'),
                Just(b'T'),
                Just(b'L'),
                Just(b'M'),
                Just(b'@'),
                Just(b'P')
            ]
        )
            .prop_map(|(n, op)| {
                let mut v = vec![0x1b, b'['];
                v.extend_from_slice(format!("{n}").as_bytes());
                v.push(op);
                v
            }),
        prop_oneof![0u8..=8, 41u8..=43].prop_map(|n| {
            let mut v = vec![0x1b, b'['];
            v.extend_from_slice(format!("{n}").as_bytes());
            v.push(b'm');
            v
        }),
        (1u8..=6, 1u8..=6).prop_map(|(t, b)| format!("\x1b[{t};{b}r").into_bytes()),
        Just(b"\x1bM".to_vec()),
        Just(b"\x1bD".to_vec()),
    ];
    proptest::collection::vec(atom, 0..32).prop_map(|chunks| {
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&c);
        }
        out
    })
}

proptest! {
    #[test]
    fn underdraw_never_happens_under_arbitrary_input(
        seed in shell_byte_strategy(),
        follow_up in shell_byte_strategy(),
    ) {
        let mut g = Grid::new(6, 8);
        // An empty grid masks handlers that only misbehave when they move
        // content.
        let mut p = Parser::new();
        p.advance(&mut g, &seed);
        g.damage_mut().clear();
        drop(g.take_pty_effects());

        let before = snapshot_cells(&g);
        let mut p2 = Parser::new();
        p2.advance(&mut g, &follow_up);

        assert_no_underdraw(&before, &mut g, &format!("follow_up = {follow_up:?}"));
    }
}

proptest! {
    #[test]
    fn cursor_motion_marks_no_row(
        seed in shell_byte_strategy(),
        moves in cursor_move_strategy(),
    ) {
        let mut g = Grid::new(6, 8);
        let mut p = Parser::new();
        p.advance(&mut g, &seed);
        g.damage_mut().clear();
        drop(g.take_pty_effects());
        let before = snapshot_cells(&g);
        let mut p2 = Parser::new();
        p2.advance(&mut g, &moves);
        let dirty: Vec<usize> = g.damage().dirty_rows().collect();
        prop_assert!(dirty.is_empty(), "{:?} marked rows {:?}", moves, dirty);
        prop_assert_eq!(before, snapshot_cells(&g));
    }
}
