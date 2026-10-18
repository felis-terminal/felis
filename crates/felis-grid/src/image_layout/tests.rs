use std::collections::HashMap;

use felis_protocol::PlacementId;
use felis_vt::Parser;

use super::*;
use crate::{Grid, test_support::drive};

/// Images keyed by id; an id absent here is one still mid-transfer.
#[derive(Default)]
struct Sizes(HashMap<ImageId, (u32, u32)>);

impl Sizes {
    fn with(mut self, id: u32, width: u32, height: u32) -> Self {
        self.0.insert(ImageId(id), (width, height));
        self
    }
}

impl LayoutImages for Sizes {
    fn drawable_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.0.get(&id).copied()
    }
}

fn placed(id: u32, anchor_row: i32, anchor_col: u16, cols: u16, rows: u16) -> ClientPlacement {
    ClientPlacement {
        image_id: ImageId(id),
        placement_id: None,
        anchor_row,
        anchor_col,
        cols,
        rows,
        source: None,
        z_index: 0,
        requested: None,
    }
}

fn with_z(mut p: ClientPlacement, z: i32) -> ClientPlacement {
    p.z_index = z;
    p
}

fn virt(id: u32, cols: u16, rows: u16, z: i32) -> VirtualPlacement {
    VirtualPlacement {
        image_id: ImageId(id),
        placement_id: None,
        anonymous: false,
        cols,
        rows,
        z_index: z,
    }
}

/// A `rows` x `cols` grid whose top-left `cols` x `rows` block is the
/// placeholder cells of image `id` (an id under 256, set through the
/// foreground).
fn placeholder_grid(grid_rows: u16, grid_cols: u16, id: u8, cols: u32, rows: u32) -> Grid {
    use felis_protocol::kitty_graphics::placeholder::{PLACEHOLDER, Rank, diacritic_for};
    let mut grid = Grid::new(grid_rows, grid_cols);
    let mut bytes = format!("\x1b[38;5;{id}m").into_bytes();
    for r in 0..rows {
        for c in 0..cols {
            let mut cell = String::from(PLACEHOLDER);
            cell.push(diacritic_for(Rank::new(r).expect("rank")));
            cell.push(diacritic_for(Rank::new(c).expect("rank")));
            bytes.extend_from_slice(cell.as_bytes());
        }
        bytes.extend_from_slice(b"\r\n");
    }
    drive(&mut Parser::new(), &mut grid, &bytes);
    grid
}

fn tile(tile_row: u32, tile_col: u32, total_cols: u16, total_rows: u16) -> PlaceholderTile {
    PlaceholderTile {
        screen_row: tile_row as u16,
        screen_col: tile_col as u16,
        tile_row,
        tile_col,
        total_cols,
        total_rows,
    }
}

fn assert_close(got: [f32; 4], want: [f32; 4]) {
    let close = got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3);
    assert!(close, "{got:?}, want {want:?}");
}

/// The issue's case: a 101 x 100 image at 10 x 20 px cells.
#[test]
fn a_requested_placement_draws_kitty_s_destination_size() {
    let cases = [
        ((0, 2), (5, 2), [40.4, 40.0]),
        ((3, 0), (3, 2), [30.0, 30.0 * 100.0 / 101.0]),
        ((0, 0), (11, 5), [101.0, 100.0]),
        ((4, 2), (4, 2), [40.0, 40.0]),
    ];
    for ((req_cols, req_rows), (cols, rows), want) in cases {
        let p = ClientPlacement {
            requested: Some(RequestedExtent {
                cols: req_cols,
                rows: req_rows,
            }),
            ..placed(1, 1, 1, cols, rows)
        };
        let draw = draw_for_placement(&p, 101, 100, 10.0, 20.0, 0).expect("draw emits");
        assert_close(draw.dst, [0.0, 0.0, want[0], want[1]]);
    }
}

#[test]
fn explicit_cols_rows_size_the_draw_directly() {
    let draw = draw_for_placement(&placed(1, 3, 5, 4, 2), 32, 32, 8.0, 16.0, 0).expect("draw");
    assert_eq!(draw.dst, [4.0 * 8.0, 2.0 * 16.0, 4.0 * 8.0, 2.0 * 16.0]);
    assert_eq!(draw.src, [0.0, 0.0, 32.0, 32.0]);
    assert_eq!(draw.image_size, [32, 32]);
}

#[test]
fn a_draw_shifts_down_by_viewport_so_images_track_scrollback() {
    let draw = draw_for_placement(&placed(1, 3, 5, 4, 2), 32, 32, 8.0, 16.0, 5).expect("draw");
    assert_eq!(draw.dst, [4.0 * 8.0, 7.0 * 16.0, 4.0 * 8.0, 2.0 * 16.0]);
}

/// Scrollback-anchored placement (docs/reference/protocols/kitty-graphics.md
/// "Scrollback-anchored placements"): browsing back exactly to its line
/// lands the draw at the window top.
#[test]
fn a_scrollback_anchored_draw_surfaces_at_the_matching_viewport() {
    let p = placed(1, -4, 1, 4, 2);
    let draw = draw_for_placement(&p, 32, 32, 8.0, 16.0, 5).expect("draw");
    assert_eq!(draw.dst[..2], [0.0, 0.0]);
    let draw = draw_for_placement(&p, 32, 32, 8.0, 16.0, 4).expect("straddling draw");
    assert_eq!(draw.dst[..2], [0.0, -16.0]);
}

#[test]
fn zero_cols_fall_back_to_the_natural_cell_count() {
    let draw = draw_for_placement(&placed(1, 1, 1, 0, 0), 32, 32, 8.0, 16.0, 0).expect("draw");
    assert_eq!(draw.dst[2..], [4.0 * 8.0, 2.0 * 16.0]);
}

/// Natural sizing arrives resolved to `ceil(px/cell)` cells (cols=4 for
/// 30 px at 8-px cells); the draw stays the native 30 px rather than the
/// 32-px box, or the resample leaves a seam.
#[test]
fn resolved_natural_cells_draw_at_native_pixels_not_stretched() {
    let draw = draw_for_placement(&placed(1, 1, 1, 4, 2), 30, 30, 8.0, 16.0, 0).expect("draw");
    assert_eq!(draw.dst[2..], [30.0, 30.0]);
}

#[test]
fn an_explicit_cell_box_unequal_to_native_still_scales() {
    let draw = draw_for_placement(&placed(1, 1, 1, 10, 5), 30, 30, 8.0, 16.0, 0).expect("draw");
    assert_eq!(draw.dst[2..], [10.0 * 8.0, 5.0 * 16.0]);
}

#[test]
fn a_source_subrect_is_the_draw_s_source() {
    let p = ClientPlacement {
        source: Some(SourceRect {
            x: 8,
            y: 0,
            width: 16,
            height: 16,
        }),
        ..placed(1, 1, 1, 2, 1)
    };
    let draw = draw_for_placement(&p, 32, 32, 8.0, 16.0, 0).expect("sub-rect");
    assert_eq!(draw.src, [8.0, 0.0, 16.0, 16.0]);
}

#[test]
fn a_source_past_the_image_clamps_to_its_bounds() {
    let p = ClientPlacement {
        source: Some(SourceRect {
            x: 24,
            y: 24,
            width: 999,
            height: 999,
        }),
        ..placed(1, 1, 1, 1, 1)
    };
    let draw = draw_for_placement(&p, 32, 32, 8.0, 16.0, 0).expect("clipped sub-rect");
    assert_eq!(draw.src, [24.0, 24.0, 8.0, 8.0]);
}

#[test]
fn a_zero_size_image_draws_nothing() {
    assert!(draw_for_placement(&placed(1, 1, 1, 1, 1), 0, 0, 8.0, 16.0, 0).is_none());
}

#[test]
fn a_placeholder_tile_carves_the_right_image_slice_into_one_cell() {
    let at = PlaceholderTile {
        screen_row: 3,
        screen_col: 5,
        ..tile(1, 2, 4, 2)
    };
    let draw = placeholder_tile_draw(ImageId(1), 32, 32, 8.0, 16.0, at).expect("tile emits");
    assert_eq!(draw.dst, [5.0 * 8.0, 3.0 * 16.0, 8.0, 16.0]);
    assert_close(draw.src, [16.0, 16.0, 8.0, 16.0]);
}

/// A 2:1 image in a 10x10 box of 8x16 cells (80x160 px) fits to the box
/// width and centers vertically: 80x40 px from y=60 to y=100.
#[test]
fn placeholder_tiles_fit_a_wide_image_with_its_aspect_ratio() {
    let at = |row| tile(row, 0, 10, 10);
    let draw = |row| placeholder_tile_draw(ImageId(1), 200, 100, 8.0, 16.0, at(row));
    assert!(draw(2).is_none());
    let top = draw(3).expect("row 3 holds the top edge");
    assert_close(top.dst, [0.0, 60.0, 8.0, 4.0]);
    assert_close(top.src, [0.0, 0.0, 20.0, 10.0]);
    let bottom = draw(6).expect("row 6 holds the bottom edge");
    assert_close(bottom.dst, [0.0, 96.0, 8.0, 4.0]);
    assert!(draw(7).is_none());
}

/// A 1:4 image in a 4x2 box of 8x16 cells (32x32 px) fits to the box
/// height and centers horizontally: 8x32 px from x=12 to x=20.
#[test]
fn placeholder_tiles_center_a_tall_image() {
    let at = |col| PlaceholderTile {
        screen_row: 0,
        tile_row: 0,
        ..tile(0, col, 4, 2)
    };
    let draw = |col| placeholder_tile_draw(ImageId(1), 25, 100, 8.0, 16.0, at(col));
    assert!(draw(0).is_none());
    let left = draw(1).expect("column 1 holds the left edge");
    assert_close(left.dst, [12.0, 0.0, 4.0, 16.0]);
    assert_close(left.src, [0.0, 0.0, 12.5, 50.0]);
    let right = draw(2).expect("column 2 holds the right edge");
    assert_close(right.dst, [16.0, 0.0, 4.0, 16.0]);
    assert_close(right.src, [12.5, 0.0, 12.5, 50.0]);
    assert!(draw(3).is_none());
}

proptest::proptest! {
    /// The tiles of one box stay inside their cells, cover exactly the
    /// fitted image, and sample it at one uniform scale, so the image is
    /// never stretched.
    #[test]
    fn placeholder_tiles_cover_the_fitted_image_at_one_scale(
        width in 1u32..400,
        height in 1u32..400,
        cols in 1u16..12,
        rows in 1u16..12,
        cw in 4u8..20,
        ch in 8u8..40,
    ) {
        let (cw, ch) = (f32::from(cw), f32::from(ch));
        let scale = (f32::from(cols) * cw / width as f32).min(f32::from(rows) * ch / height as f32);
        let mut area = 0.0f64;
        for tile_row in 0..u32::from(rows) {
            for tile_col in 0..u32::from(cols) {
                let at = tile(tile_row, tile_col, cols, rows);
                let Some(d) = placeholder_tile_draw(ImageId(1), width, height, cw, ch, at) else {
                    continue;
                };
                let [x, y, w, h] = d.dst;
                let (cx, cy) = (tile_col as f32 * cw, tile_row as f32 * ch);
                proptest::prop_assert!(x >= cx - 1e-3 && y >= cy - 1e-3);
                proptest::prop_assert!(x + w <= cx + cw + 1e-3);
                proptest::prop_assert!(y + h <= cy + ch + 1e-3);
                proptest::prop_assert!((w / d.src[2] - scale).abs() <= scale * 1e-2);
                proptest::prop_assert!((h / d.src[3] - scale).abs() <= scale * 1e-2);
                area = f64::from(w).mul_add(f64::from(h), area);
            }
        }
        let fitted = f64::from(width as f32 * scale) * f64::from(height as f32 * scale);
        proptest::prop_assert!((area - fitted).abs() <= fitted.mul_add(1e-3, 1e-3));
    }
}

#[test]
fn the_placeholder_extent_uses_c_r_or_falls_back_to_natural() {
    assert_eq!(
        placeholder_grid_extent(40, 11, 353, 221, 9.0, 22.0),
        (40, 11)
    );
    assert_eq!(placeholder_grid_extent(0, 0, 353, 221, 9.0, 22.0), (40, 11));
    assert_eq!(placeholder_grid_extent(5, 0, 90, 44, 9.0, 22.0), (5, 2));
    assert_eq!(placeholder_grid_extent(0, 0, 1, 1, 9.0, 22.0), (1, 1));
}

#[test]
fn a_placeholder_tile_outside_its_box_draws_nothing() {
    let draw = |at| placeholder_tile_draw(ImageId(1), 32, 32, 8.0, 16.0, at);
    assert!(draw(tile(0, 4, 4, 2)).is_none());
    assert!(draw(tile(2, 0, 4, 2)).is_none());
    assert!(draw(tile(0, 0, 0, 2)).is_none());
}

#[test]
fn z_minus_one_lands_above_the_cell_backgrounds() {
    // yazi's direct path places previews at z=-1; that tier paints above
    // the opaque cell backgrounds.
    assert_eq!(z_layer(-1), ZLayer::UnderText);
}

#[test]
fn only_z_below_int_min_half_paints_under_the_backgrounds() {
    assert_eq!(z_layer(Z_UNDER_BG_MAX), ZLayer::UnderText);
    assert_eq!(z_layer(Z_UNDER_BG_MAX - 1), ZLayer::UnderBg);
    assert_eq!(z_layer(i32::MIN), ZLayer::UnderBg);
}

#[test]
fn non_negative_z_paints_above_text() {
    assert_eq!(z_layer(0), ZLayer::AboveText);
    assert_eq!(z_layer(1), ZLayer::AboveText);
    assert_eq!(z_layer(i32::MAX), ZLayer::AboveText);
}

fn ids(draws: &[ImageDraw]) -> Vec<u32> {
    draws.iter().map(|d| d.image_id.0).collect()
}

#[test]
fn each_layer_is_in_ascending_z_with_arrival_order_on_ties() {
    let grid = Grid::new(10, 10);
    let images = Sizes::default()
        .with(1, 8, 16)
        .with(2, 8, 16)
        .with(3, 8, 16)
        .with(4, 8, 16);
    let placements = [
        with_z(placed(1, 1, 1, 1, 1), 5),
        with_z(placed(2, 1, 1, 1, 1), -3),
        with_z(placed(3, 1, 1, 1, 1), 5),
        with_z(placed(4, 1, 1, 1, 1), 0),
    ];
    let mut layout = ImageLayout::new();
    layout.rebuild(grid.screen(), &images, &placements, &[], (8.0, 16.0), 0);
    assert_eq!(ids(layout.above_text()), [4, 1, 3]);
    assert_eq!(ids(layout.under_text()), [2]);
    assert_eq!(layout.under_bg(), []);
}

/// kitty draws placeholder refs at z=-1 whatever `z=` the virtual
/// placement recorded.
#[test]
fn a_placeholder_draws_under_text_whatever_z_its_virtual_placement_recorded() {
    let grid = placeholder_grid(4, 4, 7, 2, 2);
    let images = Sizes::default().with(7, 16, 32);
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &images,
        &[],
        &[virt(7, 2, 2, 10)],
        (8.0, 16.0),
        0,
    );
    assert_eq!(layout.above_text(), []);
    assert_eq!(ids(layout.under_text()), [7, 7, 7, 7]);
}

#[test]
fn a_placeholder_stacks_by_z_against_anchored_placements() {
    let grid = placeholder_grid(4, 4, 7, 1, 1);
    let images = Sizes::default()
        .with(7, 8, 16)
        .with(1, 8, 16)
        .with(2, 8, 16)
        .with(3, 8, 16);
    let placements = [
        with_z(placed(1, 1, 1, 1, 1), 0),
        with_z(placed(2, 1, 1, 1, 1), -1),
        with_z(placed(3, 1, 1, 1, 1), -5),
    ];
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &images,
        &placements,
        &[virt(7, 1, 1, 0)],
        (8.0, 16.0),
        0,
    );
    assert_eq!(ids(layout.under_text()), [3, 2, 7]);
    assert_eq!(ids(layout.above_text()), [1]);
}

/// The client paints a viewport the daemon already composed, so a
/// placeholder sits on the row it shows on while anchored placements
/// shift by the viewport.
#[test]
fn placeholder_tiles_take_no_viewport_offset() {
    let grid = placeholder_grid(4, 4, 7, 1, 1);
    let images = Sizes::default().with(7, 8, 16).with(1, 8, 16);
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &images,
        &[with_z(placed(1, 1, 2, 1, 1), -1)],
        &[virt(7, 1, 1, 0)],
        (8.0, 16.0),
        2,
    );
    let [anchored, tile] = layout.under_text() else {
        panic!("one anchored draw and one tile: {:?}", layout.under_text());
    };
    assert_eq!(anchored.dst[..2], [8.0, 32.0]);
    assert_eq!(tile.dst[..2], [0.0, 0.0]);
}

#[test]
fn an_image_mid_transfer_draws_nothing_but_stays_live() {
    let grid = placeholder_grid(4, 4, 7, 1, 1);
    let images = Sizes::default();
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &images,
        &[placed(1, 1, 1, 1, 1)],
        &[virt(7, 1, 1, 0)],
        (8.0, 16.0),
        0,
    );
    assert!(layout.under_text().is_empty() && layout.above_text().is_empty());
    assert_eq!(layout.live_images(), [ImageId(1), ImageId(7)]);
}

#[test]
fn a_placement_below_the_screen_or_above_it_draws_nothing() {
    let grid = Grid::new(4, 4);
    let images = Sizes::default().with(1, 8, 16).with(2, 8, 16);
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &images,
        &[placed(1, 5, 1, 1, 1), placed(2, -3, 1, 1, 1)],
        &[],
        (8.0, 16.0),
        0,
    );
    assert_eq!(layout.above_text(), []);
}

#[test]
fn live_images_are_every_placed_image_once_in_first_mention_order() {
    let grid = Grid::new(4, 4);
    let mut layout = ImageLayout::new();
    layout.rebuild(
        grid.screen(),
        &Sizes::default(),
        &[
            placed(2, 1, 1, 1, 1),
            placed(1, 1, 1, 1, 1),
            placed(2, 2, 1, 1, 1),
        ],
        &[virt(3, 1, 1, 0), virt(2, 1, 1, 0)],
        (8.0, 16.0),
        0,
    );
    assert_eq!(layout.live_images(), [ImageId(2), ImageId(1), ImageId(3)]);
}

/// The extent decides the tile: a 1x1 box centers the 8x8 image in the
/// cell, a 4x4 box leaves the top-left cell in its margin.
#[test]
fn a_cell_naming_no_placement_takes_the_image_s_first_virtual_placement() {
    let grid = placeholder_grid(4, 4, 7, 1, 1);
    let images = Sizes::default().with(7, 8, 8);
    let named = VirtualPlacement {
        placement_id: Some(PlacementId(9)),
        ..virt(7, 4, 4, 0)
    };
    let first = VirtualPlacement {
        placement_id: Some(PlacementId(3)),
        ..virt(7, 1, 1, 0)
    };
    let mut layout = ImageLayout::new();
    layout.rebuild(grid.screen(), &images, &[], &[first, named], (8.0, 16.0), 0);
    let [tile] = layout.under_text() else {
        panic!("one tile: {:?}", layout.under_text());
    };
    assert_eq!(tile.dst, [0.0, 4.0, 8.0, 8.0]);
}
