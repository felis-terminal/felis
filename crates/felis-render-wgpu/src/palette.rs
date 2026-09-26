//! SGR color to linear-RGBA conversion. The renderer wants linear sRGB
//! so the surface format (`Bgra8UnormSrgb`) does the gamma encoding.

use std::collections::BTreeMap;

use felis_grid::Color;
use felis_protocol::messages::ThemeChannel;
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Default foreground (linear RGBA).
    pub fg: [f32; 4],
    /// Default background (linear RGBA).
    pub bg: [f32; 4],
    /// Sparse override of the xterm 256-color palette; `None` falls
    /// through to [`xterm_256`]. All 256 indices are overridable, not
    /// just the base 16: some color schemes address the low cube slots
    /// (`16..=21`) for accent colors that programs then color output
    /// with.
    pub palette: [Option<[u8; 3]>; 256],
    /// `None` means reverse video (the cursor cell's bg painted with
    /// its original fg).
    pub cursor: Option<[f32; 4]>,
}

/// Single source for [`Theme::default`] and [`configured_theme_report`],
/// so an `OSC 10 ; ?` reply and the painted surface cannot drift.
pub const DEFAULT_FG_SRGB: [u8; 3] = [0xE5, 0xE5, 0xE5];

/// Same as `Renderer::clear_color`; what `OSC 11 ; ?` reports, so a
/// background detector reads felis as dark.
pub const DEFAULT_BG_SRGB: [u8; 3] = [0x0D, 0x0D, 0x12];

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: srgb_to_linear_rgba(DEFAULT_FG_SRGB, 1.0),
            bg: srgb_to_linear_rgba(DEFAULT_BG_SRGB, 1.0),
            palette: [None; 256],
            cursor: None,
        }
    }
}

/// `(fg, bg, cursor)` as `SessionToDaemonMsg::ConfigureTheme` takes them.
pub type ConfiguredThemeReport = (
    Option<(u8, u8, u8)>,
    Option<(u8, u8, u8)>,
    Option<(u8, u8, u8)>,
);

/// The sRGB triples the client reports so an `OSC 10/11/12 ; ?` query
/// answers with felis's real surface color. fg and bg always resolve,
/// mirroring [`Theme::with_overrides`]'s fallback so the reported
/// color is what gets painted; an unset or malformed cursor reports
/// `None` because reverse video has no fixed color.
#[must_use]
#[expect(
    clippy::tuple_array_conversions,
    reason = "parse_hex_color yields [u8; 3]; the wire message and grid both speak (u8, u8, u8)"
)]
pub fn configured_theme_report(
    fg: Option<&str>,
    bg: Option<&str>,
    cursor: Option<&str>,
) -> ConfiguredThemeReport {
    let resolve_or = |s: Option<&str>, default: [u8; 3]| -> (u8, u8, u8) {
        let [r, g, b] = s.and_then(parse_hex_color).unwrap_or(default);
        (r, g, b)
    };
    let cursor = cursor.and_then(parse_hex_color).map(|[r, g, b]| (r, g, b));
    (
        Some(resolve_or(fg, DEFAULT_FG_SRGB)),
        Some(resolve_or(bg, DEFAULT_BG_SRGB)),
        cursor,
    )
}

impl Theme {
    #[must_use]
    pub fn with_overrides(fg: Option<&str>, bg: Option<&str>) -> Self {
        let mut theme = Self::default();
        if let Some(s) = fg {
            if let Some(rgb) = parse_hex_color(s) {
                theme.fg = srgb_to_linear_rgba(rgb, 1.0);
            } else {
                warn!(value = s, "theme.foreground is not #rrggbb; using default");
            }
        }
        if let Some(s) = bg {
            if let Some(rgb) = parse_hex_color(s) {
                theme.bg = srgb_to_linear_rgba(rgb, 1.0);
            } else {
                warn!(value = s, "theme.background is not #rrggbb; using default");
            }
        }
        theme
    }

    #[must_use]
    pub fn with_cursor(mut self, cursor: Option<&str>) -> Self {
        if let Some(s) = cursor {
            if let Some(rgb) = parse_hex_color(s) {
                self.cursor = Some(srgb_to_linear_rgba(rgb, 1.0));
            } else {
                warn!(
                    value = s,
                    "cursor.color is not #rrggbb; using reverse video"
                );
            }
        }
        self
    }

    #[must_use]
    pub fn with_palette(mut self, palette: &BTreeMap<u8, String>) -> Self {
        for (&i, s) in palette {
            if let Some(rgb) = parse_hex_color(s) {
                self.palette[usize::from(i)] = Some(rgb);
            } else if let Some(name) = PALETTE_NAMES.get(usize::from(i)) {
                warn!(value = %s, slot = name, "theme.palette.{name} is not #rrggbb; using default");
            } else {
                warn!(value = %s, slot = i, "theme.palette.indexed.{i} is not #rrggbb; using default");
            }
        }
        self
    }
}

/// `overrides` is indexed by [`ThemeChannel`]. `palette` is the OSC 4
/// layer and wins over the configured `theme.palette` entry.
#[must_use]
pub fn effective_theme(
    base: &Theme,
    overrides: &[Option<[u8; 3]>; 3],
    palette: &[Option<[u8; 3]>; 256],
) -> Theme {
    let mut t = *base;
    for (slot, over) in t.palette.iter_mut().zip(palette.iter()) {
        if over.is_some() {
            *slot = *over;
        }
    }
    if let Some(rgb) = overrides[ThemeChannel::Foreground as usize] {
        t.fg = srgb_to_linear_rgba(rgb, 1.0);
    }
    if let Some(rgb) = overrides[ThemeChannel::Background as usize] {
        // Keeps the base alpha (the configured `window.opacity`): an
        // `OSC 11` must not make a translucent terminal opaque.
        t.bg = srgb_to_linear_rgba(rgb, base.bg[3]);
    }
    if let Some(rgb) = overrides[ThemeChannel::Cursor as usize] {
        t.cursor = Some(srgb_to_linear_rgba(rgb, 1.0));
    }
    t
}

/// Painted on cells holding UAX #9 explicit-formatting characters so a
/// Trojan-Source attack (CVE-2021-42574) cannot hide. Hard-coded: a
/// security signal, not a theming choice.
pub const BIDI_MARKER_BG: [f32; 4] = [1.0, 1.0, 0.0, 1.0];

pub const BIDI_MARKER_FG: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Selection background. This and the chrome colors below are
/// precomputed because `powf` is not `const`; each is pinned to its
/// source hex by `precomputed_chrome_colors_match_srgb_to_linear_of_their_source_hex`.
/// Source `#335588`.
pub const SELECTION_BG: [f32; 4] = [0.033_104_762, 0.090_841_77, 0.246_201_36, 1.0];

/// IME pre-edit background; distinct from [`SELECTION_BG`] so an
/// adjacent selection and pre-edit do not blur together. Source
/// `#224477`.
pub const PREEDIT_BG: [f32; 4] = [0.015_996_292, 0.057_805_434, 0.184_474_99, 1.0];

/// OSC 8 hyperlink underline. Source `#6ca6ff`.
pub const LINK_UNDERLINE: [f32; 4] = [0.149_959_8, 0.381_326_1, 1.0, 1.0];

/// Scrollback indicator track, painted only while `viewport > 0`.
/// Source `#1d1d1d`.
pub const SCROLLBAR_TRACK: [f32; 4] = [0.012_286_488, 0.012_286_488, 0.012_286_488, 1.0];

/// Scrollback indicator thumb. Source `#5a5a5a`.
pub const SCROLLBAR_THUMB: [f32; 4] = [0.102_241_73, 0.102_241_73, 0.102_241_73, 1.0];

/// Search-match background, muted against [`SEARCH_CURRENT_BG`].
/// Source `#554422`.
pub const SEARCH_MATCH_BG: [f32; 4] = [0.090_841_77, 0.057_805_434, 0.015_996_292, 1.0];

/// The n/N-focused search match. Source `#cc7722`.
pub const SEARCH_CURRENT_BG: [f32; 4] = [0.603_827_4, 0.184_474_99, 0.015_996_292, 1.0];

/// Search bar; distinct from [`SCROLLBAR_TRACK`] so the two bands of
/// chrome stay separable. Source `#2a2a2a`.
pub const SEARCH_BAR_BG: [f32; 4] = [0.023_153_365, 0.023_153_365, 0.023_153_365, 1.0];

/// Confirmation bar; distinct from [`SEARCH_BAR_BG`] so the two bottom
/// bars cannot be mistaken for each other. Source `#552222`.
pub const CONFIRM_BAR_BG: [f32; 4] = [0.090_841_77, 0.015_996_292, 0.015_996_292, 1.0];

/// In SGR-index order.
pub const PALETTE_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright_black",
    "bright_red",
    "bright_green",
    "bright_yellow",
    "bright_blue",
    "bright_magenta",
    "bright_cyan",
    "bright_white",
];

#[must_use]
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let bytes = s.as_bytes();
    if bytes.first().copied() != Some(b'#') {
        return None;
    }
    match bytes.len() {
        7 => {
            let r = hex_byte(bytes[1], bytes[2])?;
            let g = hex_byte(bytes[3], bytes[4])?;
            let b = hex_byte(bytes[5], bytes[6])?;
            Some([r, g, b])
        }
        _ => None,
    }
}

const fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    let Some(h) = hex_nibble(hi) else { return None };
    let Some(l) = hex_nibble(lo) else { return None };
    Some((h << 4) | l)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub const XTERM_PALETTE: [[u8; 3]; 16] = {
    let mut table = [[0u8; 3]; 16];
    let mut i = 0usize;
    while i < table.len() {
        table[i] = xterm_256(i as u8);
        i += 1;
    }
    table
};

/// IEC 61966-2-1 sRGB → linear conversion for one component.
#[must_use]
pub fn srgb_byte_to_linear(component: u8) -> f32 {
    let c = f32::from(component) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[must_use]
pub fn srgb_to_linear_rgba(srgb: [u8; 3], alpha: f32) -> [f32; 4] {
    [
        srgb_byte_to_linear(srgb[0]),
        srgb_byte_to_linear(srgb[1]),
        srgb_byte_to_linear(srgb[2]),
        alpha,
    ]
}

/// Every color a cell walk can ask for, linearized once per theme
/// change: resolving at cell time costs a `powf` per channel per cell
/// per frame.
#[derive(Debug, Clone)]
pub struct ResolvedTheme {
    pub fg: [f32; 4],
    pub bg: [f32; 4],
    pub cursor: Option<[f32; 4]>,
    pub palette: [[f32; 4]; 256],
    srgb_linear: [f32; 256],
}

impl ResolvedTheme {
    #[must_use]
    pub fn new(base: &Theme) -> Self {
        let mut srgb_linear = [0.0f32; 256];
        for (byte, slot) in srgb_linear.iter_mut().enumerate() {
            *slot = srgb_byte_to_linear(byte as u8);
        }
        let mut palette = [[0.0f32; 4]; 256];
        for (index, slot) in palette.iter_mut().enumerate() {
            let rgb = base.palette[index].unwrap_or_else(|| xterm_256(index as u8));
            *slot = [
                srgb_linear[usize::from(rgb[0])],
                srgb_linear[usize::from(rgb[1])],
                srgb_linear[usize::from(rgb[2])],
                1.0,
            ];
        }
        Self {
            fg: base.fg,
            bg: base.bg,
            cursor: base.cursor,
            palette,
            srgb_linear,
        }
    }

    fn linear_rgb(&self, rgb: [u8; 3]) -> [f32; 4] {
        [
            self.srgb_linear[usize::from(rgb[0])],
            self.srgb_linear[usize::from(rgb[1])],
            self.srgb_linear[usize::from(rgb[2])],
            1.0,
        ]
    }
}

#[must_use]
pub fn resolve_fg(color: Color, theme: &ResolvedTheme) -> [f32; 4] {
    resolve(color, theme, theme.fg)
}

#[must_use]
pub fn resolve_bg(color: Color, theme: &ResolvedTheme) -> [f32; 4] {
    resolve(color, theme, theme.bg)
}

fn resolve(color: Color, theme: &ResolvedTheme, theme_default: [f32; 4]) -> [f32; 4] {
    match color {
        Color::Default => theme_default,
        Color::Indexed(i) => theme.palette[usize::from(i)],
        Color::Rgb(r, g, b) => theme.linear_rgb([r, g, b]),
    }
}

/// Array-shaped view of [`felis_grid::default_palette_color`], which
/// owns the table: the grid answers `OSC 4` queries from it, so a
/// second copy here would let a queried color and the painted one
/// disagree.
#[must_use]
// The lint's `.into()` is not const, and const is what lets
// `XTERM_PALETTE` project from this instead of being a second table.
#[allow(clippy::tuple_array_conversions)]
pub const fn xterm_256(index: u8) -> [u8; 3] {
    let (r, g, b) = felis_grid::default_palette_color(index);
    [r, g, b]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    const NO_PALETTE: [Option<[u8; 3]>; 256] = [None; 256];

    fn approx(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn srgb_zero_is_linear_zero() {
        assert!((srgb_byte_to_linear(0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn srgb_max_is_linear_one() {
        assert!((srgb_byte_to_linear(255) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn srgb_low_uses_linear_segment() {
        let c8 = 4u8;
        let expected = (f32::from(c8) / 255.0) / 12.92;
        assert!((srgb_byte_to_linear(c8) - expected).abs() < 1e-6);
    }

    #[test]
    fn default_color_uses_theme() {
        let theme = ResolvedTheme::new(&Theme::default());
        assert_eq!(resolve_fg(Color::Default, &theme), theme.fg);
        assert_eq!(resolve_bg(Color::Default, &theme), theme.bg);
    }

    #[test]
    fn cube_index_uses_the_xterm_ramp() {
        assert_eq!(xterm_256(196), [255, 0, 0]);
        assert_eq!(xterm_256(21), [0, 0, 255]);
        assert_eq!(xterm_256(16), [0, 0, 0]);
        assert_eq!(xterm_256(231), [255, 255, 255]);
    }

    #[test]
    fn grayscale_index_steps_evenly() {
        assert_eq!(xterm_256(232), [8, 8, 8]);
        assert_eq!(xterm_256(255), [238, 238, 238]);
    }

    #[test]
    fn xterm_first_16_match_the_legacy_table() {
        // Literal pins too, so a changed table row fails, not just a
        // broken projection.
        for i in 0..16u8 {
            assert_eq!(xterm_256(i), XTERM_PALETTE[usize::from(i)]);
        }
        assert_eq!(xterm_256(0), [0, 0, 0]);
        assert_eq!(xterm_256(1), [205, 0, 0]);
        assert_eq!(xterm_256(7), [229, 229, 229]);
        assert_eq!(xterm_256(8), [127, 127, 127]);
        assert_eq!(xterm_256(12), [92, 92, 255]);
        assert_eq!(xterm_256(15), [255, 255, 255]);
    }

    #[test]
    fn direct_rgb_passes_through_linearization() {
        let theme = ResolvedTheme::new(&Theme::default());
        assert!(approx(
            resolve_fg(Color::Rgb(255, 0, 0), &theme),
            srgb_to_linear_rgba([255, 0, 0], 1.0),
        ));
    }

    #[test]
    fn resolved_theme_tables_match_the_per_call_transform_bit_for_bit() {
        let overrides = BTreeMap::from([(1u8, "#ff5555".to_owned())]);
        let theme = ResolvedTheme::new(&Theme::default().with_palette(&overrides));
        for index in 0..=255u8 {
            let srgb = if index == 1 {
                [0xFF, 0x55, 0x55]
            } else {
                xterm_256(index)
            };
            assert_eq!(
                resolve_fg(Color::Indexed(index), &theme),
                srgb_to_linear_rgba(srgb, 1.0),
            );
            assert_eq!(
                resolve_bg(Color::Indexed(index), &theme),
                srgb_to_linear_rgba(srgb, 1.0),
            );
            assert_eq!(
                resolve_fg(Color::Rgb(index, index, index), &theme),
                srgb_to_linear_rgba([index; 3], 1.0),
            );
        }
    }

    fn assert_const_matches_hex(label: &str, computed: [f32; 4], hex: [u8; 3]) {
        let from_bytes = srgb_to_linear_rgba(hex, 1.0);
        for (a, b) in computed.iter().zip(from_bytes.iter()) {
            assert!(
                (a - b).abs() < 1e-3,
                "{label}: drift: precomputed {computed:?} vs computed {from_bytes:?}",
            );
        }
    }

    #[test]
    fn precomputed_chrome_colors_match_srgb_to_linear_of_their_source_hex() {
        for (label, computed, hex) in [
            ("LINK_UNDERLINE", LINK_UNDERLINE, [0x6c, 0xa6, 0xff]),
            ("PREEDIT_BG", PREEDIT_BG, [0x22, 0x44, 0x77]),
            ("SELECTION_BG", SELECTION_BG, [0x33, 0x55, 0x88]),
            ("SCROLLBAR_TRACK", SCROLLBAR_TRACK, [0x1d, 0x1d, 0x1d]),
            ("SCROLLBAR_THUMB", SCROLLBAR_THUMB, [0x5a, 0x5a, 0x5a]),
            ("SEARCH_MATCH_BG", SEARCH_MATCH_BG, [0x55, 0x44, 0x22]),
            ("SEARCH_CURRENT_BG", SEARCH_CURRENT_BG, [0xcc, 0x77, 0x22]),
            ("SEARCH_BAR_BG", SEARCH_BAR_BG, [0x2a, 0x2a, 0x2a]),
            ("CONFIRM_BAR_BG", CONFIRM_BAR_BG, [0x55, 0x22, 0x22]),
        ] {
            assert_const_matches_hex(label, computed, hex);
        }
    }

    #[test]
    fn configured_report_falls_back_to_renderer_defaults_when_unset() {
        // fg / bg must report the painted defaults, not `None`: else the
        // daemon falls back to xterm white and a background detector
        // flips to a light scheme.
        let (fg, bg, cursor) = configured_theme_report(None, None, None);
        assert_eq!(fg, Some((0xE5, 0xE5, 0xE5)));
        assert_eq!(bg, Some((0x0D, 0x0D, 0x12)));
        assert_eq!(cursor, None);
    }

    #[test]
    fn configured_report_uses_each_configured_color() {
        let (fg, bg, cursor) =
            configured_theme_report(Some("#102030"), Some("#abcdef"), Some("#ffaa00"));
        assert_eq!(fg, Some((0x10, 0x20, 0x30)));
        assert_eq!(bg, Some((0xAB, 0xCD, 0xEF)));
        assert_eq!(cursor, Some((0xFF, 0xAA, 0x00)));
    }

    #[test]
    fn configured_report_malformed_fg_bg_fall_back_to_defaults() {
        let (fg, bg, cursor) =
            configured_theme_report(Some("not-a-color"), Some("xyz"), Some("nope"));
        assert_eq!(fg, Some((0xE5, 0xE5, 0xE5)));
        assert_eq!(bg, Some((0x0D, 0x0D, 0x12)));
        assert_eq!(cursor, None);
    }

    #[test]
    fn configured_report_defaults_match_theme_default_linear() {
        let t = Theme::default();
        assert_eq!(srgb_to_linear_rgba(DEFAULT_FG_SRGB, 1.0), t.fg);
        assert_eq!(srgb_to_linear_rgba(DEFAULT_BG_SRGB, 1.0), t.bg);
    }

    #[test]
    fn parse_hex_color_rejects_malformed() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("abcdef"), None, "missing #");
        assert_eq!(parse_hex_color("#ab"), None, "too short");
        assert_eq!(parse_hex_color("#abc"), None, "CSS shorthand not accepted");
        assert_eq!(parse_hex_color("#abcd"), None, "4 digits not supported");
        assert_eq!(parse_hex_color("#abcde"), None, "5 digits not supported");
        assert_eq!(parse_hex_color("#abcdefab"), None, "alpha not supported");
        assert_eq!(parse_hex_color("#zzzzzz"), None, "non-hex digits");
        assert_eq!(parse_hex_color("#abcde "), None, "trailing space");
    }

    #[test]
    fn theme_with_overrides_overlays_only_provided_channels() {
        let theme = Theme::with_overrides(Some("#ff0000"), None);
        let expected_fg = srgb_to_linear_rgba([0xFF, 0, 0], 1.0);
        assert!(approx(theme.fg, expected_fg));
        assert!(approx(theme.bg, Theme::default().bg));
    }

    #[test]
    fn theme_with_overrides_falls_back_on_malformed_string() {
        let theme = Theme::with_overrides(Some("not-a-color"), Some("#101010"));
        assert!(approx(theme.fg, Theme::default().fg));
        assert!(approx(
            theme.bg,
            srgb_to_linear_rgba([0x10, 0x10, 0x10], 1.0)
        ));
    }

    #[test]
    fn with_cursor_some_valid_hex_stores_linear_rgba() {
        let theme = Theme::default().with_cursor(Some("#ffaa00"));
        let expected = srgb_to_linear_rgba([0xFF, 0xAA, 0x00], 1.0);
        assert_eq!(theme.cursor, Some(expected));
    }

    #[test]
    fn with_cursor_none_keeps_reverse_video_default() {
        let theme = Theme::default().with_cursor(None);
        assert_eq!(theme.cursor, None);
    }

    #[test]
    fn with_cursor_malformed_string_falls_back_to_none() {
        let theme = Theme::default().with_cursor(Some("not-a-color"));
        assert_eq!(theme.cursor, None);
    }

    #[test]
    fn with_cursor_overwrites_a_previously_set_cursor() {
        let theme = Theme::default()
            .with_cursor(Some("#ffaa00"))
            .with_cursor(Some("#00ff00"));
        let expected = srgb_to_linear_rgba([0x00, 0xFF, 0x00], 1.0);
        assert_eq!(theme.cursor, Some(expected));
    }

    #[test]
    fn theme_palette_default_is_all_none() {
        let theme = Theme::default();
        for slot in theme.palette {
            assert!(slot.is_none());
        }
    }

    #[test]
    fn with_palette_overrides_slot_one_for_resolve_indexed() {
        let overrides = BTreeMap::from([(1u8, "#ff5555".to_owned())]);
        let theme = ResolvedTheme::new(&Theme::default().with_palette(&overrides));
        let expected = srgb_to_linear_rgba([0xFF, 0x55, 0x55], 1.0);
        assert!(approx(resolve_fg(Color::Indexed(1), &theme), expected));
        let xterm_green = srgb_to_linear_rgba(XTERM_PALETTE[2], 1.0);
        assert!(approx(resolve_fg(Color::Indexed(2), &theme), xterm_green));
    }

    #[test]
    fn with_palette_overrides_extended_cube_index() {
        let overrides = BTreeMap::from([(16u8, "#d08770".to_owned())]);
        let theme = ResolvedTheme::new(&Theme::default().with_palette(&overrides));
        let expected = srgb_to_linear_rgba([0xD0, 0x87, 0x70], 1.0);
        assert!(approx(resolve_fg(Color::Indexed(16), &theme), expected));
        let xterm_200 = srgb_to_linear_rgba(xterm_256(200), 1.0);
        assert!(approx(resolve_fg(Color::Indexed(200), &theme), xterm_200));
    }

    #[test]
    fn malformed_palette_entry_keeps_xterm_default() {
        let overrides = BTreeMap::from([(1u8, "not-a-color".to_owned())]);
        let theme = ResolvedTheme::new(&Theme::default().with_palette(&overrides));
        let xterm_red = srgb_to_linear_rgba(XTERM_PALETTE[1], 1.0);
        assert!(approx(resolve_fg(Color::Indexed(1), &theme), xterm_red));
    }

    #[test]
    fn palette_names_match_sgr_order() {
        assert_eq!(PALETTE_NAMES[0], "black");
        assert_eq!(PALETTE_NAMES[7], "white");
        assert_eq!(PALETTE_NAMES[8], "bright_black");
        assert_eq!(PALETTE_NAMES[15], "bright_white");
        assert_eq!(PALETTE_NAMES.len(), 16);
    }

    #[test]
    fn effective_theme_with_no_overrides_equals_base() {
        let base = Theme::default();
        let eff = effective_theme(&base, &[None; 3], &NO_PALETTE);
        assert!(approx(eff.fg, base.fg));
        assert!(approx(eff.bg, base.bg));
        assert_eq!(eff.cursor, base.cursor);
    }

    #[test]
    fn effective_theme_lifts_each_channel_independently() {
        let base = Theme::default();
        let mut overrides = [None; 3];

        overrides[ThemeChannel::Foreground as usize] = Some([0xAB, 0x00, 0x00]);
        let eff = effective_theme(&base, &overrides, &NO_PALETTE);
        assert!(approx(eff.fg, srgb_to_linear_rgba([0xAB, 0x00, 0x00], 1.0)));
        assert!(approx(eff.bg, base.bg));
        assert_eq!(eff.cursor, None);

        overrides = [None; 3];
        overrides[ThemeChannel::Background as usize] = Some([0x00, 0xCD, 0x00]);
        let eff = effective_theme(&base, &overrides, &NO_PALETTE);
        assert!(approx(eff.fg, base.fg));
        assert!(approx(eff.bg, srgb_to_linear_rgba([0x00, 0xCD, 0x00], 1.0)));

        overrides = [None; 3];
        overrides[ThemeChannel::Cursor as usize] = Some([0x00, 0x00, 0xEF]);
        let eff = effective_theme(&base, &overrides, &NO_PALETTE);
        assert_eq!(
            eff.cursor,
            Some(srgb_to_linear_rgba([0x00, 0x00, 0xEF], 1.0))
        );
    }

    #[test]
    fn effective_theme_bg_override_keeps_configured_opacity() {
        let base = Theme {
            bg: srgb_to_linear_rgba(DEFAULT_BG_SRGB, 0.8),
            ..Theme::default()
        };
        let mut overrides = [None; 3];
        overrides[ThemeChannel::Background as usize] = Some([0x00, 0xCD, 0x00]);
        let eff = effective_theme(&base, &overrides, &NO_PALETTE);
        assert!(
            (eff.bg[3] - 0.8).abs() < 1e-6,
            "opacity must survive OSC 11"
        );
        let want = srgb_to_linear_rgba([0x00, 0xCD, 0x00], 0.8);
        assert!(approx(eff.bg, want));
    }

    #[test]
    fn effective_theme_runtime_cursor_overrides_configured_cursor() {
        let base = Theme {
            cursor: Some([0.1, 0.1, 0.1, 1.0]),
            ..Theme::default()
        };
        let mut overrides = [None; 3];
        overrides[ThemeChannel::Cursor as usize] = Some([0xFF, 0xAA, 0x00]);
        let eff = effective_theme(&base, &overrides, &NO_PALETTE);
        assert_eq!(
            eff.cursor,
            Some(srgb_to_linear_rgba([0xFF, 0xAA, 0x00], 1.0))
        );
    }

    /// The `OSC 4 ; idx ; ?` reply (daemon, from grid state) and the
    /// painted color (client, from the override layer) must agree.
    #[test]
    fn osc_4_query_reply_and_the_painted_color_agree() {
        use felis_grid::{Grid, PtyEffect};

        let mut grid = Grid::new(2, 4);
        felis_vt::Parser::new().advance(&mut grid, b"\x1b]4;3;#aabbcc\x1b\\\x1b]4;3;?\x1b\\");

        let reply = grid
            .take_pty_effects()
            .into_iter()
            .find_map(|e| match e {
                PtyEffect::Response(bytes) => Some(bytes),
                _ => None,
            })
            .expect("OSC 4 ? enqueues a reply");
        assert_eq!(
            std::str::from_utf8(&reply).unwrap(),
            "\x1b]4;3;rgb:aaaa/bbbb/cccc\x1b\\"
        );

        let mut runtime = NO_PALETTE;
        let deltas = grid.take_palette_dirty();
        assert!(!deltas.reset_all);
        for entry in deltas.entries {
            runtime[usize::from(entry.index)] = entry.rgb.map(<[u8; 3]>::from);
        }
        let theme = ResolvedTheme::new(&effective_theme(&Theme::default(), &[None; 3], &runtime));
        assert!(approx(
            resolve_fg(Color::Indexed(3), &theme),
            srgb_to_linear_rgba([0xAA, 0xBB, 0xCC], 1.0),
        ));
    }

    #[test]
    fn osc_4_layer_wins_over_config_and_falls_back_to_it_on_reset() {
        let mut base = Theme::default();
        base.palette[3] = Some([0x11, 0x22, 0x33]);

        let mut runtime = NO_PALETTE;
        runtime[3] = Some([0xAA, 0xBB, 0xCC]);
        let with_override = ResolvedTheme::new(&effective_theme(&base, &[None; 3], &runtime));
        assert!(approx(
            resolve_fg(Color::Indexed(3), &with_override),
            srgb_to_linear_rgba([0xAA, 0xBB, 0xCC], 1.0),
        ));

        let after_reset = ResolvedTheme::new(&effective_theme(&base, &[None; 3], &NO_PALETTE));
        assert!(approx(
            resolve_fg(Color::Indexed(3), &after_reset),
            srgb_to_linear_rgba([0x11, 0x22, 0x33], 1.0),
        ));
    }

    proptest::proptest! {
        #[test]
        fn parse_hex_round_trips_any_rgb(r: u8, g: u8, b: u8) {
            let s = format!("#{r:02x}{g:02x}{b:02x}");
            let parsed = parse_hex_color(&s);
            proptest::prop_assert_eq!(parsed, Some([r, g, b]));
        }

        #[test]
        fn parse_hex_uppercase_round_trips(r: u8, g: u8, b: u8) {
            let s = format!("#{r:02X}{g:02X}{b:02X}");
            proptest::prop_assert_eq!(parse_hex_color(&s), Some([r, g, b]));
        }

        #[test]
        fn parse_hex_rejects_wrong_lengths(prefix in "[#]?", n in 0usize..8) {
            let body: String = (0..n).map(|i| (b'0' + (i as u8 % 10)) as char).collect();
            let candidate = format!("{prefix}{body}");
            let len = candidate.len();
            let starts_with_hash = candidate.starts_with('#');
            if !(starts_with_hash && len == 7) {
                proptest::prop_assert_eq!(parse_hex_color(&candidate), None);
            }
        }

        #[test]
        fn palette_overrides_round_trip(rgbs in proptest::array::uniform16(proptest::array::uniform3(proptest::num::u8::ANY))) {
            let mut overrides: BTreeMap<u8, String> = BTreeMap::new();
            for (i, rgb) in rgbs.iter().enumerate() {
                let [r, g, b] = *rgb;
                #[allow(clippy::cast_possible_truncation)]
                overrides.insert(i as u8, format!("#{r:02x}{g:02x}{b:02x}"));
            }
            let theme = ResolvedTheme::new(&Theme::default().with_palette(&overrides));
            for (i, rgb) in rgbs.iter().enumerate() {
                let expected = srgb_to_linear_rgba(*rgb, 1.0);
                #[allow(clippy::cast_possible_truncation)]
                let got = resolve_fg(Color::Indexed(i as u8), &theme);
                proptest::prop_assert!(approx(got, expected),
                    "palette slot {i} did not round-trip: got {got:?}, want {expected:?}");
            }
        }
    }
}
