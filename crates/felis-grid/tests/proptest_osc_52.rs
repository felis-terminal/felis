//! Property: OSC 52 set / clear / query is a coherent state machine.
//!
//! Spec sources: `docs/reference/protocols/vt-compliance.md` "OSC"
//! (`OSC 52`) and `docs/reference/spec.md` REQ-802 / REQ-803.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;
use felis_vt::Parser;
use proptest::prelude::*;

mod common;
use common::drive_with;

const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Independent of the production base64 so the round-trip checks the
/// production encoder against a second implementation.
fn b64_encode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHA[(b0 >> 2) as usize]);
        out.push(ALPHA[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize]);
        if chunk.len() == 1 {
            out.push(b'=');
            out.push(b'=');
        } else {
            out.push(ALPHA[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize]);
            if chunk.len() == 2 {
                out.push(b'=');
            } else {
                out.push(ALPHA[(b2 & 0x3F) as usize]);
            }
        }
    }
    out
}

fn b64_decode(input: &[u8]) -> Vec<u8> {
    let lookup = |b: u8| -> u8 {
        match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in input.chunks(4) {
        if chunk.len() < 4 {
            break;
        }
        let d0 = lookup(chunk[0]);
        let d1 = lookup(chunk[1]);
        out.push((d0 << 2) | (d1 >> 4));
        if chunk[2] == b'=' {
            break;
        }
        let d2 = lookup(chunk[2]);
        out.push(((d1 & 0x0F) << 4) | (d2 >> 2));
        if chunk[3] == b'=' {
            break;
        }
        let d3 = lookup(chunk[3]);
        out.push(((d2 & 0x03) << 6) | d3);
    }
    out
}

fn osc_52_set(selector: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::from(b"\x1b]52;");
    out.extend_from_slice(selector);
    out.push(b';');
    out.extend_from_slice(&b64_encode(payload));
    out.push(0x07);
    out
}

fn osc_52_query(selector: &[u8]) -> Vec<u8> {
    let mut out = Vec::from(b"\x1b]52;");
    out.extend_from_slice(selector);
    out.extend_from_slice(b";?\x07");
    out
}

fn osc_52_clear(selector: &[u8]) -> Vec<u8> {
    let mut out = Vec::from(b"\x1b]52;");
    out.extend_from_slice(selector);
    out.extend_from_slice(b";!\x07");
    out
}

fn parse_query_response(response: &[u8]) -> (Vec<u8>, Vec<u8>) {
    assert!(response.starts_with(b"\x1b]52;"));
    assert_eq!(*response.last().unwrap(), 0x07);
    let body_start = b"\x1b]52;".len();
    let body_end = response.len() - 1;
    let mid = response[body_start..body_end]
        .iter()
        .position(|&b| b == b';')
        .unwrap();
    let selector = response[body_start..body_start + mid].to_vec();
    let payload_b64 = &response[body_start + mid + 1..body_end];
    (selector, b64_decode(payload_b64))
}

proptest! {
    #[test]
    fn set_then_query_round_trips_bytes(
        payload in proptest::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 1);
        drive_with(&mut p, &mut g,&osc_52_set(b"c", &payload));
        prop_assert!(g.take_pending_clipboard_set().is_some());

        drive_with(&mut p, &mut g,&osc_52_query(b"c"));
        let response = common::responses(&mut g).pop().unwrap();
        let (sel, decoded) = parse_query_response(&response);
        prop_assert_eq!(sel, b"c".to_vec());
        prop_assert_eq!(decoded, payload);
    }

    #[test]
    fn clear_drops_cache_so_next_query_is_empty(
        payload in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 1);
        drive_with(&mut p, &mut g,&osc_52_set(b"c", &payload));
        drop(g.take_pending_clipboard_set());

        drive_with(&mut p, &mut g,&osc_52_clear(b"c"));
        prop_assert!(common::responses(&mut g).is_empty(),
            "clear must not emit a response");
        prop_assert!(g.take_pending_clipboard_set().is_none(),
            "clear must not hand the host a clipboard write");

        drive_with(&mut p, &mut g,&osc_52_query(b"c"));
        let response = common::responses(&mut g).pop().unwrap();
        let (_, decoded) = parse_query_response(&response);
        prop_assert!(decoded.is_empty());
    }

    #[test]
    fn repeated_sets_preserve_only_the_latest(
        payloads in proptest::collection::vec(
            proptest::collection::vec(any::<u8>(), 0..32),
            1..6,
        ),
    ) {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 1);
        for payload in &payloads {
            drive_with(&mut p, &mut g,&osc_52_set(b"c", payload));
            drop(g.take_pending_clipboard_set());
        }

        drive_with(&mut p, &mut g,&osc_52_query(b"c"));
        let response = common::responses(&mut g).pop().unwrap();
        let (_, decoded) = parse_query_response(&response);
        prop_assert_eq!(decoded, payloads.last().unwrap().clone());
    }

    #[test]
    fn primary_set_does_not_leak_into_clipboard_query(
        payload in proptest::collection::vec(any::<u8>(), 1..32),
    ) {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 1);
        drive_with(&mut p, &mut g,&osc_52_set(b"p", &payload));
        drop(g.take_pending_clipboard_set());

        drive_with(&mut p, &mut g,&osc_52_query(b"c"));
        let response = common::responses(&mut g).pop().unwrap();
        let (sel, decoded) = parse_query_response(&response);
        prop_assert_eq!(sel, b"c".to_vec());
        prop_assert!(decoded.is_empty(),
            "clipboard cache must not see a primary-only write");
    }

    #[test]
    fn multi_selector_query_prefers_clipboard(
        c_payload in proptest::collection::vec(any::<u8>(), 1..32),
        p_payload in proptest::collection::vec(any::<u8>(), 1..32),
    ) {
        let mut p = Parser::new();
        let mut g = Grid::new(1, 1);
        drive_with(&mut p, &mut g,&osc_52_set(b"c", &c_payload));
        drop(g.take_pending_clipboard_set());
        drive_with(&mut p, &mut g,&osc_52_set(b"p", &p_payload));
        drop(g.take_pending_clipboard_set());

        drive_with(&mut p, &mut g,&osc_52_query(b"cp"));
        let response = common::responses(&mut g).pop().unwrap();
        let (sel, decoded) = parse_query_response(&response);
        prop_assert_eq!(sel, b"cp".to_vec());
        prop_assert_eq!(decoded, c_payload);
    }
}
