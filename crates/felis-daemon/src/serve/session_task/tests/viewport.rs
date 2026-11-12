use super::*;

/// `InputMsg::Viewport` clamps the requesting subscriber's viewport
/// to retained scrollback depth and composes the browse view for
/// that subscriber alone (session-lifecycle.md "Same-user
/// mirroring").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_viewport_clamps_to_retained_scrollback() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let rows = task.session.lock_core().grid.rows();
    let pad = "x\r\n".repeat(usize::from(rows) + 3);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut task.session.lock_core().grid, pad.as_bytes());
    let depth = task.session.lock_core().grid.scrollback().len() as u32;
    assert!(depth >= 3, "fixture needs ≥ 3 scrollback rows, got {depth}");

    let (sub_a, mut rx_a) = attach_sub(&mut task, false);
    let (_sub_b, mut rx_b) = attach_sub(&mut task, false);

    let end = task.handle_input(
        sub_a,
        InputMsg::Viewport {
            lines_from_bottom: 9_999,
            anchor: None,
        },
    );
    assert!(end.is_none(), "Viewport is a continuing input");
    assert_eq!(
        task.subs[0].stream.diff.viewport, depth,
        "viewport must clamp to retained scrollback depth",
    );

    let a_rows: usize = drain_events(&mut rx_a)
        .iter()
        .filter_map(|ev| match ev {
            OutEvent::Grid(GridMsg::RowDelta { rows }) => Some(rows.len()),
            _ => None,
        })
        .sum();
    assert_eq!(a_rows, usize::from(rows), "composed view for the requester");
    assert!(
        drain_events(&mut rx_b).is_empty(),
        "a viewport change must not disturb other subscribers",
    );
}

fn task_with_scrollback(extra: usize) -> (SessionTask, felis_vt::Parser, u32) {
    let task = SessionTask::for_tests(session_with("read _x"));
    let rows = task.session.lock_core().grid.rows();
    let pad = "x\r\n".repeat(usize::from(rows) + extra);
    let mut parser = felis_vt::Parser::new();
    parser.advance(&mut task.session.lock_core().grid, pad.as_bytes());
    let depth = task.session.lock_core().grid.scrollback().len() as u32;
    (task, parser, depth)
}

fn step(task: &mut SessionTask, sub: SubscriberId, rows: i32) {
    assert!(
        task.handle_input(sub, InputMsg::ViewportStep { rows })
            .is_none(),
        "ViewportStep is a continuing input",
    );
}

/// Two steps sent before either reply lands count from each other,
/// not from the offset the client last saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewport_steps_accumulate_from_the_daemon_offset() {
    let (mut task, _parser, depth) = task_with_scrollback(5);
    assert!(depth >= 5, "fixture needs ≥ 5 scrollback rows, got {depth}");
    let (sub, _rx) = attach_sub(&mut task, false);

    step(&mut task, sub, 1);
    step(&mut task, sub, 1);
    assert_eq!(task.subs[0].stream.diff.viewport, 2);
    step(&mut task, sub, -1);
    assert_eq!(task.subs[0].stream.diff.viewport, 1);
}

/// End then `LineUp` before End's reply: one row above the live
/// bottom, not one row above where the view was browsing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_step_after_a_return_to_live_counts_from_the_live_bottom() {
    let (mut task, _parser, _depth) = task_with_scrollback(5);
    let (sub, _rx) = attach_sub(&mut task, false);
    step(&mut task, sub, 4);

    let end = task.handle_input(
        sub,
        InputMsg::Viewport {
            lines_from_bottom: 0,
            anchor: None,
        },
    );
    assert!(end.is_none());
    step(&mut task, sub, 1);
    assert_eq!(task.subs[0].stream.diff.viewport, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewport_step_clamps_to_both_ends_of_history() {
    let (mut task, _parser, depth) = task_with_scrollback(3);
    let (sub, _rx) = attach_sub(&mut task, false);

    step(&mut task, sub, i32::MAX);
    assert_eq!(task.subs[0].stream.diff.viewport, depth);
    step(&mut task, sub, i32::MIN);
    assert_eq!(task.subs[0].stream.diff.viewport, 0);
}

/// The step applies to the offset grown by the rows output pushed
/// while browsing, so the view moves one row from the lines shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewport_step_counts_from_the_offset_output_has_grown() {
    let (mut task, mut parser, _depth) = task_with_scrollback(5);
    let (sub, _rx) = attach_sub(&mut task, false);
    step(&mut task, sub, 2);

    parser.advance(&mut task.session.lock_core().grid, b"y\r\n");
    step(&mut task, sub, 1);
    assert_eq!(task.subs[0].stream.diff.viewport, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_viewport_step_on_the_alternate_screen_stays_live() {
    let (mut task, mut parser, _depth) = task_with_scrollback(3);
    let (sub, _rx) = attach_sub(&mut task, false);
    parser.advance(&mut task.session.lock_core().grid, b"\x1b[?1049h");

    step(&mut task, sub, 1);
    assert_eq!(task.subs[0].stream.diff.viewport, 0);
}

/// `InputMsg::JumpPrompt` (docs/explanation/data-model/scrollback.md)
/// walks the per-subscriber viewport prompt-to-prompt and
/// hard-clamps at the oldest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_jump_prompt_walks_the_viewport_through_marks() {
    let mut task = SessionTask::for_tests(session_with("read _x"));
    let mut parser = felis_vt::Parser::new();
    for _ in 0..3 {
        parser.advance(&mut task.session.lock_core().grid, b"\x1b]133;A\x07");
        parser.advance(
            &mut task.session.lock_core().grid,
            "out\r\n".repeat(40).as_bytes(),
        );
    }
    let (sub, mut rx) = attach_sub(&mut task, false);

    let mut up = Vec::new();
    for _ in 0..4 {
        task.handle_input(
            sub,
            InputMsg::JumpPrompt {
                direction: PromptJump::Previous,
            },
        );
        up.push(task.subs[0].stream.diff.viewport);
        drain_events(&mut rx);
    }
    assert!(
        up[0] > 0 && up[1] > up[0] && up[2] > up[1],
        "each Previous jumps to an older prompt (strictly increasing viewport): {up:?}",
    );
    assert_eq!(
        up[3], up[2],
        "Previous at the oldest prompt is a no-op (hard clamp)"
    );

    task.handle_input(
        sub,
        InputMsg::JumpPrompt {
            direction: PromptJump::Next,
        },
    );
    let down1 = task.subs[0].stream.diff.viewport;
    assert!(down1 < up[2], "Next jumps toward the live bottom");
}
