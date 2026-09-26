//! The batched print paths (`print_str`, `print_utf8_run`) must leave the
//! grid exactly as feeding every byte through `Sink::print` does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;
use felis_vt::{Parser, Sink};
use proptest::prelude::*;

/// Forwards everything to the grid except the two batched print entry
/// points, which fall back to the trait's per-byte defaults.
struct PerByte(Grid);

impl Sink for PerByte {
    fn print(&mut self, byte: u8) {
        self.0.print(byte);
    }
    fn execute(&mut self, byte: u8) {
        self.0.execute(byte);
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        self.0.esc_dispatch(intermediates, final_byte);
    }
    fn csi_dispatch(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        ignore: bool,
        final_byte: u8,
    ) {
        self.0
            .csi_dispatch(params, subparams, intermediates, ignore, final_byte);
    }
    fn dcs_hook(&mut self, params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {
        self.0.dcs_hook(params, intermediates, ignore, final_byte);
    }
    fn dcs_put(&mut self, byte: u8) {
        self.0.dcs_put(byte);
    }
    fn dcs_unhook(&mut self) {
        self.0.dcs_unhook();
    }
    fn osc_dispatch(&mut self, body: &[u8], bell_terminated: bool) {
        self.0.osc_dispatch(body, bell_terminated);
    }
    fn osc_overflow(&mut self) {
        self.0.osc_overflow();
    }
    fn apc_dispatch(&mut self, body: &[u8]) {
        self.0.apc_dispatch(body);
    }
    fn apc_overflow(&mut self) {
        self.0.apc_overflow();
    }
}

const ROWS: u16 = 4;

fn csi(body: &str) -> Vec<u8> {
    format!("\x1b[{body}").into_bytes()
}

/// Narrow, wide, and fold-candidate scalars: combining marks, ZWJ, an
/// emoji modifier, and regional indicators exercise the deferral to
/// `put_grapheme`.
const CHARS: &[char] = &[
    'a',
    'Z',
    '~',
    'é',
    'Ω',
    'ж',
    '中',
    '，',
    'ｱ',
    'Ａ',
    'あ',
    '한',
    '😀',
    '❤',
    '\u{301}',
    '\u{FE0F}',
    '\u{200D}',
    '\u{1F3FB}',
    '\u{1F1EF}',
    '\u{1F1F5}',
];

fn atom() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        4 => proptest::collection::vec(0x20u8..=0x7E, 1..24),
        4 => proptest::collection::vec(proptest::sample::select(CHARS), 1..16)
            .prop_map(|cs| cs.into_iter().collect::<String>().into_bytes()),
        1 => prop_oneof![
            Just(vec![0xC3]),
            Just(vec![0xE4, 0xB8]),
            Just(vec![0xFF]),
            Just(vec![0x80]),
        ],
        2 => prop_oneof![
            Just(vec![b'\n']),
            Just(vec![b'\r']),
            Just(vec![b'\t']),
            Just(vec![0x08]),
        ],
        1 => (1..=ROWS, 1u16..=12).prop_map(|(r, c)| csi(&format!("{r};{c}H"))),
        1 => (0u8..=2).prop_map(|n| csi(&format!("{n}K"))),
        1 => prop_oneof![Just("0m"), Just("1m"), Just("41m"), Just("7m")].prop_map(csi),
        1 => (1u8..=4).prop_map(|n| csi(&format!("{n}b"))),
        1 => prop_oneof![
            Just("?1049h"),
            Just("?1049l"),
            Just("?7l"),
            Just("?7h"),
            Just("4h"),
            Just("4l"),
        ]
        .prop_map(csi),
    ]
}

fn stream() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(atom(), 0..48).prop_map(|atoms| atoms.concat())
}

fn assert_matches_per_byte(cols: u16, bytes: &[u8], chunk: usize) -> Result<(), TestCaseError> {
    let mut batched = Grid::new(ROWS, cols);
    let mut parser = Parser::new();
    for piece in bytes.chunks(chunk) {
        parser.advance(&mut batched, piece);
    }
    let mut reference = PerByte(Grid::new(ROWS, cols));
    Parser::new().advance(&mut reference, bytes);
    let reference = reference.0;

    prop_assert_eq!(batched.cursor(), reference.cursor());
    for r in 0..ROWS {
        prop_assert_eq!(
            batched.row_soft_wrap_continued(r),
            reference.row_soft_wrap_continued(r),
            "soft wrap of row {}",
            r
        );
        for c in 0..cols {
            prop_assert_eq!(
                batched.cell(r, c),
                reference.cell(r, c),
                "cell ({}, {})",
                r,
                c
            );
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn batched_print_matches_per_byte_print(
        cols in 2u16..=12,
        bytes in stream(),
        chunk in 1usize..=64,
    ) {
        assert_matches_per_byte(cols, &bytes, chunk)?;
    }
}

/// A run that fills the row must not arm a wrap for an emoji modifier
/// that folds into the run's last glyph.
#[test]
fn utf8_run_ending_at_the_edge_keeps_the_fold_owner() {
    assert_matches_per_byte(2, "é\u{1F3FB}".as_bytes(), usize::MAX).unwrap();
}

/// A ZWJ followed by an ASCII run leaves nothing for a later scalar to
/// join.
#[test]
fn ascii_run_disarms_a_pending_zwj() {
    assert_matches_per_byte(
        8,
        "\x1b[1;5H中\u{200D}\r    \x1b[1;7Hé".as_bytes(),
        usize::MAX,
    )
    .unwrap();
}

/// Each C0 control that moves the cursor disarms a pending ZWJ on both
/// paths alike.
#[test]
fn cursor_controls_disarm_a_pending_zwj_on_both_paths() {
    for stream in [
        "\u{1F469}\u{200D}\x08\u{1F4BB}",
        "\x1b[1;6H\u{1F469}\u{200D}\t\u{1F4BB}",
        "\x1b[?69h\x1b[3;8s\x1b[1;1H\u{1F469}\u{200D}\r\u{1F4BB}",
        "\x1b[2;1H\u{1F469}\u{200D}\x1b[1;1H\u{1F469}\u{200D}\n\u{1F4BB}",
        "\x1b[2;1H\u{1F469}\u{200D}\x1b[1;1H\u{1F469}\u{200D}\x0b\u{1F4BB}",
        "\x1b[2;1H\u{1F469}\u{200D}\x1b[1;1H\u{1F469}\u{200D}\x0c\u{1F4BB}",
    ] {
        for chunk in [1, usize::MAX] {
            assert_matches_per_byte(8, stream.as_bytes(), chunk).unwrap();
        }
    }
}
