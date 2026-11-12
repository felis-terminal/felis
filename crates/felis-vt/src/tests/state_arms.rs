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
    assert_eq!(
        parse(b"\x1b\x7fB"),
        vec![Event::Esc {
            intermediates: vec![],
            final_byte: b'B',
        }]
    );
}

/// Entering Escape clears intermediates from an aborted sequence.
#[test]
fn escape_entry_clears_stale_intermediates() {
    assert_eq!(
        parse(b"\x1b(\x1bB"),
        vec![Event::Esc {
            intermediates: vec![],
            final_byte: b'B',
        }]
    );
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

/// `CsiEntry`: a C0 control executes; DEL is ignored.
#[test]
fn csi_entry_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[\x7fH"),
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

/// `CsiParam`: a C0 control executes; DEL is ignored.
#[test]
fn csi_param_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[5\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[5\x7fH"),
        vec![Event::Csi {
            params: vec![5],
            subparams: 0,
            intermediates: vec![],
            ignore: false,
            final_byte: b'H',
        }]
    );
}

/// `CsiIntermediate`: a C0 control executes; DEL is ignored.
#[test]
fn csi_intermediate_c0_executes_and_del_is_ignored() {
    assert_eq!(parse(b"\x1b[\x20\x07"), vec![Event::Execute(0x07)]);
    assert_eq!(
        parse(b"\x1b[\x20\x7fH"),
        vec![Event::Csi {
            params: vec![],
            subparams: 0,
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'H',
        }]
    );
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
    let hook = |params: Vec<u16>, intermediates: Vec<u8>| {
        vec![Event::DcsHook {
            params,
            intermediates,
            ignore: false,
            final_byte: b'q',
        }]
    };
    assert_eq!(parse(b"\x1bPq"), hook(vec![], vec![]));
    assert_eq!(parse(b"\x1bP\x07q"), hook(vec![], vec![]));
    assert_eq!(parse(b"\x1bP q"), hook(vec![], vec![0x20]));
    assert_eq!(parse(b"\x1bP:q"), hook(vec![0], vec![]));
    assert_eq!(parse(b"\x1bP<q"), hook(vec![], vec![0x3C]));
}

/// `DcsParam`: C0/DEL is ignored; an intermediate advances to
/// `DcsIntermediate`. Both still hook.
#[test]
fn dcs_param_c0_ignored_and_intermediate_advances() {
    assert_eq!(
        parse(b"\x1bP5\x07q"),
        vec![Event::DcsHook {
            params: vec![5],
            intermediates: vec![],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP5 q"),
        vec![Event::DcsHook {
            params: vec![5],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
}

/// `DcsIntermediate`: the final byte hooks; C0/DEL is ignored; a further
/// intermediate accumulates.
#[test]
fn dcs_intermediate_arms_hook_and_accumulate() {
    assert_eq!(
        parse(b"\x1bP q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP \x07q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
    assert_eq!(
        parse(b"\x1bP\x20\x20q"),
        vec![Event::DcsHook {
            params: vec![],
            intermediates: vec![0x20, 0x20],
            ignore: false,
            final_byte: b'q',
        }]
    );
}

/// `DcsPassthrough` ignores DEL (no `dcs_put`) while putting every other byte.
#[test]
fn dcs_passthrough_puts_data_but_ignores_del() {
    assert_eq!(
        parse(b"\x1bPqAB\x7fC"),
        vec![
            Event::DcsHook {
                params: vec![],
                intermediates: vec![],
                ignore: false,
                final_byte: b'q',
            },
            Event::DcsPut(b'A'),
            Event::DcsPut(b'B'),
            Event::DcsPut(b'C'),
        ]
    );
}
