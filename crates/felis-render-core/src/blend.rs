//! How a coverage mask (a glyph, a line decoration) is weighted when it
//! blends in linear space: the remap of
//! `docs/explanation/rendering/pipeline.md` "Text weight under linear
//! blending". A GPU backend applies it per fragment; this is the
//! reference its shader mirrors.

use crate::palette::{linear_to_srgb, srgb_to_linear};

/// Closer than this, the remap divides by a near-zero span and is
/// skipped; the two colors are then indistinguishable anyway.
pub const MIN_LUMINANCE_SPAN: f32 = 0.001;

/// The coverage at which a linear-space blend of a color of relative
/// luminance `fg_luminance` over one of `bg_luminance` reaches the
/// luminance an sRGB-space blend at `coverage` reaches. All three are
/// in `0.0..=1.0`; the result is too.
#[must_use]
pub fn linear_corrected_coverage(coverage: f32, fg_luminance: f32, bg_luminance: f32) -> f32 {
    let span = fg_luminance - bg_luminance;
    if span.abs() <= MIN_LUMINANCE_SPAN {
        return coverage;
    }
    let (fg, bg) = (linear_to_srgb(fg_luminance), linear_to_srgb(bg_luminance));
    let blended = srgb_to_linear((fg - bg).mul_add(coverage, bg));
    ((blended - bg_luminance) / span).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::float_cmp,
        reason = "the passthrough returns its input bit for bit"
    )]

    use proptest::prelude::*;

    use super::*;

    const TOLERANCE: f32 = 1e-3;

    fn distinct_pair() -> impl Strategy<Value = (f32, f32)> {
        (0.0f32..=1.0, 0.0f32..=1.0).prop_filter("luminances the remap applies to", |(fg, bg)| {
            (fg - bg).abs() > MIN_LUMINANCE_SPAN
        })
    }

    proptest! {
        /// No ink stays no ink and full ink stays full ink.
        #[test]
        fn empty_and_full_coverage_are_fixed_points((fg, bg) in distinct_pair()) {
            prop_assert!(linear_corrected_coverage(0.0, fg, bg) < TOLERANCE);
            prop_assert!(linear_corrected_coverage(1.0, fg, bg) > 1.0 - TOLERANCE);
        }

        /// More ink in the mask never paints less.
        #[test]
        fn the_remap_is_monotonic_in_coverage(
            (fg, bg) in distinct_pair(),
            a in 0.0f32..=1.0,
            b in 0.0f32..=1.0,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(
                linear_corrected_coverage(lo, fg, bg) <= linear_corrected_coverage(hi, fg, bg) + 1e-6
            );
        }

        /// Colors of (nearly) equal luminance keep the mask as it is.
        #[test]
        fn equal_luminances_pass_coverage_through(
            l in 0.0f32..=1.0,
            delta in -MIN_LUMINANCE_SPAN..=MIN_LUMINANCE_SPAN,
            a in 0.0f32..=1.0,
        ) {
            let other = (l + delta).clamp(0.0, 1.0);
            prop_assert_eq!(linear_corrected_coverage(a, l, other), a);
        }

        /// For gray on gray, luminance is every channel, so blending the
        /// linear colors at the remapped coverage paints exactly what
        /// blending their sRGB encodings at the mask's coverage paints.
        #[test]
        fn gray_on_gray_matches_srgb_space_blending(
            (fg, bg) in distinct_pair(),
            a in 0.0f32..=1.0,
        ) {
            let painted = (fg - bg).mul_add(linear_corrected_coverage(a, fg, bg), bg);
            let srgb_blend = srgb_to_linear(
                (linear_to_srgb(fg) - linear_to_srgb(bg)).mul_add(a, linear_to_srgb(bg)),
            );
            prop_assert!((painted - srgb_blend).abs() < 1e-4, "{painted} vs {srgb_blend}");
        }

        /// Light text on a dark ground loses edge weight, dark text on a
        /// light ground gains it: the two directions linear blending
        /// errs in.
        #[test]
        fn the_remap_thins_light_on_dark_and_thickens_dark_on_light(
            (fg, bg) in distinct_pair(),
            a in 0.0f32..=1.0,
        ) {
            let corrected = linear_corrected_coverage(a, fg, bg);
            if fg > bg {
                prop_assert!(corrected <= a + 1e-5, "{corrected} > {a}");
            } else {
                prop_assert!(corrected >= a - 1e-5, "{corrected} < {a}");
            }
        }
    }
}
