use felis_grid::{Grapheme, Grid, RowEncode, encode_row};
use felis_protocol::{RowPayload, messages::GridMsg};

/// Each row encoded the way the daemon's `encode_row_view` encodes it:
/// the occupancy-clipped cells padded to the grid width, with the row's
/// OSC 66 band and soft-wrap bit.
pub(crate) fn encode_rows(
    grid: &Grid,
    rows: impl IntoIterator<Item = u16>,
) -> Vec<(u16, RowPayload)> {
    rows.into_iter()
        .map(|r| {
            let view = grid.row_view(r);
            let body = encode_row(
                RowEncode {
                    cells: view.cells,
                    pad_to: usize::from(grid.cols()),
                    sized_cells: &view.sized_cells,
                    soft_wrap_continued: view.soft_wrap_continued,
                },
                grid.style_table(),
            )
            .unwrap();
            (r, RowPayload(body))
        })
        .collect()
}

pub(crate) fn encode_dirty_rows(grid: &Grid) -> Vec<(u16, RowPayload)> {
    let dirty: Vec<u16> = grid
        .damage()
        .dirty_rows()
        .map(|r| u16::try_from(r).unwrap())
        .collect();
    encode_rows(grid, dirty)
}

/// The registry entries the visible rows name, each ahead of the first
/// row naming it: the shadow refuses a row that names an absent entry,
/// so a burst must ship them in this order, as the daemon does.
pub(crate) fn visible_registry_msgs(grid: &Grid) -> Vec<GridMsg> {
    let mut msgs = Vec::new();
    let mut links: Vec<u16> = Vec::new();
    let mut clusters: Vec<u32> = Vec::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let Some(cell) = grid.cell(r, c) else {
                continue;
            };
            if let Some(id) = cell.link
                && !links.contains(&id.get())
                && let Some(entry) = grid.hyperlink(id)
            {
                links.push(id.get());
                msgs.push(GridMsg::Hyperlink {
                    id: id.get(),
                    anchor: entry.id.as_ref().map(|a| a.as_str().to_owned()),
                    uri: entry.uri.as_str().to_owned(),
                });
            }
            if let Grapheme::Cluster(id) = cell.grapheme
                && !clusters.contains(&id.get())
                && let Some(text) = grid.cluster_str(id)
            {
                clusters.push(id.get());
                msgs.push(GridMsg::Cluster {
                    id: id.get(),
                    text: text.to_owned(),
                });
            }
        }
    }
    msgs
}

/// A socket parent must be a `0700` directory this uid owns
/// (REQ-107), and `TempDir` follows the process umask.
#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) fn private_dir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    tmp
}

/// An in-process daemon spawning the default shell, bound before it is
/// spawned so a connect the caller makes next is queued rather than
/// refused.
#[cfg(all(unix, feature = "daemon-tests"))]
pub(crate) fn daemon(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    use std::sync::Arc;

    use felis_daemon::SessionPool;
    use felis_daemon::serve::{DaemonCaps, bind_unix, default_session_factory};

    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(tokio::sync::Mutex::new(SessionPool::new()));
    let serving = bind_unix(
        &path,
        DaemonCaps::default(),
        pool,
        default_session_factory(),
    )
    .expect("the test daemon binds its socket");
    drop(tokio::spawn(async move { drop(serving.await) }));
    path
}
