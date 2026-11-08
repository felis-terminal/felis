//! Outline to A8 coverage: flatten, bound, deposit signed area and cover
//! in 24.8 fixed point, prefix-sum each row, trim to the ink box.

/// One pixel in 24.8 fixed point.
const ONE: i32 = 256;
const MAX_CURVE_DEPTH: u32 = 10;
/// Keeps every per-cell `area` sum inside `i32`: a segment adds at most
/// `ONE * 2 * ONE` (2^17) to one cell.
const SEGMENT_CAP: usize = 16_000;
const SEGMENT_BYTES: usize = 16;
const CELL_BYTES: usize = size_of::<Cell>();
const WORKING_SET: usize = 16 << 20;
/// The tallest box the working set admits at one pixel wide, which also
/// keeps every box-relative coordinate inside 24.8.
const MAX_SIDE: f32 = ((WORKING_SET - SEGMENT_BYTES * SEGMENT_CAP) / (CELL_BYTES + 1)) as f32;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Point {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

impl Point {
    const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add is a libm call without target FMA"
    )]
    fn lerp(self, other: Self, t: f32) -> Self {
        Self::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }
}

/// An outline command in y-up pixels, as skrifa draws it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PathEl {
    MoveTo(Point),
    LineTo(Point),
    QuadTo([Point; 2]),
    CurveTo([Point; 3]),
    Close,
}

impl PathEl {
    const fn points(&self) -> &[Point] {
        match self {
            Self::MoveTo(p) | Self::LineTo(p) => std::slice::from_ref(p),
            Self::QuadTo(p) => p,
            Self::CurveTo(p) => p,
            Self::Close => &[],
        }
    }
}

/// A coverage bitmap whose box is its ink box; `top` is measured up from
/// the baseline.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Mask {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) data: Vec<u8>,
}

/// `None` when the outline is over a limit (non-finite or out-of-range
/// points, too deep a curve, too many segments, too large a box), which
/// the caller treats as no outline. `segments` is scratch, left holding
/// the flattened outline relative to [`origin`].
#[expect(
    clippy::cast_possible_wrap,
    reason = "the budget keeps every side under MAX_SIDE"
)]
pub(crate) fn rasterize(path: &[PathEl], px: f32, segments: &mut Vec<[f32; 4]>) -> Option<Mask> {
    segments.clear();
    let side = side_limit(px)?;
    let origin = origin(path, side)?;
    flatten(path, origin, segments)?;
    let Some((min, max)) = bounds(
        segments
            .iter()
            .flat_map(|s| [Point::new(s[0], s[1]), Point::new(s[2], s[3])]),
    ) else {
        return Some(Mask::default());
    };
    let (left, top) = (min.x.floor(), max.y.ceil());
    let (width, height) = (
        (max.x.ceil() - left) as usize,
        (top - min.y.floor()) as usize,
    );
    if width == 0 || height == 0 {
        return Some(Mask::default());
    }
    if !within_budget(width, height, side, segments.capacity()) {
        return None;
    }
    let mut cells = vec![Cell::default(); width * height];
    let (xmax, ymax) = (width as i32 * ONE, height as i32 * ONE);
    let fixed = |v: f32, max: i32| ((v * ONE as f32).round() as i32).clamp(0, max);
    for s in segments.iter() {
        let from = (fixed(s[0] - left, xmax), fixed(top - s[1], ymax));
        let to = (fixed(s[2] - left, xmax), fixed(top - s[3], ymax));
        deposit(&mut cells, width, from, to);
    }
    let mut data = vec![0; width * height];
    for (row, out) in cells.chunks_exact(width).zip(data.chunks_exact_mut(width)) {
        accumulate_row(row, out);
    }
    drop(cells);
    Some(trim(
        data,
        width,
        height,
        (origin.x + left) as i32,
        (origin.y + top) as i32,
    ))
}

fn side_limit(px: f32) -> Option<f32> {
    (px.is_finite() && px > 0.0).then(|| (8.0 * px).clamp(64.0, MAX_SIDE))
}

/// Every working buffer of one rasterization, charged before the first is
/// allocated: `i32` area and cover per cell, the segment scratch as
/// allocated, the A8 output. Nothing else is allocated while the cells
/// live: [`trim`] works in place.
fn within_budget(width: usize, height: usize, side: f32, segment_capacity: usize) -> bool {
    width as f32 <= side
        && height as f32 <= side
        && width
            .checked_mul(height)
            .and_then(|cells| cells.checked_mul(CELL_BYTES + 1))
            .and_then(|bytes| bytes.checked_add(segment_capacity.checked_mul(SEGMENT_BYTES)?))
            .is_some_and(|bytes| bytes <= WORKING_SET)
}

fn bounds(points: impl IntoIterator<Item = Point>) -> Option<(Point, Point)> {
    points.into_iter().fold(None, |acc, p| {
        let (min, max) = acc.unwrap_or((p, p));
        Some((
            Point::new(min.x.min(p.x), min.y.min(p.y)),
            Point::new(max.x.max(p.x), max.y.max(p.y)),
        ))
    })
}

/// The whole-pixel corner below and left of every control point, which
/// the outline is flattened relative to. Control points, not the curve,
/// are checked: they bound the curve, and flattening must not start on a
/// point it cannot represent.
pub(super) fn origin(path: &[PathEl], side: f32) -> Option<Point> {
    let points = path.iter().flat_map(PathEl::points).copied();
    if !points
        .clone()
        .all(|p| p.x.abs() <= side && p.y.abs() <= side)
    {
        return None;
    }
    let Some((min, max)) = bounds(points) else {
        return Some(Point::default());
    };
    (max.x - min.x <= side && max.y - min.y <= side)
        .then(|| Point::new(min.x.floor(), min.y.floor()))
}

fn flatten(path: &[PathEl], origin: Point, out: &mut Vec<[f32; 4]>) -> Option<()> {
    let at = |p: Point| Point::new(p.x - origin.x, p.y - origin.y);
    let mut start = None;
    let mut current = Point::default();
    for el in path {
        match *el {
            PathEl::MoveTo(p) => {
                close(out, current, start)?;
                current = at(p);
                start = Some(current);
            }
            PathEl::LineTo(p) => {
                start.get_or_insert(current);
                current = line(out, current, at(p))?;
            }
            PathEl::QuadTo([control, end]) => {
                start.get_or_insert(current);
                let (p0, p1, p2) = (current, at(control), at(end));
                let steps = quad_steps(p0, p1, p2)?;
                for i in 1..steps {
                    let t = i as f32 / steps as f32;
                    current = line(out, current, p0.lerp(p1, t).lerp(p1.lerp(p2, t), t))?;
                }
                current = line(out, current, p2)?;
            }
            PathEl::CurveTo([control0, control1, end]) => {
                start.get_or_insert(current);
                let curve = [current, at(control0), at(control1), at(end)];
                current = flatten_cubic(out, curve)?;
            }
            PathEl::Close => {
                close(out, current, start)?;
                if let Some(s) = start {
                    current = s;
                }
            }
        }
    }
    close(out, current, start)
}

/// The split `FreeType`'s gray raster gives a quadratic: halve the step until the
/// second difference is within a quarter pixel on both axes. A tighter
/// bound draws convex curves heavier than every FreeType-based renderer.
#[expect(
    clippy::suboptimal_flops,
    reason = "mul_add is a libm call without target FMA"
)]
fn quad_steps(p0: Point, p1: Point, p2: Point) -> Option<u32> {
    let d = (p0.x - 2.0 * p1.x + p2.x)
        .abs()
        .max((p0.y - 2.0 * p1.y + p2.y).abs());
    (0..=MAX_CURVE_DEPTH)
        .find(|&depth| d <= 0.25 * 4_f32.powi(depth.cast_signed()))
        .map(|depth| 1 << depth)
}

/// The split `FreeType`'s gray raster gives a cubic: bisect until both control
/// points lie within half a pixel of the chord's thirds on both axes.
fn flatten_cubic(out: &mut Vec<[f32; 4]>, curve: [Point; 4]) -> Option<Point> {
    #[expect(
        clippy::suboptimal_flops,
        reason = "mul_add is a libm call without target FMA"
    )]
    fn flat([a, b, c, d]: [Point; 4]) -> bool {
        (2.0 * a.x - 3.0 * b.x + d.x).abs() <= 0.5
            && (2.0 * a.y - 3.0 * b.y + d.y).abs() <= 0.5
            && (a.x - 3.0 * c.x + 2.0 * d.x).abs() <= 0.5
            && (a.y - 3.0 * c.y + 2.0 * d.y).abs() <= 0.5
    }
    let mut stack = [(0, curve); MAX_CURVE_DEPTH as usize + 1];
    let mut len = 1;
    let mut current = curve[0];
    while len > 0 {
        len -= 1;
        let (depth, piece) = stack[len];
        if flat(piece) {
            current = line(out, current, piece[3])?;
            continue;
        }
        if depth == MAX_CURVE_DEPTH {
            return None;
        }
        let [a, b, c, d] = piece;
        let (ab, bc, cd) = (a.lerp(b, 0.5), b.lerp(c, 0.5), c.lerp(d, 0.5));
        let (abc, bcd) = (ab.lerp(bc, 0.5), bc.lerp(cd, 0.5));
        let mid = abc.lerp(bcd, 0.5);
        stack[len] = (depth + 1, [mid, bcd, cd, d]);
        stack[len + 1] = (depth + 1, [a, ab, abc, mid]);
        len += 2;
    }
    Some(current)
}

fn line(out: &mut Vec<[f32; 4]>, from: Point, to: Point) -> Option<Point> {
    if from != to {
        if out.len() == SEGMENT_CAP {
            return None;
        }
        out.push([from.x, from.y, to.x, to.y]);
    }
    Some(to)
}

fn close(out: &mut Vec<[f32; 4]>, current: Point, start: Option<Point>) -> Option<()> {
    match start {
        Some(start) => line(out, current, start).map(drop),
        None => Some(()),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    /// Signed height of the edges crossing the cell, in `1/ONE` px.
    cover: i32,
    /// `Σ cover · (x_entry + x_exit)` with x relative to the cell's left.
    area: i32,
}

/// Adds the line from `from` to `to` (24.8, y down, inside the
/// `width`-wide grid `cells`) to every cell it crosses.
fn deposit(cells: &mut [Cell], width: usize, from: (i32, i32), to: (i32, i32)) {
    if from.1 == to.1 {
        return;
    }
    let (sign, (xa, ya), (xb, yb)) = if from.1 < to.1 {
        (1, from, to)
    } else {
        (-1, to, from)
    };
    let (dx, dy) = (i64::from(xb - xa), i64::from(yb - ya));
    let (mut x, mut y) = (xa, ya);
    let mut row = ya / ONE;
    while y < yb {
        let row_top = row * ONE;
        let y_end = (row_top + ONE).min(yb);
        let x_end = if y_end == yb {
            xb
        } else {
            xa + (i64::from(y_end - ya) * dx / dy) as i32
        };
        let start = row as usize * width;
        deposit_row(
            &mut cells[start..start + width],
            sign,
            (x, y - row_top),
            (x_end, y_end - row_top),
        );
        (x, y) = (x_end, y_end);
        row += 1;
    }
}

/// One row's piece of an edge, y relative to the row and `from.1 < to.1`.
#[expect(
    clippy::cast_possible_wrap,
    reason = "the budget keeps every side under MAX_SIDE"
)]
fn deposit_row(row: &mut [Cell], sign: i32, from: (i32, i32), to: (i32, i32)) {
    let last = row.len() as i32 - 1;
    let (dx, dy) = (i64::from(to.0 - from.0), i64::from(to.1 - from.1));
    let step = if dx < 0 { -1 } else { 1 };
    let mut col = if dx < 0 {
        (from.0 + ONE - 1) / ONE - 1
    } else {
        from.0 / ONE
    }
    .min(last);
    let (mut x, mut y) = from;
    loop {
        let cell_left = col * ONE;
        let edge = if dx < 0 { cell_left } else { cell_left + ONE };
        let (nx, ny) = if dx == 0 || (dx > 0 && to.0 <= edge) || (dx < 0 && to.0 >= edge) {
            to
        } else {
            (edge, from.1 + (i64::from(edge - from.0) * dy / dx) as i32)
        };
        let cover = sign * (ny - y);
        let cell = &mut row[col as usize];
        cell.cover += cover;
        cell.area += cover * (x - cell_left + nx - cell_left);
        if (nx, ny) == to {
            return;
        }
        (x, y) = (nx, ny);
        col += step;
    }
}

/// Prefix-sums one row's cover and writes `min(|coverage|, 1)` as A8,
/// `floor(256 · coverage)` clamped to 255 as `FreeType`'s gray raster does.
fn accumulate_row(row: &[Cell], out: &mut [u8]) {
    let mut cover = 0_i64;
    for (cell, px) in row.iter().zip(out) {
        cover += i64::from(cell.cover);
        let value = (cover * 2 * i64::from(ONE) - i64::from(cell.area)).unsigned_abs();
        *px = (value / (2 * ONE as u64)).min(255) as u8;
    }
}

fn trim(mut data: Vec<u8>, width: usize, height: usize, left: i32, top: i32) -> Mask {
    let ink = |r: usize, c: usize| data[r * width + c] != 0;
    let row_ink = |r: &usize| (0..width).any(|c| ink(*r, c));
    let Some(r0) = (0..height).find(row_ink) else {
        return Mask::default();
    };
    let r1 = (0..height).rfind(row_ink).unwrap_or(r0);
    let col_ink = |c: &usize| (r0..=r1).any(|r| ink(r, *c));
    let c0 = (0..width).find(col_ink).unwrap_or(0);
    let c1 = (0..width).rfind(col_ink).unwrap_or(c0);
    let (w, h) = (c1 - c0 + 1, r1 - r0 + 1);
    if (w, h) != (width, height) {
        for (i, r) in (r0..=r1).enumerate() {
            let src = r * width + c0;
            data.copy_within(src..src + w, i * w);
        }
        data.truncate(w * h);
        data.shrink_to_fit();
    }
    Mask {
        width: w as u32,
        height: h as u32,
        left: left.saturating_add_unsigned(c0 as u32),
        top: top.saturating_sub_unsigned(r0 as u32),
        data,
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{Cell, ONE, accumulate_row, deposit};

    #[kani::proof]
    #[kani::unwind(5)]
    fn deposit_stays_in_its_cells_and_conserves_cover() {
        let width: usize = kani::any_where(|w| (1..=2).contains(w));
        let height: usize = kani::any_where(|h| (1..=2).contains(h));
        let (xmax, ymax) = (width as i32 * ONE, height as i32 * ONE);
        let mut point = || {
            (
                kani::any_where(|x: &i32| (0..=xmax).contains(x)),
                kani::any_where(|y: &i32| (0..=ymax).contains(y)),
            )
        };
        let (from, to) = (point(), point());
        let mut cells = [Cell::default(); 4];
        deposit(&mut cells[..width * height], width, from, to);
        let mut total = 0;
        for cell in &cells {
            total += cell.cover;
            assert!(cell.area.abs() <= cell.cover.abs() * 2 * ONE);
        }
        assert_eq!(total, to.1 - from.1);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn accumulate_row_never_overflows() {
        let row: [Cell; 4] = core::array::from_fn(|_| Cell {
            cover: kani::any(),
            area: kani::any(),
        });
        let mut out = [0_u8; 4];
        accumulate_row(&row, &mut out);
        if row.iter().all(|c| c.cover == 0 && c.area == 0) {
            assert!(out.iter().all(|&v| v == 0));
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Mask, PathEl, Point, origin, rasterize, side_limit};

    const PX: f32 = 32.0;

    fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn polygon(points: &[Point]) -> Vec<PathEl> {
        let mut path = vec![PathEl::MoveTo(points[0])];
        path.extend(points[1..].iter().map(|&q| PathEl::LineTo(q)));
        path.push(PathEl::Close);
        path
    }

    /// The bitmap, and the flattened segments shifted back to the path's
    /// own coordinates.
    fn render(path: &[PathEl]) -> (Mask, Vec<[f32; 4]>) {
        let mut segments = Vec::new();
        let mask = rasterize(path, PX, &mut segments).expect("inside every limit");
        let o = origin(path, side_limit(PX).unwrap_or(0.0)).unwrap_or_default();
        for s in &mut segments {
            *s = [s[0] + o.x, s[1] + o.y, s[2] + o.x, s[3] + o.y];
        }
        (mask, segments)
    }

    /// The A8 value at the pixel whose bottom-left corner is `(x, y)`.
    fn at(mask: &Mask, x: i32, y: i32) -> u8 {
        let (col, row) = (x - mask.left, mask.top - 1 - y);
        let (width, height) = (mask.width.cast_signed(), mask.height.cast_signed());
        if (0..width).contains(&col) && (0..height).contains(&row) {
            mask.data[(row * width + col) as usize]
        } else {
            0
        }
    }

    /// Signed area the edge `s` (y up) contributes to the unit pixel at
    /// `(x, y)`: the integral over the y it spans there of the pixel's
    /// width right of the edge.
    #[expect(
        clippy::float_cmp,
        reason = "only an exactly horizontal or vertical edge has no slope"
    )]
    fn edge_area(s: [f32; 4], x: f64, y: f64) -> f64 {
        let [x0, y0, x1, y1] = s.map(f64::from);
        if y0 == y1 {
            return 0.0;
        }
        let (lo, hi) = (y0.min(y1).max(y), y0.max(y1).min(y + 1.0));
        if lo >= hi {
            return 0.0;
        }
        let x_at = |v: f64| x0 + (v - y0) * (x1 - x0) / (y1 - y0);
        let right = |v: f64| (x + 1.0 - x_at(v)).clamp(0.0, 1.0);
        let mut cuts = vec![lo, hi];
        if x0 != x1 {
            for edge in [x, x + 1.0] {
                let v = y0 + (edge - x0) * (y1 - y0) / (x1 - x0);
                if lo < v && v < hi {
                    cuts.push(v);
                }
            }
        }
        cuts.sort_by(f64::total_cmp);
        let area: f64 = cuts
            .windows(2)
            .map(|w| (w[1] - w[0]) * (right(w[0]) + right(w[1])) / 2.0)
            .sum();
        area * (y1 - y0).signum()
    }

    /// Segments whose box comes within 1/64 px of the pixel at `(x, y)`.
    fn touching(segments: &[[f32; 4]], x: i32, y: i32) -> u32 {
        let margin = 1.0 / 64.0;
        let near = |s: &&[f32; 4]| {
            s[0].min(s[2]) - margin < (x + 1) as f32
                && s[0].max(s[2]) + margin > x as f32
                && s[1].min(s[3]) - margin < (y + 1) as f32
                && s[1].max(s[3]) + margin > y as f32
        };
        segments.iter().filter(near).count() as u32
    }

    /// Checks `mask` against the exact accumulated coverage of the
    /// flattened `segments`: one A8 step of flooring, plus two for the
    /// 24.8 rounding and truncation of every edge touching the pixel.
    fn check_against_oracle(mask: &Mask, segments: &[[f32; 4]]) -> Result<(), TestCaseError> {
        let span = |i: usize| {
            let values = segments.iter().flat_map(|s| [s[i], s[i + 2]]);
            let low = values.clone().fold(f32::MAX, f32::min).floor() as i32;
            (low - 1)..(values.fold(f32::MIN, f32::max).ceil() as i32 + 1)
        };
        for y in span(1) {
            for x in span(0) {
                let (fx, fy) = (f64::from(x), f64::from(y));
                let exact = segments
                    .iter()
                    .map(|&s| edge_area(s, fx, fy))
                    .sum::<f64>()
                    .abs()
                    .min(1.0)
                    * 256.0;
                let touching = touching(segments, x, y);
                let got = f64::from(at(mask, x, y));
                let err = (got - exact.min(255.0)).abs();
                let allowed = 2.0f64.mul_add(f64::from(touching), 1.0);
                prop_assert!(
                    err <= allowed + 1e-9,
                    "pixel ({x},{y}): got {got}, exact {exact:.3}, {touching} edges"
                );
            }
        }
        Ok(())
    }

    fn point() -> impl Strategy<Value = Point> + Clone {
        (-8.0_f32..40.0, -8.0_f32..40.0).prop_map(|(x, y)| p(x, y))
    }

    /// Coordinates on the 1/64 grid, which f32 holds exactly after any
    /// whole-pixel shift used here.
    fn grid_point() -> impl Strategy<Value = Point> + Clone {
        (0_i16..2048, 0_i16..2048).prop_map(|(x, y)| p(f32::from(x) / 64.0, f32::from(y) / 64.0))
    }

    /// A closed contour, and the same contour drawn the other way round.
    fn contour(
        point: impl Strategy<Value = Point> + Clone,
    ) -> impl Strategy<Value = [Vec<PathEl>; 2]> {
        let el = prop_oneof![
            point.clone().prop_map(PathEl::LineTo),
            [point.clone(), point.clone()].prop_map(PathEl::QuadTo),
            [point.clone(), point.clone(), point.clone()].prop_map(PathEl::CurveTo),
        ];
        (point, prop::collection::vec(el, 1..6)).prop_map(|(start, els)| {
            let end = |el: &PathEl| match *el {
                PathEl::LineTo(q) | PathEl::QuadTo([_, q]) | PathEl::CurveTo([_, _, q]) => q,
                PathEl::MoveTo(_) | PathEl::Close => unreachable!("only drawing commands"),
            };
            let ends: Vec<Point> = std::iter::once(start).chain(els.iter().map(end)).collect();
            let mut back = vec![PathEl::MoveTo(start), PathEl::LineTo(ends[els.len()])];
            back.extend(els.iter().zip(&ends).rev().map(|(el, &from)| match *el {
                PathEl::QuadTo([c, _]) => PathEl::QuadTo([c, from]),
                PathEl::CurveTo([c0, c1, _]) => PathEl::CurveTo([c1, c0, from]),
                _ => PathEl::LineTo(from),
            }));
            let mut forth = vec![PathEl::MoveTo(start)];
            forth.extend(els);
            forth.push(PathEl::Close);
            back.push(PathEl::Close);
            [forth, back]
        })
    }

    fn path(point: impl Strategy<Value = Point> + Clone) -> impl Strategy<Value = Vec<PathEl>> {
        paths(point).prop_map(|[forth, _]| forth)
    }

    /// A path of up to three contours, and the same path with every
    /// contour reversed.
    fn paths(
        point: impl Strategy<Value = Point> + Clone,
    ) -> impl Strategy<Value = [Vec<PathEl>; 2]> {
        prop::collection::vec(contour(point), 1..4).prop_map(|c| {
            let (forth, back): (Vec<_>, Vec<_>) = c.into_iter().map(<(_, _)>::from).unzip();
            [forth.concat(), back.concat()]
        })
    }

    /// A convex polygon, counter-clockwise: points around a center sorted
    /// by angle.
    fn convex(center: Point, radius: f32, angles: &mut [f32]) -> Vec<Point> {
        angles.sort_by(f32::total_cmp);
        angles
            .iter()
            .map(|a| {
                p(
                    radius.mul_add(a.cos(), center.x),
                    radius.mul_add(a.sin(), center.y),
                )
            })
            .collect()
    }

    fn convex_polygon() -> impl Strategy<Value = Vec<Point>> {
        (
            point(),
            1.0_f32..12.0,
            prop::collection::vec(0.0_f32..std::f32::consts::TAU, 3..9),
        )
            .prop_map(|(c, r, mut a)| convex(c, r, &mut a))
    }

    proptest! {
        #[test]
        fn coverage_matches_the_exact_area_of_the_flattened_outline(path in path(point())) {
            let (mask, segments) = render(&path);
            check_against_oracle(&mask, &segments)?;
        }

        #[test]
        fn contours_of_one_winding_cover_at_least_each_and_at_most_their_sum(
            polygons in prop::collection::vec(convex_polygon(), 1..4),
        ) {
            let whole: Vec<PathEl> = polygons.iter().flat_map(|q| polygon(q)).collect();
            let (mask, segments) = render(&whole);
            let parts: Vec<Mask> = polygons.iter().map(|q| render(&polygon(q)).0).collect();
            for y in -20..60 {
                for x in -20..60 {
                    let each: Vec<u32> = parts.iter().map(|m| u32::from(at(m, x, y))).collect();
                    let got = u32::from(at(&mask, x, y));
                    let most = each.iter().copied().max().unwrap_or(0);
                    let sum: u32 = each.iter().sum();
                    let slack = 2 * touching(&segments, x, y);
                    prop_assert!(got + slack >= most, "({x},{y}): {got} under a part's {most}");
                    prop_assert!(got <= 255.min(sum + parts.len() as u32 - 1 + slack), "({x},{y}): {got} over the parts' {sum}");
                }
            }
        }

        #[test]
        fn a_simple_polygon_covers_its_shoelace_area(points in convex_polygon()) {
            let (mask, _) = render(&polygon(&points));
            let n = points.len();
            let shoelace = (0..n)
                .map(|i| {
                    let (a, b) = (points[i], points[(i + 1) % n]);
                    f64::from(a.x).mul_add(f64::from(b.y), -f64::from(b.x) * f64::from(a.y))
                })
                .sum::<f64>()
                .abs()
                / 2.0;
            let perimeter: f64 = (0..n).map(|i| f64::from((points[(i + 1) % n].x - points[i].x).hypot(points[(i + 1) % n].y - points[i].y))).sum();
            let covered = mask.data.iter().map(|&v| f64::from(v)).sum::<f64>() / 256.0;
            let floor_loss = mask.data.iter().filter(|&&v| v != 0).count() as f64 / 256.0;
            let quantization = perimeter * 3.0 / 256.0;
            prop_assert!(covered <= shoelace + quantization, "{covered} over {shoelace}");
            prop_assert!(covered + floor_loss + quantization >= shoelace, "{covered} under {shoelace}");
        }

        #[test]
        fn a_whole_pixel_shift_moves_the_bitmap_and_nothing_else(
            path in path(grid_point()),
            dx in -64_i16..64,
            dy in -64_i16..64,
        ) {
            let (dx, dy) = (f32::from(dx), f32::from(dy));
            let shifted: Vec<PathEl> = path
                .iter()
                .map(|el| match *el {
                    PathEl::MoveTo(q) => PathEl::MoveTo(p(q.x + dx, q.y + dy)),
                    PathEl::LineTo(q) => PathEl::LineTo(p(q.x + dx, q.y + dy)),
                    PathEl::QuadTo(q) => PathEl::QuadTo(q.map(|q| p(q.x + dx, q.y + dy))),
                    PathEl::CurveTo(q) => PathEl::CurveTo(q.map(|q| p(q.x + dx, q.y + dy))),
                    PathEl::Close => PathEl::Close,
                })
                .collect();
            let (a, b) = (render(&path).0, render(&shifted).0);
            prop_assert_eq!(&a.data, &b.data);
            prop_assert_eq!((a.width, a.height), (b.width, b.height));
            if !a.data.is_empty() {
                prop_assert_eq!((a.left + dx as i32, a.top + dy as i32), (b.left, b.top));
            }
        }

        #[test]
        fn reversing_every_contour_keeps_the_coverage([forth, back] in paths(grid_point())) {
            let (a, segments) = render(&forth);
            let b = render(&back).0;
            for y in -10..45 {
                for x in -10..45 {
                    let (ga, gb) = (at(&a, x, y), at(&b, x, y));
                    let slack = 1 + 2 * touching(&segments, x, y);
                    prop_assert!(u32::from(ga.abs_diff(gb)) <= slack, "({x},{y}): {ga} vs {gb}");
                }
            }
        }

        #[test]
        fn the_box_is_the_ink_box_not_the_control_box(
            x0 in 0.0_f32..8.0,
            x2 in 12.0_f32..24.0,
            height in 40.0_f32..200.0,
            cubic in any::<bool>(),
        ) {
            let (start, end) = (p(x0, 0.0), p(x2, 0.0));
            let (curve, apex) = if cubic {
                (PathEl::CurveTo([p(x0, height), p(x2, height), end]), height * 0.75)
            } else {
                (PathEl::QuadTo([p(x0.midpoint(x2), height), end]), height / 2.0)
            };
            let mask = render(&[PathEl::MoveTo(start), curve, PathEl::Close]).0;
            prop_assert!(mask.top <= apex.ceil() as i32, "top {} above the apex {apex}", mask.top);
            let (w, h) = (mask.width as usize, mask.height as usize);
            let row = |r: usize| mask.data[r * w..][..w].iter().any(|&v| v != 0);
            let col = |c: usize| (0..h).any(|r| mask.data[r * w + c] != 0);
            prop_assert!(row(0) && row(h - 1) && col(0) && col(w - 1));
        }
    }

    #[test]
    fn an_empty_or_inkless_outline_is_an_empty_bitmap() {
        let mut segments = Vec::new();
        assert_eq!(rasterize(&[], PX, &mut segments), Some(Mask::default()));
        let line = polygon(&[p(0.0, 0.0), p(10.0, 10.0)]);
        assert_eq!(rasterize(&line, PX, &mut segments), Some(Mask::default()));
    }

    #[test]
    fn inkless_edge_rows_and_columns_are_trimmed_from_the_box() {
        let mut segments = Vec::new();
        let sliver = polygon(&[p(0.001, 0.0), p(4.0, 0.0), p(4.0, 4.001), p(0.001, 4.001)]);
        let mask = rasterize(&sliver, PX, &mut segments).expect("inside every limit");
        assert_eq!((mask.width, mask.height, mask.left, mask.top), (4, 4, 0, 4));
        assert_eq!(mask.data.len(), 16);
        assert!(mask.data[..12].iter().all(|&a| a == 255), "{:?}", mask.data);
    }

    #[test]
    fn an_outline_over_a_limit_has_no_bitmap() {
        let mut segments = Vec::new();
        let square = |side: f32| polygon(&[p(0.0, 0.0), p(side, 0.0), p(side, side), p(0.0, side)]);
        for bad in [f32::NAN, f32::INFINITY, 1e30] {
            let path = polygon(&[p(0.0, 0.0), p(bad, 1.0), p(1.0, 1.0)]);
            assert_eq!(rasterize(&path, PX, &mut segments), None, "point at {bad}");
        }
        assert_eq!(
            rasterize(&square(1.0), f32::NAN, &mut segments),
            None,
            "NaN size"
        );
        assert_eq!(
            rasterize(&square(300.0), PX, &mut segments),
            None,
            "past 8 em per side"
        );
        assert!(
            rasterize(&square(1300.0), 1000.0, &mut segments).is_some(),
            "inside the working set"
        );
        assert_eq!(
            rasterize(&square(1400.0), 1000.0, &mut segments),
            None,
            "past the working set"
        );
        let zigzag = |n: usize| {
            let points: Vec<Point> = (0..n)
                .map(|i| p(i as f32 / 1000.0, (i % 2) as f32))
                .collect();
            polygon(&points)
        };
        assert!(rasterize(&zigzag(15_000), PX, &mut segments).is_some());
        assert_eq!(
            rasterize(&zigzag(17_000), PX, &mut segments),
            None,
            "past the segment cap"
        );
    }
}
