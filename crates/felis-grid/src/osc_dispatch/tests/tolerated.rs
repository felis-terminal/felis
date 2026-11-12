use super::*;

/// The `payload.len() < 2` guard returns before indexing `payload[1]`;
/// flipped to `>` a one-segment payload indexes out of bounds.
#[test]
fn osc_52_single_segment_is_a_noop() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 1);
    drive(&mut p, &mut g, b"\x1b]52;c\x07");
    assert_eq!(
        g.take_pending_clipboard_sets(),
        Vec::<ClipboardWrite>::new()
    );
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
}

/// The tolerated OSC families leave nothing a program can observe: no
/// reply, no title change, no theme override
/// (`docs/reference/protocols/vt-compliance.md` "Consumed without
/// effect").
#[test]
fn tolerated_osc_families_leave_no_state_and_no_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]0;kept\x07");

    for seq in [
        &b"\x1b]13;rgb:ff/00/00\x07"[..],
        b"\x1b]17;#abc\x07",
        b"\x1b]19;?\x07",
        b"\x1b]633;A\x07",
        b"\x1b]1337;CurrentDir=/tmp\x07",
    ] {
        drive(&mut p, &mut g, seq);
        assert!(responses(&mut g).is_empty(), "reply to {seq:?}");
    }

    assert_eq!(g.title(), Some("kept"));
    for channel in [
        ThemeChannel::Foreground,
        ThemeChannel::Background,
        ThemeChannel::Cursor,
    ] {
        assert_eq!(g.theme_override(channel), None);
    }
}
