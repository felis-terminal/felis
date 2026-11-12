use super::*;

/// Bytes captured from `kitten icat --transfer-mode=stream`. Pins two
/// icat contracts: payloads use Go's `RawStdEncoding` (no base64
/// padding), and `a=a` carries no `q=` while icat never reads
/// responses, so a successful `a=a` must stay silent as kitty does or
/// the OK leaks into the shell as text.
#[test]
fn icat_replay_unpadded_frame_appends_and_a_eq_a_is_silent() {
    let mut r = Reassembler::new();
    let (mut grid, mut images, mut placements) = handler_state();
    let mut events = Vec::new();
    let body_t: &[u8] = "Ga=T,q=2,f=24,o=z,s=220,v=220,I=629321462;eJzswoEMAAAMBDGa85cbwDw+TXoFAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAzPgAA///wJeeX".as_bytes();
    // Payload length 231: not a multiple of 4 (unpadded base64).
    let body_f: &[u8] = "Ga=f,q=2,f=24,o=z,s=220,v=220,c=1,I=629321462,z=80;eJzswoEMAAAMAzCa+yNdYgDzWJo09wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMKMBAAD//87/TU0".as_bytes();
    assert!(
        dispatch_apc_body(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut events,
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 0,
                cell_pixel_h: 0,
                anchor_cursor: None,
            },
            &mut r,
            body_t,
        )
        .is_none(),
        "q=2 suppresses the a=T response",
    );
    assert!(
        images
            .get(
                images
                    .newest_with_number(629_321_462)
                    .expect("icat's number")
            )
            .is_some()
    );
    events.clear();
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        body_f,
    );
    assert!(resp.is_none(), "q=2 suppresses the a=f response");
    let entry = images
        .get(
            images
                .newest_with_number(629_321_462)
                .expect("icat's number"),
        )
        .unwrap();
    assert_eq!(entry.frame_count(), 2, "unpadded a=f payload must decode");
    assert_eq!(entry.frame(1).unwrap().gap_ms, 80);
    assert!(
        materialize_image_events(&events, &images, true)
            .iter()
            .any(|e| matches!(
                e,
                ImageMsg::Header {
                    target: ImageTarget::Frame { number },
                    ..
                } if number.get() == 2
            )),
        "the appended frame ships to attached clients",
    );
    let body_a: &[u8] = b"Ga=a,s=3,v=1,r=1,I=629321462,z=80";
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        body_a,
    );
    assert!(resp.is_none(), "successful a=a must not be acknowledged");
    // a=a against a missing image still reports ENOENT, as kitty does.
    let resp = dispatch_apc_body(
        &mut ApcCtx {
            grid: &mut grid,
            images: &mut images,
            placements: &mut placements,
            events: &mut events,
            shm: &mut ShmDeferral::default(),
            cell_pixel_w: 0,
            cell_pixel_h: 0,
            anchor_cursor: None,
        },
        &mut r,
        b"Ga=a,s=3,i=424242",
    )
    .expect("a=a on an unknown image responds");
    assert!(resp.starts_with(b"\x1b_Gi=424242;ENOENT"), "got: {resp:?}");
}

#[derive(Debug, Clone)]
enum PlacementOp {
    /// `a=p,i=1` with this `p=`, `None` for none.
    Put(Option<u32>),
    /// `a=d,d=i,i=1` (or `d=I` when `free`) with this `p=`.
    Delete { p: Option<u32>, free: bool },
    /// `a=t,i=1` again with new data.
    Retransmit,
}

fn placement_id_choice() -> impl Strategy<Value = Option<u32>> {
    // `u32::MAX` is the first id an id-less put is given.
    proptest::option::of(prop_oneof![Just(0u32), 1u32..4, Just(u32::MAX)])
}

fn placement_op() -> impl Strategy<Value = PlacementOp> {
    prop_oneof![
        6 => placement_id_choice().prop_map(PlacementOp::Put),
        2 => (placement_id_choice(), any::<bool>()).prop_map(|(p, free)| PlacementOp::Delete { p, free }),
        1 => Just(PlacementOp::Retransmit),
    ]
}

proptest! {
    /// kitty's placement identity, against a model: a put replaces only
    /// under a non-zero `p=`, else adds one no `p=` addresses; a delete
    /// without `p=` (or `p=0`) removes all, as does re-transmitting; `d=I`
    /// frees the image once none is left. Replies echo only a non-zero
    /// `p=`; each placement holds one ref on a live image under its id.
    #[test]
    fn placements_follow_kittys_identity_rules_across_puts_deletes_and_retransmits(
        ops in proptest::collection::vec(placement_op(), 0..32),
    ) {
        let (mut grid, mut images, mut placements) = handler_state();
        let (ctrls, payload) = tiny_rgba_controls();
        let mut transmit = vec![(b'a', b"t".as_slice()), (b'i', b"1".as_slice()), (b'q', b"2".as_slice())];
        transmit.extend_from_slice(&ctrls);
        dispatch_controls(&mut grid, &mut images, &mut placements, &transmit, &payload);
        let mut id_less = 0;
        let mut named = std::collections::BTreeSet::new();
        let mut image_live = true;
        for op in ops {
            match op {
                PlacementOp::Put(_) if !image_live => {
                    let put = [(b'a', b"p".as_slice()), (b'i', b"1"), (b'c', b"1"), (b'r', b"1")];
                    let reply = dispatch_controls(&mut grid, &mut images, &mut placements, &put, b"");
                    prop_assert!(reply.is_some_and(|r| r.starts_with(b"\x1b_Gi=1;ENOENT")));
                }
                PlacementOp::Put(p) => {
                    let p_text = p.map(|p| p.to_string());
                    let mut put = vec![(b'a', b"p".as_slice()), (b'i', b"1".as_slice()), (b'c', b"1".as_slice()), (b'r', b"1".as_slice())];
                    if let Some(text) = &p_text {
                        put.push((b'p', text.as_bytes()));
                    }
                    let reply = dispatch_controls(&mut grid, &mut images, &mut placements, &put, b"").expect("q=0 replies");
                    match p {
                        Some(p) if p != 0 => {
                            named.insert(p);
                            prop_assert_eq!(reply, format!("\x1b_Gi=1,p={p};OK\x1b\\").into_bytes());
                        }
                        _ => {
                            id_less += 1;
                            prop_assert_eq!(reply, b"\x1b_Gi=1;OK\x1b\\".to_vec());
                        }
                    }
                }
                PlacementOp::Delete { p, free } => {
                    let p_text = p.map(|p| p.to_string());
                    let d: &[u8] = if free { b"I" } else { b"i" };
                    let mut delete = vec![(b'a', b"d".as_slice()), (b'd', d), (b'i', b"1".as_slice())];
                    if let Some(text) = &p_text {
                        delete.push((b'p', text.as_bytes()));
                    }
                    dispatch_controls(&mut grid, &mut images, &mut placements, &delete, b"");
                    match p {
                        Some(p) if p != 0 => {
                            named.remove(&p);
                        }
                        _ => {
                            id_less = 0;
                            named.clear();
                        }
                    }
                    if free && image_live && id_less + named.len() == 0 {
                        image_live = false;
                    }
                }
                PlacementOp::Retransmit => {
                    dispatch_controls(&mut grid, &mut images, &mut placements, &transmit, &payload);
                    id_less = 0;
                    named.clear();
                    image_live = true;
                }
            }
            prop_assert_eq!(placements.len(), id_less + named.len());
            let ids: std::collections::BTreeSet<_> = placements.iter().map(|p| p.placement_id.map(|id| id.0)).collect();
            prop_assert_eq!(ids.len(), placements.len(), "every placement has its own id");
            let named_live: std::collections::BTreeSet<_> = placements
                .iter()
                .filter(|p| !p.anonymous)
                .filter_map(|p| p.placement_id.map(|id| id.0))
                .collect();
            prop_assert_eq!(&named_live, &named);
            prop_assert_eq!(images.get(ImageId(1)).is_some(), image_live);
            if image_live {
                prop_assert_eq!(images.get(ImageId(1)).unwrap().refcount() as usize, placements.len());
            }
        }
    }
}

#[test]
fn an_image_sent_without_an_id_ignores_p_and_stacks_its_placements() {
    let (mut grid, mut images, mut placements) = handler_state();
    let (ctrls, payload) = tiny_rgba_controls();
    let mut put = vec![
        (b'a', b"T".as_slice()),
        (b'p', b"5".as_slice()),
        (b'q', b"2".as_slice()),
    ];
    put.extend_from_slice(&ctrls);
    dispatch_controls(&mut grid, &mut images, &mut placements, &put, &payload);
    dispatch_controls(&mut grid, &mut images, &mut placements, &put, &payload);
    assert_eq!(placements.len(), 2);
    assert!(
        placements
            .iter()
            .all(|p| p.placement_id != Some(PlacementId(5))),
        "p= is the producer's handle on an image it named"
    );
}

/// One image of the `d=a` world: whether it has a `U=1` extent, and the
/// anchor row and height of each of its placements.
#[derive(Debug, Clone)]
struct ClearImage {
    virtual_placement: bool,
    placements: Vec<(i32, u16)>,
}

fn clear_image() -> impl Strategy<Value = ClearImage> {
    (
        any::<bool>(),
        proptest::collection::vec((-4i32..6, 1u16..4), 0..4),
    )
        .prop_map(|(virtual_placement, placements)| ClearImage {
            virtual_placement,
            placements,
        })
}

proptest! {
    /// kitty's `clear_filter_func_noncell`: `d=a` and `d=A` remove the
    /// placements with a row still on screen and leave virtual ones and
    /// those wholly in history; `d=A` then frees every image left with
    /// no placement of either kind.
    #[test]
    fn delete_all_clears_on_screen_placements_and_keeps_virtual_and_history_ones(
        world in proptest::collection::vec(clear_image(), 1..4),
        free in any::<bool>(),
    ) {
        let (mut grid, mut images, mut placements) = handler_state();
        let (ctrls, payload) = tiny_rgba_controls();
        for (n, image) in world.iter().enumerate() {
            let id = (n + 1).to_string();
            let mut transmit = vec![(b'a', b"t".as_slice()), (b'i', id.as_bytes()), (b'q', b"2".as_slice())];
            transmit.extend_from_slice(&ctrls);
            dispatch_controls(&mut grid, &mut images, &mut placements, &transmit, &payload);
            if image.virtual_placement {
                let put = [(b'a', b"p".as_slice()), (b'U', b"1"), (b'i', id.as_bytes()), (b'c', b"1"), (b'r', b"1"), (b'q', b"2")];
                dispatch_controls(&mut grid, &mut images, &mut placements, &put, b"");
            }
            for (k, &(row, rows)) in image.placements.iter().enumerate() {
                let image_id = ImageId(u32::try_from(n + 1).unwrap());
                prop_assert!(images.retain(image_id));
                let replaced = placements.upsert(Placement {
                    image_id,
                    placement_id: Some(PlacementId(u32::try_from(k + 1).unwrap())),
                    anchor: CellPos { row, col: 1 },
                    cols: Extent::Requested(NonZeroU16::MIN),
                    rows: Extent::Requested(NonZeroU16::new(rows).unwrap()),
                    source: None,
                    z_index: 0,
                    no_cursor_move: true,
                    quiet: 2,
                    anonymous: false,
                    serial: 0,
                });
                prop_assert!(replaced.is_none());
            }
        }
        let d: &[u8] = if free { b"A" } else { b"a" };
        let mut events = Vec::new();
        handle_complete(
            &mut ApcCtx {
                grid: &mut grid,
                images: &mut images,
                placements: &mut placements,
                events: &mut events,
                shm: &mut ShmDeferral::default(),
                cell_pixel_w: 8,
                cell_pixel_h: 16,
                anchor_cursor: Some((0, 0)),
            },
            complete(&[(b'a', b"d"), (b'd', d)], b""),
        );
        let mut removed: Vec<(u32, u32)> = Vec::new();
        let mut deleted: Vec<u32> = Vec::new();
        for event in &events {
            match event {
                ImageEvent::PlacementRemoved { image_id, placement_id } => {
                    removed.push((image_id.0, placement_id.unwrap().0));
                }
                ImageEvent::Delete { id } => deleted.push(id.0),
                other => return Err(TestCaseError::fail(format!("unexpected event {other:?}"))),
            }
        }
        removed.sort_unstable();
        deleted.sort_unstable();
        let mut want_removed: Vec<(u32, u32)> = Vec::new();
        let mut want_deleted: Vec<u32> = Vec::new();
        for (n, image) in world.iter().enumerate() {
            let id = u32::try_from(n + 1).unwrap();
            let mut kept = false;
            for (k, &(row, rows)) in image.placements.iter().enumerate() {
                if row + i32::from(rows) > 1 {
                    want_removed.push((id, u32::try_from(k + 1).unwrap()));
                } else {
                    kept = true;
                }
            }
            if free && !kept && !image.virtual_placement {
                want_deleted.push(id);
            }
        }
        prop_assert_eq!(removed, want_removed);
        prop_assert_eq!(deleted, want_deleted);
        for (n, image) in world.iter().enumerate() {
            let image_id = ImageId(u32::try_from(n + 1).unwrap());
            let kept: Vec<i32> = image
                .placements
                .iter()
                .filter(|&&(row, rows)| row + i32::from(rows) <= 1)
                .map(|&(row, _)| row)
                .collect();
            let mut left: Vec<i32> = placements.for_image(image_id).map(|p| p.anchor.row).collect();
            left.sort_unstable();
            let mut want = kept.clone();
            want.sort_unstable();
            prop_assert_eq!(left, want, "image {}", n + 1);
            prop_assert_eq!(
                placements.iter_virtual().any(|v| v.image_id == image_id),
                image.virtual_placement,
            );
            let stored = !free || image.virtual_placement || !kept.is_empty();
            prop_assert_eq!(images.get(image_id).is_some(), stored, "image {} stored", n + 1);
            if let Some(entry) = images.get(image_id) {
                prop_assert_eq!(entry.refcount() as usize, kept.len());
            }
        }
    }
}
