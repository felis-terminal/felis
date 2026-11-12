use super::*;

// Each test feeds a sequence into one parser state, then a follow-up byte
// whose dispatch differs per state.

/// A C0 control inside Escape executes in place and does not abort.
#[test]
fn escape_executes_c0_control_in_place() {
    assert_eq!(parse(b"\x1b\x07"), vec![Event::Execute(0x07)]);
}

/// DEL in Escape is ignored, not an abort.
#[test]
fn escape_ignores_del_then_dispatches() {
    assert_eq!(parse(b"\x1b\x7fB"), vec![esc(b'B')]);
}

/// Entering Escape clears intermediates from an aborted sequence.
#[test]
fn escape_entry_clears_stale_intermediates() {
    assert_eq!(parse(b"\x1b(\x1bB"), vec![esc(b'B')]);
}

/// `EscapeIntermediate`: a C0 control executes in place, a further
/// intermediate accumulates, DEL is ignored; none abort.
#[test]
fn escape_intermediate_arms_keep_the_sequence_alive() {
    assert_eq!(parse(b"\x1b \x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b\x20\x20B"),
        vec![Event::Esc {
            intermediates: vec![0x20, 0x20],
            final_byte: b'B',
        }]
    );
    assert_eq!(
        parse(b"\x1b\x20\x7fB"),
        vec![Event::Esc {
            intermediates: vec![0x20],
            final_byte: b'B',
        }]
    );
}

#[test]
fn csi_states_execute_c0_and_ignore_del() {
    let cases: [(&str, &[u8], Event); 3] = [
        ("CsiEntry", b"\x1b[", csi(&[], &[], b'H')),
        ("CsiParam", b"\x1b[5", csi(&[5], &[], b'H')),
        ("CsiIntermediate", b"\x1b[\x20", csi(&[], b" ", b'H')),
    ];
    for (state, prefix, dispatched) in cases {
        assert_eq!(
            parse(&[prefix, b"\x07"].concat()),
            vec![Event::Execute(0x07)],
            "{state}"
        );
        assert_eq!(
            parse(&[prefix, b"\x7fH"].concat()),
            vec![dispatched],
            "{state}"
        );
    }
}

/// `CsiIgnore`: a C0 control still executes, the final byte returns to
/// Ground, and no CSI is dispatched.
#[test]
fn csi_ignore_executes_c0_and_returns_to_ground_on_final() {
    assert_eq!(
        parse(b"\x1b[5<\x07mX"),
        vec![Event::Execute(0x07), Event::Print(b'X')]
    );
}

/// `DcsEntry`: the final byte hooks; C0/DEL is ignored; an intermediate,
/// a sub-param `:`, and a private `<` each advance rather than dropping
/// to `DcsIgnore`.
#[test]
fn dcs_entry_arms_route_to_passthrough_not_ignore() {
    assert_eq!(parse(b"\x1bPq"), vec![dcs_hook(&[], &[], b'q')]);
    assert_eq!(parse(b"\x1bP\x07q"), vec![dcs_hook(&[], &[], b'q')]);
    assert_eq!(parse(b"\x1bP q"), vec![dcs_hook(&[], &[0x20], b'q')]);
    assert_eq!(parse(b"\x1bP:q"), vec![dcs_hook(&[0], &[], b'q')]);
    assert_eq!(parse(b"\x1bP<q"), vec![dcs_hook(&[], &[0x3C], b'q')]);
}

/// `DcsParam`: C0/DEL is ignored; an intermediate advances to
/// `DcsIntermediate`. Both still hook.
#[test]
fn dcs_param_c0_ignored_and_intermediate_advances() {
    assert_eq!(parse(b"\x1bP5\x07q"), vec![dcs_hook(&[5], &[], b'q')]);
    assert_eq!(parse(b"\x1bP5 q"), vec![dcs_hook(&[5], &[0x20], b'q')]);
}

/// `DcsIntermediate`: the final byte hooks; C0/DEL is ignored; a further
/// intermediate accumulates.
#[test]
fn dcs_intermediate_arms_hook_and_accumulate() {
    assert_eq!(parse(b"\x1bP q"), vec![dcs_hook(&[], &[0x20], b'q')]);
    assert_eq!(parse(b"\x1bP \x07q"), vec![dcs_hook(&[], &[0x20], b'q')]);
    assert_eq!(
        parse(b"\x1bP\x20\x20q"),
        vec![dcs_hook(&[], &[0x20, 0x20], b'q')]
    );
}

/// `DcsPassthrough` ignores DEL (no `dcs_put`) while putting every other byte.
#[test]
fn dcs_passthrough_puts_data_but_ignores_del() {
    assert_eq!(
        parse(b"\x1bPqAB\x7fC"),
        vec![
            dcs_hook(&[], &[], b'q'),
            Event::DcsPut(b'A'),
            Event::DcsPut(b'B'),
            Event::DcsPut(b'C'),
        ]
    );
}
