use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intro_banner_reaches_the_grid() {
    let session = VttestSession::open();

    // "VT100 test program" rather than the full banner, so a changed
    // version suffix still passes.
    let hit = session
        .wait_for("VT100 test program", Duration::from_secs(3))
        .await;
    assert!(hit, "expected vttest intro banner on the grid");

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_movements_draws_unbroken_frame() {
    let session = VttestSession::open();
    session.navigate(&["Test of cursor movements"]).await;

    // The first sub-test draws an unbroken `*` frame around the outer
    // border. Gate on the prompt so the full screen has drawn, then
    // check the corners.
    assert!(
        session
            .wait_for("unbroken bor-", Duration::from_secs(5))
            .await,
        "cursor-movements frame text never reached the grid",
    );
    let frame_ok = session.with_grid(|g| {
        let last_row = g.rows() - 1;
        let last_col = g.cols() - 1;
        let corner = |r, c| match &g.cell(r, c).unwrap().grapheme {
            Grapheme::Ascii(b) => *b == b'*',
            _ => false,
        };
        corner(0, 0) && corner(0, last_col) && corner(last_row, 0) && corner(last_row, last_col)
    });
    if !frame_ok {
        let dump = session.with_grid(dump_grid);
        panic!("cursor-movements outer frame is broken at the corners. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrap_around_cursor_addressing_pins_stars_to_last_column() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of known bugs", "Wrap around with cursor"])
        .await;

    // vttest's "Bug 7": CUP-then-print at the rightmost column. On a
    // buggy terminal the `*`s land in column 0 instead of column 79.
    // The explanation is drawn before the stars and the prompt after
    // them, so only the prompt says the stars are all there.
    assert!(
        session
            .wait_for("Push <RETURN>", Duration::from_secs(5))
            .await
    );
    let stars_ok = session.with_grid(|g| {
        for r in 1u16..21 {
            let last = g.cell(r, 79).unwrap();
            let first = g.cell(r, 0).unwrap();
            let is_star = matches!(&last.grapheme, Grapheme::Ascii(b'*'));
            let first_blank = matches!(&first.grapheme, Grapheme::Empty | Grapheme::Spacer)
                || matches!(&first.grapheme, Grapheme::Ascii(b' '));
            if !is_star || !first_blank {
                return false;
            }
        }
        true
    });
    if !stars_ok {
        let dump = session.with_grid(dump_grid);
        panic!("VT100 wrap-around bug — stars leaked off column 79. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decstr_soft_reset_completes_in_menu_10() {
    let session = VttestSession::open();
    session
        .navigate(&["Test of reset", "Soft Terminal Reset"])
        .await;

    // "Push <RETURN>" reappearing is the simplest cue that the reset
    // ran.
    let hit = session
        .wait_for("Push <RETURN>", Duration::from_secs(5))
        .await;
    if !hit {
        let dump = session.with_grid(dump_grid);
        panic!("DECSTR screen never settled. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_autowrap_fills_three_rows_with_stars() {
    let session = VttestSession::open();
    session.navigate(&["Test of screen features"]).await;

    // ~240 `*` into a fresh screen with default autowrap leaves three
    // identical 80-wide rows.
    assert!(
        session
            .wait_for("WRAP AROUND mode setting", Duration::from_secs(5))
            .await,
        "autowrap test screen never drew",
    );
    let three_full_rows = session.with_grid(|g| {
        for r in [0u16, 1, 2] {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    _ => return false,
                };
                if ch != '*' {
                    return false;
                }
            }
        }
        true
    });
    if !three_full_rows {
        let dump = session.with_grid(dump_grid);
        panic!("autowrap did not fill three rows of '*'. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_tab_setting_lands_stars_on_custom_stops() {
    let session = VttestSession::open();
    session.navigate(&["Test of screen features"]).await;

    // Step past the autowrap screen to the tab-setting screen: TBC 3,
    // HTS every 6 columns, HT along row 0; row 1 uses literal spaces.
    // Both rows land `*` at columns 6, 12, …, 78.
    assert!(
        session
            .wait_for("WRAP AROUND mode", Duration::from_secs(5))
            .await
    );
    session.send("\r").await;
    assert!(
        session
            .wait_for("Test of TAB setting", Duration::from_secs(5))
            .await,
        "tab-setting screen never drew",
    );
    let stops_match = session.with_grid(|g| {
        let cols: Vec<u16> = (6..g.cols()).step_by(6).collect();
        for col in &cols {
            for row in [0u16, 1] {
                let cell = g.cell(row, *col).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => return false,
                };
                if ch != '*' {
                    return false;
                }
            }
        }
        !cols.is_empty()
    });
    if !stops_match {
        let dump = session.with_grid(dump_grid);
        panic!("HT did not land `*` on the custom tab stops. Grid:\n{dump}");
    }

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vt102_accordion_test_draws_per_row_letter_fill() {
    let session = VttestSession::open();
    session.navigate(&["Test of VT102 features"]).await;

    // The accordion seed fills row r with its own letter (A, B, …),
    // except row 3, which carries an inlined instruction.
    assert!(
        session
            .wait_for("Screen accordion test", Duration::from_secs(5))
            .await,
        "VT102 features test screen never drew",
    );
    let ok = session.with_grid(|g| {
        let letter_of = |r: u16| (b'A' + (r as u8)) as char;
        for r in [0u16, 1, 2] {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => continue,
                };
                if ch != letter_of(r) {
                    return false;
                }
            }
        }
        for r in 4u16..g.rows() {
            // Some vttest versions bleed the instruction text into the
            // first columns of row 4; sample the right half only.
            let half = g.cols() / 2;
            for c in half..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => continue,
                };
                if ch != letter_of(r) {
                    return false;
                }
            }
        }
        true
    });
    if !ok {
        let dump = session.with_grid(dump_grid);
        panic!("VT102 accordion seed letters are wrong. Grid:\n{dump}");
    }

    session.shutdown().await;
}
