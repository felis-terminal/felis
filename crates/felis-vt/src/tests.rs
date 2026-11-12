use super::*;
use crate::test_support::{Event, LogSink};

fn parse(bytes: &[u8]) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut sink = LogSink::default();
    parser.advance(&mut sink, bytes);
    sink.events
}

fn osc(body: &[u8], end: OscEnd) -> Event {
    Event::Osc {
        body: body.to_vec(),
        end,
    }
}

fn csi(params: &[u16], intermediates: &[u8], final_byte: u8) -> Event {
    Event::Csi {
        params: params.to_vec(),
        subparams: 0,
        intermediates: intermediates.to_vec(),
        ignore: false,
        final_byte,
    }
}

fn dcs_hook(params: &[u16], intermediates: &[u8], final_byte: u8) -> Event {
    Event::DcsHook {
        params: params.to_vec(),
        intermediates: intermediates.to_vec(),
        ignore: false,
        final_byte,
    }
}

const fn esc(final_byte: u8) -> Event {
    Event::Esc {
        intermediates: vec![],
        final_byte,
    }
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

mod dispatch;
mod osc_apc;
mod overflow;
mod scan_and_chunking;
mod state_arms;
mod yielding;
