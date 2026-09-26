//! Byte-bounded client-to-daemon send queue (`docs/reference/ipc.md` "Backpressure").
//!
//! Coalesces superseding input kinds by typed variant alone (principle 4) to
//! prevent stalled sockets from exhausting window memory.

use std::collections::VecDeque;

use felis_protocol::codec::{Correlated, WireCodec};
use felis_protocol::limits::CLIENT_OUTGOING_CAP;
use felis_protocol::messages::{Correlation, Directed, InputMsg, MouseAction};
use felis_protocol::minor::MinorGated;
use felis_transport::{CheckedFrame, TransportError};

/// One authorized frame waiting for the writer task. The body is only
/// reachable as a [`CheckedFrame`], so a frame that waits in the queue
/// still names what it costs to send when the writer takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingFrame {
    frame: CheckedFrame,
    /// `Some` when a newer frame of the same kind supersedes this one.
    coalesce: Option<CoalesceKey>,
    /// The coalescing slots this frame closes behind it, because the
    /// daemon reads their state when it handles this frame. See
    /// [`seals_key`].
    seals: &'static [CoalesceKey],
}

impl OutgoingFrame {
    /// An ordered frame: nothing may replace or reorder it.
    ///
    /// # Errors
    /// [`TransportError::Wire`] for a message past a per-operation
    /// limit (REQ-105a); the caller drops the frame.
    pub fn ordered<M: WireCodec + MinorGated + Directed>(msg: &M) -> Result<Self, TransportError> {
        Ok(Self::wrap(CheckedFrame::encode(msg)?, None, &[]))
    }

    /// # Errors
    /// As [`Self::ordered`].
    pub fn correlated<M: Correlated + MinorGated + Directed>(
        msg: &M,
        correlation: Correlation,
    ) -> Result<Self, TransportError> {
        Ok(Self::wrap(
            CheckedFrame::encode_correlated(msg, correlation)?,
            None,
            &[],
        ))
    }

    /// The one coalescing path: [`coalesce_key`] and [`seals_key`]
    /// answer from the variant alone.
    ///
    /// # Errors
    /// As [`Self::ordered`].
    pub fn input(msg: &InputMsg) -> Result<Self, TransportError> {
        Ok(Self::wrap(
            CheckedFrame::encode(msg)?,
            coalesce_key(msg),
            seals_key(msg),
        ))
    }

    #[cfg(test)]
    fn raw(
        kind: u16,
        body: &[u8],
        coalesce: Option<CoalesceKey>,
        seals: &'static [CoalesceKey],
    ) -> Self {
        Self::wrap(
            CheckedFrame::raw(kind, body.to_vec(), felis_protocol::Requires::BASE),
            coalesce,
            seals,
        )
    }

    const fn wrap(
        frame: CheckedFrame,
        coalesce: Option<CoalesceKey>,
        seals: &'static [CoalesceKey],
    ) -> Self {
        Self {
            frame,
            coalesce,
            seals,
        }
    }

    #[must_use]
    pub const fn frame(&self) -> &CheckedFrame {
        &self.frame
    }
}

/// The kinds whose queued value is state rather than history: only the
/// newest one is worth sending, and the daemon's behavior for the older
/// ones is indistinguishable from never having received them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceKey {
    Resize,
    FocusChange,
    ColorScheme,
    Viewport,
    /// Buttonless motion only. A drag carries a button, and a program
    /// that tracks selection needs every drag sample.
    MouseMotion,
}

/// The typed coalescing rule. `None` means the message is ordered: its
/// effect on the child depends on it arriving, in place, exactly once.
#[must_use]
pub const fn coalesce_key(msg: &InputMsg) -> Option<CoalesceKey> {
    match msg {
        InputMsg::Resize { .. } => Some(CoalesceKey::Resize),
        InputMsg::FocusChange { .. } => Some(CoalesceKey::FocusChange),
        InputMsg::ColorScheme { .. } => Some(CoalesceKey::ColorScheme),
        InputMsg::Viewport { .. } => Some(CoalesceKey::Viewport),
        InputMsg::Mouse(event) => match (event.action, event.button) {
            (MouseAction::Motion, None) => Some(CoalesceKey::MouseMotion),
            _ => None,
        },
        _ => None,
    }
}

/// Coalescing keys sealed by an ordered frame so prior entries stay immutable.
///
/// Prevents subsequent coalescing replacements from invalidating positional or
/// viewport dependencies captured by the ordered frame.
#[must_use]
pub const fn seals_key(msg: &InputMsg) -> &'static [CoalesceKey] {
    match msg {
        InputMsg::JumpPrompt { .. } => &[CoalesceKey::Viewport],
        InputMsg::Mouse(event) => match (event.action, event.button) {
            (MouseAction::Motion, None) => &[],
            _ => &[CoalesceKey::MouseMotion, CoalesceKey::Resize],
        },
        _ => &[],
    }
}

/// Backlog exceeded capacity; callers treat this as connection loss.
///
/// Refusing input at the cap reflects a dead carrier rather than transient
/// delay, avoiding event-loop stalls or silent keystroke drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutgoingFull {
    /// Bytes already queued when the push was refused.
    pub queued: usize,
    /// Bytes the refused frame would have added.
    pub incoming: usize,
    pub cap: usize,
}

/// A byte-bounded FIFO of encoded frames.
#[derive(Debug)]
pub struct OutgoingQueue {
    items: VecDeque<OutgoingFrame>,
    bytes: usize,
    cap: usize,
}

impl Default for OutgoingQueue {
    fn default() -> Self {
        Self::with_cap(CLIENT_OUTGOING_CAP)
    }
}

impl OutgoingQueue {
    #[must_use]
    pub const fn with_cap(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            cap,
        }
    }

    /// Bytes queued and not yet handed to the writer.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Enqueues a frame, replacing any unsealed frame of the same key in place.
    /// In-place replacement preserves order relative to adjacent frames.
    ///
    /// # Errors
    /// Returns [`OutgoingFull`] if queue capacity is exceeded.
    pub fn push(&mut self, frame: OutgoingFrame) -> Result<(), OutgoingFull> {
        let incoming = frame.frame.body().len();
        if let Some(key) = frame.coalesce
            && let Some(slot) = self
                .items
                .iter_mut()
                .find(|queued| queued.coalesce == Some(key))
        {
            let replaced = slot.frame.body().len();
            let after = self.bytes - replaced + incoming;
            if after > self.cap {
                return Err(OutgoingFull {
                    queued: self.bytes,
                    incoming,
                    cap: self.cap,
                });
            }
            *slot = frame;
            self.bytes = after;
            return Ok(());
        }
        if self.bytes + incoming > self.cap {
            return Err(OutgoingFull {
                queued: self.bytes,
                incoming,
                cap: self.cap,
            });
        }
        // Sealed only once the frame is actually queued: a refusal
        // declares the carrier lost, and a queue the caller is about to
        // abandon should not have been rewritten on the way out.
        for queued in &mut self.items {
            if queued
                .coalesce
                .is_some_and(|key| frame.seals.contains(&key))
            {
                queued.coalesce = None;
            }
        }
        self.bytes += incoming;
        self.items.push_back(frame);
        Ok(())
    }

    /// The oldest frame, or `None` when the queue is empty.
    pub fn pop(&mut self) -> Option<OutgoingFrame> {
        let frame = self.items.pop_front()?;
        self.bytes -= frame.frame.body().len();
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use felis_protocol::messages::{InputMods, MouseButton, MouseEvent, RequestedDims};
    use proptest::prelude::*;

    const RESIZE_KIND: u16 = 1;
    const KEY_KIND: u16 = 2;

    fn key(body: &[u8]) -> OutgoingFrame {
        OutgoingFrame::raw(KEY_KIND, body, None, &[])
    }

    fn resize(body: &[u8]) -> OutgoingFrame {
        OutgoingFrame::raw(RESIZE_KIND, body, Some(CoalesceKey::Resize), &[])
    }

    fn motion(button: Option<MouseButton>, action: MouseAction) -> InputMsg {
        InputMsg::Mouse(MouseEvent {
            button,
            action,
            mods: InputMods::empty(),
            x: 1,
            y: 1,
            px: 1,
            py: 1,
        })
    }

    /// The queue refuses a correlated arm pushed as an ordered frame.
    #[test]
    fn the_queue_refuses_a_correlated_arm_pushed_as_an_ordered_frame() {
        use felis_protocol::messages::NotifyToDaemonMsg;

        let err = OutgoingFrame::ordered(&NotifyToDaemonMsg::Subscribe {
            session_prefix: None,
        })
        .expect_err("a stream opener has no uncorrelated spelling");
        assert!(
            matches!(
                err,
                TransportError::Correlation { ref found, .. } if found.contains("Notify::Subscribe")
            ),
            "got {err:?}",
        );
    }

    #[test]
    fn ordered_frames_drain_in_push_order() {
        let mut queue = OutgoingQueue::default();
        for body in [b"a".as_slice(), b"b", b"c"] {
            queue.push(key(body)).expect("under the cap");
        }
        let drained: Vec<_> = std::iter::from_fn(|| queue.pop())
            .map(|frame| frame.frame().body().to_vec())
            .collect();
        assert_eq!(drained, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn a_replaceable_frame_keeps_its_slot_when_superseded() {
        let mut queue = OutgoingQueue::default();
        queue.push(resize(b"80x24")).expect("under the cap");
        queue.push(key(b"ls")).expect("under the cap");
        queue.push(resize(b"100x30")).expect("under the cap");

        let first = queue.pop().expect("a queued frame");
        assert_eq!(
            (first.frame().kind(), first.frame().body().to_vec()),
            (RESIZE_KIND, b"100x30".to_vec()),
            "the newest resize must still precede the key queued after the old one",
        );
        let second = queue.pop().expect("a queued frame");
        assert_eq!(
            (second.frame().kind(), second.frame().body().to_vec()),
            (KEY_KIND, b"ls".to_vec())
        );
        assert!(queue.is_empty(), "coalescing must not leave a duplicate");
    }

    /// The refusal means "this carrier is gone", so it must never fire
    /// on a message the daemon would have accepted: the largest paste
    /// has to fit an idle queue, and typing while it drains has to fit
    /// too.
    #[test]
    fn the_largest_paste_the_daemon_admits_fits_beside_a_keystroke() {
        use felis_protocol::limits::{MAX_PASTE_BYTES, PASTE_BRACKET_OVERHEAD};

        let mut queue = OutgoingQueue::default();
        queue
            .push(OutgoingFrame::raw(
                KEY_KIND,
                &vec![b'x'; MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD],
                None,
                &[],
            ))
            .expect("a paste at the daemon's limit is not a backlog");
        queue
            .push(key(b"q"))
            .expect("a keystroke behind a draining paste is not a backlog either");
        assert_eq!(queue.len(), 2);
    }

    /// The daemon jumps from wherever the viewport already is, so a
    /// scroll queued after a jump must not be hoisted in front of it.
    #[test]
    fn a_jump_closes_the_viewport_slot_behind_it() {
        const VIEWPORT_KIND: u16 = 3;
        const JUMP_KIND: u16 = 4;

        let viewport =
            |body: &[u8]| OutgoingFrame::raw(VIEWPORT_KIND, body, Some(CoalesceKey::Viewport), &[]);
        let mut queue = OutgoingQueue::default();
        queue.push(viewport(b"0")).expect("under the cap");
        queue
            .push(OutgoingFrame::raw(
                JUMP_KIND,
                b"up",
                None,
                &[CoalesceKey::Viewport],
            ))
            .expect("under the cap");
        queue.push(viewport(b"50")).expect("under the cap");

        let drained: Vec<_> = std::iter::from_fn(|| queue.pop())
            .map(|frame| (frame.frame().kind(), frame.frame().body().to_vec()))
            .collect();
        assert_eq!(
            drained,
            vec![
                (VIEWPORT_KIND, b"0".to_vec()),
                (JUMP_KIND, b"up".to_vec()),
                (VIEWPORT_KIND, b"50".to_vec()),
            ],
            "the scroll after the jump must not replace the one before it",
        );
    }

    /// Viewport frames still coalesce among themselves once a jump has
    /// opened a fresh slot; only crossing the jump is forbidden.
    #[test]
    fn viewport_frames_after_a_jump_still_coalesce_with_each_other() {
        const VIEWPORT_KIND: u16 = 3;

        let viewport =
            |body: &[u8]| OutgoingFrame::raw(VIEWPORT_KIND, body, Some(CoalesceKey::Viewport), &[]);
        let mut queue = OutgoingQueue::default();
        queue
            .push(OutgoingFrame::raw(4, b"up", None, &[CoalesceKey::Viewport]))
            .expect("under the cap");
        queue.push(viewport(b"10")).expect("under the cap");
        queue.push(viewport(b"20")).expect("under the cap");
        assert_eq!(queue.len(), 2);
        drop(queue.pop());
        let latest = queue.pop().expect("a queued frame");
        assert_eq!(latest.frame().body(), b"20");
    }

    #[test]
    fn only_a_jump_and_an_ordered_mouse_event_seal_a_slot() {
        use felis_protocol::messages::PromptJump;

        assert_eq!(
            seals_key(&InputMsg::JumpPrompt {
                direction: PromptJump::Previous
            }),
            &[CoalesceKey::Viewport],
        );
        assert_eq!(
            seals_key(&motion(Some(MouseButton::Left), MouseAction::Press)),
            &[CoalesceKey::MouseMotion, CoalesceKey::Resize],
            "a click's cell was computed against the grid it was clicked on",
        );
        assert_eq!(
            seals_key(&motion(Some(MouseButton::Left), MouseAction::Motion)),
            &[CoalesceKey::MouseMotion, CoalesceKey::Resize],
            "a drag sample is a position the program must see in place",
        );
        assert_eq!(
            seals_key(&motion(None, MouseAction::Motion)),
            &[],
            "a buttonless motion is the kind that coalesces, not one that seals",
        );
        for msg in [
            InputMsg::KeyBytes(b"a".to_vec()),
            InputMsg::NextGridFrame,
            InputMsg::Viewport {
                lines_from_bottom: 3,
            },
        ] {
            let none: &[CoalesceKey] = &[];
            assert_eq!(seals_key(&msg), none, "{msg:?} reads no coalesced state");
        }
    }

    /// A click's coordinates are cells of the grid the window had when
    /// the user clicked. A resize queued behind it must not take the
    /// slot of one queued ahead of it: the child would reflow first and
    /// the click would land on whatever then occupies that cell.
    #[test]
    fn a_button_event_closes_the_resize_slot_behind_it() {
        const CLICK_KIND: u16 = 5;

        let mut queue = OutgoingQueue::default();
        queue.push(resize(b"80x24")).expect("under the cap");
        queue
            .push(OutgoingFrame::raw(
                CLICK_KIND,
                b"press@40,12",
                None,
                seals_key(&motion(Some(MouseButton::Left), MouseAction::Press)),
            ))
            .expect("under the cap");
        queue.push(resize(b"200x60")).expect("under the cap");

        let drained: Vec<_> = std::iter::from_fn(|| queue.pop())
            .map(|frame| (frame.frame().kind(), frame.frame().body().to_vec()))
            .collect();
        assert_eq!(
            drained,
            vec![
                (RESIZE_KIND, b"80x24".to_vec()),
                (CLICK_KIND, b"press@40,12".to_vec()),
                (RESIZE_KIND, b"200x60".to_vec()),
            ],
            "the resize after the click must not replace the one before it",
        );
    }

    /// A press and a release are points in the same positional stream
    /// the motions belong to: a later motion must not be hoisted in
    /// front of them, or the program's last known position is the one
    /// from before the click.
    #[test]
    fn a_button_event_closes_the_motion_slot_behind_it() {
        const MOTION_KIND: u16 = 5;
        const PRESS_KIND: u16 = 6;

        let motion_frame = |body: &[u8]| {
            OutgoingFrame::raw(MOTION_KIND, body, Some(CoalesceKey::MouseMotion), &[])
        };
        let mut queue = OutgoingQueue::default();
        queue.push(motion_frame(b"a")).expect("under the cap");
        queue
            .push(OutgoingFrame::raw(
                PRESS_KIND,
                b"press-a",
                None,
                &[CoalesceKey::MouseMotion],
            ))
            .expect("under the cap");
        queue.push(motion_frame(b"c")).expect("under the cap");

        let drained: Vec<_> = std::iter::from_fn(|| queue.pop())
            .map(|frame| (frame.frame().kind(), frame.frame().body().to_vec()))
            .collect();
        assert_eq!(
            drained,
            vec![
                (MOTION_KIND, b"a".to_vec()),
                (PRESS_KIND, b"press-a".to_vec()),
                (MOTION_KIND, b"c".to_vec()),
            ],
            "the motion after the press must not replace the one before it",
        );
    }

    #[test]
    fn only_buttonless_motion_coalesces() {
        assert_eq!(
            coalesce_key(&motion(None, MouseAction::Motion)),
            Some(CoalesceKey::MouseMotion),
        );
        assert_eq!(
            coalesce_key(&motion(Some(MouseButton::Left), MouseAction::Drag)),
            None,
            "a drag sample is what a selection is made of",
        );
        assert_eq!(
            coalesce_key(&motion(Some(MouseButton::Left), MouseAction::Press)),
            None,
        );
    }

    #[test]
    fn keys_and_pastes_are_never_coalesced() {
        assert_eq!(coalesce_key(&InputMsg::KeyBytes(b"a".to_vec())), None);
        assert_eq!(coalesce_key(&InputMsg::Paste(b"a".to_vec())), None);
        assert_eq!(coalesce_key(&InputMsg::NextGridFrame), None);
        assert_eq!(
            coalesce_key(&InputMsg::Resize {
                dims: RequestedDims {
                    rows: 24,
                    cols: 80,
                    pixel_w: 0,
                    pixel_h: 0,
                },
            }),
            Some(CoalesceKey::Resize),
        );
    }

    proptest! {
        /// `bytes()` is the queue's own accounting of what it holds, and
        /// a refusal must leave the queue untouched: a replacement that
        /// forgot to give the superseded body back would strand the
        /// window on a carrier it wrongly believes is backed up.
        #[test]
        fn cap_accounting_matches_the_queued_bodies(
            cap in 1_usize..=64,
            ops in prop::collection::vec((0_u8..4, 1_usize..=16), 0..64usize),
        ) {
            let mut queue = OutgoingQueue::with_cap(cap);
            for (op, size) in ops {
                let before = queue.bytes();
                let len_before = queue.len();
                if op == 3 {
                    if let Some(frame) = queue.pop() {
                        prop_assert_eq!(queue.bytes(), before - frame.frame().body().len());
                        prop_assert_eq!(queue.len(), len_before - 1);
                    } else {
                        prop_assert_eq!(before, 0);
                    }
                    continue;
                }
                let body = vec![b'x'; size];
                let frame = match op {
                    0 => key(&body),
                    1 => resize(&body),
                    _ => OutgoingFrame::raw(
                        RESIZE_KIND,
                        &body,
                        Some(CoalesceKey::FocusChange),
                        &[],
                    ),
                };
                match queue.push(frame) {
                    Ok(()) => prop_assert!(queue.bytes() <= cap),
                    Err(full) => {
                        prop_assert_eq!(
                            full,
                            OutgoingFull { queued: before, incoming: size, cap },
                        );
                        prop_assert_eq!(queue.bytes(), before, "a refused frame must not be queued");
                        prop_assert_eq!(queue.len(), len_before);
                        prop_assert!(before + size > cap, "the refusal must have been forced");
                    }
                }
            }
            let held = queue.bytes();
            let drained: usize = std::iter::from_fn(|| queue.pop())
                .map(|frame| frame.frame().body().len())
                .sum();
            prop_assert_eq!(drained, held);
            prop_assert_eq!(queue.bytes(), 0);
        }

        /// Coalescing may drop superseded replaceable frames and move
        /// them within the queue, but the ordered frames must come out
        /// exactly as they went in.
        #[test]
        fn ordered_frames_survive_any_interleaving(ops in prop::collection::vec(
            (any::<bool>(), 0u8..8),
            0..64usize,
        )) {
            let mut queue = OutgoingQueue::default();
            let mut expected: Vec<Vec<u8>> = Vec::new();
            for (ordered, byte) in ops {
                let body = vec![byte];
                if ordered {
                    expected.push(body.clone());
                    queue.push(OutgoingFrame::raw(KEY_KIND, &body, None, &[])).expect("under the cap");
                } else {
                    queue
                        .push(OutgoingFrame::raw(
                            RESIZE_KIND,
                            &body,
                            Some(CoalesceKey::Resize),
                            &[],
                        ))
                        .expect("under the cap");
                }
            }
            let drained: Vec<Vec<u8>> = std::iter::from_fn(|| queue.pop())
                .filter(|frame| frame.coalesce.is_none())
                .map(|frame| frame.frame().body().to_vec())
                .collect();
            prop_assert_eq!(drained, expected);
            prop_assert_eq!(queue.bytes(), 0);
        }
    }
}
