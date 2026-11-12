use super::*;

#[test]
fn osc9_notification_relays_message_as_body() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;Build finished\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title, None);
    assert_eq!(got[0].body, "Build finished");
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
}

#[test]
fn conemu_osc9_4_progress_is_not_a_notification() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;4;1;50\x07\x1b]9;4;9\x07");
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
}

#[test]
fn bare_osc9_4_is_the_notification_4() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;4\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "4");
    assert_eq!(g.take_pty_effects(), []);
}

#[test]
fn other_conemu_subcommands_are_consumed_without_effect() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;1;500\x07\x1b]9;9;/tmp\x07");
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
    assert_eq!(g.take_pty_effects(), []);
}

/// The `ConEmu` filter fires only on the 3+-segment `9 ; <digit> ; …`
/// shape; a `params.len() >= 3 && …` guard flipped to `||` would
/// swallow this two-segment message.
#[test]
fn osc9_single_digit_message_is_relayed_not_filtered() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;5\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "5");
}

/// `ConEmu` chrome requires a single-digit first segment; the inner
/// `p.len() == 1 && digit` guard flipped to `||` would drop this.
#[test]
fn osc9_three_segments_with_non_digit_first_is_relayed() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]9;X;y\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body, "X;y");
}

#[test]
fn osc777_notify_relays_title_and_body() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]777;notify;Title;Body text\x07");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Title"));
    assert_eq!(got[0].body, "Body text");
}

#[test]
fn osc99_single_shot_relays_title() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;;Hello\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Hello"));
    assert_eq!(got[0].body, "");
}

#[test]
fn osc99_chunks_reassemble_across_advances() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    // Each chunk in a separate advance, sharing i=abc.
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=0:p=title;Part one\x1b\\");
    assert!(
        g.take_notifications().is_empty(),
        "an unfinished chunk must not relay"
    );
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=0:p=body;the body\x1b\\");
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
    drive(&mut p, &mut g, b"\x1b]99;i=abc:d=1:p=body; more\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("Part one"));
    assert_eq!(got[0].body, "the body more");
    assert_eq!(got[0].id.as_deref(), Some("abc"));
}

#[test]
fn osc99_chunk_at_the_specs_full_encoded_payload_size_reassembles() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    // "YWFh" is Base64 for "aaa", so 1024 groups is exactly the
    // notification spec's 4096-byte per-chunk encoded payload; the
    // `99;i=…:d=0:e=1:p=title;` prefix rides on top of it inside the
    // parser's OSC buffer.
    let encoded = "YWFh".repeat(1024);
    drive(
        &mut p,
        &mut g,
        format!("\x1b]99;i=big:d=0:e=1:p=title;{encoded}\x1b\\").as_bytes(),
    );
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
    drive(&mut p, &mut g, b"\x1b]99;i=big:d=1:e=1:p=title;\x1b\\");
    let got = g.take_notifications();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].title.as_deref(), Some("a".repeat(3072).as_str()));
}

#[test]
fn osc99_support_query_gets_a_truthful_reply() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;i=7:p=?;\x1b\\");
    // The reply goes on the response channel, not the notification
    // outbox.
    assert_eq!(
        g.take_notifications(),
        Vec::<notification::Notification>::new()
    );
    let replies = responses(&mut g);
    assert_eq!(replies.len(), 1);
    let reply = String::from_utf8(replies[0].clone()).unwrap();
    // Advertises title/body + the three urgencies; omits actions (a)
    // and close events (c).
    assert_eq!(reply, "\x1b]99;i=7:p=?;p=title,body:u=0,1,2\x1b\\");
}

proptest::proptest! {
    #[test]
    fn osc_7501_never_panics_on_arbitrary_payloads(
        payload in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..5000),
        st in proptest::prelude::any::<bool>(),
    ) {
        let mut bytes = b"\x1b]7501;".to_vec();
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(if st { b"\x1b\\" } else { b"\x07" });
        let mut p = Parser::new();
        let mut g = Grid::new(4, 20);
        drive(&mut p, &mut g, &bytes);
        drop(g.take_pty_effects());
    }

    /// Whatever the chunking, a parse that stops whenever the grid asks
    /// and drains at each stop delivers every report, in order.
    #[test]
    fn a_yielding_parse_delivers_every_report_in_order(
        ids in proptest::collection::vec("[a-z]{1,8}", 1..200),
        cuts in proptest::collection::vec(proptest::prelude::any::<proptest::sample::Index>(), 0..8),
    ) {
        let mut bytes = Vec::new();
        for id in &ids {
            bytes.extend_from_slice(format!("\x1b]7501;id={id}:state=done\x1b\\").as_bytes());
        }
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
        cuts.extend([0, bytes.len()]);
        cuts.sort_unstable();
        let mut p = Parser::new();
        let mut g = Grid::new(2, 8);
        let mut seen = Vec::new();
        for pair in cuts.windows(2) {
            let mut rest = &bytes[pair[0]..pair[1]];
            while let Some(n) = p.advance_until_yield(&mut g, rest) {
                let drained = g.take_pty_effects();
                proptest::prop_assert!(drained.len() <= STATUS_OUTBOX_YIELD);
                seen.extend(drained);
                rest = &rest[n..];
            }
        }
        seen.extend(g.take_pty_effects());
        let got: Vec<String> = seen
            .into_iter()
            .map(|e| match e {
                PtyEffect::ProgramStatus(ProgramStatusEffect::Report(r)) => r.id,
                other => panic!("unexpected effect {other:?}"),
            })
            .collect();
        proptest::prop_assert_eq!(got, ids);
    }
}

#[test]
fn osc99_support_query_with_an_id_outside_the_spec_alphabet_is_unanswered() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;i=\nid\n:p=?;\x07");
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
}

#[test]
fn osc99_support_query_echoes_an_id_using_every_spec_punctuation() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 10);
    drive(&mut p, &mut g, b"\x1b]99;i=aZ9_+.-:p=?;\x1b\\");
    assert_eq!(
        responses(&mut g),
        vec![b"\x1b]99;i=aZ9_+.-:p=?;p=title,body:u=0,1,2\x1b\\".to_vec()]
    );
}
