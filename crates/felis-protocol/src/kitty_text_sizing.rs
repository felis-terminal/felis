//! Kitty text-sizing protocol vocabulary: [`Sizing`], [`VAlign`], and [`HAlign`].
//!
//! Spec source: `docs/reference/protocols/kitty-text-sizing.md`.

use serde::{Deserialize, Serialize};

/// Vertical-alignment value of `v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VAlign {
    /// `v=0`. The default.
    #[default]
    Top,
    /// `v=1`.
    Bottom,
    /// `v=2`.
    Center,
}

/// Horizontal-alignment value of `h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HAlign {
    /// `h=0`. The default.
    #[default]
    Left,
    /// `h=1`.
    Right,
    /// `h=2`.
    Center,
}

/// One run's sizing parameters. `Default` is the spec's defaults.
///
/// Constructors enforce range bounds (`s` 1..=7, `w` 0..=7, `n`/`d` 0..=15,
/// and `d == 0 || n < d`) on wire deserialization and OSC 66 parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SizingWire", into = "SizingWire")]
pub struct Sizing {
    scale: u8,
    cell_width: u8,
    frac_num: u8,
    frac_den: u8,
    valign: VAlign,
    halign: HAlign,
}

impl Sizing {
    /// `None` when any field is outside its spec range or
    /// `frac_den != 0 && frac_num >= frac_den`. `frac_num` stays
    /// representable while `frac_den == 0` so a parsed `n=5` without
    /// `d` keeps its wire-visible value.
    #[must_use]
    pub const fn new(
        scale: u8,
        cell_width: u8,
        frac_num: u8,
        frac_den: u8,
        valign: VAlign,
        halign: HAlign,
    ) -> Option<Self> {
        if scale < 1 || scale > 7 || cell_width > 7 || frac_num > 15 || frac_den > 15 {
            return None;
        }
        if frac_den != 0 && frac_num >= frac_den {
            return None;
        }
        Some(Self {
            scale,
            cell_width,
            frac_num,
            frac_den,
            valign,
            halign,
        })
    }

    /// `s`. Integer scale, guaranteed 1..=7.
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }

    /// `w`. Cell-width override, guaranteed 0..=7; 0 means
    /// "auto-derive from `text`'s grapheme width".
    #[must_use]
    pub const fn cell_width(self) -> u8 {
        self.cell_width
    }

    /// `n`. Fractional-scale numerator, guaranteed 0..=15 and
    /// `< frac_den` whenever `frac_den != 0`.
    #[must_use]
    pub const fn frac_num(self) -> u8 {
        self.frac_num
    }

    /// `d`. Fractional-scale denominator, guaranteed 0..=15. `0`
    /// means "no fractional component" (`n` is then ignored).
    #[must_use]
    pub const fn frac_den(self) -> u8 {
        self.frac_den
    }

    /// `v`. Vertical alignment.
    #[must_use]
    pub const fn valign(self) -> VAlign {
        self.valign
    }

    /// `h`. Horizontal alignment.
    #[must_use]
    pub const fn halign(self) -> HAlign {
        self.halign
    }
}

impl Default for Sizing {
    fn default() -> Self {
        Self {
            scale: 1,
            cell_width: 0,
            frac_num: 0,
            frac_den: 0,
            valign: VAlign::Top,
            halign: HAlign::Left,
        }
    }
}

/// Serde twin of [`Sizing`] so deserialization funnels into the
/// checked constructor. The field order is the stored `.fcast` layout
/// (positional encoding); reordering breaks replay of existing
/// recordings. The daemon wire carries the row codec's fixed six bytes
/// instead (`docs/reference/row-codec.md`).
#[derive(Serialize, Deserialize)]
struct SizingWire {
    scale: u8,
    cell_width: u8,
    frac_num: u8,
    frac_den: u8,
    valign: VAlign,
    halign: HAlign,
}

/// Rejection carried out of [`Sizing`]'s `TryFrom` deserialization.
#[derive(Debug)]
pub struct InvalidSizing;

impl core::fmt::Display for InvalidSizing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("OSC 66 sizing fields outside their spec ranges")
    }
}

impl TryFrom<SizingWire> for Sizing {
    type Error = InvalidSizing;

    fn try_from(wire: SizingWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.scale,
            wire.cell_width,
            wire.frac_num,
            wire.frac_den,
            wire.valign,
            wire.halign,
        )
        .ok_or(InvalidSizing)
    }
}

impl From<Sizing> for SizingWire {
    fn from(sizing: Sizing) -> Self {
        Self {
            scale: sizing.scale,
            cell_width: sizing.cell_width,
            frac_num: sizing.frac_num,
            frac_den: sizing.frac_den,
            valign: sizing.valign,
            halign: sizing.halign,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoded bytes are frozen: a reorder breaks this test before
    /// it breaks the replay of a recording.
    #[test]
    fn sizing_serde_bytes_are_frozen() {
        let sizing = Sizing::new(2, 3, 1, 4, VAlign::Center, HAlign::Right).unwrap();
        let bytes = postcard::to_allocvec(&sizing).unwrap();
        assert_eq!(bytes, [2, 3, 1, 4, 2, 1]);
        assert_eq!(postcard::from_bytes::<Sizing>(&bytes).unwrap(), sizing);
    }

    /// A stored recording with out-of-range fields fails to
    /// deserialize rather than materializing an invalid `Sizing`.
    #[test]
    fn out_of_range_stored_bytes_are_rejected_on_deserialize() {
        assert!(postcard::from_bytes::<Sizing>(&[0, 3, 1, 4, 2, 1]).is_err());
        assert!(postcard::from_bytes::<Sizing>(&[2, 3, 5, 4, 2, 1]).is_err());
    }
}
