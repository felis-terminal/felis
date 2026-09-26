//! `row_ansi` must invert the parser: feeding its output back through
//! felis-vt has to rebuild the grid it encoded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{
    AttrFlags, Attributes, Cell, ClusterTable, Color, Grapheme, Grid, RowAnsiOptions, StyleTable,
    UnderlineStyle, row_ansi_with,
};

mod common;
use common::drive;

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
        Attributes::default(),
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
            fg: Color::Indexed(12),
            ..Attributes::default()
        },
        Attributes {
            fg: Color::Indexed(200),
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
fn every_stored_sgr_dimension_survives_a_parse_back() {
    let attrs = representative_attrs();
    let mut styles = StyleTable::new();
    let row: Vec<Cell> = attrs
        .iter()
        .map(|&a| Cell {
            grapheme: Grapheme::Ascii(b'x'),
            style: styles.intern(a),
            link: None,
            sizing: None,
        })
        .collect();

    let encoded = row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &RowAnsiOptions::default(),
    );
    let cols = u16::try_from(row.len()).unwrap();
    let g = drive(2, cols, &encoded);

    for (col, want) in attrs.iter().enumerate() {
        let col = u16::try_from(col).unwrap();
        let cell = g.cell(0, col).expect("cell in range");
        assert_eq!(
            g.style(cell.style),
            want,
            "pen mismatch at column {col}: {want:?}"
        );
    }
}

/// Compared by URI, not id: the fresh grid interns links in its own order.
#[test]
fn osc8_anchors_survive_a_parse_back_by_uri() {
    use felis_grid::{HyperlinkEntry, LinkTable, LinkText};
    use std::num::NonZeroU16;

    let text = |s: &str| LinkText::new(s).expect("under cap");
    let mut links = LinkTable::default();
    for (id, entry) in [
        HyperlinkEntry {
            id: None,
            uri: text("https://one.example/a"),
        },
        HyperlinkEntry {
            id: Some(text("anchor7")),
            uri: text("https://two.example/b"),
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(i, e)| (NonZeroU16::new(u16::try_from(i + 1).unwrap()).unwrap(), e))
    {
        links.install(id, entry);
    }
    let mut styles = StyleTable::new();
    let linked = |styles: &mut StyleTable, b: u8, id: Option<u16>| Cell {
        grapheme: Grapheme::Ascii(b),
        style: styles.intern(Attributes::default()),
        link: id.and_then(NonZeroU16::new),
        sizing: None,
    };
    let row = vec![
        linked(&mut styles, b'a', None),
        linked(&mut styles, b'b', Some(1)),
        linked(&mut styles, b'c', Some(1)),
        linked(&mut styles, b'd', Some(2)),
        linked(&mut styles, b'e', None),
    ];

    let opts = RowAnsiOptions {
        links: Some(&links),
        ..RowAnsiOptions::default()
    };
    let encoded = row_ansi_with(&row, &ClusterTable::default(), &styles, &opts);
    let g = drive(2, 5, &encoded);

    let uri_at = |g: &Grid, col: u16| {
        g.cell(0, col)
            .and_then(|c| c.link)
            .and_then(|id| g.hyperlink(id))
            .map(|e| e.uri.as_str().to_owned())
    };
    assert_eq!(uri_at(&g, 0), None);
    assert_eq!(uri_at(&g, 1).as_deref(), Some("https://one.example/a"));
    assert_eq!(uri_at(&g, 2).as_deref(), Some("https://one.example/a"));
    assert_eq!(uri_at(&g, 3).as_deref(), Some("https://two.example/b"));
    assert_eq!(uri_at(&g, 4), None);

    let anchor = g
        .cell(0, 3)
        .and_then(|c| c.link)
        .and_then(|id| g.hyperlink(id))
        .expect("anchor at column 3");
    assert_eq!(anchor.id.as_ref().map(LinkText::as_str), Some("anchor7"));
}

#[test]
fn glyphs_survive_a_parse_back() {
    let mut styles = StyleTable::new();
    let bold = Attributes {
        flags: AttrFlags::BOLD,
        ..Attributes::default()
    };
    let mut row = vec![Cell {
        grapheme: Grapheme::Ascii(b'a'),
        style: styles.intern(bold),
        link: None,
        sizing: None,
    }];
    for g in [Grapheme::Char('中'), Grapheme::Spacer, Grapheme::Char('é')] {
        row.push(Cell {
            grapheme: g,
            style: styles.intern(Attributes::default()),
            link: None,
            sizing: None,
        });
    }

    let encoded = row_ansi_with(
        &row,
        &ClusterTable::default(),
        &styles,
        &RowAnsiOptions::default(),
    );
    let g = drive(2, 4, &encoded);

    assert_eq!(g.cell(0, 0).unwrap().grapheme, Grapheme::Ascii(b'a'));
    assert_eq!(g.style(g.cell(0, 0).unwrap().style).flags, AttrFlags::BOLD);
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Char('中'));
    assert_eq!(g.cell(0, 2).unwrap().grapheme, Grapheme::Spacer);
    assert_eq!(g.cell(0, 3).unwrap().grapheme, Grapheme::Char('é'));
}
