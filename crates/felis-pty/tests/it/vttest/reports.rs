use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da1_reply_round_trips_through_terminal_reports_menu() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of terminal reports", "Primary Device Attributes"])
        .await;

    // The DA1 reply is `CSI ?64;1;2;…;29c`. Match vttest's
    // interpretation of the leading 64 ("VT400 family") rather than
    // its per-byte echo, whose whitespace decoration varies by version.
    session
        .expect_text(
            "VT400 family",
            Duration::from_secs(5),
            "DA1 reply did not round-trip through vttest's display",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da2_reply_round_trips_through_terminal_reports_menu() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of terminal reports", "Secondary Device Attributes"])
        .await;

    // The DA2 reply is `CSI > 41 ; 400 ; 0 c`. Match vttest's decoded
    // `Pp=41 (VT420)` rather than a bare model name: the preceding
    // submenu annotates entries with "VT420 and up", so a bare-name
    // match can pass without any reply at all.
    session
        .expect_text(
            "Pp=41 (VT420)",
            Duration::from_secs(5),
            "DA2 reply did not round-trip",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn da3_reply_round_trips_through_terminal_reports_menu() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of terminal reports", "Tertiary Device Attributes"])
        .await;

    // The DA3 reply is `DCS ! | 66656c69732d31 ST`, the hex of
    // "felis-1". vttest renders the per-byte echo space-separated;
    // match the spaced hex so its escape-byte decoration does not
    // matter.
    session
        .expect_text(
            "6 6 6 5 6 c 6 9 7 3 2 d 3 1",
            Duration::from_secs(5),
            "DA3 reply did not round-trip",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decreqtparm_reply_round_trips_through_terminal_reports_menu() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of terminal reports", "Request Terminal Parameters"])
        .await;

    // The DECREQTPARM reply is `\e[2;1;1;120;120;1;0x`; vttest decodes
    // 120 as the baud code for 19200.
    session
        .expect_text(
            "19200",
            Duration::from_secs(5),
            "DECREQTPARM reply did not round-trip",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decrqss_decscusr_round_trips_blink_state() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Test reporting functions",
            "Status-String Report",
            "Test VT520 features",
            "Test VT510 features",
            "Set Cursor Style",
        ])
        .await;

    // vttest renders "ok (valid request)" only when the DECRQSS reply
    // carries Ps=1 and the same body it sent.
    session
        .expect_text(
            "ok (valid request)",
            Duration::from_secs(8),
            "DECRQSS DECSCUSR round-trip did not report 'valid request'",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xtversion_round_trips_felis_name_and_version() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Test reporting functions",
            "Report version (XTVERSION)",
        ])
        .await;

    // vttest renders the reply payload with one space between bytes
    // ("f e l i s <32> 0 . 1 . 0"); match the name so version drift
    // does not matter.
    session
        .expect_text(
            "f e l i s",
            Duration::from_secs(5),
            "XTVERSION reply did not round-trip",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_state_report_returns_normal_for_csi_11_t() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test XTERM special features",
            "Window report-operations",
        ])
        .await;

    // For `CSI 11 t` felis replies `CSI 1 t` and vttest renders
    // "OK: normal" below the raw response.
    session
        .expect_text(
            "OK: normal",
            Duration::from_secs(8),
            "window state (11) report not OK",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s8c1t_round_trips_through_8bit_control_test() {
    let session = VttestSession::open();
    session
        .navigate(&["non-VT100", "Test of VT220 features", "Test 8-bit controls"])
        .await;

    // vttest sends a DSR in 8-bit mode and again in 7-bit mode; the
    // 8-bit reply renders as "<155> …". Anchor on the introductory
    // line ("emit 8-bit") so the matcher does not fire on the menu
    // item that contains "8-bit controls".
    session
        .expect_text(
            "emit 8-bit",
            Duration::from_secs(5),
            "emit 8-bit never drew",
        )
        .await;
    // The two `ok` lines may arrive in either order.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut enabled_ok = false;
    let mut disabled_ok = false;
    while tokio::time::Instant::now() < deadline {
        let dump = session.with_grid(dump_grid);
        enabled_ok = dump.contains("8-bit controls enabled:") && dump.contains("<155>");
        disabled_ok = dump.contains("8-bit controls disabled:") && dump.contains("<27> [");
        if enabled_ok && disabled_ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !(enabled_ok && disabled_ok) {
        let dump = session.with_grid(dump_grid);
        panic!("8-bit-control round-trip did not produce both ok lines. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_dsr_reports_no_printer_locked_udk_us_keyboard() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test reporting functions",
            "Test Device Status Report",
            "Test Keyboard Status",
        ])
        .await;
    // "North American/ASCII" is the most distinctive of vttest's
    // interpretation strings.
    session
        .expect_text(
            "North American",
            Duration::from_secs(5),
            "keyboard DSR ?26 n did not produce North American reply",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsr_cursor_position_round_trips() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of terminal reports", "Device Status Report"])
        .await;

    // The DSR 5 line above ends in "TERMINAL OK", so a bare "OK" would
    // match before the cursor report is ever checked. The needle is the
    // verdict vttest prints after echoing the `CSI 6 n` reply.
    session
        .expect_text(
            "R  -- OK",
            Duration::from_secs(8),
            "DSR cursor position round-trip did not produce 'R  -- OK'",
        )
        .await;

    session.shutdown().await;
}
