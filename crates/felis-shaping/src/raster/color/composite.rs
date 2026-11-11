//! Per-pixel arithmetic of the version 1 painter: the composite modes it
//! draws, on premultiplied RGBA8 as W3C Compositing-1 defines them, and
//! the gradient parameter of each brush shape and extend mode.

use skrifa::font::glyph::color::{CompositeMode, Extend};

use super::div255;

/// A composite of a premultiplied source onto a premultiplied backdrop.
pub(super) type Op = fn([u8; 4], [u8; 4]) -> [u8; 4];

/// The composite `mode` names; a value the format does not define clears,
/// as the `COLR` specification requires.
pub(super) fn op(mode: CompositeMode) -> Op {
    use CompositeMode as M;
    match mode {
        M::Clear | M::Unknown => |_, _| [0; 4],
        M::Src => |s, _| s,
        M::Dest => |_, d| d,
        M::SrcOver => src_over,
        M::DestOver => |s, d| src_over(d, s),
        M::SrcIn => |s, d| porter_duff(s, d, d[3], 0),
        M::DestIn => |s, d| porter_duff(s, d, 0, s[3]),
        M::SrcOut => |s, d| porter_duff(s, d, 255 - d[3], 0),
        M::DestOut => |s, d| porter_duff(s, d, 0, 255 - s[3]),
        M::SrcAtop => |s, d| porter_duff(s, d, d[3], 255 - s[3]),
        M::DestAtop => |s, d| porter_duff(s, d, 255 - d[3], s[3]),
        M::Xor => |s, d| porter_duff(s, d, 255 - d[3], 255 - s[3]),
        M::Plus => |s, d| std::array::from_fn(|i| s[i].saturating_add(d[i])),
        M::Screen => |s, d| separable(s, d, |b, s| s.mul_add(-b, b + s)),
        M::Overlay => |s, d| separable(s, d, |b, s| hard_light(s, b)),
        M::Darken => |s, d| separable(s, d, f32::min),
        M::Lighten => |s, d| separable(s, d, f32::max),
        M::ColorDodge => |s, d| separable(s, d, color_dodge),
        M::ColorBurn => |s, d| separable(s, d, color_burn),
        M::HardLight => |s, d| separable(s, d, hard_light),
        M::SoftLight => |s, d| separable(s, d, soft_light),
        M::Difference => |s, d| separable(s, d, |b, s| (b - s).abs()),
        M::Exclusion => |s, d| separable(s, d, |b, s| (-2.0 * b).mul_add(s, b + s)),
        M::Multiply => |s, d| separable(s, d, |b, s| b * s),
        M::HslHue => |s, d| blend(s, d, |b, s| set_lum(set_sat(s, sat(b)), lum(b))),
        M::HslSaturation => |s, d| blend(s, d, |b, s| set_lum(set_sat(b, sat(s)), lum(b))),
        M::HslColor => |s, d| blend(s, d, |b, s| set_lum(s, lum(b))),
        M::HslLuminosity => |s, d| blend(s, d, |b, s| set_lum(b, lum(s))),
    }
}

/// Whether `mode` changes a backdrop pixel where the source is
/// transparent.
pub(super) const fn clears_backdrop(mode: CompositeMode) -> bool {
    matches!(
        mode,
        CompositeMode::Clear
            | CompositeMode::Unknown
            | CompositeMode::Src
            | CompositeMode::SrcIn
            | CompositeMode::DestIn
            | CompositeMode::SrcOut
            | CompositeMode::DestAtop
    )
}

/// Source-over: `s + d · (1 − αs)`.
pub(super) fn src_over(s: [u8; 4], d: [u8; 4]) -> [u8; 4] {
    let keep = 255 - u32::from(s[3]);
    std::array::from_fn(|i| narrow(u32::from(s[i]) + div255(u32::from(d[i]) * keep)))
}

/// The Porter–Duff composite `s · Fa + d · Fb`, the factors in 0..=255.
fn porter_duff(s: [u8; 4], d: [u8; 4], fa: u8, fb: u8) -> [u8; 4] {
    let (fa, fb) = (u32::from(fa), u32::from(fb));
    std::array::from_fn(|i| narrow(div255(u32::from(s[i]) * fa + u32::from(d[i]) * fb)))
}

/// A blend composited source-over:
/// `cs · (1 − αb) + cb · (1 − αs) + αs · αb · B(Cb, Cs)`, with `B` taking
/// the un-premultiplied backdrop and source colors.
fn blend(s: [u8; 4], d: [u8; 4], b: impl Fn([f32; 3], [f32; 3]) -> [f32; 3]) -> [u8; 4] {
    let (sa, da) = (f32::from(s[3]) / 255.0, f32::from(d[3]) / 255.0);
    let cs: [f32; 3] = std::array::from_fn(|i| f32::from(s[i]) / 255.0);
    let cb: [f32; 3] = std::array::from_fn(|i| f32::from(d[i]) / 255.0);
    let mixed = if sa > 0.0 && da > 0.0 {
        b(cb.map(|c| (c / da).min(1.0)), cs.map(|c| (c / sa).min(1.0)))
    } else {
        [0.0; 3]
    };
    let alpha =
        narrow(u32::from(s[3]) + u32::from(d[3]) - div255(u32::from(s[3]) * u32::from(d[3])));
    let mut out = [0, 0, 0, alpha];
    for i in 0..3 {
        let c = cs[i].mul_add(1.0 - da, cb[i].mul_add(1.0 - sa, sa * da * mixed[i]));
        out[i] = unit(c).min(alpha);
    }
    out
}

/// A blend whose `B` acts on each channel alone.
fn separable(s: [u8; 4], d: [u8; 4], b: impl Fn(f32, f32) -> f32) -> [u8; 4] {
    blend(s, d, |cb, cs| std::array::from_fn(|i| b(cb[i], cs[i])))
}

fn hard_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        cb * 2.0 * cs
    } else {
        let s = 2.0f32.mul_add(cs, -1.0);
        s.mul_add(-cb, cb + s)
    }
}

fn color_dodge(cb: f32, cs: f32) -> f32 {
    if cb == 0.0 {
        0.0
    } else if cs >= 1.0 {
        1.0
    } else {
        (cb / (1.0 - cs)).min(1.0)
    }
}

fn color_burn(cb: f32, cs: f32) -> f32 {
    if cb >= 1.0 {
        1.0
    } else if cs == 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - cb) / cs).min(1.0)
    }
}

fn soft_light(cb: f32, cs: f32) -> f32 {
    if cs <= 0.5 {
        (2.0f32.mul_add(cs, -1.0) * cb).mul_add(1.0 - cb, cb)
    } else {
        let d = if cb <= 0.25 {
            16.0f32.mul_add(cb, -12.0).mul_add(cb, 4.0) * cb
        } else {
            cb.sqrt()
        };
        2.0f32.mul_add(cs, -1.0).mul_add(d - cb, cb)
    }
}

fn lum(c: [f32; 3]) -> f32 {
    0.3f32.mul_add(c[0], 0.59f32.mul_add(c[1], 0.11 * c[2]))
}

fn set_lum(color: [f32; 3], target: f32) -> [f32; 3] {
    let shift = target - lum(color);
    let color = color.map(|v| v + shift);
    let l = lum(color);
    let low = color[0].min(color[1]).min(color[2]);
    let high = color[0].max(color[1]).max(color[2]);
    let color = if low < 0.0 && l > low {
        color.map(|v| (v - l).mul_add(l / (l - low), l))
    } else {
        color
    };
    if high > 1.0 && high > l {
        color.map(|v| (v - l).mul_add((1.0 - l) / (high - l), l))
    } else {
        color
    }
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let mut order = [0, 1, 2];
    order.sort_by(|&a, &b| c[a].total_cmp(&c[b]));
    let [lo, mid, hi] = order;
    let mut out = [0.0; 3];
    if c[hi] > c[lo] {
        out[mid] = (c[mid] - c[lo]) * s / (c[hi] - c[lo]);
        out[hi] = s;
    }
    out
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "every caller's value is within 0..=255"
)]
const fn narrow(v: u32) -> u8 {
    if v > 255 { 255 } else { v as u8 }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped into 0..=255"
)]
fn unit(v: f32) -> u8 {
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

/// The gradient parameter of a linear brush from `p0` along `d`,
/// `inv_len2` being `1 / |d|²`.
pub(super) fn linear_t(p: [f32; 2], p0: [f32; 2], d: [f32; 2], inv_len2: f32) -> f32 {
    (p[0] - p0[0]).mul_add(d[0], (p[1] - p0[1]) * d[1]) * inv_len2
}

/// A two-point conical brush: circles from `c0`, `r0` to `c0 + cd`,
/// `r0 + dr`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Conical {
    pub(super) c0: [f32; 2],
    pub(super) r0: f32,
    pub(super) cd: [f32; 2],
    pub(super) dr: f32,
}

impl Conical {
    /// The largest `t` whose circle passes through `p` with a radius of
    /// zero or more, as the spec and Skia define the two-point conical
    /// gradient; `None` where no such circle exists, which paints nothing.
    #[expect(
        clippy::many_single_char_names,
        reason = "the quadratic's coefficients keep the names the spec gives them"
    )]
    pub(super) fn t(&self, p: [f32; 2]) -> Option<f32> {
        let pd = [p[0] - self.c0[0], p[1] - self.c0[1]];
        let a = self.dr.mul_add(
            -self.dr,
            self.cd[0].mul_add(self.cd[0], self.cd[1] * self.cd[1]),
        );
        let b = self
            .r0
            .mul_add(self.dr, pd[0].mul_add(self.cd[0], pd[1] * self.cd[1]));
        let c = self
            .r0
            .mul_add(-self.r0, pd[0].mul_add(pd[0], pd[1] * pd[1]));
        let radius = |t: f32| self.dr.mul_add(t, self.r0);
        let scale = self.cd[0].abs() + self.cd[1].abs() + self.dr.abs();
        if a.abs() <= f32::EPSILON * scale * scale {
            if b == 0.0 {
                return None;
            }
            let t = c / (2.0 * b);
            return (radius(t) >= 0.0).then_some(t);
        }
        let disc = b.mul_add(b, -a * c);
        if disc < 0.0 {
            return None;
        }
        let root = disc.sqrt();
        let (t1, t2) = ((b + root) / a, (b - root) / a);
        let (hi, lo) = if t1 >= t2 { (t1, t2) } else { (t2, t1) };
        if radius(hi) >= 0.0 {
            Some(hi)
        } else if radius(lo) >= 0.0 {
            Some(lo)
        } else {
            None
        }
    }
}

/// `t` brought into 0..=1 by `extend`: held at the ends, wrapped, or
/// mirrored every other period.
pub(super) fn extended(t: f32, extend: Extend) -> f32 {
    match extend {
        Extend::Repeat => t - t.floor(),
        Extend::Reflect => {
            let u = t.rem_euclid(2.0);
            if u > 1.0 { 2.0 - u } else { u }
        }
        Extend::Pad | Extend::Unknown => t.clamp(0.0, 1.0),
    }
}

/// A color line sampled at 256 points of `t` in 0..=1, premultiplied, and
/// the extend that brings any other `t` into that range.
pub(super) struct Ramp {
    colors: [[u8; 4]; 256],
    below: [u8; 4],
    extend: Extend,
}

impl Ramp {
    /// `stops` sorted by offset, each a straight color: interpolated
    /// straight, as cairo and Skia interpolate a gradient, then
    /// premultiplied.
    pub(super) fn new(stops: &[(f32, [f32; 4])], extend: Extend) -> Self {
        let premultiplied =
            |[r, g, b, a]: [f32; 4]| [unit(r * a), unit(g * a), unit(b * a), unit(a)];
        let mut colors = [[0; 4]; 256];
        for (i, out) in (0_u16..).zip(&mut colors) {
            *out = premultiplied(Self::at(stops, f32::from(i) / 255.0));
        }
        let below = premultiplied(stops.first().map_or([0.0; 4], |&(_, color)| color));
        Self {
            colors,
            below,
            extend,
        }
    }

    fn at(stops: &[(f32, [f32; 4])], t: f32) -> [f32; 4] {
        let Some(&(_, first)) = stops.first() else {
            return [0.0; 4];
        };
        let above = stops.partition_point(|&(offset, _)| offset <= t);
        let (Some(&(o0, c0)), Some(&(o1, c1))) = (
            above.checked_sub(1).and_then(|i| stops.get(i)),
            stops.get(above),
        ) else {
            return if above == 0 {
                first
            } else {
                stops[stops.len() - 1].1
            };
        };
        let f = (t - o0) / (o1 - o0);
        std::array::from_fn(|i| (c1[i] - c0[i]).mul_add(f, c0[i]))
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into 0..=255"
    )]
    pub(super) fn sample(&self, t: f32) -> [u8; 4] {
        if t < 0.0 && matches!(self.extend, Extend::Pad | Extend::Unknown) {
            return self.below;
        }
        let t = extended(t, self.extend);
        let index = (t.clamp(0.0, 1.0) * 255.0).round() as usize;
        self.colors[index.min(255)]
    }
}

#[cfg(test)]
#[expect(
    clippy::suboptimal_flops,
    clippy::float_cmp,
    reason = "the oracles read as the W3C and spec formulas they check against"
)]
mod tests {
    use proptest::prelude::*;
    use skrifa::font::glyph::color::{CompositeMode, Extend};

    use super::{Conical, Ramp, clears_backdrop, extended, linear_t, op};

    /// Every mode the format defines.
    const MODES: [CompositeMode; 28] = [
        CompositeMode::Clear,
        CompositeMode::Src,
        CompositeMode::Dest,
        CompositeMode::SrcOver,
        CompositeMode::DestOver,
        CompositeMode::SrcIn,
        CompositeMode::DestIn,
        CompositeMode::SrcOut,
        CompositeMode::DestOut,
        CompositeMode::SrcAtop,
        CompositeMode::DestAtop,
        CompositeMode::Xor,
        CompositeMode::Plus,
        CompositeMode::Screen,
        CompositeMode::Overlay,
        CompositeMode::Darken,
        CompositeMode::Lighten,
        CompositeMode::ColorDodge,
        CompositeMode::ColorBurn,
        CompositeMode::HardLight,
        CompositeMode::SoftLight,
        CompositeMode::Difference,
        CompositeMode::Exclusion,
        CompositeMode::Multiply,
        CompositeMode::HslHue,
        CompositeMode::HslSaturation,
        CompositeMode::HslColor,
        CompositeMode::HslLuminosity,
    ];

    fn premultiplied() -> impl Strategy<Value = [u8; 4]> {
        any::<[u8; 4]>().prop_map(|[r, g, b, a]| [r.min(a), g.min(a), b.min(a), a])
    }

    fn unit(p: [u8; 4]) -> [f64; 4] {
        p.map(|c| f64::from(c) / 255.0)
    }

    fn apply(mode: CompositeMode, s: [u8; 4], d: [u8; 4]) -> [u8; 4] {
        op(mode)(s, d)
    }

    /// The W3C Compositing-1 result of `mode` for premultiplied `s` over
    /// premultiplied `d`.
    fn closed_form(mode: CompositeMode, s: [f64; 4], d: [f64; 4]) -> [f64; 4] {
        let (sa, da) = (s[3], d[3]);
        let pd = |fa: f64, fb: f64| -> [f64; 4] { std::array::from_fn(|i| s[i] * fa + d[i] * fb) };
        let separable =
            |b: fn(f64, f64) -> f64| blend(s, d, |cb, cs| std::array::from_fn(|i| b(cb[i], cs[i])));
        match mode {
            CompositeMode::Clear => [0.0; 4],
            CompositeMode::Src => pd(1.0, 0.0),
            CompositeMode::Dest => pd(0.0, 1.0),
            CompositeMode::SrcOver => pd(1.0, 1.0 - sa),
            CompositeMode::DestOver => pd(1.0 - da, 1.0),
            CompositeMode::SrcIn => pd(da, 0.0),
            CompositeMode::DestIn => pd(0.0, sa),
            CompositeMode::SrcOut => pd(1.0 - da, 0.0),
            CompositeMode::DestOut => pd(0.0, 1.0 - sa),
            CompositeMode::SrcAtop => pd(da, 1.0 - sa),
            CompositeMode::DestAtop => pd(1.0 - da, sa),
            CompositeMode::Xor => pd(1.0 - da, 1.0 - sa),
            CompositeMode::Plus => pd(1.0, 1.0).map(|c| c.min(1.0)),
            CompositeMode::Multiply => separable(|cb, cs| cb * cs),
            CompositeMode::Screen => separable(screen),
            CompositeMode::Overlay => separable(|cb, cs| hard_light(cs, cb)),
            CompositeMode::Darken => separable(f64::min),
            CompositeMode::Lighten => separable(f64::max),
            CompositeMode::ColorDodge => separable(|cb, cs| {
                if cb == 0.0 {
                    0.0
                } else if cs == 1.0 {
                    1.0
                } else {
                    (cb / (1.0 - cs)).min(1.0)
                }
            }),
            CompositeMode::ColorBurn => separable(|cb, cs| {
                if cb == 1.0 {
                    1.0
                } else if cs == 0.0 {
                    0.0
                } else {
                    1.0 - ((1.0 - cb) / cs).min(1.0)
                }
            }),
            CompositeMode::HardLight => separable(hard_light),
            CompositeMode::SoftLight => separable(soft_light),
            CompositeMode::Difference => separable(|cb, cs| (cb - cs).abs()),
            CompositeMode::Exclusion => separable(|cb, cs| cb + cs - 2.0 * cb * cs),
            CompositeMode::HslHue => blend(s, d, |cb, cs| set_lum(set_sat(cs, sat(cb)), lum(cb))),
            CompositeMode::HslSaturation => {
                blend(s, d, |cb, cs| set_lum(set_sat(cb, sat(cs)), lum(cb)))
            }
            CompositeMode::HslColor => blend(s, d, |cb, cs| set_lum(cs, lum(cb))),
            CompositeMode::HslLuminosity => blend(s, d, |cb, cs| set_lum(cb, lum(cs))),
            CompositeMode::Unknown => unreachable!("not a mode the format defines"),
        }
    }

    /// `cs · (1 − αb) + cb · (1 − αs) + αs · αb · B(Cb, Cs)`, `B` on
    /// un-premultiplied colors.
    fn blend(s: [f64; 4], d: [f64; 4], b: impl Fn([f64; 3], [f64; 3]) -> [f64; 3]) -> [f64; 4] {
        let (sa, da) = (s[3], d[3]);
        let mixed = if sa > 0.0 && da > 0.0 {
            b(
                std::array::from_fn(|i| (d[i] / da).min(1.0)),
                std::array::from_fn(|i| (s[i] / sa).min(1.0)),
            )
        } else {
            [0.0; 3]
        };
        let mut out = [0.0, 0.0, 0.0, sa + da - sa * da];
        for i in 0..3 {
            out[i] = s[i] * (1.0 - da) + d[i] * (1.0 - sa) + sa * da * mixed[i];
        }
        out
    }

    fn screen(cb: f64, cs: f64) -> f64 {
        cb + cs - cb * cs
    }

    fn hard_light(cb: f64, cs: f64) -> f64 {
        if cs <= 0.5 {
            cb * 2.0 * cs
        } else {
            screen(cb, 2.0 * cs - 1.0)
        }
    }

    fn soft_light(cb: f64, cs: f64) -> f64 {
        if cs <= 0.5 {
            cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
        } else {
            let d = if cb <= 0.25 {
                ((16.0 * cb - 12.0) * cb + 4.0) * cb
            } else {
                cb.sqrt()
            };
            cb + (2.0 * cs - 1.0) * (d - cb)
        }
    }

    fn lum(c: [f64; 3]) -> f64 {
        0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
    }

    fn clip_color(c: [f64; 3]) -> [f64; 3] {
        let l = lum(c);
        let n = c[0].min(c[1]).min(c[2]);
        let x = c[0].max(c[1]).max(c[2]);
        let mut c = c;
        if n < 0.0 {
            c = c.map(|v| l + (v - l) * l / (l - n));
        }
        if x > 1.0 {
            c = c.map(|v| l + (v - l) * (1.0 - l) / (x - l));
        }
        c
    }

    fn set_lum(c: [f64; 3], l: f64) -> [f64; 3] {
        let d = l - lum(c);
        clip_color(c.map(|v| v + d))
    }

    fn sat(c: [f64; 3]) -> f64 {
        c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
    }

    fn set_sat(c: [f64; 3], s: f64) -> [f64; 3] {
        let mut order = [0, 1, 2];
        order.sort_by(|&a, &b| c[a].total_cmp(&c[b]));
        let [min, mid, max] = order;
        let mut out = [0.0; 3];
        if c[max] > c[min] {
            out[mid] = (c[mid] - c[min]) * s / (c[max] - c[min]);
            out[max] = s;
        }
        out
    }

    fn assert_near(
        mode: CompositeMode,
        got: [u8; 4],
        want: [f64; 4],
        slack: f64,
    ) -> Result<(), TestCaseError> {
        for (g, w) in got.iter().zip(want) {
            prop_assert!(
                (f64::from(*g) - w * 255.0).abs() <= slack,
                "{mode:?}: {got:?} against {:?}",
                want.map(|w| w * 255.0)
            );
        }
        Ok(())
    }

    proptest! {
        #[test]
        fn every_mode_is_its_closed_form(s in premultiplied(), d in premultiplied()) {
            for mode in MODES {
                assert_near(mode, apply(mode, s, d), closed_form(mode, unit(s), unit(d)), 1.0)?;
            }
        }

        #[test]
        fn every_mode_keeps_its_result_premultiplied(s in premultiplied(), d in premultiplied()) {
            for mode in MODES {
                let out = apply(mode, s, d);
                prop_assert!(out[..3].iter().all(|&c| c <= out[3]), "{mode:?}: {out:?}");
            }
        }

        #[test]
        fn a_transparent_source_leaves_the_backdrop_unless_the_mode_clears_it(d in premultiplied()) {
            prop_assume!(d[3] > 0);
            for mode in MODES {
                let kept = apply(mode, [0; 4], d) == d;
                prop_assert_eq!(kept, !clears_backdrop(mode), "{:?}", mode);
            }
        }

        #[test]
        fn a_linear_gradient_runs_from_zero_at_p0_to_one_at_p1(
            p0 in prop::array::uniform2(-100.0_f32..100.0),
            d in prop::array::uniform2(-100.0_f32..100.0),
            f in 0.0_f32..1.0,
            across in -50.0_f32..50.0,
        ) {
            let len2 = d[0].mul_add(d[0], d[1] * d[1]);
            prop_assume!(len2 > 1.0);
            let normal = [-d[1], d[0]];
            let scale = across / len2.sqrt();
            let p = [
                normal[0].mul_add(scale, d[0].mul_add(f, p0[0])),
                normal[1].mul_add(scale, d[1].mul_add(f, p0[1])),
            ];
            prop_assert!((linear_t(p, p0, d, 1.0 / len2) - f).abs() < 1e-3);
        }

        #[test]
        fn a_conical_t_lies_on_its_circle_with_a_radius_of_zero_or_more(
            c0 in prop::array::uniform2(-20.0_f32..20.0),
            cd in prop::array::uniform2(-20.0_f32..20.0),
            r0 in 0.0_f32..20.0,
            dr in -20.0_f32..20.0,
            p in prop::array::uniform2(-40.0_f32..40.0),
        ) {
            let g = Conical { c0, r0, cd, dr };
            if let Some(t) = g.t(p) {
                let radius = dr.mul_add(t, r0);
                prop_assert!(radius >= 0.0);
                let center = [cd[0].mul_add(t, c0[0]), cd[1].mul_add(t, c0[1])];
                let distance = (p[0] - center[0]).hypot(p[1] - center[1]);
                prop_assert!((distance - radius).abs() <= 1e-2 * (1.0 + radius), "{distance} vs {radius} at t {t}");
            }
        }


        #[test]
        fn repeat_wraps_t_into_one_period(t in -10.0_f32..10.0, k in -5_i8..5) {
            let u = extended(t, Extend::Repeat);
            prop_assert!((0.0..=1.0).contains(&u));
            let shifted = extended(t + f32::from(k), Extend::Repeat);
            prop_assert!((u - shifted).abs() < 1e-4 || (u - shifted).abs() > 1.0 - 1e-4, "{u} vs {shifted}");
        }

        #[test]
        fn reflect_mirrors_t_every_other_period(t in -10.0_f32..10.0, k in -5_i8..5) {
            let u = extended(t, Extend::Reflect);
            prop_assert!((0.0..=1.0).contains(&u));
            prop_assert!((u - extended(-t, Extend::Reflect)).abs() < 1e-4);
            prop_assert!((u - extended(t + 2.0 * f32::from(k), Extend::Reflect)).abs() < 1e-4);
        }
    }

    #[test]
    fn concentric_circles_give_t_by_distance() {
        let g = Conical {
            c0: [0.0, 0.0],
            r0: 10.0,
            cd: [0.0, 0.0],
            dr: 20.0,
        };
        assert_eq!(g.t([10.0, 0.0]), Some(0.0));
        assert_eq!(g.t([0.0, 30.0]), Some(1.0));
        assert_eq!(g.t([20.0, 0.0]), Some(0.5));
        let g = Conical { r0: 0.0, ..g };
        assert_eq!(
            g.t([0.0, 0.0]),
            Some(0.0),
            "a zero start radius is the center"
        );
    }

    #[test]
    fn equal_radii_sweep_a_cylinder() {
        let g = Conical {
            c0: [0.0, 0.0],
            r0: 5.0,
            cd: [10.0, 0.0],
            dr: 0.0,
        };
        let t = g.t([15.0, 0.0]).expect("on the end circle");
        assert!(
            (t - 2.0).abs() < 1e-6,
            "the circle past the end passes there too: {t}"
        );
        assert_eq!(g.t([0.0, 6.0]), None, "outside the cylinder");
    }

    #[test]
    fn a_focal_point_on_the_end_circle_paints_one_side() {
        let g = Conical {
            c0: [0.0, 0.0],
            r0: 0.0,
            cd: [10.0, 0.0],
            dr: 10.0,
        };
        let t = g.t([20.0, 0.0]).expect("on the end circle");
        assert!((t - 1.0).abs() < 1e-6, "{t}");
        let t = g.t([5.0, 5.0]).expect("inside the cone");
        let radius = 10.0 * t;
        assert!(((5.0 - radius).hypot(5.0) - radius).abs() < 1e-4);
        assert_eq!(g.t([-5.0, 0.0]), None, "behind the focal point");
    }

    #[test]
    fn equal_circles_paint_nothing() {
        let g = Conical {
            c0: [0.0, 0.0],
            r0: 5.0,
            cd: [0.0, 0.0],
            dr: 0.0,
        };
        assert_eq!(g.t([5.0, 0.0]), None);
    }

    #[test]
    fn a_pad_ramp_holds_its_end_colors_outside_the_line() {
        let ramp = Ramp::new(
            &[(0.25, [1.0, 0.0, 0.0, 1.0]), (0.75, [0.0, 0.0, 1.0, 0.5])],
            Extend::Pad,
        );
        assert_eq!(ramp.sample(-3.0), [255, 0, 0, 255]);
        assert_eq!(ramp.sample(0.1), [255, 0, 0, 255]);
        assert_eq!(ramp.sample(0.9), [0, 0, 128, 128]);
        assert_eq!(ramp.sample(7.0), [0, 0, 128, 128]);
        assert_eq!(
            ramp.sample(0.5),
            [95, 0, 96, 191],
            "straight, then premultiplied"
        );
    }

    /// The `COLR` color line: of stops sharing an offset, the first
    /// colors the line below it and the last at and above it.
    #[test]
    fn coincident_stops_use_the_first_below_and_the_last_from_their_offset() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let green = [0.0, 1.0, 0.0, 1.0];
        let blue = [0.0, 0.0, 1.0, 1.0];
        let at = 51.0 / 255.0;
        let ramp = Ramp::new(
            &[(0.0, red), (at, red), (at, green), (at, blue), (1.0, blue)],
            Extend::Pad,
        );
        assert_eq!(ramp.sample(50.0 / 255.0), [255, 0, 0, 255], "inside, below");
        assert_eq!(ramp.sample(at), [0, 0, 255, 255], "inside, at the offset");
        let ramp = Ramp::new(&[(0.0, red), (0.0, green), (1.0, green)], Extend::Pad);
        assert_eq!(
            ramp.sample(-1.0),
            [255, 0, 0, 255],
            "padded below the first offset"
        );
        assert_eq!(ramp.sample(0.0), [0, 255, 0, 255], "at the first offset");
        let ramp = Ramp::new(&[(0.0, red), (1.0, green), (1.0, blue)], Extend::Pad);
        assert_eq!(ramp.sample(1.0), [0, 0, 255, 255], "at the last offset");
        assert_eq!(ramp.sample(2.0), [0, 0, 255, 255], "padded past it");
    }

    /// The branch points random pairs rarely reach: transparent and
    /// opaque layers, black and white, a channel equal to its alpha.
    #[test]
    fn every_mode_is_its_closed_form_at_the_extremes() {
        let levels = [0, 1, 128, 254, 255];
        let mut pixels = Vec::new();
        for a in levels {
            let under = levels.iter().copied().filter(|&c| c <= a);
            for r in under.clone() {
                for g in under.clone() {
                    for b in under.clone() {
                        pixels.push([r, g, b, a]);
                    }
                }
            }
        }
        for &s in &pixels {
            for &d in &pixels {
                for mode in MODES {
                    assert_near(
                        mode,
                        apply(mode, s, d),
                        closed_form(mode, unit(s), unit(d)),
                        1.0,
                    )
                    .unwrap_or_else(|e| panic!("{s:?} over {d:?}: {e}"));
                }
            }
        }
    }

    #[test]
    fn an_undefined_mode_clears() {
        assert_eq!(op(CompositeMode::Unknown)([9, 9, 9, 9], [255; 4]), [0; 4]);
        assert!(clears_backdrop(CompositeMode::Unknown));
    }

    #[test]
    fn each_extend_brings_t_into_the_line() {
        let cases = [
            //  t     pad   repeat reflect
            (0.0, 0.0, 0.0, 0.0),
            (0.25, 0.25, 0.25, 0.25),
            (1.0, 1.0, 0.0, 1.0),
            (1.25, 1.0, 0.25, 0.75),
            (2.25, 1.0, 0.25, 0.25),
            (-0.25, 0.0, 0.75, 0.25),
        ];
        for (t, pad, repeat, reflect) in cases {
            assert_eq!(extended(t, Extend::Pad), pad, "pad {t}");
            assert_eq!(
                extended(t, Extend::Unknown),
                pad,
                "an unknown extend pads, {t}"
            );
            assert_eq!(extended(t, Extend::Repeat), repeat, "repeat {t}");
            assert_eq!(extended(t, Extend::Reflect), reflect, "reflect {t}");
        }
        assert_eq!(extended(f32::NEG_INFINITY, Extend::Pad), 0.0);
        assert_eq!(extended(f32::INFINITY, Extend::Pad), 1.0);
    }

    #[test]
    fn a_ramp_repeats_or_reflects_past_its_end() {
        let ramp = Ramp::new(
            &[(0.0, [1.0, 0.0, 0.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])],
            Extend::Repeat,
        );
        assert_eq!(ramp.sample(1.0 + 1.0 / 255.0), ramp.sample(1.0 / 255.0));
        assert_eq!(ramp.sample(-0.5), ramp.sample(0.5));
        let ramp = Ramp::new(
            &[(0.0, [1.0, 0.0, 0.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])],
            Extend::Reflect,
        );
        assert_eq!(ramp.sample(1.25), ramp.sample(0.75));
        assert_eq!(ramp.sample(-0.25), ramp.sample(0.25));
    }
}
