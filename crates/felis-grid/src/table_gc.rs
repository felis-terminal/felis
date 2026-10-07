//! Sweep policy for the style and sizing registries.
//!
//! The trigger points live here, not on `Grid`: there they would ride
//! its `PartialEq` and its dump. The daemon's parse core and the
//! client's shadow each hold an instance.

use crate::{Grid, ScreenBuffer};

/// Above any normal SGR palette (a 256-color TUI interns well under 300
/// distinct pens); only a truecolor flood crosses it.
const STYLE_INIT: usize = 4096;

/// A screen's worth of OSC 66 runs is far below this; only repaint churn
/// crosses it.
const SIZING_INIT: usize = 4096;

/// A quarter of the `u16` handle space. Unlike a `StyleId`, a sizing
/// handle that cannot be minted has no fallback (the run draws unsized),
/// so a session must not raise its own trigger point past the sweep's
/// reach.
const SIZING_MAX: usize = u16::MAX as usize / 4;

const _: () = {
    assert!(SIZING_INIT < SIZING_MAX);
    assert!(SIZING_MAX < u16::MAX as usize);
};

/// When to sweep a grid's style and sizing registries. Call
/// [`Self::maybe_sweep`] only where no handle into either table is held
/// across the call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct TableGc {
    style_threshold: usize,
    sizing_threshold: usize,
}

impl Default for TableGc {
    fn default() -> Self {
        Self::new()
    }
}

impl TableGc {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            style_threshold: STYLE_INIT,
            sizing_threshold: SIZING_INIT,
        }
    }

    pub fn maybe_sweep(&mut self, tables: &mut impl Sweepable) {
        if tables.style_len() > self.style_threshold {
            let scanned = tables.sweep_styles(sealed::Token);
            self.style_threshold = raise_past_live_set(tables.style_len(), scanned);
        }
        if tables.sizing_len() > self.sizing_threshold {
            tables.sweep_sizings(sealed::Token);
            self.sizing_threshold =
                raise_if_still_full(tables.sizing_len(), self.sizing_threshold, SIZING_MAX);
        }
    }

    /// Sweeps both registries now, whatever their size.
    pub fn sweep(&mut self, tables: &mut impl Sweepable) {
        let scanned = tables.sweep_styles(sealed::Token);
        self.style_threshold = raise_past_live_set(tables.style_len(), scanned);
        tables.sweep_sizings(sealed::Token);
        self.sizing_threshold =
            raise_if_still_full(tables.sizing_len(), self.sizing_threshold, SIZING_MAX);
    }

    #[must_use]
    pub const fn style_threshold(&self) -> usize {
        self.style_threshold
    }

    #[must_use]
    pub const fn sizing_threshold(&self) -> usize {
        self.sizing_threshold
    }
}

mod sealed {
    pub trait Sealed {}
    /// Unnameable outside the crate, so only [`super::TableGc`] can call
    /// a sweep through the trait.
    pub struct Token;
    impl Sealed for crate::Grid {}
    impl Sealed for crate::ScreenBuffer {}
}

/// An owner of both registries together with every handle it holds into
/// them outside its cells, so a sweep re-establishes those handles as
/// part of the sweep itself. Sealed: a new holder of an outside handle
/// must be an owner here, not a caller that sweeps its screen bare.
pub trait Sweepable: sealed::Sealed {
    #[doc(hidden)]
    fn style_len(&self) -> usize;
    #[doc(hidden)]
    fn sizing_len(&self) -> usize;
    /// Returns the cells scanned.
    #[doc(hidden)]
    fn sweep_styles(&mut self, _: sealed::Token) -> usize;
    #[doc(hidden)]
    fn sweep_sizings(&mut self, _: sealed::Token);
}

impl Sweepable for Grid {
    fn style_len(&self) -> usize {
        self.style_table_len()
    }
    fn sizing_len(&self) -> usize {
        self.sizing_count()
    }
    fn sweep_styles(&mut self, _: sealed::Token) -> usize {
        self.gc_styles()
    }
    fn sweep_sizings(&mut self, _: sealed::Token) {
        self.gc_sizings();
    }
}

/// A bare screen, as the client's shadow holds it: no pen, no open
/// sizing run.
impl Sweepable for ScreenBuffer {
    fn style_len(&self) -> usize {
        self.style_table_len()
    }
    fn sizing_len(&self) -> usize {
        self.sizing_count()
    }
    fn sweep_styles(&mut self, _: sealed::Token) -> usize {
        self.gc_styles().0
    }
    fn sweep_sizings(&mut self, _: sealed::Token) {
        self.gc_sizings();
    }
}

/// Headroom above the live set, as a divisor of the cells the sweep just
/// scanned, so a sweep costs at most `HEADROOM_DIVISOR` cell visits per
/// interned entry however deep the scrollback is. Lower spends parse
/// time; higher is residency (at 16 a full 10 k-row ring buys ~120 k
/// entries of slack, single-digit MB).
const HEADROOM_DIVISOR: usize = 16;

/// The sizing raise: capped by `max`, which is also why it does not
/// scale its headroom to the scan the way the style raise does.
const fn raise_if_still_full(len: usize, threshold: usize, max: usize) -> usize {
    if len <= threshold / 2 {
        return threshold;
    }
    let doubled = threshold.saturating_mul(2);
    if doubled > max { max } else { doubled }
}

/// Lifts the trigger point past the live set by enough headroom to amortize
/// the O(cells) sweep. Not a fixed ceiling: truecolor floods would re-sweep
/// every drain. Not `2 × len`: fails when live pens are a small fraction of
/// total scanned cells in a large scrollback.
fn raise_past_live_set(len: usize, scanned: usize) -> usize {
    // The `len` term covers a live set that is itself most of the scan
    // (a full screen of distinct pens), where a flat fraction of the
    // scan would sweep several times per frame.
    len.saturating_add(len.max(scanned / HEADROOM_DIVISOR))
        .max(STYLE_INIT)
}

#[cfg(test)]
mod tests;
