//! Property: OSC 4 / 5 / 104 / 105 is a coherent palette state machine
//! and OSC 10 / 11 / 12 / 110 / 111 / 112 a coherent theme-override one
//! (REQ-203; `docs/reference/protocols/vt-compliance.md` "OSC").
//! Validates against `XParseColor(3)` color-spec grammar.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::{Grid, default_dynamic_color, default_palette_color, default_special_color};
use felis_protocol::messages::ThemeChannel;
use felis_vt::Parser;
use proptest::prelude::*;

mod common;

fn responses_for(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut g = Grid::new(4, 16);
    let mut p = Parser::new();
    p.advance(&mut g, bytes);
    common::responses(&mut g)
}

/// The three syntaxes `XParseColor` accepts that felis's producers
/// actually emit; the four-digit form carries a low byte the terminal
/// must drop rather than mix in.
fn color_spec(rgb: (u8, u8, u8), low: (u8, u8, u8), syntax: u8) -> String {
    let ((r, g, b), (rl, gl, bl)) = (rgb, low);
    match syntax {
        0 => format!("#{r:02x}{g:02x}{b:02x}"),
        1 => format!("rgb:{r:02x}/{g:02x}/{b:02x}"),
        _ => format!("rgb:{r:02x}{rl:02x}/{g:02x}{gl:02x}/{b:02x}{bl:02x}"),
    }
}

const fn dynamic_channel(code: u16) -> ThemeChannel {
    match code {
        10 => ThemeChannel::Foreground,
        11 => ThemeChannel::Background,
        _ => ThemeChannel::Cursor,
    }
}

fn expected_dynamic_response(code: u16, rgb: (u8, u8, u8)) -> Vec<u8> {
    let (r, g, b) = rgb;
    format!("\x1b]{code};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\").into_bytes()
}

fn expected_response(code: &str, idx: u16, rgb: (u8, u8, u8)) -> Vec<u8> {
    let (r, g, b) = rgb;
    format!("\x1b]{code};{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\").into_bytes()
}

proptest! {
    #[test]
    fn osc_4_set_then_query_echoes_back_the_set_value(
        idx in 0u8..=255,
        r in 0u8..=255,
        g in 0u8..=255,
        b in 0u8..=255,
    ) {
        let cmd = format!(
            "\x1b]4;{idx};#{r:02x}{g:02x}{b:02x}\x1b\\\x1b]4;{idx};?\x1b\\"
        );
        let responses = responses_for(cmd.as_bytes());
        prop_assert_eq!(responses.len(), 1);
        prop_assert_eq!(&responses[0], &expected_response("4", u16::from(idx), (r, g, b)));
    }

    #[test]
    fn osc_104_reset_restores_default_for_a_single_index(
        idx in 0u8..=255,
        r in 0u8..=255,
        g in 0u8..=255,
        b in 0u8..=255,
    ) {
        let cmd = format!(
            "\x1b]4;{idx};?\x1b\\\
             \x1b]4;{idx};#{r:02x}{g:02x}{b:02x}\x1b\\\
             \x1b]104;{idx}\x1b\\\
             \x1b]4;{idx};?\x1b\\"
        );
        let responses = responses_for(cmd.as_bytes());
        prop_assert_eq!(responses.len(), 2);
        prop_assert_eq!(&responses[0], &responses[1]);
        prop_assert_eq!(
            &responses[0],
            &expected_response("4", u16::from(idx), default_palette_color(idx)),
        );
    }

    #[test]
    fn osc_104_with_no_index_resets_every_slot(
        idx_a in 0u8..=255,
        idx_b in 0u8..=255,
    ) {
        prop_assume!(idx_a != idx_b);
        let cmd = format!(
            "\x1b]4;{idx_a};#aabbcc\x1b\\\
             \x1b]4;{idx_b};#ddeeff\x1b\\\
             \x1b]104\x1b\\\
             \x1b]4;{idx_a};?\x1b\\\
             \x1b]4;{idx_b};?\x1b\\"
        );
        let responses = responses_for(cmd.as_bytes());
        prop_assert_eq!(responses.len(), 2);
        prop_assert_eq!(
            &responses[0],
            &expected_response("4", u16::from(idx_a), default_palette_color(idx_a)),
        );
        prop_assert_eq!(
            &responses[1],
            &expected_response("4", u16::from(idx_b), default_palette_color(idx_b)),
        );
    }

    #[test]
    fn osc_4_multi_pair_query_returns_replies_in_order(
        idx_a in 0u8..=255,
        idx_b in 0u8..=255,
        ra in 0u8..=255, ga in 0u8..=255, ba in 0u8..=255,
        rb in 0u8..=255, gb in 0u8..=255, bb in 0u8..=255,
    ) {
        prop_assume!(idx_a != idx_b);
        let cmd = format!(
            "\x1b]4;{idx_a};#{ra:02x}{ga:02x}{ba:02x};{idx_b};#{rb:02x}{gb:02x}{bb:02x}\x1b\\\
             \x1b]4;{idx_a};?;{idx_b};?\x1b\\"
        );
        let responses = responses_for(cmd.as_bytes());
        prop_assert_eq!(responses.len(), 2);
        prop_assert_eq!(&responses[0], &expected_response("4", u16::from(idx_a), (ra, ga, ba)));
        prop_assert_eq!(&responses[1], &expected_response("4", u16::from(idx_b), (rb, gb, bb)));
    }

    #[test]
    fn osc_5_set_then_reset_returns_default_special_color(
        idx in 0u8..5,
        r in 0u8..=255,
        g in 0u8..=255,
        b in 0u8..=255,
    ) {
        let cmd = format!(
            "\x1b]5;{idx};#{r:02x}{g:02x}{b:02x}\x1b\\\
             \x1b]5;{idx};?\x1b\\\
             \x1b]105;{idx}\x1b\\\
             \x1b]5;{idx};?\x1b\\"
        );
        let responses = responses_for(cmd.as_bytes());
        prop_assert_eq!(responses.len(), 2);
        prop_assert_eq!(&responses[0], &expected_response("5", u16::from(idx), (r, g, b)));
        prop_assert_eq!(
            &responses[1],
            &expected_response("5", u16::from(idx), default_special_color()),
        );
    }
}

proptest! {
    /// Set, read back through both the query reply and the daemon-facing
    /// override, then reset: the channel returns to the xterm baseline
    /// and reports the change exactly once.
    #[test]
    fn dynamic_color_set_query_and_reset_round_trip(
        code in prop_oneof![Just(10u16), Just(11), Just(12)],
        syntax in 0u8..3,
        rgb in (0u8..=255, 0u8..=255, 0u8..=255),
        low in (0u8..=255, 0u8..=255, 0u8..=255),
    ) {
        let channel = dynamic_channel(code);
        let spec = color_spec(rgb, low, syntax);
        let mut g = Grid::new(4, 16);
        let mut p = Parser::new();

        p.advance(&mut g, format!("\x1b]{code};{spec}\x1b\\\x1b]{code};?\x1b\\").as_bytes());
        prop_assert_eq!(g.theme_override(channel), Some(rgb));
        prop_assert_eq!(g.take_theme_dirty(channel), Some(Some(rgb)));
        prop_assert_eq!(g.take_theme_dirty(channel), None);
        prop_assert_eq!(common::responses(&mut g), vec![expected_dynamic_response(code, rgb)]);

        p.advance(&mut g, format!("\x1b]1{code};\x1b\\\x1b]{code};?\x1b\\").as_bytes());
        prop_assert_eq!(g.theme_override(channel), None);
        prop_assert_eq!(g.take_theme_dirty(channel), Some(None));
        prop_assert_eq!(
            common::responses(&mut g),
            vec![expected_dynamic_response(code, default_dynamic_color(channel))],
        );
    }

    /// xterm's multi-spec form: each further spec advances to the next
    /// channel, and a spec past the last one is dropped rather than
    /// wrapping onto the foreground.
    #[test]
    fn dynamic_color_specs_advance_through_the_channels(
        fg in (0u8..=255, 0u8..=255, 0u8..=255),
        bg in (0u8..=255, 0u8..=255, 0u8..=255),
        cursor in (0u8..=255, 0u8..=255, 0u8..=255),
        extra in (0u8..=255, 0u8..=255, 0u8..=255),
    ) {
        let zero = (0, 0, 0);
        let cmd = format!(
            "\x1b]10;{};{};{};{}\x1b\\",
            color_spec(fg, zero, 0),
            color_spec(bg, zero, 0),
            color_spec(cursor, zero, 0),
            color_spec(extra, zero, 0),
        );
        let mut g = Grid::new(4, 16);
        Parser::new().advance(&mut g, cmd.as_bytes());
        prop_assert_eq!(g.theme_override(ThemeChannel::Foreground), Some(fg));
        prop_assert_eq!(g.theme_override(ThemeChannel::Background), Some(bg));
        prop_assert_eq!(g.theme_override(ThemeChannel::Cursor), Some(cursor));
    }
}
