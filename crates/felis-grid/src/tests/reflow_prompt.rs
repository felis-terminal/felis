use super::*;

fn screen_ascii(g: &Grid) -> Vec<String> {
    (0..g.rows())
        .map(|r| {
            let line: String = (0..g.cols())
                .map(|c| match g.cell(r, c).map(|cell| cell.grapheme) {
                    Some(Grapheme::Ascii(b)) => char::from(b),
                    _ => ' ',
                })
                .collect();
            line.trim_end().to_owned()
        })
        .collect()
}

/// What zsh does on SIGWINCH: climb the rows its prompt took at the old
/// width (here one above the cursor), erase below, repaint.
const ZSH_REPAINT_AT_6: &[u8] = b"\x1b[1A\r\x1b[J\x1b]133;A\x07------\r\n% ";

/// One finished command, so the shell has shown it marks command starts.
const PRIOR_COMMAND: &[u8] = b"\x1b]133;C\x07out\r\n\x1b]133;D;0\x07";

fn grid_after(bytes: &[&[u8]]) -> (Parser, Grid) {
    let mut p = Parser::new();
    let mut g = Grid::new(6, 10);
    for b in bytes {
        drive(&mut p, &mut g, b);
    }
    (p, g)
}

#[test]
fn reflow_blanks_the_prompt_the_shell_repaints() {
    let (mut p, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A\x07----------\r\n% "]);

    g.reflow(6, 6);
    drive(&mut p, &mut g, ZSH_REPAINT_AT_6);

    assert_eq!(screen_ascii(&g), ["out", "------", "%", "", "", ""]);
}

#[test]
fn reflow_blanks_only_the_cursor_line_under_redraw_last() {
    let (_, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A;redraw=last\x07----------\r\n% "]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "------", "----", "", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (3, 2));
}

/// A `k=s` continuation must not become the clear's start, or the
/// prompt's first line is re-wrapped and left behind.
#[test]
fn reflow_blanks_from_the_primary_prompt_not_a_continuation() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A\x07----------\r\n\x1b]133;A;k=s\x07% ",
    ]);

    g.reflow(6, 6);

    assert_eq!(screen_ascii(&g), ["out", "", "", "", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (2, 2));
}

/// A shell inside a TUI marks prompts on the alternate screen; they say
/// nothing about the primary screen's prompt.
#[test]
fn reflow_ignores_prompt_marks_from_the_alternate_screen() {
    let (mut p, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A\x07$ \x1b]133;B\x07tui\r\n\x1b]133;C\x07\x1b[?1049h",
        b"\x1b]133;A\x07inner\x1b]133;B\x07",
    ]);

    g.reflow(6, 6);
    drive(&mut p, &mut g, b"\x1b[?1049l");

    assert_eq!(screen_ascii(&g), ["out", "$ tui", "", "", "", ""]);
}

/// bash under `redraw=last` repaints only the line its cursor is on;
/// the rows below are its to keep.
#[test]
fn reflow_keeps_rows_below_the_cursor_under_redraw_last() {
    let (_, mut g) = grid_after(&[
        PRIOR_COMMAND,
        b"\x1b]133;A;redraw=last\x07$ \r\nbelowbelow\x1b[2;3H",
    ]);

    let remap = g.reflow(6, 6).expect("geometry changed");

    assert_eq!(screen_ascii(&g), ["out", "", "belowb", "elow", "", ""]);
    assert_eq!((g.cursor().row, g.cursor().col), (1, 2));
    assert_eq!(remap.remap_row(2), None, "the cursor's row");
    assert_eq!(
        remap.remap_row(3),
        Some(3),
        "the row below keeps its anchor"
    );
}

#[test]
fn reflow_remap_drops_anchors_on_a_blanked_prompt() {
    let (_, mut g) = grid_after(&[PRIOR_COMMAND, b"\x1b]133;A\x07----------\r\n% "]);

    let remap = g.reflow(6, 6).expect("geometry changed");

    assert_eq!(
        remap.remap_row(1),
        Some(1),
        "the output row keeps its anchor"
    );
    assert_eq!(remap.remap_row(2), None, "the prompt's first row");
    assert_eq!(remap.remap_row(3), None, "the cursor's row");
}

#[derive(Debug, Clone)]
enum ShellPiece {
    Text(String),
    Newline,
    PromptStart {
        redraw: Option<&'static str>,
        continuation: bool,
    },
    InputStart,
    OutputStart,
    CommandEnd,
}

fn shell_text() -> impl Strategy<Value = ShellPiece> {
    "[ab -]{1,14}".prop_map(ShellPiece::Text)
}

fn redraw_option() -> impl Strategy<Value = Option<&'static str>> {
    prop::sample::select(vec![None, Some("0"), Some("1"), Some("last")])
}

fn prompt_start() -> impl Strategy<Value = ShellPiece> {
    (redraw_option(), prop::bool::weighted(0.2)).prop_map(|(redraw, continuation)| {
        ShellPiece::PromptStart {
            redraw,
            continuation,
        }
    })
}

fn shell_piece(prompt_weight: u32, command_weight: u32) -> impl Strategy<Value = ShellPiece> {
    prop_oneof![
        3 => shell_text(),
        2 => Just(ShellPiece::Newline),
        prompt_weight => prompt_start(),
        prompt_weight => Just(ShellPiece::InputStart),
        command_weight => Just(ShellPiece::OutputStart),
        command_weight => Just(ShellPiece::CommandEnd),
    ]
}

/// Arbitrary marked output, most often ending the way a shell leaves
/// the screen while it waits: at a prompt after a finished command.
fn shell_session() -> impl Strategy<Value = Vec<ShellPiece>> {
    let prompt = (
        prop::bool::weighted(0.7),
        prop::option::weighted(0.3, shell_text()),
        prop::bool::weighted(0.8),
        redraw_option(),
        prop::collection::vec(shell_piece(2, 1), 0..6),
    );
    (
        prop::collection::vec(shell_piece(1, 2), 0..10),
        prop::option::weighted(0.8, prompt),
    )
        .prop_map(|(mut pieces, prompt)| {
            if let Some((finished_command, text, newline, redraw, tail)) = prompt {
                if finished_command {
                    pieces.extend([ShellPiece::OutputStart, ShellPiece::CommandEnd]);
                }
                pieces.extend(text);
                if newline {
                    pieces.push(ShellPiece::Newline);
                }
                pieces.push(ShellPiece::PromptStart {
                    redraw,
                    continuation: false,
                });
                pieces.extend(tail);
            }
            pieces
        })
}

/// Append-only output: with no bare CR or cursor motion before the
/// final climb, every printed character survives in reading order.
fn shell_bytes(pieces: &[ShellPiece], climb: u16, marked: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for piece in pieces {
        match piece {
            ShellPiece::Text(s) => out.extend_from_slice(s.as_bytes()),
            ShellPiece::Newline => out.extend_from_slice(b"\r\n"),
            _ if !marked => {}
            ShellPiece::PromptStart {
                redraw,
                continuation,
            } => {
                out.extend_from_slice(b"\x1b]133;A");
                if let Some(r) = redraw {
                    out.extend_from_slice(format!(";redraw={r}").as_bytes());
                }
                if *continuation {
                    out.extend_from_slice(b";k=s");
                }
                out.push(0x07);
            }
            ShellPiece::InputStart => out.extend_from_slice(b"\x1b]133;B\x07"),
            ShellPiece::OutputStart => out.extend_from_slice(b"\x1b]133;C\x07"),
            ShellPiece::CommandEnd => out.extend_from_slice(b"\x1b]133;D;0\x07"),
        }
    }
    if climb > 0 {
        out.extend_from_slice(format!("\x1b[{climb}A").as_bytes());
    }
    out
}

/// The youngest prompt start that is not a `k=s` continuation, and
/// what the marks around it say a resize may blank, read from the
/// rules as the spec states them.
struct YoungestPrompt {
    /// How many pieces precede it.
    pieces_before: usize,
    lines_before: usize,
    chars_before: usize,
    redraw_last: bool,
    blankable: bool,
}

fn youngest_prompt(pieces: &[ShellPiece]) -> Option<YoungestPrompt> {
    let mut saw_output_start = false;
    let mut command_open = false;
    let mut at_prompt = false;
    let mut at_column_0 = true;
    let (mut lines, mut chars) = (0, 0);
    let mut prompt: Option<YoungestPrompt> = None;
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            ShellPiece::Text(s) => {
                chars += s.bytes().filter(|&b| b != b' ').count();
                at_column_0 = false;
            }
            ShellPiece::Newline => {
                lines += 1;
                at_column_0 = true;
            }
            ShellPiece::PromptStart {
                redraw,
                continuation,
            } => {
                at_prompt = true;
                if !continuation {
                    prompt = Some(YoungestPrompt {
                        pieces_before: i,
                        lines_before: lines,
                        chars_before: chars,
                        redraw_last: *redraw == Some("last"),
                        blankable: at_column_0 && !command_open && *redraw != Some("0"),
                    });
                }
            }
            ShellPiece::InputStart => at_prompt = true,
            ShellPiece::OutputStart => {
                saw_output_start = true;
                command_open = true;
                at_prompt = false;
            }
            ShellPiece::CommandEnd => {
                command_open = false;
                at_prompt = false;
            }
        }
    }
    let mut prompt = prompt?;
    prompt.blankable &= saw_output_start && at_prompt;
    Some(prompt)
}

/// Scrollback and screen as logical lines, trailing blanks dropped.
fn logical_lines(g: &Grid) -> Vec<String> {
    let cols = usize::from(g.cols());
    let mut lines: Vec<String> = Vec::new();
    for (cells, continued) in g.screen.rows_with_wrap(AltScreenRows::Include) {
        let mut row: String = cells
            .iter()
            .map(|c| match c.grapheme {
                Grapheme::Ascii(b) => char::from(b),
                _ => ' ',
            })
            .collect();
        row.extend(std::iter::repeat_n(' ', cols.saturating_sub(row.len())));
        match lines.last_mut() {
            Some(line) if continued => line.push_str(&row),
            _ => lines.push(row),
        }
    }
    for line in &mut lines {
        line.truncate(line.trim_end().len());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn printed(lines: &[String]) -> String {
    lines.concat().chars().filter(|&c| c != ' ').collect()
}

/// Characters printed on the rows above and below the cursor's.
fn printed_beside_cursor(g: &Grid) -> (usize, usize) {
    let cursor = g.screen.scrollback().len() + usize::from(g.cursor().row);
    let (mut above, mut below) = (0, 0);
    for (i, (cells, _)) in g.screen.rows_with_wrap(AltScreenRows::Include).enumerate() {
        let n = cells
            .iter()
            .filter(|c| matches!(c.grapheme, Grapheme::Ascii(b) if b != b' '))
            .count();
        match i.cmp(&cursor) {
            std::cmp::Ordering::Less => above += n,
            std::cmp::Ordering::Greater => below += n,
            std::cmp::Ordering::Equal => {}
        }
    }
    (above, below)
}

fn driven(bytes: &[u8], size: (u16, u16)) -> Grid {
    let mut p = Parser::new();
    let mut g = Grid::new(size.0, size.1);
    drive(&mut p, &mut g, bytes);
    g
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// OSC 133 marks change a reflow only by blanking the prompt the
    /// shell repaints: what the spec exempts reflows exactly as if
    /// unmarked, and otherwise the blank takes the youngest prompt's
    /// rows from its start (`redraw=last`: the cursor's row) once that
    /// start is still on screen at or above the cursor.
    #[test]
    fn prompt_marks_change_a_reflow_only_by_blanking_the_prompt(
        pieces in shell_session(),
        climb in prop_oneof![2 => Just(0u16), 1 => 1u16..4],
        size in (3u16..=8, 2u16..=10),
        new_size in (1u16..=7, 1u16..=10),
    ) {
        let mut marked = driven(&shell_bytes(&pieces, climb, true), size);
        let mut unmarked = driven(&shell_bytes(&pieces, climb, false), size);
        let prompt = youngest_prompt(&pieces);
        let start_on_screen = prompt.as_ref().is_some_and(|p| {
            let at_start = driven(&shell_bytes(&pieces[..p.pieces_before], 0, true), size);
            let line = at_start.scrollback_total_pushed + u64::from(at_start.cursor().row);
            line.checked_sub(marked.scrollback_total_pushed)
                .is_some_and(|row| row <= u64::from(marked.cursor().row))
        });
        let (above, below) = printed_beside_cursor(&marked);
        let resized = marked.reflow(new_size.0, new_size.1).is_some();
        unmarked.reflow(new_size.0, new_size.1);

        let Some(prompt) = prompt.filter(|p| p.blankable && start_on_screen && resized) else {
            prop_assert_eq!(logical_lines(&marked), logical_lines(&unmarked));
            prop_assert_eq!(screen_ascii(&marked), screen_ascii(&unmarked));
            prop_assert_eq!(marked.cursor(), unmarked.cursor());
            return Ok(());
        };

        let (mut with, without) = (logical_lines(&marked), logical_lines(&unmarked));
        let keep = prompt.lines_before.min(without.len());
        // The blank's own rows are trailing padding, trimmed away.
        with.resize(with.len().max(keep), String::new());
        prop_assert_eq!(&with[..keep], &without[..keep]);

        let (with, without) = (printed(&with), printed(&without));
        let (kept, left) = if prompt.redraw_last {
            (above, below)
        } else {
            (prompt.chars_before, 0)
        };
        prop_assert_eq!(with.len(), kept + left, "{:?} from {:?}", with, without);
        prop_assert_eq!(&with[..kept], &without[..kept]);
        prop_assert_eq!(&with[kept..], &without[without.len() - left..]);
    }
}
