//! Workloads and the daemon-to-shadow pipeline shared by the Criterion
//! benches and their instruction-count twin, so the gated numbers and the
//! local wall-clock numbers measure the same code.

use felis_client_core::ShadowScreen;
use felis_grid::{DEFAULT_SCROLLBACK_ROWS, Grid, PtyEffect, RowEncode, encode_row};
use felis_protocol::{
    MessageKind, RowPayload,
    codec::{decode, encode},
    frame::{Frame, decode as decode_frame},
    messages::GridMsg,
};
use felis_vt::Parser;

pub(crate) const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 80;
pub(crate) const STREAM_BYTES: usize = 4 * 1024;

/// The PTY parse thread's swap floor (`MIN_SWAP` in `felis-pty`), not
/// its 64 KiB `read(2)` size: the parser takes the reads accumulated
/// since its last swap, and the multi-cycle benches run one
/// `compose_diffs` per chunk of this size.
pub(crate) const PTY_CHUNK_SIZE: usize = 8 * 1024;

const PLAINTEXT_LINE: &[u8] = b"the quick brown fox jumps over the lazy dog\n";

pub(crate) fn plaintext_scroll() -> Vec<u8> {
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(PLAINTEXT_LINE);
    }
    out.truncate(STREAM_BYTES);
    out
}

pub(crate) fn csi_redraw() -> Vec<u8> {
    let row = b"\x1b[H\x1b[2J\x1b[1;31mhello\x1b[0m\x1b[2;32mworld\x1b[0m";
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(row);
    }
    out.truncate(STREAM_BYTES);
    out
}

pub(crate) fn shell_prompt() -> Vec<u8> {
    let cycle: &[u8] = concat!(
        "\x1b]133;A\x07$ \x1b]133;B\x07ls -la\n\x1b]133;C\x07",
        "\x1b[34mdrwxr-xr-x\x1b[0m  3 user user 4096 Feb  3 01:23 .\n",
        "\x1b[34mdrwxr-xr-x\x1b[0m 33 user user 4096 Feb  3 01:23 ..\n",
        "\x1b[32m-rwxr-xr-x\x1b[0m  1 user user 1024 Feb  3 01:23 run.sh\n",
        "\x1b]133;D;0\x07",
    )
    .as_bytes();
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(cycle);
    }
    out.truncate(STREAM_BYTES);
    out
}

pub(crate) fn repeat_to(seed: &[u8], target: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        let take = (target - out.len()).min(seed.len());
        out.extend_from_slice(&seed[..take]);
    }
    out
}

/// A tiling WM's split halves the window; a widen stitches the
/// soft-wrapped lines back together.
pub(crate) const REFLOW_NARROW_COLS: u16 = 40;
pub(crate) const REFLOW_WIDE_COLS: u16 = 120;

/// A full scrollback ring, the steady state of a long-lived session and
/// the most rows a reflow walks: hard-wrapped prompts, a diagnostic that
/// soft-wraps across three rows, and a CJK line whose wide characters
/// straddle the wrap column.
pub(crate) fn reflow_history_grid() -> Grid {
    let mut cycle: Vec<u8> = Vec::new();
    cycle.extend_from_slice(b"\x1b[1;32m$\x1b[0m cargo build --release\n");
    cycle.extend_from_slice(b"\x1b[33mwarning\x1b[0m: ");
    cycle.extend_from_slice(
        "unused variable `cols`: prefix it with an underscore if intentional "
            .repeat(3)
            .as_bytes(),
    );
    cycle.extend_from_slice(b"\n-> ");
    cycle.extend_from_slice("端末の再折り返しは横幅の変更で走る。".repeat(3).as_bytes());
    cycle.extend_from_slice(b"\n\n");
    let rows_per_cycle = 7;
    let cycles = (DEFAULT_SCROLLBACK_ROWS + usize::from(ROWS)) / rows_per_cycle + 1;
    let mut parser = Parser::new();
    let mut grid = Grid::new(ROWS, COLS);
    for _ in 0..cycles {
        parser.advance(&mut grid, &cycle);
    }
    assert_eq!(grid.scrollback().len(), grid.scrollback_capacity());
    drop(grid.take_pty_effects());
    grid.damage_mut().clear();
    grid
}

/// A long-lived session's next burst of output scrolls a full ring;
/// `compact` first moves its history to the cold tier, as the daemon does
/// while the session idles, so each line feed also evicts a cold row.
pub(crate) fn full_history_grid(compact: bool) -> Grid {
    let mut parser = Parser::new();
    let mut grid = Grid::new(ROWS, COLS);
    let lines = DEFAULT_SCROLLBACK_ROWS + usize::from(ROWS);
    parser.advance(&mut grid, &PLAINTEXT_LINE.repeat(lines));
    assert_eq!(grid.scrollback().len(), grid.scrollback_capacity());
    if compact {
        assert!(grid.compact_history(usize::MAX));
    }
    drop(grid.take_pty_effects());
    grid.damage_mut().clear();
    grid
}

/// The daemon's resize of the primary screen: the reflow, then the
/// full-screen replay it leaves dirty.
pub(crate) fn reflow_and_replay(grid: &mut Grid, cols: u16, scratch: &mut Vec<u8>) {
    std::hint::black_box(grid.reflow(ROWS, cols));
    emit_diffs_with(grid, scratch, true);
}

fn scroll_ops(grid: &mut Grid) -> Vec<felis_grid::ScrollOp> {
    grid.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

/// Mirror of the daemon's `compose_diffs` steady-state loop
/// (docs/reference/ipc.md): `Scrolled` frames first, then the dirty rows.
/// `coalesce` puts every dirty row in one `RowDelta`, as the daemon does;
/// without it each row gets its own frame, the counterfactual coalescing
/// is measured against and not a wire shape the daemon emits.
pub(crate) fn emit_diffs_with(grid: &mut Grid, scratch: &mut Vec<u8>, coalesce: bool) {
    scratch.clear();
    for op in scroll_ops(grid) {
        emit_grid_msg(
            scratch,
            &GridMsg::Scrolled {
                region_top: op.region_top,
                region_bottom: op.region_bottom,
                n_rows: op.n_rows,
                direction: op.direction,
            },
        );
    }
    let dirty: Vec<usize> = grid.damage().dirty_rows().collect();
    if coalesce {
        let mut rows: Vec<(u16, RowPayload)> = Vec::with_capacity(dirty.len());
        for row_idx in dirty {
            let r = row_idx as u16;
            let cells = grid.row_cells(r).unwrap_or(&[]);
            let sized = grid.row_sized_cells(r);
            let body = encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: grid.row_soft_wrap_continued(r),
                },
                grid.style_table(),
            )
            .unwrap();
            rows.push((r, RowPayload(body)));
        }
        if !rows.is_empty() {
            emit_grid_msg(scratch, &GridMsg::RowDelta { rows });
        }
    } else {
        for row_idx in dirty {
            let r = row_idx as u16;
            let cells = grid.row_cells(r).unwrap_or(&[]);
            let sized = grid.row_sized_cells(r);
            let body = encode_row(
                RowEncode {
                    cells,
                    pad_to: cells.len(),
                    sized_cells: &sized,
                    soft_wrap_continued: grid.row_soft_wrap_continued(r),
                },
                grid.style_table(),
            )
            .unwrap();
            emit_grid_msg(
                scratch,
                &GridMsg::RowDelta {
                    rows: vec![(r, RowPayload(body))],
                },
            );
        }
    }
    grid.damage_mut().clear();
}

fn emit_grid_msg(scratch: &mut Vec<u8>, msg: &GridMsg) {
    let body_bytes = encode(msg);
    let frame = Frame {
        kind: MessageKind::Grid.as_u16(),
        body: &body_bytes,
    };
    frame
        .encode_to(scratch)
        .expect("bench bodies are inside the ceiling");
}

pub(crate) fn apply_diffs(scratch: &[u8], shadow: &mut ShadowScreen) {
    let mut rest = scratch;
    while !rest.is_empty() {
        let (frame, n) = decode_frame(rest).expect("frame decodes");
        let msg: GridMsg = decode(frame.body).unwrap();
        shadow.apply(&msg).expect("shadow apply");
        rest = &rest[n..];
    }
}
