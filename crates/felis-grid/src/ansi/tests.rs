//! Tests for [`row_ansi`]. Round-tripping through the full parser,
//! rather than `apply_sgr` alone, is `tests/ansi_round_trip.rs`.

use super::*;
use crate::search::row_text;
use crate::{Cell, Color, Grapheme, HyperlinkEntry, UnderlineStyle};
use std::num::NonZeroU32;

fn cluster_table(texts: &[&str]) -> ClusterTable {
    let mut table = ClusterTable::default();
    for text in texts {
        table.intern(text).expect("under the table caps");
    }
    table
}

fn link_table(entries: &[HyperlinkEntry]) -> LinkTable {
    let mut table = LinkTable::default();
    for (i, entry) in entries.iter().enumerate() {
        let id = NonZeroU16::new(u16::try_from(i + 1).expect("fits")).expect("nonzero");
        table.install(id, entry.clone());
    }
    table
}

fn cell(styles: &mut StyleTable, g: Grapheme, attrs: Attributes) -> Cell {
    Cell {
        grapheme: g,
        style: styles.intern(attrs),
        link: None,
        sizing: None,
    }
}

fn ascii(styles: &mut StyleTable, b: u8, attrs: Attributes) -> Cell {
    cell(styles, Grapheme::Ascii(b), attrs)
}

fn plain(styles: &mut StyleTable, s: &str) -> Vec<Cell> {
    s.bytes()
        .map(|b| ascii(styles, b, Attributes::default()))
        .collect()
}

/// Strip every `CSI … m` SGR sequence, returning only the glyph bytes.
fn strip_sgr(bytes: &[u8]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i += 1; // consume the 'm'
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// Parse one CSI SGR body into the flat param slice + sub-parameter
/// bitmap [`Attributes::apply_sgr`] consumes.
fn parse_sgr_body(body: &str) -> (Vec<u16>, u32) {
    let mut params = Vec::new();
    let mut subparams = 0u32;
    let mut token = String::new();
    let mut next_is_sub = false;
    let push = |tok: &str, params: &mut Vec<u16>, idx: usize, sub: bool, mask: &mut u32| {
        params.push(tok.parse().unwrap_or(0));
        if sub && idx < 32 {
            *mask |= 1 << idx;
        }
    };
    for ch in body.chars() {
        match ch {
            ';' | ':' => {
                let idx = params.len();
                push(&token, &mut params, idx, next_is_sub, &mut subparams);
                token.clear();
                next_is_sub = ch == ':';
            }
            d => token.push(d),
        }
    }
    let idx = params.len();
    push(&token, &mut params, idx, next_is_sub, &mut subparams);
    (params, subparams)
}

/// Replay [`row_ansi`] output over an all-ASCII row, capturing the pen
/// in force at every glyph.
fn replay_pen_per_glyph(bytes: &[u8]) -> Vec<Attributes> {
    let mut pen = Attributes::default();
    let mut pens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            let body = std::str::from_utf8(&bytes[start..j]).unwrap();
            let (params, subparams) = parse_sgr_body(body);
            pen.apply_sgr(&params, subparams);
            i = j + 1;
        } else {
            pens.push(pen);
            i += 1;
        }
    }
    pens
}

#[test]
fn plain_ascii_row_has_no_escapes() {
    let mut styles = StyleTable::new();
    let row = plain(&mut styles, "hello");
    assert_eq!(row_ansi(&row, &ClusterTable::default(), &styles), b"hello");
}

#[test]
fn fully_blank_row_serializes_empty() {
    let row: Vec<Cell> = (0..80).map(|_| Cell::default()).collect();
    assert_eq!(
        row_ansi(&row, &ClusterTable::default(), &StyleTable::new()),
        Vec::<u8>::new()
    );
}

#[test]
fn trailing_blank_cells_are_trimmed() {
    let mut styles = StyleTable::new();
    let mut row = plain(&mut styles, "hi");
    row.extend((0..78).map(|_| Cell::default()));
    assert_eq!(row_ansi(&row, &ClusterTable::default(), &styles), b"hi");
}

#[test]
fn interior_blanks_are_preserved() {
    let mut styles = StyleTable::new();
    let row = plain(&mut styles, "a  b");
    assert_eq!(row_ansi(&row, &ClusterTable::default(), &styles), b"a  b");
}

#[test]
fn row_text_trim_resolves_clusters_skips_spacers_and_drops_trailing_spaces() {
    let c = |grapheme| Cell {
        grapheme,
        ..Cell::default()
    };
    let clusters = cluster_table(&["e\u{0301}"]);
    let cells = vec![
        c(Grapheme::Ascii(b'h')),
        c(Grapheme::Char('i')),
        c(Grapheme::Cluster(NonZeroU32::new(1).unwrap())),
        // A double-wide glyph's right half must not gain a placeholder.
        c(Grapheme::Spacer),
        c(Grapheme::SizedSpacer),
        c(Grapheme::Empty),
        c(Grapheme::Ascii(b' ')),
    ];

    assert_eq!(row_text_trim(&cells, &clusters), "hie\u{0301}");
}

/// Scrollback rows lack the table, so an unresolvable handle
/// contributes nothing rather than panicking.
#[test]
fn row_text_trim_skips_an_unresolvable_cluster_handle() {
    let cells = vec![
        Cell {
            grapheme: Grapheme::Ascii(b'x'),
            ..Cell::default()
        },
        Cell {
            grapheme: Grapheme::Cluster(NonZeroU32::new(9).unwrap()),
            ..Cell::default()
        },
    ];

    assert_eq!(row_text_trim(&cells, &ClusterTable::default()), "x");
}

/// The first row always starts a line regardless of its bit: its real
/// head may have been evicted.
#[test]
fn logical_line_spans_groups_by_continuation_bit() {
    // rows: A · B(cont) | C | D · E(cont), `·` marking a continuation.
    let continued = [false, true, false, false, true];
    assert_eq!(logical_line_spans(&continued), vec![(0, 1), (2, 2), (3, 4)]);
    assert_eq!(logical_line_spans(&[]), Vec::<(usize, usize)>::new());
    assert_eq!(logical_line_spans(&[true, true]), vec![(0, 1)]);
}

#[test]
fn trailing_colored_block_is_not_trimmed() {
    // A space carrying a non-default background is a drawn colored bar,
    // not a blank; it must close with a reset so the next row starts
    // clean.
    let red_bg = Attributes {
        bg: Color::Indexed(1),
        ..Attributes::default()
    };
    let mut styles = StyleTable::new();
    let mut row = plain(&mut styles, "x");
    row.push(cell(&mut styles, Grapheme::Empty, red_bg));
    row.extend((0..10).map(|_| Cell::default()));
    let out = row_ansi(&row, &ClusterTable::default(), &styles);
    assert_eq!(out, b"x\x1b[41m \x1b[0m");
}

#[test]
fn pen_change_then_return_to_default_emits_reset() {
    let bold = Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    };
    let mut styles = StyleTable::new();
    let mut row = vec![ascii(&mut styles, b'B', bold)];
    row.push(ascii(&mut styles, b'n', Attributes::default()));
    assert_eq!(
        row_ansi(&row, &ClusterTable::default(), &styles),
        b"\x1b[1mB\x1b[0mn"
    );
}

#[test]
fn pen_change_between_two_non_default_pens_re_specs_with_reset() {
    let bold = Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    };
    let italic = Attributes {
        flags: AttrFlags::ITALIC,
        ..Attributes::default()
    };
    let mut styles = StyleTable::new();
    let row = vec![
        ascii(&mut styles, b'B', bold),
        ascii(&mut styles, b'I', italic),
    ];
    // bold→italic re-specifies from a leading reset so the bold does
    // not bleed into the italic cell.
    assert_eq!(
        row_ansi(&row, &ClusterTable::default(), &styles),
        b"\x1b[1mB\x1b[0;3mI\x1b[0m"
    );
}

#[test]
fn wide_char_spacer_emits_no_extra_column() {
    let mut styles = StyleTable::new();
    let row = vec![
        ascii(&mut styles, b'a', Attributes::default()),
        cell(&mut styles, Grapheme::Char('中'), Attributes::default()),
        cell(&mut styles, Grapheme::Spacer, Attributes::default()),
        ascii(&mut styles, b'X', Attributes::default()),
    ];
    assert_eq!(
        row_ansi(&row, &ClusterTable::default(), &styles),
        "a中X".as_bytes()
    );
}

#[test]
fn cluster_text_is_resolved_from_the_table() {
    let clusters = cluster_table(&["é", "👨‍👩‍👧"]);
    let mut styles = StyleTable::new();
    let row = vec![
        ascii(&mut styles, b'a', Attributes::default()),
        cell(
            &mut styles,
            Grapheme::Cluster(NonZeroU32::new(2).unwrap()),
            Attributes::default(),
        ),
    ];
    assert_eq!(row_ansi(&row, &clusters, &styles), "a👨‍👩‍👧".as_bytes());
}

#[test]
fn ansi_256_and_rgb_colors_round_trip_through_apply_sgr() {
    let attrs_matrix = representative_attrs();
    let mut styles = StyleTable::new();
    let row: Vec<Cell> = attrs_matrix
        .iter()
        .enumerate()
        .map(|(i, &a)| ascii(&mut styles, b'a' + u8::try_from(i).unwrap(), a))
        .collect();
    let out = row_ansi(&row, &ClusterTable::default(), &styles);
    let recovered = replay_pen_per_glyph(&out);
    assert_eq!(recovered, attrs_matrix);
}

/// Every SGR dimension `row_ansi` emits.
fn representative_attrs() -> Vec<Attributes> {
    let flag = |f: AttrFlags| Attributes {
        flags: f,
        ..Attributes::default()
    };
    let ul = |s: UnderlineStyle| Attributes {
        flags: AttrFlags::UNDERLINE,
        underline_style: s,
        ..Attributes::default()
    };
    vec![
        flag(AttrFlags::BOLD),
        flag(AttrFlags::FAINT),
        flag(AttrFlags::ITALIC),
        flag(AttrFlags::BLINK),
        flag(AttrFlags::REVERSE),
        flag(AttrFlags::CONCEAL),
        flag(AttrFlags::STRIKETHROUGH),
        flag(AttrFlags::OVERLINE),
        flag(AttrFlags::BOLD | AttrFlags::ITALIC | AttrFlags::REVERSE),
        ul(UnderlineStyle::Single),
        ul(UnderlineStyle::Double),
        ul(UnderlineStyle::Curly),
        ul(UnderlineStyle::Dotted),
        ul(UnderlineStyle::Dashed),
        Attributes {
            fg: Color::Indexed(3),
            ..Attributes::default()
        },
        Attributes {
            fg: Color::Indexed(12), // bright → 9x
            ..Attributes::default()
        },
        Attributes {
            fg: Color::Indexed(200), // 256-cube → 38;5;n
            ..Attributes::default()
        },
        Attributes {
            bg: Color::Indexed(5),
            ..Attributes::default()
        },
        Attributes {
            bg: Color::Indexed(14), // bright → 10x
            ..Attributes::default()
        },
        Attributes {
            bg: Color::Indexed(231),
            ..Attributes::default()
        },
        Attributes {
            fg: Color::Rgb(0x12, 0x34, 0x56),
            bg: Color::Rgb(0x78, 0x9a, 0xbc),
            ..Attributes::default()
        },
        Attributes {
            flags: AttrFlags::UNDERLINE,
            underline_style: UnderlineStyle::Curly,
            underline_color: Color::Indexed(9),
            ..Attributes::default()
        },
        Attributes {
            flags: AttrFlags::UNDERLINE,
            underline_color: Color::Rgb(1, 2, 3),
            ..Attributes::default()
        },
    ]
}

#[test]
fn representative_row_snapshot() {
    let mut styles = StyleTable::new();
    let row: Vec<Cell> = representative_attrs()
        .iter()
        .enumerate()
        .map(|(i, &a)| ascii(&mut styles, b'a' + u8::try_from(i).unwrap(), a))
        .collect();
    let out = row_ansi(&row, &ClusterTable::default(), &styles);
    // ESC rendered as `\e` so a drift in any SGR shows as a reviewable
    // diff.
    let readable = String::from_utf8(out).unwrap().replace('\x1b', "\\e");
    insta::assert_snapshot!(readable);
}

fn readable(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace('\x1b', "\\e")
}

fn linked(styles: &mut StyleTable, b: u8, id: u16) -> Cell {
    Cell {
        link: NonZeroU16::new(id),
        ..ascii(styles, b, Attributes::default())
    }
}

fn link_entry(uri: &str, id: Option<&str>) -> HyperlinkEntry {
    HyperlinkEntry {
        id: id.map(|id| LinkText::new(id).expect("under cap")),
        uri: LinkText::new(uri).expect("under cap"),
    }
}

#[test]
fn width_mode_clips_and_pads_to_exactly_that_many_columns() {
    let mut styles = StyleTable::new();
    let row = plain(&mut styles, "hello");
    let clip = RowAnsiOptions {
        width: Some(3),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &styles, &clip),
        b"hel"
    );
    let pad = RowAnsiOptions {
        width: Some(8),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &styles, &pad),
        b"hello   "
    );
}

/// The painter overwrites the previous frame in place rather than
/// clearing first, so an empty emission would leave stale content on
/// screen.
#[test]
fn width_mode_emits_spaces_for_a_fully_blank_row() {
    let row: Vec<Cell> = (0..4).map(|_| Cell::default()).collect();
    let opts = RowAnsiOptions {
        width: Some(4),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &StyleTable::new(), &opts),
        b"    "
    );
}

/// Or a colored run bleeds to the pane edge.
#[test]
fn width_mode_resets_the_pen_before_padding() {
    let red_bg = Attributes {
        bg: Color::Indexed(1),
        ..Attributes::default()
    };
    let mut styles = StyleTable::new();
    let row = vec![cell(&mut styles, Grapheme::Ascii(b'x'), red_bg)];
    let opts = RowAnsiOptions {
        width: Some(4),
        ..RowAnsiOptions::default()
    };
    let out = row_ansi_with(&row, &ClusterTable::default(), &styles, &opts);
    assert_eq!(readable(&out), "\\e[41mx\\e[0m   ");
}

/// Column-exact mode owes one column per cell.
#[test]
fn width_mode_holds_a_column_for_sized_spacers_and_unresolvable_clusters() {
    let mut styles = StyleTable::new();
    let row = vec![
        ascii(&mut styles, b'a', Attributes::default()),
        cell(&mut styles, Grapheme::SizedSpacer, Attributes::default()),
        cell(
            &mut styles,
            Grapheme::Cluster(NonZeroU32::new(9).unwrap()),
            Attributes::default(),
        ),
        ascii(&mut styles, b'b', Attributes::default()),
    ];
    let opts = RowAnsiOptions {
        width: Some(4),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &styles, &opts),
        b"a  b"
    );
    assert_eq!(row_ansi(&row, &ClusterTable::default(), &styles), b"ab");
}

/// A split wide glyph would print two columns into the one left for it,
/// shifting everything after it into the next pane.
#[test]
fn width_mode_blanks_a_wide_glyph_whose_right_half_was_clipped() {
    let mut styles = StyleTable::new();
    let row = vec![
        ascii(&mut styles, b'a', Attributes::default()),
        cell(&mut styles, Grapheme::Char('中'), Attributes::default()),
        cell(&mut styles, Grapheme::Spacer, Attributes::default()),
    ];
    let clipped = RowAnsiOptions {
        width: Some(2),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &styles, &clipped),
        b"a "
    );
    let whole = RowAnsiOptions {
        width: Some(3),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        row_ansi_with(&row, &ClusterTable::default(), &styles, &whole),
        "a中".as_bytes()
    );
}

#[test]
fn no_truecolor_host_downgrades_rgb_to_a_256_index() {
    let mut styles = StyleTable::new();
    let row = vec![ascii(
        &mut styles,
        b'x',
        Attributes {
            fg: Color::Rgb(10, 20, 30),
            ..Attributes::default()
        },
    )];
    let opts = RowAnsiOptions {
        caps: AnsiCaps { truecolor: false },
        ..RowAnsiOptions::default()
    };
    let out = readable(&row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &opts,
    ));
    assert!(
        out.contains("38;5;"),
        "expected indexed downgrade, got {out}"
    );
    assert!(!out.contains("38;2;"), "must not emit 24-bit, got {out}");
    assert!(readable(&row_ansi(&row, &ClusterTable::default(), &styles)).contains("38;2;10;20;30"));
}

/// Every cube and ramp entry is its own nearest match, so a pen that
/// already names a palette colour downgrades to it exactly.
#[test]
fn rgb_to_256_maps_every_palette_color_to_its_own_index() {
    for idx in 16..=255u8 {
        let (r, g, b) = crate::default_palette_color(idx);
        assert_eq!(rgb_to_256(r, g, b), idx);
    }
}

/// The Kitty Unicode placeholder is a positioning sentinel, not text:
/// emitting it shows tofu on a graphics host and leaks a private-use
/// codepoint on one without.
#[test]
fn placeholder_cells_blank_in_both_their_char_and_cluster_forms() {
    let clusters = cluster_table(&[&format!("{PLACEHOLDER}\u{0305}")]);
    let mut styles = StyleTable::new();
    let row = vec![
        cell(
            &mut styles,
            Grapheme::Char(PLACEHOLDER),
            Attributes::default(),
        ),
        cell(
            &mut styles,
            Grapheme::Cluster(NonZeroU32::new(1).unwrap()),
            Attributes::default(),
        ),
        ascii(&mut styles, b'x', Attributes::default()),
    ];
    let opts = RowAnsiOptions {
        blank_placeholders: true,
        ..RowAnsiOptions::default()
    };
    assert_eq!(row_ansi_with(&row, &clusters, &styles, &opts), b"  x");
    assert_eq!(
        row_ansi(&row, &clusters, &styles),
        format!("{PLACEHOLDER}{PLACEHOLDER}\u{0305}x").as_bytes()
    );
}

#[test]
fn osc8_opens_on_a_linked_run_and_closes_at_its_end() {
    let mut styles = StyleTable::new();
    let mut row = vec![ascii(&mut styles, b'a', Attributes::default())];
    row.push(linked(&mut styles, b'b', 1));
    row.push(linked(&mut styles, b'c', 1));
    row.push(ascii(&mut styles, b'd', Attributes::default()));
    let links = link_table(&[link_entry("https://example.com", None)]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        readable(&row_ansi_with(
            &row,
            &ClusterTable::default(),
            &styles,
            &opts
        )),
        "a\\e]8;;https://example.com\\e\\bc\\e]8;;\\e\\d"
    );
}

/// Rows compose by concatenation, so a dangling anchor would swallow
/// the next row.
#[test]
fn osc8_closes_at_the_row_edge() {
    let mut styles = StyleTable::new();
    let row = vec![linked(&mut styles, b'a', 1)];
    let links = link_table(&[link_entry("https://example.com", None)]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    let out = readable(&row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &opts,
    ));
    assert!(out.ends_with("\\e]8;;\\e\\"), "unterminated anchor: {out}");
}

#[test]
fn osc8_carries_the_producer_supplied_anchor_id() {
    let mut styles = StyleTable::new();
    let row = vec![linked(&mut styles, b'a', 1)];
    let links = link_table(&[link_entry("https://example.com", Some("anchor7"))]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    let out = readable(&row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &opts,
    ));
    assert!(
        out.contains("\\e]8;id=anchor7;https://example.com"),
        "got {out}"
    );
}

/// A same-id reopen is tolerated by terminals but is pure churn on every
/// styled link.
#[test]
fn osc8_survives_a_style_change_inside_the_link() {
    let bold = Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    };
    let mut styles = StyleTable::new();
    let row = vec![
        linked(&mut styles, b'a', 1),
        Cell {
            link: NonZeroU16::new(1),
            ..ascii(&mut styles, b'b', bold)
        },
    ];
    let links = link_table(&[link_entry("https://example.com", None)]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    let out = readable(&row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &opts,
    ));
    assert_eq!(
        out.matches("\\e]8;").count(),
        2,
        "one open, one close: {out}"
    );
}

#[test]
fn osc8_reopens_when_the_link_target_changes() {
    let mut styles = StyleTable::new();
    let row = vec![linked(&mut styles, b'a', 1), linked(&mut styles, b'b', 2)];
    let links = link_table(&[
        link_entry("https://one.example", None),
        link_entry("https://two.example", None),
    ]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        readable(&row_ansi_with(
            &row,
            &ClusterTable::default(),
            &styles,
            &opts
        )),
        "\\e]8;;https://one.example\\e\\a\\e]8;;https://two.example\\e\\b\\e]8;;\\e\\"
    );
}

/// The reattach window before the `Hyperlink` message streams: readable
/// and un-clickable rather than folded into the previous anchor.
#[test]
fn osc8_skips_an_unresolvable_link_id() {
    let mut styles = StyleTable::new();
    let row = vec![linked(&mut styles, b'a', 1), linked(&mut styles, b'b', 9)];
    let links = link_table(&[link_entry("https://example.com", None)]);
    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    assert_eq!(
        readable(&row_ansi_with(
            &row,
            &ClusterTable::default(),
            &styles,
            &opts
        )),
        "\\e]8;;https://example.com\\e\\a\\e]8;;\\e\\b"
    );
}

#[test]
fn an_empty_link_table_emits_no_osc8() {
    let mut styles = StyleTable::new();
    let row = vec![linked(&mut styles, b'a', 1)];
    assert_eq!(row_ansi(&row, &ClusterTable::default(), &styles), b"a");
}

#[test]
fn sgr_set_is_always_reset_prefixed() {
    let caps = AnsiCaps::default();
    assert_eq!(
        readable(sgr_set(caps, &Attributes::default()).as_bytes()),
        "\\e[0m"
    );
    let bold = Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    };
    // The row encoder would emit a bare `\e[1m` here; `sgr_set` assumes
    // nothing about the prior pen.
    assert_eq!(readable(sgr_set(caps, &bold).as_bytes()), "\\e[0;1m");
}

#[test]
fn viewport_mode_row_snapshot() {
    let clusters = cluster_table(&[&format!("{PLACEHOLDER}\u{0305}")]);
    let links = link_table(&[link_entry("https://example.com", Some("a1"))]);
    let mut styles = StyleTable::new();
    let rgb = Attributes {
        fg: Color::Rgb(0x12, 0x34, 0x56),
        ..Attributes::default()
    };
    let row = vec![
        ascii(&mut styles, b'a', Attributes::default()),
        ascii(&mut styles, b'b', rgb),
        Cell {
            link: NonZeroU16::new(1),
            ..ascii(&mut styles, b'c', Attributes::default())
        },
        cell(
            &mut styles,
            Grapheme::Cluster(NonZeroU32::new(1).unwrap()),
            Attributes::default(),
        ),
    ];
    let opts = RowAnsiOptions {
        caps: AnsiCaps { truecolor: false },
        width: Some(8),
        blank_placeholders: true,
        links: Some(&links),
    };
    // Spaces show as `·`: trailing whitespace in a `.snap` file is at
    // the mercy of any whitespace-trimming hook.
    let out = readable(&row_ansi_with(&row, &clusters, &styles, &opts)).replace(' ', "·");
    insta::assert_snapshot!(out);
}

use proptest::prelude::*;

fn arb_color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        any::<u8>().prop_map(Color::Indexed),
        (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

fn arb_attrs() -> impl Strategy<Value = Attributes> {
    (any::<u16>(), arb_color(), arb_color(), arb_color(), 0u8..5).prop_map(
        |(bits, fg, bg, uc, ul)| Attributes {
            // PROTECTED / ISO_PROTECTED are not SGR; `row_ansi` never
            // emits them.
            flags: AttrFlags::from_bits_truncate(bits)
                & !(AttrFlags::PROTECTED | AttrFlags::ISO_PROTECTED),
            fg,
            bg,
            underline_color: uc,
            underline_style: match ul {
                0 => UnderlineStyle::Single,
                1 => UnderlineStyle::Double,
                2 => UnderlineStyle::Curly,
                3 => UnderlineStyle::Dotted,
                _ => UnderlineStyle::Dashed,
            },
        },
    )
}

fn arb_grapheme() -> impl Strategy<Value = Grapheme> {
    prop_oneof![
        Just(Grapheme::Empty),
        (0x20u8..=0x7e).prop_map(Grapheme::Ascii),
        any::<char>().prop_map(Grapheme::Char),
        Just(Grapheme::Spacer),
    ]
}

fn arb_glyph_and_pen() -> impl Strategy<Value = (Grapheme, Attributes)> {
    (arb_grapheme(), arb_attrs())
}

fn build_row(pairs: &[(Grapheme, Attributes)]) -> (Vec<Cell>, StyleTable) {
    let mut styles = StyleTable::new();
    let row = pairs
        .iter()
        .map(|&(grapheme, attrs)| Cell {
            grapheme,
            style: styles.intern(attrs),
            link: None,
            sizing: None,
        })
        .collect();
    (row, styles)
}

proptest! {
    /// Color reconstruction never alters the glyph stream. Both sides
    /// are trailing-space-trimmed: `row_ansi` trims trailing blank cells
    /// while `row_text` emits a space per empty cell.
    #[test]
    fn stripping_sgr_yields_row_text(pairs in prop::collection::vec(arb_glyph_and_pen(), 0..40)) {
        let (row, styles) = build_row(&pairs);
        let ansi = row_ansi(&row, &ClusterTable::default(), &styles);
        let stripped = strip_sgr(&ansi);
        let (text, _) = row_text(&row, &ClusterTable::default());
        prop_assert_eq!(stripped.trim_end_matches(' '), text.trim_end_matches(' '));
    }

    /// The daemon writes the output to a temp file a pager reads as text.
    #[test]
    fn output_is_valid_utf8(pairs in prop::collection::vec(arb_glyph_and_pen(), 0..40)) {
        let (row, styles) = build_row(&pairs);
        let ansi = row_ansi(&row, &ClusterTable::default(), &styles);
        prop_assert!(std::str::from_utf8(&ansi).is_ok());
    }
}
