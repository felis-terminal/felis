//! Region serialization behind the `Region` frame kind
//! (docs/explanation/data-model/scrollback.md).
//!
//! Materializes requested regions from scrollback or semantic marks to reply
//! frames (`RegionToClientMsg::Reply`). Selection regions remain client-owned.

use felis_grid::{AltScreenRows, Cell, ClusterTable, Grid, MarkLocation, PromptMark};
use felis_protocol::messages::{PromptKind, RegionPosition, RegionSource};

fn encode_row(
    cells: &[Cell],
    clusters: &ClusterTable,
    styles: &felis_grid::StyleTable,
    ansi: bool,
) -> Vec<u8> {
    if ansi {
        felis_grid::row_ansi(cells, clusters, styles)
    } else {
        felis_grid::row_text_trim(cells, clusters).into_bytes()
    }
}

/// Serialize a region of `grid`.
///
/// Rows join with `\n`, preserving soft-wrap continuations without newlines
/// ([`felis_grid::logical_lines`]). Returns `None` when a requested OSC 133
/// mark range does not exist.
#[must_use]
pub(crate) fn serialize_region(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    ansi: bool,
) -> Option<Vec<u8>> {
    let clusters = grid.cluster_table();
    match source {
        RegionSource::Scrollback | RegionSource::Visible => {
            Some(join_rows(&region_rows(grid, viewport, source, ansi)?))
        }
        RegionSource::CommandOutput => {
            serialize_mark_range(grid, PromptKind::OutputStart, clusters, ansi)
        }
        RegionSource::LastCommand => {
            serialize_mark_range(grid, PromptKind::InputStart, clusters, ansi)
        }
    }
}

/// [`serialize_region`] plus the pager start position ("Viewport
/// position" in docs/explanation/data-model/scrollback.md). The
/// position is `None` for the two `OSC 133` mark ranges: a mark range
/// is anchored to a command, not to where the user is looking, so the
/// variables stay unset.
#[must_use]
pub(crate) fn serialize_region_positioned(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    ansi: bool,
) -> Option<(Vec<u8>, Option<RegionPosition>)> {
    match source {
        RegionSource::Scrollback | RegionSource::Visible => {
            let rows = region_rows(grid, viewport, source, ansi)?;
            let position = region_position(grid, viewport, source, &rows);
            Some((join_rows(&rows), position))
        }
        _ => Some((serialize_region(grid, viewport, source, ansi)?, None)),
    }
}

/// The row sequence behind the two viewport-anchored sources, trailing
/// blanks dropped. The position mapping indexes this same sequence,
/// which [`join_rows`] turns into lines; a divergence would put the
/// pager on the wrong line.
fn region_rows(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    ansi: bool,
) -> Option<Vec<(Vec<u8>, bool)>> {
    let clusters = grid.cluster_table();
    let styles = grid.style_table();
    let mut rows: Vec<(Vec<u8>, bool)> = match source {
        // Scrollback first, then the live screen; the first live row's bit
        // stitches the seam to the youngest scrollback row.
        RegionSource::Scrollback => grid
            .rows_with_wrap(AltScreenRows::Include)
            .map(|(cells, cont)| (encode_row(cells, clusters, styles, ansi), cont))
            .collect(),
        RegionSource::Visible => {
            // The composed viewport as on screen; both encoders trim the
            // trailing blanks a pad to `cols` would add.
            (0..grid.rows())
                .map(|r| {
                    grid.viewport_row(viewport, r).map_or_else(
                        || (Vec::new(), false),
                        |view| {
                            (
                                encode_row(view.cells, clusters, styles, ansi),
                                view.soft_wrap_continued,
                            )
                        },
                    )
                })
                .collect()
        }
        RegionSource::CommandOutput | RegionSource::LastCommand => return None,
    };
    trim_trailing_blank_rows(&mut rows);
    Some(rows)
}

/// Map the window top and the cursor into the region's line space.
/// `None` when the region is empty or the source has no viewport
/// anchor.
fn region_position(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    rows: &[(Vec<u8>, bool)],
) -> Option<RegionPosition> {
    if rows.is_empty() {
        return None;
    }
    // Indices into the untrimmed sequence; `line_of` clamps, so a cursor
    // in the blank tail lands on the last line.
    let (top_row, cursor_row) = match source {
        RegionSource::Scrollback => {
            let sb_len = grid.scrollback().len();
            let viewport = usize::try_from(grid.clamp_viewport(viewport)).unwrap_or(sb_len);
            (
                sb_len.saturating_sub(viewport),
                sb_len + usize::from(grid.cursor().row),
            )
        }
        RegionSource::Visible => (
            0,
            usize::from(grid.cursor().row)
                + usize::try_from(grid.clamp_viewport(viewport)).unwrap_or(0),
        ),
        RegionSource::CommandOutput | RegionSource::LastCommand => return None,
    };
    Some(RegionPosition {
        top_line: line_of(rows, top_row),
        cursor_line: line_of(rows, cursor_row),
        cursor_column: cursor_column(grid, viewport, source, rows, cursor_row),
    })
}

/// 1-based logical-line number of region row `idx`, clamped to the
/// last line; a continuation row reports the head of the line it was
/// stitched into.
fn line_of(rows: &[(Vec<u8>, bool)], idx: usize) -> u32 {
    let idx = idx.min(rows.len().saturating_sub(1));
    let starts = rows[..=idx]
        .iter()
        .skip(1)
        .filter(|(_, continued)| !continued)
        .count();
    u32::try_from(starts + 1).unwrap_or(u32::MAX)
}

/// 1-based column of the cursor within its logical line. Counted off
/// the grid rather than the emitted bytes, which may carry SGR escapes
/// that occupy no column. Preceding rows count trimmed; the cursor's
/// own prefix counts untrimmed, where trailing spaces are content the
/// cursor sits after.
fn cursor_column(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    rows: &[(Vec<u8>, bool)],
    cursor_row: usize,
) -> u32 {
    let clusters = grid.cluster_table();
    let cursor_row = cursor_row.min(rows.len().saturating_sub(1));
    let mut head = cursor_row;
    while head > 0 && rows[head].1 {
        head -= 1;
    }
    let prefix: usize = (head..cursor_row)
        .filter_map(|idx| region_row_cells(grid, viewport, source, idx))
        .map(|cells| felis_grid::row_text_trim(&cells, clusters).chars().count())
        .sum();
    let own = region_row_cells(grid, viewport, source, cursor_row).map_or(0, |cells| {
        let col = usize::from(grid.cursor().col).min(cells.len());
        felis_grid::row_text(&cells[..col], clusters)
            .0
            .chars()
            .count()
    });
    u32::try_from(prefix + own + 1).unwrap_or(u32::MAX)
}

/// Cells of region row `idx`, re-read for the column measurement; the
/// composed [`RegionSource::Visible`] row is widened to the full column
/// count first.
fn region_row_cells(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    idx: usize,
) -> Option<Vec<Cell>> {
    match source {
        RegionSource::Scrollback => grid
            .rows_with_wrap(AltScreenRows::Include)
            .nth(idx)
            .map(|(cells, _)| cells.to_vec()),
        RegionSource::Visible => {
            let row = u16::try_from(idx).ok()?;
            let mut cells = grid.viewport_row(viewport, row)?.cells.to_vec();
            cells.resize(usize::from(grid.cols()), Cell::BLANK);
            Some(cells)
        }
        RegionSource::CommandOutput | RegionSource::LastCommand => None,
    }
}

/// Serialize the most recent `start_kind → CommandEnd` range
/// (docs/explanation/data-model/scrollback.md); `None` when no closed
/// range exists. Rows evicted past scrollback are skipped.
fn serialize_mark_range(
    grid: &Grid,
    start_kind: PromptKind,
    clusters: &ClusterTable,
    ansi: bool,
) -> Option<Vec<u8>> {
    let styles = grid.style_table();
    let mut rows: Vec<(Vec<u8>, bool)> = mark_range_cell_rows(grid, start_kind)?
        .into_iter()
        .map(|(cells, cont)| (encode_row(cells, clusters, styles, ansi), cont))
        .collect();
    trim_trailing_blank_rows(&mut rows);
    Some(join_rows(&rows))
}

/// The cell rows of the most recent `start_kind → CommandEnd` range,
/// half-open at the `D` mark. Evicted rows are always a leading prefix
/// (eviction is oldest-first), so a surviving continuation tail can
/// only land as the first row, where its bit is harmless.
fn mark_range_cell_rows(grid: &Grid, start_kind: PromptKind) -> Option<Vec<(&[Cell], bool)>> {
    let (start_line, end_line) = last_mark_range(grid.prompt_marks(), start_kind)?;
    let sb = grid.scrollback();
    Some(
        (start_line..end_line)
            .filter_map(|line| match grid.locate_line(line) {
                MarkLocation::Screen(r) => {
                    Some((grid.row_content(r)?, grid.row_soft_wrap_continued(r)))
                }
                MarkLocation::Scrollback(i) => {
                    Some((sb.row(i)?, sb.soft_wrap_continued(i).unwrap_or(false)))
                }
                MarkLocation::Evicted => None,
            })
            .collect(),
    )
}

/// One region row as it leaves in
/// [`felis_protocol::messages::RegionToClientMsg::Row`].
pub(crate) struct RegionRowOut {
    pub row: i32,
    pub text: String,
    pub ansi: Option<String>,
    pub soft_wrap_continued: bool,
}

/// Row stream for `RegionToDaemonMsg::Rows` (`felis sessions capture`).
///
/// Indexes rows in the region's coordinate space with optional `max_rows`
/// and `offset`/`limit` windowing pushed into source iterators. Returns `None`
/// when an OSC 133 mark range does not exist.
pub(crate) fn region_rows_window(
    grid: &Grid,
    viewport: u32,
    source: RegionSource,
    ansi: bool,
    max_rows: Option<u32>,
    offset: usize,
    limit: usize,
) -> Option<Vec<RegionRowOut>> {
    let clusters = grid.cluster_table();
    let styles = grid.style_table();
    let encode = |row: i32, cells: &[Cell], cont: bool| RegionRowOut {
        row,
        text: felis_grid::row_text_trim(cells, clusters),
        // `row_ansi` emits only chars and ASCII escape bytes, so the bytes
        // are UTF-8; lossy keeps this panic-free.
        ansi: ansi.then(|| {
            String::from_utf8_lossy(&felis_grid::row_ansi(cells, clusters, styles)).into_owned()
        }),
        soft_wrap_continued: cont,
    };
    let skip = |len: usize| match max_rows {
        Some(cap) => len.saturating_sub(cap as usize),
        None => 0,
    };
    match source {
        RegionSource::Scrollback => {
            let sb_len = grid.scrollback().len();
            let total = sb_len + usize::from(grid.rows());
            Some(
                grid.rows_with_wrap(AltScreenRows::Include)
                    .enumerate()
                    .skip(skip(total))
                    .skip(offset)
                    .take(limit)
                    .map(|(i, (cells, cont))| {
                        let idx = i64::try_from(i)
                            .ok()
                            .zip(i64::try_from(sb_len).ok())
                            .and_then(|(i, sb)| i32::try_from(i - sb).ok())
                            .unwrap_or(i32::MAX);
                        encode(idx, cells, cont)
                    })
                    .collect(),
            )
        }
        RegionSource::Visible => Some(
            (0..grid.rows())
                .skip(skip(usize::from(grid.rows())))
                .skip(offset)
                .take(limit)
                .map(|r| {
                    grid.viewport_row(viewport, r).map_or_else(
                        || encode(i32::from(r), &[], false),
                        |view| encode(i32::from(r), view.cells, view.soft_wrap_continued),
                    )
                })
                .collect(),
        ),
        RegionSource::CommandOutput | RegionSource::LastCommand => {
            let start_kind = if source == RegionSource::CommandOutput {
                PromptKind::OutputStart
            } else {
                PromptKind::InputStart
            };
            let mut rows: Vec<RegionRowOut> = mark_range_cell_rows(grid, start_kind)?
                .into_iter()
                .enumerate()
                .map(|(i, (cells, cont))| encode(i32::try_from(i).unwrap_or(i32::MAX), cells, cont))
                .collect();
            while rows.last().is_some_and(|r| r.text.is_empty()) {
                rows.pop();
            }
            let cut = skip(rows.len());
            rows.drain(..cut);
            rows.drain(..offset.min(rows.len()));
            rows.truncate(limit);
            Some(rows)
        }
    }
}

/// The last `CommandEnd` and the nearest preceding `start_kind`, as
/// absolute lines.
fn last_mark_range(marks: &[PromptMark], start_kind: PromptKind) -> Option<(u64, u64)> {
    let d = marks
        .iter()
        .rposition(|m| m.kind == PromptKind::CommandEnd)?;
    let s = marks[..d].iter().rposition(|m| m.kind == start_kind)?;
    Some((marks[s].line, marks[d].line))
}

/// Rows tied into one logical line by their continuation bits are
/// concatenated with no separator; each logical line is closed by
/// `\n`. The caller trims trailing blank rows first.
fn join_rows(rows: &[(Vec<u8>, bool)]) -> Vec<u8> {
    let continued: Vec<bool> = rows.iter().map(|(_, c)| *c).collect();
    let mut buf = Vec::new();
    for line in felis_grid::logical_lines(rows, &continued) {
        for (bytes, _) in line {
            buf.extend_from_slice(bytes);
        }
        buf.push(b'\n');
    }
    buf
}

/// Applied before both [`join_rows`] and the position mapping, which
/// must agree on how many lines the region has.
fn trim_trailing_blank_rows(rows: &mut Vec<(Vec<u8>, bool)>) {
    while rows.last().is_some_and(|(bytes, _)| bytes.is_empty()) {
        rows.pop();
    }
}

#[cfg(test)]
mod tests;
