use super::*;

#[derive(Debug, Default, PartialEq, Eq)]
struct LogSink {
    events: Vec<Event>,
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Print(u8),
    Execute(u8),
    Esc {
        intermediates: Vec<u8>,
        final_byte: u8,
    },
    Csi {
        params: Vec<u16>,
        subparams: u32,
        intermediates: Vec<u8>,
        ignore: bool,
        final_byte: u8,
    },
    DcsHook {
        params: Vec<u16>,
        intermediates: Vec<u8>,
        ignore: bool,
        final_byte: u8,
    },
    DcsPut(u8),
    DcsUnhook,
    Osc {
        body: Vec<u8>,
        bell: bool,
    },
    OscOverflow,
    Apc(Vec<u8>),
    ApcOverflow,
}

impl Sink for LogSink {
    fn print(&mut self, byte: u8) {
        self.events.push(Event::Print(byte));
    }
    fn execute(&mut self, byte: u8) {
        self.events.push(Event::Execute(byte));
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        self.events.push(Event::Esc {
            intermediates: intermediates.to_vec(),
            final_byte,
        });
    }
    fn csi_dispatch(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        ignore: bool,
        final_byte: u8,
    ) {
        self.events.push(Event::Csi {
            params: params.to_vec(),
            subparams,
            intermediates: intermediates.to_vec(),
            ignore,
            final_byte,
        });
    }
    fn dcs_hook(&mut self, params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {
        self.events.push(Event::DcsHook {
            params: params.to_vec(),
            intermediates: intermediates.to_vec(),
            ignore,
            final_byte,
        });
    }
    fn dcs_put(&mut self, byte: u8) {
        self.events.push(Event::DcsPut(byte));
    }
    fn dcs_unhook(&mut self) {
        self.events.push(Event::DcsUnhook);
    }
    fn osc_dispatch(&mut self, body: &[u8], bell_terminated: bool) {
        self.events.push(Event::Osc {
            body: body.to_vec(),
            bell: bell_terminated,
        });
    }
    fn osc_overflow(&mut self) {
        self.events.push(Event::OscOverflow);
    }
    fn apc_dispatch(&mut self, body: &[u8]) {
        self.events.push(Event::Apc(body.to_vec()));
    }
    fn apc_overflow(&mut self) {
        self.events.push(Event::ApcOverflow);
    }
}

fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    parser.advance(&mut sink, bytes);
    sink.events
}

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
fn parser_debug_renders_state_fields() {
    // Pins that the hand-written `Debug` names the struct and at least one
    // tracked field.
    let dbg = format!("{:?}", Parser::new());
    assert!(dbg.contains("Parser"), "got {dbg:?}");
    assert!(dbg.contains("state"), "got {dbg:?}");
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
fn osc_dispatch_bell_terminated() {
    let evs = parse(b"\x1b]0;hello world\x07");
    assert_eq!(
        evs,
        vec![Event::Osc {
            body: b"0;hello world".to_vec(),
            bell: true,
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
                bell: false,
            },
            Event::Esc {
                intermediates: vec![],
                final_byte: b'\\',
            },
        ]
    );
}

#[test]
fn apc_dispatch_treats_0x9c_as_data_not_st() {
    // 0x9C in an APC body is data (a UTF-8 continuation byte, or a raw
    // pixel byte in a Kitty graphics chunk), NOT C1 ST.
    let evs = parse(b"\x1b_Gf=24,a=T;hello\x9cworld\x1b\\");
    assert_eq!(
        evs,
        vec![
            Event::Apc(b"Gf=24,a=T;hello\x9cworld".to_vec()),
            Event::Esc {
                intermediates: vec![],
                final_byte: b'\\',
            },
        ]
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
            Event::Esc {
                intermediates: vec![],
                final_byte: b'\\',
            },
            Event::Apc(b"second".to_vec()),
            Event::Esc {
                intermediates: vec![],
                final_byte: b'\\',
            },
        ]
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
fn can_inside_osc_discards_the_partial_payload() {
    // security-model.md "OSC 8 hyperlinks and OSC 7 CWD": an OSC body
    // truncated by an in-band CAN/SUB must not install its partial value,
    // so the OSC event that fires must carry none of the partial bytes.
    for cancel in [0x18u8, 0x1A] {
        let mut bytes = Vec::from(&b"\x1b]7;/evil/path"[..]);
        bytes.push(cancel);
        let evs = parse(&bytes);
        for ev in &evs {
            if let Event::Osc { body, .. } = ev {
                assert!(
                    body.is_empty(),
                    "canceled OSC leaked a partial body: {body:?}",
                );
            }
        }
    }
}

#[test]
fn parameter_overflow_sets_ignore_flag() {
    // 17 params; max is 16.
    let mut bytes = Vec::from(&b"\x1b["[..]);
    for i in 0..17 {
        if i > 0 {
            bytes.push(b';');
        }
        bytes.extend_from_slice(format!("{i}").as_bytes());
    }
    bytes.push(b'm'); // SGR
    let evs = parse(&bytes);
    let csi = evs.iter().find_map(|e| match e {
        Event::Csi { ignore, .. } => Some(*ignore),
        _ => None,
    });
    assert_eq!(csi, Some(true));
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
fn scan_printable_run_matches_scalar() {
    // The byte-at-a-time reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| !(0x20..=0x7E).contains(&b))
            .unwrap_or(buf.len())
    }
    // Every byte value at each offset 0..16 of a printable-filled window
    // covers the SWAR body, the chunk boundary, and the scalar tail. The
    // two-byte borrow interaction is left to the Kani proof.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [b'x'; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_printable_run(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // Tail-only path.
    for len in 0..8 {
        let buf = vec![b'A'; len];
        assert_eq!(
            scan_printable_run(&buf),
            scalar(&buf),
            "all-printable len {len}"
        );
    }
}

#[test]
fn scan_mixed_print_run_matches_scalar() {
    // The per-byte control-or-DEL reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| b < 0x20 || b == 0x7F)
            .unwrap_or(buf.len())
    }
    // High-byte background (0xE4 is a UTF-8 continuation byte) with one
    // byte of every value at each offset into a second chunk.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [0xE4u8; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_mixed_print_run(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // The scalar-tail loop, where the 0x1F/0x20 and 0x7E/0x7F/0x80 edges
    // separate `<` from `<=`.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..7 {
            let mut buf = [0xE4u8; 7];
            buf[pos] = byte;
            assert_eq!(
                scan_mixed_print_run(&buf),
                scalar(&buf),
                "tail byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // The run length must equal the buffer length, not collapse to 0.
    for len in 0..16 {
        let buf: Vec<u8> = (0..len)
            .map(|i| if i % 2 == 0 { 0xE4u8 } else { b' ' })
            .collect();
        assert_eq!(scan_mixed_print_run(&buf), len, "interleaved len {len}");
    }
}

#[test]
fn scan_string_body_matches_scalar() {
    // The per-byte special-byte reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| matches!(b, 0x07 | 0x18 | 0x1A | 0x1B))
            .unwrap_or(buf.len())
    }
    // Same single-offending-byte sweep as `scan_printable_run_matches_scalar`.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [b'x'; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_string_body(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    for len in 0..8 {
        let buf = vec![0xE4u8; len];
        assert_eq!(scan_string_body(&buf), scalar(&buf), "all-body len {len}");
    }
}

#[test]
fn csi_param_chunked_advance_matches_per_byte_dispatch() {
    // `advance` over a CSI-dense stream must produce the same dispatches
    // (params, subparam mask, overflow flag) as `advance_byte` at every
    // chunk boundary. The stream mixes multi-digit 256-color SGR,
    // `:`-subparams, an empty leading param, a >16-param overflow, a C0
    // executed mid-sequence, and a CAN cancellation.
    let mut seq: Vec<u8> = Vec::new();
    seq.extend_from_slice(b"\x1b[38;5;196m\x1b[48;5;21mx");
    seq.extend_from_slice(b"\x1b[4:3m\x1b[58:2::255:128:0m");
    seq.extend_from_slice(b"\x1b[;5H");
    seq.extend_from_slice(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17m");
    seq.extend_from_slice(b"\x1b[3\x0812m"); // BS executes inside CsiParam
    seq.extend_from_slice(b"\x1b[99\x18z"); // CAN cancels; `z` prints

    let mut per_byte = LogSink::default();
    let mut parser = Parser::new();
    for &b in &seq {
        parser.advance_byte(&mut per_byte, b);
    }
    for split in 0..=seq.len() {
        let mut bulk = LogSink::default();
        let mut parser = Parser::new();
        parser.advance(&mut bulk, &seq[..split]);
        parser.advance(&mut bulk, &seq[split..]);
        assert_eq!(bulk.events, per_byte.events, "split at {split}");
    }
}

#[test]
fn osc_body_bulk_path_matches_per_byte_dispatch() {
    // The bulk `OscBuffer::extend` path must produce the same dispatch as
    // feeding the same OSC byte by byte, including a body holding C0
    // bytes outside the special set.
    let mut body = vec![b'6', b';'];
    body.extend(std::iter::repeat_n(b'a', 100));
    body.push(0x0A); // non-special C0 stays payload
    body.extend(std::iter::repeat_n(0xE4, 7)); // high bytes stay payload
    let mut seq = vec![0x1B, b']'];
    seq.extend_from_slice(&body);
    seq.push(0x07);

    let mut bulk = LogSink::default();
    Parser::new().advance(&mut bulk, &seq);
    let mut per_byte = LogSink::default();
    let mut parser = Parser::new();
    for &b in &seq {
        parser.advance_byte(&mut per_byte, b);
    }
    assert_eq!(bulk.events, per_byte.events);
}

#[test]
fn csi_intermediate_overflow_sets_ignore_and_caps_slice() {
    // The other tests never overflow the intermediate buffer, so
    // `Intermediates::push`'s `len < MAX_INTERMEDIATES` guard is pinned
    // only here: a `<=` slip writes one past the fixed array.
    let intermediates: Vec<u8> = (0..=MAX_INTERMEDIATES)
        .map(|i| b'!' + u8::try_from(i).unwrap())
        .collect();
    let mut input = vec![0x1b, b'['];
    input.extend_from_slice(&intermediates);
    input.push(b'p');
    let evs = parse(&input);
    assert_eq!(
        evs,
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: intermediates[..MAX_INTERMEDIATES].to_vec(),
            ignore: true,
            final_byte: b'p',
        }]
    );
}

struct CappedString {
    kind: &'static str,
    intro: &'static [u8],
    terminator: &'static [u8],
    limit: usize,
    overflow: Event,
    body_len: fn(&Event) -> Option<usize>,
}

const CAPPED_STRINGS: [CappedString; 2] = [
    CappedString {
        kind: "OSC",
        intro: b"\x1b]0;",
        terminator: b"\x07",
        limit: OSC_BUFFER_LIMIT,
        overflow: Event::OscOverflow,
        body_len: |e| match e {
            Event::Osc { body, .. } => Some(body.len()),
            _ => None,
        },
    },
    CappedString {
        kind: "APC",
        intro: b"\x1b_",
        terminator: b"\x1b\\",
        limit: APC_BUFFER_LIMIT,
        overflow: Event::ApcOverflow,
        body_len: |e| match e {
            Event::Apc(body) => Some(body.len()),
            _ => None,
        },
    },
];

fn assert_truncated_dispatch(s: &CappedString, events: &[Event]) {
    assert!(
        events.contains(&s.overflow),
        "{} overflow must be reported",
        s.kind
    );
    let dispatched = events
        .iter()
        .find_map(s.body_len)
        .unwrap_or_else(|| panic!("{} dispatch must still fire after overflow", s.kind));
    assert_eq!(
        dispatched, s.limit,
        "{} body must be cut at its limit",
        s.kind
    );
}

#[test]
fn string_overflow_truncates_at_buffer_limit() {
    // A runaway string (a program that never sends ST) must neither wedge
    // the parser nor grow the buffer without bound.
    for s in &CAPPED_STRINGS {
        let mut bytes = s.intro.to_vec();
        bytes.extend(std::iter::repeat_n(b'X', s.limit + 8));
        bytes.extend_from_slice(s.terminator);
        assert_truncated_dispatch(s, &parse(&bytes));
    }
}

#[test]
fn string_overflow_via_split_extend_truncates_at_limit() {
    // Splitting the body across two `advance` calls makes the second
    // `extend` truncate against an already-filled buffer.
    for s in &CAPPED_STRINGS {
        let mut parser = Parser::new();
        let mut sink = LogSink::default();
        let mut first = s.intro.to_vec();
        first.extend(std::iter::repeat_n(b'a', s.limit * 3 / 4));
        parser.advance(&mut sink, &first);
        let mut second = vec![b'b'; s.limit / 2];
        second.extend_from_slice(s.terminator);
        parser.advance(&mut sink, &second);
        assert_truncated_dispatch(s, &sink.events);
    }
}

#[test]
fn string_overflow_on_per_byte_push_path() {
    // `advance_byte` drives the per-byte `push` guard, not the bulk
    // `extend` path.
    for s in &CAPPED_STRINGS {
        let mut parser = Parser::new();
        let mut sink = LogSink::default();
        for &b in s.intro {
            parser.advance_byte(&mut sink, b);
        }
        for _ in 0..s.limit + 8 {
            parser.advance_byte(&mut sink, b'a');
        }
        for &b in s.terminator {
            parser.advance_byte(&mut sink, b);
        }
        assert_truncated_dispatch(s, &sink.events);
    }
}

#[test]
fn osc_after_osc_resets_buffer() {
    // Two consecutive OSC payloads must not concatenate.
    let evs = parse(b"\x1b]0;first\x07\x1b]0;second\x07");
    let oscs: Vec<Vec<u8>> = evs
        .iter()
        .filter_map(|e| match e {
            Event::Osc { body, .. } => Some(body.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(oscs, vec![b"0;first".to_vec(), b"0;second".to_vec()]);
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

/// Per-byte driver for handlers the bulk `advance` fast path would bypass.
fn parse_per_byte(bytes: &[u8]) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    for &b in bytes {
        parser.advance_byte(&mut sink, b);
    }
    sink.events
}

// Each test feeds a sequence into one parser state, then a follow-up byte
// whose dispatch differs per state.

/// A C0 control inside Escape executes in place and does not abort.
#[test]
fn escape_executes_c0_control_in_place() {
    assert_eq!(parse(b"\x1b\x07"), vec![Event::Execute(0x07)]);
}

/// DEL in Escape is ignored, not an abort.
#[test]
fn escape_ignores_del_then_dispatches() {
    assert_eq!(
        parse(b"\x1b\x7fB"),
        vec![Event::Esc {
            intermediates: vec![],
            final_byte: b'B',
        }]
    );
}

/// Entering Escape clears intermediates from an aborted sequence.
#[test]
fn escape_entry_clears_stale_intermediates() {
    assert_eq!(
        parse(b"\x1b(\x1bB"),
        vec![Event::Esc {
            intermediates: vec![],
            final_byte: b'B',
        }]
    );
}

/// `EscapeIntermediate`: a C0 control executes in place, a further
/// intermediate accumulates, DEL is ignored; none abort.
#[test]
fn escape_intermediate_arms_keep_the_sequence_alive() {
    assert_eq!(parse(b"\x1b \x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b\x20\x20B"),
        vec![Event::Esc {
            intermediates: vec![0x20, 0x20],
            final_byte: b'B',
        }]
    );
    assert_eq!(
        parse(b"\x1b\x20\x7fB"),
        vec![Event::Esc {
            intermediates: vec![0x20],
            final_byte: b'B',
        }]
    );
}

/// `CsiEntry`: a C0 control executes; DEL is ignored.
#[test]
fn csi_entry_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[\x7fH"),
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

/// `CsiParam`: a C0 control executes; DEL is ignored.
#[test]
fn csi_param_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[5\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[5\x7fH"),
        vec![Event::Csi {
            params: vec![5],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

/// `CsiIntermediate`: a C0 control executes; DEL is ignored.
#[test]
fn csi_intermediate_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[\x20\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[\x20\x7fH"),
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

/// `CsiIgnore`: a C0 control still executes, the final byte returns to
/// Ground, and no CSI is dispatched.
#[test]
fn csi_ignore_executes_c0_and_returns_to_ground_on_final() {
    assert_eq!(
        parse(b"\x1b[5<\x07mX"),
        vec![Event::Execute(0x07), Event::Print(b'X')]
    );
}

/// `DcsEntry`: the final byte hooks; C0/DEL is ignored; an intermediate,
/// a sub-param `:`, and a private `<` each advance rather than dropping
/// to `DcsIgnore`.
#[test]
fn dcs_entry_arms_route_to_passthrough_not_ignore() {
    let hook = |params: Vec<u16>, intermediates: Vec<u8>| {
        vec![Event::DcsHook {
            params,
            intermediates,
            ignore: false,
            final_byte: b'q',
        }]
    };
    assert_eq!(parse(b"\x1bPq"), hook(vec![], vec![]));
    assert_eq!(parse(b"\x1bP\x07q"), hook(vec![], vec![]));
    assert_eq!(parse(b"\x1bP q"), hook(vec![], vec![0x20]));
    assert_eq!(parse(b"\x1bP:q"), hook(vec![0], vec![]));
    assert_eq!(parse(b"\x1bP<q"), hook(vec![], vec![0x3C]));
}

/// `DcsParam`: C0/DEL is ignored; an intermediate advances to
/// `DcsIntermediate`. Both still hook.
#[test]
fn dcs_param_c0_ignored_and_intermediate_advances() {
    assert_eq!(
        parse(b"\x1bP5\x07q"),
        vec![Event::DcsHook {
            params: vec![5],
            intermediates: vec![],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP5 q"),
        vec![Event::DcsHook {
            params: vec![5],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
}

/// `DcsIntermediate`: the final byte hooks; C0/DEL is ignored; a further
/// intermediate accumulates.
#[test]
fn dcs_intermediate_arms_hook_and_accumulate() {
    assert_eq!(
        parse(b"\x1bP q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP \x07q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP\x20\x20q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20, 0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
}

/// Intermediate overflow alone sets `ignore` on the hook.
#[test]
fn dcs_hook_ignore_set_by_intermediate_overflow_alone() {
    assert_eq!(
        parse(b"\x1bP!\x22#$q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![b'!', b'"', b'#'],
            ignore: true,
            final_byte: b'q',
        }]
    );
}

/// `DcsPassthrough` ignores DEL (no `dcs_put`) while putting every other byte.
#[test]
fn dcs_passthrough_puts_data_but_ignores_del() {
    assert_eq!(
        parse(b"\x1bPqAB\x7fC"),
        vec![
            Event::DcsHook {
                params: vec![],
                intermediates: vec![],
                ignore: false,
                final_byte: b'q',
            },
            Event::DcsPut(b'A'),
            Event::DcsPut(b'B'),
            Event::DcsPut(b'C'),
        ]
    );
}

/// A normal APC byte on the per-byte `push` path must NOT signal overflow.
#[test]
fn apc_normal_byte_does_not_signal_overflow() {
    assert_eq!(
        parse_per_byte(b"\x1b_X\x1b\\"),
        vec![
            Event::Apc(vec![b'X']),
            Event::Esc {
                intermediates: vec![],
                final_byte: b'\\',
            },
        ]
    );
}

/// A buffer already past `LIMIT` drops the whole run instead of panicking.
#[test]
fn limited_buffer_extend_over_capacity_drops_the_run() {
    let mut buf = OscBuffer {
        bytes: vec![b'x'; OSC_BUFFER_LIMIT + 4],
    };
    assert!(!buf.extend(b"more"));
    assert_eq!(buf.bytes().len(), OSC_BUFFER_LIMIT + 4);
    assert!(!buf.push(b'y'));
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
