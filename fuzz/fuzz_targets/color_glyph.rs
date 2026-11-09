//! Color glyph fuzz target: `COLR` layers, `sbix` and `CBDT` strikes, and
//! the outline they fall through to.
//!
//! Input convention: a little-endian f32 size in pixels, a little-endian
//! u16 glyph id, then the font file. Win condition: no panic, and every
//! bitmap stays inside the rasterizer's 16 MiB working set.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((px, rest)) = data.split_first_chunk::<4>() else {
        return;
    };
    let Some((glyph, font)) = rest.split_first_chunk::<2>() else {
        return;
    };
    let px = f32::from_le_bytes(*px);
    let glyph = u32::from(u16::from_le_bytes(*glyph));
    let Some(bitmap) = felis_shaping::rasterize_face_glyph(font.to_vec(), glyph, px) else {
        return;
    };
    assert!(
        bitmap.pixels().as_bytes().len() <= 16 << 20,
        "{}x{} bitmap",
        bitmap.width(),
        bitmap.height()
    );
});
