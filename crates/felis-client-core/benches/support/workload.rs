//! Workloads and the daemon-to-shadow pipeline shared by the Criterion
//! benches and their instruction-count twin, so the gated numbers and the
//! local wall-clock numbers measure the same code.

use felis_client_core::ShadowScreen;
use felis_grid::{Grid, PtyEffect, RowEncode, encode_row};
use felis_protocol::{
    MessageKind, RowPayload,
    codec::{decode, encode},
    frame::{Frame, decode as decode_frame},
    messages::GridMsg,
};

pub(crate) const ROWS: u16 = 24;
pub(crate) const COLS: u16 = 80;
pub(crate) const STREAM_BYTES: usize = 4 * 1024;

/// The daemon's PTY read size (`READ_BUFFER_SIZE` in `felis-pty`); the
/// multi-cycle benches run one `compose_diffs` per chunk of this size.
pub(crate) const PTY_CHUNK_SIZE: usize = 8 * 1024;

pub(crate) fn plaintext_scroll() -> Vec<u8> {
    let line = b"the quick brown fox jumps over the lazy dog\n";
    let mut out = Vec::with_capacity(STREAM_BYTES);
    while out.len() < STREAM_BYTES {
        out.extend_from_slice(line);
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
