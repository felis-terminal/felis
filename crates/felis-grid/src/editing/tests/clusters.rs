use super::*;

/// A combining mark after a width-2 print folds into the wide owner,
/// not the Spacer.
#[test]
fn combining_mark_attaches_to_wide_glyph_owner_not_spacer() {
    let mut g = Grid::new(2, 6);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "世".as_bytes());
    assert_eq!(g.cursor().col, 2);
    drive(&mut p, &mut g, "\u{0301}".as_bytes());
    assert!(
        matches!(g.cell(0, 0).unwrap().grapheme, Grapheme::Cluster(_)),
        "combining mark must uplevel the wide owner to a Cluster"
    );
    assert_eq!(g.cell(0, 1).unwrap().grapheme, Grapheme::Spacer);
}

/// A mark stream stops folding at `ClusterText::CAP`: the cell keeps
/// the last cluster that fit and the table stops growing.
#[test]
fn a_mark_stream_stops_folding_at_the_cluster_cap() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"a");
    // U+0301 is two UTF-8 bytes, so base + 63 marks lands exactly on
    // the 128-byte cap.
    let marks = "\u{0301}".repeat(4096);
    drive(&mut p, &mut g, marks.as_bytes());
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("the marks that did fit must leave a Cluster");
    };
    let text = g.cluster_str(id).expect("resolvable");
    assert!(text.len() <= ClusterText::CAP, "held {} bytes", text.len());
    assert_eq!(text.chars().count(), 64, "base + 63 marks fills the cap");
    assert_eq!(g.cluster_count(), 63);
}

/// A refused trailing ZWJ must not arm the GB11 continuation, or the
/// cap would become a way to blank the rest of the line.
#[test]
fn a_refused_zwj_does_not_swallow_the_next_base_character() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, "👨".as_bytes());
    drive(&mut p, &mut g, "\u{200D}".repeat(200).as_bytes());
    let before = g.cluster_count();
    drive(&mut p, &mut g, b"X");
    assert_eq!(
        g.cluster_count(),
        before,
        "the base after a refused ZWJ must not extend the cluster"
    );
    let printed = (0..8)
        .filter_map(|c| match g.cell(0, c).unwrap().grapheme {
            Grapheme::Ascii(b) => Some(char::from(b)),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(printed, "X", "the base must land in its own cell");
}

/// The longest real emoji ZWJ sequence still folds into one cluster.
#[test]
fn the_longest_real_emoji_sequence_still_folds() {
    let mut g = Grid::new(1, 8);
    let mut p = Parser::new();
    drive(
        &mut p,
        &mut g,
        "👨\u{200D}👩\u{200D}👧\u{200D}👦".as_bytes(),
    );
    let Grapheme::Cluster(id) = g.cell(0, 0).unwrap().grapheme else {
        panic!("the family sequence must fold into one cluster");
    };
    assert_eq!(g.cluster_str(id), Some("👨\u{200D}👩\u{200D}👧\u{200D}👦"));
}
