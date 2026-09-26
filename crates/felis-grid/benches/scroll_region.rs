//! `Grid::scroll_region_up` baseline at the daemon's default 24 x 80: whole
//! grid, partial region, print-then-scroll, and `yes`-flood shapes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_vt::Parser;

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Non-blank cells: a blank grid lets the scroll short-circuit.
fn populated_grid(rows: u16, cols: u16) -> Grid {
    use felis_grid::{Attributes, Cell, Color, Grapheme};
    let mut g = Grid::new(rows, cols);
    let attrs = Attributes {
        fg: Color::Indexed(7),
        ..Attributes::default()
    };
    let style = g.style_table_mut().intern(attrs);
    for r in 0..rows {
        for c in 0..cols {
            g.set_cell(
                r,
                c,
                Cell {
                    grapheme: Grapheme::Char('x'),
                    style,
                    link: None,
                    sizing: None,
                },
            );
        }
    }
    g.damage_mut().clear();
    g
}

fn cursor_to_bottom(g: &mut Grid, parser: &mut Parser, row: u16) {
    let cup = format!("\x1b[{row};1H");
    parser.advance(g, cup.as_bytes());
}

fn bench_full_screen(group: &mut BenchmarkGroup<'_, WallTime>) {
    let cells_per_iter = (u64::from(ROWS) - 1) * u64::from(COLS);
    group.throughput(Throughput::Elements(cells_per_iter));
    group.bench_function("full_screen_scroll_up_1", |b| {
        // LF at the bottom scrolls without moving the cursor. Clearing damage
        // each iter keeps the band clean; `mixed_print_scroll_full` scrolls
        // a band with a written row in it.
        let mut g = populated_grid(ROWS, COLS);
        let mut parser = Parser::new();
        cursor_to_bottom(&mut g, &mut parser, ROWS);
        b.iter(|| {
            parser.advance(&mut g, black_box(b"\n"));
            g.damage_mut().clear();
        });
    });
}

fn bench_partial_region(group: &mut BenchmarkGroup<'_, WallTime>) {
    let region_height = u64::from(ROWS) - 2;
    let cells_per_iter = (region_height - 1) * u64::from(COLS);
    group.throughput(Throughput::Elements(cells_per_iter));
    group.bench_function("partial_region_scroll_up_1", |b| {
        let mut g = populated_grid(ROWS, COLS);
        let mut parser = Parser::new();
        let dec = format!("\x1b[1;{}r", ROWS - 1);
        parser.advance(&mut g, dec.as_bytes());
        cursor_to_bottom(&mut g, &mut parser, ROWS - 1);
        g.damage_mut().clear();
        b.iter(|| {
            parser.advance(&mut g, black_box(b"\n"));
            g.damage_mut().clear();
        });
    });
}

fn bench_mixed_print_scroll(group: &mut BenchmarkGroup<'_, WallTime>) {
    // The `kitten __benchmark__ ascii` shape. With no damage clear between
    // scrolls, every scroll moves the marks of the rows printed before it.
    let line: Vec<u8> = {
        let mut v = vec![b'x'; usize::from(COLS) - 1];
        v.push(b'\n');
        v
    };
    group.throughput(Throughput::Bytes(line.len() as u64));
    group.bench_function("mixed_print_scroll_full", |b| {
        let mut g = populated_grid(ROWS, COLS);
        let mut parser = Parser::new();
        cursor_to_bottom(&mut g, &mut parser, ROWS);
        b.iter(|| {
            parser.advance(&mut g, black_box(&line));
        });
    });
}

fn bench_flood_scroll(group: &mut BenchmarkGroup<'_, WallTime>) {
    // The vtebench `scrolling` / `yes`-flood shape: one cell per line, then
    // CRLF (the pty's ONLCR turns a producer's `\n` into `\r\n`), so the
    // row's occupied prefix stays one cell and the recycled row's blank and
    // scrollback push touch only that prefix.
    group.throughput(Throughput::Bytes(3));
    group.bench_function("flood_scroll_up_1", |b| {
        let mut g = Grid::new(ROWS, COLS);
        let mut parser = Parser::new();
        cursor_to_bottom(&mut g, &mut parser, ROWS);
        g.damage_mut().clear();
        b.iter(|| {
            parser.advance(&mut g, black_box(b"y\r\n"));
            g.damage_mut().clear();
        });
    });
}

fn bench_scroll_region(c: &mut Criterion) {
    let mut group = c.benchmark_group("scroll_region");
    bench_full_screen(&mut group);
    bench_partial_region(&mut group);
    bench_mixed_print_scroll(&mut group);
    bench_flood_scroll(&mut group);
    group.finish();
}

criterion_group!(scroll_region, bench_scroll_region);
criterion_main!(scroll_region);
