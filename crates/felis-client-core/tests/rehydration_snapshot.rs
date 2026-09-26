//! A rehydrate burst (`RehydrateBegin`, per-row `RowDelta` via
//! `felis_grid::encode_row`, `CursorState`, `RehydrateEnd`) must
//! reproduce the source `Grid` in the `ShadowScreen`
//! (`docs/explanation/architecture/session-lifecycle.md` "Attach").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::num::{NonZeroU16, NonZeroU32};

use felis_client_core::ShadowScreen;
use felis_grid::{AttrFlags, Color, Grapheme, Grid, RowEncode, ScreenBuffer, encode_row};
use felis_protocol::{RowPayload, messages::GridMsg};
use felis_vt::Parser;

/// The burst is composed the way the daemon's
/// `serve::streaming::compose_rehydrate` composes it: visible-first,
/// carrying only the registry entries the visible rows name, each ahead
/// of the row that names it, so a grid whose scrollback interned
/// unreferenced entries hands the shadow a sparse table.
fn rehydrate(rows: u16, cols: u16, bytes: &[u8]) -> (Grid, ShadowScreen) {
    let mut grid = Grid::new(rows, cols);
    Parser::new().advance(&mut grid, bytes);

    // Dims come from `SessionToClientMsg::Attached`, as in the client; the burst carries none.
    let mut shadow = ShadowScreen::new(grid.rows(), grid.cols());
    let cursor = grid.cursor();
    let mut burst = vec![GridMsg::RehydrateBegin];
    burst.extend(visible_registry_msgs(&grid));
    burst.extend((0..grid.rows()).map(|r| {
        let row_cells: Vec<_> = (0..grid.cols())
            .map(|c| grid.cell(r, c).copied().unwrap_or_default())
            .collect();
        GridMsg::RowDelta {
            rows: vec![(
                r,
                RowPayload(
                    encode_row(
                        RowEncode {
                            cells: &row_cells,
                            pad_to: row_cells.len(),
                            sized_cells: &[],
                            soft_wrap_continued: false,
                        },
                        grid.style_table(),
                    )
                    .unwrap(),
                ),
            )],
        }
    }));
    burst.push(GridMsg::CursorState {
        row: cursor.row,
        col: cursor.col,
        visible: cursor.visible,
        style: felis_protocol::messages::CursorStyle::default(),
        blink: true,
    });
    burst.push(GridMsg::RehydrateEnd);
    for msg in &burst {
        shadow.apply(msg).unwrap();
    }
    (grid, shadow)
}

fn visible_registry_msgs(grid: &Grid) -> Vec<GridMsg> {
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

fn first_mismatch(src: &ScreenBuffer, shadow: &ScreenBuffer) -> Option<String> {
    if src.rows() != shadow.rows() || src.cols() != shadow.cols() {
        return Some(format!(
            "dimensions: src={}x{} shadow={}x{}",
            src.rows(),
            src.cols(),
            shadow.rows(),
            shadow.cols()
        ));
    }
    for r in 0..src.rows() {
        for c in 0..src.cols() {
            let s = src.cell(r, c).unwrap();
            let d = shadow.cell(r, c).unwrap();
            if s != d {
                return Some(format!("cell ({r},{c}): src={s:?} shadow={d:?}"));
            }
        }
    }
    None
}

/// Not shared with `felis-grid`'s tests: the rendering shape is the
/// test contract.
fn render(screen: &ScreenBuffer) -> String {
    let mut out = String::new();
    let cur = screen.cursor();
    writeln!(
        out,
        "cursor: row={} col={} visible={}",
        cur.row, cur.col, cur.visible as u8
    )
    .unwrap();
    writeln!(out, "grid {}x{}:", screen.rows(), screen.cols()).unwrap();
    for r in 0..screen.rows() {
        out.push('|');
        for c in 0..screen.cols() {
            let ch = match &screen.cell(r, c).unwrap().grapheme {
                Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => '.',
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
                Grapheme::Cluster(id) => screen
                    .cluster_str(*id)
                    .and_then(|s| s.chars().next())
                    .unwrap_or('.'),
            };
            out.push(ch);
        }
        out.push_str("|\n");
    }
    let mut attr_lines = Vec::new();
    for r in 0..screen.rows() {
        for c in 0..screen.cols() {
            let cell = screen.cell(r, c).unwrap();
            let attrs = screen.style(cell.style);
            if *attrs == felis_grid::Attributes::default() {
                continue;
            }
            attr_lines.push(format!(
                "  ({r},{c}): fg={} bg={} flags={}",
                fmt_color(attrs.fg),
                fmt_color(attrs.bg),
                fmt_flags(attrs.flags),
            ));
        }
    }
    if attr_lines.is_empty() {
        writeln!(out, "attrs: (all default)").unwrap();
    } else {
        writeln!(out, "attrs:").unwrap();
        for line in attr_lines {
            writeln!(out, "{line}").unwrap();
        }
    }
    // `-` marks a handle the table spans but holds nothing for: with a
    // visible-first burst those are the entries only scrollback
    // references, and they must read as absent, not as an empty entry.
    let clusters = screen.cluster_table();
    if !clusters.is_empty() {
        writeln!(out, "clusters:").unwrap();
        for id in 1..=clusters.len() {
            let handle = NonZeroU32::new(u32::try_from(id).unwrap()).unwrap();
            // Codepoints, not the literal decomposed sequence: a snapshot
            // file holding it is at the mercy of anything that normalizes
            // it on the way through.
            let text = clusters
                .get(handle)
                .map_or_else(|| "-".to_owned(), fmt_codepoints);
            writeln!(out, "  {id}: {text}").unwrap();
        }
    }
    let links = screen.hyperlink_table();
    if !links.is_empty() {
        writeln!(out, "links:").unwrap();
        for id in 1..=links.len() {
            let handle = NonZeroU16::new(u16::try_from(id).unwrap()).unwrap();
            let uri = links
                .get(handle)
                .map_or_else(|| "-".to_owned(), |e| e.uri.as_str().to_owned());
            writeln!(out, "  {id}: {uri}").unwrap();
        }
    }
    out
}

fn fmt_codepoints(text: &str) -> String {
    text.chars()
        .map(|c| format!("U+{:04X}", c as u32))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fmt_color(c: Color) -> String {
    match c {
        Color::Default => "default".into(),
        Color::Indexed(i) => format!("idx{i}"),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
    }
}

fn fmt_flags(f: AttrFlags) -> String {
    if f.is_empty() {
        return "-".into();
    }
    let mut s = String::new();
    for (flag, ch) in [
        (AttrFlags::BOLD, 'B'),
        (AttrFlags::FAINT, 'F'),
        (AttrFlags::ITALIC, 'I'),
        (AttrFlags::UNDERLINE, 'U'),
        (AttrFlags::BLINK, 'L'),
        (AttrFlags::REVERSE, 'R'),
        (AttrFlags::CONCEAL, 'C'),
        (AttrFlags::STRIKETHROUGH, 'S'),
    ] {
        if f.contains(flag) {
            s.push(ch);
        }
    }
    s
}

#[test]
fn rehydration_preserves_a_colored_prompt() {
    let (src, shadow) = rehydrate(
        2,
        20,
        b"\x1b[1;32muser\x1b[0m@\x1b[1;34mhost\x1b[0m$ ls\r\nfoo bar",
    );
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=1 col=7 visible=1
    grid 2x20:
    |user@host$ ls.......|
    |foo bar.............|
    attrs:
      (0,0): fg=idx2 bg=default flags=B
      (0,1): fg=idx2 bg=default flags=B
      (0,2): fg=idx2 bg=default flags=B
      (0,3): fg=idx2 bg=default flags=B
      (0,5): fg=idx4 bg=default flags=B
      (0,6): fg=idx4 bg=default flags=B
      (0,7): fg=idx4 bg=default flags=B
      (0,8): fg=idx4 bg=default flags=B
    ");
}

#[test]
fn rehydration_preserves_reverse_video_overwrite() {
    let (src, shadow) = rehydrate(1, 10, b"abcdef\x1b[3D\x1b[7mX\x1b[0m");
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=0 col=4 visible=1
    grid 1x10:
    |abcXef....|
    attrs:
      (0,3): fg=default bg=default flags=R
    ");
}

#[test]
fn rehydration_preserves_full_erase_then_home() {
    let (src, shadow) = rehydrate(3, 6, b"AAAAAABBBBBBCCCCCC\x1b[2J\x1b[Hhi");
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=0 col=2 visible=1
    grid 3x6:
    |hi....|
    |......|
    |......|
    attrs: (all default)
    ");
}

#[test]
fn rehydration_preserves_utf8_cell_content() {
    // No trailing line feed: the cells stay on the visible grid, so this
    // exercises the multi-byte codec path rather than the scroll path.
    let (src, shadow) = rehydrate(2, 5, "あい".as_bytes());
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=0 col=4 visible=1
    grid 2x5:
    |あ.い..|
    |.....|
    attrs: (all default)
    ");
}

/// Pins: the burst's registry messages precede the rows, so clusters and
/// the OSC 8 anchor resolve the moment the rows land.
#[test]
fn rehydration_preserves_clusters_and_an_osc8_anchor() {
    let (src, shadow) = rehydrate(
        1,
        12,
        "e\u{0301} \u{1b}]8;;https://example.test/\u{7}o\u{0308}k\u{1b}]8;;\u{7}".as_bytes(),
    );
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=0 col=4 visible=1
    grid 1x12:
    |e ok........|
    attrs: (all default)
    clusters:
      1: U+0065 U+0301
      2: U+006F U+0308
    links:
      1: https://example.test/
    ");
}

/// Pins: entries only scrollback references read as absent (a hole in
/// the table) until the tail drain fills them, never as an empty entry.
#[test]
fn rehydration_leaves_the_scrolled_out_registry_entries_absent() {
    let (src, shadow) = rehydrate(
        2,
        4,
        "a\u{0301}\r\nb\u{0301}\r\nc\u{0301}\r\nd\u{0301}".as_bytes(),
    );
    assert_eq!(src.cluster_count(), 4, "one cluster per line");
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=1 col=1 visible=1
    grid 2x4:
    |c...|
    |d...|
    attrs: (all default)
    clusters:
      1: -
      2: -
      3: U+0063 U+0301
      4: U+0064 U+0301
    ");
}

#[test]
fn rehydration_preserves_an_empty_grid() {
    let (src, shadow) = rehydrate(2, 3, b"");
    assert!(first_mismatch(src.screen(), shadow.screen()).is_none());
    insta::assert_snapshot!(render(shadow.screen()), @r"
    cursor: row=0 col=0 visible=1
    grid 2x3:
    |...|
    |...|
    attrs: (all default)
    ");
}
