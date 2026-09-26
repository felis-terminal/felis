//! Parser throughput baseline, per `docs/reference/testing.md` "Performance benchmarks".

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use felis_vt::{Parser, Sink};

/// Pure parser cost, without the grid / dispatcher.
struct NoopSink;
impl Sink for NoopSink {}

fn ascii_fast_path() -> Vec<u8> {
    // `cat <large-text-file>`.
    let line = b"the quick brown fox jumps over the lazy dog. ";
    let mut out = Vec::with_capacity(4 * 1024);
    while out.len() < 4 * 1024 {
        out.extend_from_slice(line);
    }
    out.truncate(4 * 1024);
    out
}

fn csi_heavy() -> Vec<u8> {
    // A full-screen redraw from vim or lazygit.
    let mut out = Vec::with_capacity(4 * 1024);
    let row = b"\x1b[H\x1b[2J\x1b[1;31mhello\x1b[0m\x1b[2;32mworld\x1b[0m";
    while out.len() < 4 * 1024 {
        out.extend_from_slice(row);
    }
    out.truncate(4 * 1024);
    out
}

fn osc_heavy() -> Vec<u8> {
    // Shell prompt redraws: OSC 7 (cwd) and OSC 133 boundaries per command.
    let mut out = Vec::with_capacity(4 * 1024);
    let prompt = b"\x1b]7;file://localhost/home/u\x07\x1b]133;A\x07\x1b]133;B\x07$ ";
    while out.len() < 4 * 1024 {
        out.extend_from_slice(prompt);
    }
    out.truncate(4 * 1024);
    out
}

fn osc_long_body() -> Vec<u8> {
    // `kitten __benchmark__ long_escape_codes`-shaped traffic: multi-KB OSC
    // bodies on the bulk `scan_string_body` path.
    let mut out = Vec::with_capacity(32 * 1024);
    while out.len() < 32 * 1024 {
        out.extend_from_slice(b"\x1b]6;");
        out.extend(std::iter::repeat_n(b'p', 8000));
        out.push(0x07);
    }
    out
}

fn sgr_dense() -> Vec<u8> {
    // termbench-pro's sgr_fg_bg_lines shape: long digit runs (`38;5;NNN`)
    // keep the parser in CsiParam, which `csi_heavy` never reaches.
    let mut out = Vec::with_capacity(4 * 1024);
    let mut n = 0u32;
    while out.len() < 4 * 1024 {
        out.extend_from_slice(
            format!("\x1b[38;5;{}m\x1b[48;5;{}mx", n % 256, (n + 7) % 256).as_bytes(),
        );
        n += 1;
    }
    out.truncate(4 * 1024);
    out
}

fn mixed() -> Vec<u8> {
    // An interactive shell: one "command + output" cycle per iteration.
    let cycle = concat!(
        "\x1b]133;A\x07$ \x1b]133;B\x07ls -la\n\x1b]133;C\x07",
        "\x1b[34mdrwxr-xr-x\x1b[0m  3 user user 4096 Feb  3 01:23 .\n",
        "\x1b[34mdrwxr-xr-x\x1b[0m 33 user user 4096 Feb  3 01:23 ..\n",
        "\x1b[32m-rwxr-xr-x\x1b[0m  1 user user 1024 Feb  3 01:23 run.sh\n",
        "\x1b]133;D;0\x07",
    )
    .as_bytes();
    let mut out = Vec::with_capacity(4 * 1024);
    while out.len() < 4 * 1024 {
        out.extend_from_slice(cycle);
    }
    out.truncate(4 * 1024);
    out
}

fn bench_parser(c: &mut Criterion) {
    let inputs: [(&str, Vec<u8>); 6] = [
        ("ascii_fast_path", ascii_fast_path()),
        ("csi_heavy", csi_heavy()),
        ("sgr_dense", sgr_dense()),
        ("osc_heavy", osc_heavy()),
        ("osc_long_body", osc_long_body()),
        ("mixed", mixed()),
    ];
    let mut group = c.benchmark_group("parser_throughput");
    for (name, bytes) in &inputs {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut sink = NoopSink;
                parser.advance(&mut sink, black_box(bytes));
            });
        });
    }
    group.finish();
}

criterion_group!(parser_throughput, bench_parser);
criterion_main!(parser_throughput);
