use super::*;

#[test]
fn osc_9_4_report_becomes_a_progress_effect_for_either_terminator() {
    use crate::progress::ProgressState;
    for end in ["\x07", "\x1b\\"] {
        let bytes = format!("\x1b]9;4;2;71{end}\x1b]9;4;0{end}");
        assert_eq!(
            effects_of(bytes.as_bytes()),
            [
                progress(ProgressState::Error, Some(71)),
                PtyEffect::TerminalProgress(None),
            ],
            "{end:?}"
        );
    }
}

#[test]
fn osc_9_4_interrupted_or_malformed_queues_nothing() {
    for bytes in [
        &b"\x1b]9;4;1;50\x18"[..],
        b"\x1b]9;4;1;50\x1b[m",
        b"\x1b]9;4;;50\x07",
        b"\x1b]9;4;7;50\x07",
        b"\x1b]9;4;1;half\x07",
    ] {
        assert_eq!(effects_of(bytes), [], "{bytes:?}");
    }
}

/// A progress clear leaves the debt in place: a `working` record
/// reported before it still expires at the next prompt.
#[test]
fn osc_133_prompt_start_is_owed_after_progress_but_not_settled_by_its_clear() {
    use crate::progress::ProgressState;
    let prompt = b"\x1b]133;A\x07";
    assert_eq!(
        effects_of(&[&b"\x1b]9;4;3\x07"[..], prompt].concat()),
        [
            progress(ProgressState::Indeterminate, None),
            PtyEffect::PromptStarted
        ]
    );
    assert_eq!(
        effects_of(&[&b"\x1b]9;4;0\x07"[..], prompt].concat()),
        [PtyEffect::TerminalProgress(None)]
    );
    let effects = effects_of(
        &[
            &b"\x1b]7501;state=working\x07"[..],
            b"\x1b]9;4;0\x07",
            prompt,
        ]
        .concat(),
    );
    assert_eq!(
        effects.last(),
        Some(&PtyEffect::PromptStarted),
        "{effects:?}"
    );
}

#[test]
fn progress_reports_spend_the_status_budget() {
    let mut g = Grid::new(2, 8);
    let report = b"\x1b]9;4;1;5\x07";
    let bytes = report.repeat(STATUS_OUTBOX_YIELD + 1);
    assert_eq!(
        stops(&mut g, &bytes),
        [report.len() * STATUS_OUTBOX_YIELD, report.len()]
    );
}
