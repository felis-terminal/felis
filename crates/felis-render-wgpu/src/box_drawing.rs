//! Programmatic rasterization of box-drawing, block, Braille, mosaic, and Powerline glyphs.
//!
//! Fonts size box-drawing glyphs to the body rather than the full cell height, leaving gaps
//! between rows. Rasterizing them directly ensures adjacent cells butt with no gap.

use felis_shaping::{CellMetrics, GlyphBitmap, GlyphPixels};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Weight {
    Light,
    Heavy,
    Double,
}

/// `[N, E, S, W]`; `None` means no stroke from the center to that edge.
type Strokes = [Option<Weight>; 4];

const N: usize = 0;
const E: usize = 1;
const S: usize = 2;
const W: usize = 3;

const L: Option<Weight> = Some(Weight::Light);
const H: Option<Weight> = Some(Weight::Heavy);
const D: Option<Weight> = Some(Weight::Double);
const X: Option<Weight> = None;

/// Each variant names the two sides the arc connects: `DownRight` (`╭`)
/// runs from the south edge to the east edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArcCorner {
    DownRight, // ╭
    DownLeft,  // ╮
    UpLeft,    // ╯
    UpRight,   // ╰
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// Whole cell at the given alpha (255 = full, lower = shade).
    Solid(u8),
    /// Bottom `n` eighths filled; `n` in `1..=8`.
    LowerEighths(u8),
    UpperEighths(u8),
    LeftEighths(u8),
    RightEighths(u8),
    /// 2×2 mosaic, one bit per quadrant in row-major order:
    /// `1 = upper-left, 2 = upper-right, 4 = lower-left, 8 = lower-right`.
    Quadrants(u8),
}

/// `cell.ascent` is the distance from cell top to baseline; the bitmap comes
/// back with `top = cell.ascent` (the module doc's placement contract).
/// `None` for unsupported code points, so the caller falls through to
/// the font.
#[must_use]
pub fn rasterize(c: char, cell: CellMetrics) -> Option<GlyphBitmap> {
    if cell.width == 0 || cell.height == 0 {
        return None;
    }
    let cp = c as u32;
    if (0x2800..=0x28FF).contains(&cp) {
        // Unicode allocated the block so bit `n` of `cp - 0x2800` is dot `n + 1`.
        return Some(rasterize_braille((cp - 0x2800) as u8, cell));
    }
    if (0x1FB00..=0x1FB3B).contains(&cp) {
        // Not a flat bit encoding: the block omits the four patterns with
        // pre-existing glyphs (empty, `▌` = 0b010101, `▐` = 0b101010, `█`),
        // so the offset gains one at each omission (kitty's decorations.c
        // uses the same three-way split).
        let n = cp - 0x1FB00;
        let bits = (n + 1 + n / 20) as u8;
        return Some(rasterize_mosaic(bits, 2, 3, cell));
    }
    if (0x1CD00..=0x1CDE5).contains(&cp) {
        let bits = octant_pattern((cp - 0x1CD00) as u8);
        return Some(rasterize_mosaic(bits, 2, 4, cell));
    }
    // Octant-shaped glyphs Unicode placed outside the contiguous octant
    // run: four single-octant eighth blocks and two middle-quarter blocks.
    let stray_octant = match cp {
        0x1CEA0 => Some(0b1000_0000), // 𜺠 octant 8 (lower-right eighth)
        0x1CEA3 => Some(0b0100_0000), // 𜺣 octant 7 (lower-left eighth)
        0x1CEA8 => Some(0b0000_0001), // 𜺨 octant 1 (upper-left eighth)
        0x1CEAB => Some(0b0000_0010), // 𜺫 octant 2 (upper-right eighth)
        0x1FBE6 => Some(0b0001_0100), // 🯦 octants 3+5 (middle left quarter)
        0x1FBE7 => Some(0b0010_1000), // 🯧 octants 4+6 (middle right quarter)
        _ => None,
    };
    if let Some(bits) = stray_octant {
        return Some(rasterize_mosaic(bits, 2, 4, cell));
    }
    if (0xE0B0..=0xE0BF).contains(&cp) {
        return Some(rasterize_powerline(c, cell));
    }
    if !(0x2500..=0x259F).contains(&cp) {
        return None;
    }
    if let Some(strokes) = line_spec(c) {
        return Some(rasterize_strokes(strokes, cell));
    }
    if let Some(corner) = arc_corner(c) {
        return Some(rasterize_arc(corner, cell));
    }
    if let Some(block) = block_spec(c) {
        return Some(rasterize_block(block, cell));
    }
    None
}

const fn arc_corner(c: char) -> Option<ArcCorner> {
    match c {
        '\u{256D}' => Some(ArcCorner::DownRight),
        '\u{256E}' => Some(ArcCorner::DownLeft),
        '\u{256F}' => Some(ArcCorner::UpLeft),
        '\u{2570}' => Some(ArcCorner::UpRight),
        _ => None,
    }
}

const fn line_spec(c: char) -> Option<Strokes> {
    let s: Strokes = match c {
        // Dashed variants render solid: connected dashes still read as a line.
        '\u{2500}' | '\u{2504}' | '\u{2508}' | '\u{254C}' => [X, L, X, L],
        '\u{2501}' | '\u{2505}' | '\u{2509}' | '\u{254D}' => [X, H, X, H],
        '\u{2502}' | '\u{2506}' | '\u{250A}' | '\u{254E}' => [L, X, L, X],
        '\u{2503}' | '\u{2507}' | '\u{250B}' | '\u{254F}' => [H, X, H, X],

        '\u{250C}' => [X, L, L, X], // ┌
        '\u{250D}' => [X, H, L, X], // ┍
        '\u{250E}' => [X, L, H, X], // ┎
        '\u{250F}' => [X, H, H, X], // ┏
        '\u{2510}' => [X, X, L, L], // ┐
        '\u{2511}' => [X, X, L, H], // ┑
        '\u{2512}' => [X, X, H, L], // ┒
        '\u{2513}' => [X, X, H, H], // ┓
        '\u{2514}' => [L, L, X, X], // └
        '\u{2515}' => [L, H, X, X], // ┕
        '\u{2516}' => [H, L, X, X], // ┖
        '\u{2517}' => [H, H, X, X], // ┗
        '\u{2518}' => [L, X, X, L], // ┘
        '\u{2519}' => [L, X, X, H], // ┙
        '\u{251A}' => [H, X, X, L], // ┚
        '\u{251B}' => [H, X, X, H], // ┛

        '\u{251C}' => [L, L, L, X], // ├
        '\u{251D}' => [L, H, L, X], // ┝
        '\u{251E}' => [H, L, L, X], // ┞
        '\u{251F}' => [L, L, H, X], // ┟
        '\u{2520}' => [H, L, H, X], // ┠
        '\u{2521}' => [H, H, L, X], // ┡
        '\u{2522}' => [L, H, H, X], // ┢
        '\u{2523}' => [H, H, H, X], // ┣
        '\u{2524}' => [L, X, L, L], // ┤
        '\u{2525}' => [L, X, L, H], // ┥
        '\u{2526}' => [H, X, L, L], // ┦
        '\u{2527}' => [L, X, H, L], // ┧
        '\u{2528}' => [H, X, H, L], // ┨
        '\u{2529}' => [H, X, L, H], // ┩
        '\u{252A}' => [L, X, H, H], // ┪
        '\u{252B}' => [H, X, H, H], // ┫
        '\u{252C}' => [X, L, L, L], // ┬
        '\u{252D}' => [X, L, L, H], // ┭
        '\u{252E}' => [X, H, L, L], // ┮
        '\u{252F}' => [X, H, L, H], // ┯
        '\u{2530}' => [X, L, H, L], // ┰
        '\u{2531}' => [X, L, H, H], // ┱
        '\u{2532}' => [X, H, H, L], // ┲
        '\u{2533}' => [X, H, H, H], // ┳
        '\u{2534}' => [L, L, X, L], // ┴
        '\u{2535}' => [L, L, X, H], // ┵
        '\u{2536}' => [L, H, X, L], // ┶
        '\u{2537}' => [L, H, X, H], // ┷
        '\u{2538}' => [H, L, X, L], // ┸
        '\u{2539}' => [H, L, X, H], // ┹
        '\u{253A}' => [H, H, X, L], // ┺
        '\u{253B}' => [H, H, X, H], // ┻
        '\u{253C}' => [L, L, L, L], // ┼
        '\u{253D}' => [L, L, L, H], // ┽
        '\u{253E}' => [L, H, L, L], // ┾
        '\u{253F}' => [L, H, L, H], // ┿
        '\u{2540}' => [H, L, L, L], // ╀
        '\u{2541}' => [L, L, H, L], // ╁
        '\u{2542}' => [H, L, H, L], // ╂
        '\u{2543}' => [H, L, L, H], // ╃
        '\u{2544}' => [H, H, L, L], // ╄
        '\u{2545}' => [L, L, H, H], // ╅
        '\u{2546}' => [L, H, H, L], // ╆
        '\u{2547}' => [H, H, L, H], // ╇
        '\u{2548}' => [L, H, H, H], // ╈
        '\u{2549}' => [H, L, H, H], // ╉
        '\u{254A}' => [H, H, H, L], // ╊
        '\u{254B}' => [H, H, H, H], // ╋

        '\u{2574}' => [X, X, X, L], // ╴
        '\u{2575}' => [L, X, X, X], // ╵
        '\u{2576}' => [X, L, X, X], // ╶
        '\u{2577}' => [X, X, L, X], // ╷
        '\u{2578}' => [X, X, X, H], // ╸
        '\u{2579}' => [H, X, X, X], // ╹
        '\u{257A}' => [X, H, X, X], // ╺
        '\u{257B}' => [X, X, H, X], // ╻
        '\u{257C}' => [X, H, X, L], // ╼
        '\u{257D}' => [L, X, H, X], // ╽
        '\u{257E}' => [X, L, X, H], // ╾
        '\u{257F}' => [H, X, L, X], // ╿

        '\u{2550}' => [X, D, X, D], // ═
        '\u{2551}' => [D, X, D, X], // ║
        '\u{2552}' => [X, D, L, X], // ╒
        '\u{2553}' => [X, L, D, X], // ╓
        '\u{2554}' => [X, D, D, X], // ╔
        '\u{2555}' => [X, X, L, D], // ╕
        '\u{2556}' => [X, X, D, L], // ╖
        '\u{2557}' => [X, X, D, D], // ╗
        '\u{2558}' => [L, D, X, X], // ╘
        '\u{2559}' => [D, L, X, X], // ╙
        '\u{255A}' => [D, D, X, X], // ╚
        '\u{255B}' => [L, X, X, D], // ╛
        '\u{255C}' => [D, X, X, L], // ╜
        '\u{255D}' => [D, X, X, D], // ╝
        '\u{255E}' => [L, D, L, X], // ╞
        '\u{255F}' => [D, L, D, X], // ╟
        '\u{2560}' => [D, D, D, X], // ╠
        '\u{2561}' => [L, X, L, D], // ╡
        '\u{2562}' => [D, X, D, L], // ╢
        '\u{2563}' => [D, X, D, D], // ╣
        '\u{2564}' => [X, D, L, D], // ╤
        '\u{2565}' => [X, L, D, L], // ╥
        '\u{2566}' => [X, D, D, D], // ╦
        '\u{2567}' => [L, D, X, D], // ╧
        '\u{2568}' => [D, L, X, L], // ╨
        '\u{2569}' => [D, D, X, D], // ╩
        '\u{256A}' => [L, D, L, D], // ╪
        '\u{256B}' => [D, L, D, L], // ╫
        '\u{256C}' => [D, D, D, D], // ╬

        _ => return None,
    };
    Some(s)
}

const fn block_spec(c: char) -> Option<Block> {
    let b = match c {
        '\u{2580}' => Block::UpperEighths(4), // ▀ upper half
        '\u{2581}' => Block::LowerEighths(1), // ▁
        '\u{2582}' => Block::LowerEighths(2), // ▂
        '\u{2583}' => Block::LowerEighths(3), // ▃
        '\u{2584}' => Block::LowerEighths(4), // ▄ lower half
        '\u{2585}' => Block::LowerEighths(5), // ▅
        '\u{2586}' => Block::LowerEighths(6), // ▆
        '\u{2587}' => Block::LowerEighths(7), // ▇
        '\u{2588}' => Block::Solid(255),      // █ full block
        '\u{2589}' => Block::LeftEighths(7),  // ▉
        '\u{258A}' => Block::LeftEighths(6),  // ▊
        '\u{258B}' => Block::LeftEighths(5),  // ▋
        '\u{258C}' => Block::LeftEighths(4),  // ▌ left half
        '\u{258D}' => Block::LeftEighths(3),  // ▍
        '\u{258E}' => Block::LeftEighths(2),  // ▎
        '\u{258F}' => Block::LeftEighths(1),  // ▏
        '\u{2590}' => Block::RightEighths(4), // ▐ right half
        // Uniform alpha rather than kitty's stipple: it reads the same
        // once the fg color multiplies through.
        '\u{2591}' => Block::Solid(64),         // ░
        '\u{2592}' => Block::Solid(128),        // ▒
        '\u{2593}' => Block::Solid(192),        // ▓
        '\u{2594}' => Block::UpperEighths(1),   // ▔ upper 1/8
        '\u{2595}' => Block::RightEighths(1),   // ▕ right 1/8
        '\u{2596}' => Block::Quadrants(0b0100), // ▖
        '\u{2597}' => Block::Quadrants(0b1000), // ▗
        '\u{2598}' => Block::Quadrants(0b0001), // ▘
        '\u{2599}' => Block::Quadrants(0b1101), // ▙
        '\u{259A}' => Block::Quadrants(0b1001), // ▚
        '\u{259B}' => Block::Quadrants(0b0111), // ▛
        '\u{259C}' => Block::Quadrants(0b1011), // ▜
        '\u{259D}' => Block::Quadrants(0b0010), // ▝
        '\u{259E}' => Block::Quadrants(0b0110), // ▞
        '\u{259F}' => Block::Quadrants(0b1110), // ▟
        _ => return None,
    };
    Some(b)
}

/// Out-of-range coordinates clip silently so the stroke helpers need
/// no bounds branches.
fn fill_rect(pixels: &mut [u8], cell_w: u32, x0: u32, y0: u32, x1: u32, y1: u32, alpha: u8) {
    let x1 = x1.min(cell_w);
    let y1 = y1.min((pixels.len() as u32) / cell_w);
    for y in y0..y1 {
        let row = (y * cell_w) as usize;
        for x in x0..x1 {
            pixels[row + x as usize] = alpha;
        }
    }
}

fn draw_solid_stroke(pixels: &mut [u8], cell: CellMetrics, side: usize, t: u32) {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let cx = cell_w / 2;
    let cy = cell_h / 2;
    // Asymmetric split rather than `t/2` on both sides: the bar is
    // exactly `t` wide for any `t`, with the center pixel on the axis
    // for odd `t`.
    let half = t / 2;
    let t_rem = t - half;
    match side {
        // Runs `t_rem` past the center so it overlaps the opposite
        // stroke and the center pixel has no seam.
        s if s == N => fill_rect(
            pixels,
            cell_w,
            cx.saturating_sub(half),
            0,
            cx + t_rem,
            cy + t_rem,
            255,
        ),
        s if s == S => fill_rect(
            pixels,
            cell_w,
            cx.saturating_sub(half),
            cy.saturating_sub(half),
            cx + t_rem,
            cell_h,
            255,
        ),
        s if s == E => fill_rect(
            pixels,
            cell_w,
            cx.saturating_sub(half),
            cy.saturating_sub(half),
            cell_w,
            cy + t_rem,
            255,
        ),
        s if s == W => fill_rect(
            pixels,
            cell_w,
            0,
            cy.saturating_sub(half),
            cx + t_rem,
            cy + t_rem,
            255,
        ),
        _ => unreachable!("side index out of range"),
    }
}

/// Two rails run from the cell edge to the *opposite* inner rail of the
/// center square, the join geometry double-line corners (`╔`, `╝`)
/// require.
fn draw_double_stroke(
    pixels: &mut [u8],
    cell: CellMetrics,
    side: usize,
    strokes: Strokes,
    light: u32,
) {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let cx = cell_w / 2;
    let cy = cell_h / 2;
    let gap = light.max(1);
    let off = gap.div_ceil(2);
    match side {
        s if s == N => {
            // A perpendicular double stroke on the far side means the
            // rails run through; otherwise they stop at the far rail's
            // inner edge so a corner like `╔` closes.
            let stop_left = cy.saturating_sub(off);
            let stop_right = cy + off + light;
            let through = matches!(strokes[S], Some(Weight::Double));
            let (end_left, end_right) = if through {
                (cell_h, cell_h)
            } else {
                (
                    cy + (if strokes[E] == Some(Weight::Double)
                        || strokes[W] == Some(Weight::Double)
                    {
                        off + light
                    } else {
                        0
                    }),
                    stop_left,
                )
            };
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(off + light),
                0,
                cx.saturating_sub(off),
                end_left.max(stop_left),
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                cx + off,
                0,
                cx + off + light,
                end_right.max(stop_right),
                255,
            );
        }
        s if s == S => {
            let through = matches!(strokes[N], Some(Weight::Double));
            let (start_left, start_right) = if through {
                (0, 0)
            } else {
                let outer =
                    if strokes[E] == Some(Weight::Double) || strokes[W] == Some(Weight::Double) {
                        cy.saturating_sub(off + light)
                    } else {
                        cy + off + light
                    };
                (outer, cy + off + light)
            };
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(off + light),
                start_left,
                cx.saturating_sub(off),
                cell_h,
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                cx + off,
                start_right,
                cx + off + light,
                cell_h,
                255,
            );
        }
        s if s == E => {
            let through = matches!(strokes[W], Some(Weight::Double));
            let (start_top, start_bot) = if through {
                (0, 0)
            } else {
                let outer =
                    if strokes[N] == Some(Weight::Double) || strokes[S] == Some(Weight::Double) {
                        cx.saturating_sub(off + light)
                    } else {
                        cx + off + light
                    };
                (outer, cx + off + light)
            };
            fill_rect(
                pixels,
                cell_w,
                start_top,
                cy.saturating_sub(off + light),
                cell_w,
                cy.saturating_sub(off),
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                start_bot,
                cy + off,
                cell_w,
                cy + off + light,
                255,
            );
        }
        s if s == W => {
            let through = matches!(strokes[E], Some(Weight::Double));
            let (end_top, end_bot) = if through {
                (cell_w, cell_w)
            } else {
                let outer =
                    if strokes[N] == Some(Weight::Double) || strokes[S] == Some(Weight::Double) {
                        cx + off + light
                    } else {
                        cx.saturating_sub(off + light)
                    };
                (cx.saturating_sub(off), outer)
            };
            fill_rect(
                pixels,
                cell_w,
                0,
                cy.saturating_sub(off + light),
                end_top,
                cy.saturating_sub(off),
                255,
            );
            fill_rect(pixels, cell_w, 0, cy + off, end_bot, cy + off + light, 255);
        }
        _ => unreachable!("side index out of range"),
    }
}

fn rasterize_strokes(strokes: Strokes, cell: CellMetrics) -> GlyphBitmap {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let mut pixels = vec![0u8; (cell_w * cell_h) as usize];
    let light = light_thickness(cell_h);
    let heavy = heavy_thickness(cell_h);
    for side in [N, E, S, W] {
        match strokes[side] {
            Some(Weight::Light) => draw_solid_stroke(&mut pixels, cell, side, light),
            Some(Weight::Heavy) => draw_solid_stroke(&mut pixels, cell, side, heavy),
            Some(Weight::Double) => {
                draw_double_stroke(&mut pixels, cell, side, strokes, light);
            }
            None => {}
        }
    }
    coverage_bitmap(cell, pixels)
}

fn rasterize_block(block: Block, cell: CellMetrics) -> GlyphBitmap {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let mut pixels = vec![0u8; (cell_w * cell_h) as usize];
    match block {
        Block::Solid(a) => pixels.fill(a),
        Block::LowerEighths(n) => {
            let n = u32::from(n.min(8));
            let fill_h = (cell_h * n).div_ceil(8);
            let y0 = cell_h.saturating_sub(fill_h);
            fill_rect(&mut pixels, cell_w, 0, y0, cell_w, cell_h, 255);
        }
        Block::UpperEighths(n) => {
            let n = u32::from(n.min(8));
            let fill_h = (cell_h * n).div_ceil(8);
            fill_rect(&mut pixels, cell_w, 0, 0, cell_w, fill_h, 255);
        }
        Block::LeftEighths(n) => {
            let n = u32::from(n.min(8));
            let fill_w = (cell_w * n).div_ceil(8);
            fill_rect(&mut pixels, cell_w, 0, 0, fill_w, cell_h, 255);
        }
        Block::RightEighths(n) => {
            let n = u32::from(n.min(8));
            let fill_w = (cell_w * n).div_ceil(8);
            let x0 = cell_w.saturating_sub(fill_w);
            fill_rect(&mut pixels, cell_w, x0, 0, cell_w, cell_h, 255);
        }
        Block::Quadrants(bits) => {
            // One midline per axis so the quadrants partition the cell:
            // `▘` over `▖` tiles with no seam and no double-painted row.
            // The eighth-block fills round each half up independently,
            // right for standalone halves but double-blending a mosaic's
            // interior edges.
            let xm = cell_w.div_ceil(2);
            let ym = cell_h.div_ceil(2);
            let regions = [
                (0, 0, xm, ym),
                (xm, 0, cell_w, ym),
                (0, ym, xm, cell_h),
                (xm, ym, cell_w, cell_h),
            ];
            for (i, &(x0, y0, x1, y1)) in regions.iter().enumerate() {
                if bits & (1 << i) != 0 {
                    fill_rect(&mut pixels, cell_w, x0, y0, x1, y1, 255);
                }
            }
        }
    }
    coverage_bitmap(cell, pixels)
}

/// The 26 octant bit patterns U+1CD00–U+1CDE5 omits because an
/// equivalent glyph exists elsewhere. The block enumerates the remaining
/// 230 in ascending order, so codepoint to pattern is "count upward,
/// skipping these"; verified against wezterm's `OCTANT_PATTERNS` by the
/// round-trip test.
const OCTANT_EXCLUDED: [u8; 26] = [
    0x00, 0x01, 0x02, 0x03, 0x05, 0x0A, 0x0F, 0x14, 0x28, 0x3F, 0x40, 0x50, 0x55, 0x5A, 0x5F, 0x80,
    0xA0, 0xA5, 0xAA, 0xAF, 0xC0, 0xF0, 0xF5, 0xFA, 0xFC, 0xFF,
];

/// `index = cp - 0x1CD00`, `0..=0xE5`.
fn octant_pattern(index: u8) -> u8 {
    let mut remaining = u16::from(index);
    for pattern in 0u16..=255 {
        if OCTANT_EXCLUDED.contains(&(pattern as u8)) {
            continue;
        }
        if remaining == 0 {
            return pattern as u8;
        }
        remaining -= 1;
    }
    // 256 - 26 exclusions = 230 patterns; the caller's range gate caps
    // `index` at 229.
    unreachable!("octant index out of range")
}

/// Bit `n` of `bits` raises grid cell `n` in row-major order, matching
/// the Unicode SEXTANT-/OCTANT- naming. Cell boundaries partition the
/// glyph cell for the same no-seam reason as `Block::Quadrants`.
fn rasterize_mosaic(bits: u8, cols: u32, rows: u32, cell: CellMetrics) -> GlyphBitmap {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let mut pixels = vec![0u8; (cell_w * cell_h) as usize];
    let bx = |k: u32| (cell_w * k).div_ceil(cols);
    let by = |k: u32| (cell_h * k).div_ceil(rows);
    for row in 0..rows {
        for col in 0..cols {
            let bit = row * cols + col;
            if bits & (1 << bit) != 0 {
                fill_rect(
                    &mut pixels,
                    cell_w,
                    bx(col),
                    by(row),
                    bx(col + 1),
                    by(row + 1),
                    255,
                );
            }
        }
    }
    coverage_bitmap(cell, pixels)
}

/// Geometry follows Ghostty's `powerline.zig` (which the Nerd Font
/// originals match).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Powerline {
    /// E0B0 / E0B2.
    Triangle { point_east: bool },
    /// E0B1 / E0B3.
    Chevron { point_east: bool },
    /// E0B4 / E0B5 / E0B6 / E0B7.
    Cap { bulge_east: bool, outline: bool },
    /// E0B8 / E0BA / E0BC / E0BE.
    CornerTriangle { upper: bool, west: bool },
    /// E0B9 / E0BF (falling), E0BB / E0BD (rising).
    Diagonal { falling: bool },
}

const fn powerline_spec(c: char) -> Powerline {
    match c {
        '\u{E0B0}' => Powerline::Triangle { point_east: true },
        '\u{E0B1}' => Powerline::Chevron { point_east: true },
        '\u{E0B2}' => Powerline::Triangle { point_east: false },
        '\u{E0B3}' => Powerline::Chevron { point_east: false },
        '\u{E0B4}' => Powerline::Cap {
            bulge_east: true,
            outline: false,
        },
        '\u{E0B5}' => Powerline::Cap {
            bulge_east: true,
            outline: true,
        },
        '\u{E0B6}' => Powerline::Cap {
            bulge_east: false,
            outline: false,
        },
        '\u{E0B7}' => Powerline::Cap {
            bulge_east: false,
            outline: true,
        },
        '\u{E0B8}' => Powerline::CornerTriangle {
            upper: false,
            west: true,
        },
        '\u{E0BA}' => Powerline::CornerTriangle {
            upper: false,
            west: false,
        },
        '\u{E0BC}' => Powerline::CornerTriangle {
            upper: true,
            west: true,
        },
        '\u{E0BE}' => Powerline::CornerTriangle {
            upper: true,
            west: false,
        },
        '\u{E0B9}' | '\u{E0BF}' => Powerline::Diagonal { falling: true },
        // E0BB / E0BD are both the rising diagonal; the Nerd Font set has
        // the same duplication.
        _ => Powerline::Diagonal { falling: false },
    }
}

fn segment_distance(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len_sq = dx.mul_add(dx, dy * dy);
    let t = if len_sq <= f32::EPSILON {
        0.0
    } else {
        ((px - ax).mul_add(dx, (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let (ex, ey) = (px - dx.mul_add(t, ax), py - dy.mul_add(t, ay));
    ex.hypot(ey)
}

impl Powerline {
    /// Continuous cell coordinates (`0..width`, `0..height`).
    fn contains(self, x: f32, y: f32, width: f32, height: f32, stroke: f32) -> bool {
        let cy = height / 2.0;
        match self {
            Self::Triangle { point_east } => {
                let reach = if point_east {
                    x / width
                } else {
                    (width - x) / width
                };
                reach + (y - cy).abs() / cy <= 1.0
            }
            Self::Chevron { point_east } => {
                let (base, apex) = if point_east {
                    (0.0, width)
                } else {
                    (width, 0.0)
                };
                segment_distance(x, y, base, 0.0, apex, cy) <= stroke / 2.0
                    || segment_distance(x, y, base, height, apex, cy) <= stroke / 2.0
            }
            Self::Cap {
                bulge_east,
                outline,
            } => {
                let x = if bulge_east { x } else { width - x };
                let radius = width.min(cy);
                let boundary = if y < radius {
                    x.hypot(y - radius) - radius
                } else if y > height - radius {
                    let dy = y - (height - radius);
                    x.hypot(dy) - radius
                } else {
                    x - radius
                };
                if outline {
                    boundary.abs() <= stroke / 2.0
                } else {
                    boundary <= 0.0
                }
            }
            Self::CornerTriangle { upper, west } => {
                if upper == west {
                    let above = x / width + y / height <= 1.0;
                    above == upper
                } else {
                    let above = y / height <= x / width;
                    above == upper
                }
            }
            Self::Diagonal { falling } => {
                let (y0, y1) = if falling {
                    (0.0, height)
                } else {
                    (height, 0.0)
                };
                segment_distance(x, y, 0.0, y0, width, y1) <= stroke / 2.0
            }
        }
    }
}

/// 3×3 coverage samples per pixel. The axis-aligned rasterizers above
/// stay hard-edged because their edges must butt against neighboring
/// cells pixel-exactly; these glyphs terminate inside a color
/// transition, so soft edges are correct.
fn rasterize_powerline(glyph: char, cell: CellMetrics) -> GlyphBitmap {
    const SS: u32 = 3;
    let (cell_w, cell_h) = (cell.width, cell.height);
    let shape = powerline_spec(glyph);
    let (width, height) = (cell_w as f32, cell_h as f32);
    let stroke = light_thickness(cell_h) as f32;
    let mut pixels = vec![0u8; (cell_w * cell_h) as usize];
    for py in 0..cell_h {
        for px in 0..cell_w {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if shape.contains(x, y, width, height, stroke) {
                        hits += 1;
                    }
                }
            }
            pixels[(py * cell_w + px) as usize] = ((hits * 255) / (SS * SS)) as u8;
        }
    }
    coverage_bitmap(cell, pixels)
}

/// A port of kitty's `distribute_dots` (decorations.c) so braille cells
/// pixel-match kitty: each dot gets `max(1, available / 2N)` pixels and
/// an equal gap before it, leftover pixels top up the gaps round-robin,
/// and the first gap is halved to center the block. Dot `i` starts at
/// `offsets[i] + i * dot_size`.
fn distribute_dots<const N: usize>(available: u32) -> (u32, [u32; N]) {
    let n = N as u32;
    let dot_size = (available / (2 * n)).max(1);
    let mut gaps = [dot_size; N];
    let mut extra = available.saturating_sub(2 * n * dot_size);
    let mut idx = 0;
    while extra > 0 {
        gaps[idx] += 1;
        idx = (idx + 1) % N;
        extra -= 1;
    }
    gaps[0] /= 2;
    let mut offsets = [0u32; N];
    let mut acc = 0;
    for (offset, gap) in offsets.iter_mut().zip(gaps) {
        acc += gap;
        *offset = acc;
    }
    (dot_size, offsets)
}

/// `(column, row)` for dot bit `n`. Unicode numbers the dots
/// column-major for the original 6-dot cell and appends 7/8 as a bottom
/// row, hence the non-monotonic tail.
const BRAILLE_DOT_GRID: [(u32, u32); 8] = [
    (0, 0), // dot 1
    (0, 1), // dot 2
    (0, 2), // dot 3
    (1, 0), // dot 4
    (1, 1), // dot 5
    (1, 2), // dot 6
    (0, 3), // dot 7
    (1, 3), // dot 8
];

fn rasterize_braille(which: u8, cell: CellMetrics) -> GlyphBitmap {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let mut pixels = vec![0u8; (cell_w * cell_h) as usize];
    let (dot_w, x_offsets) = distribute_dots::<2>(cell_w);
    let (dot_h, y_offsets) = distribute_dots::<4>(cell_h);
    for (bit, &(col, row)) in BRAILLE_DOT_GRID.iter().enumerate() {
        if which & (1 << bit) == 0 {
            continue;
        }
        let x0 = x_offsets[col as usize] + col * dot_w;
        let y0 = y_offsets[row as usize] + row * dot_h;
        fill_rect(&mut pixels, cell_w, x0, y0, x0 + dot_w, y0 + dot_h, 255);
    }
    // U+2800 yields an all-zero bitmap, which the atlas stores as a
    // negative (no-quad) entry.
    coverage_bitmap(cell, pixels)
}

fn rasterize_arc(corner: ArcCorner, cell: CellMetrics) -> GlyphBitmap {
    let mut pixels = vec![0u8; (cell.width * cell.height) as usize];
    draw_arc(&mut pixels, cell, corner, light_thickness(cell.height));
    coverage_bitmap(cell, pixels)
}

/// For `╭` the circle center sits at `(cx + r, cy + r)`, tangent to
/// `x = cx` and `y = cy`, so the arc bulges toward the cell center
/// rather than away from it; the other corners mirror this. Axial
/// stubs continue from each endpoint to the cell boundary so the
/// neighboring `─` / `│` cell sees no seam.
fn draw_arc(pixels: &mut [u8], cell: CellMetrics, corner: ArcCorner, t: u32) {
    let (cell_w, cell_h) = (cell.width, cell.height);
    let cx = cell_w / 2;
    let cy = cell_h / 2;
    let half = t / 2;
    let t_rem = t - half;
    // 3 px for 8×16 cells: small enough that the stubs stay visible,
    // large enough to read as rounded rather than chamfered.
    let r = cx
        .min(cy)
        .min(cell_w.saturating_sub(1).saturating_sub(cx))
        .min(cell_h.saturating_sub(1).saturating_sub(cy));
    if r == 0 {
        // Too small for an arc: the matching sharp corner keeps the
        // glyph joined to its neighbors instead of vanishing.
        let strokes: Strokes = match corner {
            ArcCorner::DownRight => [X, L, L, X],
            ArcCorner::DownLeft => [X, X, L, L],
            ArcCorner::UpLeft => [L, X, X, L],
            ArcCorner::UpRight => [L, L, X, X],
        };
        for side in [N, E, S, W] {
            if strokes[side].is_some() {
                draw_solid_stroke(pixels, cell, side, t);
            }
        }
        return;
    }
    let r_inner = r.saturating_sub(half);
    let r_inner_sq = r_inner * r_inner;
    let r_outer = r + t_rem;
    let r_outer_sq = r_outer * r_outer;
    let ((center_x, center_y), (x_start, x_end, y_start, y_end)) = match corner {
        ArcCorner::DownRight => (
            (cx + r, cy + r),
            (cx, (cx + r + 1).min(cell_w), cy, (cy + r + 1).min(cell_h)),
        ),
        ArcCorner::DownLeft => (
            (cx.saturating_sub(r), cy + r),
            (cx.saturating_sub(r), cx + 1, cy, (cy + r + 1).min(cell_h)),
        ),
        ArcCorner::UpLeft => (
            (cx.saturating_sub(r), cy.saturating_sub(r)),
            (cx.saturating_sub(r), cx + 1, cy.saturating_sub(r), cy + 1),
        ),
        ArcCorner::UpRight => (
            (cx + r, cy.saturating_sub(r)),
            (cx, (cx + r + 1).min(cell_w), cy.saturating_sub(r), cy + 1),
        ),
    };
    for y in y_start..y_end {
        for x in x_start..x_end {
            let dx = x.abs_diff(center_x);
            let dy = y.abs_diff(center_y);
            let dist_sq = dx * dx + dy * dy;
            if dist_sq >= r_inner_sq && dist_sq < r_outer_sq {
                let idx = (y * cell_w + x) as usize;
                if idx < pixels.len() {
                    pixels[idx] = 255;
                }
            }
        }
    }
    // Without the stubs a `t > 1` stroke leaves a sliver of background
    // between the arc and the cell boundary.
    match corner {
        ArcCorner::DownRight => {
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(half),
                cy + r,
                cx + t_rem,
                cell_h,
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                cx + r,
                cy.saturating_sub(half),
                cell_w,
                cy + t_rem,
                255,
            );
        }
        ArcCorner::DownLeft => {
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(half),
                cy + r,
                cx + t_rem,
                cell_h,
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                0,
                cy.saturating_sub(half),
                cx.saturating_sub(r) + 1,
                cy + t_rem,
                255,
            );
        }
        ArcCorner::UpLeft => {
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(half),
                0,
                cx + t_rem,
                cy.saturating_sub(r) + 1,
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                0,
                cy.saturating_sub(half),
                cx.saturating_sub(r) + 1,
                cy + t_rem,
                255,
            );
        }
        ArcCorner::UpRight => {
            fill_rect(
                pixels,
                cell_w,
                cx.saturating_sub(half),
                0,
                cx + t_rem,
                cy.saturating_sub(r) + 1,
                255,
            );
            fill_rect(
                pixels,
                cell_w,
                cx + r,
                cy.saturating_sub(half),
                cell_w,
                cy + t_rem,
                255,
            );
        }
    }
}

/// 1 px at 16 px cells, 2 px at 36 px: the "1 px until the font
/// visibly demands more" feel kitty / wezterm settle on.
fn light_thickness(cell_h: u32) -> u32 {
    (cell_h / 18).max(1)
}

/// About 3× light, so heavy reads as clearly bolder at small sizes
/// without ballooning at large ones.
fn heavy_thickness(cell_h: u32) -> u32 {
    (light_thickness(cell_h) * 3).max(2)
}

fn coverage_bitmap(cell: CellMetrics, pixels: Vec<u8>) -> GlyphBitmap {
    GlyphBitmap::new(
        cell.width,
        cell.height,
        0,
        i32::try_from(cell.ascent).unwrap_or(i32::MAX),
        GlyphPixels::Coverage(pixels),
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const fn cell(width: u32, height: u32, ascent: u32) -> CellMetrics {
        CellMetrics {
            width,
            height,
            ascent,
        }
    }

    fn px(bitmap: &GlyphBitmap, x: u32, y: u32) -> u8 {
        bitmap.pixels().as_bytes()[(y * bitmap.width() + x) as usize]
    }

    #[test]
    fn vertical_light_fills_the_full_cell_height() {
        // The synthetic glyph must reach y=0 and y=cell_h-1: that is
        // what closes the row-to-row gap.
        let bitmap = rasterize('│', cell(8, 16, 12)).expect("vertical light supported");
        assert_eq!(bitmap.width(), 8);
        assert_eq!(bitmap.height(), 16);
        assert_eq!(bitmap.left, 0);
        assert_eq!(
            bitmap.top, 12,
            "top must equal ascent for top-align placement"
        );
        let cx = bitmap.width() / 2;
        assert_ne!(px(&bitmap, cx, 0), 0, "top edge of │ must be opaque");
        assert_ne!(
            px(&bitmap, cx, bitmap.height() - 1),
            0,
            "bottom edge of │ must be opaque"
        );
    }

    #[test]
    fn horizontal_light_fills_the_full_cell_width() {
        let bitmap = rasterize('─', cell(8, 16, 12)).expect("horizontal light supported");
        let cy = bitmap.height() / 2;
        assert_ne!(px(&bitmap, 0, cy), 0, "left edge of ─ must be opaque");
        assert_ne!(
            px(&bitmap, bitmap.width() - 1, cy),
            0,
            "right edge of ─ must be opaque"
        );
    }

    #[test]
    fn down_right_corner_extends_only_down_and_right() {
        let bitmap = rasterize('┌', cell(8, 16, 12)).expect("┌ supported");
        let cx = bitmap.width() / 2;
        let cy = bitmap.height() / 2;
        assert_eq!(px(&bitmap, cx, 0), 0, "no stroke reaches the top");
        assert_eq!(px(&bitmap, 0, cy), 0, "no stroke reaches the left");
        assert_ne!(
            px(&bitmap, cx, bitmap.height() - 1),
            0,
            "S stroke must reach the bottom"
        );
        assert_ne!(
            px(&bitmap, bitmap.width() - 1, cy),
            0,
            "E stroke must reach the right"
        );
    }

    #[test]
    fn cross_reaches_every_edge() {
        let bitmap = rasterize('┼', cell(8, 16, 12)).expect("┼ supported");
        let cx = bitmap.width() / 2;
        let cy = bitmap.height() / 2;
        assert_ne!(px(&bitmap, cx, 0), 0);
        assert_ne!(px(&bitmap, cx, bitmap.height() - 1), 0);
        assert_ne!(px(&bitmap, 0, cy), 0);
        assert_ne!(px(&bitmap, bitmap.width() - 1, cy), 0);
    }

    #[test]
    fn heavy_is_thicker_than_light() {
        assert!(heavy_thickness(16) > light_thickness(16));
        assert!(heavy_thickness(32) > light_thickness(32));
        assert!(heavy_thickness(48) > light_thickness(48));
    }

    #[test]
    fn full_block_fills_the_cell_at_full_alpha() {
        let bitmap = rasterize('█', cell(8, 16, 12)).expect("█ supported");
        assert!(bitmap.pixels().as_bytes().iter().all(|&a| a == 255));
    }

    /// Shade alpha is calibrated so the rendered color sits near the
    /// Unicode-named fraction once the fg color multiplies through.
    #[test]
    fn shade_glyphs_carry_calibrated_alpha() {
        let light = rasterize('░', cell(8, 16, 12)).expect("░ supported");
        let medium = rasterize('▒', cell(8, 16, 12)).expect("▒ supported");
        let dark = rasterize('▓', cell(8, 16, 12)).expect("▓ supported");
        let l = light.pixels().as_bytes()[0];
        let m = medium.pixels().as_bytes()[0];
        let d = dark.pixels().as_bytes()[0];
        assert!(l < m && m < d, "alpha must rise with shade darkness");
        assert!(
            d < 255,
            "dark shade is still strictly less than a full block"
        );
        assert!(l > 0, "light shade must be visible");
    }

    #[test]
    fn upper_half_block_fills_top_only() {
        let bitmap = rasterize('▀', cell(8, 16, 12)).expect("▀ supported");
        assert_ne!(px(&bitmap, 4, 0), 0, "top pixel must be filled");
        assert_eq!(
            px(&bitmap, 4, bitmap.height() - 1),
            0,
            "bottom must be empty"
        );
    }

    /// The boundary rows of a stacked `▀` / `▄` pair meet at the cell
    /// midline without overlap or gap.
    #[test]
    fn lower_half_block_fills_bottom_only() {
        let bitmap = rasterize('▄', cell(8, 16, 12)).expect("▄ supported");
        assert_eq!(px(&bitmap, 4, 0), 0, "top must be empty");
        assert_ne!(
            px(&bitmap, 4, bitmap.height() - 1),
            0,
            "bottom pixel must be filled"
        );
    }

    #[test]
    fn eighth_blocks_subdivide_the_cell() {
        let one = rasterize('\u{2581}', cell(16, 16, 12)).expect("▁ supported");
        let seven = rasterize('\u{2587}', cell(16, 16, 12)).expect("▇ supported");
        let one_filled: u32 = one
            .pixels()
            .as_bytes()
            .iter()
            .map(|&p| u32::from(p > 0))
            .sum();
        let seven_filled: u32 = seven
            .pixels()
            .as_bytes()
            .iter()
            .map(|&p| u32::from(p > 0))
            .sum();
        assert_eq!(one_filled, 16 * 2, "▁ fills 2 rows × 16 cols");
        assert_eq!(seven_filled, 16 * 14, "▇ fills 14 rows × 16 cols");
    }

    #[test]
    fn unsupported_code_points_return_none() {
        assert!(rasterize('A', cell(8, 16, 12)).is_none());
        assert!(rasterize('あ', cell(8, 16, 12)).is_none());
        assert!(
            rasterize('\u{2600}', cell(8, 16, 12)).is_none(),
            "miscellaneous symbols out of scope"
        );
        // Diagonals stay deferred; growing support must update this.
        assert!(rasterize('\u{2571}', cell(8, 16, 12)).is_none());
        assert!(rasterize('\u{2573}', cell(8, 16, 12)).is_none());
    }

    #[test]
    fn zero_cell_dimensions_return_none() {
        assert!(rasterize('│', cell(0, 16, 12)).is_none());
        assert!(rasterize('│', cell(8, 0, 12)).is_none());
        assert!(rasterize('⣿', cell(0, 16, 12)).is_none());
    }

    /// The diagonal pair is the discriminator for the bit mapping.
    #[test]
    fn quadrant_diagonal_fills_upper_left_and_lower_right() {
        let bitmap = rasterize('▚', cell(8, 16, 12)).expect("▚ supported");
        assert_ne!(px(&bitmap, 1, 1), 0, "upper-left quadrant filled");
        assert_ne!(px(&bitmap, 6, 14), 0, "lower-right quadrant filled");
        assert_eq!(px(&bitmap, 6, 1), 0, "upper-right quadrant empty");
        assert_eq!(px(&bitmap, 1, 14), 0, "lower-left quadrant empty");
    }

    /// Every cell a mosaic glyph can raise is a tile of one partition of
    /// the cell: the sub-rectangles must leave no gap (a seam of
    /// background between stacked glyphs) and no overlap (a double-blended
    /// seam), at any cell size, for quadrants, sextants and octants alike.
    fn covered(bitmap: &GlyphBitmap, coverage: &mut [u32]) {
        for (slot, &p) in coverage.iter_mut().zip(bitmap.pixels().as_bytes()) {
            *slot += u32::from(p > 0);
        }
    }

    proptest! {
        #[test]
        fn mosaic_cells_partition_the_cell(w in 1u32..24, h in 1u32..24, rows in 2u32..=4) {
            let mut coverage = vec![0u32; (w * h) as usize];
            for bit in 0..(2 * rows) {
                covered(&rasterize_mosaic(1 << bit, 2, rows, cell(w, h, 12)), &mut coverage);
            }
            prop_assert!(coverage.iter().all(|&n| n == 1), "coverage {:?}", coverage);
        }

        #[test]
        fn quadrant_glyphs_partition_the_cell(w in 1u32..24, h in 1u32..24) {
            let mut coverage = vec![0u32; (w * h) as usize];
            for g in ['\u{2598}', '\u{259D}', '\u{2596}', '\u{2597}'] {
                covered(&rasterize(g, cell(w, h, 12)).expect("quadrant supported"), &mut coverage);
            }
            prop_assert!(coverage.iter().all(|&n| n == 1), "coverage {:?}", coverage);
        }
    }

    /// Pins the glyphs just before and after each omitted pattern; an
    /// off-by-one shifts every later sextant by one pseudo-pixel.
    #[test]
    fn sextant_mapping_skips_the_omitted_patterns() {
        // (char, expected bits), bit n = cell n+1 row-major.
        let cases: &[(char, u8)] = &[
            ('\u{1FB00}', 0b00_0001), // SEXTANT-1
            ('\u{1FB13}', 0b01_0100), // SEXTANT-35 (last before ▌ skip)
            ('\u{1FB14}', 0b01_0110), // SEXTANT-235 (first after)
            ('\u{1FB27}', 0b10_1001), // SEXTANT-146 (last before ▐ skip)
            ('\u{1FB28}', 0b10_1011), // SEXTANT-1246 (first after)
            ('\u{1FB3B}', 0b11_1110), // SEXTANT-23456 (last; █ skipped)
        ];
        for &(ch, bits) in cases {
            let bitmap = rasterize(ch, cell(8, 15, 12)).expect("sextant supported");
            for (bit, (x, y)) in [(2, 2), (6, 2), (2, 7), (6, 7), (2, 12), (6, 12)]
                .into_iter()
                .enumerate()
            {
                let want = bits & (1 << bit) != 0;
                assert_eq!(
                    px(&bitmap, x, y) > 0,
                    want,
                    "{ch}: cell {bit} at ({x},{y}) mismatch",
                );
            }
        }
    }

    /// Pinned against the values wezterm's explicit `OCTANT_PATTERNS`
    /// table carries.
    #[test]
    fn octant_mapping_matches_the_reference_table() {
        assert_eq!(octant_pattern(0x00), 0b0000_0100, "1CD00 = OCTANT-3");
        assert_eq!(octant_pattern(0x20), 0b0010_1001, "1CD20 = OCTANT-146");
        assert_eq!(octant_pattern(0xE5), 0b1111_1110, "1CDE5 = OCTANT-2345678");
        let upper_left = rasterize('\u{1CEA8}', cell(8, 16, 12)).expect("𜺨 supported");
        assert_ne!(px(&upper_left, 1, 1), 0);
        assert_eq!(px(&upper_left, 6, 14), 0);
        let lower_right = rasterize('\u{1CEA0}', cell(8, 16, 12)).expect("𜺠 supported");
        assert_ne!(px(&lower_right, 6, 14), 0);
        assert_eq!(px(&lower_right, 1, 1), 0);
    }

    /// The triangle anchors to the full west edge with its apex at the
    /// east midline, so adjacent prompt segments meet without a gap.
    #[test]
    fn powerline_right_triangle_spans_west_edge_to_east_apex() {
        let bitmap = rasterize('\u{E0B0}', cell(8, 16, 12)).expect("E0B0 supported");
        assert_ne!(px(&bitmap, 0, 1), 0, "west edge near top");
        assert_ne!(px(&bitmap, 0, 8), 0, "west edge midline");
        assert_ne!(px(&bitmap, 0, 14), 0, "west edge near bottom");
        assert_ne!(px(&bitmap, 7, 8), 0, "apex at east midline");
        assert_eq!(px(&bitmap, 7, 0), 0, "north-east corner empty");
        assert_eq!(px(&bitmap, 7, 15), 0, "south-east corner empty");
    }

    #[test]
    fn powerline_left_triangle_mirrors_the_right_one() {
        let bitmap = rasterize('\u{E0B2}', cell(8, 16, 12)).expect("E0B2 supported");
        assert_ne!(px(&bitmap, 7, 8), 0, "east edge midline");
        assert_ne!(px(&bitmap, 1, 8), 0, "apex at west midline");
        assert_eq!(px(&bitmap, 0, 0), 0, "north-west corner empty");
    }

    #[test]
    fn powerline_chevron_is_stroke_not_fill() {
        let bitmap = rasterize('\u{E0B1}', cell(8, 16, 12)).expect("E0B1 supported");
        assert_ne!(px(&bitmap, 4, 4), 0, "on the upper stroke");
        assert_ne!(px(&bitmap, 4, 11), 0, "on the lower stroke");
        assert_eq!(px(&bitmap, 0, 8), 0, "interior west midline empty");
    }

    /// The corners outside the quarter-circle arcs stay empty.
    #[test]
    fn powerline_cap_rounds_the_east_corners() {
        let bitmap = rasterize('\u{E0B4}', cell(8, 16, 12)).expect("E0B4 supported");
        assert_ne!(px(&bitmap, 0, 8), 0, "west edge midline");
        assert_ne!(px(&bitmap, 7, 8), 0, "east bulge midline");
        assert_eq!(px(&bitmap, 7, 0), 0, "north-east corner outside arc");
        assert_eq!(px(&bitmap, 7, 15), 0, "south-east corner outside arc");
    }

    #[test]
    fn powerline_corner_triangles_cover_their_named_corner() {
        // (glyph, opaque corner x/y, empty corner x/y)
        let cases: &[(char, u32, u32, u32, u32)] = &[
            ('\u{E0B8}', 0, 15, 7, 0), // lower-left
            ('\u{E0BA}', 7, 15, 0, 0), // lower-right
            ('\u{E0BC}', 0, 0, 7, 15), // upper-left
            ('\u{E0BE}', 7, 0, 0, 15), // upper-right
        ];
        for &(ch, ox, oy, ex, ey) in cases {
            let bitmap = rasterize(ch, cell(8, 16, 12)).expect("corner triangle supported");
            assert_ne!(px(&bitmap, ox, oy), 0, "{ch}: corner ({ox},{oy}) filled");
            assert_eq!(px(&bitmap, ex, ey), 0, "{ch}: corner ({ex},{ey}) empty");
        }
    }

    #[test]
    fn powerline_diagonals_trace_the_corner_line() {
        let falling = rasterize('\u{E0B9}', cell(8, 16, 12)).expect("E0B9 supported");
        assert_ne!(px(&falling, 4, 8), 0, "center is on the diagonal");
        assert_eq!(px(&falling, 7, 0), 0, "off-diagonal corner empty");
        let rising = rasterize('\u{E0BB}', cell(8, 16, 12)).expect("E0BB supported");
        assert_ne!(px(&rising, 4, 8), 0, "center is on the diagonal");
        assert_eq!(px(&rising, 0, 0), 0, "off-diagonal corner empty");
    }

    /// Dots 1–3 fill the left column top-down, 4–6 the right column,
    /// 7/8 the bottom row; braille canvases depend on this mapping.
    #[test]
    fn braille_single_dots_land_in_their_grid_position() {
        let dots: &[(char, u32, u32)] = &[
            ('\u{2801}', 1, 1),  // dot 1: top-left
            ('\u{2802}', 1, 5),  // dot 2: mid-upper-left
            ('\u{2804}', 1, 9),  // dot 3: mid-lower-left
            ('\u{2808}', 5, 1),  // dot 4: top-right
            ('\u{2810}', 5, 5),  // dot 5: mid-upper-right
            ('\u{2820}', 5, 9),  // dot 6: mid-lower-right
            ('\u{2840}', 1, 13), // dot 7: bottom-left
            ('\u{2880}', 5, 13), // dot 8: bottom-right
        ];
        for &(ch, x, y) in dots {
            let bitmap = rasterize(ch, cell(8, 16, 12)).expect("braille supported");
            assert_ne!(px(&bitmap, x, y), 0, "{ch}: dot must cover ({x},{y})");
            let filled: u32 = bitmap
                .pixels()
                .as_bytes()
                .iter()
                .map(|&p| u32::from(p > 0))
                .sum();
            assert_eq!(filled, 2 * 2, "{ch}: exactly one 2×2 dot");
        }
    }

    /// The inter-dot gaps stay transparent, or a braille canvas
    /// degrades into a block blob.
    #[test]
    fn braille_full_pattern_keeps_inter_dot_gaps() {
        let bitmap = rasterize('\u{28FF}', cell(8, 16, 12)).expect("⣿ supported");
        for &x in &[1, 5] {
            for &y in &[1, 5, 9, 13] {
                assert_ne!(px(&bitmap, x, y), 0, "dot at ({x},{y}) must be opaque");
            }
        }
        assert_eq!(px(&bitmap, 4, 1), 0, "column gap must stay empty");
        assert_eq!(px(&bitmap, 1, 4), 0, "row gap must stay empty");
        assert_eq!(px(&bitmap, 0, 0), 0, "leading margin must stay empty");
    }

    #[test]
    fn braille_blank_pattern_is_all_zero() {
        let bitmap = rasterize('\u{2800}', cell(8, 16, 12)).expect("⠀ supported");
        assert!(bitmap.pixels().as_bytes().iter().all(|&p| p == 0));
        assert_eq!(bitmap.width(), 8, "blank still claims the cell box");
    }

    #[test]
    fn braille_survives_tiny_cells() {
        let bitmap = rasterize('\u{2801}', cell(1, 1, 1)).expect("⠁ supported at any size");
        assert_eq!(bitmap.pixels().as_bytes().len(), 1);
        assert_ne!(bitmap.pixels().as_bytes()[0], 0);
    }

    /// Both rails reach the cell edges, so `═══` renders as two
    /// continuous lines rather than pairs of dashes.
    #[test]
    fn double_horizontal_paints_two_full_width_rails() {
        let bitmap = rasterize('═', cell(16, 16, 12)).expect("═ supported");
        let cy = bitmap.height() / 2;
        let light = light_thickness(16);
        let gap = light.max(1);
        let off = gap.div_ceil(2);
        let top_rail_y = cy - off - 1;
        let bot_rail_y = cy + off;
        for &y in &[top_rail_y, bot_rail_y] {
            assert_ne!(px(&bitmap, 0, y), 0, "rail at y={y} must reach left edge");
            assert_ne!(
                px(&bitmap, bitmap.width() - 1, y),
                0,
                "rail at y={y} must reach right edge",
            );
        }
        assert_eq!(px(&bitmap, bitmap.width() / 2, cy), 0, "gap between rails");
    }

    /// Each arc reaches the same cell-edge pixels as its square
    /// counterpart (the lazygit-corner regression) and not the other two.
    #[test]
    fn arc_corners_meet_cell_edges_on_their_named_sides() {
        let cases: &[(char, [bool; 4])] = &[
            // [N, E, S, W]: true means the arc must reach that edge.
            ('╭', [false, true, true, false]),
            ('╮', [false, false, true, true]),
            ('╯', [true, false, false, true]),
            ('╰', [true, true, false, false]),
        ];
        for &(ch, [n, e, s, w]) in cases {
            let bitmap =
                rasterize(ch, cell(16, 16, 12)).unwrap_or_else(|| panic!("{ch} supported"));
            let cx = bitmap.width() / 2;
            let cy = bitmap.height() / 2;
            let check = |x, y, expect_on, side| {
                if expect_on {
                    assert_ne!(
                        px(&bitmap, x, y),
                        0,
                        "{ch}: {side} edge at ({x},{y}) must be opaque",
                    );
                } else {
                    assert_eq!(
                        px(&bitmap, x, y),
                        0,
                        "{ch}: {side} edge at ({x},{y}) must stay empty",
                    );
                }
            };
            check(cx, 0, n, "N");
            check(bitmap.width() - 1, cy, e, "E");
            check(cx, bitmap.height() - 1, s, "S");
            check(0, cy, w, "W");
        }
    }

    /// The synthetic arc's `size_px == cell`, so `╭───╮` over `│` reads
    /// as one continuous outline.
    #[test]
    fn arc_bitmap_fills_the_cell_so_stacked_arcs_have_no_seam() {
        let bitmap = rasterize('╭', cell(16, 16, 12)).expect("╭ supported");
        assert_eq!(bitmap.width(), 16);
        assert_eq!(bitmap.height(), 16);
        // The top-left quadrant stays empty so an adjacent `╮` does not
        // collide with the arc's body.
        for y in 0..bitmap.height() / 2 {
            for x in 0..bitmap.width() / 2 {
                assert_eq!(
                    px(&bitmap, x, y),
                    0,
                    "╭: top-left quadrant must be empty at ({x},{y})",
                );
            }
        }
    }

    /// Arcs bulge toward the cell center: a circle centered on the cell
    /// mid-axes puts the arc on the wrong side of the tangent and the
    /// corner sticks out. At 16×16 with `r = 7` the inward arc's
    /// midpoint is near 10 on both axes; the inverted geometry would
    /// put it near 14.
    #[test]
    fn arc_curves_inward_so_corner_looks_rounded_not_inverted() {
        let bitmap = rasterize('╭', cell(16, 16, 12)).expect("╭ supported");
        assert_ne!(
            px(&bitmap, 10, 10),
            0,
            "╭ arc midpoint must land near the cell center (10, 10)",
        );
        assert_eq!(
            px(&bitmap, 14, 14),
            0,
            "╭ must not bulge toward the cell's bottom-right (14, 14)",
        );
        let bottom_left = rasterize('╰', cell(16, 16, 12)).expect("╰ supported");
        assert_ne!(
            px(&bottom_left, 10, 6),
            0,
            "╰ arc midpoint must mirror `╭` about cy",
        );
        assert_eq!(
            px(&bottom_left, 14, 2),
            0,
            "╰ must not bulge toward the cell's top-right",
        );
    }

    /// A cell whose radius collapses to 0 falls back to a sharp corner
    /// rather than an empty bitmap.
    #[test]
    fn arc_falls_back_to_sharp_corner_when_cell_is_too_small_for_a_radius() {
        let bitmap = rasterize('╭', cell(2, 2, 2)).expect("╭ supported at any size");
        let cx = bitmap.width() / 2;
        let cy = bitmap.height() / 2;
        assert_ne!(
            px(&bitmap, bitmap.width() - 1, cy),
            0,
            "fallback corner must still reach the E edge"
        );
        assert_ne!(
            px(&bitmap, cx, bitmap.height() - 1),
            0,
            "fallback corner must still reach the S edge"
        );
    }
}
