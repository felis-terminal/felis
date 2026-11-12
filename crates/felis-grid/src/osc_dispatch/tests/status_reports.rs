use super::*;

#[test]
fn osc_7501_report_becomes_a_status_effect_for_either_terminator() {
    use crate::program_status::{Kind, State};
    let want = PtyEffect::ProgramStatus(ProgramStatusEffect::Report(Box::new(
        program_status::Report {
            id: "build/test".to_owned(),
            kind: Some(Kind::Permission),
            progress: Some(40),
            app: Some("claude".to_owned()),
            title: Some("hi".to_owned()),
            ..report(State::Blocked)
        },
    )));
    let payload = "id=build/test:state=blocked:kind=permission:progress=40:app=claude:title=aGk=";
    for end in ["\x07", "\x1b\\"] {
        let bytes = format!("\x1b]7501;{payload}{end}");
        assert_eq!(
            effects_of(bytes.as_bytes()),
            std::slice::from_ref(&want),
            "{end:?}"
        );
    }
}

#[test]
fn osc_7501_clear_becomes_a_clear_effect() {
    assert_eq!(
        effects_of(b"\x1b]7501;state=clear\x07\x1b]7501;id=a:state=clear\x07"),
        [
            PtyEffect::ProgramStatus(ProgramStatusEffect::Clear { id: String::new() }),
            PtyEffect::ProgramStatus(ProgramStatusEffect::Clear { id: "a".to_owned() }),
        ]
    );
}

#[test]
fn osc_7501_interrupted_by_an_escape_sequence_is_not_a_report() {
    let da1 = PtyEffect::Response(DA1_REPLY.to_vec());
    assert_eq!(effects_of(b"\x1b]7501;state=done\x1b[c"), [da1]);
    assert_eq!(effects_of(b"\x1b]7501;state=done\x18"), []);
}

#[test]
fn osc_7501_report_the_decoder_drops_queues_nothing() {
    for bytes in [
        &b"\x1b]7501;id=a\x07"[..],
        b"\x1b]7501;state=finished\x07",
        b"\x1b]7501;id=a//b:state=done\x07",
        b"\x1b]7501;state=done:msg=Bw==\x07",
    ] {
        assert_eq!(effects_of(bytes), [], "{bytes:?}");
    }
}

const OSC_7501_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// One bare reply per query, ST-terminated whichever terminator asked,
/// and it spends nothing from the status budget.
#[test]
fn osc_7501_query_is_answered_with_the_bare_reply() {
    let reply = PtyEffect::Response(OSC_7501_REPLY.to_vec());
    assert_eq!(
        effects_of(b"\x1b]7501;?\x1b\\"),
        std::slice::from_ref(&reply)
    );
    assert_eq!(effects_of(b"\x1b]7501;?\x07"), std::slice::from_ref(&reply));
    assert_eq!(
        effects_of(b"\x1b]7501; ? \x07"),
        std::slice::from_ref(&reply)
    );
    assert_eq!(
        effects_of(b"\x1b]7501;?\x07\x1b]7501;?\x07"),
        [reply.clone(), reply]
    );
}

/// Detection is answered only for a sequence that finished; a
/// cancelled query is not a question.
#[test]
fn osc_7501_interrupted_query_is_not_answered() {
    assert_eq!(effects_of(b"\x1b]7501;?\x18"), []);
    assert_eq!(effects_of(b"\x1b]7501;?\x1a"), []);
}

/// The DA1-after-query detection the spec recommends: the 7501 reply
/// arrives first.
#[test]
fn osc_7501_query_reply_precedes_a_da1_sent_after_it() {
    assert_eq!(
        effects_of(b"\x1b]7501;?\x1b\\\x1b[c"),
        [
            PtyEffect::Response(OSC_7501_REPLY.to_vec()),
            PtyEffect::Response(DA1_REPLY.to_vec()),
        ]
    );
}

/// The 4096-byte cap counts the terminator, so the same body fits with
/// BEL and is one byte over with ST.
#[test]
fn osc_7501_sequence_cap_counts_the_terminator_the_parser_saw() {
    let head = "\x1b]7501;state=done:x=";
    let pad = "a".repeat(4096 - head.len() - 1);
    let body = format!("{head}{pad}");
    assert_eq!(
        effects_of(format!("{body}\x07").as_bytes()),
        [PtyEffect::ProgramStatus(ProgramStatusEffect::Report(
            Box::new(report(program_status::State::Done))
        ))]
    );
    assert_eq!(effects_of(format!("{body}\x1b\\").as_bytes()), []);
}

#[test]
fn osc_133_prompt_start_queues_prompt_started_unless_a_continuation() {
    const WORKING: &[u8] = b"\x1b]7501;state=working\x07";
    let working = || {
        PtyEffect::ProgramStatus(ProgramStatusEffect::Report(Box::new(report(
            program_status::State::Working,
        ))))
    };
    for prompt in [&b"\x1b]133;A\x07"[..], b"\x1b]133;A;redraw=0\x1b\\"] {
        assert_eq!(
            effects_of(&[WORKING, prompt].concat()),
            [working(), PtyEffect::PromptStarted],
            "{prompt:?}"
        );
    }
    for mark in [
        &b"\x1b]133;A;k=s\x07"[..],
        b"\x1b]133;B\x07",
        b"\x1b]133;C\x07",
        b"\x1b]133;D;0\x07",
    ] {
        assert_eq!(
            effects_of(&[WORKING, mark].concat()),
            [working()],
            "{mark:?}"
        );
    }
}

/// A prompt start only matters to a `working` or `blocked` record, so
/// one is queued only when such a report came since the last one: a
/// shell that never reports queues nothing per prompt.
#[test]
fn osc_133_prompt_start_queues_only_after_a_transient_report() {
    let prompt = b"\x1b]133;A\x07";
    assert_eq!(effects_of(&prompt.repeat(3)), []);
    for state in ["idle", "done", "error"] {
        let bytes = [format!("\x1b]7501;state={state}\x07").as_bytes(), prompt].concat();
        assert!(
            !effects_of(&bytes).contains(&PtyEffect::PromptStarted),
            "{state}"
        );
    }
    let bytes = [
        &b"\x1b]7501;state=blocked\x07"[..],
        prompt,
        prompt,
        b"\x1b]7501;id=a:state=working\x07",
        prompt,
    ]
    .concat();
    let prompts = effects_of(&bytes)
        .into_iter()
        .filter(|e| *e == PtyEffect::PromptStarted)
        .count();
    assert_eq!(prompts, 2, "one per transient report, not one per prompt");
}

/// Only a primary prompt start or RIS settles what a report owes: a
/// continuation prompt, DECSTR and screen switches in between leave it.
#[test]
fn osc_133_prompt_start_stays_owed_across_events_that_expire_nothing() {
    for between in [
        &b"\x1b]133;A;k=s\x07"[..],
        b"\x1b[!p",
        b"\x1b[?1049h\x1b[?1049l",
    ] {
        let bytes = [
            &b"\x1b]7501;state=working\x07"[..],
            between,
            b"\x1b]133;A\x07",
        ]
        .concat();
        let effects = effects_of(&bytes);
        assert_eq!(
            effects.last(),
            Some(&PtyEffect::PromptStarted),
            "{between:?}: {effects:?}"
        );
    }
}

/// RIS removes every record, so nothing is owed to the next prompt.
#[test]
fn osc_133_prompt_start_after_ris_owes_nothing() {
    let effects = effects_of(b"\x1b]7501;state=working\x07\x1bc\x1b]133;A\x07");
    assert!(!effects.contains(&PtyEffect::PromptStarted), "{effects:?}");
}

#[test]
fn osc_133_prompt_start_on_the_alternate_screen_still_queues_prompt_started() {
    let effects = effects_of(b"\x1b]7501;state=working\x07\x1b[?1049h\x1b]133;A\x07");
    assert_eq!(
        effects.last(),
        Some(&PtyEffect::PromptStarted),
        "{effects:?}"
    );
}

#[test]
fn ris_after_a_report_keeps_the_report_ahead_of_the_reset() {
    assert_eq!(
        effects_of(b"\x1b]7501;state=working\x07\x1bc"),
        [
            PtyEffect::ProgramStatus(ProgramStatusEffect::Report(Box::new(report(
                program_status::State::Working
            )))),
            PtyEffect::HardReset,
            PtyEffect::ImagesCleared(ImageClear::Screen),
        ]
    );
}

#[test]
fn the_report_that_fills_the_status_budget_stops_the_parse_after_its_terminator() {
    for report in [
        &b"\x1b]7501;state=idle\x07"[..],
        b"\x1b]7501;state=idle\x1b\\",
    ] {
        let mut g = Grid::new(2, 8);
        let short = report.repeat(STATUS_OUTBOX_YIELD - 1);
        assert_eq!(stops(&mut g, &short), Vec::<usize>::new(), "{report:?}");
        let mut more = report.to_vec();
        more.extend_from_slice(report);
        assert_eq!(
            stops(&mut g, &more),
            [report.len(), report.len()],
            "{report:?}"
        );
        drop(g.take_pty_effects());
        assert_eq!(
            stops(&mut g, report),
            Vec::<usize>::new(),
            "the drain refills the budget"
        );
    }
}

#[test]
fn an_interrupted_report_does_not_spend_the_status_budget() {
    let mut g = Grid::new(2, 8);
    let filler = b"\x1b]7501;state=idle\x1b[c".repeat(STATUS_OUTBOX_YIELD * 2);
    assert_eq!(stops(&mut g, &filler), Vec::<usize>::new());
}

/// Prompt starts and prompt edges share the budget: a session task that
/// drains only on request must still see them in bounded batches. A
/// primary `A` after a `working` report queues a start and an edge.
#[test]
fn prompt_starts_and_reports_fill_the_status_budget_together() {
    let mut g = Grid::new(2, 8);
    let prompt = b"\x1b]133;A\x07";
    let report = b"\x1b]7501;state=working\x07";
    let finished = b"\x1b]133;D;0\x07";
    let mut bytes = Vec::new();
    for _ in 0..STATUS_OUTBOX_YIELD / 4 {
        bytes.extend_from_slice(report);
        bytes.extend_from_slice(prompt);
        bytes.extend_from_slice(finished);
    }
    assert_eq!(stops(&mut g, &bytes), [bytes.len()]);
    assert_eq!(g.take_pty_effects().len(), STATUS_OUTBOX_YIELD);
}
