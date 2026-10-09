//! Williams DFA terminal parser and Kitty extension dispatchers.
//!
//! Fixed-capacity slabs overflow to an "ignore" path without per-byte
//! allocations. Not a `vte` wrapper: Kitty graphics APC framing and keyboard
//! protocol need first-class state in the table.

#![cfg_attr(not(test), forbid(unsafe_code))]
// Handlers take `&mut self` and `&mut S` uniformly so the dispatch table
// inlines without per-arm coercions.
#![allow(
    clippy::needless_pass_by_ref_mut,
    clippy::unused_self,
    clippy::match_same_arms,
    clippy::missing_const_for_fn
)]

use std::fmt;

pub mod bidi;
pub mod kitty_graphics;
pub mod kitty_text_sizing;
pub mod notification;
#[cfg(feature = "state-dump")]
pub mod state;
pub mod utf8;

mod machine;

#[cfg(kani)]
mod kani_proofs;

/// The Williams reference notes "16 parameters maximum" as the practical
/// xterm limit.
pub const MAX_PARAMS: usize = 16;

/// The Williams reference and xterm allow two; three leaves room for
/// future final-byte assignments.
pub const MAX_INTERMEDIATES: usize = 3;

/// Cap on a single OSC body before truncation + [`Sink::osc_overflow`].
///
/// The notification spec caps an OSC 99 chunk's encoded payload at 4096
/// bytes, but the buffered body is `99;<controls>;<payload>`, so a cap of
/// exactly 4096 chops the tail off every spec-legal full-size chunk.
pub const OSC_BUFFER_LIMIT: usize = 8192;

/// Cap on a single APC body before truncation + [`Sink::apc_overflow`].
///
/// Set above 4096 bytes because the buffered body includes `G<controls>;`
/// prefix around the 4096-byte chunk payload.
pub const APC_BUFFER_LIMIT: usize = 8192;

const _: () = assert!(APC_BUFFER_LIMIT > 4096 && OSC_BUFFER_LIMIT > 4096);

/// Williams DFA states plus DCS / OSC sub-states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
}

/// Sink the parser drives; each method is a Williams action.
///
/// Implementations may defer the work: the parser only guarantees it
/// won't call the same dispatch back-to-back without the bytes that
/// distinguish them.
#[allow(unused_variables)]
pub trait Sink {
    fn print(&mut self, byte: u8) {}
    /// `bytes` is non-empty and contains only `0x20..=0x7E`.
    fn print_str(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.print(b);
        }
    }
    /// UTF-8 analogue of [`Sink::print_str`].
    ///
    /// Non-empty slice starting with `0x80..=0xFF` and containing only
    /// printable bytes (`0x20..=0x7E | 0x80..=0xFF`). Interleaved ASCII avoids
    /// splitting at word boundaries.
    fn print_utf8_run(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.print(b);
        }
    }
    fn execute(&mut self, byte: u8) {}
    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {}
    /// `subparams` is a bitmap over `params`: bit `i` (i ≥ 1) is set when
    /// slot `i` was opened by `:` rather than `;` (`SGR 4:3` curly underline
    /// vs `SGR 4;3` underline+italic). Bit 0 is unused.
    ///
    /// `ignore` indicates parameter or intermediate overflow.
    fn csi_dispatch(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        ignore: bool,
        final_byte: u8,
    ) {
    }
    fn dcs_hook(&mut self, params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {}
    fn dcs_put(&mut self, byte: u8) {}
    fn dcs_unhook(&mut self) {}
    /// OSC dispatch for bytes between introducer and terminator.
    ///
    /// Truncated to [`OSC_BUFFER_LIMIT`], numeric prefix included.
    /// `bell_terminated` is true for `BEL`, false for `ST`.
    fn osc_dispatch(&mut self, body: &[u8], bell_terminated: bool) {}
    /// Fires at least once per overflowing payload but not necessarily per
    /// dropped byte: the bulk body path coalesces a dropped run into one call.
    fn osc_overflow(&mut self) {}
    /// `body` is the raw bytes between the `_` introducer and the terminating
    /// ST, truncated to [`APC_BUFFER_LIMIT`], in which case
    /// [`Sink::apc_overflow`] fires before this dispatch.
    fn apc_dispatch(&mut self, body: &[u8]) {}
    /// Same coalescing contract as [`Sink::osc_overflow`].
    fn apc_overflow(&mut self) {}
    /// Asked once after each APC dispatch by
    /// [`Parser::advance_until_yield`]; `true` stops the parse there.
    fn take_yield(&mut self) -> bool {
        false
    }
}

#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Parser {
    state: State,
    params: Params,
    intermediates: Intermediates,
    osc: OscBuffer,
    apc: ApcBuffer,
    sos_pm_apc_kind: Option<SosPmApcKind>,
}

/// APC is the only flavor the parser dispatches; SOS / PM are discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
enum SosPmApcKind {
    Sos,
    Pm,
    Apc,
}

impl fmt::Debug for Parser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parser")
            .field("state", &self.state)
            .field("params_len", &self.params.len)
            .field("intermediates_len", &self.intermediates.len)
            .field("osc_len", &self.osc.bytes.len())
            .field("apc_len", &self.apc.bytes.len())
            .field("sos_pm_apc_kind", &self.sos_pm_apc_kind)
            .finish()
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

const SWAR_ONES: u64 = 0x0101_0101_0101_0101;

const SWAR_HIGH: u64 = 0x8080_8080_8080_8080;

/// Generic rather than fn-pointer so `mask_fn` inlines into the chunk loop.
#[inline]
fn scan_until(buf: &[u8], mask_fn: impl Fn(u64) -> u64, scalar_pred: impl Fn(u8) -> bool) -> usize {
    let mut i = 0;
    let (chunks, remainder) = buf.as_chunks::<8>();
    for chunk in chunks {
        // Little-endian load: the first offending byte is then
        // `trailing_zeros / 8`.
        let mask = mask_fn(u64::from_le_bytes(*chunk));
        if mask != 0 {
            return i + (mask.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    for &b in remainder {
        if scalar_pred(b) {
            return i;
        }
        i += 1;
    }
    i
}

#[inline]
fn scan_printable_run(buf: &[u8]) -> usize {
    scan_until(buf, nonprintable_mask, |b| !(0x20..=0x7E).contains(&b))
}

/// Per-lane `0x80` bit set for `b >= 0x80`, `b < 0x20`, or `b == 0x7F`.
///
/// Low-to-high borrows can corrupt higher lanes, but the lowest set lane
/// is always the exact first non-printable byte. `kani_proofs` pins this.
#[inline]
const fn nonprintable_mask(x: u64) -> u64 {
    let ge80 = x & SWAR_HIGH;
    let lt20 = x.wrapping_sub(SWAR_ONES.wrapping_mul(0x20)) & !x & SWAR_HIGH;
    let xor7f = x ^ SWAR_ONES.wrapping_mul(0x7F);
    let eq7f = xor7f.wrapping_sub(SWAR_ONES) & !xor7f & SWAR_HIGH;
    // `|` → `^` is an equivalent mutant: the three predicates are mutually
    // exclusive in the lowest set lane.
    ge80 | lt20 | eq7f
}

/// Leading run of printable ASCII or non-ASCII bytes, ending at the first
/// C0 control or DEL.
#[inline]
fn scan_mixed_print_run(buf: &[u8]) -> usize {
    scan_until(buf, control_or_del_mask, |b| b < 0x20 || b == 0x7F)
}

/// [`nonprintable_mask`] without the `>= 0x80` term; same borrow caveat.
#[inline]
const fn control_or_del_mask(x: u64) -> u64 {
    let lt20 = x.wrapping_sub(SWAR_ONES.wrapping_mul(0x20)) & !x & SWAR_HIGH;
    let xor7f = x ^ SWAR_ONES.wrapping_mul(0x7F);
    let eq7f = xor7f.wrapping_sub(SWAR_ONES) & !xor7f & SWAR_HIGH;
    lt20 | eq7f
}

/// Leading run of bytes an `OscString` / `SosPmApcString` body absorbs
/// verbatim. It ends at BEL, CAN, SUB, or ESC: exactly the bytes the
/// per-byte handlers treat specially.
#[inline]
fn scan_string_body(buf: &[u8]) -> usize {
    scan_until(buf, string_special_mask, |b| {
        matches!(b, 0x07 | 0x18 | 0x1A | 0x1B)
    })
}

/// Same borrow caveat as [`nonprintable_mask`].
#[inline]
const fn string_special_mask(x: u64) -> u64 {
    let bel = x ^ SWAR_ONES.wrapping_mul(0x07);
    let eq_bel = bel.wrapping_sub(SWAR_ONES) & !bel & SWAR_HIGH;
    let can = x ^ SWAR_ONES.wrapping_mul(0x18);
    let eq_can = can.wrapping_sub(SWAR_ONES) & !can & SWAR_HIGH;
    let sub = x ^ SWAR_ONES.wrapping_mul(0x1A);
    let eq_sub = sub.wrapping_sub(SWAR_ONES) & !sub & SWAR_HIGH;
    let esc = x ^ SWAR_ONES.wrapping_mul(0x1B);
    let eq_esc = esc.wrapping_sub(SWAR_ONES) & !esc & SWAR_HIGH;
    // `|` → `^` is an equivalent mutant: the four target bytes are
    // distinct, so at most one probe lights any given lane.
    eq_bel | eq_can | eq_sub | eq_esc
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize, Default),
    serde(default)
)]
struct Params {
    values: [u16; MAX_PARAMS],
    len: usize,
    overflowed: bool,
    /// Bit `i` (i ≥ 1) set when slot `i` was opened by `:` (sub-param of
    /// slot `i-1`); cleared when opened by `;`. Bit 0 is unused.
    subparam_mask: u32,
}

impl Params {
    const fn new() -> Self {
        Self {
            values: [0; MAX_PARAMS],
            len: 0,
            overflowed: false,
            subparam_mask: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.overflowed = false;
        self.values = [0; MAX_PARAMS];
        self.subparam_mask = 0;
    }

    fn start_with(&mut self, byte: u8) {
        // An empty CSI (`CSI H`) dispatches from CsiEntry without entering
        // CsiParam, so the first byte here is always a digit or `;`.
        if byte == b';' {
            self.next_slot(false);
        } else {
            self.len = 1;
            self.values[0] = u16::from(byte - b'0');
        }
    }

    fn start_empty(&mut self) {
        // CSI starting with `:`: an empty slot 0, with slot 1 opened as a
        // sub-param once a digit arrives.
        self.len = 1;
        self.values[0] = 0;
    }

    fn push_digit(&mut self, digit: u8) {
        if self.len == 0 {
            self.len = 1;
        }
        let idx = self.len - 1;
        let v = self.values[idx]
            .saturating_mul(10)
            .saturating_add(u16::from(digit));
        self.values[idx] = v;
    }

    /// Consume the leading run of parameter bytes (`0x30..=0x3B`) from
    /// `buf`, returning how many were eaten. The result equals feeding each
    /// byte to `push_digit` / `next_slot`, as the per-byte `CsiParam`
    /// handler does, so a run split across `advance` chunks is byte-for-byte
    /// equivalent.
    #[inline(never)]
    fn push_run(&mut self, buf: &[u8]) -> usize {
        // `len == 0` implies `values[0] == 0`: only `clear` empties the list.
        // Loading the slot anyway reads `clear`'s wide zeroing store, which
        // blocks store forwarding once per CSI.
        let (mut len, mut value) = match self.len {
            0 => (1, 0),
            len => (len, u32::from(self.values[len - 1])),
        };
        let mut consumed = buf.len();
        for (i, &b) in buf.iter().enumerate() {
            let digit = b.wrapping_sub(b'0');
            if digit < 10 {
                value = (value * 10 + u32::from(digit)).min(u16::MAX.into());
            } else if digit == 10 || digit == 11 {
                self.values[len - 1] = value as u16;
                if len < MAX_PARAMS {
                    len += 1;
                    value = 0;
                    if digit == 10 {
                        self.subparam_mask |= 1 << (len - 1);
                    }
                } else {
                    self.overflowed = true;
                }
            } else {
                consumed = i;
                break;
            }
        }
        if consumed > 0 {
            self.values[len - 1] = value as u16;
            self.len = len;
        }
        consumed
    }

    fn next_slot(&mut self, is_sub: bool) {
        if self.len == 0 {
            self.len = 1;
        }
        if self.len < MAX_PARAMS {
            self.len += 1;
            self.values[self.len - 1] = 0;
            if is_sub {
                self.subparam_mask |= 1 << (self.len - 1);
            }
        } else {
            self.overflowed = true;
        }
    }

    fn as_slice(&self) -> &[u16] {
        &self.values[..self.len]
    }

    const fn subparams(&self) -> u32 {
        self.subparam_mask
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize, Default),
    serde(default)
)]
struct Intermediates {
    bytes: [u8; MAX_INTERMEDIATES],
    len: usize,
    overflowed: bool,
}

impl Intermediates {
    const fn new() -> Self {
        Self {
            bytes: [0; MAX_INTERMEDIATES],
            len: 0,
            overflowed: false,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }

    fn push(&mut self, byte: u8) {
        if self.len < MAX_INTERMEDIATES {
            self.bytes[self.len] = byte;
            self.len += 1;
        } else {
            self.overflowed = true;
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitedBuffer<const LIMIT: usize> {
    bytes: Vec<u8>,
}

impl<const LIMIT: usize> LimitedBuffer<LIMIT> {
    const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn clear(&mut self) {
        self.bytes.clear();
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.bytes.len() < LIMIT {
            self.bytes.push(byte);
            true
        } else {
            false
        }
    }

    /// Bulk analogue of [`Self::push`]; returns `false` when any byte
    /// was dropped.
    fn extend(&mut self, run: &[u8]) -> bool {
        let take = run.len().min(LIMIT.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&run[..take]);
        take == run.len()
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

type OscBuffer = LimitedBuffer<OSC_BUFFER_LIMIT>;

/// Kitty graphics' `key=value;…;<base64>` framing lives in felis-grid.
type ApcBuffer = LimitedBuffer<APC_BUFFER_LIMIT>;

/// The remainder is `None` when the body carries no `;` at all, which
/// several dispatches read differently from a present-but-empty
/// remainder: `OSC 1 ST` sets no icon name where `OSC 1 ; ST` clears it.
#[must_use]
pub fn split_osc_first(body: &[u8]) -> (&[u8], Option<&[u8]>) {
    match body.iter().position(|b| *b == b';') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    }
}

/// `None` when the prefix is empty or is not a `u16`'s worth of decimal
/// digits; no OSC felis answers has such a code.
#[must_use]
pub fn osc_number(body: &[u8]) -> Option<(u16, Option<&[u8]>)> {
    let (code, rest) = split_osc_first(body);
    if code.is_empty() {
        return None;
    }
    let mut number: u16 = 0;
    for &b in code {
        let digit = b.checked_sub(b'0').filter(|d| *d < 10)?;
        number = number.checked_mul(10)?.checked_add(u16::from(digit))?;
    }
    Some((number, rest))
}

/// The `;`-delimited fields of an OSC body, for the fixed-arity codes
/// (OSC 4/5/104/105 color lists, OSC 10/11/12 spec runs). A payload that
/// is one opaque string is a subslice of the body instead, so a literal
/// `;` inside it survives.
pub fn split_osc(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|b| *b == b';')
}

#[cfg(test)]
mod tests;
