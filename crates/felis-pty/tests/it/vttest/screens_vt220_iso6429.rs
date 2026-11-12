use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decsca_leaves_protected_asterisk_box_after_decsed() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Protected-Areas (DECSCA)",
        ])
        .await;

    // After DECSED only the DECSCA-protected ~12x40 centered box of `*`
    // survives; the instruction text draws after the pattern.
    session
        .expect_text(
            "solid box made of *",
            Duration::from_secs(5),
            "DECSCA test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        // Center of the box, and a cell expected to be cleared.
        let center_is_star = char_at(g, 10, 40) == Some('*');
        let outside_is_blank = matches!(g.cell(1, 0).unwrap().grapheme, Grapheme::Empty);
        if center_is_star && outside_is_blank {
            Ok(())
        } else {
            Err("DECSCA box not formed correctly".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn su_lifts_asterisks_to_top_row() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test other ISO-6429 features",
            "Test Scroll-Up",
        ])
        .await;

    // vttest SUs a row of `*` up to row 0; it spans at least 20 cells.
    session
        .expect_text(
            "horizontal row of *",
            Duration::from_secs(5),
            "SU test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        if (0u16..20).all(|c| char_at(g, 0, c) == Some('*')) {
            Ok(())
        } else {
            Err("SU did not lift asterisks to the top row".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sr_centers_asterisk_column_on_the_screen() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test other ISO-6429 features",
            "Test Scroll-Right",
        ])
        .await;

    // vttest SRs a column of `*` in rows 0..=19 to column 39.
    session
        .expect_text(
            "vertical column of *'s centered",
            Duration::from_secs(5),
            "SR test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        if (0u16..20).all(|r| char_at(g, r, 39) == Some('*')) {
            Ok(())
        } else {
            Err("SR did not center the asterisk column".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rep_draws_diagonal_of_plus_characters() {
    let session = VttestSession::open();
    session
        .navigate(&["non-VT100", "Test other ISO-6429 features", "Test Repeat"])
        .await;

    // Row r holds 12 `+` starting at col r+1, so col 12 is `+` on rows
    // 0..=11 only if REP repeats.
    session
        .expect_text(
            "diagonal of 2 +",
            Duration::from_secs(5),
            "REP test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        if (0u16..12).all(|r| char_at(g, r, 12) == Some('+')) {
            Ok(())
        } else {
            Err("REP diagonal is missing or incomplete".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cnl_numbers_lines_in_sequence_at_column_zero() {
    let session = VttestSession::open();
    session
        .navigate(&["non-VT100", "ISO-6429 cursor-movement", "Test Next-Line"])
        .await;

    // Row N begins with N+1 at column 0 (a CNL that fell through to
    // CUD would stair-step). Anchor on the explanation text: the menu
    // label "Test Next-Line (CNL)" is too close to the menu line.
    session
        .expect_text(
            "should be numbered in sequence",
            Duration::from_secs(5),
            "CNL test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        let stacked = (0u16..19).all(|r| {
            let expected = format!("{}", r + 1);
            let prefix: Option<String> = (0..expected.len() as u16)
                .map(|c| char_at(g, r, c))
                .collect();
            prefix == Some(expected)
        });
        if stacked {
            Ok(())
        } else {
            Err("CNL did not stack numbers at column 0".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hpa_draws_box_outline_in_the_middle() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "ISO-6429 cursor-movement",
            "Test Character-Position-Absolute",
        ])
        .await;

    // vttest paints a `*` box outline via HPA: a top edge of ~40 cells
    // and side columns through rows 1..11.
    session
        .expect_text(
            "box-outline made of *",
            Duration::from_secs(5),
            "HPA test screen never drew",
        )
        .await;
    // vttest's hold prompt comes last, after the box and its caption.
    session
        .expect_text(
            "Push <RETURN>",
            Duration::from_secs(5),
            "HPA test screen never finished",
        )
        .await;
    session.check_or_dump(|g| {
        let is_star = |r: u16, c: u16| char_at(g, r, c) == Some('*');
        // vttest centers the box vertically; find the top row.
        let Some(top) = (0..g.rows()).find(|r| (0..g.cols()).any(|c| is_star(*r, c))) else {
            return Err("HPA box outline broken: no '*' anywhere".into());
        };
        let start = (0..g.cols()).find(|c| is_star(top, *c)).unwrap();
        for off in 0..30u16 {
            if !is_star(top, start + off) {
                return Err(format!(
                    "HPA box outline broken: row {top} col {} is not '*'",
                    start + off
                ));
            }
        }
        let mut end = start + 29;
        while end + 1 < g.cols() && is_star(top, end + 1) {
            end += 1;
        }
        for r in (top + 1)..(top + 11) {
            if !is_star(r, start) {
                return Err(format!(
                    "HPA box outline broken: row {r} col {start} (left side) blank"
                ));
            }
            if !is_star(r, end) {
                return Err(format!(
                    "HPA box outline broken: row {r} col {end} (right side) blank"
                ));
            }
        }
        Ok(())
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ech_paints_clear_gap_diagonal() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Erase Char",
        ])
        .await;

    // Row R: E's at cols 0..=(73-R), an ECH gap at 74-R, `**` at
    // 75-R..=76-R, then dots.
    session
        .expect_text("ECH test", Duration::from_secs(5), "ECH screen never drew")
        .await;
    session.check_or_dump(|g| {
        // Scan for the stars: releases drift on the exact column, and
        // the pattern "many E's, one gap, two stars" is what matters.
        let row = 5u16;
        let col = (0..g.cols())
            .find(|&c| char_at(g, row, c) != Some('E'))
            .unwrap_or_else(|| g.cols());
        if col == 0 {
            return Err("ECH diagonal not formed: no E's at the row start".into());
        }
        let gap = &g.cell(row, col).unwrap().grapheme;
        let blank = matches!(gap, Grapheme::Empty | Grapheme::Spacer)
            || matches!(gap, Grapheme::Ascii(b' '));
        let star = char_at(g, row, col + 1) == Some('*') && char_at(g, row, col + 2) == Some('*');
        if blank && star {
            Ok(())
        } else {
            Err("ECH diagonal not formed".into())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dectcem_hides_cursor_in_menu_11_1_2_2() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test of VT220 features",
            "Test screen-display functions",
            "Test Visible/Invisible Cursor",
        ])
        .await;

    // vttest hides the cursor via `CSI ?25 l`; check the grid state.
    session
        .expect_text(
            "cursor should be invisible",
            Duration::from_secs(5),
            "DECTCEM screen never drew",
        )
        .await;
    let visible = session.with_grid(|g| g.cursor().visible);
    assert!(!visible, "DECTCEM ?25l did not hide the cursor");

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bce_clear_paints_screen_background_blue() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test ISO-6429 colors",
            "Test BCE-style clear line/display (ED, EL)",
        ])
        .await;

    // vttest sets bg=blue, ED-clears, then draws a box. With BCE the
    // blank cells outside the box carry bg=blue; row 0 col 0 is one.
    session
        .expect_text(
            "background should be blue",
            Duration::from_secs(5),
            "BCE test screen never drew",
        )
        .await;
    session.check_or_dump(|g| {
        let cell = g.cell(0, 0).unwrap();
        if matches!(g.style(cell.style).bg, felis_grid::Color::Default) {
            Err("BCE did not paint screen bg on cleared cells".into())
        } else {
            Ok(())
        }
    });

    session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn iso_6429_colors_paints_named_palette() {
    let session = VttestSession::open();
    session
        .navigate(&[
            "non-VT100",
            "Test ISO-6429 colors",
            "Display color test-pattern",
        ])
        .await;

    // The pattern screen prints a "bright off" matrix then a "bright
    // on" one; the second one's arrival proves both completed without
    // the parser dropping any SGR.
    session
        .expect_text(
            "bright *on*",
            Duration::from_secs(5),
            "color test-pattern never drew the 'bright on' matrix",
        )
        .await;
    session.check_or_dump(|g| {
        if ["red", "green", "blue", "magenta"]
            .into_iter()
            .all(|label| grid_contains(g, label))
        {
            Ok(())
        } else {
            Err("ISO-6429 colors labels missing".into())
        }
    });

    // Text presence proves vttest spoke; a colored cell proves felis
    // honored the SGRs in between.
    session.check_or_dump(|g| {
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let attrs = g.style(cell.style);
                if !matches!(attrs.fg, felis_grid::Color::Default)
                    || !matches!(attrs.bg, felis_grid::Color::Default)
                {
                    return Ok(());
                }
            }
        }
        Err("colors test left no colored cells on the grid".into())
    });

    session.shutdown().await;
}
