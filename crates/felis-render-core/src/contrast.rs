//! `theme.minimum_contrast`: raising a foreground to a WCAG 2 contrast
//! ratio against its cell background. Colors are linear RGB, which is
//! what WCAG's relative luminance is defined over.

/// The highest ratio WCAG 2 defines: white against black.
pub const MAX_CONTRAST_RATIO: f32 = 21.0;

const BLACK: [f32; 3] = [0.0, 0.0, 0.0];
const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
const BISECTION_STEPS: u32 = 20;

#[must_use]
pub fn relative_luminance(c: [f32; 4]) -> f32 {
    0.0722f32.mul_add(c[2], 0.2126f32.mul_add(c[0], 0.7152 * c[1]))
}

#[must_use]
pub fn contrast_ratio(a: [f32; 4], b: [f32; 4]) -> f32 {
    let (ya, yb) = (relative_luminance(a), relative_luminance(b));
    (ya.max(yb) + 0.05) / (ya.min(yb) + 0.05)
}

/// Moves `fg` along the Oklab line toward white or black (hue kept),
/// the side it sits on first, by the least amount that reaches
/// `minimum`; out of reach, the better of white and black.
#[must_use]
pub fn ensure_contrast(fg: [f32; 4], bg: [f32; 4], minimum: f32) -> [f32; 4] {
    if minimum.is_nan() || minimum <= 1.0 || contrast_ratio(fg, bg) >= minimum {
        return fg;
    }
    let lighter_first = relative_luminance(fg) >= relative_luminance(bg);
    let order = if lighter_first {
        [WHITE, BLACK]
    } else {
        [BLACK, WHITE]
    };
    let with_alpha = |[r, g, b]: [f32; 3]| [r, g, b, fg[3]];
    for extreme in order {
        if contrast_ratio(with_alpha(extreme), bg) >= minimum {
            return with_alpha(bisect_toward(fg, extreme, bg, minimum));
        }
    }
    let [first, second] = order.map(with_alpha);
    if contrast_ratio(first, bg) >= contrast_ratio(second, bg) {
        first
    } else {
        second
    }
}

/// `extreme` itself passes; the invariant that the upper end always
/// passes is what makes the result meet `minimum`.
fn bisect_toward(fg: [f32; 4], extreme: [f32; 3], bg: [f32; 4], minimum: f32) -> [f32; 3] {
    let from = linear_to_oklab([fg[0], fg[1], fg[2]]);
    let to = linear_to_oklab(extreme);
    let at = |t: f32| -> [f32; 3] {
        if t >= 1.0 {
            return extreme;
        }
        let lab = [0, 1, 2].map(|i| (to[i] - from[i]).mul_add(t, from[i]));
        oklab_to_linear(lab).map(|c| c.clamp(0.0, 1.0))
    };
    let passes = |rgb: [f32; 3]| contrast_ratio([rgb[0], rgb[1], rgb[2], 1.0], bg) >= minimum;
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    let mut best = extreme;
    for _ in 0..BISECTION_STEPS {
        let mid = lo.midpoint(hi);
        let rgb = at(mid);
        if passes(rgb) {
            hi = mid;
            best = rgb;
        } else {
            lo = mid;
        }
    }
    best
}

type Matrix = [[f32; 3]; 3];

const LINEAR_TO_LMS: Matrix = [
    [0.412_221_46, 0.536_332_55, 0.051_445_995],
    [0.211_903_5, 0.680_699_5, 0.107_396_96],
    [0.088_302_46, 0.281_718_84, 0.629_978_7],
];
const LMS_TO_OKLAB: Matrix = [
    [0.210_454_26, 0.793_617_8, -0.004_072_047],
    [1.977_998_5, -2.428_592_2, 0.450_593_7],
    [0.025_904_037, 0.782_771_77, -0.808_675_77],
];
const OKLAB_TO_LMS: Matrix = [
    [1.0, 0.396_337_78, 0.215_803_76],
    [1.0, -0.105_561_346, -0.063_854_17],
    [1.0, -0.089_484_18, -1.291_485_5],
];
const LMS_TO_LINEAR: Matrix = [
    [4.076_741_7, -3.307_711_6, 0.230_969_94],
    [-1.268_438, 2.609_757_4, -0.341_319_4],
    [-0.004_196_086_3, -0.703_418_6, 1.707_614_7],
];

fn apply(matrix: &Matrix, v: [f32; 3]) -> [f32; 3] {
    matrix.map(|row| row[2].mul_add(v[2], row[0].mul_add(v[0], row[1] * v[1])))
}

fn linear_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    apply(&LMS_TO_OKLAB, apply(&LINEAR_TO_LMS, rgb).map(f32::cbrt))
}

fn oklab_to_linear(lab: [f32; 3]) -> [f32; 3] {
    apply(&LMS_TO_LINEAR, apply(&OKLAB_TO_LMS, lab).map(|c| c * c * c))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use proptest::prelude::*;

    use super::*;

    fn rgba(r: f32, g: f32, b: f32) -> [f32; 4] {
        [r, g, b, 1.0]
    }

    fn hue(c: [f32; 4]) -> f32 {
        let [_, a, b] = linear_to_oklab([c[0], c[1], c[2]]);
        b.atan2(a)
    }

    #[test]
    fn white_on_black_is_the_maximum_ratio() {
        let ratio = contrast_ratio(rgba(1.0, 1.0, 1.0), rgba(0.0, 0.0, 0.0));
        assert!((ratio - MAX_CONTRAST_RATIO).abs() < 1e-4);
    }

    #[test]
    fn a_minimum_of_one_or_less_or_nan_returns_fg_unchanged() {
        let (fg, bg) = (rgba(0.02, 0.02, 0.3), rgba(0.0, 0.0, 0.0));
        for minimum in [1.0, 0.0, -3.0, f32::NAN] {
            assert_eq!(ensure_contrast(fg, bg, minimum), fg, "minimum {minimum}");
        }
    }

    #[test]
    fn a_pair_that_already_meets_the_minimum_is_unchanged() {
        let (fg, bg) = (rgba(0.8, 0.8, 0.8), rgba(0.005, 0.005, 0.006));
        assert!(contrast_ratio(fg, bg) > 7.0);
        assert_eq!(ensure_contrast(fg, bg, 7.0), fg);
    }

    #[test]
    fn dark_blue_on_black_is_lightened_and_keeps_its_hue() {
        let (fg, bg) = (rgba(0.0, 0.0, 0.5), rgba(0.0, 0.0, 0.0));
        let out = ensure_contrast(fg, bg, 4.5);
        assert!(contrast_ratio(out, bg) >= 4.5);
        assert!(relative_luminance(out) > relative_luminance(fg));
        assert!((hue(out) - hue(fg)).abs() < 0.05, "{out:?}");
    }

    #[test]
    fn light_gray_on_white_is_darkened() {
        let (fg, bg) = (rgba(0.6, 0.6, 0.6), rgba(1.0, 1.0, 1.0));
        let out = ensure_contrast(fg, bg, 4.5);
        assert!(contrast_ratio(out, bg) >= 4.5);
        assert!(relative_luminance(out) < relative_luminance(fg));
    }

    #[test]
    fn the_other_side_is_taken_when_the_near_side_cannot_reach_the_minimum() {
        let bg = rgba(0.5, 0.5, 0.5);
        let fg = rgba(0.55, 0.55, 0.55);
        let out = ensure_contrast(fg, bg, 4.5);
        assert!(contrast_ratio(out, bg) >= 4.5);
        assert!(relative_luminance(out) < relative_luminance(bg));
    }

    #[test]
    fn an_unreachable_minimum_takes_the_better_of_white_and_black() {
        let bg = rgba(0.1, 0.1, 0.1);
        let out = ensure_contrast(rgba(0.12, 0.12, 0.12), bg, MAX_CONTRAST_RATIO);
        assert_eq!(out, rgba(1.0, 1.0, 1.0));
    }

    #[test]
    fn alpha_is_kept() {
        let out = ensure_contrast([0.0, 0.0, 0.5, 0.25], rgba(0.0, 0.0, 0.0), 4.5);
        assert_eq!(out[3], 0.25);
    }

    proptest! {
        #[test]
        fn the_result_meets_the_minimum_whenever_white_or_black_does(
            fg in proptest::array::uniform3(0.0f32..=1.0),
            bg in proptest::array::uniform3(0.0f32..=1.0),
            minimum in 1.0f32..=MAX_CONTRAST_RATIO,
        ) {
            let (fg, bg) = (rgba(fg[0], fg[1], fg[2]), rgba(bg[0], bg[1], bg[2]));
            let out = ensure_contrast(fg, bg, minimum);
            prop_assert!(out.iter().all(|c| (0.0..=1.0).contains(c)), "{:?}", out);
            let reachable = contrast_ratio(rgba(1.0, 1.0, 1.0), bg) >= minimum
                || contrast_ratio(rgba(0.0, 0.0, 0.0), bg) >= minimum;
            if reachable {
                prop_assert!(contrast_ratio(out, bg) >= minimum, "{:?} on {:?}", out, bg);
            }
            prop_assert!(contrast_ratio(out, bg) >= contrast_ratio(fg, bg).min(minimum));
        }
    }
}
