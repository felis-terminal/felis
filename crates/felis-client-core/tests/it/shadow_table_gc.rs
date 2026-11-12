//! The shadow interns a pen per attribute run and a sizing entry per
//! sized cell of every `RowDelta`, and a window has no session restart
//! to clear them, so `ShadowScreen::apply` must sweep the registries the
//! way the daemon's parse does
//! (docs/explanation/data-model/grid-and-cells.md).

use felis_client_core::ShadowScreen;
use felis_grid::Grid;
use felis_protocol::messages::GridMsg;
use felis_vt::Parser;

use crate::support::encode_rows;

#[test]
fn repainting_one_sized_row_does_not_grow_the_shadow_registries_without_bound() {
    let mut grid = Grid::new(2, 20);
    let mut parser = Parser::default();
    parser.advance(&mut grid, b"\x1b]66;s=2;HEADER\x1b\\");
    let delta = GridMsg::RowDelta {
        rows: encode_rows(&grid, [0]),
    };

    let mut shadow = ShadowScreen::new(2, 20);
    for _ in 0..20_000 {
        shadow.apply(&delta).expect("row delta apply");
    }

    let sizings = shadow.screen().sizing_count();
    assert!(
        sizings < u16::MAX as usize,
        "the registry must not walk to the handle cap, got {sizings}",
    );
    // A sweep that dropped a live handle would leave the header at default size.
    let stamped = (0..20)
        .filter(|c| shadow.screen().cell(0, *c).unwrap().sizing.is_some())
        .count();
    assert!(stamped > 0, "the repainted row must still carry sizings");
    let handle = shadow.screen().cell(0, 0).unwrap().sizing.expect("sized");
    assert_eq!(
        shadow.screen().sizing_by_handle(handle).map(|s| s.scale()),
        Some(2),
        "and resolve to the sizing the daemon sent",
    );
}
