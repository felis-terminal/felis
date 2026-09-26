//! SGR (Select Graphic Rendition) attribute model.
//! The parser flattens `:` and `;` into one slice with a sub-param bitmap.
//! This distinguishes sub-params like `4:3` (curly) from `4;3` (underline + italic),
//! and `38:2::r:g:b` from `38;2;r;g;b`.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Cell-level style flags.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct AttrFlags: u16 {
        /// `SGR 1`.
        const BOLD          = 1 << 0;
        /// `SGR 2`.
        const FAINT         = 1 << 1;
        /// `SGR 3`.
        const ITALIC        = 1 << 2;
        /// `SGR 4` / `SGR 21` / `SGR 4:1..5`; the shape lives on
        /// [`Attributes::underline_style`].
        const UNDERLINE     = 1 << 3;
        /// `SGR 5` / `SGR 6`, collapsed.
        const BLINK         = 1 << 4;
        /// `SGR 7`.
        const REVERSE       = 1 << 5;
        /// `SGR 8`.
        const CONCEAL       = 1 << 6;
        /// `SGR 9`.
        const STRIKETHROUGH = 1 << 7;
        /// `SGR 53`.
        const OVERLINE      = 1 << 8;
        /// DECSCA (`CSI 1 " q`): survives DECSEL / DECSED, not plain
        /// ED / EL. Per-cell, as in xterm, so a protected stretch
        /// survives a pen reset.
        const PROTECTED     = 1 << 9;
        /// ECMA-48 `SPA` / `EPA`. A separate bit because DECSERA
        /// respects [`Self::PROTECTED`] but not this one (esctest's
        /// `test_DECSERA_doesNotRespectISOProtect`), while DECSED and
        /// DECSEL skip both.
        const ISO_PROTECTED = 1 << 10;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnderlineStyle {
    /// `SGR 4` or `SGR 4:1`.
    #[default]
    Single,
    /// `SGR 21` or `SGR 4:2`.
    Double,
    /// `SGR 4:3`.
    Curly,
    /// `SGR 4:4`.
    Dotted,
    /// `SGR 4:5`.
    Dashed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    /// `0..=15` the xterm 16 colors, `16..=231` the 6×6×6 cube,
    /// `232..=255` the grayscale ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributes {
    pub fg: Color,
    pub bg: Color,
    /// `SGR 58`. [`Color::Default`] means "follow the foreground";
    /// the renderer substitutes, not the grid.
    pub underline_color: Color,
    pub flags: AttrFlags,
    /// Meaningful only when [`AttrFlags::UNDERLINE`] is set.
    pub underline_style: UnderlineStyle,
}

impl Color {
    /// Bijective, which [`Attributes`]'s `Hash` relies on.
    const fn pack(self) -> u32 {
        match self {
            Self::Default => 0,
            Self::Indexed(n) => 0x0100_0000 | n as u32,
            Self::Rgb(r, g, b) => 0x0200_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        }
    }
}

/// Not derived: the per-field derive (~10 hasher writes) is ~9% of the
/// parse thread on an SGR flood, through
/// [`crate::style_table::StyleTable::intern`]. The packing is
/// bijective, so the derived `PartialEq` stays the equality.
impl core::hash::Hash for Attributes {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        let [lo, hi] = self.pack();
        state.write_u64(lo);
        state.write_u64(hi);
    }
}

// The flags occupy bits 32..48 of the hi word; wider ones would collide
// with the underline style at bit 48.
const _: () = assert!((AttrFlags::all().bits() as u64) << 32 < 1 << 48);

impl Attributes {
    /// Bijective: equal words mean equal attributes.
    pub(crate) const fn pack(&self) -> [u64; 2] {
        let lo = (self.fg.pack() as u64) | ((self.bg.pack() as u64) << 32);
        let hi = (self.underline_color.pack() as u64)
            | ((self.flags.bits() as u64) << 32)
            | ((self.underline_style as u64) << 48);
        [lo, hi]
    }

    /// `subparams` is the parser's bitmap: bit `i` is set when slot `i`
    /// was opened by `:`. A malformed extended sequence is dropped
    /// silently, per ECMA-48 ("ignore parameters you don't
    /// understand", not "abort the run").
    pub fn apply_sgr(&mut self, params: &[u16], subparams: u32) {
        if params.is_empty() {
            *self = Self::default();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            let consumed = match p {
                0 => {
                    *self = Self::default();
                    1
                }
                1 => {
                    self.flags.insert(AttrFlags::BOLD);
                    1
                }
                2 => {
                    self.flags.insert(AttrFlags::FAINT);
                    1
                }
                3 => {
                    self.flags.insert(AttrFlags::ITALIC);
                    1
                }
                4 => {
                    // Only one sub-param counts; a further one is
                    // illegal per kitty/foot and ignored.
                    let next_is_sub = next_is_subparam(subparams, i);
                    if next_is_sub {
                        // An unknown shape code keeps the underline on.
                        let style_code = params.get(i + 1).copied().unwrap_or(0);
                        if style_code == 0 {
                            self.flags.remove(AttrFlags::UNDERLINE);
                            self.underline_style = UnderlineStyle::default();
                        } else {
                            self.flags.insert(AttrFlags::UNDERLINE);
                            self.underline_style = match style_code {
                                2 => UnderlineStyle::Double,
                                3 => UnderlineStyle::Curly,
                                4 => UnderlineStyle::Dotted,
                                5 => UnderlineStyle::Dashed,
                                _ => UnderlineStyle::Single,
                            };
                        }
                        2
                    } else {
                        self.flags.insert(AttrFlags::UNDERLINE);
                        self.underline_style = UnderlineStyle::Single;
                        1
                    }
                }
                5 | 6 => {
                    self.flags.insert(AttrFlags::BLINK);
                    1
                }
                7 => {
                    self.flags.insert(AttrFlags::REVERSE);
                    1
                }
                8 => {
                    self.flags.insert(AttrFlags::CONCEAL);
                    1
                }
                9 => {
                    self.flags.insert(AttrFlags::STRIKETHROUGH);
                    1
                }
                21 => {
                    self.flags.insert(AttrFlags::UNDERLINE);
                    self.underline_style = UnderlineStyle::Double;
                    1
                }
                22 => {
                    self.flags.remove(AttrFlags::BOLD | AttrFlags::FAINT);
                    1
                }
                23 => {
                    self.flags.remove(AttrFlags::ITALIC);
                    1
                }
                24 => {
                    self.flags.remove(AttrFlags::UNDERLINE);
                    self.underline_style = UnderlineStyle::default();
                    1
                }
                25 => {
                    self.flags.remove(AttrFlags::BLINK);
                    1
                }
                27 => {
                    self.flags.remove(AttrFlags::REVERSE);
                    1
                }
                28 => {
                    self.flags.remove(AttrFlags::CONCEAL);
                    1
                }
                29 => {
                    self.flags.remove(AttrFlags::STRIKETHROUGH);
                    1
                }
                30..=37 => {
                    self.fg = Color::Indexed((p - 30) as u8);
                    1
                }
                38 => {
                    let (color, n) = parse_extended(params, i, subparams);
                    if let Some(c) = color {
                        self.fg = c;
                    }
                    1 + n
                }
                39 => {
                    self.fg = Color::Default;
                    1
                }
                40..=47 => {
                    self.bg = Color::Indexed((p - 40) as u8);
                    1
                }
                48 => {
                    let (color, n) = parse_extended(params, i, subparams);
                    if let Some(c) = color {
                        self.bg = c;
                    }
                    1 + n
                }
                49 => {
                    self.bg = Color::Default;
                    1
                }
                53 => {
                    self.flags.insert(AttrFlags::OVERLINE);
                    1
                }
                55 => {
                    self.flags.remove(AttrFlags::OVERLINE);
                    1
                }
                58 => {
                    let (color, n) = parse_extended(params, i, subparams);
                    if let Some(c) = color {
                        self.underline_color = c;
                    }
                    1 + n
                }
                59 => {
                    self.underline_color = Color::Default;
                    1
                }
                90..=97 => {
                    self.fg = Color::Indexed((p - 90 + 8) as u8);
                    1
                }
                100..=107 => {
                    self.bg = Color::Indexed((p - 100 + 8) as u8);
                    1
                }
                _ => 1,
            };
            i += consumed;
        }
    }
}

pub(crate) const fn next_is_subparam(subparams: u32, i: usize) -> bool {
    let next = i + 1;
    next < 32 && (subparams & (1u32 << next)) != 0
}

/// Color payload after `SGR 38`/`48`/`58` and count of extra params consumed.
/// Malformed payloads return `(None, 0)` to skip bad params individually.
/// Colon forms `[2, cs, r, g, b]` carry a color-space slot absent in classic forms,
/// distinguished only by the sub-param mask.
fn parse_extended(params: &[u16], disc_idx: usize, subparams: u32) -> (Option<Color>, usize) {
    let tail = &params[disc_idx + 1..];
    let colon_form = next_is_subparam(subparams, disc_idx);
    match tail.first().copied() {
        Some(5) => {
            if let Some(&n) = tail.get(1) {
                (Some(Color::Indexed(n.min(255) as u8)), 2)
            } else {
                (None, 0)
            }
        }
        Some(2) if colon_form => {
            // The whole sub-param run is consumed either way so a
            // trailing slot cannot leak into the next SGR.
            let run = subparam_group_len(subparams, disc_idx, tail.len());
            if run >= 5 {
                let r = component(tail[2]);
                let g = component(tail[3]);
                let b = component(tail[4]);
                (Some(Color::Rgb(r, g, b)), run)
            } else if run >= 4 {
                let r = component(tail[1]);
                let g = component(tail[2]);
                let b = component(tail[3]);
                (Some(Color::Rgb(r, g, b)), run)
            } else {
                (None, 0)
            }
        }
        Some(2) => {
            if tail.len() >= 4 {
                let r = component(tail[1]);
                let g = component(tail[2]);
                let b = component(tail[3]);
                return (Some(Color::Rgb(r, g, b)), 4);
            }
            (None, 0)
        }
        _ => (None, 0),
    }
}

/// Length of the sub-param run starting at `tail[0]`.
const fn subparam_group_len(subparams: u32, disc_idx: usize, tail_len: usize) -> usize {
    let mut len = 1usize;
    while len < tail_len {
        let slot = disc_idx + 1 + len;
        if slot < 32 && (subparams & (1u32 << slot)) != 0 {
            len += 1;
        } else {
            break;
        }
    }
    len
}

fn component(p: u16) -> u8 {
    p.min(255) as u8
}

#[cfg(test)]
mod tests;
