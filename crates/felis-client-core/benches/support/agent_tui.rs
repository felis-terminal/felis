//! A coding agent's inline TUI at 120x40, one synchronized write per
//! update: typed keys, spinner ticks, streamed lines and pushed lines in
//! the mix and shapes a capture of Claude Code showed.

use std::fmt::Write as _;

use felis_client_core::ShadowScreen;
use felis_grid::Grid;
use felis_vt::Parser;

use crate::workload::{apply_diffs, emit_diffs_with};

pub(crate) const ROWS: u16 = 40;
pub(crate) const COLS: u16 = 120;

/// Rule, prompt, rule, three status lines and a blank.
const BOX_ROWS: u16 = 7;
const PROMPT: &str = "\u{276f} ";
const PROMPT_COLS: u16 = 2;
const SPINNER: [char; 6] = [
    '\u{b7}', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}', '\u{273d}',
];
const WORDS: [&str; 24] = [
    "the",
    "daemon",
    "keeps",
    "each",
    "session",
    "while",
    "no",
    "window",
    "is",
    "attached,",
    "so",
    "closing",
    "it",
    "does",
    "not",
    "end",
    "the",
    "shell;",
    "a",
    "client",
    "only",
    "renders",
    "what",
    "arrives.",
];
const LINKS: [&str; 3] = ["design.md", "principles.md", "non-goals.md"];

#[derive(Default)]
pub(crate) struct AgentTui {
    writes: Vec<Vec<u8>>,
    /// Row of the box's top rule; the spinner sits two rows above it and
    /// the paragraph in progress three.
    box_top: u16,
    input: u16,
    words: usize,
    ticks: usize,
    scrolled: usize,
    last_line: String,
}

pub(crate) struct Expected {
    pub(crate) cursor: (u16, u16),
    pub(crate) scrolled: usize,
    /// The last pushed line, which ends up on the row above the spinner.
    pub(crate) last_line: String,
}

impl AgentTui {
    /// The session's writes in order, and where they leave the screen.
    pub(crate) fn writes() -> (Vec<Vec<u8>>, Expected) {
        let mut tui = Self {
            box_top: 3,
            ..Self::default()
        };
        tui.start();
        tui.type_line(110);
        tui.push();
        tui.tick_n(70);
        for line in 0..50 {
            tui.push();
            if line % 2 == 0 {
                tui.rewrite();
            }
            if line % 5 == 4 {
                tui.tick();
            }
        }
        tui.type_line(50);
        tui.push();
        tui.tick_n(20);
        tui.type_line(30);
        for _ in 0..6 {
            tui.push();
        }
        let expected = Expected {
            cursor: (tui.box_top + 1, PROMPT_COLS + tui.input),
            scrolled: tui.scrolled,
            last_line: tui.last_line,
        };
        (tui.writes, expected)
    }

    fn frame(&mut self, body: &str) {
        let mut w = Vec::with_capacity(body.len() + 16);
        w.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        w.extend_from_slice(body.as_bytes());
        w.extend_from_slice(b"\x1b[?25h\x1b[?2026l");
        self.writes.push(w);
    }

    const fn col(&self) -> u16 {
        PROMPT_COLS + self.input
    }

    fn start(&mut self) {
        let mut s = String::from("\x1b[?2004h\x1b[?1004h\x1b[?2031h");
        s.push_str("\x1b[1m Agent\x1b[22m v2 \u{b7} ~/src/felis\r\n\r\n\r\n");
        self.draw_box(&mut s);
        self.frame(&s);
    }

    fn draw_box(&self, s: &mut String) {
        self.box_rows(s, "\r\x1b[1B");
        let _ = write!(s, "\r\x1b[5A\x1b[{}C", self.col());
    }

    fn box_rows(&self, s: &mut String, sep: &str) {
        let rule = "\u{2500}".repeat(usize::from(COLS));
        let input = "x".repeat(usize::from(self.input));
        let _ = write!(s, "\x1b[38;2;136;136;136m{rule}\x1b[39m{sep}");
        let _ = write!(
            s,
            "\x1b[38;2;153;153;153m{PROMPT}\x1b[39m{input}\x1b[K{sep}"
        );
        let _ = write!(s, "\x1b[38;2;136;136;136m{rule}\x1b[39m{sep}");
        let _ = write!(
            s,
            "\x1b[2C\x1b[38;2;255;193;7m\u{26a0} Notice\x1b[38;2;153;153;153m \u{b7} a status line the agent repaints\x1b[39m\x1b[K{sep}"
        );
        let _ = write!(
            s,
            "\x1b[2C\x1b[38;2;153;153;153mmodel/medium | ctx:6% | v2\x1b[39m\x1b[K{sep}"
        );
        let _ = write!(
            s,
            "\x1b[2C\x1b[38;2;255;193;7m\u{23f5}\u{23f5} auto mode on\x1b[38;2;153;153;153m (shift+tab to cycle)\x1b[39m\x1b[K{sep}"
        );
        s.push_str("\x1b[K");
    }

    fn type_line(&mut self, keys: u16) {
        assert!(
            self.col() + keys < COLS,
            "a typed line stays on the prompt row"
        );
        for _ in 0..keys {
            let c = self.col();
            let s = format!(
                "\x1b[{c}D\x1b[5B\r\x1b[{c}C\x1b[5Ax\r\r\n\r\n\r\n\r\n\r\n\x1b[{}C\x1b[5A",
                c + 1
            );
            self.frame(&s);
            self.input += 1;
        }
    }

    fn tick(&mut self) {
        let c = self.col();
        let glyph = SPINNER[self.ticks % SPINNER.len()];
        self.ticks += 1;
        let mut s = format!(
            "\x1b[{c}D\x1b[5B\r\x1b[8A\x1b[38;2;235;159;127m{glyph}\x1b[39m \x1b[3G\x1b[38;2;181;181;181mthinking ({}s)\x1b[39m\r",
            self.ticks / 10
        );
        s.push_str(&"\r\n".repeat(8));
        let _ = write!(s, "\x1b[{c}C\x1b[5A");
        self.frame(&s);
    }

    fn tick_n(&mut self, n: usize) {
        for _ in 0..n {
            self.tick();
        }
    }

    fn line(&mut self, s: &mut String) {
        s.push_str("\x1b[2K");
        self.last_line.clear();
        let mut col = 3usize;
        let mut first = true;
        loop {
            let i = self.words;
            let link = i % 11 == 7;
            let word = if link {
                LINKS[i % LINKS.len()]
            } else {
                WORDS[i % WORDS.len()]
            };
            if col + word.len() > usize::from(COLS) - 4 {
                break;
            }
            self.words += 1;
            if !first {
                let _ = write!(s, "\x1b[{col}G");
                self.last_line.push(' ');
            }
            self.last_line.push_str(word);
            first = false;
            if link {
                let _ = write!(s, "\x1b[38;2;177;185;249m{word}\x1b[39m");
            } else {
                s.push_str(word);
            }
            col += word.len() + 1;
        }
        s.push_str("\x1b[K");
    }

    fn rewrite(&mut self) {
        let c = self.col();
        let mut s = format!("\x1b[{c}D\x1b[4A\r\x1b[2C");
        self.line(&mut s);
        let _ = write!(s, "\r\x1b[4B\x1b[{c}C");
        self.frame(&s);
    }

    fn push(&mut self) {
        let mut s = format!("\x1b[{}D\x1b[3A\r\x1b[2C", self.col());
        self.input = 0;
        self.line(&mut s);
        s.push_str("\r\n\x1b[38;2;235;159;127m\u{2722}\x1b[39m\x1b[K\r\n\x1b[K\r\n");
        self.box_rows(&mut s, "\r\n");
        let _ = write!(s, "\r\x1b[5A\x1b[{}C", self.col());
        if self.box_top + BOX_ROWS < ROWS {
            self.box_top += 1;
        } else {
            self.scrolled += 1;
        }
        self.frame(&s);
    }
}

pub(crate) struct AgentPipeline {
    pub(crate) parser: Parser,
    pub(crate) grid: Grid,
    pub(crate) shadow: ShadowScreen,
    pub(crate) scratch: Vec<u8>,
}

impl AgentPipeline {
    pub(crate) fn new() -> Self {
        Self {
            parser: Parser::new(),
            grid: Grid::new(ROWS, COLS),
            shadow: ShadowScreen::new(ROWS, COLS),
            scratch: Vec::with_capacity(64 * 1024),
        }
    }

    pub(crate) fn feed(&mut self, writes: &[Vec<u8>]) {
        for w in writes {
            self.parser.advance(&mut self.grid, w);
            emit_diffs_with(&mut self.grid, &mut self.scratch, true);
            apply_diffs(&self.scratch, &mut self.shadow);
        }
    }

    pub(crate) fn check(&self, expected: &Expected) {
        let c = self.grid.cursor();
        assert_eq!((c.row, c.col), expected.cursor, "daemon cursor");
        assert_eq!(
            self.grid.scrollback().len(),
            expected.scrolled,
            "rows scrolled into history"
        );
        let paragraph = self
            .grid
            .row_cells(expected.cursor.0 - 4)
            .expect("the paragraph row is on screen");
        assert_eq!(
            felis_grid::row_text_trim(paragraph, self.grid.cluster_table()),
            format!("  {}", expected.last_line),
            "last pushed line"
        );
        let text = |cells: Option<&[felis_grid::Cell]>| -> Vec<felis_grid::Grapheme> {
            cells
                .unwrap_or(&[])
                .iter()
                .map(|cell| cell.grapheme)
                .collect()
        };
        for row in 0..ROWS {
            assert_eq!(
                text(self.shadow.screen().row_cells(row)),
                text(self.grid.row_cells(row)),
                "client row {row}"
            );
        }
    }
}
