//! Parser totality and invariant properties: the Williams DFA is the
//! trust root for every byte from a child program, so a panic here
//! corrupts grid state on attacker-controlled input.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_vt::{
    MAX_INTERMEDIATES, MAX_PARAMS, OSC_BUFFER_LIMIT, Parser, Sink, State, utf8::Decoder,
};
use proptest::prelude::*;

/// One dispatch, with its payload. Comparing logs rather than call
/// counts catches a bulk arm that reorders events or hands a callback
/// different bytes than the per-byte path does.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        bell_terminated: bool,
    },
    Apc(Vec<u8>),
}

#[derive(Debug, Default)]
struct LogSink {
    events: Vec<Event>,
    /// Overflow notifications are outside `events`: the bulk body path
    /// coalesces a dropped run into one call, so a feed split into
    /// single bytes legitimately reports more of them.
    overflows: u64,
}

impl LogSink {
    fn max_params(&self) -> usize {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Csi { params, .. } | Event::DcsHook { params, .. } => Some(params.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    fn max_intermediates(&self) -> usize {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Esc { intermediates, .. }
                | Event::Csi { intermediates, .. }
                | Event::DcsHook { intermediates, .. } => Some(intermediates.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    fn max_osc_body(&self) -> usize {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Osc { body, .. } => Some(body.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    fn executes(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, Event::Execute(_)))
            .count()
    }
}

impl Sink for LogSink {
    fn print(&mut self, byte: u8) {
        self.events.push(Event::Print(byte));
    }
    fn print_str(&mut self, bytes: &[u8]) {
        self.events.extend(bytes.iter().copied().map(Event::Print));
    }
    fn print_utf8_run(&mut self, bytes: &[u8]) {
        self.events.extend(bytes.iter().copied().map(Event::Print));
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
            bell_terminated,
        });
    }
    fn osc_overflow(&mut self) {
        self.overflows += 1;
    }
    fn apc_dispatch(&mut self, body: &[u8]) {
        self.events.push(Event::Apc(body.to_vec()));
    }
    fn apc_overflow(&mut self) {
        self.overflows += 1;
    }
}

/// Records printed bytes and byte-class contract breaches in order.
///
/// Breaches accumulate as messages for deferred assertion because callbacks
/// cannot invoke `prop_assert!` directly.
#[derive(Debug, Default)]
struct PrintRecordingSink {
    printed: Vec<u8>,
    breaches: Vec<String>,
}

impl Sink for PrintRecordingSink {
    fn print(&mut self, byte: u8) {
        self.printed.push(byte);
    }

    fn print_str(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            self.breaches.push("print_str received an empty run".into());
        }
        if let Some(bad) = bytes.iter().copied().find(|b| !(0x20..=0x7E).contains(b)) {
            self.breaches.push(format!(
                "print_str received {bad:#04x}, not printable ASCII"
            ));
        }
        self.printed.extend_from_slice(bytes);
    }

    fn print_utf8_run(&mut self, bytes: &[u8]) {
        match bytes.first() {
            None => self
                .breaches
                .push("print_utf8_run received an empty run".into()),
            Some(&first) if first < 0x80 => self.breaches.push(format!(
                "print_utf8_run run starts with {first:#04x}, not a 0x80..=0xFF byte"
            )),
            Some(_) => {}
        }
        if let Some(bad) = bytes.iter().copied().find(|&b| b < 0x20 || b == 0x7F) {
            self.breaches
                .push(format!("print_utf8_run received {bad:#04x}, not printable"));
        }
        self.printed.extend_from_slice(bytes);
    }
}

/// Biased toward long printable runs, multi-byte UTF-8, and the C0/DEL
/// bytes and escape sequences that cut a run short: uniform random bytes
/// almost never produce a run longer than two bytes.
fn bulk_print_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            4 => "[ -~]{1,40}".prop_map(String::into_bytes),
            3 => "\\PC{1,20}".prop_map(String::into_bytes),
            2 => any::<u8>().prop_map(|b| vec![b]),
            1 => Just(b"\x1b[1;2m".to_vec()),
            1 => Just(b"\x1b]0;title\x07".to_vec()),
            1 => Just(b"\x1bP$qm\x1b\\".to_vec()),
        ],
        0..24,
    )
    .prop_map(|chunks| chunks.concat())
}

/// CSI sequences dense in parameter bytes: 1–3 digit SGR-style values,
/// runs long enough to saturate a `u16`, empty params, `:` subparams,
/// counts past [`MAX_PARAMS`], private markers, and a C0, CAN, or ESC
/// landing mid-sequence.
fn csi_param_stream() -> impl Strategy<Value = Vec<u8>> {
    let param = prop_oneof![
        6 => "[0-9]{1,3}",
        2 => "[0-9]{4,20}",
        1 => Just(String::new()),
    ];
    let separator = prop_oneof![3 => Just(";"), 1 => Just(":")];
    let final_part = prop_oneof![
        8 => "[@-~]",
        1 => Just("\x08m".to_owned()),
        1 => Just("\x18".to_owned()),
        1 => Just(" q".to_owned()),
        1 => Just("\x1b".to_owned()),
    ];
    let sequence = (
        "[<=>?]?",
        proptest::collection::vec((param.clone(), separator), 0..24),
        param,
        final_part,
    )
        .prop_map(|(private, params, last, final_part)| {
            let mut seq = format!("\x1b[{private}");
            for (param, separator) in params {
                seq.push_str(&param);
                seq.push_str(separator);
            }
            seq.push_str(&last);
            seq.push_str(&final_part);
            seq.into_bytes()
        });
    proptest::collection::vec(sequence, 1..12).prop_map(|seqs| seqs.concat())
}

const fn current_state_is_valid(state: State) -> bool {
    matches!(
        state,
        State::Ground
            | State::Escape
            | State::EscapeIntermediate
            | State::CsiEntry
            | State::CsiParam
            | State::CsiIntermediate
            | State::CsiIgnore
            | State::DcsEntry
            | State::DcsParam
            | State::DcsIntermediate
            | State::DcsPassthrough
            | State::DcsIgnore
            | State::OscString
            | State::SosPmApcString
    )
}

proptest! {
    /// Any byte slice: no panic, a defined DFA state, and dispatched
    /// param / intermediate slices within the documented caps.
    #[test]
    fn parser_is_total_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut parser = Parser::new();
        let mut sink = LogSink::default();
        parser.advance(&mut sink, &bytes);
        prop_assert!(current_state_is_valid(parser.state()));
        prop_assert!(sink.max_params() <= MAX_PARAMS);
        prop_assert!(sink.max_intermediates() <= MAX_INTERMEDIATES);
        prop_assert!(sink.max_osc_body() <= OSC_BUFFER_LIMIT);
    }

    /// Single-byte feed must produce the same event stream as the slice
    /// form: same dispatches, same payloads, same order.
    #[test]
    fn slice_and_per_byte_advance_agree(
        bytes in prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..512),
            bulk_print_bytes(),
        ],
    ) {
        let mut p1 = Parser::new();
        let mut s1 = LogSink::default();
        p1.advance(&mut s1, &bytes);

        let mut p2 = Parser::new();
        let mut s2 = LogSink::default();
        for b in &bytes {
            p2.advance_byte(&mut s2, *b);
        }
        prop_assert_eq!(p1.state(), p2.state());
        prop_assert_eq!(s1.events, s2.events);
    }

    /// Splitting the input at any boundary must not change the trace.
    #[test]
    fn split_advance_is_observationally_equivalent(
        bytes in proptest::collection::vec(any::<u8>(), 0..256),
        cut in 0usize..=256,
    ) {
        let cut = cut.min(bytes.len());
        let mut p1 = Parser::new();
        let mut s1 = LogSink::default();
        p1.advance(&mut s1, &bytes);

        let mut p2 = Parser::new();
        let mut s2 = LogSink::default();
        let (head, tail) = bytes.split_at(cut);
        p2.advance(&mut s2, head);
        p2.advance(&mut s2, tail);

        prop_assert_eq!(p1.state(), p2.state());
        prop_assert_eq!(s1.events, s2.events);
    }

    /// The bulk CSI parameter arm, entered afresh at every chunk
    /// boundary, must leave the parser (params, subparam mask, overflow
    /// flag) and the dispatch log exactly where the per-byte path does.
    #[test]
    fn csi_param_runs_match_per_byte_dispatch_at_any_chunking(
        bytes in csi_param_stream(),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let mut reference = Parser::new();
        let mut per_byte = LogSink::default();
        for b in &bytes {
            reference.advance_byte(&mut per_byte, *b);
        }

        let mut cuts: Vec<usize> = cuts.iter().map(|cut| cut.index(bytes.len() + 1)).collect();
        cuts.push(0);
        cuts.push(bytes.len());
        cuts.sort_unstable();
        let mut parser = Parser::new();
        let mut bulk = LogSink::default();
        for pair in cuts.windows(2) {
            parser.advance(&mut bulk, &bytes[pair[0]..pair[1]]);
        }

        prop_assert_eq!(&parser, &reference);
        prop_assert_eq!(bulk.events, per_byte.events);
    }

    /// CAN / SUB drop the parser to Ground from any state (Williams), and
    /// `execute` fires once per cancel byte.
    #[test]
    fn cancel_bytes_return_to_ground(prefix in proptest::collection::vec(any::<u8>(), 0..256)) {
        for cancel in [0x18u8, 0x1Au8] {
            let mut parser = Parser::new();
            let mut sink = LogSink::default();
            parser.advance(&mut sink, &prefix);
            let executes_before = sink.executes();
            parser.advance_byte(&mut sink, cancel);
            prop_assert_eq!(parser.state(), State::Ground);
            prop_assert_eq!(sink.executes(), executes_before + 1);
        }
    }

    /// Every bulk print run matches the byte classes its docs promise.
    #[test]
    fn bulk_print_runs_match_their_documented_byte_classes(bytes in bulk_print_bytes()) {
        let mut parser = Parser::new();
        let mut sink = PrintRecordingSink::default();
        parser.advance(&mut sink, &bytes);
        prop_assert!(sink.breaches.is_empty(), "{:?}", sink.breaches);
    }

    /// The same, for arbitrary bytes.
    #[test]
    fn bulk_print_runs_match_their_byte_classes_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut parser = Parser::new();
        let mut sink = PrintRecordingSink::default();
        parser.advance(&mut sink, &bytes);
        prop_assert!(sink.breaches.is_empty(), "{:?}", sink.breaches);
    }

    /// The UTF-8 decoder never panics and emits at most one char per byte.
    #[test]
    fn utf8_decoder_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let mut dec = Decoder::new();
        let mut count: usize = 0;
        for b in &bytes {
            dec.push(*b, |_c| count += 1);
        }
        prop_assert!(count <= bytes.len());
    }

    /// A known-good UTF-8 string emits exactly the same chars in order.
    #[test]
    fn utf8_decoder_round_trips_valid_strings(s in "\\PC{0,64}") {
        let mut dec = Decoder::new();
        let mut out = String::new();
        for b in s.as_bytes() {
            dec.push(*b, |c| out.push(c));
        }
        prop_assert_eq!(out, s);
    }
}
