//! Per-pixel arithmetic of the version 1 painter: the composite modes it
//! draws, on premultiplied RGBA8 as W3C Compositing-1 defines them, and
//! the gradient parameter of each brush shape.

use super::div255;

/// Source-over: `s + d · (1 − αs)`.
pub(super) fn src_over(s: [u8; 4], d: [u8; 4]) -> [u8; 4] {
    let keep = 255 - u32::from(s[3]);
    std::array::from_fn(|i| narrow(u32::from(s[i]) + div255(u32::from(d[i]) * keep)))
}

/// Source-in: `s · αd`.
pub(super) fn src_in(s: [u8; 4], d: [u8; 4]) -> [u8; 4] {
    let a = u32::from(d[3]);
    s.map(|c| narrow(div255(u32::from(c) * a)))
}

/// Soft light, a separable blend composited source-over:
/// `cs · (1 − αb) + cb · (1 − αs) + αs · αb · B(Cb, Cs)`, the blend term on
/// un-premultiplied values.
pub(super) fn soft_light(s: [u8; 4], d: [u8; 4]) -> [u8; 4] {
    let (sa, da) = (f32::from(s[3]) / 255.0, f32::from(d[3]) / 255.0);
    let mut out = [0; 4];
    for i in 0..3 {
        let (cs, cb) = (f32::from(s[i]) / 255.0, f32::from(d[i]) / 255.0);
        let blend = if sa > 0.0 && da > 0.0 {
            sa * da * soft_light_term((cb / da).min(1.0), (cs / sa).min(1.0))
        } else {
            0.0
        };
        out[i] = unit(cs.mul_add(1.0 - da, cb.mul_add(1.0 - sa, blend)));
    }
    out[3] = narrow(u32::from(s[3]) + u32::from(d[3]) - div255(u32::from(s[3]) * u32::from(d[3])));
    out
}

/// W3C `B(Cb, Cs)` for soft-light, on un-premultiplied channels.
fn soft_light_term(cb: f32, cs: f32) -> f32 {
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

/// A color line sampled at 256 points of `t` in 0..=1, premultiplied, for
/// a pad extend: `t` outside the line takes its end color.
pub(super) struct Ramp([[u8; 4]; 256]);

impl Ramp {
    /// `stops` sorted by offset, each a straight color: interpolated
    /// straight, as cairo and Skia interpolate a gradient, then
    /// premultiplied.
    pub(super) fn new(stops: &[(f32, [f32; 4])]) -> Self {
        let mut ramp = [[0; 4]; 256];
        for (i, out) in (0_u16..).zip(&mut ramp) {
            let t = f32::from(i) / 255.0;
            let color = Self::at(stops, t);
            let a = color[3];
            *out = [
                unit(color[0] * a),
                unit(color[1] * a),
                unit(color[2] * a),
                unit(a),
            ];
        }
        Self(ramp)
    }

    fn at(stops: &[(f32, [f32; 4])], t: f32) -> [f32; 4] {
        let Some(&(first_offset, first)) = stops.first() else {
            return [0.0; 4];
        };
        if t <= first_offset {
            return first;
        }
        for pair in stops.windows(2) {
            let ((o0, c0), (o1, c1)) = (pair[0], pair[1]);
            if t <= o1 {
                if o1 <= o0 {
                    return c1;
                }
                let f = (t - o0) / (o1 - o0);
                return std::array::from_fn(|i| (c1[i] - c0[i]).mul_add(f, c0[i]));
            }
        }
        stops.last().map_or(first, |&(_, c)| c)
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into 0..=255"
    )]
    pub(super) fn sample(&self, t: f32) -> [u8; 4] {
        let index = (t.clamp(0.0, 1.0) * 255.0).round() as usize;
        self.0[index.min(255)]
    }
}

#[cfg(test)]
#[expect(
    clippy::suboptimal_flops,
    reason = "the oracles read as the W3C and spec formulas they check against"
)]
mod tests {
    use proptest::prelude::*;

    use super::{Conical, Ramp, linear_t, soft_light, src_in, src_over};

    fn premultiplied() -> impl Strategy<Value = [u8; 4]> {
        any::<[u8; 4]>().prop_map(|[r, g, b, a]| [r.min(a), g.min(a), b.min(a), a])
    }

    fn unit(p: [u8; 4]) -> [f64; 4] {
        p.map(|c| f64::from(c) / 255.0)
    }

    fn assert_near(got: [u8; 4], want: [f64; 4], slack: f64) -> Result<(), TestCaseError> {
        for (g, w) in got.iter().zip(want) {
            prop_assert!(
                (f64::from(*g) - w * 255.0).abs() <= slack,
                "{got:?} against {:?}",
                want.map(|w| w * 255.0)
            );
        }
        Ok(())
    }

    /// `B(Cb, Cs)` as W3C Compositing-1 writes it.
    fn soft_light_term(cb: f64, cs: f64) -> f64 {
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

    proptest! {
        #[test]
        fn source_over_is_the_closed_form(s in premultiplied(), d in premultiplied()) {
            let (fs, fd) = (unit(s), unit(d));
            let want = std::array::from_fn(|i| fs[i] + fd[i] * (1.0 - fs[3]));
            assert_near(src_over(s, d), want, 1.0)?;
        }

        #[test]
        fn source_in_is_the_closed_form(s in premultiplied(), d in premultiplied()) {
            let (fs, fd) = (unit(s), unit(d));
            assert_near(src_in(s, d), fs.map(|c| c * fd[3]), 0.5)?;
        }

        #[test]
        fn soft_light_is_the_closed_form(s in premultiplied(), d in premultiplied()) {
            let (fs, fd) = (unit(s), unit(d));
            let mut want = [0.0; 4];
            for i in 0..3 {
                let blend = if fs[3] > 0.0 && fd[3] > 0.0 {
                    fs[3] * fd[3] * soft_light_term((fd[i] / fd[3]).min(1.0), (fs[i] / fs[3]).min(1.0))
                } else {
                    0.0
                };
                want[i] = fs[i] * (1.0 - fd[3]) + fd[i] * (1.0 - fs[3]) + blend;
            }
            want[3] = fs[3] + fd[3] - fs[3] * fd[3];
            assert_near(soft_light(s, d), want, 1.0)?;
        }

        #[test]
        fn every_mode_keeps_its_result_premultiplied(s in premultiplied(), d in premultiplied()) {
            for out in [src_over(s, d), src_in(s, d), soft_light(s, d)] {
                prop_assert!(out[..3].iter().all(|&c| c <= out[3]), "{out:?}");
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
        let ramp = Ramp::new(&[(0.25, [1.0, 0.0, 0.0, 1.0]), (0.75, [0.0, 0.0, 1.0, 0.5])]);
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

    #[test]
    fn coincident_stops_step_between_their_colors() {
        let ramp = Ramp::new(&[
            (0.0, [1.0, 0.0, 0.0, 1.0]),
            (0.5, [1.0, 0.0, 0.0, 1.0]),
            (0.5, [0.0, 1.0, 0.0, 1.0]),
            (1.0, [0.0, 1.0, 0.0, 1.0]),
        ]);
        assert_eq!(ramp.sample(0.4), [255, 0, 0, 255]);
        assert_eq!(ramp.sample(0.6), [0, 255, 0, 255]);
    }
}
