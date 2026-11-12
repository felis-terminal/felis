use super::*;

#[test]
fn osc_133_a_records_a_prompt_start_mark_at_cursor_line() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(5, 10);
    // With no scrolling yet the absolute line equals the cursor row
    // (docs/explanation/data-model/scrollback.md).
    drive(&mut p, &mut g, b"\x1b[3;1H");
    drive(&mut p, &mut g, b"\x1b]133;A\x07");
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].line, 2);
    assert_eq!(marks[0].kind, PromptKind::PromptStart);
    assert_eq!(marks[0].exit_code, None);
}

#[test]
fn osc_133_d_with_exit_code_is_parsed() {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b]133;D;42\x07");
    let marks = g.prompt_marks();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].kind, PromptKind::CommandEnd);
    assert_eq!(marks[0].exit_code, Some(42));
}

/// A later bare `D` must yield `None` rather than fall back to an older
/// command's code.
#[test]
fn last_command_exit_reads_only_the_youngest_d_mark() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    assert_eq!(g.last_command_exit(), None, "no marks yet");
    drive(&mut p, &mut g, b"\x1b]133;D;42\x07\r\n");
    assert_eq!(g.last_command_exit(), Some(42));
    drive(&mut p, &mut g, b"\x1b]133;A\x07\r\n");
    assert_eq!(g.last_command_exit(), Some(42));
    drive(&mut p, &mut g, b"\x1b]133;D\x07\r\n");
    assert_eq!(g.last_command_exit(), None, "bare D shadows the older 42");
}

#[test]
fn osc_133_unknown_kind_is_ignored() {
    let mut p = Parser::new();
    let mut g = Grid::new(2, 4);
    drive(&mut p, &mut g, b"\x1b]133;Q\x07");
    assert_eq!(g.prompt_marks(), []);
}

/// Four prompt-start marks at absolute lines 0..=3 with 3 rows scrolled
/// into scrollback, so the marks span the scrollback/screen boundary
/// (`docs/explanation/data-model/scrollback.md` "Prompt marks (OSC
/// 133)").
fn grid_with_four_prompts() -> Grid {
    use felis_protocol::messages::PromptKind;
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    // A@0; LF; A@1; LF; A@2; LF→scroll; A@3; LF→scroll; LF→scroll.
    drive(
        &mut p,
        &mut g,
        b"\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\x1b]133;A\x07\r\n\r\n",
    );
    assert_eq!(g.scrollback_total_pushed(), 3);
    let lines: Vec<u64> = g
        .prompt_marks()
        .iter()
        .filter(|m| m.kind == PromptKind::PromptStart)
        .map(|m| m.line)
        .collect();
    assert_eq!(lines, vec![0, 1, 2, 3]);
    g
}

#[test]
fn prompt_jump_previous_walks_up_through_scrollback() {
    use felis_protocol::messages::PromptJump;
    let g = grid_with_four_prompts();
    // Each Previous lands the next prompt up at the top composed row
    // (viewport = tp - line).
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), Some(1)); // → line 2 at top
    assert_eq!(g.prompt_jump_target(1, PromptJump::Previous), Some(2)); // → line 1
    assert_eq!(g.prompt_jump_target(2, PromptJump::Previous), Some(3)); // → line 0
    // At the oldest prompt: hard clamp, no wrap.
    assert_eq!(g.prompt_jump_target(3, PromptJump::Previous), None);
}

#[test]
fn prompt_jump_next_walks_down_toward_the_live_bottom() {
    use felis_protocol::messages::PromptJump;
    let g = grid_with_four_prompts();
    assert_eq!(g.prompt_jump_target(3, PromptJump::Next), Some(2)); // → line 1
    assert_eq!(g.prompt_jump_target(2, PromptJump::Next), Some(1)); // → line 2
    // The last prompt sits on the live screen, so the viewport clamps
    // to 0.
    assert_eq!(g.prompt_jump_target(1, PromptJump::Next), Some(0));
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}

#[test]
fn prompt_jump_is_disabled_on_the_alternate_screen() {
    use felis_protocol::messages::PromptJump;
    let mut p = Parser::new();
    let mut g = Grid::new(3, 10);
    drive(&mut p, &mut g, b"\x1b]133;A\x07\r\n\x1b]133;A\x07");
    drive(&mut p, &mut g, b"\x1b[?1049h");
    assert!(g.on_alternate_screen());
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), None);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}

#[test]
fn prompt_jump_with_no_marks_is_a_no_op() {
    use felis_protocol::messages::PromptJump;
    let g = Grid::new(3, 10);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Previous), None);
    assert_eq!(g.prompt_jump_target(0, PromptJump::Next), None);
}
