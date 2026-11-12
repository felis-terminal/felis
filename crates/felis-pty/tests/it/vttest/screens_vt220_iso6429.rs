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
    assert!(
        session
            .wait_for("solid box made of *", Duration::from_secs(5))
            .await,
        "DECSCA test screen never drew",
    );
    let box_present = session.with_grid(|g| {
        // Center of the box, and a cell expected to be cleared.
        let center = g.cell(10, 40).unwrap();
        let outside = g.cell(1, 0).unwrap();
        let is_star = |c: &felis_grid::Cell| match &c.grapheme {
            Grapheme::Ascii(b) => *b == b'*',
            _ => false,
        };
        let is_blank = |c: &felis_grid::Cell| matches!(c.grapheme, Grapheme::Empty);
        is_star(center) && is_blank(outside)
    });
    if !box_present {
        let dump = session.with_grid(dump_grid);
        panic!("DECSCA box not formed correctly. Grid:\n{dump}");
    }

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
    assert!(
        session
            .wait_for("horizontal row of *", Duration::from_secs(5))
            .await,
        "SU test screen never drew",
    );
    session.check_or_dump(|g| {
        for c in 0u16..20 {
            if !matches!(g.cell(0, c).unwrap().grapheme, Grapheme::Ascii(b'*')) {
                return Err("SU did not lift asterisks to the top row".into());
            }
        }
        Ok(())
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
    assert!(
        session
            .wait_for("vertical column of *'s centered", Duration::from_secs(5))
            .await,
        "SR test screen never drew",
    );
    let centered = session.with_grid(|g| {
        for r in 0u16..20 {
            let cell = g.cell(r, 39).unwrap();
            let ch = match &cell.grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
                _ => return false,
            };
            if ch != '*' {
                return false;
            }
        }
        true
    });
    if !centered {
        let dump = session.with_grid(dump_grid);
        panic!("SR did not center the asterisk column. Grid:\n{dump}");
    }

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
    assert!(
        session
            .wait_for("diagonal of 2 +", Duration::from_secs(5))
            .await,
        "REP test screen never drew",
    );
    let diagonal_ok = session.with_grid(|g| {
        for r in 0u16..12 {
            let cell = g.cell(r, 12).unwrap();
            let ch = match &cell.grapheme {
                Grapheme::Ascii(b) => *b as char,
                Grapheme::Char(c) => *c,
                _ => return false,
            };
            if ch != '+' {
                return false;
            }
        }
        true
    });
    if !diagonal_ok {
        let dump = session.with_grid(dump_grid);
        panic!("REP diagonal is missing or incomplete. Grid:\n{dump}");
    }

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
    assert!(
        session
            .wait_for("should be numbered in sequence", Duration::from_secs(5))
            .await,
        "CNL test screen never drew",
    );
    let stacked_correctly = session.with_grid(|g| {
        for r in 0u16..19 {
            let expected = format!("{}", r + 1);
            let mut row_prefix = String::new();
            for c in 0..expected.len() as u16 {
                let cell = g.cell(r, c).unwrap();
                let ch = match &cell.grapheme {
                    Grapheme::Ascii(b) => *b as char,
                    Grapheme::Char(c) => *c,
                    _ => return false,
                };
                row_prefix.push(ch);
            }
            if row_prefix != expected {
                return false;
            }
        }
        true
    });
    if !stacked_correctly {
        let dump = session.with_grid(dump_grid);
        panic!("CNL did not stack numbers at column 0. Grid:\n{dump}");
    }

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
    assert!(
        session
            .wait_for("box-outline made of *", Duration::from_secs(5))
            .await,
        "HPA test screen never drew",
    );
    // vttest's hold prompt comes last, after the box and its caption.
    assert!(
        session
            .wait_for("Push <RETURN>", Duration::from_secs(5))
            .await,
        "HPA test screen never finished",
    );
    session.check_or_dump(|g| {
        let is_star = |r: u16, c: u16| -> bool {
            matches!(g.cell(r, c).unwrap().grapheme, Grapheme::Ascii(b'*'))
        };
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
    assert!(
        session.wait_for("ECH test", Duration::from_secs(5)).await,
        "ECH screen never drew",
    );
    let ok = session.with_grid(|g| {
        // Scan for the stars: releases drift on the exact column, and
        // the pattern "many E's, one gap, two stars" is what matters.
        let row = 5u16;
        let mut e_count = 0u16;
        let mut col = 0u16;
        while col < g.cols() {
            if matches!(g.cell(row, col).unwrap().grapheme, Grapheme::Ascii(b'E')) {
                e_count += 1;
                col += 1;
            } else {
                break;
            }
        }
        if e_count == 0 {
            return false;
        }
        let gap = g.cell(row, col).unwrap();
        let star_l = g.cell(row, col + 1).unwrap();
        let star_r = g.cell(row, col + 2).unwrap();
        let blank = matches!(gap.grapheme, Grapheme::Empty | Grapheme::Spacer)
            || matches!(gap.grapheme, Grapheme::Ascii(b' '));
        let star = matches!(star_l.grapheme, Grapheme::Ascii(b'*'))
            && matches!(star_r.grapheme, Grapheme::Ascii(b'*'));
        blank && star
    });
    if !ok {
        let dump = session.with_grid(dump_grid);
        panic!("ECH diagonal not formed. Grid:\n{dump}");
    }

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
    assert!(
        session
            .wait_for("cursor should be invisible", Duration::from_secs(5))
            .await,
        "DECTCEM screen never drew",
    );
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
    assert!(
        session
            .wait_for("background should be blue", Duration::from_secs(5))
            .await,
        "BCE test screen never drew",
    );
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
    if !session
        .wait_for("bright *on*", Duration::from_secs(5))
        .await
    {
        let dump = session.with_grid(dump_grid);
        panic!("color test-pattern never drew the 'bright on' matrix. Grid:\n{dump}");
    }
    let labeled = session.with_grid(|g| {
        grid_contains(g, "red")
            && grid_contains(g, "green")
            && grid_contains(g, "blue")
            && grid_contains(g, "magenta")
    });
    if !labeled {
        let dump = session.with_grid(dump_grid);
        panic!("ISO-6429 colors labels missing. Grid:\n{dump}");
    }

    // Text presence proves vttest spoke; a colored cell proves felis
    // honored the SGRs in between.
    let has_colored_cell = session.with_grid(|g| {
        for r in 0..g.rows() {
            for c in 0..g.cols() {
                let cell = g.cell(r, c).unwrap();
                let attrs = g.style(cell.style);
                if !matches!(attrs.fg, felis_grid::Color::Default)
                    || !matches!(attrs.bg, felis_grid::Color::Default)
                {
                    return true;
                }
            }
        }
        false
    });
    if !has_colored_cell {
        let dump = session.with_grid(dump_grid);
        panic!("colors test left no colored cells on the grid. Grid:\n{dump}");
    }

    session.shutdown().await;
}
