//! Insta snapshots for OSC handlers (the per-OSC-handler snapshot suite
//! in `docs/reference/testing.md` "Snapshot tests").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use felis_grid::Grid;
use felis_protocol::messages::ThemeChannel;

mod common;
use common::drive;

fn render(grid: &Grid) -> String {
    let mut out = String::new();
    writeln!(out, "title: {:?}", grid.title()).unwrap();
    writeln!(out, "cwd: {:?}", grid.cwd()).unwrap();
    for (label, channel) in [
        ("fg", ThemeChannel::Foreground),
        ("bg", ThemeChannel::Background),
        ("cursor", ThemeChannel::Cursor),
    ] {
        writeln!(out, "theme.{label}: {:?}", grid.theme_override(channel)).unwrap();
    }
    let marks = grid.prompt_marks();
    if marks.is_empty() {
        writeln!(out, "prompt_marks: (none)").unwrap();
    } else {
        writeln!(out, "prompt_marks:").unwrap();
        for m in marks {
            writeln!(
                out,
                "  line={} kind={:?} exit={:?}",
                m.line, m.kind, m.exit_code
            )
            .unwrap();
        }
    }
    out
}

#[test]
fn osc_0_sets_title() {
    let g = drive(2, 8, b"\x1b]0;hello\x07");
    insta::assert_snapshot!(render(&g), @r#"
    title: Some("hello")
    cwd: None
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    "#);
}

#[test]
fn osc_2_sets_title_st_terminated() {
    let g = drive(2, 8, b"\x1b]2;window\x1b\\");
    insta::assert_snapshot!(render(&g), @r#"
    title: Some("window")
    cwd: None
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    "#);
}

#[test]
fn osc_7_sets_cwd() {
    let g = drive(2, 8, b"\x1b]7;file://host/home/user\x07");
    insta::assert_snapshot!(render(&g), @r#"
    title: None
    cwd: Some("file://host/home/user")
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    "#);
}

#[test]
fn osc_10_11_12_set_each_channel_via_hash_form() {
    let g = drive(
        2,
        8,
        b"\x1b]10;#aabbcc\x07\x1b]11;#112233\x07\x1b]12;#ffffff\x07",
    );
    insta::assert_snapshot!(render(&g), @r"
    title: None
    cwd: None
    theme.fg: Some((170, 187, 204))
    theme.bg: Some((17, 34, 51))
    theme.cursor: Some((255, 255, 255))
    prompt_marks: (none)
    ");
}

#[test]
fn osc_10_accepts_x11_rgb_form_with_short_components() {
    // X11 left-justifies short hex components: `f` reads as `f0`.
    let g = drive(2, 8, b"\x1b]10;rgb:f/8/0\x07");
    insta::assert_snapshot!(render(&g), @r"
    title: None
    cwd: None
    theme.fg: Some((240, 128, 0))
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    ");
}

#[test]
fn osc_110_111_112_clear_each_channel() {
    let g = drive(
        2,
        8,
        b"\x1b]10;#aabbcc\x07\x1b]11;#112233\x07\x1b]12;#ffffff\x07\
          \x1b]110\x07\x1b]111\x07\x1b]112\x07",
    );
    insta::assert_snapshot!(render(&g), @r"
    title: None
    cwd: None
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    ");
}

#[test]
fn osc_133_records_a_b_c_d_marks_with_exit_code() {
    // The line is observed at OSC time, so wrap and `\r\n` move it
    // (docs/explanation/data-model/scrollback.md).
    let g = drive(
        3,
        8,
        b"\x1b]133;A\x07prompt\x1b]133;B\x07cmd\x1b]133;C\x07\r\nout\x1b]133;D;0\x07",
    );
    insta::assert_snapshot!(render(&g), @"
    title: None
    cwd: None
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks:
      line=0 kind=PromptStart exit=None
      line=0 kind=InputStart exit=None
      line=1 kind=OutputStart exit=None
      line=2 kind=CommandEnd exit=Some(0)
    ");
}

#[test]
fn malformed_osc_payload_keeps_state_at_default() {
    let g = drive(2, 8, b"\x1b]10\x07\x1b]7;\x01bad\x07\x1b]99;ignored\x07");
    insta::assert_snapshot!(render(&g), @r"
    title: None
    cwd: None
    theme.fg: None
    theme.bg: None
    theme.cursor: None
    prompt_marks: (none)
    ");
}

fn render_sizings(grid: &Grid) -> String {
    use felis_grid::Grapheme;

    let mut out = String::new();
    writeln!(out, "registry: {} entries", grid.sizing_count()).unwrap();
    writeln!(out, "sized cells: {}", grid.sized_cell_count()).unwrap();
    for r in 0..grid.rows() {
        let mut text = String::new();
        let mut sizings = String::new();
        for c in 0..grid.cols() {
            let cell = grid.cell(r, c).copied().unwrap_or_default();
            let ch = match cell.grapheme {
                Grapheme::Ascii(b) => b as char,
                Grapheme::Char(ch) => ch,
                Grapheme::Cluster(id) => grid
                    .cluster_str(id)
                    .and_then(|s| s.chars().next())
                    .unwrap_or(' '),
                Grapheme::Empty | Grapheme::Spacer | Grapheme::SizedSpacer => ' ',
            };
            text.push(ch);
            sizings.push(grid.cell_sizing_handle(r, c).map_or('.', |h| {
                char::from_digit(u32::from(h.get()), 16).unwrap_or('?')
            }));
        }
        writeln!(out, "row {r}: {text:?} sizings={sizings:?}").unwrap();
    }
    out
}

#[test]
fn osc_66_default_metadata_prints_text_with_default_handle() {
    let g = drive(1, 8, b"\x1b]66;;hi\x07");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 2
    row 0: "hi      " sizings="11......"
    "#);
}

#[test]
fn osc_66_sized_run_stamps_only_its_text() {
    // REQ-603: each scale=2 ASCII char claims a 2×2 block.
    let g = drive(2, 8, b"abc\x1b]66;s=2;DE\x07fg");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 8
    row 0: "abcD E f" sizings="...1111."
    row 1: "g       " sizings="...1111."
    "#);
}

#[test]
fn osc_66_two_runs_share_or_split_the_registry() {
    // The dispatcher does not de-dupe identical metadata.
    let g = drive(2, 8, b"\x1b]66;s=2;A\x07\x1b]66;s=2;B\x07");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 2 entries
    sized cells: 8
    row 0: "A B     " sizings="1122...."
    row 1: "        " sizings="1122...."
    "#);
}

#[test]
fn osc_66_clears_sizing_when_overprinted_with_default() {
    let g = drive(1, 8, b"\x1b]66;;XYZ\x07\rabc");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 0
    row 0: "abc     " sizings="........"
    "#);
}

#[test]
fn osc_66_overprint_into_spanned_cell_clears_whole_block() {
    // Kitty spec: "If any of the cells used by a multi-cell character
    // are modified after creation, the entire character is erased."
    let g = drive(4, 8, b"\x1b]66;s=3;A\x07\r\n$");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 0
    row 0: "        " sizings="........"
    row 1: "$       " sizings="........"
    row 2: "        " sizings="........"
    row 3: "        " sizings="........"
    "#);
}

#[test]
fn osc_66_overprint_primary_directly_clears_whole_block() {
    let g = drive(3, 8, b"\x1b]66;s=2;A\x07\x1b[1;1Hx");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 0
    row 0: "x       " sizings="........"
    row 1: "        " sizings="........"
    row 2: "        " sizings="........"
    "#);
}

#[test]
fn osc_66_two_printfs_with_intervening_newline_keep_screen_consistent() {
    let g = drive(4, 8, b"\x1b]66;s=2;A\x07\r\n\r\n\x1b]66;s=2;B\x07");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 2 entries
    sized cells: 8
    row 0: "A       " sizings="11......"
    row 1: "        " sizings="11......"
    row 2: "B       " sizings="22......"
    row 3: "        " sizings="22......"
    "#);
}

#[test]
fn osc_66_rejects_invalid_metadata_silently() {
    let g = drive(1, 8, b"\x1b]66;s=0;X\x07ok");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 0 entries
    sized cells: 0
    row 0: "ok      " sizings="........"
    "#);
}

#[test]
fn osc_66_stamps_both_halves_of_a_wide_glyph() {
    // REQ-603: a wide char claims a `2×scale` wide block.
    let g = drive(2, 8, "\x1b]66;s=2;あい\x07".as_bytes());
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 16
    row 0: "あ   い   " sizings="11111111"
    row 1: "        " sizings="11111111"
    "#);
}

#[test]
fn osc_66_discards_run_whose_scaled_bounding_box_overflows_screen() {
    // REQ-406: a run that cannot fit is a whole no-op.
    let g = drive(1, 8, b"\x1b]66;s=7;ABCDE\x07");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 0 entries
    sized cells: 0
    row 0: "        " sizings="........"
    "#);
}

#[test]
fn osc_66_w_override_widens_the_block_horizontally() {
    // REQ-602 + REQ-603: `w=3` claims 3 cells per char regardless of
    // natural width.
    let g = drive(2, 8, b"\x1b]66;s=2:w=3;A\x07");
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 12
    row 0: "A       " sizings="111111.."
    row 1: "        " sizings="111111.."
    "#);
}

#[test]
fn osc_66_w_override_extends_past_wide_glyph_natural_width() {
    let g = drive(1, 8, "\x1b]66;w=3;あ\x07".as_bytes());
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 3
    row 0: "あ       " sizings="111....."
    "#);
}

#[test]
fn osc_66_stamps_sized_spacer_grapheme_on_spanned_cells() {
    use felis_grid::Grapheme;

    let g = drive(2, 4, b"\x1b]66;s=2;X\x07");
    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'X'));
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::SizedSpacer);
    assert_eq!(g.cell(1, 0).unwrap().grapheme, Grapheme::SizedSpacer);
    assert_eq!(g.cell(1, 1).unwrap().grapheme, Grapheme::SizedSpacer);
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Empty);
    assert_eq!(g.cell(1, 2).unwrap().grapheme, Grapheme::Empty);
}

#[test]
fn osc_66_survives_0x9c_byte_in_japanese_text_body() {
    // 0x9C-as-data pin: 作 = E4 BD 9C.
    let g = drive(2, 8, "\x1b]66;s=2;作業\x07".as_bytes());
    insta::assert_snapshot!(render_sizings(&g), @r#"
    registry: 1 entries
    sized cells: 16
    row 0: "作   業   " sizings="11111111"
    row 1: "        " sizings="11111111"
    "#);
}
