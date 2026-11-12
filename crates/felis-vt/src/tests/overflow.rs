use super::*;

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
