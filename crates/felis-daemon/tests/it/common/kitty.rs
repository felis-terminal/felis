use felis_daemon::graphics::{
    ApcCtx, ImageEvent, SavedScreens, ShmDeferral, apply_reflow_remap, apply_screen_switch,
    clear_images, dispatch_apc_body, free_unreferenced_anonymous, scroll_region_placements,
};
use felis_grid::images::{ImageStore, Placements};
use felis_grid::kitty_graphics::Reassembler;
use felis_grid::{Grid, PtyEffect, ScreenSwitch};
use felis_vt::Parser;

/// The Kitty graphics half of a `pool::Session`, without a PTY.
pub(crate) struct KittyState {
    pub(crate) parser: Parser,
    pub(crate) reassembler: Reassembler,
    pub(crate) grid: Grid,
    pub(crate) images: ImageStore,
    pub(crate) placements: Placements,
    pub(crate) saved: SavedScreens,
    pub(crate) events: Vec<ImageEvent>,
    pub(crate) shm: ShmDeferral,
    /// `(0, 0)` until a window reports its cell size.
    pub(crate) cell_px: (u16, u16),
}

impl KittyState {
    pub(crate) fn new(rows: u16, cols: u16, store_cap: usize) -> Self {
        Self {
            parser: Parser::new(),
            reassembler: Reassembler::new(),
            grid: Grid::new(rows, cols),
            images: ImageStore::new(store_cap),
            placements: Placements::new(),
            saved: SavedScreens::default(),
            events: Vec::new(),
            shm: ShmDeferral::default(),
            cell_px: (0, 0),
        }
    }

    /// One APC body, anchored at the live cursor.
    pub(crate) fn dispatch(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        self.dispatch_at(None, body)
    }

    fn dispatch_at(&mut self, anchor_cursor: Option<(u16, u16)>, body: &[u8]) -> Option<Vec<u8>> {
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut self.shm,
                cell_pixel_w: self.cell_px.0,
                cell_pixel_h: self.cell_px.1,
                anchor_cursor,
            },
            &mut self.reassembler,
            body,
        )
    }

    /// The image arms of `serve::streaming::replay_effects`, which is
    /// crate-private, in stream order; the replies it would write to the
    /// PTY are dropped.
    pub(crate) fn replay(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
        for effect in self.grid.take_pty_effects() {
            match effect {
                PtyEffect::ImagesCleared(scope) => clear_images(
                    &mut self.images,
                    &mut self.placements,
                    &mut self.events,
                    scope,
                ),
                PtyEffect::Apc(apc) => {
                    self.dispatch_at(Some((apc.cursor_row, apc.cursor_col)), &apc.body);
                }
                PtyEffect::ScrolledIntoScrollback(n) => {
                    let retain = u32::try_from(self.grid.scrollback().len()).unwrap();
                    self.shift(n, retain);
                }
                PtyEffect::AltScreenScrolled(n) => self.shift(n, 0),
                PtyEffect::RegionScrolled { op, into_history } => {
                    let retain = u32::try_from(self.grid.scrollback().len()).unwrap();
                    scroll_region_placements(
                        &mut self.images,
                        &mut self.placements,
                        &mut self.events,
                        (op, into_history),
                        self.cell_px,
                        retain,
                    );
                }
                PtyEffect::HardReset => {
                    apply_screen_switch(
                        ScreenSwitch::LeftAlternate { kept: false },
                        &mut self.images,
                        &mut self.placements,
                        &mut self.saved,
                        &mut self.events,
                    );
                    self.saved.alternate = None;
                }
                PtyEffect::PrimaryReflowed(remap) => apply_reflow_remap(
                    &remap,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.events,
                ),
                PtyEffect::ScreenSwitch(switch) => apply_screen_switch(
                    switch,
                    &mut self.images,
                    &mut self.placements,
                    &mut self.saved,
                    &mut self.events,
                ),
                _ => {}
            }
        }
    }

    /// `serve::streaming::shift_placements`, which is crate-private.
    fn shift(&mut self, n: u32, retain: u32) {
        if !self.placements.is_empty() {
            self.events.push(ImageEvent::PlacementsShifted { lines: n });
        }
        for p in self.placements.shift_up(n, retain) {
            self.images.release(p.image_id);
            self.events.push(ImageEvent::PlacementRemoved {
                image_id: p.image_id,
                placement_id: p.placement_id,
            });
        }
        free_unreferenced_anonymous(&mut self.images, &mut self.placements, &mut self.events);
    }
}
