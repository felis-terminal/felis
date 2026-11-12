use super::*;

fn frame_of(size: usize) -> Frame {
    Frame {
        pixels: vec![0u8; size].into(),
        gap_ms: 40,
    }
}

fn frame_charge(size: usize) -> usize {
    frame_of(size).byte_len()
}

#[cfg(feature = "state-dump")]
type StoreEdit<'a> = &'a dyn Fn(&mut ImageStore);

#[cfg(feature = "state-dump")]
#[test]
fn a_restored_store_must_hold_well_formed_frames_and_charge_exactly_its_total() {
    let full = || {
        let mut s = ImageStore::new(charge(3));
        s.insert(ImageId(1), entry(3)).unwrap();
        s
    };
    let refusal = |edit: StoreEdit<'_>| {
        let mut s = full();
        edit(&mut s);
        s.check_restored().err().map(|err| err.0)
    };
    let with_entry = |edit: fn(&mut ImageEntry)| {
        move |s: &mut ImageStore| edit(s.entries.get_mut(ImageId(1)).unwrap())
    };
    assert_eq!(refusal(&|_| {}), None, "a store charged exactly to its cap");

    let frames = "frames of image 1";
    let refused: [(&str, StoreEdit<'_>); 6] = [
        (frames, &with_entry(|e| e.frames.clear())),
        (frames, &with_entry(|e| e.current = e.frames.len())),
        (frames, &with_entry(|e| e.frames[0] = frame_of(4))),
        (
            frames,
            &with_entry(|e| e.frames.resize(MAX_IMAGE_FRAMES + 1, frame_of(3))),
        ),
        ("image store byte total", &|s| s.bytes_total += 1),
        ("image store byte total", &|s| {
            s.bytes_cap -= 1;
        }),
    ];
    for (expected, edit) in refused {
        let err = refusal(edit).unwrap_or_else(|| panic!("{expected:?} was accepted"));
        assert_eq!(err, expected);
    }
}

#[test]
fn fresh_store_is_empty() {
    let s = ImageStore::new(1024);
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.bytes_used(), 0);
    assert_eq!(s.bytes_cap(), 1024);
    assert!(s.get(ImageId(1)).is_none());
}

#[test]
fn insert_and_get_round_trips() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(7), entry(64)).unwrap();
    let got = s.get(ImageId(7)).unwrap();
    assert_eq!(got.byte_len(), charge(64));
    assert_eq!(got.refcount(), 0);
    assert_eq!(s.bytes_used(), charge(64));
}

#[test]
fn insert_replacing_same_id_overwrites_and_keeps_refcount_reset() {
    // Producers re-transmit under the same id; the dispatcher
    // invalidates existing placements, so the refcount resets.
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(100)).unwrap();
    assert!(s.retain(ImageId(1)));
    assert_eq!(s.get(ImageId(1)).unwrap().refcount(), 1);

    s.insert(ImageId(1), entry(50)).unwrap();
    let after = s.get(ImageId(1)).unwrap();
    assert_eq!(after.byte_len(), charge(50));
    assert_eq!(after.refcount(), 0);
    assert_eq!(s.bytes_used(), charge(50));
}

#[test]
fn retain_and_release_track_refcount() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(2), entry(10)).unwrap();
    assert!(s.retain(ImageId(2)));
    assert!(s.retain(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 2);
    assert!(s.release(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 1);
    assert!(s.release(ImageId(2)));
    assert_eq!(s.get(ImageId(2)).unwrap().refcount(), 0);
    // Releasing past zero is a logic bug: `false` lets the dispatcher
    // log instead of silently underflowing.
    assert!(!s.release(ImageId(2)));
}

#[test]
fn retain_and_release_on_unknown_id_return_false() {
    let mut s = ImageStore::new(1024);
    assert!(!s.retain(ImageId(99)));
    assert!(!s.release(ImageId(99)));
}

#[test]
fn delete_removes_entry_and_frees_bytes() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(3), entry(80)).unwrap();
    assert_eq!(s.bytes_used(), charge(80));
    let removed = s.delete(ImageId(3)).unwrap();
    assert_eq!(removed.byte_len(), charge(80));
    assert!(s.is_empty());
    assert_eq!(s.bytes_used(), 0);
    assert!(s.delete(ImageId(3)).is_none());
}

#[test]
fn delete_removes_pinned_entries_too() {
    // a=d is the producer's explicit "drop this"; pin status does not
    // save it.
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(4), entry(20)).unwrap();
    s.retain(ImageId(4));
    assert!(s.delete(ImageId(4)).is_some());
    assert!(s.is_empty());
}

#[test]
fn insert_larger_than_cap_is_rejected() {
    let mut s = ImageStore::new(64);
    let err = s.insert(ImageId(1), entry(128)).unwrap_err();
    let InsertError::OverCapacity { wanted, cap } = err;
    assert_eq!(wanted, charge(128));
    assert_eq!(cap, 64);
    assert!(s.is_empty());
}

#[test]
fn insert_evicts_oldest_unpinned_entries_to_fit() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    s.insert(ImageId(3), entry(40)).unwrap();
    assert!(s.get(ImageId(1)).is_none());
    assert!(s.get(ImageId(2)).is_some());
    assert!(s.get(ImageId(3)).is_some());
    assert_eq!(s.bytes_used(), 2 * charge(40));
}

#[test]
fn pinned_entries_are_skipped_during_eviction() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(40)).unwrap();
    // id=1 is pinned, so id=2 goes even though it is newer.
    s.insert(ImageId(3), entry(40)).unwrap();
    assert!(s.get(ImageId(1)).is_some());
    assert!(s.get(ImageId(2)).is_none());
    assert!(s.get(ImageId(3)).is_some());
}

#[test]
fn insert_fails_when_all_entries_are_pinned() {
    // The store cannot make room without dropping live data, so the
    // dispatcher gets OverCapacity (ENOTSUP) rather than an
    // evicted placed image.
    let mut s = ImageStore::new(2 * charge(50));
    s.insert(ImageId(1), entry(50)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(50)).unwrap();
    s.retain(ImageId(2));
    let err = s.insert(ImageId(3), entry(40)).unwrap_err();
    assert!(matches!(err, InsertError::OverCapacity { .. }));
    assert!(s.get(ImageId(1)).is_some());
    assert!(s.get(ImageId(2)).is_some());
    assert!(s.get(ImageId(3)).is_none());
}

#[test]
fn a_refused_replacement_leaves_the_old_entry_in_the_store() {
    // Both entries are pinned, so the replacement cannot be made to
    // fit. The store must come out of the refusal unchanged: id=1 is
    // still there, at its old size, and still counted.
    let mut s = ImageStore::new(charge(60) + charge(40));
    s.insert(ImageId(1), entry(60)).unwrap();
    s.retain(ImageId(1));
    s.insert(ImageId(2), entry(40)).unwrap();
    s.retain(ImageId(2));

    let err = s.insert(ImageId(1), entry(100)).unwrap_err();

    assert!(matches!(err, InsertError::OverCapacity { .. }));
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(60));
    assert_eq!(s.bytes_used(), charge(60) + charge(40));
}

/// The frame cap is a count, so the byte cap says nothing about it:
/// this store has room for orders of magnitude more four-byte frames
/// than the cap admits. The refusal must land on the frame that would
/// be the 4097th, and leave the image whole.
#[test]
fn the_frame_cap_admits_its_last_frame_and_refuses_the_next() {
    let mut s = ImageStore::new(usize::MAX);
    s.insert(ImageId(1), entry(4)).unwrap();
    // The root counts toward the cap, so the pushes stop one short.
    for _ in 1..MAX_IMAGE_FRAMES {
        s.push_frame(ImageId(1), frame_of(4)).unwrap();
    }
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), MAX_IMAGE_FRAMES);

    let err = s.push_frame(ImageId(1), frame_of(4)).unwrap_err();

    assert_eq!(
        err,
        FrameError::TooManyFrames {
            cap: MAX_IMAGE_FRAMES
        }
    );
    assert_eq!(
        s.get(ImageId(1)).unwrap().frame_count(),
        MAX_IMAGE_FRAMES,
        "a refusal must not have grown the image",
    );
}

#[test]
fn bytes_used_tracks_inserts_replaces_and_deletes() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(100)).unwrap();
    s.insert(ImageId(2), entry(50)).unwrap();
    assert_eq!(s.bytes_used(), charge(100) + charge(50));
    s.insert(ImageId(1), entry(80)).unwrap(); // replace
    assert_eq!(s.bytes_used(), charge(80) + charge(50));
    s.delete(ImageId(2));
    assert_eq!(s.bytes_used(), charge(80));
}

#[test]
fn replacement_evicts_other_entries_if_needed() {
    // Existing id=1 is dropped first, but the replacement still does not
    // fit beside id=2, so id=2 gives up its slot.
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    s.insert(ImageId(1), entry(70)).unwrap();
    assert!(s.get(ImageId(1)).is_some());
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(70));
    assert!(s.get(ImageId(2)).is_none());
}

// (docs/reference/protocols/kitty-graphics.md "Animation")

/// Each frame's pixel buffer is one byte so byte-accounting deltas are
/// easy to assert.
fn animated(gaps: &[u32]) -> ImageEntry {
    let mut e = ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0u8]);
    e.set_gap(0, gaps[0]);
    for &g in &gaps[1..] {
        e.frames.push(Frame {
            pixels: vec![0u8].into(),
            gap_ms: g,
        });
    }
    e
}

#[test]
fn new_entry_is_a_single_frame_still() {
    let e = ImageEntry::new(2, 2, ImageFormat::Rgba32, vec![0u8; 16]);
    assert_eq!(e.frame_count(), 1);
    assert!(!e.is_animated());
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.byte_len(), charge(16));
    let mut e = e;
    assert_eq!(e.advance(1_000_000), None);
}

#[test]
fn push_frame_grows_byte_total_and_evicts_others() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    assert_eq!(s.bytes_used(), 2 * charge(40));
    // Image 2 (refcount 0) is evicted; image 1 is never evicted to fit
    // its own frame.
    let pushed = s.push_frame(ImageId(1), frame_of(40)).unwrap();
    assert_eq!(pushed.index, 1);
    assert!(
        s.get(ImageId(2)).is_none(),
        "image 2 should have been evicted"
    );
    assert_eq!(
        pushed.evicted,
        vec![ImageId(2)],
        "an eviction the caller cannot see is one it cannot announce",
    );
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), 2);
    assert_eq!(s.bytes_used(), charge(40) + frame_charge(40));
}

#[test]
fn push_frame_rejects_when_pinned_image_blocks_eviction() {
    let mut s = ImageStore::new(charge(60));
    s.insert(ImageId(1), entry(60)).unwrap();
    s.retain(ImageId(1));
    // No other image to evict and the pinned one cannot shrink.
    let err = s.push_frame(ImageId(1), frame_of(60)).unwrap_err();
    assert!(matches!(err, FrameError::OverCapacity { .. }));
    assert_eq!(s.get(ImageId(1)).unwrap().frame_count(), 1);
    assert_eq!(s.bytes_used(), charge(60));
}

/// Everything reachable through [`ImageStore::animation_control`] is a
/// display knob; the store charges when pixels enter, and nothing here
/// moves pixels.
#[test]
fn animation_control_never_moves_the_stores_byte_total() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    let before = s.bytes_used();

    let mut ctl = s.animation_control(ImageId(1)).unwrap();
    ctl.set_gap(0, 50);
    ctl.set_gap(1, 60);
    ctl.set_mode(AnimationMode::Running);
    ctl.set_max_loops(3);
    assert!(ctl.jump_to(1));

    assert_eq!(s.bytes_used(), before);
}

#[test]
fn remove_frame_keeps_current_in_range_and_frees_bytes() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    s.push_frame(ImageId(1), frame_of(30)).unwrap();
    // Removing a frame before the displayed one shifts `current` down
    // so it still points at the same logical frame.
    s.animation_control(ImageId(1)).unwrap().jump_to(2);
    let freed = s.remove_frame(ImageId(1), 1).unwrap();
    assert_eq!(freed, frame_charge(20));
    let e = s.get(ImageId(1)).unwrap();
    assert_eq!(e.frame_count(), 2);
    assert_eq!(e.current_frame(), 1);
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(30));
}

#[test]
fn remove_last_frame_is_refused() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    // The store never holds a zero-frame entry; whole-image delete is
    // the caller's job.
    assert_eq!(s.remove_frame(ImageId(1), 0), Err(FrameError::NoSuchFrame));
}

#[test]
fn running_animation_advances_one_frame_per_gap() {
    let mut e = animated(&[100, 100, 100]);
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.advance(50), None);
    assert_eq!(e.advance(100), Some(1));
    assert_eq!(e.advance(200), Some(2));
}

#[test]
fn running_animation_loops_to_root_and_counts_loops() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Running);
    e.set_max_loops(2); // play through twice, then stop
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(1)); // -> frame 1
    assert_eq!(e.advance(20), Some(0)); // wrap -> loop 1, back to root
    assert_eq!(e.advance(30), Some(1)); // -> frame 1
    assert_eq!(e.advance(40), None);
    assert_eq!(e.current_frame(), 1);
}

#[test]
fn loading_mode_halts_on_last_frame() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Loading);
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(1)); // -> last frame
    // Loading waits at the last frame for more frames.
    assert_eq!(e.advance(20), None);
    assert_eq!(e.current_frame(), 1);
}

#[test]
fn gapless_frames_are_skipped() {
    // Frame 1 is gapless (a compositing base).
    let mut e = animated(&[10, 0, 10]);
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.advance(0), None); // anchor
    assert_eq!(e.advance(10), Some(2));
}

#[test]
fn stopped_animation_never_advances() {
    let mut e = animated(&[10, 10]);
    e.set_mode(AnimationMode::Stopped);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.advance(1000), None);
    assert_eq!(e.current_frame(), 0);
}

#[test]
fn jump_to_changes_current_and_reanchors() {
    let mut e = animated(&[10, 10, 10]);
    e.set_mode(AnimationMode::Running);
    assert!(e.jump_to(2));
    assert_eq!(e.current_frame(), 2);
    assert!(!e.jump_to(2)); // already there
    assert!(!e.jump_to(9)); // out of range
}

#[test]
fn advance_animations_only_ticks_placed_images() {
    let mut s = ImageStore::new(10_000);
    let mut a = animated(&[10, 10]);
    a.set_mode(AnimationMode::Running);
    let mut b = animated(&[10, 10]);
    b.set_mode(AnimationMode::Running);
    s.insert(ImageId(1), a).unwrap();
    s.insert(ImageId(2), b).unwrap();
    s.retain(ImageId(1)); // placed

    assert_eq!(s.next_animation_due_ms(0), Some(0));
    let changed = s.advance_animations(0); // anchors, no change yet
    assert_eq!(changed, Vec::<(ImageId, usize)>::new());
    assert_eq!(s.next_animation_due_ms(0), Some(10));
    let changed = s.advance_animations(10);
    assert_eq!(changed, vec![(ImageId(1), 1)]);
    assert_eq!(s.get(ImageId(2)).unwrap().current_frame(), 0);
}

#[test]
fn next_animation_due_is_none_when_nothing_placed_or_animating() {
    let mut s = ImageStore::new(10_000);
    let still = ImageEntry::new(1, 1, ImageFormat::Rgb24, vec![0]);
    s.insert(ImageId(1), still).unwrap();
    s.retain(ImageId(1));
    assert_eq!(s.next_animation_due_ms(0), None);
}

/// `is_animatable` requires `frames.len() > 1`; relaxing that to `>= 1`
/// would tick a one-frame "animation".
#[test]
fn single_frame_entry_with_a_gap_is_not_animatable() {
    let mut e = animated(&[100]); // one frame, gap 100
    e.set_mode(AnimationMode::Running);
    assert_eq!(e.frame_count(), 1);
    assert_eq!(e.advance(0), None);
    assert_eq!(e.advance(1_000), None);
    assert_eq!(e.current_frame(), 0);
    assert_eq!(e.next_due_ms(), None);
}

/// The reject test is `needed > cap`; `>=` would spuriously reject an
/// exactly-fitting image.
#[test]
fn insert_exactly_at_cap_is_accepted() {
    let mut s = ImageStore::new(charge(64));
    s.insert(ImageId(1), entry(64)).unwrap();
    assert_eq!(s.bytes_used(), charge(64));
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(64));
}

/// Deleting the `!` in the `!contains_key` skip would return a
/// colliding live id or spin.
#[test]
fn allocate_anonymous_id_counts_down_and_skips_live_ids() {
    let mut s = ImageStore::new(1024);
    let first = s.allocate_anonymous_id();
    assert_eq!(first, ImageId(u32::MAX));
    let second = s.allocate_anonymous_id();
    assert_eq!(second, ImageId(u32::MAX - 1));
    // Occupy the next id that would be handed out.
    s.insert(ImageId(u32::MAX - 2), entry(8)).unwrap();
    let third = s.allocate_anonymous_id();
    assert_eq!(third, ImageId(u32::MAX - 3), "must skip the live id");
}

/// Growing then shrinking pins the byte-total delta math and the
/// grow-versus-shrink eviction guard.
#[test]
fn replace_frame_adjusts_byte_accounting_by_the_delta() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    s.push_frame(ImageId(1), frame_of(20)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(20));
    s.replace_frame(ImageId(1), 1, frame_of(50)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(50));
    assert_eq!(
        s.get(ImageId(1)).unwrap().frame(1).unwrap().pixels.len(),
        50
    );
    s.replace_frame(ImageId(1), 1, frame_of(5)).unwrap();
    assert_eq!(s.bytes_used(), charge(10) + frame_charge(5));
}

/// Growing a frame near the cap must evict other refcount-0 images by
/// exactly the growth delta: pins the `new_len > old_len` branch and
/// the `new_len - old_len` evict amount. The `>`→`>=` widening is
/// pinned by `equal_size_replace_frame_evicts_nothing_in_an_over_cap_store`.
#[test]
fn replace_frame_grows_near_cap_and_evicts_others() {
    let mut s = ImageStore::new(2 * charge(40));
    s.insert(ImageId(1), entry(40)).unwrap();
    s.insert(ImageId(2), entry(40)).unwrap();
    assert_eq!(s.bytes_used(), 2 * charge(40));
    s.replace_frame(ImageId(1), 0, frame_of(70)).unwrap();
    assert!(
        s.get(ImageId(2)).is_none(),
        "image 2 should have been evicted to fit the larger frame"
    );
    assert_eq!(s.get(ImageId(1)).unwrap().byte_len(), charge(70));
    assert_eq!(s.bytes_used(), charge(70));
}

/// A same-size replacement charges nothing and evicts nothing,
/// even when the store starts over capacity.
#[test]
fn equal_size_replace_frame_evicts_nothing_in_an_over_cap_store() {
    let entries = vec![
        (ImageId(1), entry(40)),
        // The bystander: refcount 0, first to go in an eviction pass.
        (ImageId(2), entry(40)),
    ];
    let mut s = ImageStore::over_cap_for_test(entries, charge(40));
    assert!(s.bytes_used() > s.bytes_cap(), "store must start over cap");

    s.replace_frame(ImageId(1), 0, frame_of(40)).unwrap();

    assert!(
        s.get(ImageId(2)).is_some(),
        "a zero-byte replacement must not evict a bystander image"
    );
    assert_eq!(s.bytes_used(), 2 * charge(40));
    // `entry` builds its root frame gapless, `frame_of` at 40 ms, so
    // the gap proves the swap landed.
    assert_eq!(s.get(ImageId(1)).unwrap().frame(0).unwrap().gap_ms, 40);
}

/// Entries restored from a dump that predates `atime` all hold 0; the
/// LRU then picks them in store order.
#[test]
fn least_recently_used_breaks_atime_ties_by_store_order() {
    let s = ImageStore::over_cap_for_test(
        vec![
            (ImageId(3), entry(1)),
            (ImageId(1), entry(1)),
            (ImageId(2), entry(1)),
        ],
        charge(1) * 3,
    );
    assert_eq!(s.least_recently_used(None), Some(ImageId(3)));
    assert_eq!(s.least_recently_used(Some(ImageId(3))), Some(ImageId(1)));
}

#[test]
fn edit_frame_rewrites_an_unshared_buffer_where_it_lies_and_charges_nothing() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    let used = s.bytes_used();
    let at = s.get(ImageId(1)).unwrap().frame(0).unwrap().pixels.as_ptr();
    s.edit_frame(ImageId(1), 0, |px| px[3] = 7).unwrap();
    let pixels = &s.get(ImageId(1)).unwrap().frame(0).unwrap().pixels;
    assert_eq!(pixels.as_ptr(), at, "an unshared buffer is not copied");
    assert_eq!(pixels[3], 7);
    assert_eq!(s.bytes_used(), used);
}

#[test]
fn edit_frame_leaves_a_shared_handle_with_the_old_pixels() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    let held = s
        .get(ImageId(1))
        .unwrap()
        .frame(0)
        .unwrap()
        .pixels
        .slice(2..6);
    s.edit_frame(ImageId(1), 0, |px| px.fill(9)).unwrap();
    assert_eq!(held[..], [0; 4]);
    assert_eq!(
        s.get(ImageId(1)).unwrap().frame(0).unwrap().pixels[..],
        [9; 10]
    );
}

#[test]
fn edit_frame_rejects_unknown_image_and_frame() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    assert_eq!(
        s.edit_frame(ImageId(9), 0, |_| {}),
        Err(FrameError::NoSuchImage)
    );
    assert_eq!(
        s.edit_frame(ImageId(1), 1, |_| {}),
        Err(FrameError::NoSuchFrame)
    );
}

/// Pins against the `-> Ok(())` body mutant.
#[test]
fn replace_frame_rejects_unknown_image_and_frame() {
    let mut s = ImageStore::new(1000);
    assert_eq!(
        s.replace_frame(
            ImageId(9),
            0,
            Frame {
                pixels: vec![0].into(),
                gap_ms: 0
            }
        ),
        Err(FrameError::NoSuchImage),
    );
    s.insert(ImageId(1), entry(10)).unwrap();
    assert_eq!(
        s.replace_frame(
            ImageId(1),
            5,
            Frame {
                pixels: vec![0].into(),
                gap_ms: 0
            }
        ),
        Err(FrameError::NoSuchFrame),
    );
}

/// Pins the `frame_idx < current` branch and its `current -= 1`.
#[test]
fn remove_frame_below_current_decrements_current() {
    let mut s = ImageStore::new(1000);
    s.insert(ImageId(1), entry(10)).unwrap();
    for _ in 0..3 {
        s.push_frame(
            ImageId(1),
            Frame {
                pixels: vec![0u8; 10].into(),
                gap_ms: 40,
            },
        )
        .unwrap();
    }
    s.animation_control(ImageId(1)).unwrap().jump_to(2);
    assert_eq!(s.get(ImageId(1)).unwrap().current_frame(), 2);
    s.remove_frame(ImageId(1), 0).unwrap();
    let e = s.get(ImageId(1)).unwrap();
    assert_eq!(e.frame_count(), 3);
    assert_eq!(
        e.current_frame(),
        1,
        "current follows the same logical frame"
    );
}

/// Catches the `len -> 0` and `is_empty -> true` constant-body mutants.
#[test]
fn len_and_is_empty_reflect_a_populated_store() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(1), entry(8)).unwrap();
    s.insert(ImageId(2), entry(8)).unwrap();
    assert_eq!(s.len(), 2);
    assert!(!s.is_empty());
}

/// The constant `empty()` body and the `!=`→`==` retain mutant both
/// break this.
#[test]
fn iter_ids_reflects_insertion_order_and_deletion() {
    let mut s = ImageStore::new(1024);
    s.insert(ImageId(3), entry(8)).unwrap();
    s.insert(ImageId(1), entry(8)).unwrap();
    s.insert(ImageId(2), entry(8)).unwrap();
    assert_eq!(
        s.iter_ids().collect::<Vec<_>>(),
        vec![ImageId(3), ImageId(1), ImageId(2)],
    );
    s.delete(ImageId(1));
    assert_eq!(
        s.iter_ids().collect::<Vec<_>>(),
        vec![ImageId(3), ImageId(2)],
        "delete drops only the named id from the order",
    );
}

/// A store that counted pixels alone lets a flood of 1x1 transmissions
/// past a 256 MiB cap while really holding gigabytes: 4 charged bytes
/// hide ~40 real ones.
#[test]
fn the_budget_charges_per_entry_overhead_not_only_pixels() {
    let one_pixel = entry(4);
    assert!(
        one_pixel.byte_len() > 4 * 8,
        "a 4-byte image charged {} bytes — the fixed cost is missing",
        one_pixel.byte_len(),
    );
    // Entry count, not just byte volume, is what the cap bounds.
    let per_entry = one_pixel.byte_len();
    let mut store = ImageStore::new(per_entry * 4);
    for id in 0..4 {
        assert!(store.insert(ImageId(id), entry(4)).is_ok(), "id {id}");
    }
    // Pinned so eviction cannot make room; the fifth is refused on
    // overhead alone.
    for id in 0..4 {
        assert!(store.retain(ImageId(id)));
    }
    assert!(store.insert(ImageId(4), entry(4)).is_err());
}
