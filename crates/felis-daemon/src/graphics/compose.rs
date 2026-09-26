//! Frame compositing for Kitty animation (`a=f` / `a=c`).
//!
//! felis stores coalesced frames (docs/reference/protocols/kitty-graphics.md),
//! so the compositor produces full canvases beside the decoder.

use felis_protocol::messages::ImageFormat;

/// Widening (3→4) sets full alpha; narrowing (4→3) drops it.
pub(crate) fn convert_bpp(pixels: &[u8], src_bpp: usize, dst_bpp: usize) -> Vec<u8> {
    if src_bpp == dst_bpp {
        return pixels.to_vec();
    }
    let count = pixels.len() / src_bpp;
    let mut out = Vec::with_capacity(count * dst_bpp);
    for px in pixels.chunks_exact(src_bpp) {
        out.push(px[0]);
        out.push(px[1]);
        out.push(px[2]);
        if dst_bpp == 4 {
            out.push(255);
        }
    }
    out
}

/// A `width × height × bpp` canvas filled with the Kitty background
/// color `Y` (`0xRRGGBBAA`, default `0` = transparent black).
pub(crate) fn background_canvas(
    width: u32,
    height: u32,
    format: ImageFormat,
    bgcolor: u32,
) -> Vec<u8> {
    let bpp = format.bytes_per_pixel();
    let px_count = (width as usize) * (height as usize);
    let r = ((bgcolor >> 24) & 0xff) as u8;
    let g = ((bgcolor >> 16) & 0xff) as u8;
    let b = ((bgcolor >> 8) & 0xff) as u8;
    let a = (bgcolor & 0xff) as u8;
    let mut canvas = Vec::with_capacity(px_count * bpp);
    for _ in 0..px_count {
        canvas.push(r);
        canvas.push(g);
        canvas.push(b);
        if bpp == 4 {
            canvas.push(a);
        }
    }
    canvas
}

/// Straight-alpha "over" blend, in place on `under`; mirrors kitty's
/// `alpha_blend`. `u32` intermediates: `u * ua * (255 - oa)` overflows
/// `u16`.
fn alpha_blend(under: &mut [u8], over: &[u8]) {
    let oa = u32::from(over[3]);
    if oa == 0 {
        return;
    }
    if oa == 255 {
        under.copy_from_slice(&over[..4]);
        return;
    }
    let ua = u32::from(under[3]);
    let comp = ua * (255 - oa) / 255;
    let out_a = oa + comp;
    for c in 0..3 {
        let o = u32::from(over[c]);
        let u = u32::from(under[c]);
        let num = o * oa + u * comp;
        under[c] = num.checked_div(out_a).map_or(0, |v| v.min(255) as u8);
    }
    under[3] = out_a as u8;
}

/// The `a=f` path, mirroring kitty `compose()`: place `src` into `dst`
/// at `(off_x, off_y)`, clipping to the canvas. `replace` (or a 3-byte
/// canvas) overwrites; a 4-byte canvas alpha-blends.
pub(crate) fn blit(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    off_x: u32,
    off_y: u32,
    bpp: usize,
    replace: bool,
) {
    let blend = !replace && bpp == 4;
    let rows = src_h.min(dst_h.saturating_sub(off_y));
    let cols = src_w.min(dst_w.saturating_sub(off_x));
    for y in 0..rows {
        let dst_row = (((y + off_y) * dst_w + off_x) as usize) * bpp;
        let src_row = ((y * src_w) as usize) * bpp;
        for x in 0..cols as usize {
            let d = dst_row + x * bpp;
            let s = src_row + x * bpp;
            if blend {
                let (head, _) = dst.split_at_mut(d + 4);
                alpha_blend(&mut head[d..d + 4], &src[s..s + 4]);
            } else {
                dst[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
            }
        }
    }
}

/// The `a=c` path, mirroring kitty `compose_rectangles()`. Callers
/// validate bounds and source/dest overlap first.
pub(crate) fn blit_region(
    dst: &mut [u8],
    src: &[u8],
    img_w: u32,
    dst_x: u32,
    dst_y: u32,
    src_x: u32,
    src_y: u32,
    w: u32,
    h: u32,
    bpp: usize,
    replace: bool,
) {
    let blend = !replace && bpp == 4;
    for y in 0..h {
        let dst_row = (((dst_y + y) * img_w + dst_x) as usize) * bpp;
        let src_row = (((src_y + y) * img_w + src_x) as usize) * bpp;
        for x in 0..w as usize {
            let d = dst_row + x * bpp;
            let s = src_row + x * bpp;
            if blend {
                let (head, _) = dst.split_at_mut(d + 4);
                alpha_blend(&mut head[d..d + 4], &src[s..s + 4]);
            } else {
                dst[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn background_canvas_fills_rgba() {
        let c = background_canvas(2, 1, ImageFormat::Rgba32, 0xFF00_00FF);
        assert_eq!(c, vec![0xFF, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn background_canvas_rgb_drops_alpha() {
        let c = background_canvas(2, 1, ImageFormat::Rgb24, 0x10_20_30_FF);
        assert_eq!(c, vec![0x10, 0x20, 0x30, 0x10, 0x20, 0x30]);
    }

    /// The clipped rectangle `blit` writes, as a direct copy.
    fn expected_replace(
        dst: &[u8],
        dst_w: u32,
        dst_h: u32,
        src: &[u8],
        src_w: u32,
        src_h: u32,
        off_x: u32,
        off_y: u32,
        bpp: usize,
    ) -> Vec<u8> {
        let mut out = dst.to_vec();
        let rows = src_h.min(dst_h.saturating_sub(off_y));
        let cols = src_w.min(dst_w.saturating_sub(off_x));
        for y in 0..rows {
            for x in 0..cols {
                let d = ((((y + off_y) * dst_w) + off_x + x) as usize) * bpp;
                let s = (((y * src_w) + x) as usize) * bpp;
                out[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
            }
        }
        out
    }

    const MAX_DIM: u32 = 4;

    /// Enough bytes for any canvas the properties below ask for.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(any::<u8>(), (MAX_DIM * MAX_DIM * 4) as usize)
    }

    fn sized(pool: &[u8], w: u32, h: u32, bpp: usize) -> Vec<u8> {
        pool[..(w as usize) * (h as usize) * bpp].to_vec()
    }

    proptest! {
        #[test]
        fn convert_bpp_preserves_color_and_round_trips(
            pixels in prop::collection::vec(any::<u8>(), 0..64).prop_map(|mut v| {
                let keep = v.len() - v.len() % 4;
                v.truncate(keep);
                v
            }),
        ) {
            let narrowed = convert_bpp(&pixels, 4, 3);
            prop_assert_eq!(narrowed.len(), pixels.len() / 4 * 3);
            let widened = convert_bpp(&narrowed, 3, 4);
            prop_assert_eq!(widened.len(), pixels.len());
            for (out, src) in widened.as_chunks::<4>().0.iter().zip(pixels.as_chunks::<4>().0) {
                prop_assert_eq!(out, &[src[0], src[1], src[2], 255]);
            }
            prop_assert_eq!(convert_bpp(&pixels, 4, 4), pixels);
            prop_assert_eq!(convert_bpp(&narrowed, 3, 3), narrowed);
        }

        #[test]
        fn blit_writes_exactly_the_clipped_rectangle(
            dst_w in 1u32..=MAX_DIM,
            dst_h in 1u32..=MAX_DIM,
            src_w in 1u32..=MAX_DIM,
            src_h in 1u32..=MAX_DIM,
            bpp in prop_oneof![Just(3usize), Just(4)],
            off_x in 0u32..=MAX_DIM,
            off_y in 0u32..=MAX_DIM,
            dst_pool in bytes(),
            src_pool in bytes(),
        ) {
            let dst = sized(&dst_pool, dst_w, dst_h, bpp);
            let src = sized(&src_pool, src_w, src_h, bpp);
            let want = expected_replace(&dst, dst_w, dst_h, &src, src_w, src_h, off_x, off_y, bpp);
            let mut got = dst.clone();
            blit(&mut got, dst_w, dst_h, &src, src_w, src_h, off_x, off_y, bpp, true);
            prop_assert_eq!(&got, &want);
            if bpp == 3 {
                let mut got = dst;
                blit(&mut got, dst_w, dst_h, &src, src_w, src_h, off_x, off_y, bpp, false);
                prop_assert_eq!(got, want, "an opaque canvas never blends");
            }
        }

        #[test]
        fn blit_blend_honors_the_source_alpha_endpoints(
            dst_w in 1u32..=MAX_DIM,
            dst_h in 1u32..=MAX_DIM,
            dst_pool in bytes(),
            src_pool in bytes(),
            opaque in any::<bool>(),
        ) {
            let dst = sized(&dst_pool, dst_w, dst_h, 4);
            let mut src = sized(&src_pool, dst_w, dst_h, 4);
            for px in src.as_chunks_mut::<4>().0 {
                px[3] = if opaque { 255 } else { 0 };
            }
            let mut got = dst.clone();
            blit(&mut got, dst_w, dst_h, &src, dst_w, dst_h, 0, 0, 4, false);
            prop_assert_eq!(got, if opaque { src } else { dst });
        }

        /// kitty's `alpha_blend`: the result alpha is the "over"
        /// composite of the two, and no channel wraps.
        #[test]
        fn blit_blend_composites_alpha_without_overflow(
            under in prop::array::uniform4(any::<u8>()),
            over in prop::array::uniform4(any::<u8>()),
        ) {
            let mut got = under.to_vec();
            blit(&mut got, 1, 1, &over, 1, 1, 0, 0, 4, false);
            let (oa, ua) = (u32::from(over[3]), u32::from(under[3]));
            let want_a = oa + ua * (255 - oa) / 255;
            prop_assert_eq!(u32::from(got[3]), want_a.min(255));
        }
    }

    #[test]
    fn blit_alpha_blend_half_over_opaque_under() {
        let mut canvas = vec![255u8, 0, 0, 255];
        blit(&mut canvas, 1, 1, &[0, 255, 0, 128], 1, 1, 0, 0, 4, false);
        assert!(canvas[0] < 135 && canvas[0] > 120, "R={}", canvas[0]);
        assert!(canvas[1] > 120 && canvas[1] < 135, "G={}", canvas[1]);
        assert_eq!(canvas[3], 255, "result stays opaque");
    }

    #[test]
    fn blit_region_copies_rectangle() {
        let src = vec![1u8, 2, 3, 4, 9, 9, 9, 9];
        let mut dst = vec![0u8; 8];
        blit_region(&mut dst, &src, 2, 1, 0, 0, 0, 1, 1, 4, true);
        assert_eq!(&dst[4..8], &[1, 2, 3, 4]);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
    }
}
