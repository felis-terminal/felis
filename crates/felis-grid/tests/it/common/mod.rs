//! Shared helpers for felis-grid integration tests.
#![allow(dead_code)]

use std::fmt::Write as _;

use felis_grid::{
    AttrFlags, Attributes, Color, Grapheme, Grid, PtyEffect, ScrollOp, row_text_trim,
};
use felis_vt::Parser;

pub(crate) fn drive(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
    let mut g = Grid::new(rows, cols);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    g
}

pub(crate) fn responses(g: &mut Grid) -> Vec<Vec<u8>> {
    g.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Response(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}

pub(crate) fn scroll_ops(g: &mut Grid) -> Vec<ScrollOp> {
    g.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::Scrolled { op, .. } => Some(op),
            _ => None,
        })
        .collect()
}

pub(crate) fn drive_take(rows: u16, cols: u16, bytes: &[u8]) -> (Grid, Vec<Vec<u8>>) {
    let mut g = Grid::new(rows, cols);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    let queued = responses(&mut g);
    (g, queued)
}

pub(crate) fn drive_with(parser: &mut Parser, grid: &mut Grid, bytes: &[u8]) {
    parser.advance(grid, bytes);
}

pub(crate) fn fmt_color(c: Color) -> String {
    match c {
        Color::Default => "default".into(),
        Color::Indexed(i) => format!("idx{i}"),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
    }
}

pub(crate) fn fmt_flags(f: AttrFlags) -> String {
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
        (AttrFlags::OVERLINE, 'O'),
    ] {
        if f.contains(flag) {
            s.push(ch);
        }
    }
    s
}

pub(crate) fn write_cursor(out: &mut String, grid: &Grid) {
    let cur = grid.cursor();
    writeln!(
        out,
        "cursor: row={} col={} visible={} pending_wrap={}",
        cur.row, cur.col, cur.visible as u8, cur.pending_wrap as u8,
    )
    .unwrap();
}

pub(crate) fn write_pen(out: &mut String, grid: &Grid) {
    let pen = grid.pen();
    writeln!(
        out,
        "pen: fg={} bg={} flags={}",
        fmt_color(pen.fg),
        fmt_color(pen.bg),
        fmt_flags(pen.flags),
    )
    .unwrap();
}

pub(crate) fn cell_char(grid: &Grid, r: u16, c: u16) -> char {
    match &grid.cell(r, c).unwrap().grapheme {
        Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => '.',
        Grapheme::Ascii(b) => *b as char,
        Grapheme::Char(c) => *c,
        Grapheme::Cluster(id) => grid
            .cluster_str(*id)
            .and_then(|s| s.chars().next())
            .unwrap_or('.'),
    }
}

pub(crate) fn write_cells(out: &mut String, grid: &Grid) {
    writeln!(out, "grid {}x{}:", grid.rows(), grid.cols()).unwrap();
    for r in 0..grid.rows() {
        out.push('|');
        for c in 0..grid.cols() {
            out.push(cell_char(grid, r, c));
        }
        out.push('|');
        out.push('\n');
    }
}

pub(crate) fn styled_cell_lines(grid: &Grid) -> Vec<String> {
    let mut lines = Vec::new();
    for r in 0..grid.rows() {
        for c in 0..grid.cols() {
            let attrs = grid.style(grid.cell(r, c).unwrap().style);
            if *attrs == Attributes::default() {
                continue;
            }
            lines.push(format!(
                "  ({r},{c}): fg={} bg={} flags={}",
                fmt_color(attrs.fg),
                fmt_color(attrs.bg),
                fmt_flags(attrs.flags),
            ));
        }
    }
    lines
}

pub(crate) fn write_dirty_rows(out: &mut String, grid: &Grid) {
    let dirty: Vec<_> = grid.damage().dirty_rows().collect();
    writeln!(out, "dirty_rows: {dirty:?}").unwrap();
}

/// `_` is a `Spacer`, `·` an `Empty`; a zero-width scalar inside a
/// cluster is spelled `\u{…}` so a dropped selector shows. Cells past
/// a row's watermark read blank, as every reader sees them.
pub(crate) fn render_roles(grid: &Grid) -> String {
    let mut out = String::new();
    write_cursor(&mut out, grid);
    for r in 0..grid.rows() {
        let row = grid.row_content(r).unwrap();
        let cells: Vec<String> = (0..grid.cols())
            .map(|c| role_token(grid, grid.cell(r, c).unwrap().grapheme))
            .collect();
        writeln!(out, "cells: {}", cells.join("|")).unwrap();
        writeln!(out, "text: {:?}", row_text_trim(row, grid.cluster_table())).unwrap();
    }
    out
}

fn role_token(grid: &Grid, g: Grapheme) -> String {
    match g {
        Grapheme::Empty => "·".into(),
        Grapheme::Spacer => "_".into(),
        Grapheme::SizedSpacer => "#".into(),
        Grapheme::Ascii(b) => char::from(b).into(),
        Grapheme::Char(c) => spell(c),
        Grapheme::Cluster(id) => grid
            .cluster_str(id)
            .map_or_else(|| "?".into(), |s| s.chars().map(spell).collect()),
    }
}

fn spell(c: char) -> String {
    if unicode_width::UnicodeWidthChar::width(c) == Some(0) || c == '\u{200D}' {
        format!("\\u{{{:x}}}", u32::from(c))
    } else {
        c.into()
    }
}
