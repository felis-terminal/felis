//! Scales a straight RGBA8 strike image by a Mitchell–Netravali cubic
//! (B = C = 1/3), one axis at a time: across first, into an 8-bit scratch
//! image, then down. Colors are weighted by alpha, which is resampling the
//! premultiplied image and dividing alpha back out.

/// The most source samples one output sample reads; a larger footprint is
/// trimmed evenly from both ends.
const MAX_TAPS: usize = 64;

/// The kernel at distance `x`, with the negative lobe folded positive.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add is a libm call without target FMA, and fusing would move the rounding"
)]
fn kernel(x: f32) -> f32 {
    let x = x.abs();
    if x < 1.0 {
        ((16.0 + x * x * (21.0 * x - 36.0)) / 18.0).abs()
    } else if x < 2.0 {
        ((32.0 + x * (-60.0 + x * (36.0 - 7.0 * x))) / 18.0).abs()
    } else {
        0.0
    }
}

/// Bytes of the scratch image a `width × height` to `to_width × to_height`
/// resample writes between its passes.
pub(super) fn scratch_len(height: usize, to_width: usize) -> Option<usize> {
    to_width.checked_mul(height)?.checked_mul(4)
}

/// `src` is `width × height`; `scratch` and `out` are resized to fit.
pub(super) fn resample(
    src: &[u8],
    (width, height): (usize, usize),
    (to_width, to_height): (usize, usize),
    scratch: &mut Vec<u8>,
    out: &mut Vec<u8>,
) {
    scratch.clear();
    scratch.resize(to_width * height * 4, 0);
    out.clear();
    out.resize(to_width * to_height * 4, 0);
    let mut weights = Vec::new();
    let row = |w: usize| move |x: usize, y: usize| (y * w + x) * 4;
    let column = |w: usize| move |y: usize, x: usize| (y * w + x) * 4;
    pass(
        src,
        (width, height, row(width)),
        (scratch, to_width, row(to_width)),
        &mut weights,
    );
    pass(
        scratch,
        (height, to_width, column(to_width)),
        (out, to_height, column(to_width)),
        &mut weights,
    );
}

/// Resamples `len` samples along each of `lines` lines to `to_len`;
/// `at(sample, line)` is a pixel's byte offset.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "sample positions are clamped into the image before use"
)]
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add is a libm call without target FMA, and fusing would move the rounding"
)]
fn pass(
    src: &[u8],
    (len, lines, at_src): (usize, usize, impl Fn(usize, usize) -> usize),
    (dst, to_len, at_dst): (&mut [u8], usize, impl Fn(usize, usize) -> usize),
    weights: &mut Vec<f32>,
) {
    const UNIT: f32 = 1.0 / 255.0;
    let ratio = len as f32 / to_len as f32;
    let stretch = ratio.max(1.0);
    let (radius, inverse) = (2.0 * stretch, 1.0 / stretch);
    for sample in 0..to_len {
        let center = (sample as f32 + 0.5) * ratio;
        let last = len as i64 - 1;
        let mut lo = ((center - radius).floor() as i64).clamp(0, last) as usize;
        let mut hi = ((center + radius).ceil() as i64).clamp(lo as i64 + 1, len as i64) as usize;
        while hi - lo > MAX_TAPS {
            (lo, hi) = (lo + 1, hi - 1);
        }
        weights.clear();
        weights.extend((lo..hi).map(|i| kernel((i as f32 + 0.5 - center) * inverse)));
        let total_inverse = 1.0 / weights.iter().sum::<f32>();
        for line in 0..lines {
            let (mut color, mut alpha) = ([0.0_f32; 3], 0.0_f32);
            for (i, &weight) in (lo..).zip(weights.iter()) {
                let pixel = &src[at_src(i, line)..][..4];
                let a = f32::from(pixel[3]) * UNIT;
                for (sum, &c) in color.iter_mut().zip(pixel) {
                    *sum += f32::from(c) * UNIT * weight * a;
                }
                alpha += a * weight;
            }
            if alpha != 0.0 {
                let inverse = 1.0 / alpha;
                for c in &mut color {
                    *c *= inverse;
                }
            }
            alpha *= total_inverse;
            let out = &mut dst[at_dst(sample, line)..][..4];
            for (byte, value) in out.iter_mut().zip(color.into_iter().chain([alpha])) {
                *byte = (value * 255.0) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::resample;

    proptest! {
        #[test]
        fn a_constant_image_resamples_to_at_most_one_below_it_per_pass(
            pixel in any::<[u8; 4]>(),
            (width, height) in (1_usize..40, 1_usize..40),
            (to_width, to_height) in (1_usize..40, 1_usize..40),
        ) {
            let src: Vec<u8> = pixel.repeat(width * height);
            let (mut scratch, mut out) = (Vec::new(), Vec::new());
            resample(&src, (width, height), (to_width, to_height), &mut scratch, &mut out);
            prop_assert_eq!(out.len(), to_width * to_height * 4);
            for got in out.as_chunks::<4>().0 {
                prop_assert!(got[3] <= pixel[3] && pixel[3] - got[3] <= 2, "alpha {got:?} from {pixel:?}");
                if got[3] != 0 {
                    for (g, p) in got[..3].iter().zip(&pixel[..3]) {
                        prop_assert!(g <= p && p - g <= 2, "{got:?} from {pixel:?}");
                    }
                }
            }
        }
    }
}
