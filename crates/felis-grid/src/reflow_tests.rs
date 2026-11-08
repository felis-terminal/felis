//! [`Grid::reflow`] against an oracle that holds one buffer per row, the
//! plain shape [`PackedRows`] flattens: the grid and the returned
//! [`ReflowRemap`] must agree in every field.

use felis_vt::Parser;
use proptest::prelude::*;

use crate::test_support::drive;
use crate::*;

fn packed(rows: &[Vec<Cell>]) -> PackedRows {
    let mut out = PackedRows::default();
    for row in rows {
        out.push(row);
    }
    out
}

impl Grid {
    fn reflow_reference(&mut self, rows: u16, cols: u16) -> Option<ReflowRemap> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.screen.rows && cols == self.screen.cols {
            return None;
        }
        if self.screen.on_alternate_screen() {
            self.resize(rows, cols);
            return None;
        }
        self.pending_bidi.discard();

        let was_full_vertical =
            self.margins.top == 0 && self.margins.bottom + 1 == self.screen.rows;
        let was_full_horizontal =
            self.margins.left == 0 && self.margins.right + 1 == self.screen.cols;

        let sb_len = self.screen.scrollback().len();
        let base_abs = self
            .scrollback_total_pushed
            .saturating_sub(u64::try_from(sb_len).unwrap_or(u64::MAX));

        let old_rows = self.screen.rows;
        let old_cols = self.screen.cols;
        let old_cursor_col = self.screen.cursor.col;
        let blank = self.prompt_redraw_rows();
        let mut live_extent = usize::from(self.screen.cursor.row) + 1;
        for r in (0..old_rows).rev() {
            if blank.as_ref().is_some_and(|b| b.contains(&r)) {
                continue;
            }
            let row = self.screen.row_content(r).unwrap_or(&[]);
            if row.iter().any(|c| *c != Cell::default()) {
                live_extent = live_extent.max(usize::from(r) + 1);
                break;
            }
        }
        let mut combined: Vec<Vec<Cell>> = Vec::with_capacity(sb_len + live_extent);
        let mut continued: Vec<bool> = Vec::with_capacity(sb_len + live_extent);
        let old_cols_usize = usize::from(old_cols);
        for (i, (cells, cont)) in self
            .screen
            .rows_with_wrap(AltScreenRows::Include)
            .take(sb_len + live_extent)
            .enumerate()
        {
            let screen_row = i.checked_sub(sb_len).and_then(|r| u16::try_from(r).ok());
            if screen_row.is_some_and(|r| blank.as_ref().is_some_and(|b| b.contains(&r))) {
                combined.push(vec![Cell::default(); old_cols_usize]);
                continued.push(false);
                continue;
            }
            let blank_end = blank.as_ref().map(|b| b.end);
            let cont = cont && (blank_end.is_none() || screen_row != blank_end);
            let mut row = cells.to_vec();
            if i < sb_len {
                row.resize(old_cols_usize, Cell::default());
            }
            combined.push(row);
            continued.push(cont);
        }

        let spans = logical_line_spans(&continued);
        let mut line_of_row: Vec<usize> = vec![0; combined.len()];
        for (li, &(start, end)) in spans.iter().enumerate() {
            for slot in &mut line_of_row[start..=end] {
                *slot = li;
            }
        }

        let cursor_combined = sb_len + usize::from(self.screen.cursor.row);
        let cursor_line = line_of_row.get(cursor_combined).copied().unwrap_or(0);
        let cursor_run_index = {
            let (start, end) = spans[cursor_line];
            let mut j = 0usize;
            for k in start..cursor_combined {
                let contribution = if k < end {
                    self.nontail_contribution(&combined[k], &combined[k + 1])
                        .len()
                } else {
                    combined[k].len()
                };
                j += contribution;
            }
            j + usize::from(self.screen.cursor.col)
        };

        let mut out_rows: Vec<Vec<Cell>> = Vec::with_capacity(combined.len());
        let mut out_continued: Vec<bool> = Vec::with_capacity(combined.len());
        let mut new_line_start: Vec<usize> = Vec::with_capacity(spans.len());
        let mut new_row_of_old: Vec<usize> = Vec::with_capacity(combined.len());
        let mut row_starts: Vec<usize> = Vec::new();
        let mut dropped: Vec<usize> = Vec::new();
        let mut cursor_phys = (0usize, 0usize);
        let blocks = self.whole_sized_blocks(&packed(&combined));
        let mut landing = Landing::default();
        for (li, &(start, end)) in spans.iter().enumerate() {
            new_line_start.push(out_rows.len());
            let run = self.stitch_logical_line_reference(&combined, start, end);
            let line_first = out_rows.len();
            let target = (li == cursor_line).then_some(cursor_run_index);
            row_starts.clear();
            dropped.clear();
            if let Some((lr, lc)) = self.rewrap_run_reference(
                &run,
                cols,
                &mut out_rows,
                &mut out_continued,
                target,
                &mut row_starts,
                &mut dropped,
            ) {
                cursor_phys = (line_first + lr, lc);
            }
            let mut off = 0usize;
            for k in start..=end {
                let within = row_starts.partition_point(|&s| s <= off).saturating_sub(1);
                new_row_of_old.push(line_first + within);
                let len = if k < end {
                    self.nontail_contribution(&combined[k], &combined[k + 1])
                        .len()
                } else {
                    combined[k].len()
                };
                landing.rows.push(LandedRow { line: li, off, len });
                off += len;
            }
            landing.lines.push(LandedLine {
                first_row: line_first,
                row_starts: row_starts.clone(),
                dropped: dropped.clone(),
            });
        }
        for &(k, c, scale, block_w) in &blocks {
            let cells = (0..scale).flat_map(|dr| (0..block_w).map(move |dc| (k + dr, c + dc)));
            let primary = landing.at(k, c);
            let moved_whole = primary.is_some_and(|(pr, pc)| {
                cells
                    .clone()
                    .all(|(ok, oc)| landing.at(ok, oc) == Some((pr + ok - k, pc + oc - c)))
            });
            if !moved_whole {
                for (r, c) in cells.filter_map(|(ok, oc)| landing.at(ok, oc)) {
                    self.drop_cell_sizing(&mut out_rows[r], c);
                }
            }
        }
        let total_new = out_rows.len();

        let win_start = total_new.saturating_sub(usize::from(rows));

        let cols_usize = usize::from(cols);
        let rows_usize = usize::from(rows);
        let history: Vec<(Vec<Cell>, bool)> = (0..win_start)
            .map(|phys| {
                let row = &out_rows[phys];
                let occ = row
                    .iter()
                    .rposition(|c| *c != Cell::default())
                    .map_or(0, |i| i + 1);
                (row[..occ].to_vec(), out_continued[phys])
            })
            .collect();
        let mut viewport_cells = vec![Cell::default(); rows_usize * cols_usize];
        let mut viewport_soft_wrap = vec![false; rows_usize];
        for r in 0..rows_usize {
            let phys = win_start + r;
            if phys < total_new {
                viewport_cells[r * cols_usize..(r + 1) * cols_usize]
                    .copy_from_slice(&out_rows[phys]);
                viewport_soft_wrap[r] = out_continued[phys];
            }
        }
        self.screen.install_ring(
            rows,
            cols,
            history.iter().map(|(row, wrap)| (row.as_slice(), *wrap)),
            &viewport_cells,
            &viewport_soft_wrap,
        );

        let cursor_row = cursor_phys.0.saturating_sub(win_start).min(rows_usize - 1);
        self.screen.cursor.row = u16::try_from(cursor_row).unwrap_or(rows - 1);
        self.screen.cursor.col = u16::try_from(cursor_phys.1)
            .unwrap_or(cols - 1)
            .min(cols - 1);
        if blank.is_some() {
            self.screen.cursor.col = old_cursor_col.min(cols - 1);
        }
        self.screen.cursor.pending_wrap = false;

        self.scrollback_total_pushed =
            base_abs.saturating_add(u64::try_from(win_start).unwrap_or(u64::MAX));
        for mark in &mut self.prompt_marks {
            let Some(j) = mark.line.checked_sub(base_abs) else {
                continue;
            };
            let Ok(j) = usize::try_from(j) else { continue };
            if let Some(&li) = line_of_row.get(j) {
                mark.line =
                    base_abs.saturating_add(u64::try_from(new_line_start[li]).unwrap_or(u64::MAX));
            }
        }
        self.shell_prompt
            .remap_output_start(base_abs, &continued, |j| {
                line_of_row.get(j).map(|&li| new_line_start[li])
            });
        self.prune_evicted_marks();

        self.screen.retrack_geometry(rows, cols);
        self.finish_geometry_change(rows, cols, was_full_vertical, was_full_horizontal);

        Some(ReflowRemap {
            old_sb_len: sb_len,
            new_win_start: win_start,
            new_total: total_new,
            new_sb_len: self.screen.scrollback().len(),
            new_row_of_old,
            cleared: blank.map(|b| sb_len + usize::from(b.start)..sb_len + usize::from(b.end)),
        })
    }

    fn stitch_logical_line_reference(
        &self,
        combined: &[Vec<Cell>],
        start: usize,
        end: usize,
    ) -> Vec<Cell> {
        let mut run: Vec<Cell> = Vec::new();
        for k in start..=end {
            if k < end {
                run.extend_from_slice(self.nontail_contribution(&combined[k], &combined[k + 1]));
            } else {
                run.extend_from_slice(&combined[k]);
            }
        }
        while run.last().is_some_and(|c| *c == Cell::default()) {
            run.pop();
        }
        run
    }

    fn rewrap_run_reference(
        &self,
        run: &[Cell],
        cols: u16,
        out_rows: &mut Vec<Vec<Cell>>,
        out_continued: &mut Vec<bool>,
        cursor_target: Option<usize>,
        row_starts: &mut Vec<usize>,
        dropped: &mut Vec<usize>,
    ) -> Option<(usize, usize)> {
        let cols_usize = usize::from(cols);
        let mut cur: Vec<Cell> = Vec::with_capacity(cols_usize);
        let mut rows_pushed = 0usize;
        let mut first = true;
        let mut cursor_pos = None;
        let mut i = 0usize;
        row_starts.push(0);
        while i < run.len() {
            let cell = run[i];
            let has_spacer = i + 1 < run.len() && matches!(run[i + 1].grapheme, Grapheme::Spacer);
            if has_spacer && self.screen.grapheme_width(cell.grapheme) == 2 {
                if cols_usize < 2 {
                    dropped.push(i);
                    i += 2;
                    continue;
                }
                if cur.len() + 2 > cols_usize {
                    pad_and_push_reference(
                        &mut cur,
                        cols_usize,
                        out_rows,
                        out_continued,
                        &mut first,
                    );
                    rows_pushed += 1;
                    row_starts.push(i);
                }
                if cursor_target == Some(i) {
                    cursor_pos = Some((rows_pushed, cur.len()));
                }
                if cursor_target == Some(i + 1) {
                    cursor_pos = Some((rows_pushed, cur.len() + 1));
                }
                cur.push(cell);
                cur.push(run[i + 1]);
                i += 2;
            } else {
                if cur.len() + 1 > cols_usize {
                    pad_and_push_reference(
                        &mut cur,
                        cols_usize,
                        out_rows,
                        out_continued,
                        &mut first,
                    );
                    rows_pushed += 1;
                    row_starts.push(i);
                }
                if cursor_target == Some(i) {
                    cursor_pos = Some((rows_pushed, cur.len()));
                }
                cur.push(cell);
                i += 1;
            }
        }
        let last_col = cur.len();
        pad_and_push_reference(&mut cur, cols_usize, out_rows, out_continued, &mut first);
        if cursor_target.is_some() && cursor_pos.is_none() {
            cursor_pos = Some((rows_pushed, last_col.min(cols_usize.saturating_sub(1))));
        }
        cursor_pos
    }
}

fn pad_and_push_reference(
    cur: &mut Vec<Cell>,
    cols: usize,
    out_rows: &mut Vec<Vec<Cell>>,
    out_continued: &mut Vec<bool>,
    first: &mut bool,
) {
    cur.resize(cols, Cell::default());
    out_rows.push(std::mem::take(cur));
    out_continued.push(!*first);
    *first = false;
}

#[derive(Clone, Debug)]
enum Step {
    Bytes(Vec<u8>),
    Reflow(u16, u16),
    Compact(usize),
}

fn token() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "a",
        "b",
        "xyz",
        " ",
        "\r\n",
        "\n",
        "\r",
        "\t",
        "\x08",
        "端",
        "末",
        "👍",
        "e\u{301}",
        "\x1b[31m",
        "\x1b[0m",
        "\x1b[44m\x1b[K",
        "\x1b[44m\x1b[2J",
        "\x1b[5G",
        "\x1b[2A",
        "\x1b[H",
        "\x1b[3;2H",
        "\x1b]8;;http://x\x07",
        "\x1b]8;;\x07",
        "\x1b]66;s=2;AB\x07",
        "\x1b]66;s=2:w=3;A\x07",
        "\x1b]66;w=2;x\x07",
        "\x1b]133;A\x07",
        "\x1b]133;A;redraw=last\x07",
        "\x1b]133;A;redraw=0\x07",
        "\x1b]133;B\x07",
        "\x1b]133;C\x07",
        "\x1b]133;D;0\x07",
        "\x1b[?1049h",
        "\x1b[?1049l",
        "\x1b[?7l",
        "\x1b[?7h",
    ])
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => proptest::collection::vec(token(), 1..24)
            .prop_map(|ts| Step::Bytes(ts.concat().into_bytes())),
        1 => proptest::collection::vec(any::<u8>(), 1..32).prop_map(Step::Bytes),
        2 => (prop::sample::select(vec!["", ";redraw=last", ";redraw=0"]), token())
            .prop_map(|(redraw, prompt)| Step::Bytes(format!(
                "\x1b]133;C\x07out\r\n\x1b]133;D;0\x07\x1b]133;A{redraw}\x07$ {prompt}{prompt}\x1b]133;B\x07"
            ).into_bytes())),
        2 => (1u16..=8, 1u16..=24).prop_map(|(r, c)| Step::Reflow(r, c)),
        1 => (1usize..=32).prop_map(Step::Compact),
    ]
}

proptest! {
    #[test]
    fn reflow_matches_the_per_row_reference(
        rows in 1u16..=8,
        cols in 1u16..=24,
        scrollback in 0usize..=24,
        steps in proptest::collection::vec(step(), 1..24),
    ) {
        let mut parser = Parser::new();
        let mut grid = Grid::with_scrollback(rows, cols, scrollback);
        for step in &steps {
            match step {
                Step::Bytes(bytes) => drive(&mut parser, &mut grid, bytes),
                Step::Compact(n) => {
                    grid.compact_history(*n);
                }
                Step::Reflow(r, c) => {
                    let mut reference = grid.clone();
                    let expected = reference.reflow_reference(*r, *c);
                    let got = grid.reflow(*r, *c);
                    prop_assert_eq!(got, expected, "remap diverged after {:?}", steps);
                    prop_assert!(grid == reference, "grid diverged after {:?}", steps);
                }
            }
        }
    }
}
