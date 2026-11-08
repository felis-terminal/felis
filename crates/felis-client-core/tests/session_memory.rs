//! Heap a parked session holds for its next window, and what the window
//! holds once attached (`docs/reference/testing.md` "Heap footprint").
//! The ceilings are structural, far from today's figures; the table this
//! prints (`just bench-mem`) carries the figures themselves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../benches/support/reattach.rs"]
#[allow(dead_code)]
mod reattach;

use std::fmt::Write as _;

use bytes::Bytes;
use felis_grid::{Cell, DEFAULT_SCROLLBACK_ROWS, Grid};

use reattach::{COLS, ROWS, Session, Window, images, shell_history};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// A history row that brought an allocation of its own would cost
/// `DEFAULT_SCROLLBACK_ROWS` of them; filling the ring must stay an
/// order of magnitude below one per row.
const ALLOCATIONS_CEILING: u64 = (DEFAULT_SCROLLBACK_ROWS / 10) as u64;

/// What a full session may hold beyond its cell ring and image pixels:
/// registries, prompt marks and parser state, about 100 bytes a row.
const SIDE_BYTES_CEILING: usize = 1 << 20;

/// A window mirrors the viewport only; a shadow that kept history would
/// hold the session's whole ring.
const WINDOW_TEXT_CEILING: usize = 1 << 20;

#[derive(Clone, Copy)]
struct Footprint {
    live_bytes: usize,
    live_blocks: usize,
    peak_bytes: usize,
    allocations: u64,
}

/// The caller builds the inputs outside the profiled region, so only
/// what `build` allocates counts, and only what it returns stays live.
fn measure<T>(build: impl FnOnce() -> T) -> (T, Footprint) {
    let profiler = dhat::Profiler::builder().testing().build();
    let held = build();
    let stats = dhat::HeapStats::get();
    drop(profiler);
    let footprint = Footprint {
        live_bytes: stats.curr_bytes,
        live_blocks: stats.curr_blocks,
        peak_bytes: stats.max_bytes,
        allocations: stats.total_blocks,
    };
    (held, footprint)
}

fn prompt(out: &mut String, cmd: usize) {
    write!(
        out,
        "\x1b]133;A\x07\x1b[1;32muser@host\x1b[0m:\x1b[1;34m~/src\x1b[0m$ \
         \x1b]133;B\x07cargo test -p crate-{cmd}\r\n\x1b]133;C\x07"
    )
    .unwrap();
}

fn idle_prompt() -> Vec<u8> {
    let mut out = String::new();
    prompt(&mut out, 0);
    out.into_bytes()
}

/// ASCII build and test output under a two-color prompt, which grows
/// none of the style, cluster or link registries past their seed.
fn plain_history(lines: usize) -> Vec<u8> {
    let mut out = String::with_capacity(lines * 64);
    let (mut line, mut cmd) = (0, 0);
    while line < lines {
        prompt(&mut out, cmd);
        let output = 18 + cmd % 13;
        for n in line..line + output {
            if n % 3 == 0 {
                write!(
                    out,
                    "   Compiling felis-grid-{} v0.{}.{}",
                    n % 31,
                    n % 9,
                    n % 17
                )
            } else {
                write!(out, "test grid::tests::case_{n} ... ok")
            }
            .unwrap();
            out.push_str("\r\n");
        }
        out.push_str("\x1b]133;D;0\x07");
        line += output + 1;
        cmd += 1;
    }
    out.into_bytes()
}

/// Enough lines that every retained row has scrolled through the ring.
fn full_history() -> usize {
    DEFAULT_SCROLLBACK_ROWS + 2 * usize::from(ROWS)
}

fn session(inputs: &[&[u8]]) -> (Session, Footprint) {
    measure(|| {
        let mut session = Session::new(DEFAULT_SCROLLBACK_ROWS);
        for input in inputs {
            session.feed(input);
        }
        session
    })
}

/// The burst is composed outside the profiled region but copied inside
/// it, as a window's read buffer is, so a window that kept slices of
/// the burst would be charged for the whole buffer.
fn window(session: &Session) -> Footprint {
    let burst = session.burst();
    let (window, footprint) = measure(|| {
        let mut window = Window::new();
        window.rehydrate(Bytes::copy_from_slice(&burst));
        window
    });
    window.check(session);
    footprint
}

fn mean_occupied_cols(grid: &Grid) -> usize {
    let history = grid.scrollback();
    history.iter().map(<[Cell]>::len).sum::<usize>() / history.len().max(1)
}

struct Report(String);

impl Report {
    fn new() -> Self {
        let mut out = format!(
            "{ROWS}x{COLS}, {DEFAULT_SCROLLBACK_ROWS} rows of scrollback; \
             size_of::<Cell>() = {} B, so a row of cells is {} B\n\n",
            size_of::<Cell>(),
            size_of::<Cell>() * usize::from(COLS),
        );
        out.push_str(
            "| scenario | live B | live blocks | peak B | allocations | B per row | mean occupied cols |\n\
             | --- | --: | --: | --: | --: | --: | --: |\n",
        );
        Self(out)
    }

    /// Per row counts the cells and everything else a session holds,
    /// pixels aside, over the rows a full ring retains.
    fn row(&mut self, name: &str, fp: Footprint, session: Option<&Session>) {
        let history = session.map_or(0, |s| s.grid.scrollback().len());
        let (per_row, occupied) = match session {
            Some(s) if history > 0 => {
                let rows = usize::from(ROWS) + history;
                let text = fp.live_bytes - s.images.bytes_used();
                (
                    (text / rows).to_string(),
                    mean_occupied_cols(&s.grid).to_string(),
                )
            }
            _ => ("-".to_owned(), "-".to_owned()),
        };
        writeln!(
            self.0,
            "| {name} | {} | {} | {} | {} | {per_row} | {occupied} |",
            fp.live_bytes, fp.live_blocks, fp.peak_bytes, fp.allocations
        )
        .unwrap();
    }
}

#[test]
fn session_and_window_heap_stays_within_structural_ceilings() {
    let idle_input = idle_prompt();
    let plain_input = plain_history(full_history());
    let rich_input = shell_history(full_history());
    let image_input = images(4);

    let (idle, idle_fp) = session(&[&idle_input]);
    let (plain, plain_fp) = session(&[&plain_input]);
    let (rich, rich_fp) = session(&[&rich_input]);
    let (pictured, pictured_fp) = session(&[&rich_input, &image_input]);
    let rich_window = window(&rich);
    let pictured_window = window(&pictured);

    let mut report = Report::new();
    report.row("idle shell", idle_fp, Some(&idle));
    report.row("plain 10k", plain_fp, Some(&plain));
    report.row("rich 10k", rich_fp, Some(&rich));
    report.row("rich 10k + images", pictured_fp, Some(&pictured));
    report.row("window, rich 10k", rich_window, None);
    report.row("window, rich 10k + images", pictured_window, None);
    println!("{}", report.0);

    let ring = rich.grid.reserved_cell_bytes();
    let pixels = pictured.images.bytes_used();
    assert!(pixels > 0, "the image session holds no pixels");
    for (name, s, fp) in [
        ("plain", &plain, plain_fp),
        ("rich", &rich, rich_fp),
        ("images", &pictured, pictured_fp),
    ] {
        assert_eq!(s.grid.scrollback().len(), DEFAULT_SCROLLBACK_ROWS, "{name}");
        let side = fp.live_bytes - ring - s.images.bytes_used();
        assert!(
            side < SIDE_BYTES_CEILING,
            "{name}: {side} bytes beyond the ring and the pixels",
        );
    }
    for (name, fp) in [("plain", plain_fp), ("rich", rich_fp)] {
        assert!(
            fp.allocations < ALLOCATIONS_CEILING,
            "{name}: filling the ring took {} allocations",
            fp.allocations,
        );
    }
    assert!(
        pictured_fp.live_bytes - rich_fp.live_bytes < pixels * 3 / 2,
        "the session holds its {pixels} pixel bytes more than once",
    );
    assert!(
        rich_window.live_bytes < WINDOW_TEXT_CEILING,
        "the window holds {} bytes of text state",
        rich_window.live_bytes,
    );
    assert!(
        pictured_window.live_bytes - rich_window.live_bytes < pixels * 3 / 2,
        "the window holds its {pixels} pixel bytes more than once",
    );
}
