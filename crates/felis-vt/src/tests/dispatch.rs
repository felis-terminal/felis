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
fn esc_dispatch_simple() {
    let evs = parse(b"\x1bc"); // ESC c — full reset
    assert_eq!(
        evs,
        vec![Event::Esc {
            intermediates: vec![],
            final_byte: b'c'
        }]
    );
}

#[test]
fn csi_dispatch_with_params() {
    let evs = parse(b"\x1b[1;2H"); // CUP row=1, col=2
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![1, 2],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

#[test]
fn csi_dispatch_no_params() {
    let evs = parse(b"\x1b[H"); // CUP default
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

#[test]
fn csi_single_digit_param_decodes_as_decimal() {
    // Opens with `5`: the sibling tests open with `1`, where `'1' - '0'`
    // and `'1' / '0'` both yield 1 and never pin the digit decode.
    let evs = parse(b"\x1b[5H"); // CUP row=5
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![5],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
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
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![0],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'm',
        }]
    );
}

#[test]
fn csi_state_is_cleared_between_sequences() {
    // A second CSI must not inherit the first's params or intermediates;
    // every other test parses a single sequence from a fresh parser.
    let evs = parse(b"\x1b[1!p\x1b[H");
    assert_eq!(
        evs,
        vec![
            Event::Csi {
                params: vec![1],
                subparams: 0,
                intermediates: vec![b'!'],
                ignore: false,
                final_byte: b'p',
            },
            Event::Csi {
                params: vec![],
                subparams: 0,
                intermediates: vec![],
                ignore: false,
                final_byte: b'H',
            },
        ]
    );
}

#[test]
fn csi_dispatch_with_private_intermediate() {
    let evs = parse(b"\x1b[?25h"); // DECTCEM show cursor
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![25],
            subparams: 0,
            intermediates: vec![b'?'],
            ignore: false,
            final_byte: b'h',
        }]
    );
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
    let evs = parse(b"\x1bP1;2qHi\x1b\\");
    assert!(matches!(
        evs[0],
        Event::DcsHook {
            ref params,
            ref intermediates,
            ignore: false,
            final_byte: b'q',
        } if params == &[1, 2] && intermediates.is_empty()
    ));
    assert_eq!(evs[1], Event::DcsPut(b'H'));
    assert_eq!(evs[2], Event::DcsPut(b'i'));
    assert_eq!(evs[3], Event::DcsUnhook);
}

#[test]
fn esc_dispatch_table_carries_final_byte_and_intermediates() {
    // The full event (final byte + intermediates) is the oracle: a
    // dispatcher that dropped the `#` intermediate from DECALN fails here.
    let cases: &[(&[u8], &[u8], u8, &str)] = &[
        (b"\x1b7", b"", b'7', "DECSC: ESC 7"),
        (b"\x1b8", b"", b'8', "DECRC: ESC 8"),
        (b"\x1bD", b"", b'D', "IND: ESC D"),
        (b"\x1bE", b"", b'E', "NEL: ESC E"),
        (b"\x1bM", b"", b'M', "RI: ESC M"),
        (b"\x1b#8", b"#", b'8', "DECALN: ESC # 8"),
    ];
    for (bytes, intermediates, final_byte, label) in cases {
        let evs = parse(bytes);
        assert_eq!(
            evs.last(),
            Some(&Event::Esc {
                intermediates: intermediates.to_vec(),
                final_byte: *final_byte,
            }),
            "{label}: got {evs:?}",
        );
    }
}
