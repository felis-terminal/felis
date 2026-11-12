use super::*;

//
// Called directly the way `ShadowScreen::apply` does, pinning the
// admission rule and the blank-band math a parser-only test never
// reaches.

/// Row rotation, blanking with the default cell rather than the pen.
/// `None` for a directive the grid refuses.
fn scroll_directive_model(
    rows: &[Vec<Option<u8>>],
    region_top: u16,
    region_bottom: u16,
    n_rows: u16,
    direction: ScrollDirection,
) -> Option<Vec<Vec<Option<u8>>>> {
    let mut out = rows.to_vec();
    let top = usize::from(region_top);
    let bottom = usize::from(region_bottom);
    if top > bottom || bottom >= rows.len() {
        return None;
    }
    let height = bottom - top + 1;
    let n = usize::from(n_rows);
    if n == 0 || n > height {
        return None;
    }
    let blank = vec![None; rows[0].len()];
    for r in top..=bottom {
        out[r] = match direction {
            ScrollDirection::Up if r + n <= bottom => rows[r + n].clone(),
            ScrollDirection::Down if r >= top + n => rows[r - n].clone(),
            _ => blank.clone(),
        };
    }
    Some(out)
}

proptest! {
    #[test]
    fn apply_scroll_directive_matches_a_row_rotation_model(
        rows in 1u16..=6,
        cols in 1u16..=3,
        region_top in 0u16..=7,
        region_bottom in 0u16..=7,
        n_rows in 0u16..=8,
        down in any::<bool>(),
    ) {
        let filled: Vec<Vec<Option<u8>>> = (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| Some(b'a' + ((u32::from(r) * u32::from(cols) + u32::from(c)) % 26) as u8))
                    .collect()
            })
            .collect();
        let mut bytes = Vec::new();
        for (r, row) in filled.iter().enumerate() {
            bytes.extend_from_slice(format!("\x1b[{};1H", r + 1).as_bytes());
            bytes.extend(row.iter().map(|b| b.expect("filled")));
        }
        let mut p = Parser::new();
        let mut g = Grid::new(rows, cols);
        drive(&mut p, &mut g, &bytes);

        let direction = if down { ScrollDirection::Down } else { ScrollDirection::Up };
        let applied = g.apply_scroll_directive(region_top, region_bottom, n_rows, direction);

        let expected = scroll_directive_model(&filled, region_top, region_bottom, n_rows, direction);
        prop_assert_eq!(
            applied,
            expected.is_some(),
            "a directive outside the grid must be refused, not normalized"
        );
        let expected = expected.unwrap_or_else(|| filled.clone());
        for (r, row) in expected.iter().enumerate() {
            for (c, want) in row.iter().enumerate() {
                let cell = g.cell(r as u16, c as u16).expect("in-range cell");
                match want {
                    Some(b) => prop_assert_eq!(
                        cell.grapheme, Grapheme::Ascii(*b),
                        "row {} col {} should hold {}", r, c, *b as char
                    ),
                    None => prop_assert!(cell.is_blank(), "row {} col {} should be blank", r, c),
                }
            }
        }
    }
}

/// A scroll directive recycles the rotated-out storage as the vacated
/// row without clearing it, so the row written there next may equal
/// what the storage still holds; it must still land.
#[test]
fn a_row_written_over_a_vacated_row_lands_even_when_it_matches_the_rotated_out_row() {
    let mut screen = ScreenBuffer::with_scrollback(4, 3, 0);
    let row = [Cell {
        grapheme: Grapheme::Ascii(b'y'),
        ..Cell::default()
    }; 3];
    for r in 0..4 {
        screen.write_row_cells(r, &row);
    }
    assert!(screen.apply_scroll_directive(0, 3, 1, ScrollDirection::Up));
    assert_eq!(screen.cell(3, 0).map(|c| c.grapheme), Some(Grapheme::Empty));
    screen.write_row_cells(3, &row);
    assert_eq!(
        screen.cell(3, 0).map(|c| c.grapheme),
        Some(Grapheme::Ascii(b'y')),
    );
}
