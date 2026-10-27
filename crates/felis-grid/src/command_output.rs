//! The output of a finished shell command, cut between its primary-screen
//! `OSC 133 ; C` and `OSC 133 ; D` marks for the windows that announce it.

pub use felis_protocol::messages::MAX_ANNOUNCE_BYTES;
use felis_protocol::messages::{OmittedOutput, PromptKind};
#[cfg(feature = "state-dump")]
use serde::{Deserialize, Serialize};

use crate::{Cell, Grid, PtyEffect, ShellPrompt, row_text_trim};

/// Youngest rows one [`PtyEffect::CommandOutput`] keeps; iTerm2's
/// `kMaxAccessibilityAnnounceLines`.
pub const MAX_ANNOUNCE_ROWS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(Serialize, Deserialize))]
pub(crate) enum OutputStart {
    /// `line` is absolute, as [`crate::PromptMark::line`]; `col` is the
    /// row width when the `C` arrived with a wrap pending.
    At { line: u64, col: u16 },
    /// Cells moved across the start, or a reflow could not place it.
    Lost,
    /// Scrollback eviction passed the start line.
    Evicted,
}

/// Primary-screen cells a grid operation moved, in screen rows as they
/// stood before the move.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CellsMoved {
    /// The top `n` rows of the band `0..=bottom` entered history.
    IntoHistory { bottom: u16, n: u16 },
    /// Rows of `top..=bottom` shifted vertically, whole or in part.
    Rows { top: u16, bottom: u16 },
    /// Cells of `top..=bottom` from column `first` on shifted sideways.
    Columns { top: u16, bottom: u16, first: u16 },
}

/// A start recorded with a wrap pending sits past the end of its row, so
/// the output begins on the next one.
const fn first_output_line(line: u64, col: u16, cols: u16) -> u64 {
    if col >= cols { line + 1 } else { line }
}

impl ShellPrompt {
    pub(crate) const fn evict_output_start_below(&mut self, oldest_retained: u64, cols: u16) {
        if let Some(OutputStart::At { line, col }) = self.output_start
            && first_output_line(line, col, cols) < oldest_retained
        {
            self.output_start = Some(OutputStart::Evicted);
        }
    }

    /// Only a column-0 start on a row that begins a logical line has a
    /// place in the re-wrapped text: the head of that line's new first
    /// row. `new_first_row` maps a pre-reflow combined row index to that
    /// row, relative to `base_abs` like its argument.
    pub(crate) fn remap_output_start(
        &mut self,
        base_abs: u64,
        continued: &[bool],
        new_first_row: impl Fn(usize) -> Option<usize>,
    ) {
        let Some(OutputStart::At { line, col }) = self.output_start else {
            return;
        };
        let Some(j) = line.checked_sub(base_abs) else {
            self.output_start = Some(OutputStart::Evicted);
            return;
        };
        let head = usize::try_from(j)
            .ok()
            .filter(|&j| col == 0 && continued.get(j) == Some(&false))
            .and_then(new_first_row);
        self.output_start = Some(head.map_or(OutputStart::Lost, |row| OutputStart::At {
            line: base_abs.saturating_add(u64::try_from(row).unwrap_or(u64::MAX)),
            col: 0,
        }));
    }
}

impl Grid {
    /// Takes effect from the next `OSC 133 ; D`; the start is tracked
    /// either way, so a command already running is still captured.
    pub const fn set_capture_command_output(&mut self, on: bool) {
        self.capture_command_output = on;
    }

    #[must_use]
    pub const fn capture_command_output(&self) -> bool {
        self.capture_command_output
    }

    /// Primary-screen `OSC 133` only.
    pub(crate) fn observe_command_output(&mut self, kind: PromptKind) {
        match kind {
            PromptKind::OutputStart => {
                let cursor = self.screen.cursor;
                let col = if cursor.pending_wrap {
                    self.screen.cols
                } else {
                    cursor.col
                };
                self.shell_prompt.output_start = Some(OutputStart::At {
                    line: self.scrollback_total_pushed + u64::from(cursor.row),
                    col,
                });
            }
            PromptKind::CommandEnd => {
                let Some(start) = self.shell_prompt.output_start.take() else {
                    return;
                };
                if !self.capture_command_output {
                    return;
                }
                if let Some(effect) = self.cut_command_output(start) {
                    self.pty_effects.push(effect);
                    self.yield_pending |= self.pty_effects.command_output_budget_spent();
                }
            }
            PromptKind::PromptStart | PromptKind::InputStart => {}
        }
    }

    /// Every primary-screen operation that moves cells reports here
    /// before it moves them: the start's line names a screen row only
    /// through `scrollback_total_pushed`, and its column names a cell.
    pub(crate) fn note_cells_moved(&mut self, moved: CellsMoved) {
        if self.screen.on_alternate_screen() {
            return;
        }
        let Some(OutputStart::At { line, col }) = &mut self.shell_prompt.output_start else {
            return;
        };
        let Some(row) = line.checked_sub(self.scrollback_total_pushed) else {
            return;
        };
        let lost = match moved {
            CellsMoved::IntoHistory { bottom, n } => {
                if row > u64::from(bottom) {
                    *line += u64::from(n);
                }
                false
            }
            CellsMoved::Rows { top, bottom } => (u64::from(top)..=u64::from(bottom)).contains(&row),
            CellsMoved::Columns { top, bottom, first } => {
                (u64::from(top)..=u64::from(bottom)).contains(&row) && *col > first
            }
        };
        if lost {
            self.shell_prompt.output_start = Some(OutputStart::Lost);
        }
    }

    /// Rows are written as the region serializer writes them, then
    /// capped to the youngest [`MAX_ANNOUNCE_ROWS`] and
    /// [`MAX_ANNOUNCE_BYTES`].
    fn cut_command_output(&self, start: OutputStart) -> Option<PtyEffect> {
        let total = self.scrollback_total_pushed;
        let oldest = total.saturating_sub(u64::try_from(self.screen.scrollback().len()).ok()?);
        let (from_line, from_col, mut unknown) = match start {
            OutputStart::At { line, col } if col >= self.screen.cols && line + 1 >= oldest => {
                (line + 1, 0, false)
            }
            OutputStart::At { line, col } if line >= oldest => (line, usize::from(col), false),
            OutputStart::At { .. } | OutputStart::Evicted => (oldest, 0, true),
            OutputStart::Lost => return None,
        };
        let cursor = self.screen.cursor;
        let mut end_line = total + u64::from(cursor.row);
        let mut end_col = if cursor.pending_wrap {
            usize::MAX
        } else {
            usize::from(cursor.col)
        };
        if end_col == 0 && end_line > from_line {
            end_line -= 1;
            end_col = usize::MAX;
        }
        let clusters = self.cluster_table();
        // Youngest first, with each row's soft-wrap continuation bit.
        let mut kept: Vec<(String, bool)> = Vec::new();
        let mut bytes = 0usize;
        let mut has_text = false;
        let mut dropped = 0u64;
        for line in (from_line..=end_line).rev() {
            let (cells, continued) = self.line_cells(line);
            let lo = if line == from_line { from_col } else { 0 };
            let hi = if line == end_line {
                end_col
            } else {
                usize::MAX
            };
            let slice = cells
                .get(lo.min(cells.len())..hi.min(cells.len()))
                .unwrap_or(&[]);
            let mut text = row_text_trim(slice, clusters);
            // The blank tail is trimmed from the text, so it costs no bytes.
            let separator =
                usize::from(has_text && kept.last().is_some_and(|(_, younger)| !younger));
            if kept.len() == MAX_ANNOUNCE_ROWS
                || bytes + separator + text.len() > MAX_ANNOUNCE_BYTES
            {
                if !has_text && kept.len() < MAX_ANNOUNCE_ROWS {
                    let mut cut = text.len() - MAX_ANNOUNCE_BYTES;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                    kept.push((text, continued));
                    unknown = true;
                } else {
                    dropped = line - from_line + 1;
                }
                break;
            }
            bytes += separator + text.len();
            has_text |= !text.is_empty();
            kept.push((text, continued));
        }
        let blank_tail = kept.iter().take_while(|(row, _)| row.is_empty()).count();
        let mut text = String::with_capacity(bytes);
        for (i, (row, continued)) in kept[blank_tail..].iter().rev().enumerate() {
            if i > 0 && !continued {
                text.push('\n');
            }
            text.push_str(row);
        }
        let omitted = if unknown {
            OmittedOutput::Unknown
        } else if dropped > 0 {
            OmittedOutput::Rows(u32::try_from(dropped).unwrap_or(u32::MAX))
        } else {
            OmittedOutput::None
        };
        Some(PtyEffect::CommandOutput { text, omitted })
    }

    /// Occupancy-clipped cells and continuation bit of a retained
    /// absolute line.
    fn line_cells(&self, line: u64) -> (&[Cell], bool) {
        let total = self.scrollback_total_pushed;
        if let Some(row) = line.checked_sub(total) {
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            return (
                self.screen.row_content(row).unwrap_or(&[]),
                self.screen.row_soft_wrap_continued(row),
            );
        }
        let sb = self.screen.scrollback();
        let oldest = total.saturating_sub(u64::try_from(sb.len()).unwrap_or(u64::MAX));
        let Some(idx) = line
            .checked_sub(oldest)
            .and_then(|i| usize::try_from(i).ok())
        else {
            return (&[], false);
        };
        (
            sb.row(idx).unwrap_or(&[]),
            sb.soft_wrap_continued(idx).unwrap_or(false),
        )
    }
}

#[cfg(test)]
mod tests;
