//! CoreText draws a macOS face's outlines, dilated by the platform's font
//! smoothing as every native window draws them.

use std::{ffi::c_void, ptr::NonNull, sync::OnceLock};

use objc2_core_foundation::{
    CFBoolean, CFData, CFNumber, CFPreferencesCopyAppValue, CFRetained, CFString,
    CGAffineTransform, CGPoint, CGRect, CGSize, kCFPreferencesCurrentApplication,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGGlyph, CGImageAlphaInfo, CGTextDrawingMode,
};
use objc2_core_text::{CTFont, CTFontDescriptor};

/// The `AppleFontSmoothing` user default as the preferences hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SmoothingPreference {
    Unset,
    Integer(i64),
    Boolean(bool),
    Text(String),
    /// A float or any other property-list type.
    Other,
}

impl SmoothingPreference {
    /// Whether text is smoothed: an integer 0 turns it off; unset behaves
    /// as 2, and a boolean or a non-integer number is ignored. A string
    /// counts as its leading integer, 0 when it has none, as
    /// `-[NSString integerValue]` reads it.
    pub(crate) fn smooths(&self) -> bool {
        match self {
            Self::Integer(level) => *level != 0,
            Self::Text(text) => leading_integer(text) != 0,
            Self::Unset | Self::Boolean(_) | Self::Other => true,
        }
    }

    fn read() -> Self {
        let key = CFString::from_static_str("AppleFontSmoothing");
        // SAFETY: reading CoreFoundation's immutable application-id
        // constant.
        let app = unsafe { kCFPreferencesCurrentApplication };
        let Some(value) = CFPreferencesCopyAppValue(&key, app) else {
            return Self::Unset;
        };
        if let Some(flag) = value.downcast_ref::<CFBoolean>() {
            return Self::Boolean(flag.as_bool());
        }
        if let Some(number) = value.downcast_ref::<CFNumber>() {
            return match number.as_i64() {
                Some(level) if !number.is_float_type() => Self::Integer(level),
                _ => Self::Other,
            };
        }
        if let Some(text) = value.downcast_ref::<CFString>() {
            return Self::Text(text.to_string());
        }
        Self::Other
    }
}

fn leading_integer(text: &str) -> i64 {
    let text = text.trim_start();
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let value = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0_i64, |acc, d| {
            acc.saturating_mul(10).saturating_add(i64::from(d - b'0'))
        });
    if negative { -value } else { value }
}

/// The process's `AppleFontSmoothing`, read once.
pub(crate) fn font_smoothing() -> &'static SmoothingPreference {
    static PREFERENCE: OnceLock<SmoothingPreference> = OnceLock::new();
    PREFERENCE.get_or_init(SmoothingPreference::read)
}

/// The descriptors CoreText reads from `data`, one per face of a
/// collection; `None` when it reads none.
pub(crate) fn descriptors_from_data(
    data: &CFData,
) -> Option<CFRetained<objc2_core_foundation::CFArray>> {
    unsafe extern "C-unwind" {
        fn CTFontManagerCreateFontDescriptorsFromData(
            data: &CFData,
        ) -> Option<NonNull<objc2_core_foundation::CFArray>>;
    }
    // SAFETY: `data` is a live CFData; the call returns a +1 array or
    // NULL, which the binding in objc2-core-text would unwrap.
    let array = unsafe { CTFontManagerCreateFontDescriptorsFromData(data) }?;
    // SAFETY: a Create call's result is owned by the caller.
    Some(unsafe { CFRetained::from_raw(array) })
}

/// `descriptor` at `px`; `None` where CoreText creates no font.
pub(crate) fn font_from_descriptor(
    descriptor: &CTFontDescriptor,
    px: f64,
) -> Option<CFRetained<CTFont>> {
    unsafe extern "C-unwind" {
        fn CTFontCreateWithFontDescriptor(
            descriptor: &CTFontDescriptor,
            size: f64,
            matrix: *const CGAffineTransform,
        ) -> Option<NonNull<CTFont>>;
    }
    // SAFETY: a null matrix asks for the identity transform; the result is
    // a +1 font or NULL.
    let font = unsafe { CTFontCreateWithFontDescriptor(descriptor, px, std::ptr::null()) }?;
    // SAFETY: a Create call's result is owned by the caller.
    Some(unsafe { CFRetained::from_raw(font) })
}

/// A canvas's pixel format: one gray byte, or RGBX for the probe that
/// checks smoothing adds no color.
#[derive(Clone, Copy)]
pub(crate) enum Format {
    Gray,
    #[cfg(test)]
    Rgbx,
}

impl Format {
    const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Gray => 1,
            #[cfg(test)]
            Self::Rgbx => 4,
        }
    }
}

/// Draws `glyph` white on black into `canvas`, `width` pixels per row
/// and top row first, with its pen at `origin` in the canvas's y-up
/// space; `false` when CoreGraphics makes no context.
pub(crate) fn draw(
    font: &CTFont,
    glyph: CGGlyph,
    origin: CGPoint,
    width: usize,
    format: Format,
    smooth: bool,
    canvas: &mut [u8],
) -> bool {
    let row = width * format.bytes_per_pixel();
    if row == 0 || !canvas.len().is_multiple_of(row) {
        return false;
    }
    let height = canvas.len() / row;
    let (space, info) = match format {
        Format::Gray => (CGColorSpace::new_device_gray(), CGImageAlphaInfo::None),
        #[cfg(test)]
        Format::Rgbx => (
            CGColorSpace::new_device_rgb(),
            CGImageAlphaInfo::NoneSkipLast,
        ),
    };
    let Some(space) = space else {
        return false;
    };
    // SAFETY: `canvas` holds `height` rows of `row` bytes and outlives
    // the context, which is released at the end of this function, before
    // the caller reads or frees the buffer.
    let context = unsafe {
        CGBitmapContextCreate(
            canvas.as_mut_ptr().cast::<c_void>(),
            width,
            height,
            8,
            row,
            Some(&space),
            info.0,
        )
    };
    let Some(context) = context else {
        return false;
    };
    let c = Some(&*context);
    CGContext::set_gray_fill_color(c, 0.0, 1.0);
    CGContext::fill_rect(
        c,
        CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64)),
    );
    CGContext::set_allows_antialiasing(c, true);
    CGContext::set_should_antialias(c, true);
    CGContext::set_allows_font_smoothing(c, true);
    CGContext::set_should_smooth_fonts(c, smooth);
    CGContext::set_allows_font_subpixel_positioning(c, false);
    CGContext::set_should_subpixel_position_fonts(c, false);
    CGContext::set_allows_font_subpixel_quantization(c, false);
    CGContext::set_should_subpixel_quantize_fonts(c, false);
    CGContext::set_gray_fill_color(c, 1.0, 1.0);
    CGContext::set_text_drawing_mode(c, CGTextDrawingMode::Fill);
    CGContext::set_text_matrix(c, IDENTITY);
    // SAFETY: one glyph and one position, both live for the call.
    unsafe {
        font.draw_glyphs(NonNull::from(&glyph), NonNull::from(&origin), 1, &context);
    }
    true
}

const IDENTITY: CGAffineTransform = CGAffineTransform {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    tx: 0.0,
    ty: 0.0,
};

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CFArray;

    use super::*;
    use crate::Font;

    #[test]
    fn an_integer_zero_alone_turns_smoothing_off() {
        assert!(!SmoothingPreference::Integer(0).smooths());
        for level in 1..=3 {
            assert!(SmoothingPreference::Integer(level).smooths(), "{level}");
        }
        assert!(SmoothingPreference::Unset.smooths());
        assert!(SmoothingPreference::Boolean(false).smooths());
        assert!(SmoothingPreference::Boolean(true).smooths());
        assert!(SmoothingPreference::Other.smooths());
    }

    #[test]
    fn a_string_counts_as_its_leading_integer() {
        assert!(!SmoothingPreference::Text("0".into()).smooths());
        assert!(!SmoothingPreference::Text(" 0 ".into()).smooths());
        assert!(SmoothingPreference::Text("2".into()).smooths());
        assert!(SmoothingPreference::Text("-1".into()).smooths());
        assert!(!SmoothingPreference::Text("off".into()).smooths());
    }

    /// The pinned probe face as CoreText reads it at `px`.
    fn probe_font(font: &Font, px: f64) -> CFRetained<CTFont> {
        let data = CFData::from_bytes(font.bytes.as_ref().as_ref());
        let descriptors = descriptors_from_data(&data).expect("CoreText reads the probe face");
        // SAFETY: CoreText documents the array as holding font descriptors.
        let descriptors =
            unsafe { CFRetained::cast_unchecked::<CFArray<CTFontDescriptor>>(descriptors) };
        let descriptor = descriptors.get(0).expect("the probe face has one face");
        font_from_descriptor(&descriptor, px).expect("CoreText creates the probe font")
    }

    fn render(font: &CTFont, glyph: CGGlyph, format: Format, smooth: bool) -> Vec<u8> {
        const SIDE: usize = 64;
        let mut canvas = vec![0; SIDE * SIDE * format.bytes_per_pixel()];
        let origin = CGPoint::new(16.0, 16.0);
        assert!(draw(font, glyph, origin, SIDE, format, smooth, &mut canvas));
        canvas
    }

    fn ink(canvas: &[u8]) -> u64 {
        canvas.iter().map(|&b| u64::from(b)).sum()
    }

    /// Whether this host's session can smooth at all: a `LaunchDaemon`
    /// runner has no Aqua session, and Apple documents nothing either way.
    #[test]
    fn smoothing_changes_the_drawing_where_the_setting_enables_it() {
        let font = Font::load_test_font();
        let glyph = CGGlyph::try_from(font.glyph_for('H').expect("the probe face draws H"))
            .expect("a 16-bit glyph id");
        let ct = probe_font(&font, 24.0);
        let plain = render(&ct, glyph, Format::Gray, false);
        assert!(ink(&plain) > 0, "CoreText drew no ink");
        let preference = font_smoothing();
        if preference.smooths() {
            assert_ne!(
                render(&ct, glyph, Format::Gray, true),
                plain,
                "smoothing changed nothing with AppleFontSmoothing={preference:?}: this session \
                 cannot smooth, so the runner needs an Aqua session"
            );
        }
        let rgbx = render(&ct, glyph, Format::Rgbx, true);
        assert!(
            rgbx.as_chunks::<4>()
                .0
                .iter()
                .all(|[r, g, b, _]| r == g && g == b),
            "smoothing drew color fringes"
        );
    }
}
