use super::*;

#[test]
fn osc_dispatch_bell_terminated() {
    let evs = parse(b"\x1b]0;hello world\x07");
    assert_eq!(
        evs,
        vec![Event::Osc {
            body: b"0;hello world".to_vec(),
            end: OscEnd::Bel,
        }]
    );
}

#[test]
fn osc_dispatch_st_terminated() {
    let evs = parse(b"\x1b]2;title\x1b\\");
    assert_eq!(
        evs,
        vec![
            Event::Osc {
                body: b"2;title".to_vec(),
                end: OscEnd::St,
            },
            esc(b'\\'),
        ]
    );
}

#[test]
fn osc_ended_by_an_escape_sequence_dispatches_as_interrupted_before_it() {
    assert_eq!(
        parse(b"\x1b]2;title\x1b[1m"),
        vec![osc(b"2;title", OscEnd::Interrupted), csi(&[1], &[], b'm')]
    );
    assert_eq!(
        parse(b"\x1b]2;title\x1b7"),
        vec![osc(b"2;title", OscEnd::Interrupted), esc(b'7')]
    );
}

#[test]
fn osc_ended_by_another_osc_dispatches_both_separately() {
    assert_eq!(
        parse(b"\x1b]2;a\x1b]2;b\x07"),
        vec![osc(b"2;a", OscEnd::Interrupted), osc(b"2;b", OscEnd::Bel)]
    );
}

#[test]
fn osc_ended_by_a_doubled_escape_is_interrupted_and_the_second_escape_stays_live() {
    assert_eq!(
        parse(b"\x1b]2;a\x1b\x1b\\"),
        vec![osc(b"2;a", OscEnd::Interrupted), esc(b'\\')]
    );
}

#[test]
fn osc_ended_by_can_or_sub_after_its_escape_is_interrupted_with_the_body() {
    for cancel in [0x18u8, 0x1A] {
        let mut bytes = b"\x1b]2;a\x1b".to_vec();
        bytes.push(cancel);
        assert_eq!(
            parse(&bytes),
            vec![osc(b"2;a", OscEnd::Interrupted), Event::Execute(cancel)]
        );
    }
}

#[test]
fn osc_ended_by_a_c0_control_after_its_escape_is_interrupted() {
    assert_eq!(
        parse(b"\x1b]2;a\x1b\n"),
        vec![osc(b"2;a", OscEnd::Interrupted), Event::Execute(b'\n')]
    );
}

#[test]
fn osc_escape_at_a_chunk_end_waits_for_the_next_chunk() {
    for (next, want) in [
        (&b"\\"[..], vec![osc(b"2;a", OscEnd::St), esc(b'\\')]),
        (&b"x"[..], vec![osc(b"2;a", OscEnd::Interrupted), esc(b'x')]),
    ] {
        let mut parser = Parser::new();
        let mut sink = LogSink::default();
        parser.advance(&mut sink, b"\x1b]2;a\x1b");
        assert_eq!(sink.events, vec![]);
        assert_eq!(parser.state(), State::Escape);
        parser.advance(&mut sink, next);
        assert_eq!(sink.events, want);
        assert_eq!(parser.state(), State::Ground);
    }
}

#[test]
fn osc_terminated_by_st_or_bel_does_not_leak_its_body_into_the_next_osc() {
    assert_eq!(
        parse(b"\x1b]2;a\x1b\\\x1b]2;b\x1b\\"),
        vec![
            osc(b"2;a", OscEnd::St),
            esc(b'\\'),
            osc(b"2;b", OscEnd::St),
            esc(b'\\'),
        ]
    );
    assert_eq!(
        parse(b"\x1b]0;first\x07\x1b]0;second\x07"),
        vec![osc(b"0;first", OscEnd::Bel), osc(b"0;second", OscEnd::Bel)]
    );
}

#[test]
fn escape_outside_an_osc_dispatches_no_osc() {
    assert_eq!(parse(b"\x1b\\"), vec![esc(b'\\')]);
    assert_eq!(
        parse(b"\x1b_Gx\x1b\\"),
        vec![Event::Apc(b"Gx".to_vec()), esc(b'\\')]
    );
}

#[test]
fn apc_dispatch_treats_0x9c_as_data_not_st() {
    // 0x9C in an APC body is data (a UTF-8 continuation byte, or a raw
    // pixel byte in a Kitty graphics chunk), NOT C1 ST.
    let evs = parse(b"\x1b_Gf=24,a=T;hello\x9cworld\x1b\\");
    assert_eq!(
        evs,
        vec![Event::Apc(b"Gf=24,a=T;hello\x9cworld".to_vec()), esc(b'\\')]
    );
}

#[test]
fn apc_payload_can_be_empty() {
    // A producer that pre-flushes its connection must not make the parser
    // partial.
    let evs = parse(b"\x1b_\x1b\\");
    assert!(matches!(evs.first(), Some(Event::Apc(b)) if b.is_empty()));
}

#[test]
fn sos_body_is_silently_dropped() {
    // Only APC bodies route; SOS fires no apc_dispatch.
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    parser.advance(&mut sink, b"\x1bXshould-vanish\x1b\\");
    assert_eq!(parser.state(), State::Ground);
    assert!(
        !sink
            .events
            .iter()
            .any(|e| matches!(e, Event::Apc(_) | Event::ApcOverflow)),
        "SOS body must not surface as APC dispatch"
    );
}

#[test]
fn pm_body_is_silently_dropped() {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    parser.advance(&mut sink, b"\x1b^vanish\x1b\\");
    assert_eq!(parser.state(), State::Ground);
    assert!(
        !sink
            .events
            .iter()
            .any(|e| matches!(e, Event::Apc(_) | Event::ApcOverflow)),
        "PM body must not surface as APC dispatch"
    );
}

#[test]
fn apc_after_apc_resets_buffer() {
    // Two consecutive APC bodies must not concatenate.
    let evs = parse(b"\x1b_first\x1b\\\x1b_second\x1b\\");
    assert_eq!(
        evs,
        vec![
            Event::Apc(b"first".to_vec()),
            esc(b'\\'),
            Event::Apc(b"second".to_vec()),
            esc(b'\\'),
        ]
    );
}

#[test]
fn can_inside_osc_discards_the_partial_payload() {
    // security-model.md "OSC 8 hyperlinks and OSC 7 CWD": an OSC body
    // truncated by an in-band CAN/SUB must not install its partial value,
    // so the OSC event that fires must carry none of the partial bytes.
    for cancel in [0x18u8, 0x1A] {
        let mut bytes = Vec::from(&b"\x1b]7;/evil/path"[..]);
        bytes.push(cancel);
        assert_eq!(
            parse(&bytes),
            vec![
                Event::Execute(cancel),
                Event::Osc {
                    body: vec![],
                    end: OscEnd::Interrupted,
                },
            ],
            "cancel byte {cancel:#04x}",
        );
    }
}

#[test]
fn osc_number_separates_an_absent_payload_from_an_empty_one() {
    // The distinction a dispatcher reads to tell `OSC 1 ST` (leave the
    // icon name alone) from `OSC 1 ; ST` (clear it).
    assert_eq!(osc_number(b"1"), Some((1, None)));
    assert_eq!(osc_number(b"1;"), Some((1, Some(&b""[..]))));
    assert_eq!(osc_number(b"133;D;0"), Some((133, Some(&b"D;0"[..]))));
}

#[test]
fn osc_number_rejects_a_code_that_is_not_a_u16_of_digits() {
    assert_eq!(osc_number(b""), None);
    assert_eq!(osc_number(b";x"), None);
    assert_eq!(osc_number(b"1x;body"), None);
    assert_eq!(osc_number(b"65536;body"), None);
}

#[test]
fn split_osc_first_keeps_every_later_separator_in_the_remainder() {
    // Only the first `;` is consumed, so a title or URI carrying `;`
    // reaches its handler whole.
    assert_eq!(split_osc_first(b"a;b;c"), (&b"a"[..], Some(&b"b;c"[..])));
    assert_eq!(split_osc_first(b"solo"), (&b"solo"[..], None));
}

#[test]
fn split_osc_yields_one_empty_field_per_separator() {
    // The fixed-arity codes count fields: `OSC 4 ; ; ?` is an idx/spec
    // pair with an empty idx.
    assert_eq!(split_osc(b";;").collect::<Vec<_>>(), vec![b""; 3]);
    assert_eq!(
        split_osc(b"4;?").collect::<Vec<_>>(),
        vec![&b"4"[..], &b"?"[..]]
    );
}
