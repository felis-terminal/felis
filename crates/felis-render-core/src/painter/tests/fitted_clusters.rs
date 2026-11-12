use super::*;

/// A one-row grid fed `bytes`, walked and painted against the pinned
/// set.
struct Painted {
    index: GlyphIndex,
    frame: ShapeFrame,
    glyphs: Vec<GlyphDraw>,
}

fn paint_pinned(bytes: &[u8], cols: u16) -> Painted {
    let stack = FontStack::pinned_test_stack();
    let mut grid = Grid::new(1, cols);
    Parser::new().advance(&mut grid, bytes);
    let mut index = GlyphIndex::new(stack, 18.0, NonZeroU32::new(1024).unwrap());
    let mut frame = ShapeFrame::empty();
    GridWalker::default().walk(
        &mut index,
        grid.screen(),
        &mut Shaper::new(),
        &[],
        &mut frame,
    );
    let theme = Theme::default();
    let mut out = RowPaint::default();
    CellPainter::new(
        grid.screen(),
        index.cell_metrics(),
        &ResolvedTheme::new(&theme),
        &index,
        &frame,
    )
    .without_compaction()
    .with_viewport(0, 1)
    .extend_screen(&mut out);
    Painted {
        index,
        frame,
        glyphs: out.glyphs,
    }
}

fn cluster_fit_px_at(p: &Painted, col: u16) -> u16 {
    match p.frame.cell_at(0, col) {
        ShapedCell::Cluster { fit_px, .. } => fit_px,
        other => panic!("col {col} is not a cluster: {other:?}"),
    }
}

/// The fg quads' union as `[x0, y0, x1, y1]`.
fn ink(fg: &[GlyphDraw]) -> [f32; 4] {
    fg.iter()
        .map(|q| {
            let [x, y] = q.origin_px;
            [x, y, x + q.size_px[0], y + q.size_px[1]]
        })
        .reduce(|[a, b, c, d], [x0, y0, x1, y1]| [a.min(x0), b.min(y0), c.max(x1), d.max(y1)])
        .expect("the cluster painted")
}

/// Each repro is the only ink on its row, its cluster at `col` with
/// `cells` cells: a ZWJ pair the face has no glyph for, a modifier on a
/// base that takes none, and a VS16 widen refused at the last column.
#[test]
fn a_cluster_wider_than_its_cells_fills_and_centres_in_them() {
    let repros: [(&str, &[u8], u16, u16); 3] = [
        ("non-RGI ZWJ", "\u{1F408}\u{200D}\u{1F408}".as_bytes(), 0, 2),
        ("modifier", "\u{1F600}\u{1F3FB}".as_bytes(), 0, 2),
        ("refused widen", "\x1b[4G\u{2764}\u{FE0F}".as_bytes(), 3, 1),
    ];
    for (name, bytes, col, cells) in repros {
        let p = paint_pinned(bytes, 4);
        let fit_px = f32::from(cluster_fit_px_at(&p, col));
        assert_ne!(fit_px, 0.0, "{name}: fitted");
        // The size is floored to a whole pixel, which costs up to one
        // part in `fit_px + 1` of the block.
        let filled = |ink: f32, block: f32| ink >= block * fit_px / (fit_px + 1.0) - 1.0;
        let m = p.index.cell_metrics();
        let (cw, ch) = (m.width as f32, m.height as f32);
        let (bx0, bx1) = (f32::from(col) * cw, f32::from(col + cells) * cw);
        let [x0, y0, x1, y1] = ink(&p.glyphs);
        assert!(
            x0 >= bx0 - 1.0 && x1 <= bx1 + 1.0 && y0 >= -1.0 && y1 <= ch + 1.0,
            "{name}: ink {x0}..{x1} x {y0}..{y1} inside {bx0}..{bx1} x 0..{ch}"
        );
        assert!(
            ((x0 - bx0) - (bx1 - x1)).abs() <= 1.0 && (y0 - (ch - y1)).abs() <= 1.0,
            "{name}: ink {x0}..{x1} x {y0}..{y1} centred in {bx0}..{bx1} x 0..{ch}"
        );
        assert!(
            filled(x1 - x0, bx1 - bx0) || filled(y1 - y0, ch),
            "{name}: ink {x0}..{x1} x {y0}..{y1} fills an axis"
        );
    }
}

/// The fit is the bare char's: a cluster of one glyph shrinks to the
/// size `fitted_bitmap` gives that glyph alone.
#[test]
fn a_one_glyph_cluster_fits_as_its_bare_char_does() {
    let cluster = paint_pinned("\x1b[4G\u{263A}\u{FE0F}".as_bytes(), 4);
    let bare = paint_pinned("\x1b[4G\u{263A}".as_bytes(), 4);
    let [a0, b0, a1, b1] = ink(&cluster.glyphs);
    let [c0, d0, c1, d1] = ink(&bare.glyphs);
    assert!(
        ((a1 - a0) - (c1 - c0)).abs() <= 1.0 && ((b1 - b0) - (d1 - d0)).abs() <= 1.0,
        "cluster {}x{} vs bare {}x{}",
        a1 - a0,
        b1 - b0,
        c1 - c0,
        d1 - d0,
    );
}

/// Under an OSC 66 fractional scale a fitted cluster's pen shrinks
/// with its glyphs: the second cat sits one advance at the fitted
/// size after the first, not one advance at the native size.
#[test]
fn a_fitted_cluster_advances_its_pen_at_the_fitted_size() {
    let cats = "\u{1F408}\u{200D}\u{1F408}";
    let stack = FontStack::pinned_test_stack();
    let mut grid = Grid::new(2, 8);
    Parser::new().advance(
        &mut grid,
        format!("\x1b]66;w=1:s=2:n=3:d=4;{cats}\x07").as_bytes(),
    );
    let mut index = GlyphIndex::new(stack, 18.0, NonZeroU32::new(1024).unwrap());
    let mut frame = ShapeFrame::empty();
    GridWalker::default().walk(
        &mut index,
        grid.screen(),
        &mut Shaper::new(),
        &[],
        &mut frame,
    );
    let ShapedCell::Cluster { start, len, fit_px } = frame.cell_at(0, 0) else {
        panic!("not a cluster: {:?}", frame.cell_at(0, 0));
    };
    assert_ne!(fit_px, 0, "fitted");
    let native = Shaper::new().shape_cluster(index.stack(), 18.0, FontStyle::REGULAR, cats);
    let pool = frame.cluster_slice(start, len);
    let expected = native.glyphs[0].advance_px * f32::from(fit_px) / (18.0 * 1.5);
    assert!(
        (pool[0].advance_px - expected).abs() < 1e-3,
        "pool advance {} vs {expected}",
        pool[0].advance_px
    );
    let theme = Theme::default();
    let mut out = RowPaint::default();
    CellPainter::new(
        grid.screen(),
        index.cell_metrics(),
        &ResolvedTheme::new(&theme),
        &index,
        &frame,
    )
    .without_compaction()
    .with_viewport(0, 2)
    .extend_screen(&mut out);
    let [first, second] = out.glyphs.as_slice() else {
        panic!("two cats: {} quads", out.glyphs.len());
    };
    let step = second.origin_px[0] - first.origin_px[0];
    let fitted_advance = native.glyphs[0].advance_px * f32::from(fit_px) / 18.0;
    assert!(
        (step - fitted_advance).abs() < 1e-3,
        "step {step} vs the fitted advance {fitted_advance}"
    );
}
