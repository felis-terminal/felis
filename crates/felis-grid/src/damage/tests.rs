use crate::test_support::{drive, screen_switches, scroll_ops};
use crate::*;
use felis_vt::Parser;

/// The daemon saves and restores Kitty placements on this event.
#[test]
fn entering_alt_screen_pushes_a_screen_switch_event() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h");
    let switches = screen_switches(&mut g);
    assert_eq!(switches, vec![ScreenSwitch::EnteredAlternate]);
}

#[test]
fn leaving_alt_screen_pushes_a_screen_switch_event() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h\x1b[?1049l");
    let switches = screen_switches(&mut g);
    assert_eq!(
        switches,
        vec![ScreenSwitch::EnteredAlternate, ScreenSwitch::LeftAlternate,],
    );
}

/// A duplicate would make the daemon double-save placements and
/// clobber the saved primary.
#[test]
fn redundant_alt_screen_entry_does_not_push_an_extra_event() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049h\x1b[?1049h");
    let switches = screen_switches(&mut g);
    assert_eq!(switches, vec![ScreenSwitch::EnteredAlternate]);
}

/// Some programs emit `?1049l` on cleanup regardless of state.
#[test]
fn redundant_alt_screen_leave_does_not_push_an_event() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?1049l");
    let switches = screen_switches(&mut g);
    assert!(switches.is_empty(), "got: {switches:?}");
}

#[test]
fn synchronized_output_mode_toggles_via_2026() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 3);
    assert!(!g.synchronized_output());
    drive(&mut p, &mut g, b"\x1b[?2026h");
    assert!(g.synchronized_output());
    drive(&mut p, &mut g, b"\x1b[?2026l");
    assert!(!g.synchronized_output());
}

#[test]
fn synchronized_output_holds_writes_into_one_damage_burst() {
    // The grid still mutates under sync; holding frame emission is the
    // daemon's job.
    let mut p = Parser::new();
    let mut g = Grid::new(3, 6);
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[?2026h");
    drive(&mut p, &mut g, b"first\r\nnext");
    drive(&mut p, &mut g, b"\x1b[?2026l");
    assert!(!g.synchronized_output());
    let dirty: Vec<_> = g.damage().dirty_rows().collect();
    assert!(dirty.contains(&0));
    assert!(dirty.contains(&1));
}

#[test]
fn mark_range_matches_per_row_mark() {
    for rows in [1usize, 7, 64, 65, 130] {
        for start in 0..rows {
            for end in start..=rows {
                let mut bulk = Damage::default();
                bulk.resize(rows);
                bulk.clear();
                let mut per_row = Damage::default();
                per_row.resize(rows);
                per_row.clear();
                bulk.mark_range(start, end);
                for r in start..end {
                    per_row.mark(r);
                }
                assert_eq!(
                    bulk.dirty_rows().collect::<Vec<_>>(),
                    per_row.dirty_rows().collect::<Vec<_>>(),
                    "rows={rows} start={start} end={end}"
                );
            }
        }
    }
    // Out-of-range end clamps to `len`; start >= end is a no-op.
    let mut d = Damage::default();
    d.resize(10);
    d.clear();
    d.mark_range(5, 100);
    assert_eq!(
        d.dirty_rows().collect::<Vec<_>>(),
        (5..10).collect::<Vec<_>>()
    );
    d.clear();
    d.mark_range(8, 3);
    assert!(d.dirty_rows().next().is_none());
}

/// The scroll-aware wire shape of `docs/reference/ipc.md`: the shift
/// travels as a directive, so only the blanked band is dirty.
#[test]
fn pure_scroll_up_marks_only_the_blanked_band() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[2;5r");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[2S");
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![3, 4]);
    assert_eq!(
        scroll_ops(&mut g),
        vec![ScrollOp {
            region_top: 1,
            region_bottom: 4,
            n_rows: 2,
            direction: ScrollDirection::Up,
        }]
    );
}

#[test]
fn pure_scroll_down_marks_only_the_blanked_band_at_the_top() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[2;5r");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[2T");
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(
        scroll_ops(&mut g),
        vec![ScrollOp {
            region_top: 1,
            region_bottom: 4,
            n_rows: 2,
            direction: ScrollDirection::Down,
        }]
    );
}

/// A row written before the scroll is still owed after it, one band
/// higher; the rows nobody wrote ride the directive.
#[test]
fn a_scroll_over_a_written_row_moves_its_mark_and_ships_the_directive() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[2;5r");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[5;1Hdirt");
    drive(&mut p, &mut g, b"\x1b[2S");
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![2, 3, 4]);
    assert_eq!(
        scroll_ops(&mut g),
        vec![ScrollOp {
            region_top: 1,
            region_bottom: 4,
            n_rows: 2,
            direction: ScrollDirection::Up,
        }]
    );
}

/// DL is a scroll of the band from the cursor row to the bottom margin;
/// rows outside the band stay clean.
#[test]
fn dl_ships_a_directive_for_the_band_below_the_cursor() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[1;5r");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[3;1H\x1b[2M");
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![3, 4]);
    assert_eq!(
        scroll_ops(&mut g),
        vec![ScrollOp {
            region_top: 2,
            region_bottom: 4,
            n_rows: 2,
            direction: ScrollDirection::Up,
        }]
    );
}

#[test]
fn il_ships_a_downward_directive_for_the_band_below_the_cursor() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[2;1H");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[L");
    assert_eq!(g.damage().dirty_rows().collect::<Vec<_>>(), vec![1]);
    assert_eq!(
        scroll_ops(&mut g),
        vec![ScrollOp {
            region_top: 1,
            region_bottom: 5,
            n_rows: 1,
            direction: ScrollDirection::Down,
        }]
    );
}

/// Line feeds between drains queue one directive for their summed shift.
#[test]
fn adjacent_scrolls_of_one_band_coalesce_up_to_its_height() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[4;1H");
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"a\nb\nc\n");
    let full = |n_rows| ScrollOp {
        region_top: 0,
        region_bottom: 3,
        n_rows,
        direction: ScrollDirection::Up,
    };
    assert_eq!(scroll_ops(&mut g), vec![full(3)]);
    drive(&mut p, &mut g, b"\n\n\n\n\n");
    assert_eq!(scroll_ops(&mut g), vec![full(4)]);
}

/// The shadow has no partial-row shift, so no directive can carry it.
#[test]
fn subrect_scroll_under_declrmm_marks_every_region_row() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?69h\x1b[3;6s\x1b[2;5r");
    g.damage_mut().clear();
    let _drained = scroll_ops(&mut g);
    drive(&mut p, &mut g, b"\x1b[2S");
    assert_eq!(
        g.damage().dirty_rows().collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(scroll_ops(&mut g), Vec::<ScrollOp>::new());
}

/// Per-row scan over a flag per row, read from [`Damage::dirty_rows`] once:
/// the exhaustive callers below ask about every `(start, end)` pair, so a
/// re-collected list would dominate their runtime.
fn dirty_flags(d: &Damage, rows: usize) -> Vec<bool> {
    let mut flags = vec![false; rows];
    for r in d.dirty_rows() {
        flags[r] = true;
    }
    flags
}

fn naive_any_dirty(flags: &[bool], start: usize, end: usize) -> bool {
    (start..end).any(|r| flags[r])
}

#[test]
fn any_dirty_in_range_matches_a_per_row_scan() {
    for rows in [1usize, 7, 64, 65, 130] {
        for dirty_row in 0..rows {
            let mut d = Damage::new(rows);
            d.clear();
            d.mark(dirty_row);
            let flags = dirty_flags(&d, rows);
            for start in 0..rows {
                for end in start..=rows {
                    assert_eq!(
                        d.any_dirty_in_range(start, end),
                        naive_any_dirty(&flags, start, end),
                        "rows={rows} dirty={dirty_row} start={start} end={end}",
                    );
                }
            }
        }
    }
}

#[test]
fn any_dirty_in_range_matches_a_per_row_scan_with_a_dense_set() {
    for rows in [65usize, 130] {
        let mut d = Damage::new(rows);
        d.clear();
        for r in (0..rows).step_by(2) {
            d.mark(r);
        }
        let flags = dirty_flags(&d, rows);
        for start in 0..rows {
            for end in start..=rows {
                assert_eq!(
                    d.any_dirty_in_range(start, end),
                    naive_any_dirty(&flags, start, end),
                    "rows={rows} start={start} end={end}",
                );
            }
        }
    }
}

/// A band filling a whole word, shifted by its full height, vacates
/// every row without a shift of 64 bits.
#[test]
fn shifting_a_full_word_band_by_its_height_marks_every_row() {
    for direction in [ScrollDirection::Up, ScrollDirection::Down] {
        let mut d = Damage::new(64);
        d.mark(5);
        d.shift_band(0, 63, 64, direction);
        assert!(d.all_dirty_in_range(0, 64), "{direction:?}");
    }
}

/// DECSTBM moves the margins and the cursor, neither of which is cell
/// content; marking the screen would turn vim's DECSTBM + DL scroll
/// into a replay of every row.
#[test]
fn setting_the_scroll_margins_marks_no_row() {
    let mut g = Grid::new(6, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"row");
    g.damage_mut().clear();
    drive(&mut p, &mut g, b"\x1b[2;5r\x1b[r");
    assert_eq!(g.damage().dirty_rows().count(), 0);
}
