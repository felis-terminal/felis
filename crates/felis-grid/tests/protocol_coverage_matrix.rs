//! Protocol coverage matrix: for every CSI / OSC sequence
//! `docs/reference/protocols/support-matrix.md` marks supported (a ⚠️ row is
//! tolerated and mutates nothing), fire a representative byte sequence and
//! assert the grid mutated something observable. Semantic correctness is
//! pinned by `tests/snapshot_csi.rs` / `tests/snapshot_csi_handlers.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![expect(
    clippy::struct_excessive_bools,
    clippy::same_item_push,
    reason = "test harness: the fingerprint struct mirrors independent grid state bits, and the explicit fill loop reads clearer than a resize"
)]

use felis_grid::{Cell, Grapheme, Grid, ModeSnapshot};
use felis_protocol::messages::ThemeChannel;
use felis_vt::Parser;

const ROWS: u16 = 12;
const COLS: u16 = 24;

#[derive(PartialEq, Eq, Debug, Clone)]
struct GridFingerprint {
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    cursor_pending_wrap: bool,
    cursor_style: u8,
    cells: Vec<Cell>,
    modes: ModeSnapshot,
    on_alt_screen: bool,
    reverse_video: bool,
    focus_reporting: bool,
    synchronized_output: bool,
    application_keypad: bool,
    autowrap: bool,
    origin_mode: bool,
    insert_mode: bool,
    c1_8bit: bool,
    title: Option<String>,
    cwd: Option<String>,
    kitty_kbd_flags: u8,
    kitty_kbd_stack_depth: usize,
    prompt_marks_len: usize,
    dirty_row_count: usize,
    scrollback_len: usize,
}

impl GridFingerprint {
    fn snapshot(grid: &Grid) -> Self {
        let cells: Vec<Cell> = (0..grid.rows())
            .flat_map(|r| (0..grid.cols()).map(move |c| (r, c)))
            .map(|(r, c)| grid.cell(r, c).copied().unwrap_or_default())
            .collect();
        let cursor = grid.cursor();
        Self {
            cursor_row: cursor.row,
            cursor_col: cursor.col,
            cursor_visible: cursor.visible,
            cursor_pending_wrap: cursor.pending_wrap,
            cursor_style: grid.cursor_style() as u8,
            cells,
            modes: grid.mode_snapshot(),
            on_alt_screen: grid.on_alternate_screen(),
            reverse_video: grid.reverse_video(),
            focus_reporting: grid.focus_reporting(),
            synchronized_output: grid.synchronized_output(),
            application_keypad: grid.application_keypad(),
            autowrap: grid.autowrap(),
            origin_mode: grid.origin_mode(),
            insert_mode: grid.insert_mode(),
            c1_8bit: grid.c1_8bit(),
            title: grid.title().map(str::to_owned),
            cwd: grid.cwd().map(str::to_owned),
            kitty_kbd_flags: grid.kitty_kbd_flags().bits(),
            kitty_kbd_stack_depth: grid.kitty_kbd_stack_depth(),
            prompt_marks_len: grid.prompt_marks().len(),
            dirty_row_count: grid.damage().dirty_rows().count(),
            scrollback_len: grid.scrollback().len(),
        }
    }
}

struct Probe {
    name: &'static str,
    sequence: &'static [u8],
}

fn seed_state(grid: &mut Grid, parser: &mut Parser) {
    let mut filler = Vec::with_capacity(usize::from(ROWS) * (usize::from(COLS) + 2));
    for _ in 0..ROWS {
        for _ in 0..COLS {
            filler.push(b'X');
        }
        filler.extend_from_slice(b"\r\n");
    }
    filler.truncate(filler.len().saturating_sub(2));
    parser.advance(grid, &filler);
    // Scrollback for the `ED 3` probe, which touches scrollback only.
    for _ in 0..3 {
        parser.advance(grid, b"\r\n");
    }
    parser.advance(grid, b"\x1b[5;5H");
}

fn assert_probe_changes_state(probe: &Probe) {
    let mut grid = Grid::new(ROWS, COLS);
    let mut parser = Parser::new();
    seed_state(&mut grid, &mut parser);
    let before = GridFingerprint::snapshot(&grid);
    parser.advance(&mut grid, probe.sequence);
    let after = GridFingerprint::snapshot(&grid);
    let drained_signal = grid.take_bell_pending()
        // DECSTR's only observable here is its erased-range effect: it does
        // not move the cursor (xterm) and the seed has no non-default modes.
        || !grid.take_pty_effects().is_empty()
        || grid.take_pending_clipboard_set().is_some()
        || grid.take_title_dirty().is_some()
        || grid.take_cwd_dirty().is_some()
        || grid.take_kitty_kbd_dirty().is_some()
        || grid.take_theme_dirty(ThemeChannel::Foreground).is_some()
        || grid.take_theme_dirty(ThemeChannel::Background).is_some()
        || grid.take_theme_dirty(ThemeChannel::Cursor).is_some();
    assert!(
        before != after || drained_signal,
        "probe `{}` left grid state unchanged — possible silent no-op handler. \
         sequence: {:?}",
        probe.name,
        probe.sequence,
    );
}

const PROBES: &[Probe] = &[
    Probe {
        name: "CUP — `CSI 1;1 H`",
        sequence: b"\x1b[1;1H",
    },
    Probe {
        name: "CUU — `CSI 2 A`",
        sequence: b"\x1b[5;5H\x1b[2A",
    },
    Probe {
        name: "CUD — `CSI 1 B`",
        sequence: b"\x1b[1B",
    },
    Probe {
        name: "CUF — `CSI 3 C`",
        sequence: b"\x1b[3C",
    },
    Probe {
        name: "CUB — `CSI 1 D`",
        sequence: b"\x1b[5;5H\x1b[1D",
    },
    Probe {
        name: "CHA — `CSI 7 G`",
        sequence: b"\x1b[7G",
    },
    Probe {
        name: "VPA — `CSI 4 d`",
        sequence: b"\x1b[4d",
    },
    Probe {
        name: "HVP — `CSI 2;3 f`",
        sequence: b"\x1b[2;3f",
    },
    Probe {
        name: "CNL — `CSI 1 E`",
        sequence: b"\x1b[1E",
    },
    Probe {
        name: "CPL — `CSI 5;5 H` then `CSI 1 F`",
        sequence: b"\x1b[5;5H\x1b[1F",
    },
    Probe {
        name: "HPR — `CSI 2 a`",
        sequence: b"\x1b[2a",
    },
    Probe {
        name: "VPR — `CSI 2 e`",
        sequence: b"\x1b[2e",
    },
    Probe {
        name: "ED 0 — `CSI J` (clear-to-bottom)",
        sequence: b"\x1b[J",
    },
    Probe {
        name: "ED 1 — `CSI 1 J`",
        sequence: b"\x1b[1J",
    },
    Probe {
        name: "ED 2 — `CSI 2 J`",
        sequence: b"\x1b[2J",
    },
    Probe {
        name: "ED 3 — `CSI 3 J` (with scrollback)",
        sequence: b"\x1b[3J",
    },
    Probe {
        name: "EL 0 — `CSI K`",
        sequence: b"\x1b[K",
    },
    Probe {
        name: "EL 1 — `CSI 1 K`",
        sequence: b"\x1b[1 K\x1b[1K",
    },
    Probe {
        name: "EL 2 — `CSI 2 K`",
        sequence: b"\x1b[2K",
    },
    Probe {
        name: "ECH — `CSI 5 X`",
        sequence: b"\x1b[5X",
    },
    Probe {
        name: "DCH — `CSI 1 P` (regression: sl trail)",
        sequence: b"\x1b[1P",
    },
    Probe {
        name: "ICH — `CSI 1 @`",
        sequence: b"\x1b[1@",
    },
    Probe {
        name: "IL  — `CSI 1 L`",
        sequence: b"\x1b[1L",
    },
    Probe {
        name: "DL  — `CSI 1 M`",
        sequence: b"\x1b[1M",
    },
    Probe {
        name: "SU  — `CSI 1 S`",
        sequence: b"\x1b[1S",
    },
    Probe {
        name: "SD  — `CSI 1 T`",
        sequence: b"\x1b[1T",
    },
    Probe {
        name: "DECSTBM — `CSI 2;6 r`",
        sequence: b"\x1b[2;6r",
    },
    Probe {
        name: "SGR bold + color — `CSI 1;31 m` then print",
        sequence: b"\x1b[1;31mX",
    },
    Probe {
        name: "SGR underline-curly — `CSI 4:3 m`",
        sequence: b"\x1b[4:3mX",
    },
    Probe {
        name: "SGR true color — `CSI 38;2;1;2;3 m`",
        sequence: b"\x1b[38;2;1;2;3mX",
    },
    Probe {
        name: "SGR reset — `CSI 1 m` then `CSI 0 m`",
        sequence: b"\x1b[1m\x1b[0mX",
    },
    Probe {
        name: "DECTCEM hide — `CSI ?25 l`",
        sequence: b"\x1b[?25l",
    },
    Probe {
        name: "DECSCUSR — `CSI 4 SP q` (steady underline)",
        sequence: b"\x1b[4 q",
    },
    Probe {
        name: "DECSET 1000 (mouse buttons)",
        sequence: b"\x1b[?1000h",
    },
    Probe {
        name: "DECSET 1004 (focus events)",
        sequence: b"\x1b[?1004h",
    },
    Probe {
        name: "DECSET 2004 (bracketed paste)",
        sequence: b"\x1b[?2004h",
    },
    Probe {
        name: "DECSET 2026 (synchronized output)",
        sequence: b"\x1b[?2026h",
    },
    Probe {
        name: "DECSET 1049 (alt-screen)",
        sequence: b"\x1b[?1049h",
    },
    Probe {
        name: "DECSET 1 (DECCKM application cursor)",
        sequence: b"\x1b[?1h",
    },
    Probe {
        name: "IRM — `CSI 4 h` then print",
        sequence: b"\x1b[4hX",
    },
    Probe {
        name: "DECOM — `CSI ?6 h`",
        sequence: b"\x1b[?6h",
    },
    Probe {
        name: "DECAWM off — `CSI ?7 l`",
        sequence: b"\x1b[?7l",
    },
    Probe {
        name: "DECSCNM — `CSI ?5 h`",
        sequence: b"\x1b[?5h",
    },
    Probe {
        name: "HTS — `ESC H` then HT",
        sequence: b"\x1bH\t",
    },
    Probe {
        name: "TBC — `CSI 3 g` (clear all stops) + HT",
        sequence: b"\x1b[3g\x1b[1;1H\t\x1b[1;1H\t",
    },
    Probe {
        name: "CHT — `CSI 1 I`",
        sequence: b"\x1b[1I",
    },
    Probe {
        name: "CBT — `CSI 1 Z`",
        sequence: b"\x1b[1Z",
    },
    Probe {
        name: "DA1 — `CSI c`",
        sequence: b"\x1b[c",
    },
    Probe {
        name: "DA2 — `CSI > c`",
        sequence: b"\x1b[>c",
    },
    Probe {
        name: "DSR cursor pos — `CSI 6 n`",
        sequence: b"\x1b[6n",
    },
    // Without the intermediate write the save/restore round-trip is a
    // correct no-op, which the matrix would mis-flag.
    Probe {
        name: "DECSC + move + write + DECRC",
        sequence: b"\x1b7\x1b[8;8HQ\x1b8",
    },
    Probe {
        name: "ANSI SCO save + move + write + restore",
        sequence: b"\x1b[s\x1b[8;8HQ\x1b[u",
    },
    Probe {
        name: "DECSTR — `CSI ! p`",
        sequence: b"\x1b[!p",
    },
    Probe {
        name: "OSC 0 — title",
        sequence: b"\x1b]0;hello\x07",
    },
    Probe {
        name: "OSC 7 — cwd",
        sequence: b"\x1b]7;file:///tmp\x07",
    },
    Probe {
        name: "OSC 10 — fg color set",
        sequence: b"\x1b]10;rgb:11/22/33\x07",
    },
    Probe {
        name: "OSC 11 — bg color set",
        sequence: b"\x1b]11;rgb:11/22/33\x07",
    },
    Probe {
        name: "OSC 8 — hyperlink open",
        sequence: b"\x1b]8;;https://x.test\x07X",
    },
    Probe {
        name: "OSC 52 — clipboard set",
        sequence: b"\x1b]52;c;aGVsbG8=\x07",
    },
    Probe {
        name: "OSC 133 A — prompt mark",
        sequence: b"\x1b]133;A\x07",
    },
    Probe {
        name: "BEL — `0x07`",
        sequence: b"\x07",
    },
    Probe {
        name: "Kitty kbd push — `CSI > 1 u`",
        sequence: b"\x1b[>1u",
    },
    Probe {
        name: "Kitty kbd set  — `CSI = 1 u`",
        sequence: b"\x1b[=1u",
    },
    // Pop against an empty stack is a no-op.
    Probe {
        name: "Kitty kbd push + pop",
        sequence: b"\x1b[>1u\x1b[<u",
    },
    Probe {
        name: "Kitty kbd query — `CSI ? u`",
        sequence: b"\x1b[?u",
    },
    Probe {
        name: "REP — print 'A' then `CSI 2 b`",
        sequence: b"A\x1b[2b",
    },
    Probe {
        name: "S8C1T — `ESC SP G`",
        sequence: b"\x1b G",
    },
    Probe {
        name: "DECRQM — `CSI ?25 $p`",
        sequence: b"\x1b[?25$p",
    },
    Probe {
        name: "XTWINOPS 18 — report screen size in chars",
        sequence: b"\x1b[18t",
    },
    Probe {
        name: "XTVERSION — `CSI > 0 q`",
        sequence: b"\x1b[>0q",
    },
];

#[test]
fn every_adopted_sequence_observably_mutates_grid_state() {
    for probe in PROBES {
        assert_probe_changes_state(probe);
    }
}

#[test]
fn baseline_printable_byte_changes_state() {
    let mut grid = Grid::new(ROWS, COLS);
    let mut parser = Parser::new();
    let before = GridFingerprint::snapshot(&grid);
    parser.advance(&mut grid, b"X");
    let after = GridFingerprint::snapshot(&grid);
    assert_ne!(before, after);
    assert!(matches!(
        grid.cell(0, 0).unwrap().grapheme,
        Grapheme::Ascii(b'X'),
    ));
    assert_eq!(grid.cursor().col, 1);
}
