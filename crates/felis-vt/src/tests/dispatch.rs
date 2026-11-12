use super::*;

#[test]
fn prints_ascii_letters() {
    let evs = parse(b"Hi");
    assert_eq!(evs, vec![Event::Print(b'H'), Event::Print(b'i')]);
}

#[test]
fn executes_c0_controls() {
    let evs = parse(b"\x07\n");
    assert_eq!(evs, vec![Event::Execute(0x07), Event::Execute(b'\n')]);
}

#[test]
fn bare_c1_bytes_in_ground_state_do_not_introduce_sequences() {
    // Williams' DFA has anywhere-state transitions on the C1 introducers
    // (0x90 DCS, 0x9B CSI, 0x9D OSC, 0x9F APC), but those bytes are also
    // UTF-8 continuation bytes, so honoring them would corrupt CJK / emoji.
    let evs = parse(b"\x90\x9b\x9d\x9e\x9f");
    for ev in &evs {
        assert!(
            matches!(ev, Event::Print(_)),
            "bare C1 byte produced non-print event: {ev:?}",
        );
    }
    assert_eq!(evs.len(), 5, "each byte should produce one print event");
}

#[test]
fn csi_dispatch_decodes_params() {
    let cases: &[(&[u8], &[u16])] = &[
        (b"\x1b[1;2H", &[1, 2]),
        (b"\x1b[H", &[]),
        // `5`, not `1`: `'1' - '0'` and `'1' / '0'` both yield 1, so a
        // param opening with `1` never pins the digit decode.
        (b"\x1b[5H", &[5]),
    ];
    for (input, params) in cases {
        assert_eq!(parse(input), vec![csi(params, &[], b'H')], "{input:?}");
    }
}

#[test]
fn csi_subparam_mask_marks_colon_opened_slots() {
    // `4:3:2` opens slots 1 and 2 with `:`, so the mask must be 0b110; no
    // other test pins a concrete non-zero mask.
    let evs = parse(b"\x1b[4:3:2m");
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![4, 3, 2],
            subparams: 0b110,
            intermediates: vec![],
            ignore: false,
            final_byte: b'm',
        }]
    );
}

#[test]
fn csi_leading_colon_keeps_empty_slot_zero() {
    // `CSI : m` has no digit after the colon, so `start_empty` alone seeds
    // slot 0: the param list must be `[0]`, not empty.
    let evs = parse(b"\x1b[:m");
    assert_eq!(evs, vec![csi(&[0], &[], b'm')]);
}

#[test]
fn csi_state_is_cleared_between_sequences() {
    // A second CSI must not inherit the first's params or intermediates;
    // every other test parses a single sequence from a fresh parser.
    let evs = parse(b"\x1b[1!p\x1b[H");
    assert_eq!(evs, vec![csi(&[1], b"!", b'p'), csi(&[], &[], b'H')]);
}

#[test]
fn csi_dispatch_with_private_intermediate() {
    let evs = parse(b"\x1b[?25h"); // DECTCEM show cursor
    assert_eq!(evs, vec![csi(&[25], b"?", b'h')]);
}

#[test]
fn cancellation_via_can() {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    parser.advance(&mut sink, b"\x1b[1;");
    parser.advance(&mut sink, b"\x18"); // CAN
    assert_eq!(parser.state(), State::Ground);
    assert!(sink.events.contains(&Event::Execute(0x18)));
}

#[test]
fn dcs_hook_put_unhook() {
    assert_eq!(
        parse(b"\x1bP1;2qHi\x1b\\"),
        vec![
            dcs_hook(&[1, 2], &[], b'q'),
            Event::DcsPut(b'H'),
            Event::DcsPut(b'i'),
            Event::DcsUnhook,
            esc(b'\\'),
        ]
    );
}

#[test]
fn esc_dispatch_table_carries_final_byte_and_intermediates() {
    // The full event (final byte + intermediates) is the oracle: a
    // dispatcher that dropped the `#` intermediate from DECALN fails here.
    let cases: &[(&[u8], &[u8], u8, &str)] = &[
        (b"\x1bc", b"", b'c', "RIS: ESC c"),
        (b"\x1b7", b"", b'7', "DECSC: ESC 7"),
        (b"\x1b8", b"", b'8', "DECRC: ESC 8"),
        (b"\x1bD", b"", b'D', "IND: ESC D"),
        (b"\x1bE", b"", b'E', "NEL: ESC E"),
        (b"\x1bM", b"", b'M', "RI: ESC M"),
        (b"\x1b#8", b"#", b'8', "DECALN: ESC # 8"),
    ];
    for (bytes, intermediates, final_byte, label) in cases {
        assert_eq!(
            parse(bytes),
            vec![Event::Esc {
                intermediates: intermediates.to_vec(),
                final_byte: *final_byte,
            }],
            "{label}",
        );
    }
}
