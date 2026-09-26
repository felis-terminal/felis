//! DECRQSS round-trip property tests: vttest expects the reply's
//! parameter string to restore the state as-is when replayed into a CSI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_grid::Grid;
use felis_vt::Parser;
use proptest::prelude::*;

mod common;

fn round_trip(setup: &[u8], query: &[u8]) -> (Grid, Grid) {
    let mut first = Grid::new(24, 80);
    let mut parser = Parser::new();
    parser.advance(&mut first, setup);
    drop(first.take_pty_effects());
    let mut rqss = Vec::with_capacity(8 + query.len());
    rqss.extend_from_slice(b"\x1bP$q");
    rqss.extend_from_slice(query);
    rqss.extend_from_slice(b"\x1b\\");
    parser.advance(&mut first, &rqss);
    let replies = common::responses(&mut first);
    let reply = replies.into_iter().next().expect("decrqss reply");
    let body = strip_dcs_envelope(&reply);
    let payload = match body {
        Some((1, payload)) => payload,
        Some((_, _)) => panic!("DECRQSS returned invalid for query {query:?}"),
        None => panic!("malformed DECRQSS reply: {reply:?}"),
    };
    let mut second = Grid::new(24, 80);
    let mut parser2 = Parser::new();
    let mut csi = Vec::with_capacity(2 + payload.len());
    csi.push(0x1b);
    csi.push(b'[');
    csi.extend_from_slice(&payload);
    parser2.advance(&mut second, &csi);
    (first, second)
}

fn strip_dcs_envelope(bytes: &[u8]) -> Option<(u8, Vec<u8>)> {
    let bytes = bytes.strip_prefix(b"\x1bP")?;
    let bytes = bytes.strip_suffix(b"\x1b\\")?;
    let ps = match bytes.first()? {
        b'0' => 0u8,
        b'1' => 1u8,
        _ => return None,
    };
    let rest = bytes.get(1..)?;
    let payload = rest.strip_prefix(b"$r")?;
    Some((ps, payload.to_vec()))
}

proptest! {
    #[test]
    fn sgr_style_flags_round_trip(
        flags in proptest::collection::vec(prop_oneof![
            Just(1u8), Just(2u8), Just(3u8), Just(4u8), Just(5u8),
            Just(7u8), Just(8u8), Just(9u8), Just(53u8),
        ], 0..8),
    ) {
        let mut setup: Vec<u8> = Vec::new();
        if !flags.is_empty() {
            setup.extend_from_slice(b"\x1b[");
            for (i, f) in flags.iter().enumerate() {
                if i > 0 { setup.push(b';'); }
                setup.extend_from_slice(f.to_string().as_bytes());
            }
            setup.push(b'm');
        }
        let (first, second) = round_trip(&setup, b"m");
        prop_assert_eq!(first.pen(), second.pen());
    }

    #[test]
    fn sgr_color_round_trip(
        fg_idx in prop_oneof![
            Just(None),
            (0u8..=15).prop_map(Some),
        ],
        bg_rgb in prop_oneof![
            Just(None),
            (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(Some),
        ],
        ul_color in prop_oneof![
            Just(None),
            (0u8..=255).prop_map(|n| Some(format!("58:5:{n}"))),
            (any::<u8>(), any::<u8>(), any::<u8>())
                .prop_map(|(r, g, b)| Some(format!("58:2::{r}:{g}:{b}"))),
        ],
        bold in any::<bool>(),
    ) {
        let mut setup: Vec<u8> = Vec::from(&b"\x1b["[..]);
        let mut params: Vec<String> = Vec::new();
        if bold { params.push("1".into()); }
        if let Some(n) = fg_idx {
            params.push(if n < 8 { format!("{}", 30 + n) } else { format!("{}", 90 + (n - 8)) });
        }
        if let Some((r, g, b)) = bg_rgb {
            params.push(format!("48;2;{r};{g};{b}"));
        }
        if let Some(ul) = ul_color {
            params.push(ul);
        }
        if params.is_empty() {
            params.push(String::new());
        }
        setup.extend_from_slice(params.join(";").as_bytes());
        setup.push(b'm');
        let (first, second) = round_trip(&setup, b"m");
        prop_assert_eq!(first.pen(), second.pen());
    }

    #[test]
    fn decstbm_round_trip(
        top in 1u16..=20,
        bottom in 4u16..=24,
    ) {
        prop_assume!(top < bottom && bottom <= 24);
        let setup = format!("\x1b[{top};{bottom}r");
        let (first, second) = round_trip(setup.as_bytes(), b"r");
        // The grid exposes no scroll-region getter; home the cursor and compare.
        let mut a = first;
        let mut b = second;
        let mut p1 = Parser::new();
        let mut p2 = Parser::new();
        p1.advance(&mut a, b"\x1b[H");
        p2.advance(&mut b, b"\x1b[H");
        prop_assert_eq!(a.cursor(), b.cursor());
    }

    #[test]
    fn decsca_round_trip(
        protect in any::<bool>(),
    ) {
        let setup = if protect { b"\x1b[1\"q".to_vec() } else { b"\x1b[0\"q".to_vec() };
        let (first, second) = round_trip(&setup, b"\"q");
        let mut a = first;
        let mut b = second;
        let mut p1 = Parser::new();
        let mut p2 = Parser::new();
        p1.advance(&mut a, b"X");
        p2.advance(&mut b, b"X");
        let a_style = a.cell(0, 0).unwrap().style;
        let b_style = b.cell(0, 0).unwrap().style;
        let a_flags = a.style(a_style).flags;
        let b_flags = b.style(b_style).flags;
        prop_assert_eq!(
            a_flags.contains(felis_grid::AttrFlags::PROTECTED),
            b_flags.contains(felis_grid::AttrFlags::PROTECTED)
        );
    }
}
