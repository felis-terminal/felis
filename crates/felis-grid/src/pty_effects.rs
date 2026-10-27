//! The grid's outbox of parser side-effects, in byte-stream order
//! between the daemon's drains. An [`Apc`](crate::PtyEffect::Apc)
//! carries a copy of a producer-chosen body, so it is the one kind
//! with a budget; the queue owns the APC count so no caller can add
//! one past it.

use crate::{ApcBody, PtyEffect, ScrollOp};

/// Maximum APC bodies buffered between [`PtyEffectQueue::take`]
/// drains; this bounds a flood inside one `Parser::advance`.
pub const APC_OUTBOX_CAP: usize = 64;

/// Maximum APC body bytes buffered between drains. A count cap alone
/// would admit 64 bodies of [`felis_vt::APC_BUFFER_LIMIT`] each.
pub const APC_OUTBOX_BYTES: usize = 1024 * 1024;

const _: () = assert!(APC_OUTBOX_BYTES >= felis_vt::APC_BUFFER_LIMIT);

/// [`ProgramStatus`](crate::PtyEffect::ProgramStatus),
/// [`PromptStarted`](crate::PtyEffect::PromptStarted) and
/// [`TerminalProgress`](crate::PtyEffect::TerminalProgress) effects
/// buffered between drains before the parse yields. None is ever dropped:
/// the daemon's state must match what the producer sent.
pub const STATUS_OUTBOX_YIELD: usize = 64;

/// [`CommandOutput`](crate::PtyEffect::CommandOutput) effects buffered
/// between drains before the parse yields; with
/// [`MAX_ANNOUNCE_BYTES`](crate::MAX_ANNOUNCE_BYTES) this bounds the text
/// waiting in the queue. None is dropped.
pub const COMMAND_OUTPUT_YIELD: usize = 4;

#[derive(Debug, Clone, Default)]
pub struct PtyEffectQueue {
    effects: Vec<PtyEffect>,
    apc_len: usize,
    apc_bytes: usize,
    status_len: usize,
    command_output_len: usize,
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
        if let PtyEffect::Apc(apc) = &effect
            && !self.reserve_apc(apc.body.len())
        {
            return false;
        }
        if matches!(
            effect,
            PtyEffect::ProgramStatus(_) | PtyEffect::PromptStarted | PtyEffect::TerminalProgress(_)
        ) {
            self.status_len += 1;
        }
        if matches!(effect, PtyEffect::CommandOutput { .. }) {
            self.command_output_len += 1;
        }
        self.effects.push(effect);
        true
    }

    /// `false` when this drain's APC budget is spent; a refused APC
    /// costs no allocation.
    pub fn push_apc(&mut self, body: &[u8], cursor_row: u16, cursor_col: u16) -> bool {
        if !self.reserve_apc(body.len()) {
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

    /// Coalesces like [`Self::push_scroll`]: moving a placement by `a`
    /// then `b` rows, clipping at the margins each time, is moving it
    /// by `a + b` once, up to rounding each source cut to whole pixels.
    pub fn push_region_scroll(&mut self, op: ScrollOp, into_history: bool) {
        let same = |tail: &ScrollOp, history: bool| {
            history == into_history
                && tail.region_top == op.region_top
                && tail.region_bottom == op.region_bottom
                && tail.direction == op.direction
        };
        let tail = match self.effects.as_mut_slice() {
            [
                ..,
                PtyEffect::RegionScrolled {
                    op: tail,
                    into_history: history,
                },
                PtyEffect::Scrolled { .. },
            ]
            | [
                ..,
                PtyEffect::RegionScrolled {
                    op: tail,
                    into_history: history,
                },
            ] if same(tail, *history) => Some(tail),
            _ => None,
        };
        if let Some(tail) = tail {
            tail.n_rows = tail.n_rows.saturating_add(op.n_rows);
        } else {
            self.effects
                .push(PtyEffect::RegionScrolled { op, into_history });
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

    /// A body of the largest size might not fit before the next
    /// [`Self::take`].
    pub const fn apc_budget_spent(&self) -> bool {
        self.apc_len >= APC_OUTBOX_CAP
            || self.apc_bytes + felis_vt::APC_BUFFER_LIMIT > APC_OUTBOX_BYTES
    }

    pub const fn status_budget_spent(&self) -> bool {
        self.status_len >= STATUS_OUTBOX_YIELD
    }

    pub const fn command_output_budget_spent(&self) -> bool {
        self.command_output_len >= COMMAND_OUTPUT_YIELD
    }

    /// The only drain; it refills every budget.
    pub fn take(&mut self) -> Vec<PtyEffect> {
        self.apc_len = 0;
        self.apc_bytes = 0;
        self.status_len = 0;
        self.command_output_len = 0;
        std::mem::take(&mut self.effects)
    }

    const fn reserve_apc(&mut self, len: usize) -> bool {
        if self.apc_len >= APC_OUTBOX_CAP || self.apc_bytes + len > APC_OUTBOX_BYTES {
            return false;
        }
        self.apc_len += 1;
        self.apc_bytes += len;
        true
    }

    #[cfg(feature = "state-dump")]
    pub(crate) fn effects(&self) -> &[PtyEffect] {
        &self.effects
    }
}

#[cfg(feature = "state-dump")]
impl serde::Serialize for PtyEffectQueue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.effects.serialize(serializer)
    }
}

/// The budget counts are the queue's own, so they are recounted rather
/// than carried.
#[cfg(feature = "state-dump")]
impl<'de> serde::Deserialize<'de> for PtyEffectQueue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let effects = Vec::<PtyEffect>::deserialize(deserializer)?;
        let (apc_len, apc_bytes) = effects
            .iter()
            .filter_map(|effect| match effect {
                PtyEffect::Apc(apc) => Some(apc.body.len()),
                _ => None,
            })
            .fold((0, 0), |(n, bytes), len| (n + 1, bytes + len));
        let status_len = effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    PtyEffect::ProgramStatus(_)
                        | PtyEffect::PromptStarted
                        | PtyEffect::TerminalProgress(_)
                )
            })
            .count();
        let command_output_len = effects
            .iter()
            .filter(|effect| matches!(effect, PtyEffect::CommandOutput { .. }))
            .count();
        Ok(Self {
            effects,
            apc_len,
            apc_bytes,
            status_len,
            command_output_len,
        })
    }
}

#[cfg(test)]
mod tests;
