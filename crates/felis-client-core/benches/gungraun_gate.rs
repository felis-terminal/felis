//! Instruction-count twin of the `daemon_side` and `multi_cycle_end_to_end`
//! groups in `end_to_end_throughput.rs`, run under callgrind for the PR
//! regression gate (`just bench-gate-ir`). Setup runs outside the measured
//! region, and the returned buffers drop outside it, as `iter_batched` does
//! for the Criterion twin.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[cfg(target_os = "linux")]
#[path = "support/workload.rs"]
mod workload;

#[cfg(target_os = "linux")]
#[allow(
    unused_qualifications,
    reason = "gungraun's #[library_benchmark] expansion emits `super::` paths with user spans"
)]
mod gate {
    use std::hint::black_box;

    use felis_client_core::ShadowScreen;
    use felis_grid::Grid;
    use felis_vt::Parser;
    use gungraun::{library_benchmark, library_benchmark_group};

    use crate::workload::{
        COLS, PTY_CHUNK_SIZE, ROWS, STREAM_BYTES, apply_diffs, csi_redraw, emit_diffs_with,
        plaintext_scroll, repeat_to, shell_prompt,
    };

    const SCRATCH_CAPACITY: usize = 128 * 1024;

    #[derive(Clone, Copy)]
    pub(crate) enum Workload {
        PlaintextScroll,
        CsiRedraw,
        ShellPrompt,
    }

    impl Workload {
        fn bytes(self) -> Vec<u8> {
            match self {
                Self::PlaintextScroll => plaintext_scroll(),
                Self::CsiRedraw => csi_redraw(),
                Self::ShellPrompt => shell_prompt(),
            }
        }
    }

    pub(crate) struct DaemonInput {
        bytes: Vec<u8>,
        scratch: Vec<u8>,
        parser: Parser,
        grid: Grid,
    }

    fn setup_daemon(workload: Workload) -> DaemonInput {
        DaemonInput {
            bytes: workload.bytes(),
            scratch: Vec::with_capacity(SCRATCH_CAPACITY),
            parser: Parser::new(),
            grid: Grid::new(ROWS, COLS),
        }
    }

    pub(crate) struct MultiInput {
        bytes: Vec<u8>,
        scratch: Vec<u8>,
        parser: Parser,
        grid: Grid,
        shadow: ShadowScreen,
        batched: bool,
    }

    fn setup_multi(workload: Workload, batched: bool) -> MultiInput {
        MultiInput {
            bytes: repeat_to(&workload.bytes(), 32 * STREAM_BYTES),
            scratch: Vec::with_capacity(SCRATCH_CAPACITY),
            parser: Parser::new(),
            grid: Grid::new(ROWS, COLS),
            shadow: ShadowScreen::new(ROWS, COLS),
            batched,
        }
    }

    #[library_benchmark]
    #[bench::plaintext_scroll(args = (Workload::PlaintextScroll), setup = setup_daemon)]
    #[bench::csi_redraw(args = (Workload::CsiRedraw), setup = setup_daemon)]
    #[bench::shell_prompt(args = (Workload::ShellPrompt), setup = setup_daemon)]
    fn daemon_side(input: DaemonInput) -> (Vec<u8>, Vec<u8>) {
        let DaemonInput {
            bytes,
            mut scratch,
            mut parser,
            mut grid,
        } = input;
        parser.advance(&mut grid, black_box(&bytes));
        emit_diffs_with(&mut grid, &mut scratch, true);
        black_box(scratch.len());
        (bytes, scratch)
    }

    #[library_benchmark]
    #[bench::plaintext_scroll_batched(args = (Workload::PlaintextScroll, true), setup = setup_multi)]
    #[bench::plaintext_scroll_individual(args = (Workload::PlaintextScroll, false), setup = setup_multi)]
    #[bench::csi_redraw_batched(args = (Workload::CsiRedraw, true), setup = setup_multi)]
    #[bench::csi_redraw_individual(args = (Workload::CsiRedraw, false), setup = setup_multi)]
    #[bench::shell_prompt_batched(args = (Workload::ShellPrompt, true), setup = setup_multi)]
    #[bench::shell_prompt_individual(args = (Workload::ShellPrompt, false), setup = setup_multi)]
    fn multi_cycle_end_to_end(input: MultiInput) -> (Vec<u8>, Vec<u8>) {
        let MultiInput {
            bytes,
            mut scratch,
            mut parser,
            mut grid,
            mut shadow,
            batched,
        } = input;
        for chunk in bytes.chunks(PTY_CHUNK_SIZE) {
            parser.advance(&mut grid, black_box(chunk));
            emit_diffs_with(&mut grid, &mut scratch, batched);
            apply_diffs(&scratch, &mut shadow);
        }
        black_box(scratch.len());
        (bytes, scratch)
    }

    library_benchmark_group!(
        name = end_to_end_throughput,
        benchmarks = [daemon_side, multi_cycle_end_to_end]
    );

    gungraun::main!(library_benchmark_groups = end_to_end_throughput);

    pub(super) fn run() {
        main();
    }
}

#[cfg(target_os = "linux")]
fn main() {
    gate::run();
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::print_stderr,
    reason = "the notice is the bench's only output where Valgrind is unavailable"
)]
fn main() {
    eprintln!("gungraun_gate: callgrind runs on Linux only; nothing to measure here");
}
