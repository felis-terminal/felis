//! CoreText draws a macOS face's outlines, dilated by the platform's font
//! smoothing as every native window draws them. The face is CoreText's
//! reading of felis's own bytes, so its glyph ids are the shaper's; the
//! box it draws into is the unhinted outline's, grown by a pad for the
//! dilation, and everything else stays on felis's side.

use std::{collections::HashSet, ffi::c_void, ptr::NonNull, sync::Arc, sync::OnceLock};

use objc2_core_foundation::{
    CFAllocator, CFAllocatorContext, CFArray, CFBoolean, CFData, CFDictionary, CFIndex, CFNumber,
    CFPreferencesCopyAppValue, CFRetained, CFString, CFType, CGAffineTransform, CGPoint, CGRect,
    CGSize, kCFPreferencesCurrentApplication,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGGlyph, CGImageAlphaInfo, CGTextDrawingMode,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, kCTFontNameAttribute, kCTFontOpticalSizeAttribute,
    kCTFontVariationAttribute,
};
use skrifa::raw::{TableProvider, types::NameId};
use tracing::debug;

use super::{SkrifaOutlines, coverage};
use crate::{Font, FontBytes, GlyphBitmap, GlyphId, metadata};

#[cfg(test)]
mod tests;

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

/// What [`CoreTextOutlines::coverage`] did with a glyph.
pub(crate) enum Drawn {
    Glyph(GlyphBitmap),
    /// The glyph draws nothing, or its canvas is over a raster limit.
    Nothing,
    /// CoreText cannot draw it; skrifa does.
    Skrifa,
}

/// `CTFontCreateWithFontDescriptor` parses tables, so fonts are kept for
/// the last few (font, size) pairs, as skrifa's scalers are.
const FONT_SLOTS: usize = 8;

struct FontSlot {
    font: u64,
    px: u32,
    used: u64,
    ct: CFRetained<CTFont>,
}

struct DescriptorSlot {
    font: u64,
    used: u64,
    descriptor: CFRetained<CTFontDescriptor>,
}

/// Every CoreText object lives here, on the thread that rasterizes:
/// `CTFont` is not `Send`, and [`Font`] must be.
#[derive(Default)]
pub(crate) struct CoreTextOutlines {
    fonts: Vec<FontSlot>,
    descriptors: Vec<DescriptorSlot>,
    /// Faces CoreText cannot draw, by font id; they draw through skrifa.
    failed: HashSet<u64>,
    clock: u64,
}

impl CoreTextOutlines {
    pub(crate) fn coverage(
        &mut self,
        skrifa: &mut SkrifaOutlines,
        font: &Font,
        glyph: GlyphId,
        px: f32,
    ) -> Drawn {
        let Ok(id) = CGGlyph::try_from(glyph) else {
            return Drawn::Skrifa;
        };
        if !(px.is_finite() && px > 0.0) {
            return Drawn::Nothing;
        }
        let Some(ink_box) = skrifa.layer_box(font, glyph, px) else {
            return Drawn::Nothing;
        };
        let [left, top, right, bottom] = ink_box;
        if right <= left || top <= bottom {
            return Drawn::Nothing;
        }
        let Some(ct) = self.font(font, px) else {
            return Drawn::Skrifa;
        };
        let canvas = match Canvas::draw(&ct, id, ink_box, px, font_smoothing().smooths()) {
            Ok(canvas) => canvas,
            Err(Unpainted::OverLimit) => return Drawn::Nothing,
            Err(Unpainted::NoContext) => return Drawn::Skrifa,
        };
        let mut mask = canvas.trim();
        if mask.width == 0 {
            return Drawn::Skrifa;
        }
        coverage::shrink_within(&mut mask.data);
        Drawn::Glyph(mask.into())
    }

    /// `font` at `px` as CoreText draws it, `None` for a face it cannot
    /// draw, which is remembered.
    fn font(&mut self, font: &Font, px: f32) -> Option<CFRetained<CTFont>> {
        if self.failed.contains(&font.id) {
            return None;
        }
        self.clock += 1;
        let key = (font.id, px.to_bits());
        if let Some(slot) = self.fonts.iter_mut().find(|s| (s.font, s.px) == key) {
            slot.used = self.clock;
            return Some(slot.ct.clone());
        }
        let opened = self
            .descriptor(font)
            .and_then(|descriptor| at_size(font, &descriptor, px));
        let ct = match opened {
            Ok(ct) => ct,
            Err(reason) => {
                debug!(
                    font = %font.display_name(),
                    reason,
                    "CoreText cannot draw this face; skrifa draws it"
                );
                self.failed.insert(font.id);
                self.descriptors.retain(|slot| slot.font != font.id);
                return None;
            }
        };
        let slot = FontSlot {
            font: key.0,
            px: key.1,
            used: self.clock,
            ct: ct.clone(),
        };
        replace_oldest(&mut self.fonts, slot, |s| s.used);
        Some(ct)
    }

    fn descriptor(&mut self, font: &Font) -> Result<CFRetained<CTFontDescriptor>, &'static str> {
        if let Some(slot) = self.descriptors.iter_mut().find(|s| s.font == font.id) {
            slot.used = self.clock;
            return Ok(slot.descriptor.clone());
        }
        let descriptor = open(font)?;
        let slot = DescriptorSlot {
            font: font.id,
            used: self.clock,
            descriptor: descriptor.clone(),
        };
        replace_oldest(&mut self.descriptors, slot, |s| s.used);
        Ok(descriptor)
    }
}

fn replace_oldest<T>(slots: &mut Vec<T>, slot: T, used: impl Fn(&T) -> u64) {
    if slots.len() < FONT_SLOTS {
        slots.push(slot);
    } else if let Some(oldest) = slots.iter_mut().min_by_key(|s| used(s)) {
        *oldest = slot;
    }
}

/// The descriptor for `font`'s face at its variation position, read from
/// its own bytes. A collection's member is picked by PostScript name, as
/// CoreText does not say its array follows the collection's order.
fn open(font: &Font) -> Result<CFRetained<CTFontDescriptor>, &'static str> {
    let name = metadata::any_name(&font.face, NameId::POSTSCRIPT_NAME)
        .ok_or("the face has no PostScript name")?;
    let data = shared_data(&font.bytes).ok_or("CoreFoundation wraps no data")?;
    let descriptors = descriptors_from_data(&data).ok_or("CoreText reads no face")?;
    // SAFETY: CoreText documents the array as holding font descriptors.
    let descriptors =
        unsafe { CFRetained::cast_unchecked::<CFArray<CTFontDescriptor>>(descriptors) };
    let mut named = descriptors
        .iter()
        .filter(|descriptor| post_script_name(descriptor).as_deref() == Some(name.as_str()));
    let member = named.next().ok_or("no face carries the PostScript name")?;
    if named.next().is_some() {
        return Err("two faces carry the PostScript name");
    }
    copy_with_attributes(&member, &instance_attributes(font)).ok_or("CoreText copies no descriptor")
}

/// The descriptor's font at `px`, once the glyph count and the variation
/// CoreText reports prove it is the face felis shaped with.
fn at_size(
    font: &Font,
    descriptor: &CTFontDescriptor,
    px: f32,
) -> Result<CFRetained<CTFont>, &'static str> {
    let ct = font_from_descriptor(descriptor, f64::from(px)).ok_or("CoreText creates no font")?;
    let glyphs = skrifa::FontRef::from_index(font.bytes.as_ref().as_ref(), font.index)
        .ok()
        .and_then(|face| face.maxp().ok())
        .and_then(|maxp| CFIndex::try_from(maxp.num_glyphs()).ok());
    // SAFETY: `ct` is a live CTFont.
    if glyphs != Some(unsafe { ct.glyph_count() }) || fault(Fault::GlyphCount) {
        return Err("the glyph count differs");
    }
    if !variation_matches(font, &ct) {
        return Err("the variation differs");
    }
    Ok(ct)
}

/// Every axis of the face with felis's setting, its default where felis
/// set none, in design-space values.
fn requested_axes(font: &Font) -> Vec<(u32, f64, f64)> {
    font.face
        .axes()
        .iter()
        .map(|axis| {
            let value = font
                .position
                .iter()
                .find(|(tag, _)| *tag == axis.tag())
                .map_or_else(|| axis.default_value(), |&(_, value)| value);
            let tolerance = f64::from(axis.max_value() - axis.min_value()) / 32768.0;
            (
                u32::from_be_bytes(axis.tag().to_be_bytes()),
                f64::from(value),
                tolerance,
            )
        })
        .collect()
}

/// Every axis named, so nothing is left to CoreText's matching, and
/// automatic optical sizing off, so `opsz` stays where the shaper's is.
fn instance_attributes(font: &Font) -> CFRetained<CFDictionary<CFString, CFType>> {
    let axes = requested_axes(font);
    let tags: Vec<CFRetained<CFNumber>> = axes
        .iter()
        .map(|&(tag, _, _)| CFNumber::new_i64(i64::from(tag)))
        .collect();
    let values: Vec<CFRetained<CFNumber>> = axes
        .iter()
        .map(|&(_, value, _)| CFNumber::new_f64(value))
        .collect();
    let variation = CFDictionary::<CFNumber, CFNumber>::from_slices(
        &tags.iter().map(|tag| &**tag).collect::<Vec<_>>(),
        &values.iter().map(|value| &**value).collect::<Vec<_>>(),
    );
    let none = CFString::from_static_str("none");
    // SAFETY: reading CoreText's immutable attribute-name constants.
    let keys = unsafe { [kCTFontVariationAttribute, kCTFontOpticalSizeAttribute] };
    CFDictionary::from_slices(&keys, &[(*variation).as_ref(), (*none).as_ref()])
}

/// Whether CoreText drew the instance asked for: it treats the variation
/// as a hint, and reports only the axes off their default.
fn variation_matches(font: &Font, ct: &CTFont) -> bool {
    let reported = copy_variation(ct).map(|variation| {
        // SAFETY: CoreText documents the dictionary as mapping axis
        // identifiers to values, both numbers.
        unsafe { CFRetained::cast_unchecked::<CFDictionary<CFNumber, CFNumber>>(variation) }
    });
    let axes = font.face.axes();
    requested_axes(font)
        .into_iter()
        .zip(axes.iter())
        .all(|((tag, value, tolerance), axis)| {
            let actual = reported
                .as_ref()
                .and_then(|reported| reported.get(&CFNumber::new_i64(i64::from(tag))))
                .and_then(|value| value.as_f64())
                .unwrap_or_else(|| f64::from(axis.default_value()));
            (actual - value).abs() <= tolerance
        })
}

fn post_script_name(descriptor: &CTFontDescriptor) -> Option<String> {
    if fault(Fault::Name) {
        return None;
    }
    // SAFETY: reading CoreText's immutable attribute-name constant; a
    // value of another type fails the downcast.
    let value = unsafe { descriptor.attribute(kCTFontNameAttribute) }?;
    Some(value.downcast::<CFString>().ok()?.to_string())
}

/// A `CFData` over `bytes` without a copy. Its deallocator holds a clone
/// of the `Arc`, so the mapping outlives whatever CoreText keeps of it.
fn shared_data(bytes: &FontBytes) -> Option<CFRetained<CFData>> {
    unsafe extern "C-unwind" fn release(info: *const c_void) {
        // SAFETY: `info` is the box `shared_data` made for this
        // allocator; CoreFoundation releases it once, when it frees the
        // allocator.
        drop(unsafe { Box::from_raw(info.cast_mut().cast::<FontBytes>()) });
    }
    let slice: &[u8] = bytes.as_ref().as_ref();
    let length = CFIndex::try_from(slice.len()).ok()?;
    let info = Box::into_raw(Box::new(Arc::clone(bytes)));
    let mut context = CFAllocatorContext {
        version: 0,
        info: info.cast(),
        retain: None,
        release: Some(release),
        copyDescription: None,
        allocate: None,
        reallocate: None,
        deallocate: None,
        preferredSize: None,
    };
    // SAFETY: the context is valid for the call, which copies it.
    let Some(allocator) = (unsafe { CFAllocator::new(None, &raw mut context) }) else {
        // SAFETY: no allocator took `info`, so this is its only owner.
        drop(unsafe { Box::from_raw(info) });
        return None;
    };
    // SAFETY: the bytes stay mapped while `info` holds its `Arc`, which
    // lasts until CoreFoundation frees the allocator; the data retains
    // the allocator as its deallocator, so that is after the data.
    unsafe { CFData::with_bytes_no_copy(None, slice.as_ptr(), length, Some(&allocator)) }
}

/// The descriptors CoreText reads from `data`, one per face of a
/// collection; `None` when it reads none.
fn descriptors_from_data(data: &CFData) -> Option<CFRetained<CFArray>> {
    unsafe extern "C-unwind" {
        fn CTFontManagerCreateFontDescriptorsFromData(data: &CFData) -> Option<NonNull<CFArray>>;
    }
    if fault(Fault::Descriptors) {
        return None;
    }
    // SAFETY: `data` is a live CFData; the call returns a +1 array or
    // NULL, which the binding in objc2-core-text would unwrap.
    let array = unsafe { CTFontManagerCreateFontDescriptorsFromData(data) }?;
    // SAFETY: a Create call's result is owned by the caller.
    Some(unsafe { CFRetained::from_raw(array) })
}

fn copy_with_attributes(
    descriptor: &CTFontDescriptor,
    attributes: &CFDictionary<CFString, CFType>,
) -> Option<CFRetained<CTFontDescriptor>> {
    unsafe extern "C-unwind" {
        fn CTFontDescriptorCreateCopyWithAttributes(
            original: &CTFontDescriptor,
            attributes: &CFDictionary,
        ) -> Option<NonNull<CTFontDescriptor>>;
    }
    if fault(Fault::CopyWithAttributes) {
        return None;
    }
    // SAFETY: the dictionary maps CoreText attribute names to values of
    // the types CoreText documents for them; the result is a +1
    // descriptor or NULL.
    let copy =
        unsafe { CTFontDescriptorCreateCopyWithAttributes(descriptor, attributes.as_ref()) }?;
    // SAFETY: a Create call's result is owned by the caller.
    Some(unsafe { CFRetained::from_raw(copy) })
}

/// `descriptor` at `px`; `None` where CoreText creates no font.
fn font_from_descriptor(descriptor: &CTFontDescriptor, px: f64) -> Option<CFRetained<CTFont>> {
    unsafe extern "C-unwind" {
        fn CTFontCreateWithFontDescriptor(
            descriptor: &CTFontDescriptor,
            size: f64,
            matrix: *const CGAffineTransform,
        ) -> Option<NonNull<CTFont>>;
    }
    if fault(Fault::CreateFont) {
        return None;
    }
    // SAFETY: a null matrix asks for the identity transform; the result is
    // a +1 font or NULL.
    let font = unsafe { CTFontCreateWithFontDescriptor(descriptor, px, std::ptr::null()) }?;
    // SAFETY: a Create call's result is owned by the caller.
    Some(unsafe { CFRetained::from_raw(font) })
}

fn copy_variation(ct: &CTFont) -> Option<CFRetained<CFDictionary>> {
    if fault(Fault::CopyVariation) {
        return None;
    }
    // SAFETY: `ct` is a live CTFont.
    unsafe { ct.variation() }
}

/// The CoreText calls a test can make fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    Descriptors,
    Name,
    CopyWithAttributes,
    CreateFont,
    CopyVariation,
    GlyphCount,
    BlankCanvas,
}

#[cfg(test)]
thread_local! {
    static FAULT: std::cell::Cell<Option<Fault>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn inject(fault: Option<Fault>) {
    FAULT.with(|cell| cell.set(fault));
}

#[cfg(test)]
fn fault(which: Fault) -> bool {
    FAULT.with(|cell| cell.get() == Some(which))
}

#[cfg(not(test))]
const fn fault(_: Fault) -> bool {
    false
}

/// CoreGraphics dilates a smoothed outline by an amount it does not
/// document, and Skia notes it grows with the size, so the pad does too.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the side limit rejects any pad near i64's range"
)]
fn pad(px: f32) -> i64 {
    1 + (px / 16.0).ceil() as i64
}

enum Unpainted {
    OverLimit,
    NoContext,
}

/// One glyph as CoreGraphics drew it, before the trim to its ink.
pub(super) struct Canvas {
    pub(super) data: Vec<u8>,
    pub(super) width: usize,
    pub(super) height: usize,
    /// The canvas's left edge from the pen, and its top edge up from the
    /// baseline.
    pub(super) left: i32,
    pub(super) top: i32,
}

impl Canvas {
    /// `glyph` drawn into `ink_box` (the unhinted outline's
    /// `[left, top, right, bottom]`, y up) grown by the pad. The canvas
    /// is charged before it is allocated, and is the glyph's only
    /// image-sized buffer.
    fn draw(
        ct: &CTFont,
        glyph: CGGlyph,
        ink_box: [i32; 4],
        px: f32,
        smooth: bool,
    ) -> Result<Self, Unpainted> {
        let [left, top, right, bottom] = ink_box.map(i64::from);
        let pad = pad(px);
        let side = coverage::side_limit(px).ok_or(Unpainted::OverLimit)?;
        let span = |from: i64, to: i64| {
            usize::try_from(to - from + 2 * pad)
                .ok()
                .filter(|&n| n as f32 <= side)
                .ok_or(Unpainted::OverLimit)
        };
        let (width, height) = (span(left, right)?, span(bottom, top)?);
        width
            .checked_mul(height)
            .filter(|&bytes| bytes <= coverage::IMAGE_BUDGET)
            .ok_or(Unpainted::OverLimit)?;
        let edge = |v: i64| i32::try_from(v).map_err(|_| Unpainted::OverLimit);
        let (canvas_left, canvas_top) = (edge(left - pad)?, edge(top + pad)?);
        let mut data = vec![0; width * height];
        #[expect(clippy::cast_precision_loss, reason = "the side limit bounds both")]
        let origin = CGPoint::new((pad - left) as f64, (pad - bottom) as f64);
        if !draw(ct, glyph, origin, width, Format::Gray, smooth, &mut data) {
            return Err(Unpainted::NoContext);
        }
        if fault(Fault::BlankCanvas) {
            data.fill(0);
        }
        Ok(Self {
            data,
            width,
            height,
            left: canvas_left,
            top: canvas_top,
        })
    }

    fn trim(self) -> coverage::Mask {
        coverage::trim_in_place(self.data, self.width, self.height, self.left, self.top)
    }
}

/// A canvas's pixel format: one gray byte, or RGBX for the probe that
/// checks smoothing adds no color.
#[derive(Clone, Copy)]
pub(super) enum Format {
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
pub(super) fn draw(
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
    #[expect(clippy::cast_precision_loss, reason = "the side limit bounds both")]
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
