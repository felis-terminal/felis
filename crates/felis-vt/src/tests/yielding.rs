use super::*;

/// Runs `advance_until_yield` over `chunks` until each is consumed,
/// returning the events and every stop offset.
fn parse_yielding(chunks: &[&[u8]]) -> (Vec<Event>, Vec<usize>) {
    parse_yielding_with(
        LogSink {
            yield_on_apc: true,
            ..LogSink::default()
        },
        chunks,
    )
}

fn parse_yielding_with(mut sink: LogSink, chunks: &[&[u8]]) -> (Vec<Event>, Vec<usize>) {
    let mut parser = Parser::new();
    let mut stops = Vec::new();
    for chunk in chunks {
        let mut rest = *chunk;
        while let Some(n) = parser.advance_until_yield(&mut sink, rest) {
            stops.push(n);
            rest = &rest[n..];
        }
    }
    (sink.events, stops)
}

fn parse_yielding_on_osc(chunks: &[&[u8]]) -> (Vec<Event>, Vec<usize>) {
    parse_yielding_with(
        LogSink {
            yield_on_osc: true,
            ..LogSink::default()
        },
        chunks,
    )
}

#[test]
fn advance_until_yield_stops_on_the_bel_that_ends_the_osc() {
    let (evs, stops) = parse_yielding_on_osc(&[b"a\x1b]0;x\x07b"]);
    assert_eq!(stops, vec![7]);
    assert_eq!(evs, parse(b"a\x1b]0;x\x07b"));
}

#[test]
fn advance_until_yield_stops_on_the_backslash_of_the_st_that_ends_the_osc() {
    let (evs, stops) = parse_yielding_on_osc(&[b"a\x1b]0;x\x1b\\b"]);
    assert_eq!(
        stops,
        vec![8],
        "after the \\, once the ESC is known to be ST"
    );
    assert_eq!(evs, parse(b"a\x1b]0;x\x1b\\b"));
}

#[test]
fn advance_until_yield_stops_on_an_st_split_across_chunks() {
    let (evs, stops) = parse_yielding_on_osc(&[b"\x1b]0;x\x1b", b"\\b"]);
    assert_eq!(stops, vec![1]);
    assert_eq!(evs, parse(b"\x1b]0;x\x1b\\b"));
}

#[test]
fn advance_until_yield_stops_on_the_byte_that_both_interrupts_an_osc_and_opens_the_next() {
    let (evs, stops) = parse_yielding_on_osc(&[b"\x1b]0;x\x1b]0;y\x07"]);
    assert_eq!(stops, vec![7, 4]);
    assert_eq!(evs, parse(b"\x1b]0;x\x1b]0;y\x07"));
}

#[test]
fn advance_until_yield_does_not_ask_after_bytes_outside_a_string() {
    let sink = LogSink {
        armed: true,
        ..LogSink::default()
    };
    let (_, stops) = parse_yielding_with(sink, &[b"a\x1b[1m\x1b7\x07\n"]);
    assert_eq!(stops, Vec::<usize>::new());
}

#[test]
fn advance_until_yield_stops_on_the_byte_that_ends_the_apc() {
    let (evs, stops) = parse_yielding(&[b"a\x1b_Gx\x1b\\b"]);
    assert_eq!(stops, vec![6], "stops after the ESC of ESC \\");
    assert_eq!(evs, parse(b"a\x1b_Gx\x1b\\b"));
}

#[test]
fn advance_until_yield_stops_on_an_apc_ending_a_chunk() {
    let (evs, stops) = parse_yielding(&[b"\x1b_Gx\x1b", b"\\b"]);
    assert_eq!(stops, vec![5], "a stop on the last byte still reports it");
    assert_eq!(evs, parse(b"\x1b_Gx\x1b\\b"));
}

#[test]
fn advance_until_yield_stops_on_can_and_sub() {
    for end in [0x18u8, 0x1A] {
        let bytes = [b"\x1b_Gx".as_slice(), &[end], b"b"].concat();
        let (evs, stops) = parse_yielding(&[&bytes]);
        assert_eq!(stops, vec![5], "terminator {end:#x}");
        assert_eq!(evs, parse(&bytes));
    }
}

#[test]
fn advance_until_yield_resumes_an_escape_that_ended_the_apc() {
    let (evs, stops) = parse_yielding(&[b"\x1b_Gx\x1b[1m"]);
    assert_eq!(stops, vec![5]);
    assert_eq!(evs, parse(b"\x1b_Gx\x1b[1m"), "ESC [ still reads as a CSI");
}

#[test]
fn advance_until_yield_without_a_yield_parses_everything() {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    let bytes = b"a\x1b_Gx\x1b\\b";
    assert_eq!(parser.advance_until_yield(&mut sink, bytes), None);
    assert_eq!(sink.events, parse(bytes));
}
