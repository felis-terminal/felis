use crate::test_support::{drive, responses};
use crate::*;
use felis_protocol::messages::ThemeChannel;
use felis_vt::Parser;

mod clusters;
mod color;
mod hyperlink;
mod notifications;
mod pointer_shape;
mod progress_reports;
mod prompt_marks;
mod status_reports;
mod title_cwd;
mod tolerated;

/// The prompt edges every `A` and `D` queues are left out: the cases
/// here pin the status effects around them.
pub(super) fn effects_of(bytes: &[u8]) -> Vec<PtyEffect> {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 20);
    drive(&mut p, &mut g, bytes);
    g.take_pty_effects()
        .into_iter()
        .filter(|effect| !matches!(effect, PtyEffect::Prompt(_)))
        .collect()
}

pub(super) fn report(state: program_status::State) -> program_status::Report {
    program_status::Report {
        id: String::new(),
        state,
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
    }
}

pub(super) fn progress(state: progress::ProgressState, value: Option<u8>) -> PtyEffect {
    PtyEffect::TerminalProgress(Some(progress::Progress { state, value }))
}

/// Feeds `bytes` through `advance_until_yield`, returning how many bytes
/// each stop consumed.
pub(super) fn stops(g: &mut Grid, bytes: &[u8]) -> Vec<usize> {
    let mut p = Parser::new();
    let mut rest = bytes;
    let mut out = Vec::new();
    while let Some(n) = p.advance_until_yield(g, rest) {
        out.push(n);
        rest = &rest[n..];
    }
    out
}
