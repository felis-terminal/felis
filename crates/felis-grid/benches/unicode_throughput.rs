//! Unicode print-path throughput against a real `Grid`, approximating
//! `kitten __benchmark__ unicode`. The stream is synthesized from code-point
//! ranges rather than third-party text so the data is self-authored.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_grid::Grid;
use felis_vt::{Parser, Sink};

const ROWS: u16 = 24;
const COLS: u16 = 80;

fn sample_block() -> String {
    let mut s = String::with_capacity(2048);

    let puncts = ['，', '。', '：', '；', '！', '？'];
    let mut cp: u32 = 0x4E00;
    for line in 0..7 {
        for i in 0..40 {
            if let Some(c) = char::from_u32(cp) {
                s.push(c);
            }
            cp += 7;
            if cp > 0x9FFF {
                cp = 0x4E00;
            }
            if i % 13 == 12 {
                s.push(puncts[(i + line) % puncts.len()]);
            }
        }
        s.push('\n');
    }

    s.push_str("‘’“”‹›«»—–§¶†‡©®™\n");
    for e in 0x1F600u32..0x1F610 {
        if let Some(c) = char::from_u32(e) {
            s.push(c);
        }
    }
    s.push('\n');
    for a in 0x00C0u32..0x00F0 {
        if let Some(c) = char::from_u32(a) {
            s.push(c);
        }
    }
    s.push('\n');
    let marks = ['\u{0300}', '\u{0301}', '\u{0302}', '\u{0308}', '\u{0327}'];
    for (i, base) in "aeiouncy".chars().enumerate() {
        s.push(base);
        s.push(marks[i % marks.len()]);
        if i % 2 == 0 {
            s.push(marks[(i + 2) % marks.len()]);
        }
    }
    s.push('\n');
    s.push('\t');
    s
}

fn unicode_stream() -> Vec<u8> {
    let block = sample_block();
    let block = block.as_bytes();
    let target = 64 * 1024;
    let mut out = Vec::with_capacity(target + block.len());
    while out.len() < target {
        out.extend_from_slice(block);
    }
    out
}

/// Decodes runs the way the grid's batched entry points do but takes
/// widths from unicode-width, not the grid's BMP table, and stores no
/// cells: the delta against `cjk_emoji_combining` is the grid write plus
/// the gap between the two width lookups.
struct DecodeOnlySink {
    utf8: felis_vt::utf8::Decoder,
    width_acc: u32,
}
impl Sink for DecodeOnlySink {
    fn print_str(&mut self, bytes: &[u8]) {
        self.width_acc += bytes.len() as u32;
    }
    fn print_utf8_run(&mut self, bytes: &[u8]) {
        use unicode_width::UnicodeWidthChar;
        match simdutf8::basic::from_utf8(bytes) {
            Ok(s) => {
                for c in s.chars() {
                    self.width_acc += c.width().unwrap_or(0) as u32;
                }
            }
            Err(_) => {
                for &b in bytes {
                    self.print(b);
                }
            }
        }
    }
    fn print(&mut self, byte: u8) {
        use unicode_width::UnicodeWidthChar;
        if (0x20..=0x7E).contains(&byte) {
            self.width_acc += 1;
            return;
        }
        let acc = &mut self.width_acc;
        self.utf8.push(byte, |c| {
            *acc += c.width().unwrap_or(0) as u32;
        });
    }
}

fn bench_unicode(c: &mut Criterion) {
    let bytes = unicode_stream();
    let mut group = c.benchmark_group("unicode_throughput");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("cjk_emoji_combining", |b| {
        b.iter(|| {
            // A fresh grid per iter: reuse would drift the working set through
            // scrollback.
            let mut grid = Grid::new(ROWS, COLS);
            let mut parser = Parser::new();
            parser.advance(&mut grid, black_box(&bytes));
            black_box(&grid);
        });
    });
    group.bench_function("decode_only", |b| {
        b.iter(|| {
            let mut sink = DecodeOnlySink {
                utf8: felis_vt::utf8::Decoder::new(),
                width_acc: 0,
            };
            let mut parser = Parser::new();
            parser.advance(&mut sink, black_box(&bytes));
            black_box(sink.width_acc);
        });
    });
    group.finish();
}

criterion_group!(unicode_throughput, bench_unicode);
criterion_main!(unicode_throughput);
