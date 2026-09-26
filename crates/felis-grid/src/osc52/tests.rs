use crate::test_support::{drive, responses};
use crate::*;
use felis_vt::Parser;

#[test]
fn osc_52_set_populates_pending_clipboard_with_decoded_payload() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;aGVsbG8=\x07");
    let set = g.take_pending_clipboard_set().unwrap();
    assert_eq!(set.selection, ClipboardSelection::CLIPBOARD);
    assert_eq!(set.data, b"hello");
    assert!(g.take_pending_clipboard_set().is_none());
}

#[test]
fn osc_52_set_round_trips_japanese_clipboard_bytes() {
    // "日本語" holds 0x9C, the C1 ST byte: a parser honoring it as ST
    // truncates the payload.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;5pel5pys6Kqe\x07");
    let set = g.take_pending_clipboard_set().unwrap();
    assert_eq!(set.selection, ClipboardSelection::CLIPBOARD);
    assert_eq!(set.data, "日本語".as_bytes());
}

#[test]
fn osc_52_selection_chars_map_to_bitflags() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;;YQ==\x07");
    assert_eq!(
        g.take_pending_clipboard_set().unwrap().selection,
        ClipboardSelection::CLIPBOARD
    );
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;cp;YQ==\x07");
    assert_eq!(
        g.take_pending_clipboard_set().unwrap().selection,
        ClipboardSelection::CLIPBOARD | ClipboardSelection::PRIMARY
    );
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;s;YQ==\x07");
    assert!(g.take_pending_clipboard_set().is_none());
}

#[test]
fn osc_52_query_replies_with_base64_of_last_set_for_that_selection() {
    // Answered from the OSC 52 cache, not the OS clipboard: a hostile
    // program must not read what the user did not set via OSC 52.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;aGk=\x07");
    drop(g.take_pending_clipboard_set());

    drive(&mut p, &mut g, b"\x1b]52;c;?\x07");
    let responses = responses(&mut g);
    assert_eq!(responses, vec![b"\x1b]52;c;aGk=\x07".to_vec()]);
}

#[test]
fn osc_52_query_without_prior_set_replies_with_empty_data() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;?\x07");
    let responses = responses(&mut g);
    // An empty body reads as "no clipboard data" (xterm).
    assert_eq!(responses, vec![b"\x1b]52;c;\x07".to_vec()]);
}

#[test]
fn osc_52_clear_drops_cached_value_for_named_selections() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;aGk=\x07");
    drop(g.take_pending_clipboard_set());

    drive(&mut p, &mut g, b"\x1b]52;c;!\x07");
    drive(&mut p, &mut g, b"\x1b]52;c;?\x07");
    assert_eq!(responses(&mut g), vec![b"\x1b]52;c;\x07".to_vec()]);
    assert!(g.take_pending_clipboard_set().is_none());
}

#[test]
fn osc_52_query_picks_clipboard_over_primary_when_both_cached() {
    // xterm replies with the first requested selector that has data.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;aGk=\x07");
    drop(g.take_pending_clipboard_set());
    drive(&mut p, &mut g, b"\x1b]52;p;eW8=\x07");
    drop(g.take_pending_clipboard_set());

    drive(&mut p, &mut g, b"\x1b]52;cp;?\x07");
    assert_eq!(responses(&mut g), vec![b"\x1b]52;cp;aGk=\x07".to_vec()]);
}

#[test]
fn osc_52_query_falls_back_to_primary_when_clipboard_empty() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;p;eW8=\x07");
    drop(g.take_pending_clipboard_set());

    drive(&mut p, &mut g, b"\x1b]52;cp;?\x07");
    assert_eq!(responses(&mut g), vec![b"\x1b]52;cp;eW8=\x07".to_vec()]);
}

#[test]
fn osc_52_invalid_base64_is_rejected() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;===\x07");
    assert!(g.take_pending_clipboard_set().is_none());
    let mut g = Grid::new(1, 1);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]52;c;a@b!\x07");
    assert!(g.take_pending_clipboard_set().is_none());
}

#[test]
fn osc_52_replace_keeps_only_the_latest_set() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(
        &mut p,
        &mut g,
        b"\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07",
    );
    let set = g.take_pending_clipboard_set().unwrap();
    assert_eq!(set.data, b"second");
}

#[test]
fn osc_52_payload_with_embedded_semicolon_round_trips() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c;YWI7Y2Q=\x07");
    let set = g.take_pending_clipboard_set().unwrap();
    assert_eq!(set.data, b"ab;cd");
}
