use super::*;

#[test]
fn foreign_apc_dialect_stays_silent_without_disturbing_reassembler() {
    // Pins: a non-`G` APC is another dialect, so no reply (a reply would
    // inject kitty bytes into a protocol we do not speak).
    let mut r = Reassembler::new();
    assert_eq!(
        reassemble_apc_body(&mut r, b"not-a-kitty-graphics-body"),
        BodyDispatch::Silent
    );
    let complete = assembled(reassemble_apc_body(&mut r, &body("Ga=q", b"")))
        .expect("valid command after a foreign body must assemble");
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'a' && v == b"q")
    );
}

#[test]
fn malformed_g_envelope_is_dropped_without_a_reply() {
    let mut r = Reassembler::new();
    assert_eq!(
        reassemble_apc_body(&mut r, b"Gbadcontrols"),
        BodyDispatch::Silent
    );
    assert!(assembled(reassemble_apc_body(&mut r, &body("Ga=q", b""))).is_some());
}

#[test]
fn a_first_chunk_with_both_image_id_and_number_is_refused_at_once() {
    let mut r = Reassembler::new();
    let (mut grid, mut images, mut placements) = handler_state();
    let Some(complete) = assembled(reassemble_apc_body(&mut r, b"Ga=T,i=1,I=2,m=1;zz")) else {
        panic!("expected the command to complete");
    };
    let reply = run(&mut grid, &mut images, &mut placements, complete);
    assert_eq!(
        reply,
        Some(
            b"\x1b_Gi=1,I=2;EINVAL:must not specify both image id and image number\x1b\\".to_vec()
        )
    );
}

#[test]
fn reassembly_overflow_replies_einval_with_the_head_ids() {
    // Pins: the overflow error echoes the head chunk's `i=` (the
    // continuation carries no controls of its own).
    let mut r = Reassembler::new();
    let near_cap = vec![b'x'; kitty_graphics::REASSEMBLY_BUFFER_LIMIT - 4];
    let mut head = b"Gi=9,a=t,m=1;".to_vec();
    head.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head), BodyDispatch::Silent);
    match reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])) {
        BodyDispatch::Reply(reply) => {
            let text = String::from_utf8_lossy(&reply).into_owned();
            assert!(text.starts_with("\x1b_Gi=9;EINVAL:"), "reply was {text:?}");
            assert!(
                text.contains("reassembly buffer limit"),
                "reply was {text:?}"
            );
        }
        other => panic!("expected an EINVAL reply, got {other:?}"),
    }
    assert!(assembled(reassemble_apc_body(&mut r, &body("Ga=q", b""))).is_some());
}

#[test]
fn reassembly_overflow_reply_respects_q_gating() {
    let near_cap = vec![b'x'; kitty_graphics::REASSEMBLY_BUFFER_LIMIT - 4];

    let mut r = Reassembler::new();
    let mut head_q2 = b"Gi=9,a=t,q=2,m=1;".to_vec();
    head_q2.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head_q2), BodyDispatch::Silent);
    assert_eq!(
        reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])),
        BodyDispatch::Silent,
        "q=2 must suppress the overflow reply",
    );

    let mut r = Reassembler::new();
    let mut head_q1 = b"Gi=9,a=t,q=1,m=1;".to_vec();
    head_q1.extend_from_slice(&near_cap);
    assert_eq!(reassemble_apc_body(&mut r, &head_q1), BodyDispatch::Silent);
    assert!(
        matches!(
            reassemble_apc_body(&mut r, &body("Gm=0", &[b'y'; 8])),
            BodyDispatch::Reply(_)
        ),
        "q=1 suppresses OK only; the overflow error must still reply",
    );
}

#[test]
fn end_to_end_parser_feeds_apc_through_grid_relay_into_dispatcher() {
    let mut p = Parser::new();
    let mut g = felis_grid::Grid::new(1, 4);
    p.advance(&mut g, b"\x1b_Gi=42,a=t,f=24;cmdiLXBheWxvYWQ=\x1b\\");
    let bodies: Vec<_> = g
        .take_pty_effects()
        .into_iter()
        .filter_map(|e| match e {
            felis_grid::PtyEffect::Apc(body) => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 1, "one APC body reached the grid");
    let mut r = Reassembler::new();
    let complete = assembled(reassemble_apc_body(&mut r, &bodies[0].body))
        .expect("daemon assembles the relayed body");
    assert_eq!(complete.payload.as_deref(), Some(&b"rgb-payload"[..]));
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'i' && v == b"42")
    );
    assert!(
        complete
            .controls
            .iter()
            .any(|(k, v)| *k == b'f' && v == b"24")
    );
}
