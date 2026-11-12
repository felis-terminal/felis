use super::*;

#[test]
fn apc_dispatch_buffers_kitty_graphics_body_for_daemon_drain() {
    // docs/explanation/protocols/kitty-graphics.md "Dispatcher
    // architecture": the grid is a relay; the daemon runs the Kitty
    // graphics dispatcher off `take_pty_effects`.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b_Gi=1,a=q;\x1b\\");
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1, "one APC body");
    assert_eq!(drained[0].body, b"Gi=1,a=q;");
    assert!(
        apc_bodies(&mut g).is_empty(),
        "second drain returns no bodies (queue was emptied)",
    );
}

#[test]
fn apc_dispatch_preserves_burst_order_across_multiple_apcs() {
    // Chunked transmission (`m=1`): the daemon's `Reassembler::feed`
    // depends on chunks arriving in the order the producer emitted them.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(
        &mut p,
        &mut g,
        b"\x1b_Ga=t,m=1;chunk1\x1b\\\x1b_Gm=1;chunk2\x1b\\\x1b_Gm=0;chunk3\x1b\\",
    );
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].body, b"Ga=t,m=1;chunk1");
    assert_eq!(drained[1].body, b"Gm=1;chunk2");
    assert_eq!(drained[2].body, b"Gm=0;chunk3");
}

#[test]
fn apc_body_captures_the_cursor_after_a_mid_burst_move() {
    // yazi wraps each preview in DECSC / CUP / image / DECRC in one
    // write, and the daemon drains the outbox only after the whole
    // `Parser::advance`, so each body must carry the cursor as it stood
    // at APC time.
    let mut p = Parser::new();
    let mut g = Grid::new(24, 80);
    drive(
        &mut p,
        &mut g,
        b"\x1b7\x1b[4;3H\x1b_Ga=T,f=24,s=1,v=1;AAA\x1b\\\x1b8",
    );
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1);
    // The CUP target, captured before DECRC.
    assert_eq!(
        (drained[0].cursor_row, drained[0].cursor_col),
        (3, 2),
        "body must carry the cursor at APC time, not after DECRC",
    );
    assert_eq!((g.cursor().row, g.cursor().col), (0, 0));
}

#[test]
fn apc_outbox_drops_past_cap_and_bells_so_producer_sees_loss() {
    // A producer flooding APCs without a chance to drain: the same
    // drain cycle reports both the surviving bodies and the loss
    // signal.
    const OVERFLOW: usize = 6;
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    let mut bytes = Vec::new();
    for i in 0..APC_OUTBOX_CAP + OVERFLOW {
        let body = format!("Gi={i};{i}");
        bytes.extend_from_slice(b"\x1b_");
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(b"\x1b\\");
    }
    drive(&mut p, &mut g, &bytes);
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), APC_OUTBOX_CAP);
    assert_eq!(drained[0].body, b"Gi=0;0");
    let last = APC_OUTBOX_CAP - 1;
    assert_eq!(drained[last].body, format!("Gi={last};{last}").into_bytes());
    assert!(
        g.take_bell_pending(),
        "drop past cap must signal the producer via BEL",
    );
}

#[test]
fn apc_overflow_signals_bell_so_truncation_is_audible() {
    // The parser calls `apc_overflow` before the truncated body reaches
    // `apc_dispatch`; one loss event per truncated dispatch.
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    let mut bytes = Vec::from(*b"\x1b_G");
    // One KiB past the buffer cap, so truncation fires regardless of
    // the cap's exact value.
    bytes.extend(std::iter::repeat_n(b'x', felis_vt::APC_BUFFER_LIMIT + 1024));
    bytes.extend_from_slice(b"\x1b\\");
    drive(&mut p, &mut g, &bytes);
    assert!(g.take_bell_pending(), "apc_overflow must BEL");
    let drained = apc_bodies(&mut g);
    assert_eq!(drained.len(), 1, "truncated body still delivered");
    assert!(
        drained[0].body.len() <= felis_vt::APC_BUFFER_LIMIT,
        "body capped at parser's APC_BUFFER_LIMIT, got {}",
        drained[0].body.len(),
    );
}

/// Feeds each body as one APC through `advance_until_yield` and reports
/// whether the parse stopped on it.
fn yields(g: &mut Grid, bodies: &[&str]) -> Vec<bool> {
    let mut p = Parser::new();
    bodies
        .iter()
        .map(|body| {
            let bytes = format!("\x1b_{body}\x1b\\");
            p.advance_until_yield(g, bytes.as_bytes()).is_some()
        })
        .collect()
}

#[test]
fn parse_yields_after_a_cursor_moving_placement_or_an_offloaded_decode_only() {
    let cases: &[(&[&str], &[bool])] = &[
        (&["Ga=T,f=24,s=1,v=1;AAAA"], &[true]),
        (&["Ga=p,i=1"], &[true]),
        (&["Ga=t,f=24,s=1,v=1;AAAA"], &[false]),
        (&["Ga=q,f=24,s=1,v=1;AAAA"], &[false]),
        (&["Ga=T,C=1,f=24,s=1,v=1;AAAA"], &[false]),
        (&["Ga=T,U=1,f=24,s=1,v=1;AAAA"], &[false]),
        (&["Ga=T,C=01,f=24,s=1,v=1;AAAA"], &[false]),
        // A repeated key reads its last value, as the dispatcher does.
        (&["Ga=t,a=T,f=24,s=1,v=1;AAAA"], &[true]),
        (&["Ga=T,a=t,f=24,s=1,v=1;AAAA"], &[false]),
        (&["Ga=T,C=1,C=0,f=24,s=1,v=1;AAAA"], &[true]),
        (
            &["Ga=T,f=24,s=1,v=1,m=1;AA", "Gm=1;AA", "Gm=0;"],
            &[false, false, true],
        ),
        // `m=` means nothing to file-backed media: each chunk is whole.
        (&["Ga=T,t=f,m=1;L3RtcA=="], &[true]),
        (&["Ga=T,t=s,m=1;L3NobQ=="], &[true]),
        // A file-backed command abandons an open direct stream.
        (
            &["Ga=T,m=1;AA", "Ga=t,t=f;L3RtcA==", "Gm=0;AA"],
            &[false, true, false],
        ),
        (&["Ga=t,f=100;AA"], &[true]),
        (&["Ga=q,i=1,t=s;L3NobQ=="], &[true]),
        (&["Ga=T,C=1,f=32,s=256,v=256,o=z;AA"], &[true]),
        (&["Ga=T,C=1,f=32,s=256,v=256;AA"], &[false]),
        (&["Ga=p,i=1,C=1,f=100"], &[false]),
        (&["Ga=t,f=100,m=1;AA", "Gm=0;AA"], &[false, true]),
    ];
    for (bodies, want) in cases {
        let mut g = Grid::new(8, 16);
        assert_eq!(yields(&mut g, bodies), *want, "{bodies:?}");
    }
}

#[test]
fn only_the_body_completing_an_offloaded_decode_stops_the_parse() {
    let mut g = Grid::new(8, 16);
    yields(
        &mut g,
        &[
            "Ga=T,f=100,m=1;AA",
            "Gm=1;AA",
            "Gm=0;AA",
            "Ga=t,f=24,s=1,v=1;AAAA",
        ],
    );
    let flags: Vec<bool> = apc_bodies(&mut g)
        .iter()
        .map(|apc| apc.stops_parse)
        .collect();
    assert_eq!(flags, [false, false, true, false]);
}

#[test]
fn parse_does_not_yield_on_a_placement_the_outbox_dropped() {
    let mut g = Grid::new(8, 16);
    let fill: Vec<&str> = std::iter::repeat_n("Ga=t,i=1;AAAA", APC_OUTBOX_CAP).collect();
    yields(&mut g, &fill);
    assert_eq!(yields(&mut g, &["Ga=p,i=1"]), [false]);
    assert!(g.take_bell_pending(), "the drop rings the bell");
    drop(g.take_pty_effects());
    assert_eq!(yields(&mut g, &["Ga=p,i=1"]), [true]);
}

fn apc_body() -> impl Strategy<Value = Vec<u8>> {
    const PREFIX: &str = "Ga=t,i=1,m=1;";
    let largest = felis_vt::APC_BUFFER_LIMIT - PREFIX.len();
    let payload = prop_oneof![
        0..64usize,
        (128 * 1024 - 64usize)..=(128 * 1024),
        (largest - 2)..=largest,
        0usize..=largest,
    ];
    prop_oneof![
        payload.prop_map(|n| format!("{PREFIX}{}", "A".repeat(n)).into_bytes()),
        Just(b"Ga=p,i=1,C=1".to_vec()),
        Just(b"Ga=p,i=1".to_vec()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A parse that yields whenever the grid asks, read in arbitrary
    /// chunks, hands the daemon every APC body whole and in order: a
    /// flood of per-cell placements (yazi) and bodies as large as
    /// kitty's own client sends (128 KiB chunks) alike.
    #[test]
    fn a_yielding_parse_delivers_every_apc_body_intact(
        bodies in proptest::collection::vec(apc_body(), 0..80),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
    ) {
        let mut stream = Vec::new();
        for body in &bodies {
            stream.extend_from_slice(b"\x1b_");
            stream.extend_from_slice(body);
            stream.extend_from_slice(b"\x1b\\");
        }
        let mut cuts: Vec<usize> = cuts.iter().map(|i| i.index(stream.len() + 1)).collect();
        cuts.extend([0, stream.len()]);
        cuts.sort_unstable();
        let mut p = Parser::new();
        let mut g = Grid::new(8, 16);
        let mut delivered = Vec::new();
        for read in cuts.windows(2) {
            let mut rest = &stream[read[0]..read[1]];
            while let Some(consumed) = p.advance_until_yield(&mut g, rest) {
                delivered.extend(apc_bodies(&mut g).into_iter().map(|apc| apc.body));
                rest = &rest[consumed..];
            }
        }
        delivered.extend(apc_bodies(&mut g).into_iter().map(|apc| apc.body));
        prop_assert!(delivered == bodies, "{} of {} bodies delivered intact", delivered.len(), bodies.len());
        prop_assert!(!g.take_bell_pending(), "nothing was truncated or dropped");
    }
}

fn alt_scrolls(g: &mut Grid) -> Vec<u32> {
    g.take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            PtyEffect::AltScreenScrolled(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[test]
fn alt_screen_full_scroll_reports_the_rows_it_dropped() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[?1049h\x1b[4;1H\n\n");
    assert_eq!(alt_scrolls(&mut g), [1, 1]);
}

#[test]
fn alt_screen_region_scroll_and_primary_scroll_report_nothing() {
    let mut p = Parser::new();
    let mut g = Grid::new(4, 8);
    drive(&mut p, &mut g, b"\x1b[4;1H\n");
    assert!(alt_scrolls(&mut g).is_empty(), "primary");
    drive(&mut p, &mut g, b"\x1b[?1049h\x1b[2;3r\x1b[3;1H\n");
    assert!(alt_scrolls(&mut g).is_empty(), "partial region");
}

#[test]
fn ris_keeps_the_graphics_commands_queued_before_it() {
    // The tracker already counted the transmit; dropping it here would
    // leave the daemon's reassembler a step behind the grid's.
    let mut p = Parser::new();
    let mut g = Grid::new(8, 16);
    drive(&mut p, &mut g, b"\x1b_Ga=t,i=1;AAAA\x1b\\\x1bc");
    assert_eq!(apc_bodies(&mut g).len(), 1);
}
