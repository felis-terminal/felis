use super::*;

#[test]
fn ime_commit_forwards_japanese_text_as_utf8_bytes() {
    let bytes = ime_commit_bytes(&ImeInput::Commit("こんにちは".to_owned()))
        .expect("non-empty commit must forward bytes");
    assert_eq!(bytes, "こんにちは".as_bytes());
}

#[test]
fn empty_ime_commit_is_dropped() {
    // Some IMEs send an empty Commit on cancel.
    assert_eq!(ime_commit_bytes(&ImeInput::Commit(String::new())), None);
}

#[test]
fn preedit_from_ime_anchors_at_cursor_cell_with_text() {
    let overlay = preedit_from_ime(
        &ImeInput::Preedit("こん".to_owned(), Some((6, 6))),
        at(3, 5),
    )
    .unwrap();
    assert_eq!(overlay.anchor, (3, 5));
    assert_eq!(overlay.text, "こん");
    assert_eq!(overlay.cursor, Some((6, 6)));
}

#[test]
fn preedit_from_ime_empty_string_clears_overlay() {
    // Platform IMEs send an empty Preedit when composition ends without commit.
    assert!(preedit_from_ime(&ImeInput::Preedit(String::new(), None), at(0, 0)).is_none());
}

#[test]
fn preedit_from_ime_commit_event_clears_overlay() {
    assert!(preedit_from_ime(&ImeInput::Commit("hello".to_owned()), at(0, 0)).is_none());
    assert!(preedit_from_ime(&ImeInput::Enabled, at(0, 0)).is_none());
    assert!(preedit_from_ime(&ImeInput::Disabled, at(0, 0)).is_none());
}

#[test]
fn ime_preedit_enabled_disabled_carry_no_bytes_today() {
    // Pre-edit text rides the renderer overlay, never the PTY.
    assert_eq!(ime_commit_bytes(&ImeInput::Enabled), None);
    assert_eq!(ime_commit_bytes(&ImeInput::Disabled), None);
    assert_eq!(
        ime_commit_bytes(&ImeInput::Preedit("こん".to_owned(), Some((2, 2)))),
        None,
    );
}
