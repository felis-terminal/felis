use super::*;

/// A session's graphics state driven by raw APC bodies and text, settled
/// as a drain settles it.
struct Rig {
    parser: Parser,
    grid: felis_grid::Grid,
    images: felis_grid::images::ImageStore,
    placements: felis_grid::images::Placements,
    events: Vec<ImageEvent>,
    reassembler: Reassembler,
}

impl Rig {
    fn new() -> Self {
        let (grid, images, placements) = handler_state();
        Self {
            parser: Parser::new(),
            grid,
            images,
            placements,
            events: Vec::new(),
            reassembler: Reassembler::new(),
        }
    }

    /// `controls` over a 1×1 RGB payload, answered as text.
    fn send(&mut self, controls: &str) -> Option<String> {
        let body = format!("G{controls};AAAA");
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut self.grid,
                images: &mut self.images,
                placements: &mut self.placements,
                events: &mut self.events,
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 8,
                cell_pixel_h: 16,
                anchor_cursor: None,
            },
            &mut self.reassembler,
            body.as_bytes(),
        )
        .map(|reply| String::from_utf8(reply).unwrap())
    }

    fn print(&mut self, text: &str) {
        self.parser.advance(&mut self.grid, text.as_bytes());
        for effect in self.grid.take_pty_effects() {
            match effect {
                felis_grid::PtyEffect::ImagesCleared(scope) => {
                    clear_images(
                        &mut self.images,
                        &mut self.placements,
                        &mut self.events,
                        scope,
                    );
                }
                felis_grid::PtyEffect::ScrolledIntoScrollback(n) => {
                    self.placements.shift_up(n, 100);
                }
                felis_grid::PtyEffect::RegionScrolled { op, into_history } => {
                    scroll_region_placements(
                        &mut self.images,
                        &mut self.placements,
                        &mut self.events,
                        (op, into_history),
                        (8, 16),
                        100,
                    );
                }
                _ => {}
            }
        }
        settle_relatives(
            &self.grid,
            &mut self.images,
            &mut self.placements,
            &mut self.events,
            (8, 16),
        );
    }

    fn placement(&self, image: u32) -> &Placement {
        self.placements
            .for_image(ImageId(image))
            .next()
            .expect("the image has a placement")
    }

    fn anchor(&self, image: u32) -> (i32, u16) {
        let p = self.placement(image);
        (p.anchor.row, p.anchor.col)
    }

    /// The last anchor the client was sent for the image's placement,
    /// `None` when it was last told to drop it.
    fn mirrored(&self, image: u32) -> Option<(i32, u16)> {
        self.events.iter().rev().find_map(|e| match *e {
            ImageEvent::Placement {
                image_id,
                anchor_row,
                anchor_col,
                ..
            } if image_id.0 == image => Some(Some((anchor_row, anchor_col))),
            ImageEvent::PlacementRemoved { image_id, .. } if image_id.0 == image => Some(None),
            _ => None,
        })?
    }
}

fn reply(id: u32, status: &str) -> String {
    format!("\x1b_Gi={id};{status}\x1b\\")
}

fn reply_p(id: u32, p: u32, status: &str) -> String {
    format!("\x1b_Gi={id},p={p};{status}\x1b\\")
}

#[test]
fn a_parent_that_does_not_exist_is_answered_enoparent() {
    let mut rig = Rig::new();
    rig.send("a=t,i=1,s=1,v=1,f=24,q=1");
    rig.send("a=t,i=2,s=1,v=1,f=24,q=1");
    assert_eq!(
        rig.send("a=p,i=1,p=1,P=9,Q=1"),
        Some(reply_p(1, 1, "ENOPARENT:parent placement not found")),
        "no image 9",
    );
    assert_eq!(
        rig.send("a=p,i=1,p=1,P=2"),
        Some(reply_p(1, 1, "ENOPARENT:parent placement not found")),
        "image 2 has no placement",
    );
    rig.send("a=p,i=2,p=1,q=1");
    assert_eq!(
        rig.send("a=p,i=1,p=1,P=2,Q=7"),
        Some(reply_p(1, 1, "ENOPARENT:parent placement not found")),
        "image 2 has no placement 7",
    );
    assert_eq!(rig.placements.for_image(ImageId(1)).count(), 0);
}

#[test]
fn a_relative_placement_sits_at_the_parents_top_left_cell_offset_by_h_and_v() {
    let mut rig = Rig::new();
    rig.print("\x1b[3;5H");
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,c=2,r=2,C=1,q=1");
    rig.print("\x1b[10;10H");
    assert_eq!(
        rig.send("a=T,i=2,p=1,P=1,Q=1,H=2,V=-1,s=1,v=1,f=24,c=2,r=2"),
        Some(reply_p(2, 1, "OK")),
    );
    assert_eq!(rig.anchor(2), (2, 7));
    assert_eq!(rig.mirrored(2), Some((2, 7)));
    let cursor = rig.grid.cursor();
    assert_eq!(
        (cursor.row, cursor.col),
        (9, 9),
        "a relative placement never moves the cursor",
    );
    rig.print("\x1b[6;1H");
    rig.send("a=p,i=1,p=1,C=1,q=2");
    assert_eq!(rig.anchor(2), (5, 3), "it follows its parent");
    assert_eq!(rig.mirrored(2), Some((5, 3)));
    rig.send("a=p,i=1,p=2,C=1,q=2");
    assert_eq!(
        rig.send("a=p,i=2,p=1,P=1,H=-5"),
        Some(reply_p(2, 1, "OK")),
        "Q=0 takes the parent image's oldest placement",
    );
    assert_eq!(
        rig.anchor(2),
        (6, 1),
        "a column left of the screen pins to the first"
    );
}

#[test]
fn a_put_that_would_close_a_cycle_is_answered_ecycle() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    rig.send("a=T,i=2,p=1,P=1,Q=1,s=1,v=1,f=24,q=1");
    rig.send("a=T,i=3,p=1,P=2,Q=1,s=1,v=1,f=24,q=1");
    assert_eq!(
        rig.send("a=p,i=1,p=1,P=3,Q=1"),
        Some(reply_p(1, 1, "ECYCLE:parent chain would form a cycle")),
    );
    assert_eq!(
        rig.send("a=p,i=1,p=1,P=1,Q=1"),
        Some(reply_p(1, 1, "EINVAL:placement is its own parent")),
    );
    assert!(
        rig.placement(1).parent.is_none(),
        "a refused put leaves the placement as it was"
    );
}

#[test]
fn a_chain_may_take_eight_links_and_no_more() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    for id in 2..=9 {
        assert_eq!(
            rig.send(&format!("a=T,i={id},p=1,P={},Q=1,s=1,v=1,f=24", id - 1)),
            Some(reply_p(id, 1, "OK")),
        );
    }
    assert_eq!(
        rig.send("a=T,i=10,p=1,P=9,Q=1,s=1,v=1,f=24"),
        Some(reply_p(10, 1, "ETOODEEP:parent chain too deep")),
    );
}

#[test]
fn a_virtual_placement_made_relative_is_answered_einval() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    rig.send("a=t,i=2,s=1,v=1,f=24,q=1");
    assert_eq!(
        rig.send("a=p,i=2,p=1,U=1,P=1,Q=1"),
        Some(reply_p(
            2,
            1,
            "EINVAL:a virtual placement cannot be relative"
        )),
    );
    assert_eq!(
        rig.send("a=p,i=9,U=1,P=1"),
        Some(reply(9, "EINVAL:a virtual placement cannot be relative")),
        "refused before the image is looked up",
    );
    assert!(!rig.placements.has_virtual(ImageId(2)));
}

#[test]
fn deleting_the_parent_deletes_its_relative_placements_and_their_unplaced_images() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    rig.send("a=T,i=2,p=1,P=1,Q=1,s=1,v=1,f=24,q=1");
    rig.send("a=T,i=3,p=1,P=2,Q=1,s=1,v=1,f=24,q=1");
    rig.send("a=T,i=4,p=1,P=1,Q=1,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=4,p=2,C=1,q=1");
    rig.send("a=d,d=i,i=1,q=2");
    assert!(rig.images.get(ImageId(1)).is_some(), "d=i keeps its data");
    assert!(rig.images.get(ImageId(2)).is_none());
    assert!(rig.images.get(ImageId(3)).is_none(), "the chain goes whole");
    assert_eq!(
        rig.send("a=p,i=2"),
        Some(reply(2, "ENOENT:image not in store"))
    );
    assert!(
        rig.images.get(ImageId(4)).is_some(),
        "an image another placement shows stays",
    );
    assert_eq!(rig.placements.for_image(ImageId(4)).count(), 1);
}

#[test]
fn a_placement_put_again_under_the_parents_key_is_not_the_parent() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    rig.send("a=T,i=2,p=1,P=1,Q=1,s=1,v=1,f=24,q=1");
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    assert!(
        rig.images.get(ImageId(2)).is_none(),
        "transmitting the parent's image again deleted the parent",
    );
}

#[test]
fn a_relative_placement_scrolls_with_its_parent() {
    let mut rig = Rig::new();
    rig.print("\x1b[2;1H");
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=1");
    rig.send("a=T,i=2,p=1,P=1,Q=1,V=2,s=1,v=1,f=24,q=1");
    rig.print("\x1b[24;1H\n\n");
    assert_eq!(rig.anchor(1), (0, 1));
    assert_eq!(rig.anchor(2), (2, 1));
    rig.print("\x1b[2;10r\x1b[10;1H\n");
    assert_eq!(
        rig.anchor(2),
        (2, 1),
        "stays put while the band leaves its parent"
    );
    rig.print("\x1b[r\x1b[6;1H");
    rig.send("a=p,i=1,p=1,C=1,q=2");
    rig.print("\x1b[2;10r\x1b[10;1H\n");
    assert_eq!(rig.anchor(1), (5, 1));
    assert_eq!(rig.anchor(2), (7, 1), "moves with the parent the band took");
}

#[test]
fn a_virtual_parent_places_its_relatives_at_its_placeholder_cells() {
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,p=1,U=1,c=2,r=2,q=1");
    rig.send("a=t,i=6,s=1,v=1,f=24,q=1");
    assert_eq!(
        rig.send("a=p,i=6,p=1,P=5,Q=1,H=1,V=1"),
        Some(reply_p(6, 1, "OK")),
    );
    assert!(
        !rig.placement(6).is_shown(),
        "nothing draws while no placeholder cell is on the screen",
    );
    assert_eq!(rig.mirrored(6), None);
    rig.print("\x1b[4;6H\x1b[38;5;5m\u{10EEEE}\u{10EEEE}\x1b[5;3H\u{10EEEE}\x1b[39m");
    assert_eq!(rig.anchor(6), (5, 4), "the least row and the least column");
    assert_eq!(rig.mirrored(6), Some((5, 4)));
    rig.print("\x1b[5;1H\x1b[2K");
    assert_eq!(rig.anchor(6), (5, 7));
    rig.print("\x1b[4;1H\x1b[2K");
    assert!(!rig.placement(6).is_shown());
    assert_eq!(rig.mirrored(6), None);
    rig.print("\x1b[1;1H\x1b[38;5;5m\u{10EEEE}\x1b[39m\x1b[2J");
    assert_eq!(
        rig.placements.for_image(ImageId(6)).count(),
        0,
        "a screen clear takes what the screen's placeholders place",
    );
    assert!(rig.placements.has_virtual(ImageId(5)));
}

#[test]
fn a_parent_moved_off_its_anonymous_id_keeps_its_relative_placements() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,s=1,v=1,f=24,C=1,q=2");
    rig.send("a=T,i=2,p=1,P=1,H=1,s=1,v=1,f=24,q=2");
    rig.send(&format!("a=p,i=1,p={},C=1,q=2", u32::MAX));
    assert_eq!(rig.placements.for_image(ImageId(1)).count(), 2);
    assert_eq!(
        rig.placements.for_image(ImageId(2)).count(),
        1,
        "the anonymous parent moved to a fresh id, its relative with it",
    );
}

#[test]
fn a_hidden_placement_moved_off_its_anonymous_id_stays_undrawn() {
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,p=1,U=1,c=1,r=1,q=1");
    rig.send("a=T,i=6,P=5,Q=1,s=1,v=1,f=24,q=2");
    rig.events.clear();
    rig.send(&format!("a=p,i=6,p={},C=1,q=2", u32::MAX));
    let drawn: Vec<_> = rig
        .events
        .iter()
        .filter_map(|e| match e {
            ImageEvent::Placement { placement_id, .. } => *placement_id,
            _ => None,
        })
        .collect();
    assert_eq!(drawn, vec![PlacementId(u32::MAX)]);
}

#[test]
fn a_put_may_hang_off_the_anonymous_placement_its_id_moves() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,s=1,v=1,f=24,C=1,q=2");
    assert_eq!(
        rig.send(&format!("a=p,i=1,p={},P=1,V=1", u32::MAX)),
        Some(reply_p(1, u32::MAX, "OK")),
    );
    assert_eq!(rig.placements.for_image(ImageId(1)).count(), 2);
}

#[test]
fn q_0_takes_the_first_placement_put_even_after_it_moved_off_its_id() {
    let mut rig = Rig::new();
    rig.print("\x1b[2;1H");
    rig.send("a=T,i=1,s=1,v=1,f=24,C=1,q=2");
    rig.print("\x1b[7;1H");
    rig.send("a=p,i=1,p=3,C=1,q=2");
    rig.send(&format!("a=p,i=1,p={},C=1,q=2", u32::MAX));
    rig.send("a=T,i=2,p=1,P=1,s=1,v=1,f=24,q=2");
    assert_eq!(rig.anchor(2), (2, 1));
}

#[test]
fn a_screen_clear_takes_a_placement_offset_to_the_last_row_there_is() {
    let mut rig = Rig::new();
    rig.send("a=T,i=1,p=1,s=1,v=1,f=24,C=1,q=2");
    rig.send(&format!(
        "a=T,i=2,p=1,P=1,V={},s=1,v=1,f=24,r=2,q=2",
        i32::MAX
    ));
    rig.print("\x1b[2J");
    assert!(rig.placements.is_empty());
}

#[test]
fn a_named_virtual_parent_follows_the_cells_that_name_it() {
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,U=1,c=1,r=1,q=2");
    rig.send("a=p,i=5,p=2,U=1,c=1,r=1,q=2");
    rig.send("a=T,i=6,p=1,P=5,Q=2,s=1,v=1,f=24,q=2");
    rig.print("\x1b[3;4H\x1b[38;5;5;58;5;2m\u{10EEEE}\x1b[39;59m");
    assert_eq!(rig.mirrored(6), Some((3, 4)));
}

#[test]
fn a_placeholder_tile_outside_the_parents_box_does_not_place_it() {
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,p=1,U=1,c=1,r=1,q=2");
    rig.send("a=T,i=6,p=1,P=5,s=1,v=1,f=24,q=2");
    let below = felis_protocol::kitty_graphics::placeholder::diacritic_for(
        felis_protocol::kitty_graphics::placeholder::Rank::new(1).unwrap(),
    );
    rig.print(&format!("\x1b[3;4H\x1b[38;5;5m\u{10EEEE}{below}\x1b[39m"));
    assert!(!rig.placement(6).is_shown());
}

#[test]
fn a_refused_relative_put_leaves_the_anonymous_placement_under_its_id() {
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,U=1,c=1,r=1,q=2");
    assert_eq!(
        rig.send(&format!("a=p,i=5,p={},P=9", u32::MAX)),
        Some(reply_p(5, u32::MAX, "ENOPARENT:parent placement not found")),
    );
    let held = rig
        .placements
        .virtual_for_cell(ImageId(5), u32::MAX)
        .expect("the virtual placement stays");
    assert_eq!(held.placement_id, Some(PlacementId(u32::MAX)));
}

#[test]
fn a_placeholder_tile_past_an_omitted_extent_does_not_place_the_parent() {
    use felis_protocol::kitty_graphics::placeholder::{Rank, diacritic_for};
    let mut rig = Rig::new();
    rig.send("a=t,i=5,s=1,v=1,f=24,q=1");
    rig.send("a=p,i=5,p=1,U=1,q=2");
    rig.send("a=T,i=6,p=1,P=5,s=1,v=1,f=24,q=2");
    let rank = |r| diacritic_for(Rank::new(r).unwrap());
    rig.print(&format!(
        "\x1b[3;4H\x1b[38;5;5m\u{10EEEE}{}{}\x1b[39m",
        rank(0),
        rank(5)
    ));
    assert!(!rig.placement(6).is_shown());
    rig.print(&format!(
        "\x1b[4;4H\x1b[38;5;5m\u{10EEEE}{}{}\x1b[39m",
        rank(0),
        rank(0)
    ));
    assert_eq!(rig.mirrored(6), Some((4, 4)));
}
