use std::fmt::Write as _;
use std::ops::RangeInclusive;
use std::sync::LazyLock;

use felis_vt::Parser;
use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::pty_effects::COMMAND_OUTPUT_YIELD;
use crate::test_support::drive;

const A: &str = "\x1b]133;A\x07";
const B: &str = "\x1b]133;B\x07";
const C: &str = "\x1b]133;C\x07";
const D: &str = "\x1b]133;D;0\x07";

fn capturing(rows: u16, cols: u16, scrollback: usize) -> Grid {
    let mut g = Grid::with_scrollback(rows, cols, scrollback);
    g.set_capture_command_output(true);
    g
}

fn feed(g: &mut Grid, bytes: &str) {
    drive(&mut Parser::new(), g, bytes.as_bytes());
}

fn outputs(g: &mut Grid) -> Vec<(String, OmittedOutput)> {
    g.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::CommandOutput { text, omitted } => Some((text, omitted)),
            _ => None,
        })
        .collect()
}

fn output_of(rows: u16, cols: u16, bytes: &str) -> Vec<(String, OmittedOutput)> {
    let mut g = capturing(rows, cols, 100);
    feed(&mut g, bytes);
    outputs(&mut g)
}

fn whole(text: &str) -> Vec<(String, OmittedOutput)> {
    vec![(text.to_owned(), OmittedOutput::None)]
}

fn numbered_lines(range: std::ops::Range<usize>) -> String {
    let mut lines = String::new();
    for i in range {
        let _ = write!(lines, "l{i}\r\n");
    }
    lines
}

/// A ZWJ chain of pictographs as long as [`crate::ClusterText::CAP`]
/// lets one cell hold.
fn max_cluster() -> String {
    let mut cluster = String::from("\u{1F468}");
    while cluster.len() + "\u{200D}\u{1F468}".len() <= crate::ClusterText::CAP {
        cluster.push_str("\u{200D}\u{1F468}");
    }
    cluster
}

static MAX_CLUSTER: LazyLock<String> = LazyLock::new(max_cluster);

/// One grapheme of generated output.
#[derive(Debug, Clone, Copy)]
enum Unit {
    Ascii(u8),
    Wide,
    Combining,
    MaxCluster,
}

impl Unit {
    const fn width(self) -> u16 {
        match self {
            Self::Ascii(_) | Self::Combining => 1,
            Self::Wide | Self::MaxCluster => 2,
        }
    }

    fn push_to(self, s: &mut String) {
        match self {
            Self::Ascii(b) => s.push(char::from(b)),
            Self::Wide => s.push('\u{6F22}'),
            Self::Combining => s.push_str("e\u{301}"),
            Self::MaxCluster => s.push_str(&MAX_CLUSTER),
        }
    }
}

/// Bytes either side of [`MAX_ANNOUNCE_BYTES`] a near-cap line spans:
/// fewer than the blank rows generated below one, so a row that fits
/// only because blank rows cost nothing is common.
const NEAR: usize = 128;

/// Wide enough that a line of [`MAX_ANNOUNCE_BYTES`] + [`NEAR`] bytes
/// of [`sized_line`] fits one row.
fn big_cols() -> u16 {
    let unit = MAX_CLUSTER.len();
    u16::try_from(2 * ((MAX_ANNOUNCE_BYTES + NEAR) / unit) + unit).unwrap()
}

/// Mixed-width lines, or an ASCII line exactly one row wide, which
/// leaves a wrap pending when it ends the output.
fn small_line(cols: u16) -> impl Strategy<Value = Vec<Unit>> {
    let unit = prop_oneof![
        6 => (b'a'..=b'z').prop_map(Unit::Ascii),
        2 => Just(Unit::Wide),
        1 => Just(Unit::Combining),
        1 => Just(Unit::MaxCluster),
    ];
    prop_oneof![
        4 => vec(unit, 0..=36),
        1 => vec((b'a'..=b'z').prop_map(Unit::Ascii), usize::from(cols)),
    ]
}

/// Maximum-size clusters with an ASCII tail, `bytes` long, so the cut
/// of an over-cap row lands inside a cluster run.
fn sized_line(bytes: usize) -> Vec<Unit> {
    let unit = MAX_CLUSTER.len();
    let mut line = vec![Unit::MaxCluster; bytes / unit];
    line.extend(std::iter::repeat_n(Unit::Ascii(b'f'), bytes % unit));
    line
}

fn big_line() -> impl Strategy<Value = Vec<Unit>> {
    prop_oneof![
        1 => 1..=64usize,
        3 => MAX_ANNOUNCE_BYTES - NEAR..=MAX_ANNOUNCE_BYTES + NEAR,
        1 => 0..=MAX_ANNOUNCE_BYTES + NEAR,
    ]
    .prop_map(sized_line)
}

/// Lines interleaved with runs of blank lines long enough to reach the
/// row cap.
fn lines(
    line: impl Strategy<Value = Vec<Unit>>,
    chunks: RangeInclusive<usize>,
) -> impl Strategy<Value = Vec<Vec<Unit>>> {
    vec(
        prop_oneof![
            2 => line.prop_map(|l| vec![l]),
            1 => (1..=300usize).prop_map(|n| vec![Vec::new(); n]),
        ],
        chunks,
    )
    .prop_map(|chunks| chunks.into_iter().flatten().collect())
}

/// Output between a `C` and a `D`, with no cell moved in between.
#[derive(Debug, Clone)]
struct Command {
    rows: u16,
    cols: u16,
    scrollback: usize,
    /// Rows printed before the command row.
    history: usize,
    /// Cells printed on the command row before `C`; `cols` leaves a
    /// wrap pending.
    prompt: u16,
    lines: Vec<Vec<Unit>>,
    final_newline: bool,
}

/// Wide rows end on a line of any size over a run of blank lines, the
/// shape where blank rows decide whether the youngest text fits. The
/// scrollback keeps everything, keeps little, or sits within a row of
/// keeping exactly the output's first row.
fn command() -> impl Strategy<Value = Command> {
    let shape = prop_oneof![
        (2u16..=12).prop_flat_map(|cols| (Just(cols), lines(small_line(cols), 1..=40))),
        (
            Just(big_cols()),
            (lines(big_line(), 0..=6), big_line(), 0..=300usize).prop_map(
                |(mut lines, youngest, blank)| {
                    lines.push(youngest);
                    lines.extend(std::iter::repeat_n(Vec::new(), blank));
                    lines
                }
            ),
        ),
    ];
    (shape, 2u16..=6, 0..=8usize, any::<bool>())
        .prop_flat_map(|((cols, lines), rows, history, final_newline)| {
            prop_oneof![Just(0), Just(cols), 0..=cols].prop_map(move |prompt| Command {
                rows,
                cols,
                scrollback: 0,
                history,
                prompt,
                lines: lines.clone(),
                final_newline,
            })
        })
        .prop_flat_map(|command| {
            let (_, first_line, cursor_line) = command.layout();
            let edge = command.pushed(cursor_line).saturating_sub(first_line);
            let edge = usize::try_from(edge).unwrap();
            prop_oneof![0..=40usize, Just(400), edge.saturating_sub(1)..=edge + 1].prop_map(
                move |scrollback| Command {
                    scrollback,
                    ..command.clone()
                },
            )
        })
}

#[derive(Debug, Default)]
struct ModelRow {
    text: String,
    continued: bool,
}

/// The text of rows joined the way the cut joins them, and where each
/// row's text starts in it.
struct Tails {
    text: String,
    starts: Vec<usize>,
}

impl Tails {
    fn new(rows: &[ModelRow]) -> Self {
        let mut text = String::new();
        let mut starts = Vec::with_capacity(rows.len() + 1);
        for (i, row) in rows.iter().enumerate() {
            if i > 0 && !row.continued {
                text.push('\n');
            }
            starts.push(text.len());
            text.push_str(&row.text);
        }
        starts.push(text.len());
        text.truncate(text.trim_end_matches('\n').len());
        Self { text, starts }
    }

    /// The joined text of rows `k..`.
    fn from(&self, k: usize) -> &str {
        &self.text[self.starts[k].min(self.text.len())..]
    }
}

impl Command {
    fn bytes(&self) -> String {
        let mut s = "h\r\n".repeat(self.history);
        s.push_str(&"x".repeat(usize::from(self.prompt)));
        s.push_str(C);
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                s.push_str("\r\n");
            }
            for unit in line {
                unit.push_to(&mut s);
            }
        }
        if self.final_newline {
            s.push_str("\r\n");
        }
        s.push_str(D);
        s
    }

    /// The output's screen rows, the absolute line of the first, and
    /// the line the cursor stands on at `D`.
    fn layout(&self) -> (Vec<ModelRow>, u64, u64) {
        let mut rows = vec![ModelRow::default()];
        let mut col = self.prompt;
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                rows.push(ModelRow::default());
                col = 0;
            }
            for unit in line {
                if col + unit.width() > self.cols {
                    rows.push(ModelRow {
                        text: String::new(),
                        continued: true,
                    });
                    col = 0;
                }
                unit.push_to(&mut rows.last_mut().unwrap().text);
                col += unit.width();
            }
        }
        let history = u64::try_from(self.history).unwrap();
        let cursor_line =
            history + u64::try_from(rows.len()).unwrap() - 1 + u64::from(self.final_newline);
        let mut first_line = history;
        if self.prompt == self.cols {
            rows.remove(0);
            first_line += 1;
        }
        if !self.final_newline && rows.len() > 1 && rows.last().is_some_and(|r| r.text.is_empty()) {
            rows.pop();
        }
        (rows, first_line, cursor_line)
    }

    /// Rows scrolled into history by the time the cursor reaches
    /// `cursor_line`.
    fn pushed(&self, cursor_line: u64) -> u64 {
        (cursor_line + 1).saturating_sub(u64::from(self.rows))
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Undisturbed output is cut to the youngest rows that fit both
    /// caps: whole when it fits, `Rows(n)` for the `n` rows dropped,
    /// `Unknown` when the start left scrollback or the youngest row
    /// with text alone overflows the byte cap and keeps only its tail.
    #[test]
    fn command_output_is_the_youngest_text_within_the_caps(command in command()) {
        let mut g = capturing(command.rows, command.cols, command.scrollback);
        feed(&mut g, &command.bytes());
        let got = outputs(&mut g);
        prop_assert_eq!(got.len(), 1);
        let (text, omitted) = &got[0];

        let (rows, first_line, cursor_line) = command.layout();
        let pushed = command.pushed(cursor_line);
        let oldest = pushed - pushed.min(u64::try_from(command.scrollback).unwrap());
        let evicted = oldest > first_line;

        prop_assert!(text.len() <= MAX_ANNOUNCE_BYTES);
        prop_assert!(text.split('\n').count() <= MAX_ANNOUNCE_ROWS);
        prop_assert!(Tails::new(&rows).from(0).ends_with(text.as_str()));

        let retained = &rows[usize::try_from(oldest.saturating_sub(first_line)).unwrap()..];
        let tails = Tails::new(retained);
        let n = retained.len();
        let k = (0..=n)
            .find(|&k| n - k <= MAX_ANNOUNCE_ROWS && tails.from(k).len() <= MAX_ANNOUNCE_BYTES)
            .unwrap();
        if k > 0 && n - k < MAX_ANNOUNCE_ROWS && tails.from(k).is_empty() {
            prop_assert!(tails.from(k - 1).ends_with(text.as_str()));
            prop_assert!(text.len() + 4 > MAX_ANNOUNCE_BYTES, "cut on the char boundary nearest the cap");
            prop_assert_eq!(*omitted, OmittedOutput::Unknown);
        } else {
            prop_assert_eq!(text.as_str(), tails.from(k));
            let expected = if evicted {
                OmittedOutput::Unknown
            } else if k > 0 {
                OmittedOutput::Rows(u32::try_from(k).unwrap())
            } else {
                OmittedOutput::None
            };
            prop_assert_eq!(*omitted, expected);
        }
    }
}

/// Screen rows a cell move shifts, and for a sideways move the first
/// column it shifts.
enum Moved {
    Rows(RangeInclusive<u16>),
    Columns(RangeInclusive<u16>, u16),
}

/// On an 8×20 screen, rows and columns 0-based.
const CELL_MOVES: &[(&str, &str, Moved)] = &[
    ("IL", "\x1b[3;1H\x1b[L", Moved::Rows(2..=7)),
    ("DL", "\x1b[3;1H\x1b[M", Moved::Rows(2..=7)),
    ("band scroll up", "\x1b[3;7r\x1b[S", Moved::Rows(2..=6)),
    ("band line feed", "\x1b[3;7r\x1b[7;1H\n", Moved::Rows(2..=6)),
    ("band scroll down", "\x1b[3;7r\x1b[T", Moved::Rows(2..=6)),
    (
        "reverse index",
        "\x1b[3;7r\x1b[3;1H\x1bM",
        Moved::Rows(2..=6),
    ),
    (
        "subrectangle scroll",
        "\x1b[?69h\x1b[5;10s\x1b[3;7r\x1b[S\x1b[?69l",
        Moved::Rows(2..=6),
    ),
    ("ICH", "\x1b[5;6H\x1b[@", Moved::Columns(4..=4, 5)),
    ("DCH", "\x1b[5;6H\x1b[P", Moved::Columns(4..=4, 5)),
    (
        "IRM print",
        "\x1b[5;6H\x1b[4hX\x1b[4l",
        Moved::Columns(4..=4, 5),
    ),
    ("SL", "\x1b[3;7r\x1b[ @", Moved::Columns(2..=6, 0)),
    ("SR", "\x1b[3;7r\x1b[ A", Moved::Columns(2..=6, 0)),
    (
        "DECIC",
        "\x1b[3;7r\x1b[5;6H\x1b['}",
        Moved::Columns(2..=6, 5),
    ),
    (
        "DECDC",
        "\x1b[3;7r\x1b[5;6H\x1b['~",
        Moved::Columns(2..=6, 5),
    ),
    ("DECBI", "\x1b[3;7r\x1b[5;1H\x1b6", Moved::Columns(2..=6, 0)),
    (
        "DECFI",
        "\x1b[3;7r\x1b[5;20H\x1b9",
        Moved::Columns(2..=6, 0),
    ),
];

/// Every start position against every cell move: a start whose cell
/// moved yields no output; any other yields its row from the start
/// column, as the row stands at `D`.
#[test]
fn a_cell_move_turns_exactly_the_starts_it_moves_into_no_output() {
    for (name, op, moved) in CELL_MOVES {
        for row in 0u16..7 {
            for col in 0u16..20 {
                let lost = match moved {
                    Moved::Rows(rows) => rows.contains(&row),
                    Moved::Columns(rows, first) => rows.contains(&row) && col > *first,
                };
                let mut g = capturing(8, 20, 100);
                feed(
                    &mut g,
                    &format!(
                        "\x1b[{};{}H{C}o{op}\x1b[r\x1b[{};1H{D}",
                        row + 1,
                        col + 1,
                        row + 2
                    ),
                );
                let expected = if lost {
                    vec![]
                } else {
                    let line = row_text_trim(g.row_content(row).unwrap(), g.cluster_table());
                    whole(line.get(usize::from(col)..).unwrap_or_default())
                };
                assert_eq!(outputs(&mut g), expected, "{name}: start at {row},{col}");
            }
        }
    }
}

#[test]
fn a_start_below_a_partial_top_scrolling_region_keeps_its_row() {
    let mut g = capturing(6, 20, 100);
    feed(
        &mut g,
        &format!("\x1b[1;3r\x1b[5;1H{C}out\x1b[3;1H\n\n\n\x1b[5;4H{D}"),
    );
    assert_eq!(outputs(&mut g), whole("out"));
}

#[test]
fn a_start_inside_a_partial_top_scrolling_region_moves_with_its_row() {
    let mut g = capturing(6, 20, 100);
    feed(
        &mut g,
        &format!("\x1b[1;3r\x1b[3;1H{C}abc\r\n\n\n\x1b[r\x1b[3;1Hdef\r\n{D}"),
    );
    assert_eq!(outputs(&mut g), whole("abc\n\n\ndef"));
}

#[test]
fn ed_3_with_the_start_in_history_yields_the_screen_as_unknown() {
    let mut g = capturing(3, 10, 100);
    feed(&mut g, &format!("$ x\r\n{C}a\r\nb\r\nc\r\nd\r\n\x1b[3J{D}"));
    assert_eq!(
        outputs(&mut g),
        [("c\nd".to_owned(), OmittedOutput::Unknown)]
    );
}

#[test]
fn ed_3_with_the_start_on_screen_keeps_it() {
    let mut g = capturing(3, 10, 100);
    feed(
        &mut g,
        &format!("a\r\nb\r\nc\r\n$ x\r\n{C}out\r\n\x1b[3J{D}"),
    );
    assert_eq!(outputs(&mut g), whole("out"));
}

#[test]
fn ris_during_a_command_leaves_no_start_and_keeps_capturing() {
    let mut g = capturing(5, 10, 100);
    feed(&mut g, &format!("$ x\r\n{C}a\r\n\x1bc{D}{C}b\r\n{D}"));
    assert_eq!(outputs(&mut g), whole("b"));
}

#[test]
fn a_burst_of_commands_yields_after_each_budget_and_drops_none() {
    let mut g = capturing(5, 10, 100);
    let pair = format!("{C}x\r\n{D}");
    let bytes = pair.repeat(COMMAND_OUTPUT_YIELD * 2 + 1);
    let mut p = Parser::new();
    let mut rest = bytes.as_bytes();
    let mut stops = Vec::new();
    let mut seen = 0;
    while let Some(n) = p.advance_until_yield(&mut g, rest) {
        stops.push(n);
        let drained = outputs(&mut g).len();
        assert_eq!(drained, COMMAND_OUTPUT_YIELD);
        seen += drained;
        rest = &rest[n..];
    }
    seen += outputs(&mut g).len();
    let budget = pair.len() * COMMAND_OUTPUT_YIELD;
    assert_eq!(stops, [budget, budget]);
    assert_eq!(seen, COMMAND_OUTPUT_YIELD * 2 + 1);
}

#[test]
fn a_reflow_keeps_a_start_at_the_head_of_a_logical_line() {
    for cols in [4, 20] {
        let mut g = capturing(5, 10, 100);
        feed(&mut g, &format!("$ x\r\n{C}abcdefghijkl\r\n"));
        g.reflow(5, cols);
        feed(&mut g, D);
        assert_eq!(outputs(&mut g), whole("abcdefghijkl"), "{cols} columns");
    }
}

#[test]
fn a_reflow_turns_any_other_start_into_no_output() {
    let mut g = capturing(5, 10, 100);
    feed(&mut g, &format!("$ x{C}out\r\n"));
    g.reflow(5, 20);
    feed(&mut g, D);
    assert_eq!(outputs(&mut g), []);
}

#[test]
fn a_d_with_no_c_yields_nothing() {
    assert_eq!(output_of(5, 10, &format!("$ x\r\nout\r\n{D}")), []);
}

#[test]
fn a_nested_shell_takes_the_start_and_the_outer_command_yields_nothing() {
    assert_eq!(
        output_of(
            10,
            20,
            &format!("$ bash\r\n{C}{A}$ {B}ls\r\n{C}inner\r\n{D}{A}$ exit\r\n{D}")
        ),
        whole("inner")
    );
}

#[test]
fn marks_on_the_alternate_screen_are_ignored() {
    assert_eq!(
        output_of(5, 10, &format!("\x1b[?1049h{C}alt\r\n{D}\x1b[?1049l")),
        []
    );
    assert_eq!(
        output_of(
            5,
            10,
            &format!("$ vim\r\n{C}\x1b[?1049h{A}{C}alt\r\n{D}\x1b[?1049ldone\r\n{D}")
        ),
        whole("done")
    );
}

#[test]
fn capture_off_yields_no_effect_but_tracks_the_start() {
    let mut g = Grid::with_scrollback(5, 10, 100);
    feed(&mut g, &format!("$ x\r\n{C}a\r\n{D}$ y\r\n{C}b\r\n"));
    assert_eq!(outputs(&mut g), []);
    g.set_capture_command_output(true);
    feed(&mut g, D);
    assert_eq!(outputs(&mut g), whole("b"));
}

#[test]
fn two_commands_in_one_batch_yield_two_outputs_in_order() {
    assert_eq!(
        output_of(
            10,
            20,
            &format!("$ a\r\n{C}1\r\n{D}{A}$ b\r\n{C}2\r\n{D}{A}$ ")
        ),
        [
            ("1".to_owned(), OmittedOutput::None),
            ("2".to_owned(), OmittedOutput::None)
        ]
    );
}

#[test]
fn trailing_blank_rows_count_toward_the_row_cap() {
    let mut g = capturing(5, 10, 1000);
    feed(
        &mut g,
        &format!("$ x\r\n{C}old\r\n{}{D}", "\r\n".repeat(300)),
    );
    assert_eq!(outputs(&mut g), [(String::new(), OmittedOutput::Rows(45))]);
}

#[test]
fn a_near_cap_row_above_many_blank_rows_fits_whole() {
    let cluster = max_cluster();
    let per_row = (MAX_ANNOUNCE_BYTES - 50) / cluster.len();
    let row: String = cluster.repeat(per_row);
    let mut g = capturing(5, u16::try_from(per_row * 2 + 2).unwrap(), 1000);
    feed(
        &mut g,
        &format!("$ x\r\n{C}{row}\r\n{}{D}", "\r\n".repeat(255)),
    );
    assert_eq!(outputs(&mut g), whole(&row));
}

#[test]
fn a_start_with_a_wrap_pending_counts_no_row_of_its_own() {
    let mut g = capturing(5, 4, 1000);
    feed(&mut g, &format!("$ xx{C}{}{D}", numbered_lines(0..256)));
    let lines: Vec<String> = (0..256).map(|i| format!("l{i}")).collect();
    assert_eq!(outputs(&mut g), whole(&lines.join("\n")));
}

#[test]
fn evicting_only_the_command_row_of_a_wrap_pending_start_loses_nothing() {
    let mut g = capturing(3, 4, 2);
    feed(&mut g, &format!("$ xx{C}a\r\nb\r\nc\r\nd\r\n{D}"));
    assert_eq!(outputs(&mut g), whole("a\nb\nc\nd"));
}
