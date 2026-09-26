//! The grid's outbox of parser side-effects, in byte-stream order
//! between the daemon's drains. An [`Apc`](crate::PtyEffect::Apc)
//! carries a copy of a producer-chosen body, so it is the one kind
//! with a budget; the queue owns the APC count so no caller can add
//! one past it.

use crate::{ApcBody, PtyEffect, ScrollOp};

/// Maximum APC bodies buffered between [`PtyEffectQueue::take`]
/// drains. Each body is already capped at
/// [`felis_vt::APC_BUFFER_LIMIT`]; this bounds a flood inside one
/// `Parser::advance`. Kitty graphics traffic emits 1–4 per animation
/// frame.
pub const APC_OUTBOX_CAP: usize = 64;

const _: () = assert!(APC_OUTBOX_CAP * felis_vt::APC_BUFFER_LIMIT == 512 * 1024);

#[derive(Debug, Clone, Default)]
pub struct PtyEffectQueue {
    effects: Vec<PtyEffect>,
    apc_len: usize,
}

impl PartialEq for PtyEffectQueue {
    fn eq(&self, other: &Self) -> bool {
        self.effects == other.effects
    }
}

impl Eq for PtyEffectQueue {}

impl PtyEffectQueue {
    /// `false` only for an [`Apc`](PtyEffect::Apc) refused by
    /// [`APC_OUTBOX_CAP`]. [`Self::push_apc`] copies the body only once
    /// there is room for it.
    pub fn push(&mut self, effect: PtyEffect) -> bool {
        if matches!(effect, PtyEffect::Apc(_)) && !self.reserve_apc() {
            return false;
        }
        self.effects.push(effect);
        true
    }

    /// `false` when this drain's APC budget is spent; a refused APC
    /// costs no allocation.
    pub fn push_apc(&mut self, body: &[u8], cursor_row: u16, cursor_col: u16) -> bool {
        if !self.reserve_apc() {
            return false;
        }
        self.effects.push(PtyEffect::Apc(ApcBody {
            body: body.to_vec(),
            cursor_row,
            cursor_col,
        }));
        true
    }

    /// Coalesces into the tail entry, reaching past a trailing
    /// [`Scrolled`](PtyEffect::Scrolled), which commutes with a placement
    /// shift. No general `last_mut` exists: it would hand out a slot an
    /// APC could be written into past the budget.
    pub fn push_scroll(&mut self, n: u32) {
        let tail = match self.effects.as_mut_slice() {
            [
                ..,
                PtyEffect::ScrolledIntoScrollback(total),
                PtyEffect::Scrolled { .. },
            ]
            | [.., PtyEffect::ScrolledIntoScrollback(total)] => Some(total),
            _ => None,
        };
        if let Some(total) = tail {
            *total = total.saturating_add(n);
        } else {
            self.effects.push(PtyEffect::ScrolledIntoScrollback(n));
        }
    }

    /// Coalesces into a tail directive of the same band, direction and
    /// geometry: two shifts of one band are one shift by the sum, capped
    /// at the band height, which already blanks the band.
    pub fn push_scrolled(&mut self, op: ScrollOp, geometry_gen: u64, scroll_seq: u64) {
        if let Some(PtyEffect::Scrolled {
            op: tail,
            geometry_gen: tail_gen,
            last_seq,
            ..
        }) = self.effects.last_mut()
            && *tail_gen == geometry_gen
            && tail.region_top == op.region_top
            && tail.region_bottom == op.region_bottom
            && tail.direction == op.direction
        {
            let height = op.region_bottom - op.region_top + 1;
            tail.n_rows = tail.n_rows.saturating_add(op.n_rows).min(height);
            *last_seq = scroll_seq;
            return;
        }
        self.effects.push(PtyEffect::Scrolled {
            op,
            geometry_gen,
            first_seq: scroll_seq,
            last_seq: scroll_seq,
        });
    }

    /// The only drain; it refills the APC budget.
    pub fn take(&mut self) -> Vec<PtyEffect> {
        self.apc_len = 0;
        std::mem::take(&mut self.effects)
    }

    const fn reserve_apc(&mut self) -> bool {
        if self.apc_len >= APC_OUTBOX_CAP {
            return false;
        }
        self.apc_len += 1;
        true
    }
}

#[cfg(test)]
mod tests;
