use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intro_banner_reaches_the_grid() {
    let session = VttestSession::open();

    // "VT100 test program" rather than the full banner, so a changed
    // version suffix still passes.
    session
        .expect_text(
            "VT100 test program",
            Duration::from_secs(3),
            "expected vttest intro banner on the grid",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_movements_draws_unbroken_frame() {
    let session = VttestSession::open();
    session.navigate(&["Test of cursor movements"]).await;

    // The first sub-test draws an unbroken `*` frame around the outer
    // border. Gate on the prompt so the full screen has drawn, then
    // check the corners.
    session
        .expect_text(
            "unbroken bor-",
            Duration::from_secs(5),
            "cursor-movements frame text never reached the grid",
        )
        .await;
    session.check_or_dump(|g| {
        let last_row = g.rows() - 1;
        let last_col = g.cols() - 1;
        let corners = [(0, 0), (0, last_col), (last_row, 0), (last_row, last_col)];
        if corners.iter().all(|&(r, c)| char_at(g, r, c) == Some('*')) {
            Ok(())
        } else {
            Err("cursor-movements outer frame is broken at the corners".into())
        }
    });

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
    session
        .expect_text(
            "Push <RETURN>",
            Duration::from_secs(5),
            "wrap-around screen never finished",
        )
        .await;
    session.check_or_dump(|g| {
        for r in 1u16..21 {
            let first = &g.cell(r, 0).unwrap().grapheme;
            let first_blank = matches!(first, Grapheme::Empty | Grapheme::Spacer)
                || matches!(first, Grapheme::Ascii(b' '));
            if char_at(g, r, 79) != Some('*') || !first_blank {
                return Err("VT100 wrap-around bug — stars leaked off column 79".into());
            }
        }
        Ok(())
    });

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
    session
        .expect_text(
            "Push <RETURN>",
            Duration::from_secs(5),
            "DECSTR screen never settled",
        )
        .await;

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_autowrap_fills_three_rows_with_stars() {
    let session = VttestSession::open();
    session.navigate(&["Test of screen features"]).await;

    // ~240 `*` into a fresh screen with default autowrap leaves three
    // identical 80-wide rows.
    session
        .expect_text(
            "WRAP AROUND mode setting",
            Duration::from_secs(5),
            "autowrap test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        let full = [0u16, 1, 2]
            .into_iter()
            .all(|r| (0..g.cols()).all(|c| char_at(g, r, c) == Some('*')));
        if full {
            Ok(())
        } else {
            Err("autowrap did not fill three rows of '*'".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn screen_features_tab_setting_lands_stars_on_custom_stops() {
    let session = VttestSession::open();
    session.navigate(&["Test of screen features"]).await;

    // Step past the autowrap screen to the tab-setting screen: TBC 3,
    // HTS every 6 columns, HT along row 0; row 1 uses literal spaces.
    // Both rows land `*` at columns 6, 12, …, 78.
    session
        .expect_text(
            "WRAP AROUND mode",
            Duration::from_secs(5),
            "autowrap test screen never drew",
        )
        .await;
    session.send("\r").await;
    session
        .expect_text(
            "Test of TAB setting",
            Duration::from_secs(5),
            "tab-setting screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        let cols: Vec<u16> = (6..g.cols()).step_by(6).collect();
        let landed = cols.iter().all(|&col| {
            [0u16, 1]
                .into_iter()
                .all(|row| char_at(g, row, col) == Some('*'))
        });
        if landed && !cols.is_empty() {
            Ok(())
        } else {
            Err("HT did not land `*` on the custom tab stops".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vt102_accordion_test_draws_per_row_letter_fill() {
    let session = VttestSession::open();
    session.navigate(&["Test of VT102 features"]).await;

    // The accordion seed fills row r with its own letter (A, B, …),
    // except row 3, which carries an inlined instruction.
    session
        .expect_text(
            "Screen accordion test",
            Duration::from_secs(5),
            "VT102 features test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        let letter_of = |r: u16| (b'A' + (r as u8)) as char;
        let row_ok = |r: u16, cols: std::ops::Range<u16>| {
            cols.filter_map(|c| char_at(g, r, c))
                .all(|ch| ch == letter_of(r))
        };
        let top = [0u16, 1, 2].into_iter().all(|r| row_ok(r, 0..g.cols()));
        // Some vttest versions bleed the instruction text into the
        // first columns of row 4; sample the right half only.
        let half = g.cols() / 2;
        let rest = (4u16..g.rows()).all(|r| row_ok(r, half..g.cols()));
        if top && rest {
            Ok(())
        } else {
            Err("VT102 accordion seed letters are wrong".into())
        }
    });

    session.shutdown().await;
}
