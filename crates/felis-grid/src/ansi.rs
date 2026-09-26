//! Grid to ANSI/SGR re-encoding: serializes stored cell state into byte streams.
//!
//! [`row_ansi`] resets the pen on changes rather than minimal-diffing to keep
//! output deterministic and free of stale carried attributes.

use core::fmt::Write as _;
use std::num::NonZeroU16;

use felis_protocol::kitty_graphics::placeholder::PLACEHOLDER;

use crate::{
    AttrFlags, Attributes, Cell, ClusterTable, Color, Grapheme, LinkTable, LinkText, StyleId,
    StyleTable, UnderlineStyle,
};

/// What the host can render, for the consumers that paint into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnsiCaps {
    /// Host renders 24-bit `38;2;r;g;b` color. When false every
    /// [`Color::Rgb`] is emitted as its nearest xterm-256 index.
    pub truecolor: bool,
}

impl Default for AnsiCaps {
    fn default() -> Self {
        Self { truecolor: true }
    }
}

/// Emission options for [`row_ansi_with`]. [`Default`] reproduces
/// [`row_ansi`] exactly.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowAnsiOptions<'a> {
    pub caps: AnsiCaps,
    /// Exact column count to clip or pad with spaces (`None` trims blanks).
    /// Continuations and unresolvable clusters emit a space to preserve columns.
    pub width: Option<u16>,
    /// Emit a space for a Kitty Unicode-placeholder cell instead of the
    /// `U+10EEEE` sentinel.
    pub blank_placeholders: bool,
    /// Hyperlink registry the cells' 1-based `link` ids resolve through.
    /// `None` emits no OSC 8.
    pub links: Option<&'a LinkTable>,
}

const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Serialize one cell row to its glyphs plus reconstructed SGR without trailing newline.
///
/// Trailing visually-blank cells are trimmed; non-default backgrounds are preserved.
#[must_use]
pub fn row_ansi(row: &[Cell], clusters: &ClusterTable, styles: &StyleTable) -> Vec<u8> {
    row_ansi_with(row, clusters, styles, &RowAnsiOptions::default())
}

/// [`row_ansi`] under explicit emission options.
///
/// Assumes a default pen and no open hyperlink on entry, and leaves
/// both closed on exit, so rows compose by concatenation.
#[must_use]
pub fn row_ansi_with(
    row: &[Cell],
    clusters: &ClusterTable,
    styles: &StyleTable,
    opts: &RowAnsiOptions<'_>,
) -> Vec<u8> {
    let (end, pad) = match opts.width {
        Some(w) => {
            let w = usize::from(w);
            let taken = row.len().min(w);
            (taken, w - taken)
        }
        None => (
            row.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1),
            0,
        ),
    };
    if end == 0 && pad == 0 {
        return Vec::new();
    }

    let mut out = String::with_capacity(end + pad + 16);
    let mut pen = Attributes::default();
    let mut pen_id = StyleId::DEFAULT;
    // Tracked apart from the pen: a style change inside one link must
    // not close and reopen the anchor.
    let mut link = None;
    let mut link_open = false;
    for (i, cell) in row[..end].iter().enumerate() {
        match cell.grapheme {
            Grapheme::Spacer => continue,
            Grapheme::SizedSpacer if opts.width.is_none() => continue,
            _ => {}
        }
        if cell.style != pen_id {
            let cell_attrs = *styles.resolve(cell.style);
            push_sgr_transition(&mut out, pen, cell_attrs, opts.caps);
            pen = cell_attrs;
            pen_id = cell.style;
        }
        if cell.link != link {
            push_link_transition(&mut out, cell.link, opts.links, &mut link_open);
            link = cell.link;
        }
        // A wide glyph whose right half fell outside the clip would print
        // two columns into the one left for it, pushing everything after
        // it into the neighboring pane of a tiled host frame.
        let clipped_wide = opts.width.is_some()
            && i + 1 == end
            && matches!(row.get(end).map(|c| c.grapheme), Some(Grapheme::Spacer));
        if clipped_wide {
            out.push(' ');
            continue;
        }
        match cell.grapheme {
            Grapheme::Empty | Grapheme::SizedSpacer => out.push(' '),
            Grapheme::Ascii(b) => out.push(char::from(b)),
            Grapheme::Char(c) if opts.blank_placeholders && c == PLACEHOLDER => out.push(' '),
            Grapheme::Char(c) => out.push(c),
            Grapheme::Cluster(id) => match clusters.get(id) {
                Some(s) if opts.blank_placeholders && s.starts_with(PLACEHOLDER) => {
                    out.push(' ');
                }
                Some(s) => out.push_str(s),
                None if opts.width.is_some() => out.push(' '),
                None => {}
            },
            Grapheme::Spacer => unreachable!("skipped above"),
        }
    }
    // Close both before the pad: padding inside an anchor makes the
    // blank tail clickable, and inheriting a background paints it.
    if link_open {
        out.push_str(OSC8_CLOSE);
    }
    if pen != Attributes::default() {
        out.push_str("\x1b[0m");
    }
    for _ in 0..pad {
        out.push(' ');
    }
    out.into_bytes()
}

/// An id past the table (the reattach window before its `Hyperlink`
/// message streams) is treated as no link, so the text stays readable
/// and un-clickable instead of joining the previous URI.
fn push_link_transition(
    out: &mut String,
    to: Option<NonZeroU16>,
    links: Option<&LinkTable>,
    open: &mut bool,
) {
    let entry = to.zip(links).and_then(|(id, links)| links.get(id));
    match entry {
        Some(e) => {
            let id = e.id.as_ref().map_or("", LinkText::as_str);
            let params = if id.is_empty() {
                String::new()
            } else {
                format!("id={id}")
            };
            let _ = write!(out, "\x1b]8;{params};{}\x1b\\", e.uri);
            *open = true;
        }
        None if *open => {
            out.push_str(OSC8_CLOSE);
            *open = false;
        }
        None => {}
    }
}

/// The SGR that sets the pen to exactly `attrs` from an unknown state:
/// always reset-prefixed, so a caller painting cells out of row order
/// needs no model of the pen in force.
#[must_use]
pub fn sgr_set(caps: AnsiCaps, attrs: &Attributes) -> String {
    let mut params = vec!["0".to_owned()];
    push_attr_params(&mut params, attrs, caps);
    format!("\x1b[{}m", params.join(";"))
}

/// Plain-text counterpart of [`row_ansi`]: trailing spaces (including
/// those a trailing `Empty` renders to) are trimmed, interior spaces
/// survive.
#[must_use]
pub fn row_text_trim(row: &[Cell], clusters: &ClusterTable) -> String {
    let mut s = String::with_capacity(row.len());
    for cell in row {
        push_cell_text(cell, clusters, &mut s);
    }
    let trimmed = s.trim_end_matches(' ').len();
    s.truncate(trimmed);
    s
}

/// The one grapheme-to-text ruleset every plain-text consumer shares
/// (search, [`row_text_trim`], selection extraction in the client):
/// `Empty` decodes to one space, `Spacer` / `SizedSpacer` contribute
/// nothing, and an unresolvable cluster handle (which a reattaching
/// client can hold briefly) contributes nothing.
pub fn push_cell_text(cell: &Cell, clusters: &ClusterTable, out: &mut String) {
    match cell.grapheme {
        Grapheme::Empty => out.push(' '),
        Grapheme::Ascii(b) => out.push(b as char),
        Grapheme::Char(c) => out.push(c),
        Grapheme::Cluster(id) => {
            if let Some(text) = clusters.get(id) {
                out.push_str(text);
            }
        }
        Grapheme::Spacer | Grapheme::SizedSpacer => {}
    }
}

/// Inclusive `(start, end)` row spans tied into logical lines by soft-wrap bits.
///
/// Selection's `extract_text` applies the same rule inline; the two must agree.
#[must_use]
pub fn logical_line_spans(continued: &[bool]) -> Vec<(usize, usize)> {
    let total = continued.len();
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (i, cont) in continued.iter().enumerate() {
        if i > 0 && !cont {
            lines.push((start, i - 1));
            start = i;
        }
    }
    if total > 0 {
        lines.push((start, total - 1));
    }
    lines
}

/// `rows` regrouped into logical lines by [`logical_line_spans`]. The
/// shorter of `rows` and `continued` bounds the walk.
pub fn logical_lines<'a, T>(
    rows: &'a [T],
    continued: &[bool],
) -> impl Iterator<Item = &'a [T]> + use<'a, T> {
    let len = continued.len().min(rows.len());
    logical_line_spans(&continued[..len])
        .into_iter()
        .map(move |(start, end)| &rows[start..=end])
}

/// Requires `from != to`.
fn push_sgr_transition(out: &mut String, from: Attributes, to: Attributes, caps: AnsiCaps) {
    if to == Attributes::default() {
        out.push_str("\x1b[0m");
        return;
    }
    let mut params: Vec<String> = Vec::new();
    if from != Attributes::default() {
        params.push("0".to_owned());
    }
    push_attr_params(&mut params, &to, caps);
    out.push_str("\x1b[");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(';');
        }
        out.push_str(p);
    }
    out.push('m');
}

fn push_attr_params(params: &mut Vec<String>, a: &Attributes, caps: AnsiCaps) {
    let f = a.flags;
    if f.contains(AttrFlags::BOLD) {
        params.push("1".to_owned());
    }
    if f.contains(AttrFlags::FAINT) {
        params.push("2".to_owned());
    }
    if f.contains(AttrFlags::ITALIC) {
        params.push("3".to_owned());
    }
    if f.contains(AttrFlags::UNDERLINE) {
        params.push(
            match a.underline_style {
                UnderlineStyle::Single => "4",
                // Double is the only shape with a legacy code; the rest
                // exist only in the `4:N` sub-parameter form.
                UnderlineStyle::Double => "21",
                UnderlineStyle::Curly => "4:3",
                UnderlineStyle::Dotted => "4:4",
                UnderlineStyle::Dashed => "4:5",
            }
            .to_owned(),
        );
    }
    if f.contains(AttrFlags::BLINK) {
        params.push("5".to_owned());
    }
    if f.contains(AttrFlags::REVERSE) {
        params.push("7".to_owned());
    }
    if f.contains(AttrFlags::CONCEAL) {
        params.push("8".to_owned());
    }
    if f.contains(AttrFlags::STRIKETHROUGH) {
        params.push("9".to_owned());
    }
    if f.contains(AttrFlags::OVERLINE) {
        params.push("53".to_owned());
    }
    push_color_params(params, a.fg, ColorRole::Foreground, caps);
    push_color_params(params, a.bg, ColorRole::Background, caps);
    push_color_params(params, a.underline_color, ColorRole::Underline, caps);
}

#[derive(Clone, Copy)]
enum ColorRole {
    Foreground,
    Background,
    Underline,
}

fn push_color_params(params: &mut Vec<String>, color: Color, role: ColorRole, caps: AnsiCaps) {
    match color {
        Color::Default => {}
        Color::Indexed(n) => match role {
            // Underline color has no 4-bit form.
            ColorRole::Foreground if n < 8 => params.push((30 + u16::from(n)).to_string()),
            ColorRole::Foreground if n < 16 => params.push((90 + u16::from(n) - 8).to_string()),
            ColorRole::Foreground => params.push(format!("38;5;{n}")),
            ColorRole::Background if n < 8 => params.push((40 + u16::from(n)).to_string()),
            ColorRole::Background if n < 16 => params.push((100 + u16::from(n) - 8).to_string()),
            ColorRole::Background => params.push(format!("48;5;{n}")),
            ColorRole::Underline => params.push(format!("58;5;{n}")),
        },
        Color::Rgb(r, g, b) => {
            let base = match role {
                ColorRole::Foreground => 38,
                ColorRole::Background => 48,
                ColorRole::Underline => 58,
            };
            let mut s = String::new();
            if caps.truecolor {
                let _ = write!(s, "{base};2;{r};{g};{b}");
            } else {
                let _ = write!(s, "{base};5;{}", rgb_to_256(r, g, b));
            }
            params.push(s);
        }
    }
}

/// Nearest xterm-256 index over the cube and gray ramp only: indices
/// `0..=15` are theme-dependent, so the host may paint them anything.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let dist = |a: u8, b: u8| {
        let d = i32::from(a) - i32::from(b);
        d * d
    };
    let mut best = 16u8;
    let mut best_d = i32::MAX;
    for i in 16..=255u8 {
        let (pr, pg, pb) = crate::default_palette_color(i);
        let d = dist(r, pr) + dist(g, pg) + dist(b, pb);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests;
