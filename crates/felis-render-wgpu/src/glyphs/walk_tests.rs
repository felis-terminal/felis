use std::{
    num::{NonZeroU16, NonZeroU32},
    sync::{Arc, OnceLock},
};

use felis_grid::{
    AttrFlags, Attributes, Cell, ClusterText, Color, Grapheme, HAlign, ScreenBuffer,
    ScrollDirection, Sizing, TableGc, VAlign,
};
use felis_shaping::{Font, FontStack, Shaper, SizingKey};
use proptest::prelude::*;

use super::{ClusterGlyph, GlyphIndex, GridWalker, ShapeFrame, ShapedCell};

fn stack() -> FontStack {
    static FONT: OnceLock<Arc<Font>> = OnceLock::new();
    let font = FONT.get_or_init(|| Arc::new(Font::load_default().expect("system monospace font")));
    FontStack::new(Arc::clone(font))
}

const CLUSTERS: [&str; 3] = ["e\u{301}", "\u{1F44D}\u{1F3FD}", "n\u{303}"];

/// Row and column bounds exceed the grid so out-of-range writes are
/// exercised too.
#[derive(Debug, Clone)]
enum Op {
    /// Row, text, font style.
    WriteRow(u16, Vec<u8>, u8),
    /// Row, col, char, font style.
    SetChar(u16, u16, char, u8),
    /// Row, col, index into [`CLUSTERS`].
    SetCluster(u16, u16, u8),
    /// Top, bottom, count, up.
    Scroll(u16, u16, u16, bool),
    Resize(u16, u16),
    Reconnect,
    GcStyles,
    /// Row, col, OSC 66 scale.
    Sized(u16, u16, u8),
    /// Chars an overlay primes between walks, as `populate_chars` does.
    Overlay(Vec<char>),
}

fn op() -> impl Strategy<Value = Op> {
    let text = proptest::collection::vec(0x20u8..0x7f, 0..20);
    let ch = prop_oneof![
        (0x21u8..0x7f).prop_map(char::from),
        Just('\u{2500}'),
        Just('\u{3042}'),
    ];
    prop_oneof![
        6 => (0u16..12, text, 0u8..4).prop_map(|(r, t, s)| Op::WriteRow(r, t, s)),
        6 => (0u16..12, 0u16..20, ch, 0u8..4).prop_map(|(r, c, ch, s)| Op::SetChar(r, c, ch, s)),
        3 => (0u16..12, 0u16..20, 0u8..3).prop_map(|(r, c, i)| Op::SetCluster(r, c, i)),
        3 => (0u16..12, 0u16..12, 1u16..4, any::<bool>()).prop_map(|(t, b, n, u)| Op::Scroll(t, b, n, u)),
        1 => (1u16..12, 1u16..20).prop_map(|(r, c)| Op::Resize(r, c)),
        1 => Just(Op::Reconnect),
        1 => Just(Op::GcStyles),
        2 => (0u16..12, 0u16..20, 1u8..4).prop_map(|(r, c, s)| Op::Sized(r, c, s)),
        1 => proptest::collection::vec(0xc0u32..0x100, 1..40)
            .prop_map(|cs| Op::Overlay(cs.into_iter().filter_map(char::from_u32).collect())),
    ]
}

fn attrs(style: u8) -> Attributes {
    let flags = [
        AttrFlags::empty(),
        AttrFlags::BOLD,
        AttrFlags::ITALIC,
        AttrFlags::BOLD | AttrFlags::ITALIC,
    ];
    Attributes {
        fg: Color::Indexed(style),
        flags: flags[usize::from(style % 4)],
        ..Attributes::default()
    }
}

struct Harness {
    screen: ScreenBuffer,
    features: Vec<String>,
    shaper: Shaper,
    index: GlyphIndex,
    walker: GridWalker,
    frame: ShapeFrame,
}

impl Harness {
    fn new(features: Vec<String>, atlas_side: u32) -> Self {
        Self {
            screen: ScreenBuffer::with_scrollback(6, 10, 0),
            features,
            shaper: Shaper::new(),
            index: GlyphIndex::new(stack(), 14.0, NonZeroU32::new(atlas_side).unwrap()),
            walker: GridWalker::default(),
            frame: ShapeFrame::empty(),
        }
    }

    fn apply(&mut self, op: Op) {
        let (rows, cols) = (self.screen.rows(), self.screen.cols());
        match op {
            Op::WriteRow(row, text, style) => {
                let style = self.screen.style_table_mut().intern(attrs(style));
                let cells: Vec<Cell> = (0..usize::from(cols))
                    .map(|c| Cell {
                        grapheme: text.get(c).map_or(Grapheme::Empty, |b| Grapheme::Ascii(*b)),
                        style,
                        link: None::<NonZeroU16>,
                        sizing: None,
                    })
                    .collect();
                self.screen.write_row_cells(row, &cells);
            }
            Op::SetChar(row, col, ch, style) => {
                let style = self.screen.style_table_mut().intern(attrs(style));
                let grapheme = u8::try_from(ch).map_or(Grapheme::Char(ch), Grapheme::Ascii);
                let cell = Cell {
                    grapheme,
                    style,
                    ..Cell::default()
                };
                self.screen.set_cell(row, col, cell);
            }
            Op::SetCluster(row, col, i) => {
                // Installed before any row names it, as the wire's causal
                // delivery guarantees.
                let id = NonZeroU32::new(u32::from(i) + 1).unwrap();
                let text = ClusterText::new(CLUSTERS[usize::from(i)]).unwrap();
                self.screen.install_cluster(id, text);
                let cell = Cell {
                    grapheme: Grapheme::Cluster(id),
                    ..Cell::default()
                };
                self.screen.set_cell(row, col, cell);
            }
            Op::Scroll(top, bottom, n, up) => {
                let dir = [ScrollDirection::Down, ScrollDirection::Up][usize::from(up)];
                let _ = self.screen.apply_scroll_directive(top, bottom, n, dir);
            }
            Op::Resize(rows, cols) => self.screen.resize(rows, cols),
            Op::Reconnect => self.screen = ScreenBuffer::with_scrollback(rows, cols, 0),
            Op::GcStyles => TableGc::new().sweep(&mut self.screen),
            Op::Sized(row, col, scale) => {
                let sizing = Sizing::new(scale, 0, 0, 0, VAlign::Center, HAlign::Center);
                let handle = sizing.and_then(|s| self.screen.install_sizing(s));
                self.screen.set_cell_sizing(row, col, handle);
            }
            Op::Overlay(chars) => {
                let h = self.index.cell_metrics().height;
                for c in chars {
                    self.index.ensure(c, h, SizingKey::default());
                }
                self.index.drain_pending().for_each(drop);
            }
        }
    }

    /// Walks the way `Renderer::populate_glyphs` does, then checks the
    /// frame against a from-scratch walk and the index against every
    /// glyph a full walk needs.
    fn frame(&mut self) -> Result<(), TestCaseError> {
        let Self {
            screen,
            features,
            shaper,
            index,
            walker,
            frame,
        } = self;
        walker.walk(index, screen, shaper, features, frame);
        if index.atlas_was_reset() {
            walker.walk(index, screen, shaper, features, frame);
        }
        let thrashed = index.atlas_was_reset();
        index.drain_pending().for_each(drop);

        let mut fresh_index = GlyphIndex::new(stack(), 14.0, NonZeroU32::new(4096).unwrap());
        let mut fresh = ShapeFrame::empty();
        GridWalker::default().walk(&mut fresh_index, screen, shaper, features, &mut fresh);
        prop_assert_eq!(resolved(frame), resolved(&fresh));

        if !thrashed {
            let walked = index.walked.take();
            GridWalker::default().walk(index, screen, shaper, features, &mut ShapeFrame::empty());
            prop_assert!(!index.atlas_was_reset());
            prop_assert_eq!(index.drain_pending().count(), 0);
            index.walked = walked;
        }
        screen.clear_damage();
        Ok(())
    }
}

/// The frame with each cluster's pool slice in place of its offset.
fn resolved(frame: &ShapeFrame) -> Vec<(ShapedCell, Vec<ClusterGlyph>)> {
    frame
        .cells()
        .iter()
        .map(|&cell| match cell {
            ShapedCell::Cluster { start, len, fit_px } => (
                ShapedCell::Cluster {
                    start: 0,
                    len,
                    fit_px,
                },
                frame.cluster_slice(start, len).to_vec(),
            ),
            cell => (cell, Vec::new()),
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn walking_the_marked_rows_matches_a_full_walk(
        ops in proptest::collection::vec(op(), 1..40),
        features in prop_oneof![Just(vec![]), Just(vec!["liga".to_owned()])],
        atlas_side in prop_oneof![Just(4096u32), 64u32..160],
    ) {
        let mut h = Harness::new(features, atlas_side);
        h.frame()?;
        for op in ops {
            h.apply(op);
            h.frame()?;
        }
    }
}

#[test]
fn a_one_cell_write_walks_only_its_row() {
    let mut h = Harness::new(vec!["liga".to_owned()], 4096);
    h.frame().unwrap();
    h.apply(Op::SetChar(3, 2, 'x', 0));
    h.frame().unwrap();
    h.apply(Op::SetChar(4, 2, 'y', 0));
    let Harness {
        screen,
        features,
        shaper,
        index,
        walker,
        frame,
    } = &mut h;
    walker.walk(index, screen, shaper, features, frame);
    assert_eq!(walker.rows, [4]);
}

#[test]
fn rewalked_clusters_do_not_grow_the_pool_without_bound() {
    let mut h = Harness::new(vec![], 4096);
    for i in 0..3000u16 {
        h.apply(Op::SetCluster(0, i % 10, (i % 3) as u8));
        h.frame().unwrap();
    }
    assert!(h.frame.cluster_glyphs.len() <= 4096);
}
