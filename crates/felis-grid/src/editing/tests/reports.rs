use super::*;

/// `DSR ? 996 n` answers the last client-reported OS preference,
/// defaulting to light; DECSET 2031 records the opt-in.
#[test]
fn decset_2031_and_dsr_996_report_color_scheme() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2031h");
    assert!(g.color_scheme_notify());
    drive(&mut p, &mut g, b"\x1b[?996n");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1b[?997;2n");
    g.set_os_dark(true);
    drive(&mut p, &mut g, b"\x1b[?996n");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1b[?997;1n");
    drive(&mut p, &mut g, b"\x1b[?2031l");
    assert!(!g.color_scheme_notify());
}

/// `DECSET 2048` records the opt-in and queues no response of its own
/// (the daemon is the only emitter of the report); the report reads
/// the live geometry.
#[test]
fn decset_2048_records_the_mode_without_queueing_a_response() {
    let mut g = Grid::new(24, 80);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2048h");
    assert!(g.in_band_resize_notify());
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    assert_eq!(
        std::str::from_utf8(&g.resize_notify_report_bytes()).unwrap(),
        "\x1b[48;24;80;0;0t"
    );
    drive(&mut p, &mut g, b"\x1b[?2048l");
    assert!(!g.in_band_resize_notify());
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
}

/// DECRQM round-trips modes 2048 and 2031 as functional modes (Ps=1 set
/// / Ps=2 reset), not the unknown Ps=0.
#[test]
fn decrqm_reports_the_2048_and_2031_state() {
    for mode in [2048, 2031] {
        let mut g = Grid::new(4, 8);
        let mut p = Parser::new();
        for (toggle, ps) in [
            (String::new(), 2),
            (format!("\x1b[?{mode}h"), 1),
            (format!("\x1b[?{mode}l"), 2),
        ] {
            drive(&mut p, &mut g, format!("{toggle}\x1b[?{mode}$p").as_bytes());
            assert_eq!(
                reply_text(&mut g),
                [format!("\x1b[?{mode};{ps}$y")],
                "{toggle:?}"
            );
        }
    }
}

/// `CSI Pn t` with Pn >= 24 (DECSLPP) is consumed silently without
/// falling through to a window-op reply; codes below 24 keep working.
#[test]
fn csi_pn_t_ge_24_is_consumed_and_decslpp_is_unsupported() {
    let mut g = Grid::new(8, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1bP$qt\x1b\\");
    let r = responses(&mut g);
    assert_eq!(std::str::from_utf8(&r[0]).unwrap(), "\x1bP0$r\x1b\\");
    drive(&mut p, &mut g, b"\x1b[27t");
    assert_eq!(responses(&mut g), Vec::<Vec<u8>>::new());
    drive(&mut p, &mut g, b"\x1b[11t");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1b[1t");
}

/// DECRQM is swallowed at conformance level 2 (esctest's
/// `test_DECSCL_Level2DoesntSupportDECRQM`) and works again at level 4.
#[test]
fn decrqm_returns_no_reply_at_conformance_level_below_three() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[62;1\"p");
    drive(&mut p, &mut g, b"\x1b[4$p");
    let r = responses(&mut g);
    assert!(
        r.is_empty(),
        "DECRQM at level 2 must produce no reply (got {:?})",
        r.iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
    );
    drive(&mut p, &mut g, b"\x1b[64;1\"p\x1b[4$p");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1b[4;2$y");
}

/// DECRQCRA: bold subtracts one from the checksum.
#[test]
fn decrqcra_bold_attribute_subtracts_one_from_checksum() {
    let mut g = drive_new(4, 8, b"\x1b[1ma\x1b[3;1;1;1;1;1*y");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1bP3!~FF9E\x1b\\");
}

/// DECRQCRA: missing rectangle params default to the whole grid.
#[test]
fn decrqcra_missing_rect_params_default_to_full_grid() {
    let mut g = drive_new(4, 8, b"\x1b[4*y");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1bP4!~FC00\x1b\\");
}

/// DECRQSS `s` reports the current margins as 1-based inclusive.
#[test]
fn decrqss_s_reports_left_right_margins() {
    let mut g = drive_new(4, 16, b"\x1b[?69h\x1b[5;10s\x1bP$qs\x1b\\");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1bP1$r5;10s\x1b\\");
}

/// DECRQSS answers `*x` (DECSACE) and `"p` (DECSCL) with the last-set
/// value, and the status-line / page-length requests (`$}`, `$~`,
/// `*|`, `t`) with the invalid form `0$r`.
#[test]
fn decrqss_answers_supported_settings_and_rejects_the_rest() {
    let mut g = Grid::new(8, 16);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[0*x\x1bP$q*x\x1b\\");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1bP1$r0*x\x1b\\");

    drive(&mut p, &mut g, b"\x1b[65;1\"p\x1bP$q\"p\x1b\\");
    let r = reply_text(&mut g);
    assert_eq!(r[0], "\x1bP1$r64;1\"p\x1b\\");

    for (setter, query) in [
        (&b"\x1b[1$}"[..], &b"\x1bP$q$}\x1b\\"[..]),
        (&b"\x1b[2$~"[..], &b"\x1bP$q$~\x1b\\"[..]),
        (&b"\x1b[24*|"[..], &b"\x1bP$q*|\x1b\\"[..]),
        (&b"\x1b[27t"[..], &b"\x1bP$qt\x1b\\"[..]),
    ] {
        drive(&mut p, &mut g, setter);
        drive(&mut p, &mut g, query);
        let r = responses(&mut g);
        assert_eq!(
            std::str::from_utf8(&r[0]).unwrap(),
            "\x1bP0$r\x1b\\",
            "unsupported DECRQSS must answer invalid"
        );
    }
}

/// DECXCPR (`?6n`) replies with the page number, always 1 (esctest's
/// `test_DECDSR_DECXCPR`).
#[test]
fn decdsr_decxcpr_replies_with_three_param_cursor_position() {
    let mut g = drive_new(8, 16, b"\x1b[6;5H\x1b[?6n");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1b[?6;5;1R");
}

/// DECMSR (`?62n`) and DECCKSR (`?63;Pid n`) report zero: felis has
/// no macro store.
#[test]
fn decdsr_macro_status_replies_with_zero_space_and_checksum() {
    let mut g = drive_new(4, 8, b"\x1b[?62n\x1b[?63;42n");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1b[0*{");
    assert_eq!(responses[1], "\x1bP42!~0000\x1b\\");
}

/// `?75n` / `?85n` reply "OK" / "not configured", the xterm replies
/// esctest accepts.
#[test]
fn decdsr_integrity_and_session_replies_match_xterm() {
    let mut g = drive_new(4, 8, b"\x1b[?75n\x1b[?85n");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1b[?70n");
    assert_eq!(responses[1], "\x1b[?83n");
}

/// Locator DSRs report "no locator" (Ps=50) and type 0.
#[test]
fn decdsr_locator_replies_with_no_locator_attached() {
    let mut g = drive_new(4, 8, b"\x1b[?53n\x1b[?55n\x1b[?56n");
    let responses = reply_text(&mut g);
    assert_eq!(responses[0], "\x1b[?50n");
    assert_eq!(responses[1], "\x1b[?50n");
    assert_eq!(responses[2], "\x1b[?57;0n");
}

/// Mode 2027 reports permanently set; DECRESET does not move it.
#[test]
fn decrqm_dec_mode_2027_reports_permanently_set() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?2027$p");
    let initial = reply_text(&mut g);
    assert_eq!(initial[0], "\x1b[?2027;3$y");
    drive(&mut p, &mut g, b"\x1b[?2027l\x1b[?2027$p");
    let after_reset = reply_text(&mut g);
    assert_eq!(after_reset[0], "\x1b[?2027;3$y");
}

/// Unknown DEC modes report Ps=0 rather than the soft-state default,
/// until a DECSET / DECRST writes them: from then on they are
/// soft-tracked and report the written state
/// (`docs/reference/protocols/vt-compliance.md` "Consumed without
/// effect").
#[test]
fn decrqm_dec_unknown_mode_reports_ps_zero_until_written() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b[?9999$p");
    let cold = reply_text(&mut g);
    assert_eq!(cold[0], "\x1b[?9999;0$y");

    drive(&mut p, &mut g, b"\x1b[?9999h\x1b[?9999$p");
    let set = reply_text(&mut g);
    assert_eq!(set[0], "\x1b[?9999;1$y");

    drive(&mut p, &mut g, b"\x1b[?9999l\x1b[?9999$p");
    let reset = reply_text(&mut g);
    assert_eq!(reset[0], "\x1b[?9999;2$y");
}

/// The `DECSET 2048` report states the PTY winsize's pixel extents the
/// daemon set, height first.
#[test]
fn resize_notify_report_states_the_winsize_pixels() {
    let mut g = Grid::new(8, 10);
    g.set_winsize_pixels((805, 170));
    assert_eq!(
        std::str::from_utf8(&g.resize_notify_report_bytes()).unwrap(),
        "\x1b[48;8;10;170;805t"
    );
}

#[test]
fn ris_keeps_the_winsize_pixels() {
    let mut g = Grid::new(8, 10);
    let mut p = Parser::new();
    g.set_winsize_pixels((805, 170));
    drive(&mut p, &mut g, b"\x1bc\x1b[14t");
    assert_eq!(responses(&mut g), vec![b"\x1b[4;170;805t".to_vec()]);
}

/// Each `xterm_window_op` report arm emits its wire bytes; the pixel
/// arms answer `0` while the winsize's pixels are unknown.
#[test]
fn xterm_window_op_report_arms_emit_their_replies() {
    let mut g = Grid::new(8, 10);
    let mut p = Parser::new();
    let reply = |g: &mut Grid, p: &mut Parser, seq: &[u8]| -> String {
        drive(p, g, seq);
        let r = responses(g);
        String::from_utf8(r.into_iter().next().unwrap_or_default()).unwrap()
    };
    assert_eq!(reply(&mut g, &mut p, b"\x1b[11t"), "\x1b[1t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[13t"), "\x1b[3;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[14t"), "\x1b[4;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[15t"), "\x1b[5;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[16t"), "\x1b[6;0;0t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[19t"), "\x1b[9;8;10t");
}

/// The text-area and cell-size readbacks answer from the PTY's winsize,
/// the cell floored as a `TIOCGWINSZ` reader derives it; the screen
/// size stays the host's and unanswered.
#[test]
fn xterm_window_op_pixel_arms_answer_the_winsize_extents() {
    let mut g = Grid::new(8, 10);
    let mut p = Parser::new();
    g.set_winsize_pixels((805, 170));
    let reply = |g: &mut Grid, p: &mut Parser, seq: &[u8]| -> String {
        drive(p, g, seq);
        let r = responses(g);
        String::from_utf8(r.into_iter().next().unwrap_or_default()).unwrap()
    };
    assert_eq!(reply(&mut g, &mut p, b"\x1b[14t"), "\x1b[4;170;805t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[16t"), "\x1b[6;21;80t");
    assert_eq!(reply(&mut g, &mut p, b"\x1b[15t"), "\x1b[5;0;0t");
}

/// `CSI 20 t` / `CSI 21 t` report icon name / window title with the
/// `L` / `l` marker.
#[test]
fn xterm_window_op_reports_icon_and_window_title() {
    let mut g = Grid::new(4, 8);
    let mut p = Parser::new();
    drive(&mut p, &mut g, b"\x1b]2;WIN\x1b\\\x1b]1;ICON\x1b\\");
    drive(&mut p, &mut g, b"\x1b[21t");
    let r = responses(&mut g);
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1b]lWIN\x1b\\"
    );
    drive(&mut p, &mut g, b"\x1b[20t");
    let r = responses(&mut g);
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1b]LICON\x1b\\"
    );
}

/// DECRQSS ` q` reports the DECSCUSR Ps that produced the cursor
/// shape.
#[test]
fn decrqss_space_q_reports_cursor_shape() {
    let (_g, r) = drive_take(4, 8, b"\x1b[4 q\x1bP$q q\x1b\\");
    assert_eq!(
        String::from_utf8(r.into_iter().next().unwrap()).unwrap(),
        "\x1bP1$r4 q\x1b\\"
    );
}
